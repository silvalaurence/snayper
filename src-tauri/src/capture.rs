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

/// Grab just the watched rectangle straight off the screen with GDI BitBlt.
/// xcap re-creates its capture session on every call (~0.7s for a full
/// monitor), which dominated watch latency; this path is a few milliseconds.
#[cfg(windows)]
pub fn capture_region(x: i64, y: i64, w: u32, h: u32) -> Result<Shot> {
    #[link(name = "user32")]
    extern "system" {
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn CreateCompatibleBitmap(hdc: isize, w: i32, h: i32) -> isize;
        fn SelectObject(hdc: isize, obj: isize) -> isize;
        fn BitBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, rop: u32) -> i32;
        fn GetDIBits(hdc: isize, hbm: isize, start: u32, lines: u32, bits: *mut u8, bmi: *mut BitmapInfo, usage: u32) -> i32;
        fn DeleteObject(obj: isize) -> i32;
        fn DeleteDC(hdc: isize) -> i32;
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_pels: i32,
        y_pels: i32,
        clr_used: u32,
        clr_important: u32,
    }
    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    const SRCCOPY: u32 = 0x00CC_0020;
    const DIB_RGB_COLORS: u32 = 0;

    let w = w.max(1).min(i32::MAX as u32) as i32;
    let h = h.max(1).min(i32::MAX as u32) as i32;

    unsafe {
        let screen = GetDC(0);
        if screen == 0 {
            return Err(anyhow!("GetDC failed"));
        }
        let mem = CreateCompatibleDC(screen);
        if mem == 0 {
            ReleaseDC(0, screen);
            return Err(anyhow!("CreateCompatibleDC failed"));
        }
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if bmp == 0 {
            DeleteDC(mem);
            ReleaseDC(0, screen);
            return Err(anyhow!("CreateCompatibleBitmap failed"));
        }

        let old = SelectObject(mem, bmp);
        let copied = BitBlt(mem, 0, 0, w, h, screen, x as i32, y as i32, SRCCOPY);

        let mut result = Err(anyhow!("BitBlt failed"));
        if copied != 0 {
            // Negative height asks GDI for top-down rows, matching our other shots.
            let mut info: BitmapInfo = std::mem::zeroed();
            info.header.size = std::mem::size_of::<BitmapInfoHeader>() as u32;
            info.header.width = w;
            info.header.height = -h;
            info.header.planes = 1;
            info.header.bit_count = 32;
            info.header.compression = 0; // BI_RGB

            let mut buf = vec![0u8; w as usize * h as usize * 4];
            let lines = GetDIBits(
                mem,
                bmp,
                0,
                h as u32,
                buf.as_mut_ptr(),
                &mut info,
                DIB_RGB_COLORS,
            );
            if lines != 0 {
                // GDI hands back BGRA; the rest of the app speaks RGBA.
                for px in buf.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                result = Ok(Shot {
                    width: w as u32,
                    height: h as u32,
                    rgba: buf,
                });
            }
        }

        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(0, screen);
        result
    }
}

/// Non-Windows fallback: take the whole monitor and slice the rectangle out.
#[cfg(not(windows))]
pub fn capture_region(x: i64, y: i64, w: u32, h: u32) -> Result<Shot> {
    let shot = capture_primary()?;
    let x = x.clamp(0, shot.width as i64 - 1) as u32;
    let y = y.clamp(0, shot.height as i64 - 1) as u32;
    let w = w.max(1).min(shot.width - x);
    let h = h.max(1).min(shot.height - y);
    let full = RgbaImage::from_raw(shot.width, shot.height, shot.rgba)
        .ok_or_else(|| anyhow!("screenshot buffer has the wrong size"))?;
    let cropped = image::imageops::crop_imm(&full, x, y, w, h).to_image();
    Ok(Shot {
        width: cropped.width(),
        height: cropped.height(),
        rgba: cropped.into_raw(),
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
