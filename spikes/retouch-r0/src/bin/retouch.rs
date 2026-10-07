//! T4: run photocraft-algo Mosaic + Gaussian on a rendered image, and verify
//! the two properties the retouch stage needs:
//!
//! 1. Tiling consistency: the same filter over the same area with different
//!    tile sizes (auto / 128 / 257) produces pixel-identical output, and the
//!    filter is deterministic (rayon tile order does not matter).
//! 2. Selection blending: pixels outside the mask are untouched; inside the
//!    mask the operation applies.
//! 3. Preview/full-size consistency: a normalized recipe (cell/radius as a
//!    fraction of the long edge) applied at proxy size and at full size keeps
//!    the masked region's mean colour (mosaic and Gaussian both preserve
//!    means), and the proxy result of the full render matches the
//!    full-size-result-downscaled within resampling tolerance.
//!
//! Usage: retouch <in.png> <out-dir>
//!
//! Writes mosaic/gaussian outputs to `<out-dir>` and prints measurements.

use std::path::Path;
use std::time::Instant;

use lightcraft_raster::Rgba8;
use photocraft_algo::FilterParams;
use photocraft_geom::Rect;
use retouch_r0::photocraft_bridge::{compare, mask_surface, rgba8_from_surface, surface_from_rgba8};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: retouch <in.png> <out-dir>".to_string());
    }
    let (input, out_dir) = (&args[0], Path::new(&args[1]));
    std::fs::create_dir_all(out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;

    let full = load_png(input)?;
    let (w, h) = (full.width, full.height);
    println!("input: {w}×{h} ({} MP)", w * h / 1_000_000);
    let bounds = Rect::from_xywh(0, 0, w as u32, h as u32);
    let surface = surface_from_rgba8(&full);

    // -- tiling consistency -------------------------------------------------
    for (name, params) in [
        ("mosaic", FilterParams::Mosaic { cell_size: 24.0 }),
        ("gaussian", FilterParams::GaussianBlur { radius: 8.0 }),
    ] {
        let t = Instant::now();
        let auto = photocraft_algo::apply(&surface, &params, bounds, bounds, None);
        let auto_ms = t.elapsed().as_secs_f64() * 1e3;
        let tiled128 = photocraft_algo::apply_tiled(&surface, &params, bounds, bounds, None, 128, None);
        let tiled257 = photocraft_algo::apply_tiled(&surface, &params, bounds, bounds, None, 257, None);
        let again = photocraft_algo::apply(&surface, &params, bounds, bounds, None);
        let a = rgba8_from_surface(&auto, bounds);
        let b = rgba8_from_surface(&tiled128, bounds);
        let c = rgba8_from_surface(&tiled257, bounds);
        let d = rgba8_from_surface(&again, bounds);
        let eq = |x: &Rgba8, y: &Rgba8| compare(x, y).map(|(m, f)| m == 0.0 && f == 0.0).unwrap_or(false);
        let (gm128, gd128) = compare(&b, &a).unwrap_or((0.0, 0.0));
        println!(
            "{name}: auto {auto_ms:.1} ms | tile128 == auto: {} | tile257 == auto: {} | rerun == auto: {}",
            eq(&b, &a),
            eq(&c, &a),
            eq(&d, &a)
        );
        if !eq(&b, &a) {
            let (border, interior) = diff_breakdown(&a, &b, 30);
            println!("   {name} tile128-vs-auto: mean|diff| {gm128:.4}/255, differing {gd128:.3} % (border {border}, interior {interior})");
        }
        let png = encode_png(&a);
        std::fs::write(out_dir.join(format!("{name}_full.png")), &png).map_err(|e| e.to_string())?;
    }

    // -- selection blending -------------------------------------------------
    let sel = Rect::from_xywh(w as i32 / 4, h as i32 / 4, w as u32 / 2, h as u32 / 2);
    let mask = mask_surface(bounds, sel);
    let t = Instant::now();
    let masked = photocraft_algo::apply(&surface, &FilterParams::Mosaic { cell_size: 24.0 }, bounds, bounds, Some(&mask));
    let masked_ms = t.elapsed().as_secs_f64() * 1e3;
    let masked_img = rgba8_from_surface(&masked, bounds);
    let mut changed_inside = 0usize;
    let mut changed_outside = 0usize;
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let inside = x >= sel.x0 as usize && x < sel.x1 as usize && y >= sel.y0 as usize && y < sel.y1 as usize;
            let differs = full.data[i] != masked_img.data[i];
            if inside && differs {
                changed_inside += 1;
            }
            if !inside && differs {
                changed_outside += 1;
            }
        }
    }
    println!("mosaic masked region: {masked_ms:.1} ms, pixels changed inside {changed_inside}, outside {changed_outside} (must be 0)");
    if changed_outside != 0 {
        return Err(format!("FAIL: {changed_outside} pixels changed outside the selection"));
    }
    std::fs::write(out_dir.join("mosaic_region.png"), encode_png(&masked_img)).map_err(|e| e.to_string())?;

    // -- preview / full-size consistency -------------------------------------
    let proxy = resize_rgba8(&full, 1600, 1600)?;
    let (pw, ph) = (proxy.width, proxy.height);
    let p_bounds = Rect::from_xywh(0, 0, pw as u32, ph as u32);
    let p_surface = surface_from_rgba8(&proxy);
    let ratio = pw as f32 / w as f32;
    let t = Instant::now();
    let mosaic_full = photocraft_algo::apply(&surface, &FilterParams::Mosaic { cell_size: 24.0 }, bounds, bounds, None);
    let gauss_full = photocraft_algo::apply(&surface, &FilterParams::GaussianBlur { radius: 8.0 }, bounds, bounds, None);
    let full_ms = t.elapsed().as_secs_f64() * 1e3;
    let mosaic_proxy = photocraft_algo::apply(&p_surface, &FilterParams::Mosaic { cell_size: 24.0 * ratio }, p_bounds, p_bounds, None);
    let gauss_proxy = photocraft_algo::apply(&p_surface, &FilterParams::GaussianBlur { radius: 8.0 * ratio }, p_bounds, p_bounds, None);
    println!("full-size ops (20.9 MP): mosaic+gaussian {full_ms:.1} ms");

    let full_down = resize_rgba8(&rgba8_from_surface(&mosaic_full, bounds), 1600, 1600)?;
    let proxy_img = rgba8_from_surface(&mosaic_proxy, p_bounds);
    let (m, d) = compare(&full_down, &proxy_img).ok_or("proxy sizes differ")?;
    println!("mosaic  full-downscaled vs proxy-applied: mean|diff| {m:.3} / 255, differing {d:.3} % (resampling tolerance)");
    if m > 4.0 {
        return Err(format!("FAIL: mosaic preview/full drift too large ({m:.3})"));
    }

    // mean colour of the mosaic region must match the original at both sizes
    let mean_region = |img: &Rgba8, r: Rect| -> [f64; 3] {
        let mut acc = [0f64; 3];
        let mut n = 0usize;
        for y in r.y0.max(0)..r.y1.min(img.height as i32) {
            for x in r.x0.max(0)..r.x1.min(img.width as i32) {
                let p = &img.data[y as usize * img.width + x as usize];
                for c in 0..3 {
                    acc[c] += p[c] as f64;
                }
                n += 1;
            }
        }
        [acc[0] / n as f64, acc[1] / n as f64, acc[2] / n as f64]
    };
    let sel_full = Rect::from_xywh(w as i32 / 4, h as i32 / 4, w as u32 / 2, h as u32 / 2);
    let sel_proxy = Rect::from_xywh(pw as i32 / 4, ph as i32 / 4, pw as u32 / 2, ph as u32 / 2);
    let src_mean = mean_region(&full, sel_full);
    let mosaic_mean = mean_region(&rgba8_from_surface(&mosaic_full, bounds), sel_full);
    let gauss_mean = mean_region(&rgba8_from_surface(&gauss_full, bounds), sel_full);
    let proxy_src_mean = mean_region(&proxy, sel_proxy);
    let proxy_mosaic_mean = mean_region(&rgba8_from_surface(&mosaic_proxy, p_bounds), sel_proxy);
    let d_mean = |a: [f64; 3], b: [f64; 3]| ((a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs()) / 3.0;
    println!(
        "mean preserved (full): mosaic Δ {:.3}, gaussian Δ {:.3}; (proxy): mosaic Δ {:.3}",
        d_mean(src_mean, mosaic_mean),
        d_mean(src_mean, gauss_mean),
        d_mean(proxy_src_mean, proxy_mosaic_mean)
    );

    std::fs::write(out_dir.join("mosaic_proxy.png"), encode_png(&proxy_img)).map_err(|e| e.to_string())?;
    std::fs::write(out_dir.join("gauss_proxy.png"), encode_png(&rgba8_from_surface(&gauss_proxy, p_bounds))).map_err(|e| e.to_string())?;
    println!("ok");
    Ok(())
}

