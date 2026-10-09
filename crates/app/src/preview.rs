//! Browse previews: the camera's own JPEG, colour-managed for display.
//!
//! Raw files go through `lightcraft-raw` embedded previews (millisecond
//! path, never the full decode); plain JPEGs decode directly. Both land in
//! [`edit_core::PreviewFrame`] (BGRA) for the [`edit_core::PreviewProvider`]
//! trait, while [`thumbnail_bytes`] keeps the original JPEG bytes so GPUI's
//! `img()` element can decode and cache them itself.

use std::path::Path;

use std::io::Cursor;

use image::ImageDecoder as _;

use edit_core::traits::PreviewProvider;
use edit_core::{Error, PreviewFrame, Result};

use crate::browse::{PhotoKind, classify_file};

/// Embedded-preview thumbnails backed by `lightcraft-raw`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrowsePreviewProvider;

impl BrowsePreviewProvider {
    /// JPEG bytes for a thumbnail: the file itself for JPEGs, the embedded
    /// camera preview for raws. `None` when the raw carries no preview.
    pub fn thumbnail_bytes(path: &Path) -> Option<Vec<u8>> {
        let bytes = std::fs::read(path).ok()?;
        match classify_file(path) {
            Some(PhotoKind::Jpeg) => Some(bytes),
            Some(PhotoKind::Raw) => {
                lightcraft_raw::embedded_preview(&bytes).map(|jpg| jpg.to_vec())
            }
            None => None,
        }
    }

    /// The camera's own preview for a photo, ready for display: the largest
    /// embedded JPEG of a raw (or the JPEG itself), oriented and converted to
    /// sRGB, downscaled to `max_edge` when given. `None` when the file has no
    /// preview or cannot be decoded.
    pub fn decode_preview(path: &Path, max_edge: Option<u32>) -> Option<image::RgbaImage> {
        let bytes = std::fs::read(path).ok()?;
        let (jpeg, hint) = match classify_file(path)? {
            PhotoKind::Jpeg => (bytes, PreviewHint::default()),
            PhotoKind::Raw => {
                let hint = PreviewHint::from_raw(&bytes);
                (lightcraft_raw::embedded_preview(&bytes)?.to_vec(), hint)
            }
        };
        decode_preview_bytes(&jpeg, hint, max_edge).ok()
    }

    /// Decode JPEG bytes into BGRA preview pixels.
    pub fn decode_jpeg(jpeg: &[u8]) -> Result<PreviewFrame> {
        let img = image::load_from_memory(jpeg).map_err(|e| Error::Engine {
            stage: "preview",
            message: format!("thumbnail decode failed: {e}"),
        })?;
        let rgba = img.to_rgba8();
        let (width, height) = (rgba.width(), rgba.height());
        let mut pixels = rgba.into_raw();
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Ok(PreviewFrame {
            width,
            height,
            pixels,
        })
    }
}

/// What the container says about a preview JPEG that the JPEG itself omits.
/// Camera previews carry no ICC profile: Nikon stores the working space in
/// the maker note and the orientation only on the raw's own IFD0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreviewHint {
    /// The camera shot Adobe RGB, so the untagged JPEG pixels are Adobe RGB.
    pub adobe_rgb: bool,
    /// EXIF orientation (1-8) of the raw, used when the JPEG has none.
    pub orientation: Option<u8>,
}

