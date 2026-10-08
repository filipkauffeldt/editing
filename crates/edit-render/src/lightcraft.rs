//! The raw decode → develop-source path, adapted from LightCraft's engine so
//! an in-process render is pixel-identical to `lightcraft-cli render` at the
//! same revision.
//!
//! Adapted from `crates/engine/src/files.rs` and
//! `crates/engine/src/camera_preview.rs` at rev
//! `f2f449611d248a907f755d6e18f3701db5755f03` (MIT OR Apache-2.0,
//! https://github.com/storytold/lightcraft). Only visibility was changed
//! (`pub(crate)` → `pub`) and the plain-file tail now decodes JPEG/PNG
//! through the `image` crate (zune-jpeg inside image 0.25) while the
//! camera-look reference decode keeps `lightcraft-codecs` for decoder-level
//! scaling. The math is untouched. This is the bridge the app's render
//! service wraps, and the memo records what it cost.

use std::borrow::Cow;

use lightcraft_codecs::DecodeOptions;
use lightcraft_color::{Mat3, cct::xy_to_temp_tint, luminance_2020};
use lightcraft_geom::Orientation;
use lightcraft_pipeline::tone::ToneMap;
use lightcraft_pipeline::{SourceInfo, tone::CameraTone};
use lightcraft_raster::Rgb32f;
use lightcraft_raster::resample::{Filter, fit};
use lightcraft_raw::{Method, RawFormat, color::CameraTransform};

/// Sensor clip level (normalised) for highlight reconstruction.
const HIGHLIGHT_CLIP: f32 = 0.99;

/// The look fitted to a raw's own camera JPEG (matrix + tone curve).
#[derive(Clone, Copy, Debug)]
pub struct CameraLook {
    pub matrix: Mat3,
    pub tone: CameraTone,
}

/// Raw formats whose decoder supplies vendor white-balance multipliers but no
/// camera colour matrix: their starting look is fitted to the file's own JPEG,
/// and white balance is relative to the as-shot look.
pub fn file_local_look(format: RawFormat) -> bool {
    matches!(format, RawFormat::Arw | RawFormat::Nef | RawFormat::Nrw)
}

fn is_lens_opcode(op: &lightcraft_raw::Opcode) -> bool {
    matches!(
        op,
        lightcraft_raw::Opcode::WarpRectilinear { .. }
            | lightcraft_raw::Opcode::FixVignetteRadial { .. }
    )
}

/// Lens corrections embedded in a DNG's `OpcodeList3` (`WarpRectilinear`,
/// `FixVignetteRadial`), re-expressed for the default-cropped, EXIF-oriented
/// image.
pub fn embedded_lens(raw: &lightcraft_raw::RawInfo) -> Option<lightcraft_develop::EmbeddedLens> {
    use lightcraft_develop::{EmbeddedLens, EmbeddedVignette, EmbeddedWarp};
    use lightcraft_geom::Point;
    let (aw, ah) = (raw.active_area.width as f64, raw.active_area.height as f64);
    if aw < 2.0 || ah < 2.0 {
        return None;
    }
    let c = raw
        .crop
        .clipped(raw.active_area.width, raw.active_area.height);
    let (cx0, cy0, cw, ch) = if c.width == 0 || c.height == 0 {
        (0.0, 0.0, aw, ah)
    } else {
        (c.x as f64, c.y as f64, c.width as f64, c.height as f64)
    };
    let long = cw.max(ch);
    let centre = |rel: [f64; 2]| -> (Point, f64) {
        let (px, py) = (rel[0] * (aw - 1.0), rel[1] * (ah - 1.0));
        let m = [
            (0.0, 0.0),
            (aw - 1.0, 0.0),
            (0.0, ah - 1.0),
            (aw - 1.0, ah - 1.0),
        ]
        .iter()
        .map(|&(x, y)| (x - px).hypot(y - py))
        .fold(0.0, f64::max)
        .max(1e-9);
        (
            Point::new((px + 0.5 - cx0) / cw, (py + 0.5 - cy0) / ch),
            m / long,
        )
    };
    let mut lens = EmbeddedLens::default();
    for op in &raw.opcodes.list3 {
        match op {
            lightcraft_raw::Opcode::WarpRectilinear { planes, center }
                if !planes.is_empty() && lens.warp.is_none() =>
            {
                let (center, radius) = centre(*center);
                let p = |i: usize| planes[i.min(planes.len() - 1)];
                lens.warp = Some(EmbeddedWarp {
                    planes: [p(0), p(1), p(2)],
                    center,
                    radius,
                });
            }
            lightcraft_raw::Opcode::FixVignetteRadial { k, center } if lens.vignette.is_none() => {
                let (center, radius) = centre(*center);
                lens.vignette = Some(EmbeddedVignette {
                    k: *k,
                    center,
                    radius,
                });
            }
            _ => {}
        }
    }
    if lens.warp.is_none() && lens.vignette.is_none() {
        return None;
    }
    Some(lightcraft_pipeline::optics::reorient_lens(
        &lens,
        raw.orientation,
        cw,
        ch,
    ))
}

