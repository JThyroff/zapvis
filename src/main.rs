use anyhow::{anyhow, Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use eframe::egui;
use egui::{ColorImage, TextureHandle};
use image::RgbaImage;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use zapvis::PersistentSsh;

/// Request sent to the remote worker thread
enum RemoteWorkerRequest {
    Exists {
        path: String,
        response_tx: Sender<Result<bool>>,
    },
    Cat {
        path: String,
        response_tx: Sender<Result<Vec<u8>>>,
    },
}

/// Spawn a remote worker thread that exclusively owns the SSH connection
/// and processes requests serially. Returns the request sender.
fn spawn_remote_worker(ssh: PersistentSsh) -> Sender<RemoteWorkerRequest> {
    let (tx, rx) = channel::<RemoteWorkerRequest>();
    
    thread::spawn(move || {
        let mut ssh = ssh;
        while let Ok(req) = rx.recv() {
            match req {
                RemoteWorkerRequest::Exists { path, response_tx } => {
                    let result = ssh.exists(&path);
                    let _ = response_tx.send(result);
                }
                RemoteWorkerRequest::Cat { path, response_tx } => {
                    let result = ssh.cat(&path);
                    let _ = response_tx.send(result);
                }
            }
        }
    });
    
    tx
}

/// zapvis: sequence-only image viewer.
/// Opens a file, matches it against configured patterns with # as digit placeholders,
/// then navigates by changing the numeric id and stat()'ing the constructed filename.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Image file to open (recommended). Folder mode is intentionally not supported.
    input: Option<String>,

    /// Optional pattern override, e.g. "########_#.png"
    #[arg(long)]
    pattern: Option<String>,

    /// Show config file path and content, then exit
    #[arg(short, long)]
    config: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    patterns: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Handle --config flag
    if args.config {
        let path = config_path()?;
        println!("Config path: {}", path.display());
        if path.exists() {
            let content = fs::read_to_string(&path)
                .context("Failed to read config file")?;
            println!("\nConfig content:\n{}", content);
        } else {
            println!("Config file does not exist.");
        }
        return Ok(());
    }

    // Input is required if not showing config
    let input = args
        .input
        .ok_or_else(|| anyhow!("Input file is required (unless using --config flag)"))?;
    let input_spec = if let Some((user_host, remote_path)) = parse_remote_input(&input) {
        let file_name = file_name_from_str_path(&remote_path)?;
        let dir = Path::new(&remote_path)
            .parent()
            .ok_or_else(|| anyhow!("Remote input has no parent directory"))?
            .to_string_lossy()
            .to_string();
        InputSpec {
            file_name,
            source: SequenceSource::Remote { user_host, dir },
        }
    } else {
        let input = PathBuf::from(&input);
        if !input.is_file() {
            return Err(anyhow!(
                "Input must be an image FILE path. Folder mode is intentionally not supported."
            ));
        }
        let file_name = file_name_from_path(&input)?;
        let dir = input
            .parent()
            .ok_or_else(|| anyhow!("Input has no parent directory"))?
            .to_path_buf();
        InputSpec {
            file_name,
            source: SequenceSource::Local(dir),
        }
    };

    let mut cfg = load_config().unwrap_or_default();

    // If user provided --pattern, try it first and store it if it works.
    if let Some(pat) = args.pattern.clone() {
        if pattern_matches_file(&pat, &input_spec.file_name)? {
            maybe_add_pattern(&mut cfg, pat);
            save_config(&cfg).ok(); // ignore save errors (still can run)
        } else {
            return Err(anyhow!(
                "Provided --pattern did not match the input filename. Nothing saved."
            ));
        }
    }

    // Establish persistent SSH early if remote (and spawn worker thread)
    let remote_worker_tx = match &input_spec.source {
        SequenceSource::Remote { user_host, .. } => {
            match PersistentSsh::connect(user_host) {
                Ok(ssh) => Some(spawn_remote_worker(ssh)),
                Err(e) => {
                    eprintln!("Failed to establish persistent SSH: {}", e);
                    None
                }
            }
        }
        SequenceSource::Local(_) => None,
    };

    // Determine which pattern to use:
    let (pattern, seq) = match pick_sequence(&cfg, &input_spec, remote_worker_tx.clone()) {
        Ok(v) => v,
        Err(e) => {
            // Your rule: if no hits, quit. (No interactive prompt here.)
            eprintln!("{e}");
            eprintln!("\nKnown patterns in config:");
            for (i, p) in cfg.patterns.iter().enumerate() {
                eprintln!("  {}) {}", i + 1, p);
            }
            eprintln!("\nTip: run with --pattern \"########_#.png\" to add/try a new one.");
            return Err(anyhow!("No sequence pattern matched. Quitting."));
        }
    };

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "zapvis",
        native_options,
        Box::new(|cc| Ok(Box::new(ZapVisApp::new(cc, pattern, seq, remote_worker_tx)))),
    )
    .map_err(|e| anyhow!(e.to_string()))?;

    Ok(())
}

