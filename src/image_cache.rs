use anyhow::{anyhow, Context, Result};
use egui::TextureHandle;
use image::RgbaImage;
use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use crate::image_util::{load_image_rgba, load_image_rgba_from_bytes, rgba_to_texture};
use crate::remote_worker::{RemoteRange, RemoteWorkerRequest};
use crate::sequence::{SequenceSource, SequenceSpec};

// Load request for the single background loader thread
#[derive(Clone)]
struct LoadRequest {
    idx: u64,
    file_name: String,
    seq_source: SequenceSource,
    request_tx: Option<Sender<RemoteWorkerRequest>>,
}

/// Bidirectional image cache with lazy sliding window.
/// Implements a hysteresis-based cache window:
/// - Window size: 20 images before and after current index (40 total)
/// - Reload threshold: Only recalculate window when index moves ±10 from center
/// This reduces unnecessary reloads during back-and-forth navigation.
pub struct ImageCache {
    cache: BTreeMap<u64, TextureHandle>,
    cache_radius: usize,
    pending_loads: HashSet<u64>,
    load_request_tx: Sender<LoadRequest>,
    result_rx: Receiver<(u64, RgbaImage)>,
    seq_source: SequenceSource,
    request_tx: Option<Sender<RemoteWorkerRequest>>,
    remote_range: Option<RemoteRange>,
    /// Center of the current cache window (for hysteresis logic)
    window_center: Option<u64>,
}

impl ImageCache {
    pub fn new(
        cache_radius: usize,
        seq_source: SequenceSource,
        request_tx: Option<Sender<RemoteWorkerRequest>>,
        remote_range: Option<RemoteRange>,
    ) -> Self {
        let (load_request_tx, load_request_rx) = channel::<LoadRequest>();
        let (result_tx, result_rx) = channel::<(u64, RgbaImage)>();

        // Spawn single loader thread that processes requests from queue
        thread::spawn(move || {
            while let Ok(req) = load_request_rx.recv() {
                // Wrap in closure that returns Result to use ?
                let rgba: Result<RgbaImage> = (|| {
                    match &req.seq_source {
                        SequenceSource::Local(dir) => {
                            load_image_rgba(&dir.join(&req.file_name))
                        }
                        SequenceSource::Remote { user_host, dir } => {
                            let remote_path = crate::sequence::build_remote_path(dir, &req.file_name);
                            if let Some(tx) = &req.request_tx {
                                let (response_tx, response_rx) = channel();
                                eprintln!("[SSH] cat: {} (idx={})", remote_path, req.idx);
                                tx.send(RemoteWorkerRequest::Cat {
                                    idx: req.idx,
                                    path: remote_path.clone(),
                                    response_tx,
                                }).context("Failed to send CAT request")?;
                                let bytes = response_rx.recv().context("remote worker hung up")??;
                                eprintln!("[SSH] cat received {} bytes (idx={})", bytes.len(), req.idx);
                                load_image_rgba_from_bytes(&bytes, &format!("{}:{}", user_host, remote_path))
                            } else {
                                Err(anyhow!("SSH connection not available for background loading"))
                            }
                        }
                    }
                })();

                if let Ok(rgba) = rgba {
                    let _ = result_tx.send((req.idx, rgba));
                }
            }
        });

        Self {
            cache: BTreeMap::new(),
            cache_radius,
            pending_loads: HashSet::new(),
            load_request_tx,
            result_rx,
            seq_source,
            request_tx,
            remote_range,
            window_center: None,
        }
    }

    /// Get texture for specific index if cached
    pub fn get(&self, idx: u64) -> Option<&TextureHandle> {
        self.cache.get(&idx)
    }

    /// Process any decoded images from background loader thread (convert to textures)
    fn process_decoded_images(&mut self, ctx: &egui::Context) -> usize {
        let mut converted = 0;
        // Process all available decoded images (non-blocking)
        while let Ok((idx, rgba_image)) = self.result_rx.try_recv() {
            // Only insert if this idx is still pending (i.e., not evicted out-of-range)
            if self.pending_loads.remove(&idx) {
                let (w, h) = (rgba_image.width(), rgba_image.height());
                if let Ok(tex) = rgba_to_texture(ctx, idx, rgba_image) {
                    eprintln!("[Cache] loaded idx={} ({}x{})", idx, w, h);
                    self.cache.insert(idx, tex);
                    converted += 1;
                }
            }
        }
        converted
    }