/// The largest block size to bin a raw's mosaic by for a source of at most
/// `max_edge` pixels (90 % floor; X-Trans accepts 3× down to 75 %).
pub fn bin_factor(raw: &lightcraft_raw::RawImage, max_edge: usize) -> Option<usize> {
    let c = raw
        .crop
        .clipped(raw.active_area.width, raw.active_area.height);
    let long = if c.width > 1 && c.height > 1 {
        c.width.max(c.height)
    } else {
        raw.active_area.width.max(raw.active_area.height)
    };
    let need = (max_edge.saturating_mul(9) / 10).max(1);
    let need3 = (max_edge.saturating_mul(3) / 4).max(1);
    [8usize, 6, 4, 3, 2].into_iter().find(|&k| {
        raw.can_bin(k) && (long / k >= need || (k == 3 && !raw.can_bin(2) && long / k >= need3))
    })
}

/// A DNG `ProfileToneCurve` as the finish stage's camera tone curve.
pub fn dng_tone_curve(curve: &lightcraft_raw::profile::ToneCurve) -> Option<CameraTone> {
    let knots: [[f32; 2]; 32] = std::array::from_fn(|i| {
        let x = 2f32.powf(-12.0 + 12.0 * i as f32 / 31.0);
        [x, curve.eval(x).min(0.9995)]
    });
    CameraTone::new(knots)
}

fn luma(p: [f64; 3]) -> f64 {
    p[0] * 0.2627 + p[1] * 0.6780 + p[2] * 0.0593
}

fn displayed(scene: [f64; 3], tone: &ToneMap) -> [f64; 3] {
    let scene = scene.map(|v| v.max(0.0));
    let y = luma(scene);
    if y <= 0.0 {
        return [0.0; 3];
    }
    scene.map(|v| v * f64::from(tone.apply(y as f32)) / y)
}

/// A fit must cut the held-out squared error to below this share of the fallback's.
const MIN_IMPROVEMENT: f64 = 0.7;
/// ...and stay within this per-channel RMS of the camera JPEG (linear display values).
const MAX_HOLDOUT_RMS: f64 = 0.10;

