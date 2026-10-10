//! A cheap stand-in for the straightened canvas while a straighten angle is
//! being dragged: the already-rendered image turned about its centre on a
//! canvas of the same size, with no develop pipeline involved.
//!
//! It follows the engine's convention (see [`edit_core::Crop`]): a positive
//! angle turns the image clockwise on screen. The pixels carry whatever
//! channel order the caller uses; only their layout matters. The result is a
//! preview, never an export: the committed crop is rendered by the pipeline.

use edit_core::Crop;
use image::RgbaImage;

/// What shows outside the turned image.
const OUTSIDE: [u8; 4] = [0, 0, 0, 255];

/// `src` turned clockwise by `degrees` about its centre, same size. Rows are
/// shared across the available cores.
pub fn straighten_preview(src: &RgbaImage, degrees: f32) -> RgbaImage {
    if degrees == 0.0 || src.width() == 0 || src.height() == 0 {
        return src.clone();
    }
    let (w, h) = src.dimensions();
    let mut out = RgbaImage::new(w, h);
    // Each output pixel samples the source turned back by the angle.
    let (sin, cos) = (-(degrees as f64)).to_radians().sin_cos();
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let src_bytes = src.as_raw();
    fill_rows(&mut out, |x, y| {
        let dx = x as f64 + 0.5 - cx;
        let dy = y as f64 + 0.5 - cy;
        sample(
            src_bytes,
            w,
            h,
            cx + dx * cos - dy * sin,
            cy + dx * sin + dy * cos,
        )
    });
    out
}

/// The picture a crop shows, cut from an already rendered image: turned by
/// the crop's quarter turns, straightened by its angle and cut to its frame,
/// then shrunk to at most `max_edge` on the long edge (never enlarged). This
/// is for thumbnails and the unedited view, which show the camera preview
/// without the develop pipeline; the committed crop renders through the
/// pipeline.
pub fn crop_preview(src: &RgbaImage, crop: &Crop, max_edge: u32) -> RgbaImage {
    let turned = match crop.quarter_turns % 4 {
        1 => Some(image::imageops::rotate90(src)),
        2 => Some(image::imageops::rotate180(src)),
        3 => Some(image::imageops::rotate270(src)),
        _ => None,
    };
    let turned = turned.as_ref().unwrap_or(src);
    let (w, h) = turned.dimensions();
    if w == 0 || h == 0 {
        return turned.clone();
    }
    let rect = crop.rect.clamped();
    let (rx, ry) = (rect.x as f64 * w as f64, rect.y as f64 * h as f64);
    let (rw, rh) = (
        (rect.w as f64 * w as f64).max(1.0),
        (rect.h as f64 * h as f64).max(1.0),
    );
    let scale = (max_edge as f64 / rw.max(rh)).min(1.0);
    let (ow, oh) = (
        ((rw * scale).round() as u32).max(1),
        ((rh * scale).round() as u32).max(1),
    );
    let mut out = RgbaImage::new(ow, oh);
    let (sin, cos) = (-(crop.rotation_degrees as f64)).to_radians().sin_cos();
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let bytes = turned.as_raw();
    fill_rows(&mut out, |x, y| {
        // The output pixel's place in the straightened canvas, then back
        // through the angle to the turned source.
        let px = rx + (x as f64 + 0.5) / ow as f64 * rw;
        let py = ry + (y as f64 + 0.5) / oh as f64 * rh;
        let (dx, dy) = (px - cx, py - cy);
        sample(
            bytes,
            w,
            h,
            cx + dx * cos - dy * sin,
            cy + dx * sin + dy * cos,
        )
    });
    out
}

/// Fill `out` pixel by pixel from `pixel(x, y)`, rows shared across threads.
fn fill_rows(out: &mut RgbaImage, pixel: impl Fn(u32, u32) -> [u8; 4] + Sync) {
    let (w, h) = out.dimensions();
    let rows = h as usize;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8);
    let per = rows.div_ceil(threads).max(1);
    let stride = w as usize * 4;
    let pixel = &pixel;
    std::thread::scope(|scope| {
        for (band, chunk) in out.chunks_mut(per * stride).enumerate() {
            scope.spawn(move || {
                for (i, row) in chunk.chunks_mut(stride).enumerate() {
                    let y = (band * per + i) as u32;
                    for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = pixel(x as u32, y);
                    }
                }
            });
        }
    });
}

