//! T3: face-detector bake-off on 10-20 real crowd photos.
//!
//! Usage: facebake <gt.txt> --images <dir> [--yunet <model.onnx>]
//!
//! `gt.txt` is WIDER-Face format (path, count, x y w h …). Images live at
//! `<dir>/<path>`. Runs every compiled-in detector at one or more score
//! thresholds, reports recall by face-size bucket, CPU time and false
//! positives. Binary size is measured separately (see the memo).

use std::path::{Path, PathBuf};
use std::time::Instant;

use retouch_r0::detect_common::{BBox, evaluate, format_bucket_table, parse_wider_gt};

struct Config {
    gt: PathBuf,
    images: PathBuf,
    yunet_model: Option<PathBuf>,
}

fn main() -> Result<(), String> {
    let cfg = parse_args(std::env::args().skip(1).collect())?;
    let gt_text = std::fs::read_to_string(&cfg.gt).map_err(|e| format!("{}: {e}", cfg.gt.display()))?;
    let gt = parse_wider_gt(&gt_text).ok_or("failed to parse WIDER ground truth")?;
    println!("{} images in ground truth; using all present under {}", gt.len(), cfg.images.display());

    let mut runs: Vec<(String, Vec<retouch_r0::detect_common::ImageResult>)> = Vec::new();

    #[cfg(feature = "libfd")]
    {
        for (label, conf) in [("libfd conf>=0.2 (port defaults)", 0.2f32), ("libfd conf>=0.5", 0.5f32)] {
            let res = run_libfd(&gt, &cfg.images, conf)?;
            println!("\n== {label} ==");
            println!("{}", format_bucket_table(&res.0, res.1));
            runs.push((label.to_string(), res.0));
        }
    }
    #[cfg(not(feature = "libfd"))]
    println!("libfacedetection_rs not compiled (feature libfd off)");

    #[cfg(feature = "tract")]
    {
        let model = cfg.yunet_model.as_deref().ok_or("--yunet <model.onnx> is required with feature tract")?;
        for (label, conf) in [("yunet conf>=0.6 (crowd settings)", 0.6f32), ("yunet conf>=0.9 (OpenCV default)", 0.9f32)] {
            let res = run_yunet(&gt, &cfg.images, model, conf)?;
            println!("\n== {label} ==");
            println!("{}", format_bucket_table(&res.0, res.1));
            runs.push((label.to_string(), res.0));
        }
    }
    #[cfg(not(feature = "tract"))]
    println!("tract+YuNet not compiled (feature tract off)");

    if runs.is_empty() {
        return Err("no detector compiled: build with --features libfd or --features tract".to_string());
    }
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<Config, String> {
    let mut gt = None;
    let mut images = None;
    let mut yunet_model = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--images" => {
                i += 1;
                images = Some(PathBuf::from(args.get(i).ok_or("--images needs a value")?));
            }
            "--yunet" => {
                i += 1;
                yunet_model = Some(PathBuf::from(args.get(i).ok_or("--yunet needs a value")?));
            }
            a if a.starts_with('-') => return Err(format!("unknown option {a}")),
            f if gt.is_none() => gt = Some(PathBuf::from(f)),
            f => return Err(format!("unexpected argument {f}")),
        }
        i += 1;
    }
    let gt = gt.ok_or("usage: facebake <gt.txt> --images <dir> [--yunet <model.onnx>]")?;
    let images = images.ok_or("usage: facebake <gt.txt> --images <dir> [--yunet <model.onnx>]")?;
    Ok(Config { gt, images, yunet_model })
}

fn load_rgb8(path: &Path) -> Result<(Vec<u8>, usize, usize), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("{}: {e}", path.display()))?.to_rgb8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok((img.into_raw(), w, h))
}

#[cfg(feature = "libfd")]
fn run_libfd(
    gt: &[(String, Vec<(i32, i32, i32, i32)>)],
    images: &Path,
    conf: f32,
) -> Result<(Vec<retouch_r0::detect_common::ImageResult>, f64), String> {
    use libfacedetection_rs::Detector;
    let mut detector = Detector::new();
    let warm = image::DynamicImage::new_rgb8(64, 48);
    detector.detect(&warm).map_err(|e| format!("libfd warmup: {e}"))?;
    let mut rows = Vec::new();
    let mut total_ms = 0.0;
    for (rel, boxes) in gt {
        let path = images.join(rel);
        if !path.exists() {
            continue;
        }
        let (rgb, w, h) = load_rgb8(&path)?;
        let dyn_img = image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(w as u32, h as u32, rgb).ok_or("rgb wrap")?);
        let t = Instant::now();
        let detection = detector.detect(&dyn_img).map_err(|e| format!("{}: {e}", path.display()))?;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        total_ms += ms;
        let dets: Vec<BBox> = detection
            .faces()
            .iter()
            .filter(|f| f.score >= conf)
            .map(|f| BBox { x: f.x as f32, y: f.y as f32, w: f.width as f32, h: f.height as f32, score: f.score })
            .collect();
        let (buckets, fps) = evaluate(boxes, &dets, 0.5);
        rows.push(retouch_r0::detect_common::ImageResult { detections: dets.len(), buckets, false_positives: fps, ms });
    }
    Ok((rows, total_ms))
}

#[cfg(feature = "tract")]
fn run_yunet(
    gt: &[(String, Vec<(i32, i32, i32, i32)>)],
    images: &Path,
    model: &Path,
    conf: f32,
) -> Result<(Vec<retouch_r0::detect_common::ImageResult>, f64), String> {
    let mut yunet = retouch_r0::yunet::Yunet::load(model)?;
    let warm: Vec<u8> = vec![0u8; 64 * 48 * 3];
    yunet.detect_rgb8(&warm, 64, 48, conf, 0.3).map_err(|e| format!("yunet warmup: {e}"))?;
    let mut rows = Vec::new();
    let mut total_ms = 0.0;
    for (rel, boxes) in gt {
        let path = images.join(rel);
        if !path.exists() {
            continue;
        }
        let (rgb, w, h) = load_rgb8(&path)?;
        let t = Instant::now();
        let dets = yunet.detect_rgb8(&rgb, w, h, conf, 0.3).map_err(|e| format!("{}: {e}", path.display()))?;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        total_ms += ms;
        let (buckets, fps) = evaluate(boxes, &dets, 0.5);
        rows.push(retouch_r0::detect_common::ImageResult { detections: dets.len(), buckets, false_positives: fps, ms });
    }
    Ok((rows, total_ms))
}