fn fit_pairs(sensor: &Rgb32f, reference: &Rgb32f) -> Option<CameraLook> {
    if (sensor.width, sensor.height) != (reference.width, reference.height)
        || sensor.data.len() != reference.data.len()
    {
        return None;
    }
    let mut pairs = Vec::new();
    let mut colour = 0;
    for (input, output) in sensor.data.iter().zip(&reference.data) {
        let y = luminance_2020(*output);
        if !input
            .iter()
            .all(|v| v.is_finite() && *v > 0.001 && *v < 1.5)
            || !output
                .iter()
                .all(|v| v.is_finite() && *v > 0.004 && *v < 0.98)
            || !(0.015..0.85).contains(&y)
        {
            continue;
        }
        let min = output.iter().copied().fold(f32::INFINITY, f32::min);
        let max = output.iter().copied().fold(0.0, f32::max);
        colour += usize::from(max - min > 0.05);
        pairs.push((input.map(f64::from), output.map(f64::from)));
    }
    if pairs.len() < 256 || colour < pairs.len() / 20 {
        return None;
    }
    let mut gram = [[0.0; 3]; 3];
    let mut cross = [[0.0; 3]; 3];
    for (i, (x, y)) in pairs.iter().enumerate() {
        if i % 3 == 0 {
            continue;
        }
        let (lx, ly) = (luma(*x), luma(*y));
        for row in 0..3 {
            for col in 0..3 {
                gram[row][col] += x[row] * x[col] / (lx * lx);
                cross[row][col] += y[row] * x[col] / (ly * lx);
            }
        }
    }
    let trace: f64 = (0..3).map(|i| gram[i][i]).sum();
    let inverse = Mat3(gram).inverse()?;
    let condition = trace * (0..3).map(|i| inverse.0[i][i].abs()).sum::<f64>();
    if trace <= 0.0 || !condition.is_finite() || condition > 1e6 {
        return None;
    }
    let regularization = trace * 1e-4;
    for i in 0..3 {
        gram[i][i] += regularization;
        cross[i][i] += regularization;
    }
    let matrix = Mat3(cross).mul(&Mat3(gram).inverse()?);
    if !matrix
        .0
        .iter()
        .flatten()
        .all(|v| v.is_finite() && v.abs() < 8.0)
    {
        return None;
    }
    let tone_pairs: Vec<_> = pairs
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 3 != 0)
        .map(|(_, (x, y))| (luma(matrix.apply(*x).map(|v| v.max(0.0))), luma(*y)))
        .collect();
    let curve = fit_tone(tone_pairs)?;
    let tone = ToneMap::camera(&curve, 0.0, 0.0, 0.0);
    let original_tone = ToneMap::new(0.0, 0.0, 0.0);
    let mut before = 0.0;
    let mut after = 0.0;
    let mut samples = 0;
    for (i, (x, target)) in pairs.iter().enumerate() {
        if i % 3 != 0 {
            continue;
        }
        let corrected = displayed(matrix.apply(*x), &tone);
        let original = displayed(*x, &original_tone);
        for c in 0..3 {
            before += (original[c] - target[c]).powi(2);
            after += (corrected[c] - target[c]).powi(2);
            samples += 1;
        }
    }
    if lightcraft_pipeline::profiling() {
        eprintln!(
            "[profile] camera look holdout RMS {:.5} -> {:.5} ({samples} channels)",
            (before / samples as f64).sqrt(),
            (after / samples as f64).sqrt()
        );
    }
    if !after.is_finite()
        || samples == 0
        || after >= before * MIN_IMPROVEMENT
        || after / samples as f64 > MAX_HOLDOUT_RMS.powi(2)
    {
        return None;
    }
    Some(CameraLook {
        matrix,
        tone: curve,
    })
}

fn median(values: &mut [f64]) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied()
}

