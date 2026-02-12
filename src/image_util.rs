use anyhow::{Context, Result};
use egui::{ColorImage, TextureHandle};
use image::RgbaImage;
use std::path::Path;

/// Load and decode image to RGBA (can be done in background thread)
pub fn load_image_rgba(path: &Path) -> Result<RgbaImage> {
    let img = image::open(path)
        .with_context(|| format!("image::open failed for {}", path.display()))?;
    Ok(img.to_rgba8())
}

pub fn load_image_rgba_from_bytes(bytes: &[u8], source: &str) -> Result<RgbaImage> {
    let img = image::load_from_memory(bytes)
        .with_context(|| format!("image::load_from_memory failed for {}", source))?;
    Ok(img.to_rgba8())
}

/// Convert RgbaImage to egui TextureHandle (must be done on main thread with Context)
pub fn rgba_to_texture(ctx: &egui::Context, idx: u64, rgba: RgbaImage) -> Result<TextureHandle> {
    let (w, h) = rgba.dimensions();
    let pixels = rgba.into_raw();
    let color_image = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels);
    Ok(ctx.load_texture(
        format!("zapvis_image_{idx}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}