impl PreviewHint {
    /// Read the hint from a raw's TIFF structure (IFD0 orientation, Exif
    /// `ColorSpace`, Nikon maker-note `ColorSpace`).
    pub fn from_raw(bytes: &[u8]) -> Self {
        use lightcraft_tiff::{Tiff, makernote::parse_makernote, tags};
        let Ok(tiff) = Tiff::parse(bytes) else {
            return Self::default();
        };
        let orientation = tiff
            .ifds
            .first()
            .and_then(|ifd| ifd.u16(tags::ORIENTATION))
            .and_then(|v| u8::try_from(v).ok())
            .filter(|v| (1..=8).contains(v));
        let mut adobe_rgb = false;
        if let Some(exif) = tiff.exif() {
            // 0xFFFF "uncalibrated" with the R03 interop index is Adobe RGB.
            adobe_rgb |= exif.u16(0xA001) == Some(0xFFFF)
                && exif
                    .interop
                    .as_ref()
                    .and_then(|i| i.bytes(1))
                    .is_some_and(|b| b.starts_with(b"R03"));
            if let Some(note) = exif.get(tags::MAKER_NOTE) {
                let make = tiff
                    .find(tags::MAKE)
                    .and_then(|e| e.value.as_str())
                    .unwrap_or_default();
                if let Some(mn) =
                    parse_makernote(bytes, note.offset, note.count() as u64, tiff.order, &make)
                {
                    // Nikon ColorSpace: 1 = sRGB, 2 = Adobe RGB.
                    adobe_rgb |= make.to_ascii_uppercase().starts_with("NIKON")
                        && mn.ifd.u16(0x001E) == Some(2);
                }
            }
        }
        Self {
            adobe_rgb,
            orientation,
        }
    }
}

/// Convert straight-alpha RGBA8 pixels to sRGB in place: through the embedded
/// ICC profile when there is one, else from Adobe RGB when the container says
/// so. Untagged files are already sRGB and are left alone.
fn to_srgb(rgba: &mut [u8], icc: Option<&[u8]>, adobe_rgb: bool) -> Result<()> {
    let engine = |message: String| Error::Engine {
        stage: "preview",
        message,
    };
    let source = match (icc, adobe_rgb) {
        (Some(icc), _) => moxcms::ColorProfile::new_from_slice(icc)
            .map_err(|e| engine(format!("bad ICC profile: {e}")))?,
        (None, true) => moxcms::ColorProfile::new_adobe_rgb(),
        (None, false) => return Ok(()),
    };
    let transform = source
        .create_transform_8bit(
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::Layout::Rgba,
            moxcms::TransformOptions::default(),
        )
        .map_err(|e| engine(format!("colour transform failed: {e}")))?;
    let mut out = vec![0u8; rgba.len()];
    transform
        .transform(rgba, &mut out)
        .map_err(|e| engine(format!("colour transform failed: {e}")))?;
    rgba.copy_from_slice(&out);
    Ok(())
}

/// Decode a preview JPEG into oriented, display-sRGB RGBA, never larger than
/// `max_edge` on the long edge (`None` keeps the full size).
pub fn decode_preview_bytes(
    jpeg: &[u8],
    hint: PreviewHint,
    max_edge: Option<u32>,
) -> Result<image::RgbaImage> {
    let fail = |message: String| Error::Engine {
        stage: "preview",
        message,
    };
    let mut decoder = image::ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .map_err(|e| fail(e.to_string()))?
        .into_decoder()
        .map_err(|e| fail(e.to_string()))?;
    let icc = decoder.icc_profile().ok().flatten();
    let mut orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    if orientation == image::metadata::Orientation::NoTransforms
        && let Some(o) = hint
            .orientation
            .and_then(image::metadata::Orientation::from_exif)
    {
        orientation = o;
    }
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| fail(e.to_string()))?;
    if let Some(edge) = max_edge
        && img.width().max(img.height()) > edge
    {
        img = img.thumbnail(edge, edge);
    }
    img.apply_orientation(orientation);
    let mut rgba = img.to_rgba8();
    to_srgb(&mut rgba, icc.as_deref(), hint.adobe_rgb)?;
    Ok(rgba)
}