fn fit_tone(mut pairs: Vec<(f64, f64)>) -> Option<CameraTone> {
    if pairs.len() < 128
        || !pairs
            .iter()
            .all(|(x, y)| x.is_finite() && *x > 0.0 && y.is_finite())
    {
        return None;
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut knots = [[0.0; 2]; 32];
    for (i, knot) in knots.iter_mut().enumerate() {
        let bin = pairs.get(i * pairs.len() / 32..(i + 1) * pairs.len() / 32)?;
        let mut xs: Vec<_> = bin.iter().map(|p| p.0).collect();
        let mut ys: Vec<_> = bin.iter().map(|p| p.1).collect();
        *knot = [median(&mut xs)? as f32, median(&mut ys)? as f32];
    }
    if knots[31][0] < knots[0][0] * 1.5 {
        return None;
    }
    let mut blocks: Vec<(f32, usize)> = Vec::new();
    for knot in knots {
        blocks.push((knot[1], 1));
        while blocks.len() >= 2 {
            let (a, an) = *blocks.get(blocks.len() - 2)?;
            let (b, bn) = *blocks.last()?;
            if a <= b {
                break;
            }
            blocks.truncate(blocks.len() - 2);
            blocks.push(((a * an as f32 + b * bn as f32) / (an + bn) as f32, an + bn));
        }
    }
    let mut i = 0;
    for (y, n) in blocks {
        for knot in knots.get_mut(i..i + n)? {
            knot[1] = y;
        }
        i += n;
    }
    CameraTone::new(knots)
}

/// Estimate the starting look of a raw without a camera colour matrix (Sony
/// ARW, Nikon NEF) from its own JPEG. The JPEG supplies correspondences only.
pub fn fit_preview(
    raw: &lightcraft_raw::RawImage,
    bytes: &[u8],
    transform: &CameraTransform,
) -> Option<CameraLook> {
    if !transform.matrix_is_fallback || !file_local_look(raw.format) {
        return None;
    }
    let jpeg = lightcraft_raw::embedded_preview(bytes)?;
    let decoded = lightcraft_codecs::decode(
        &jpeg,
        DecodeOptions {
            max_size: Some((384, 384)),
            max_pixels: 64_000_000,
        },
    )
    .ok()?;
    let reference = decoded.to_working();
    let crop = raw
        .crop
        .clipped(raw.active_area.width, raw.active_area.height);
    if crop.width == 0 || crop.height == 0 || reference.width == 0 || reference.height == 0 {
        return None;
    }
    let aspect = crop.width as f64 / crop.height as f64;
    if (reference.width as f64 / reference.height as f64 / aspect - 1.0).abs() > 0.02 {
        return None;
    }
    let k = (crop.width.max(crop.height).div_ceil(384).max(2)).div_ceil(2) * 2;
    let sensor = raw.develop_binned(k, 0.99).ok()??;
    let mut sensor = fit(&sensor, 96, 96, Filter::Box);
    let reference = fit(&reference, sensor.width, sensor.height, Filter::Box);
    let gain = 2f32.powf(transform.baseline_exposure as f32);
    sensor.map_in_place(|p| {
        transform
            .matrix
            .apply_f32(std::array::from_fn(|i| p[i] * transform.wb[i] * gain))
    });
    let look = fit_pairs(&sensor, &reference)?;
    if lightcraft_pipeline::profiling() {
        eprintln!(
            "[profile] {:?} camera look: {:?}, {:?}",
            raw.format, look.matrix.0, look.tone
        );
    }
    Some(look)
}

/// Decode a file into a linear Rec.2020 image no larger than `max_edge`,
/// oriented, plus the `SourceInfo` the pipeline renders it against. Mirrors
/// `lightcraft-engine::files::load_bytes` for the raw path.
pub fn load_bytes(
    bytes: Cow<'_, [u8]>,
    max_edge: usize,
    cancel: &edit_core::CancelToken,
) -> Result<(Rgb32f, SourceInfo), String> {
    load_bytes_now(bytes, max_edge, cancel)
}

fn load_bytes_now(
    bytes: Cow<'_, [u8]>,
    max_edge: usize,
    cancel: &edit_core::CancelToken,
) -> Result<(Rgb32f, SourceInfo), String> {
    cancel.check().map_err(|e| e.to_string())?;
    if lightcraft_raw::probe(&bytes).is_some() {
        let mut raw = match lightcraft_raw::decode(&bytes) {
            Ok(r) => r,
            Err(lightcraft_raw::RawError::Unsupported(why)) => {
                return load_embedded_preview(&bytes, max_edge).ok_or(format!(
                    "unsupported raw ({why}) without an embedded preview"
                ));
            }
            Err(e) => return Err(e.to_string()),
        };
        cancel.check().map_err(|e| e.to_string())?;
        let xy = lightcraft_raw::color::as_shot_white_xy(&raw);
        let t = lightcraft_raw::color::camera_transform(&raw, xy);
        let camera_look = fit_preview(&raw, &bytes, &t);
        drop(bytes);
        let lens = embedded_lens(&raw.info());
        raw.opcodes.list3.retain(|op| !is_lens_opcode(op));
        let t0 = std::time::Instant::now();
        let binned = match bin_factor(&raw, max_edge) {
            Some(k) => raw
                .develop_binned(k, HIGHLIGHT_CLIP)
                .map_err(|e| e.to_string())?,
            None => None,
        };
        let mut img = match binned {
            Some(img) => img,
            None => {
                let method = if max_edge <= 600 {
                    Method::Bilinear
                } else {
                    Method::Ahd
                };
                raw.develop(method).map_err(|e| e.to_string())?
            }
        };
        raw.data = lightcraft_raw::RawData::U16(Vec::new());
        cancel.check().map_err(|e| e.to_string())?;
        let mut stages = vec![("develop", t0.elapsed())];
        lightcraft_raw::highlight::reconstruct(&mut img, t.wb, HIGHLIGHT_CLIP);
        stages.push(("highlights", t0.elapsed()));
        let m = camera_look
            .map(|p| p.matrix.mul(&t.matrix))
            .unwrap_or(t.matrix)
            .to_f32();
        let gain = 2f32.powf(t.baseline_exposure as f32);
        let wb = t.wb;
        let tables = lightcraft_raw::profile::ProfileTables::new(
            &raw.color.profile,
            lightcraft_raw::color::illuminant_weight(&raw.color, xy),
        );
        img.map_in_place(|p| {
            let c = [p[0] * wb[0], p[1] * wb[1], p[2] * wb[2]];
            let rgb = [
                m[0][0] * c[0] + m[0][1] * c[1] + m[0][2] * c[2],
                m[1][0] * c[0] + m[1][1] * c[1] + m[1][2] * c[2],
                m[2][0] * c[0] + m[2][1] * c[1] + m[2][2] * c[2],
            ];
            match &tables {
                Some(tables) => tables.apply(rgb, gain).map(|v| v.max(0.0)),
                None => rgb.map(|v| (v * gain).max(0.0)),
            }
        });
        stages.push(("transform", t0.elapsed()));
        stages.push(("colour", t0.elapsed()));
        cancel.check().map_err(|e| e.to_string())?;
        let img = fit(&img, max_edge, max_edge, Filter::Box);
        stages.push(("fit", t0.elapsed()));
        cancel.check().map_err(|e| e.to_string())?;
        let img = img.into_oriented(raw.orientation);
        stages.push(("orient", t0.elapsed()));
        if lightcraft_pipeline::profiling() {
            let mut prev = std::time::Duration::ZERO;
            let parts: Vec<String> = stages
                .iter()
                .map(|(n, t)| {
                    let d = *t - prev;
                    prev = *t;
                    format!("{n} {:.1}", d.as_secs_f64() * 1e3)
                })
                .collect();
            eprintln!(
                "[profile] raw source {}×{} (max {max_edge}, ms after decode): {}",
                img.width,
                img.height,
                parts.join(", ")
            );
        }
        let (temp, tint) = xy_to_temp_tint(xy);
        let relative = file_local_look(raw.format) && t.matrix_is_fallback;
        let camera_tone = camera_look.map(|p| p.tone).or_else(|| {
            raw.color
                .profile
                .tone_curve
                .as_ref()
                .and_then(dng_tone_curve)
        });
        let (temp, tint) = if relative {
            (6500.0, 0.0)
        } else {
            (temp.round(), tint.round())
        };
        return Ok((
            img,
            SourceInfo {
                raw: true,
                as_shot_temp: temp,
                as_shot_tint: tint,
                lens,
                relative_wb: relative,
                camera_tone,
            },
        ));
    }
    let d = crate::plain::decode_plain(&bytes, max_edge, cancel)?;
    drop(bytes);
    Ok((d.image, d.info))
}

/// The embedded preview of a raw file as a working-space image no larger than
/// `max_edge`, oriented.
pub fn load_embedded_preview(bytes: &[u8], max_edge: usize) -> Option<(Rgb32f, SourceInfo)> {
    let jpeg = lightcraft_raw::embedded_preview(bytes)?;
    let d = lightcraft_codecs::decode(&jpeg, DecodeOptions::fit(max_edge as u32, max_edge as u32))
        .ok()?;
    let img = d.to_working();
    let img = if img.width.max(img.height) > max_edge {
        fit(&img, max_edge, max_edge, Filter::Mitchell)
    } else {
        img
    };
    Some((
        img.oriented(Orientation::from_exif(d.orientation)),
        SourceInfo::default(),
    ))
}
