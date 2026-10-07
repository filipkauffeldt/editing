//! Thumbnail previews for M1.
//!
//! Raw files go through `lightcraft-raw` embedded previews (millisecond
//! path, never the full decode); plain JPEGs decode directly. Both land in
//! [`edit_core::PreviewFrame`] (BGRA) for the [`edit_core::PreviewProvider`]
//! trait, while [`thumbnail_bytes`] keeps the original JPEG bytes so GPUI's
//! `img()` element can decode and cache them itself.

use std::path::Path;

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