/// Bilinear sample at source position `(x, y)` in pixel units, where pixel
/// centres sit at half integers.
fn sample(bytes: &[u8], w: u32, h: u32, x: f64, y: f64) -> [u8; 4] {
    if x < 0.0 || y < 0.0 || x > w as f64 || y > h as f64 {
        return OUTSIDE;
    }
    let (fx, fy) = (x - 0.5, y - 0.5);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
    let at = |ix: f64, iy: f64| {
        let ix = (ix.max(0.0) as usize).min(w as usize - 1);
        let iy = (iy.max(0.0) as usize).min(h as usize - 1);
        let i = (iy * w as usize + ix) * 4;
        [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
    };
    let (a, b, c, d) = (
        at(x0, y0),
        at(x0 + 1.0, y0),
        at(x0, y0 + 1.0),
        at(x0 + 1.0, y0 + 1.0),
    );
    let mut out = [0u8; 4];
    for k in 0..4 {
        let top = a[k] as f32 + (b[k] as f32 - a[k] as f32) * tx;
        let bottom = c[k] as f32 + (d[k] as f32 - c[k] as f32) * tx;
        out[k] = (top + (bottom - top) * ty).round() as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_y(img: &RgbaImage, x0: u32, x1: u32) -> f32 {
        let (mut sum, mut weight) = (0.0, 0.0);
        for x in x0..x1 {
            // Stay clear of the empty corners a turned canvas has.
            for y in 20..img.height() - 20 {
                let w = (255.0 - img.get_pixel(x, y)[0] as f32).max(0.0);
                sum += w * y as f32;
                weight += w;
            }
        }
        assert!(weight > 0.0);
        sum / weight
    }

    #[test]
    fn zero_degrees_is_the_source() {
        let img = RgbaImage::from_fn(9, 5, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
        assert_eq!(straighten_preview(&img, 0.0), img);
    }

    #[test]
    fn a_positive_angle_levels_a_line_that_rises_to_the_right() {
        let (w, h) = (240u32, 160u32);
        let slope = 6f32.to_radians().tan();
        let img = RgbaImage::from_fn(w, h, |x, y| {
            let line = 80.0 - slope * (x as f32 - 120.0);
            let dark = (y as f32 + 0.5 - line).abs() <= 1.5;
            let v = if dark { 0 } else { 255 };
            image::Rgba([v, v, v, 255])
        });
        let tilt = |img: &RgbaImage| line_y(img, 150, 154) - line_y(img, 86, 90);
        assert!(tilt(&img) < -3.0);
        assert!(tilt(&straighten_preview(&img, 6.0)).abs() < 1.0);
        assert!(tilt(&straighten_preview(&img, -6.0)) < -6.0);
    }

    fn quadrants(img: &RgbaImage) -> [u8; 4] {
        // Red channel at the middle of each quadrant: tl, tr, br, bl.
        let (w, h) = img.dimensions();
        [
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (3 * w / 4, 3 * h / 4),
            (w / 4, 3 * h / 4),
        ]
        .map(|(x, y)| img.get_pixel(x, y)[0])
    }

    fn marked() -> RgbaImage {
        // 80x40, four distinct quadrants.
        RgbaImage::from_fn(80, 40, |x, y| {
            let v = match (x < 40, y < 20) {
                (true, true) => 10,
                (false, true) => 80,
                (false, false) => 160,
                (true, false) => 240,
            };
            image::Rgba([v, 0, 0, 255])
        })
    }

    #[test]
    fn a_full_unturned_crop_is_the_source() {
        let img = marked();
        let out = crop_preview(&img, &Crop::default(), 1000);
        assert_eq!(out.dimensions(), (80, 40));
        assert_eq!(quadrants(&out), [10, 80, 160, 240]);
    }

    #[test]
    fn quarter_turns_move_the_quadrants_clockwise() {
        let img = marked();
        for (turns, size, want) in [
            (1, (40, 80), [240, 10, 80, 160]),
            (2, (80, 40), [160, 240, 10, 80]),
            (3, (40, 80), [80, 160, 240, 10]),
        ] {
            let crop = Crop {
                quarter_turns: turns,
                ..Crop::default()
            };
            let out = crop_preview(&img, &crop, 1000);
            assert_eq!(out.dimensions(), size, "{turns} turns");
            assert_eq!(quadrants(&out), want, "{turns} turns");
        }
    }

    #[test]
    fn the_frame_cuts_the_turned_picture_and_the_size_is_capped() {
        let img = marked();
        // The lower half of the turned (portrait) picture.
        let crop = Crop {
            rect: edit_core::Rect::new(0.0, 0.5, 1.0, 0.5),
            quarter_turns: 1,
            ..Crop::default()
        };
        let out = crop_preview(&img, &crop, 1000);
        assert_eq!(out.dimensions(), (40, 40));
        assert_eq!(quadrants(&out), [160, 80, 80, 160]);
        // Capped, never enlarged.
        let small = crop_preview(&img, &Crop::default(), 20);
        assert_eq!(small.dimensions(), (20, 10));
        let same = crop_preview(&img, &Crop::default(), 10_000);
        assert_eq!(same.dimensions(), (80, 40));
    }

    #[test]
    fn a_straightened_crop_matches_the_straighten_preview() {
        let img = RgbaImage::from_fn(120, 80, |x, y| {
            image::Rgba([(x * 2) as u8, (y * 3) as u8, 9, 255])
        });
        let crop = Crop {
            rect: edit_core::Rect::new(0.2, 0.25, 0.5, 0.5),
            rotation_degrees: 8.0,
            ..Crop::default()
        };
        let cut = crop_preview(&img, &crop, 10_000);
        let turned = straighten_preview(&img, 8.0);
        assert_eq!(cut.dimensions(), (60, 40));
        for (x, y) in [(5, 5), (30, 20), (55, 35)] {
            let a = cut.get_pixel(x, y);
            let b = turned.get_pixel(24 + x, 20 + y);
            for k in 0..3 {
                assert!(
                    (a[k] as i32 - b[k] as i32).abs() <= 2,
                    "{x},{y}: {a:?} {b:?}"
                );
            }
        }
    }

    #[test]
    fn the_corners_of_a_turned_canvas_are_empty() {
        let img = RgbaImage::from_pixel(100, 60, image::Rgba([200, 100, 50, 255]));
        let out = straighten_preview(&img, 10.0);
        assert_eq!(out.get_pixel(0, 0).0, OUTSIDE);
        assert_eq!(out.get_pixel(50, 30).0, [200, 100, 50, 255]);
        assert_eq!(out.dimensions(), (100, 60));
    }
}