/// Represents a compiled sequence extracted from a filename pattern and a concrete file.
#[derive(Debug, Clone)]
enum SequenceSource {
    Local(PathBuf),
    Remote { user_host: String, dir: String },
}

#[derive(Debug, Clone)]
struct SequenceSpec {
    source: SequenceSource,
    prefix: String,
    width: usize,
    suffix: String,
    index: u64,
}

#[derive(Debug, Clone)]
struct InputSpec {
    file_name: String,
    source: SequenceSource,
}

impl SequenceSpec {
    fn file_name_for(&self, idx: u64) -> String {
        format!("{}{:0width$}{}", self.prefix, idx, self.suffix, width = self.width)
    }

    fn path_display(&self, idx: u64) -> String {
        match &self.source {
            SequenceSource::Local(dir) => dir.join(self.file_name_for(idx)).display().to_string(),
            SequenceSource::Remote { user_host, dir } => {
                let remote_path = build_remote_path(dir, &self.file_name_for(idx));
                format!("{}:{}", user_host, remote_path)
            }
        }
    }

    fn exists_with_ssh(&self, idx: u64, request_tx: Option<Sender<RemoteWorkerRequest>>) -> Result<bool> {
        match &self.source {
            SequenceSource::Local(dir) => Ok(dir.join(self.file_name_for(idx)).exists()),
            SequenceSource::Remote { dir, .. } => {
                let remote_path = build_remote_path(dir, &self.file_name_for(idx));
                if let Some(tx) = request_tx {
                    let (response_tx, response_rx) = channel();
                    tx.send(RemoteWorkerRequest::Exists {
                        path: remote_path,
                        response_tx,
                    })?;
                    response_rx.recv()?
                } else {
                    Err(anyhow!("Remote SSH connection not available"))
                }
            }
        }
    }
}

/// Compile a pattern like "image_#####.png" into:
/// - regex to extract index
/// - prefix/width/suffix for reconstruction
///
/// MVP limitation: supports exactly ONE contiguous # group.
fn compile_pattern(pat: &str) -> Result<(Regex, String, usize, String)> {
    let hash_runs: Vec<(usize, usize)> = find_hash_runs(pat);
    if hash_runs.len() != 1 {
        return Err(anyhow!(
            "Pattern must contain exactly ONE contiguous # run (for now). Got: {pat}"
        ));
    }
    let (start, end) = hash_runs[0];
    let prefix = &pat[..start];
    let suffix = &pat[end..];
    let width = end - start;

    // Build regex: escape prefix/suffix, capture digits of exact width
    let re_str = format!(
        "^{}(\\d{{{}}}){}$",
        regex::escape(prefix),
        width,
        regex::escape(suffix)
    );
    let re = Regex::new(&re_str).context("Failed to compile regex from pattern")?;
    Ok((re, prefix.to_string(), width, suffix.to_string()))
}

fn find_hash_runs(s: &str) -> Vec<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            runs.push((start, i));
        } else {
            i += 1;
        }
    }
    runs
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let txt = fs::read_to_string(&path).context("Failed to read config")?;
    let cfg: Config = toml::from_str(&txt).context("Failed to parse config TOML")?;
    Ok(cfg)
}

fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let txt = toml::to_string_pretty(cfg).context("Failed to serialize config TOML")?;
    fs::write(&path, txt).context("Failed to write config")?;
    Ok(())
}

fn config_path() -> Result<PathBuf> {
    let proj = ProjectDirs::from("dev", "zapvis", "zapvis")
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    Ok(proj.config_dir().join("config.toml"))
}

fn maybe_add_pattern(cfg: &mut Config, pat: String) {
    if !cfg.patterns.iter().any(|p| p == &pat) {
        cfg.patterns.push(pat);
    }
}

fn pattern_matches_file(pat: &str, file_name: &str) -> Result<bool> {
    let (re, _, _, _) = compile_pattern(pat)?;
    Ok(re.is_match(file_name))
}

