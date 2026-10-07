//! Plain-file (JPEG/PNG) decode for the render service, through the `image`
//! crate (zune-jpeg inside image 0.25 decodes JPEG).
//!
//! Output is scene-linear Rec.2020, the pipeline working space. An embedded
//! ICC profile is honored with `moxcms`: bulk-transform to sRGB, then
//! linearize and map primaries with `lightcraft_color`. Untagged files are
//! sRGB (the container's normal default).

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder as _};
use lightcraft_color::transfer::decode_srgb8;
use lightcraft_pipeline::SourceInfo;
use lightcraft_raster::Rgb32f;

/// A decoded plain file: working-space pixels plus neutral source info.
pub struct Plain {
    pub image: Rgb32f,
    pub info: SourceInfo,
}

/// Decode a plain file to scene-linear Rec.2020 (oriented, never upscaled,
/// downscaled on the long edge to `max_edge`). `cancel` is checked after the
/// container decode and after the pixel pass.
pub fn decode_plain(
    bytes: &[u8],
    max_edge: usize,
    cancel: &edit_core::CancelToken,
) -> Result<Plain, String> {
    cancel.check().map_err(|e| e.to_string())?;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut dec = reader.into_decoder().map_err(|e| e.to_string())?;
    let icc = dec.icc_profile().ok().flatten();
    let orientation = dec
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    cancel.check().map_err(|e| e.to_string())?;

    // Downscale on the long edge; never upscale.
    let long = img.width().max(img.height());
    if long as usize > max_edge && max_edge > 0 {
        img = img.thumbnail(max_edge as u32, max_edge as u32);
    }

    let rgba = img.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let src = rgba.as_raw().clone();
    let linear = match icc {
        Some(profile) => moxcms_to_linear(&src, &profile, width, height)?,
        None => linear_from_rgba8(&src, width, height),
    };
    cancel.check().map_err(|e| e.to_string())?;
    Ok(Plain {
        image: linear,
        info: SourceInfo::default(),
    })
}

/// Linearize an RGBA8 buffer (sRGB TRC) and map primaries to Rec.2020.
fn linear_from_rgba8(rgba8: &[u8], width: u32, height: u32) -> Rgb32f {
    let m = lightcraft_color::SRGB
        .to_space(&lightcraft_color::WORKING)
        .to_f32();
    let mut out = Rgb32f::new(width as usize, height as usize);
    lightcraft_raster::par_rows(&mut out.data, width as usize, |y, row| {
        let src_row = &rgba8[y * width as usize * 4..];
        for (x, o) in row.iter_mut().enumerate() {
            let rgb = [
                decode_srgb8(src_row[x * 4]),
                decode_srgb8(src_row[x * 4 + 1]),
                decode_srgb8(src_row[x * 4 + 2]),
            ];
            *o = [
                m[0][0] * rgb[0] + m[0][1] * rgb[1] + m[0][2] * rgb[2],
                m[1][0] * rgb[0] + m[1][1] * rgb[1] + m[1][2] * rgb[2],
                m[2][0] * rgb[0] + m[2][1] * rgb[1] + m[2][2] * rgb[2],
            ];
        }
    });
    out
}

/// ICC path: parse the profile and bulk-transform RGBA → sRGB, then reuse the
/// same linearize + primaries mapping.
fn moxcms_to_linear(
    rgba8: &[u8],
    profile_bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<Rgb32f, String> {
    let src = moxcms::ColorProfile::new_from_slice(profile_bytes).map_err(|e| e.to_string())?;
    let transform = src
        .create_transform_8bit(
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::Layout::Rgba,
            moxcms::TransformOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let mut out = vec![0u8; rgba8.len()];
    transform
        .transform(rgba8, &mut out)
        .map_err(|e| e.to_string())?;
    Ok(linear_from_rgba8(&out, width, height))
}
