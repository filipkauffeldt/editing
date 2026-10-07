//! T2: output parity between the in-process render (T1) and
//! `lightcraft-cli render` of the same file at the same revision.
//!
//! Usage: parity <a.png> <b.png>
//!
//! Decodes both PNGs and reports per-channel max/mean absolute difference and
//! the share of differing pixels. Exit code 0 when the images are
//! pixel-identical.

use retouch_r0::photocraft_bridge::compare;
use retouch_r0::photocraft_bridge::surface_from_rgba8;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: parity <a.png> <b.png>".to_string());
    }
    let (pa, pb) = (&args[0], &args[1]);
    let load = |p: &str| -> Result<lightcraft_raster::Rgba8, String> {
        let bytes = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
        let img = image::load_from_memory(&bytes).map_err(|e| format!("{p}: {e}"))?.to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut out = lightcraft_raster::Rgba8::new(w, h);
        for (i, p) in img.pixels().enumerate() {
            out.data[i] = [p[0], p[1], p[2], p[3]];
        }
        Ok(out)
    };
    let a = load(pa)?;
    let b = load(pb)?;
    println!("{}: {}×{} ({} bytes)", pa, a.width, a.height, std::fs::metadata(pa).map(|m| m.len()).unwrap_or(0));
    println!("{}: {}×{} ({} bytes)", pb, b.width, b.height, std::fs::metadata(pb).map(|m| m.len()).unwrap_or(0));
    if (a.width, a.height) != (b.width, b.height) {
        println!("FAIL: dimensions differ");
        std::process::exit(1);
    }
    let (mean_abs, share_diff) = compare(&a, &b).ok_or("dimensions differ")?;
    let mut max_abs = 0f64;
    let mut max_at = (0usize, 0usize);
    for (i, (x, y)) in a.data.iter().zip(&b.data).enumerate() {
        for c in 0..4 {
            let d = (x[c] as f64 - y[c] as f64).abs();
            if d > max_abs {
                max_abs = d;
                max_at = (i, c);
            }
        }
    }
    println!(
        "mean |diff|/channel: {mean_abs:.6}  max |diff|: {max_abs} (px {}, ch {})  differing channel samples: {:.6} %",
        max_at.0,
        max_at.1,
        share_diff * 100.0
    );
    // exercise the surface bridge while we are here (used by T4)
    let s = surface_from_rgba8(&a);
    let back = retouch_r0::photocraft_bridge::rgba8_from_surface(&s, photocraft_geom::Rect::from_xywh(0, 0, a.width as u32, a.height as u32));
    let (m, d) = compare(&a, &back).ok_or("roundtrip size mismatch")?;
    println!("surface roundtrip: mean {m:.6} differing channel samples {d:.6} %");
    if max_abs == 0.0 {
        println!("PASS: pixel-identical");
        Ok(())
    } else {
        println!("FAIL: pixels differ");
        std::process::exit(1);
    }
}
