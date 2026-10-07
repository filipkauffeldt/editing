//! The one boundary where LightCraft and PhotoCraft types meet: 8-bit RGBA
//! images from the render pipeline become photocraft tiled surfaces and back.
//! Keep every conversion here (the two families both ship `raster`/`color`/
//! `geom` crates; mixing them anywhere else would be confusing).

use lightcraft_raster::Rgba8;
use photocraft_color::PixelFormat;
use photocraft_geom::Rect;
use photocraft_raster::Surface;

pub const RGBA8: PixelFormat = PixelFormat::RGBA8;

/// Whole-image surface from a rendered image (area = 0,0,w,h).
pub fn surface_from_rgba8(img: &Rgba8) -> Surface {
    let bytes = img.as_bytes();
    Surface::from_interleaved(RGBA8, Rect::from_xywh(0, 0, img.width as u32, img.height as u32), &bytes)
}

/// Render image from a whole-image surface.
pub fn rgba8_from_surface(s: &Surface, rect: Rect) -> Rgba8 {
    let w = rect.width() as usize;
    let h = rect.height() as usize;
    let bytes = s.to_interleaved(rect);
    Rgba8::from_bytes(w, h, &bytes).unwrap_or_else(|| Rgba8::new(w, h))
}

/// A selection surface that is fully selected inside `rect` (coverage 1 in
/// channel 0, which is what `photocraft-algo::apply` reads) and nothing
/// outside — the mask a retouch stage would blend through.
pub fn mask_surface(img_rect: Rect, sel_rect: Rect) -> Surface {
    let mut s = Surface::new(RGBA8);
    s.fill_rect(img_rect, &[0.0, 0.0, 0.0, 0.0]);
    let r = sel_rect.intersect(&img_rect);
    if !r.is_empty() {
        s.fill_rect(r, &[1.0, 0.0, 0.0, 0.0]);
    }
    s
}

/// Mean absolute per-channel difference between two RGBA images, plus the
/// share of pixels that differ at all. Used for tiling / size-consistency
/// checks.
pub fn compare(a: &Rgba8, b: &Rgba8) -> Option<(f64, f64)> {
    if (a.width, a.height) != (b.width, b.height) {
        return None;
    }
    let n = a.data.len();
    let mut sum = 0f64;
    let mut diff = 0usize;
    for (x, y) in a.data.iter().zip(&b.data) {
        for c in 0..4 {
            let d = (x[c] as f64 - y[c] as f64).abs();
            sum += d;
            if d > 0.0 {
                diff += 1;
            }
        }
    }
    Some((sum / (n * 4) as f64, diff as f64 / (n * 4) as f64))
}
