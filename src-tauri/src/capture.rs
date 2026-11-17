use std::io::Cursor;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{ImageFormat, RgbaImage};

/// A full-screen capture stored as raw RGBA so the app is not tied to a
/// specific `image` crate version.
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Capture the primary monitor. The overlay must be hidden when this runs.
pub fn capture_primary() -> Result<Shot> {
    let monitors = xcap::Monitor::all()?;
    let monitor = monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .ok_or_else(|| anyhow!("no primary monitor found"))?;

    let image = monitor.capture_image()?;
    let (width, height) = (image.width(), image.height());
    Ok(Shot {
        width,
        height,
        rgba: image.into_raw(),
    })
}

/// Crop `w`x`h` at `x`,`y` (physical pixels) out of the shot and return PNG bytes.
/// Coordinates are clamped to the screenshot bounds.
pub fn crop_png(shot: &Shot, x: i64, y: i64, w: u32, h: u32) -> Result<Vec<u8>> {
    if shot.width == 0 || shot.height == 0 {
        return Err(anyhow!("screenshot is empty"));
    }

    let x = x.clamp(0, shot.width as i64 - 1) as u32;
    let y = y.clamp(0, shot.height as i64 - 1) as u32;
    let w = w.max(1).min(shot.width - x);
    let h = h.max(1).min(shot.height - y);

    let full = RgbaImage::from_raw(shot.width, shot.height, shot.rgba.clone())
        .ok_or_else(|| anyhow!("screenshot buffer has the wrong size"))?;
    let mut cropped = image::imageops::crop_imm(&full, x, y, w, h).to_image();

    // Vision models do not need more than 1600px; smaller images are much faster.
    const MAX_WIDTH: u32 = 1600;
    if cropped.width() > MAX_WIDTH {
        let height =
            ((cropped.height() as u64 * MAX_WIDTH as u64) / cropped.width() as u64).max(1) as u32;
        cropped = image::imageops::resize(
            &cropped,
            MAX_WIDTH,
            height,
            image::imageops::FilterType::Triangle,
        );
    }

    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(cropped).write_to(&mut buf, ImageFormat::Png)?;
    Ok(buf.into_inner())
}

pub fn to_b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Cheap fingerprint of a screen region: changes when the pixels change.
pub fn crop_hash(shot: &Shot, x: i64, y: i64, w: u32, h: u32) -> u64 {
    if shot.width == 0 || shot.height == 0 {
        return 0;
    }
    let x = x.clamp(0, shot.width as i64 - 1) as u32;
    let y = y.clamp(0, shot.height as i64 - 1) as u32;
    let w = w.max(1).min(shot.width - x);
    let h = h.max(1).min(shot.height - y);

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for row in 0..h {
        let base = ((y + row) as usize * shot.width as usize + x as usize) * 4;
        let mut i = 0;
        while i + 2 < (w as usize) * 4 {
            hash ^= shot.rgba[base + i] as u64;
            hash = hash.wrapping_mul(0x1_0000_0001_b3);
            i += 32;
        }
    }
    hash
}