impl PreviewProvider for BrowsePreviewProvider {
    fn embedded_preview(&self, path: &Path) -> Result<PreviewFrame> {
        let bytes = std::fs::read(path).map_err(|e| Error::Engine {
            stage: "preview",
            message: format!("cannot read {}: {e}", path.display()),
        })?;
        match classify_file(path) {
            Some(PhotoKind::Jpeg) => Self::decode_jpeg(&bytes),
            Some(PhotoKind::Raw) => {
                let jpeg = lightcraft_raw::embedded_preview(&bytes).ok_or(Error::Engine {
                    stage: "preview",
                    message: format!("no embedded preview in {}", path.display()),
                })?;
                Self::decode_jpeg(&jpeg)
            }
            None => Err(Error::Engine {
                stage: "preview",
                message: format!("unsupported file: {}", path.display()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg_bytes() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 6, image::Rgb([30, 120, 200]));
        let mut bytes = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut bytes);
            img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();
        }
        bytes
    }

    #[test]
    fn jpeg_round_trips_through_preview() {
        let dir = std::env::temp_dir().join(format!("edit-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thumb.jpg");
        std::fs::write(&path, tiny_jpeg_bytes()).unwrap();

        let provider = BrowsePreviewProvider;
        let frame = provider.embedded_preview(&path).unwrap();
        assert_eq!((frame.width, frame.height), (8, 6));
        assert_eq!(frame.pixels.len(), 8 * 6 * 4);

        let bytes = BrowsePreviewProvider::thumbnail_bytes(&path).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn jpeg_of(img: &image::RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
        img.write_with_encoder(enc).unwrap();
        bytes
    }

    fn near(a: u8, b: u8, tol: u8) -> bool {
        a.abs_diff(b) <= tol
    }

    #[test]
    fn primaries_keep_their_channels() {
        // Flat blocks so JPEG chroma subsampling cannot bleed between them.
        let mut img = image::RgbImage::new(48, 16);
        for (x, _, px) in img.enumerate_pixels_mut() {
            *px = match x / 16 {
                0 => image::Rgb([220, 20, 20]),
                1 => image::Rgb([20, 220, 20]),
                _ => image::Rgb([20, 20, 220]),
            };
        }
        let out = decode_preview_bytes(&jpeg_of(&img), PreviewHint::default(), None).unwrap();
        let (r, g, b) = (
            out.get_pixel(8, 8),
            out.get_pixel(24, 8),
            out.get_pixel(40, 8),
        );
        assert!(r[0] > 180 && r[1] < 60 && r[2] < 60, "red block {r:?}");
        assert!(g[1] > 180 && g[0] < 60 && g[2] < 60, "green block {g:?}");
        assert!(b[2] > 180 && b[0] < 60 && b[1] < 60, "blue block {b:?}");
    }

    #[test]
    fn untagged_adobe_rgb_is_converted_to_srgb() {
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([200, 100, 100]));
        let jpeg = jpeg_of(&img);
        let plain = decode_preview_bytes(&jpeg, PreviewHint::default(), None).unwrap();
        let adobe = decode_preview_bytes(
            &jpeg,
            PreviewHint {
                adobe_rgb: true,
                orientation: None,
            },
            None,
        )
        .unwrap();
        let (p, a) = (plain.get_pixel(8, 8), adobe.get_pixel(8, 8));
        assert!(
            near(p[0], 200, 4) && near(p[1], 100, 4),
            "sRGB passes through {p:?}"
        );
        // Adobe RGB reds are more saturated once shown as sRGB.
        assert!(
            a[0] > p[0] + 8 && a[1] <= p[1] && a[2] <= p[2],
            "{p:?} -> {a:?}"
        );
        assert_eq!(a[3], 255);
    }

    #[test]
    fn raw_orientation_turns_the_preview_and_downscales() {
        let img = image::RgbImage::from_pixel(40, 20, image::Rgb([10, 10, 10]));
        let hint = PreviewHint {
            adobe_rgb: false,
            orientation: Some(6),
        };
        let out = decode_preview_bytes(&jpeg_of(&img), hint, Some(20)).unwrap();
        assert_eq!((out.width(), out.height()), (10, 20));
    }

    #[test]
    fn unsupported_files_error_cleanly() {
        let dir = std::env::temp_dir().join(format!("edit-preview-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();
        let provider = BrowsePreviewProvider;
        assert!(provider.embedded_preview(&path).is_err());
        assert!(BrowsePreviewProvider::thumbnail_bytes(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_without_preview_reports_no_preview() {
        let dir = std::env::temp_dir().join(format!("edit-preview-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake.NEF");
        std::fs::write(&path, b"not a raw file").unwrap();
        let provider = BrowsePreviewProvider;
        let err = provider.embedded_preview(&path).unwrap_err();
        assert!(err.to_string().contains("preview"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