    /// Update cache centered on new_index, preloading neighbors and evicting out-of-range entries.
    /// Uses a lazy sliding window with hysteresis:
    /// - Window is only recalculated when new_index moves ±10 from the current window center
    /// - This avoids unnecessary reloads during back-and-forth navigation
    pub fn update_for_index(
        &mut self,
        new_index: u64,
        seq: &SequenceSpec,
        ctx: &egui::Context,
    ) -> (usize, usize) {
        // First, process any decoded images waiting to become textures
        self.process_decoded_images(ctx);

        const RELOAD_THRESHOLD: u64 = 10;
        
        // Determine if we need to recalculate the window
        let needs_recalc = match self.window_center {
            None => true, // First time, always calculate
            Some(center) => {
                // Check if current index has moved more than RELOAD_THRESHOLD from center
                let distance = if new_index > center {
                    new_index - center
                } else {
                    center - new_index
                };
                distance > RELOAD_THRESHOLD
            }
        };

        if !needs_recalc {
            // Within the inner window - just process decoded images, no reload
            return (0, 0);
        }

        // Update window center to new index
        self.window_center = Some(new_index);
        eprintln!("[Cache] window center updated to {}", new_index);

        let radius = self.cache_radius as u64;
        let min_idx = new_index.saturating_sub(radius);
        let max_idx = new_index.saturating_add(radius);

        // Update remote range for SSH worker to check
        if let Some(r) = &self.remote_range {
            r.set(min_idx, max_idx);
        }

        // Evict entries outside the desired range
        let to_evict: Vec<u64> = self
            .cache
            .keys()
            .filter(|&&idx| idx < min_idx || idx > max_idx)
            .copied()
            .collect();

        let evicted_count = to_evict.len();
        if evicted_count > 0 {
            eprintln!("[Cache] evicted {} entries", evicted_count);
        }
        for idx in to_evict {
            self.cache.remove(&idx);
        }

        // Cancel pending loads outside range
        self.pending_loads.retain(|&idx| idx >= min_idx && idx <= max_idx);

        // Launch background loads for missing entries in range
        let mut launched_count = 0;
        for idx in min_idx..=max_idx {
            if !self.cache.contains_key(&idx) && !self.pending_loads.contains(&idx) {
                // For local files: check existence directly. For remote: always try to load
                let should_load = match &self.seq_source {
                    SequenceSource::Local(dir) => dir.join(seq.file_name_for(idx)).exists(),
                    SequenceSource::Remote { .. } => true,
                };

                if should_load {
                    self.pending_loads.insert(idx);
                    let file_name = format!("{}{:0width$}{}", seq.prefix, idx, seq.suffix, width = seq.width);
                    let req = LoadRequest {
                        idx,
                        file_name,
                        seq_source: self.seq_source.clone(),
                        request_tx: self.request_tx.clone(),
                    };
                    let _ = self.load_request_tx.send(req);
                    launched_count += 1;
                }
            }
        }

        (launched_count, evicted_count)
    }

    /// Process any newly decoded images on each frame
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.process_decoded_images(ctx);
    }

    pub fn cache_info(&self) -> String {
        format!("Cache: {} loaded, {} pending", self.cache.len(), self.pending_loads.len())
    }

    pub fn is_pending(&self, idx: u64) -> bool {
        self.pending_loads.contains(&idx)
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        // Clear pending loads and close loader channel
        let pending_count = self.pending_loads.len();
        if pending_count > 0 {
            eprintln!("[Loader] cancelling {} pending loads", pending_count);
        }
        self.pending_loads.clear();
        eprintln!("[Loader] exiting");
        // Dropping load_request_tx will cause loader thread to exit
    }
}