/// Try patterns from config; return first that matches AND has neighbor evidence.
/// Evidence: current file matches and at least one neighbor exists (idx±1).
fn pick_sequence(
    cfg: &Config,
    input: &InputSpec,
    request_tx: Option<Sender<RemoteWorkerRequest>>,
) -> Result<(String, SequenceSpec)> {
    // If config empty, fail quickly.
    if cfg.patterns.is_empty() {
        return Err(anyhow!("No patterns configured."));
    }

    let file_name = input.file_name.clone();
    let source = input.source.clone();

    for pat in &cfg.patterns {
        let (re, prefix, width, suffix) = compile_pattern(pat)?;
        if let Some(cap) = re.captures(&file_name) {
            let idx_str = cap.get(1).unwrap().as_str();
            let idx: u64 = idx_str.parse().context("Failed to parse captured index")?;

            let spec = SequenceSpec {
                source: source.clone(),
                prefix,
                width,
                suffix,
                index: idx,
            };

            // Neighbor evidence via stat(): cheap and avoids enumeration.
            let has_next = spec.exists_with_ssh(idx + 1, request_tx.clone()).unwrap_or(false);
            let has_prev = idx > 0 && spec.exists_with_ssh(idx - 1, request_tx.clone()).unwrap_or(false);

            if has_next || has_prev {
                return Ok((pat.clone(), spec));
            }
        }
    }

    Err(anyhow!("No configured pattern matched with neighbor evidence."))
}

// Load request for the single background loader thread
#[derive(Clone)]
struct LoadRequest {
    idx: u64,
    file_name: String,
    seq_source: SequenceSource,
    request_tx: Option<Sender<RemoteWorkerRequest>>,
}

/// Bidirectional image cache with configurable radius.
/// Maintains textures for indices in range [current - radius, current + radius].
/// Uses a single background loader thread with a queue for image decoding.
struct ImageCache {
    cache: BTreeMap<u64, TextureHandle>,
    cache_radius: usize,
    pending_loads: HashSet<u64>,
    load_request_tx: Sender<LoadRequest>,
    result_rx: Receiver<(u64, RgbaImage)>,
    seq_source: SequenceSource,
    request_tx: Option<Sender<RemoteWorkerRequest>>,
}

impl ImageCache {
    fn new(
        cache_radius: usize,
        seq_source: SequenceSource,
        request_tx: Option<Sender<RemoteWorkerRequest>>,
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
                            let remote_path = build_remote_path(dir, &req.file_name);
                            if let Some(tx) = &req.request_tx {
                                let (response_tx, response_rx) = channel();
                                tx.send(RemoteWorkerRequest::Cat {
                                    path: remote_path.clone(),
                                    response_tx,
                                }).context("Failed to send CAT request")?;
                                let bytes = response_rx.recv().context("remote worker hung up")??;
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
        }
    }

    /// Get texture for specific index if cached
    fn get(&self, idx: u64) -> Option<&TextureHandle> {
        self.cache.get(&idx)
    }

    /// Process any decoded images from background loader thread (convert to textures)
    fn process_decoded_images(&mut self, ctx: &egui::Context) -> usize {
        let mut converted = 0;
        // Process all available decoded images (non-blocking)
        while let Ok((idx, rgba_image)) = self.result_rx.try_recv() {
            // Only insert if this idx is still pending (i.e., not evicted out-of-range)
            if self.pending_loads.remove(&idx) {
                if let Ok(tex) = rgba_to_texture(ctx, idx, rgba_image) {
                    self.cache.insert(idx, tex);
                    converted += 1;
                }
            }
        }
        converted
    }

