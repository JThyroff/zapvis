use anyhow::{anyhow, Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use eframe::egui;
use egui::{ColorImage, TextureHandle};
use image::GenericImageView;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// zapviz: sequence-only image viewer.
/// Opens a file, matches it against configured patterns with # as digit placeholders,
/// then navigates by changing the numeric id and stat()'ing the constructed filename.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Image file to open (recommended). Folder mode is intentionally not supported.
    input: PathBuf,

    /// Optional pattern override, e.g. "########_#.png"
    #[arg(long)]
    pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    patterns: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let input = args.input;
    if !input.is_file() {
        return Err(anyhow!(
            "Input must be an image FILE path. Folder mode is intentionally not supported."
        ));
    }

    let mut cfg = load_config().unwrap_or_default();

    // If user provided --pattern, try it first and store it if it works.
    if let Some(pat) = args.pattern.clone() {
        if pattern_matches_file(&pat, &input)? {
            maybe_add_pattern(&mut cfg, pat);
            save_config(&cfg).ok(); // ignore save errors (still can run)
        } else {
            return Err(anyhow!(
                "Provided --pattern did not match the input filename. Nothing saved."
            ));
        }
    }

    // Determine which pattern to use:
    let (pattern, seq) = match pick_sequence(&cfg, &input) {
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
        "zapviz",
        native_options,
        Box::new(|cc| Ok(Box::new(ZapVizApp::new(cc, pattern, seq)))),
    )
    .map_err(|e| anyhow!(e.to_string()))?;

    Ok(())
}

/// Represents a compiled sequence extracted from a filename pattern and a concrete file.
#[derive(Debug, Clone)]
struct SequenceSpec {
    dir: PathBuf,
    prefix: String,
    width: usize,
    suffix: String,
    index: u64,
}

impl SequenceSpec {
    fn path_for(&self, idx: u64) -> PathBuf {
        let name = format!("{}{:0width$}{}", self.prefix, idx, self.suffix, width = self.width);
        self.dir.join(name)
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
    let proj = ProjectDirs::from("dev", "zapviz", "zapviz")
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    Ok(proj.config_dir().join("config.toml"))
}

fn maybe_add_pattern(cfg: &mut Config, pat: String) {
    if !cfg.patterns.iter().any(|p| p == &pat) {
        cfg.patterns.push(pat);
    }
}

fn pattern_matches_file(pat: &str, file: &Path) -> Result<bool> {
    let file_name = file
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("Non-UTF8 filename not supported"))?;

    let (re, _, _, _) = compile_pattern(pat)?;
    Ok(re.is_match(file_name))
}

/// Try patterns from config; return first that matches AND has neighbor evidence.
/// Evidence: current file matches and at least one neighbor exists (idx±1).
fn pick_sequence(cfg: &Config, input: &Path) -> Result<(String, SequenceSpec)> {
    // If config empty, fail quickly.
    if cfg.patterns.is_empty() {
        return Err(anyhow!("No patterns configured."));
    }

    let file_name = input
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("Non-UTF8 filename not supported"))?
        .to_string();

    let dir = input
        .parent()
        .ok_or_else(|| anyhow!("Input has no parent directory"))?
        .to_path_buf();

    for pat in &cfg.patterns {
        let (re, prefix, width, suffix) = compile_pattern(pat)?;
        if let Some(cap) = re.captures(&file_name) {
            let idx_str = cap.get(1).unwrap().as_str();
            let idx: u64 = idx_str.parse().context("Failed to parse captured index")?;

            let spec = SequenceSpec {
                dir: dir.clone(),
                prefix,
                width,
                suffix,
                index: idx,
            };

            // Neighbor evidence via stat(): cheap and avoids enumeration.
            let has_next = spec.path_for(idx + 1).exists();
            let has_prev = idx > 0 && spec.path_for(idx - 1).exists();

            if has_next || has_prev {
                return Ok((pat.clone(), spec));
            }
        }
    }

    Err(anyhow!("No configured pattern matched with neighbor evidence."))
}

// ---------------- GUI app ----------------

struct ZapVizApp {
    pattern: String,
    seq: SequenceSpec,

    texture: Option<TextureHandle>,
    status: String,
}

impl ZapVizApp {
    fn new(_cc: &eframe::CreationContext<'_>, pattern: String, seq: SequenceSpec) -> Self {
        Self {
            pattern,
            seq,
            texture: None,
            status: String::new(),
        }
    }

    fn load_current(&mut self, ctx: &egui::Context) {
        let path = self.seq.path_for(self.seq.index);
        match load_image_to_texture(ctx, &path) {
            Ok(tex) => {
                self.texture = Some(tex);
                self.status = format!("{}  (pattern: {})", path.display(), self.pattern);
            }
            Err(e) => {
                self.texture = None;
                self.status = format!("Failed to load {}: {e}", path.display());
            }
        }
    }

    fn try_step(&mut self, ctx: &egui::Context, delta: i64) {
        let cur = self.seq.index as i64;
        let next = cur + delta;
        if next < 0 {
            return;
        }
        let next_u = next as u64;
        let p = self.seq.path_for(next_u);
        if p.exists() {
            self.seq.index = next_u;
            self.load_current(ctx);
        } else {
            // strict: don't scan; just stop
            self.status = format!("No file: {}", p.display());
        }
    }
}

impl eframe::App for ZapVizApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Load initial image once
        if self.texture.is_none() && self.status.is_empty() {
            self.load_current(ctx);
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
            ui.label("Keys: ←/→ or A/D. Esc closes the window.");
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = &self.texture {
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

fn load_image_to_texture(ctx: &egui::Context, path: &Path) -> Result<TextureHandle> {
    let img = image::open(path).with_context(|| format!("image::open failed for {}", path.display()))?;
    let rgba = img.to_rgba8();
    let (w, h) = img.dimensions();
    let pixels = rgba.into_raw();

    let color_image = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels);
    Ok(ctx.load_texture(
        "zapviz_image",
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}
