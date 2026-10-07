//! T1: render a raw file (NEF preferred) to PNG in-process with
//! `lightcraft-pipeline`, no subprocess.
//!
//! Usage: render <in.nef> <out.png> [--size N]
//!
//! The develop path mirrors `lightcraft-cli render` at the pinned revision:
//! raw defaults (`DevelopSettings::for_raw`, as-shot WB, capture sharpening),
//! then the CPU pipeline. Prints stage timings to stdout.

use std::time::Instant;

use lightcraft_develop::DevelopSettings;
use lightcraft_pipeline::RenderRequest;
use retouch_r0::lightcraft_bridge::load_bytes;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, output, size) = parse_args(&args)?;
    let bytes = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let file_mb = bytes.len() as f64 / 1e6;

    let t0 = Instant::now();
    let (src, info) = load_bytes(std::borrow::Cow::Owned(bytes), size.unwrap_or(usize::MAX))?;
    let load_ms = t0.elapsed().as_secs_f64() * 1e3;
    println!("load: {}×{} in {:.1} ms (file {file_mb:.1} MB)", src.width, src.height, load_ms);
    println!("source info: raw={} as_shot=({:.0},{:.1}) relative_wb={} camera_tone={}",
        info.raw, info.as_shot_temp, info.as_shot_tint, info.relative_wb, info.camera_tone.is_some());

    let settings = if info.raw { DevelopSettings::for_raw(info.as_shot_temp, info.as_shot_tint) } else { DevelopSettings::default() };
    let req = RenderRequest::fit(src.width, src.height);

    let t1 = Instant::now();
    let rendered = lightcraft_pipeline::render(&src, &info, &settings, &req);
    let render_ms = t1.elapsed().as_secs_f64() * 1e3;
    println!("render: {}×{} in {:.1} ms", rendered.image.width, rendered.image.height, render_ms);

    let t2 = Instant::now();
    let png = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&rendered.image), &lightcraft_codecs::EncodeMeta::default())
        .map_err(|e| format!("encode png: {e}"))?;
    let encode_ms = t2.elapsed().as_secs_f64() * 1e3;
    std::fs::write(&output, &png).map_err(|e| format!("{output}: {e}"))?;
    println!("encode: {} bytes PNG in {:.1} ms", png.len(), encode_ms);
    println!("total: {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    println!("wrote {output}");
    Ok(())
}

fn parse_args(args: &[String]) -> Result<(String, String, Option<usize>), String> {
    let mut input = None;
    let mut output = None;
    let mut size = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                i += 1;
                let v = args.get(i).ok_or("--size needs a value")?;
                size = Some(v.parse().map_err(|e| format!("--size: {e}"))?);
            }
            a if a.starts_with('-') => return Err(format!("unknown option {a}")),
            f if input.is_none() => input = Some(f.to_string()),
            f if output.is_none() => output = Some(f.to_string()),
            f => return Err(format!("unexpected argument {f}")),
        }
        i += 1;
    }
    let input = input.ok_or("usage: render <in.nef> <out.png> [--size N]")?;
    let output = output.ok_or("usage: render <in.nef> <out.png> [--size N]")?;
    Ok((input, output, size))
}