    /// Update cache centered on new_index, preloading neighbors and evicting out-of-range entries
    fn update_for_index(
        &mut self,
        new_index: u64,
        seq: &SequenceSpec,
        ctx: &egui::Context,
    ) -> (usize, usize) {
        // First, process any decoded images waiting to become textures
        self.process_decoded_images(ctx);

        let radius = self.cache_radius as u64;
        let min_idx = new_index.saturating_sub(radius);
        let max_idx = new_index.saturating_add(radius);

        // Evict entries outside the desired range
        let to_evict: Vec<u64> = self
            .cache
            .keys()
            .filter(|&&idx| idx < min_idx || idx > max_idx)
            .copied()
            .collect();
        
        let evicted_count = to_evict.len();
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
    fn tick(&mut self, ctx: &egui::Context) {
        self.process_decoded_images(ctx);
    }

    fn cache_info(&self) -> String {
        format!("Cache: {} loaded, {} pending", self.cache.len(), self.pending_loads.len())
    }

    fn is_pending(&self, idx: u64) -> bool {
        self.pending_loads.contains(&idx)
    }
}

struct ZapVisApp {
    pattern: String,
    seq: SequenceSpec,
    cache: ImageCache,
    status: String,
}

impl ZapVisApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        pattern: String,
        seq: SequenceSpec,
        request_tx: Option<Sender<RemoteWorkerRequest>>,
    ) -> Self {
        let cache = ImageCache::new(10, seq.source.clone(), request_tx);

        Self {
            pattern,
            seq,
            cache,
            status: String::new(),
        }
    }

    fn update_cache_and_status(&mut self, ctx: &egui::Context) {
        let (loaded, evicted) = self.cache.update_for_index(self.seq.index, &self.seq, ctx);
        
        let path = self.seq.path_display(self.seq.index);
        let idx = self.seq.index;
        
        if self.cache.get(idx).is_some() {
            // Image is cached and ready
            self.status = format!(
                "{}  (pattern: {})  |  {} | +{} -{}",
                path,
                self.pattern,
                self.cache.cache_info(),
                loaded,
                evicted
            );
        } else if self.cache.is_pending(idx) {
            // Image is being loaded
            self.status = format!(
                "Loading {} | {}",
                path,
                self.cache.cache_info()
            );
        } else {
            // Image not found or failed to load
            self.status = format!(
                "Not found / failed: {} | {} | +{} -{}",
                path,
                self.cache.cache_info(),
                loaded,
                evicted
            );
        }
    }

    fn try_step(&mut self, ctx: &egui::Context, delta: i64) {
        let cur = self.seq.index as i64;
        let next = cur + delta;
        if next < 0 {
            return;
        }
        let next_u = next as u64;
        
        // For local files, check existence first (fast, non-blocking)
        if let SequenceSource::Local(dir) = &self.seq.source {
            if !dir.join(self.seq.file_name_for(next_u)).exists() {
                let p = self.seq.path_display(next_u);
                self.status = format!("No file: {} | {}", p, self.cache.cache_info());
                return;
            }
        }
        
        // For remote: proceed optimistically (don't block UI with recv())
        // The cache loader will attempt to fetch and show "Failed to load" if it doesn't exist
        self.seq.index = next_u;
        self.update_cache_and_status(ctx);
    }
}

impl eframe::App for ZapVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process any decoded images from background threads
        self.cache.tick(ctx);

        // Load initial cache once
        if self.cache.cache.is_empty() && self.status.is_empty() {
            self.update_cache_and_status(ctx);
        }

        // Keyboard navigation
        let input = ctx.input(|i| i.clone());
        if input.key_pressed(egui::Key::ArrowRight) || input.key_pressed(egui::Key::D) {
            self.try_step(ctx, 1);
        }
        if input.key_pressed(egui::Key::ArrowLeft) || input.key_pressed(egui::Key::A) {
            self.try_step(ctx, -1);
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.label(&self.status);
            ui.label("Keys: Left/Right or A/D. Esc closes the window.");
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = self.cache.get(self.seq.index) {
                let avail = ui.available_size();

                let tex_size = tex.size_vec2();
                let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y).min(1.0);
                let size = tex_size * scale;

                ui.add(egui::Image::new(tex).fit_to_exact_size(size));
            } else {
                ui.label("No image loaded.");
            }
        });

        // Allow ESC to quit
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// Load and decode image to RGBA (can be done in background thread)
fn load_image_rgba(path: &Path) -> Result<RgbaImage> {
    let img = image::open(path)
        .with_context(|| format!("image::open failed for {}", path.display()))?;
    Ok(img.to_rgba8())
}

fn load_image_rgba_from_bytes(bytes: &[u8], source: &str) -> Result<RgbaImage> {
    let img = image::load_from_memory(bytes)
        .with_context(|| format!("image::load_from_memory failed for {}", source))?;
    Ok(img.to_rgba8())
}

/// Convert RgbaImage to egui TextureHandle (must be done on main thread with Context)
fn rgba_to_texture(ctx: &egui::Context, idx: u64, rgba: RgbaImage) -> Result<TextureHandle> {
    let (w, h) = rgba.dimensions();
    let pixels = rgba.into_raw();
    let color_image = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels);
    Ok(ctx.load_texture(
        format!("zapvis_image_{idx}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

fn parse_remote_input(input: &str) -> Option<(String, String)> {
    let re = Regex::new(r"^([^@]+@[^:]+):(/.+)$").ok()?;
    let caps = re.captures(input)?;
    Some((caps.get(1)?.as_str().to_string(), caps.get(2)?.as_str().to_string()))
}

fn build_remote_path(dir: &str, file_name: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        format!("/{}", file_name)
    } else {
        format!("{}/{}", trimmed, file_name)
    }
}

fn file_name_from_path(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Non-UTF8 filename not supported"))
}

fn file_name_from_str_path(path: &str) -> Result<String> {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Non-UTF8 filename not supported"))
}