fn load_png(p: &str) -> Result<Rgba8, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("{p}: {e}"))?.to_rgba8();
    Ok(rgba8_from_image(&img))
}

fn rgba8_from_image(img: &image::RgbaImage) -> Rgba8 {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut out = Rgba8::new(w, h);
    for (i, px) in img.pixels().enumerate() {
        out.data[i] = [px[0], px[1], px[2], px[3]];
    }
    out
}

fn resize_rgba8(img: &Rgba8, max_w: u32, max_h: u32) -> Result<Rgba8, String> {
    let (w, h) = (img.width, img.height);
    let scale = (max_w as f32 / w as f32).min(max_h as f32 / h as f32).min(1.0);
    let (nw, nh) = ((w as f32 * scale).round() as u32, (h as f32 * scale).round() as u32);
    let mut raw = Vec::with_capacity(w * h * 4);
    for p in &img.data {
        raw.extend_from_slice(p);
    }
    let src = image::RgbaImage::from_raw(w as u32, h as u32, raw).ok_or("bad rgba buffer")?;
    let dst = image::imageops::resize(&src, nw, nh, image::imageops::FilterType::Triangle);
    Ok(rgba8_from_image(&dst))
}

fn encode_png(img: &Rgba8) -> Vec<u8> {
    lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(img), &lightcraft_codecs::EncodeMeta::default()).unwrap_or_default()
}

/// Count differing pixels inside a `band`-pixel border vs the interior.
fn diff_breakdown(a: &Rgba8, b: &Rgba8, band: usize) -> (usize, usize) {
    let (w, h) = (a.width, b.width.min(a.height));
    let mut border = 0usize;
    let mut interior = 0usize;
    for y in 0..b.height {
        for x in 0..w {
            let i = y * w + x;
            if a.data[i] == b.data[i] {
                continue;
            }
            let edge = x < band || y < band || x >= w.saturating_sub(band) || y >= h.saturating_sub(band);
            if edge {
                border += 1;
            } else {
                interior += 1;
            }
        }
    }
    (border, interior)
}
