use eframe::egui;
use std::sync::mpsc::Sender;

use crate::image_cache::ImageCache;
use crate::remote_worker::{RemoteRange, RemoteWorkerRequest};
use crate::sequence::{SequenceSource, SequenceSpec};

pub struct ZapVisApp {
    pattern: String,
    seq: SequenceSpec,
    cache: ImageCache,
    status: String,
    step_size: u64,
    is_fullscreen: bool,
    saved_window_pos: Option<egui::Pos2>,
    saved_window_size: Option<egui::Vec2>,
}

impl ZapVisApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        pattern: String,
        seq: SequenceSpec,
        request_tx: Option<Sender<RemoteWorkerRequest>>,
        remote_range: RemoteRange,
    ) -> Self {
        let cache_remote_range = match &seq.source {
            SequenceSource::Remote { .. } => Some(remote_range),
            SequenceSource::Local(_) => None,
        };
        let cache = ImageCache::new(10, seq.source.clone(), request_tx, cache_remote_range);

        Self {
            pattern,
            seq,
            cache,
            status: String::new(),
            step_size: 1,
            is_fullscreen: false,
            saved_window_pos: None,
            saved_window_size: None,
        }
    }

    fn update_cache_and_status(&mut self, ctx: &egui::Context) {
        let (loaded, evicted) = self.cache.update_for_index(self.seq.index, &self.seq, ctx);

        let path = self.seq.path_display(self.seq.index);
        let idx = self.seq.index;

        if self.cache.get(idx).is_some() {
            // Image is cached and ready
            self.status = format!(
                "{}  (pattern: {})  |  {} | +{} -{} | step: {}",
                path,
                self.pattern,
                self.cache.cache_info(),
                loaded,
                evicted,
                self.step_size
            );
        } else if self.cache.is_pending(idx) {
            // Image is being loaded
            self.status = format!(
                "Loading {} | {} | step: {}",
                path,
                self.cache.cache_info(),
                self.step_size
            );
        } else {
            // Image not found or failed to load
            self.status = format!(
                "Not found / failed: {} | {} | +{} -{} | step: {}",
                path,
                self.cache.cache_info(),
                loaded,
                evicted,
                self.step_size
            );
        }
    }

    fn try_step(&mut self, ctx: &egui::Context, delta: i64) {
        let cur = self.seq.index as i64;
        let step = self.step_size as i64;
        let next = cur + delta * step;
        if next < 0 {
            return;
        }
        let next_u = next as u64;
        eprintln!("[Step] navigating from {} to {} (step={})", cur, next_u, step);

        // For local files, check existence first (fast, non-blocking)
        if let SequenceSource::Local(dir) = &self.seq.source {
            if !dir.join(self.seq.file_name_for(next_u)).exists() {
                let p = self.seq.path_display(next_u);
                self.status = format!("No file: {} | {}", p, self.cache.cache_info());
                eprintln!("[Step] file not found: {}", p);
                return;
            }
        }

        // For remote: proceed optimistically (don't block UI with recv())
        // The cache loader will attempt to fetch and show "Failed to load" if it doesn't exist
        self.seq.index = next_u;
        self.update_cache_and_status(ctx);
    }

    fn set_step_size(&mut self, new_step: u64, ctx: &egui::Context) {
        if new_step == self.step_size {
            return;
        }
        eprintln!("[Step] changing step size from {} to {}", self.step_size, new_step);
        self.step_size = new_step;
        
        // Update cache step size and clear cache except current image
        self.cache.set_step_size(new_step);
        self.cache.clear_except_current(self.seq.index);
        self.update_cache_and_status(ctx);
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        if self.is_fullscreen {
            // Restore to normal windowed mode
            eprintln!("[Fullscreen] Restoring to normal windowed mode");
            
            // First, un-maximize the window
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            
            // Then restore previous size and position if available
            if let Some(size) = self.saved_window_size {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
            if let Some(pos) = self.saved_window_pos {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
            
            self.is_fullscreen = false;
        } else {
            // Enter fullscreen (maximized) mode
            eprintln!("[Fullscreen] Entering fullscreen mode");
            
            // Save current window size and position before maximizing
            ctx.input(|i| {
                if let Some(viewport) = i.raw.viewports.get(&i.raw.viewport_id) {
                    self.saved_window_size = viewport.inner_rect.map(|r| r.size());
                    self.saved_window_pos = viewport.outer_rect.map(|r| r.min);
                }
            });
            
            // Maximize the window
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            
            self.is_fullscreen = true;
        }
    }
}

impl eframe::App for ZapVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process any decoded images from background threads
        self.cache.tick(ctx);

        // Load initial cache once
        if self.cache.is_empty() && self.status.is_empty() {
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

        // Step size selection (keys 0-9 for powers of 10)
        if input.key_pressed(egui::Key::Num0) {
            self.set_step_size(1, ctx);
        }
        if input.key_pressed(egui::Key::Num1) {
            self.set_step_size(10, ctx);
        }
        if input.key_pressed(egui::Key::Num2) {
            self.set_step_size(100, ctx);
        }
        if input.key_pressed(egui::Key::Num3) {
            self.set_step_size(1000, ctx);
        }
        if input.key_pressed(egui::Key::Num4) {
            self.set_step_size(10000, ctx);
        }
        if input.key_pressed(egui::Key::Num5) {
            self.set_step_size(100000, ctx);
        }
        if input.key_pressed(egui::Key::Num6) {
            self.set_step_size(1000000, ctx);
        }
        if input.key_pressed(egui::Key::Num7) {
            self.set_step_size(10000000, ctx);
        }
        if input.key_pressed(egui::Key::Num8) {
            self.set_step_size(100000000, ctx);
        }
        if input.key_pressed(egui::Key::Num9) {
            self.set_step_size(1000000000, ctx);
        }

        // Fullscreen toggle (F key)
        if input.key_pressed(egui::Key::F) {
            self.toggle_fullscreen(ctx);
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.label(&self.status);
            ui.label("Keys: Left/Right or A/D. 0-9 for step size. F for fullscreen. Esc closes the window.");
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = self.cache.get(self.seq.index) {
                let avail = ui.available_size();

                let tex_size = tex.size_vec2();
                // In fullscreen mode, allow scaling up to fill the window
                // In normal mode, cap at 1.0x to avoid upscaling
                let scale = if self.is_fullscreen {
                    (avail.x / tex_size.x).min(avail.y / tex_size.y)
                } else {
                    (avail.x / tex_size.x).min(avail.y / tex_size.y).min(1.0)
                };
                let size = tex_size * scale;

                ui.add(egui::Image::new(tex).fit_to_exact_size(size));
            } else {
                ui.label("No image loaded.");
            }
        });

        // Allow ESC to quit (closes SSH connection and stops all pending image loads)
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            eprintln!("[UI] ESC pressed, closing application");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
