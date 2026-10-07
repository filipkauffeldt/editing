//! The render service: one decode → develop → retouch → crop path at every
//! size.
//!
//! Sliders set [`DevelopSettings`] and the service renders a downscaled proxy
//! while dragging and the full proxy on release — the same code path at every
//! size (plan sections 1.5 and 2.3). Full resolution is export-only (E1).
//!
//! Decode:
//! - raw through `lightcraft-raw` (binned to the requested edge) or the
//!   camera-embedded preview when decode is unsupported
//! - plain files through the `image` crate (see [`plain`])
//!
//! The upstream LightCraft crates stay inside one bridge module. Keep it that
//! way: a revision bump is one small PR.

pub mod lightcraft;
pub mod plain;

pub use lightcraft::SourceFacts;

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use edit_core::cancel::CancelToken;
use edit_core::error::{Error, Result};
use edit_core::render::{Aspect, Crop, Frame, RenderRequest, RenderSize};
use edit_core::traits::DevelopEngine;
use edit_core::{DevelopSettings, ItemId};
use lightcraft_pipeline::Quality;

/// Long edge of the downscaled proxy rendered during a slider drag. The
/// proxy-decode bins raws in milliseconds and keeps every slider frame in the
/// ~15–64 ms class the R0 memo measured.
pub const PROXY_DRAG_LONG_EDGE: u32 = 1600;

/// Item → display-photo-path resolution, kept behind a trait so the service
/// stays independent of any concrete catalog handle.
pub trait ItemPhotoPaths: Send + Sync {
    /// The item's display photo path on disk (JPEG side preferred).
    fn photo_path(&self, item: ItemId) -> Result<Option<PathBuf>>;
}

/// One decoded source: working-space pixels, the facts the pipeline needs to
/// normalize settings, and warm stage caches.
struct DecodeSource {
    img: Arc<lightcraft_raster::Rgb32f>,
    info: lightcraft_pipeline::SourceInfo,
    stages: lightcraft_pipeline::StageCache,
}

type DecodeKey = (PathBuf, i64, u16);

/// Decoded sources for the last render sizes (one slot per edge class, so the
/// drag edge and the release edge never evict each other mid-drag).
struct DecodeCache {
    sources: HashMap<DecodeKey, Arc<DecodeSource>>,
    order: VecDeque<DecodeKey>,
    capacity: usize,
}

impl DecodeCache {
    fn new(capacity: usize) -> Self {
        Self {
            sources: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn get(&self, key: &DecodeKey) -> Option<Arc<DecodeSource>> {
        self.sources.get(key).map(Arc::clone)
    }

    fn put(&mut self, key: DecodeKey, source: Arc<DecodeSource>) {
        if !self.sources.contains_key(&key) {
            while self.sources.len() >= self.capacity {
                let Some(old) = self.order.pop_front() else {
                    break;
                };
                self.sources.remove(&old);
            }
            self.order.push_back(key.clone());
        }
        self.sources.insert(key, source);
    }
}

/// The develop engine. State carries only decoded sources; every render is
/// pure math, so concurrent callers share one instance safely.
pub struct RenderService {
    decoded: Mutex<DecodeCache>,
    paths: Option<Arc<dyn ItemPhotoPaths>>,
}

impl Default for RenderService {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderService {
    /// A service whose callers always pass paths.
    pub fn new() -> Self {
        Self {
            decoded: Mutex::new(DecodeCache::new(2)),
            paths: None,
        }
    }

    /// A service that resolves items to paths for the DevelopEngine contract.
    pub fn with_paths(paths: Arc<dyn ItemPhotoPaths>) -> Self {
        Self {
            paths: Some(paths),
            ..Self::new()
        }
    }

    /// One render, from source file to BGRA frame. `DisplaySrgb` and
    /// `ExportSrgb` share the same pixel math; an export adds only the file
    /// side (JPEG encode, IPTC) in E1.
    pub fn render_path(
        &self,
        path: &Path,
        size: RenderSize,
        develop: &DevelopSettings,
        crop: Option<&Crop>,
        cancel: &CancelToken,
    ) -> Result<Frame> {
        cancel.check()?;
        // The full-proxy edge is resolved from the file's own bytes on a
        // decode miss only: cache hits must not re-read 25 MB raws.
        // Proxy edges key by their long edge; Full uses the 0 sentinel
        // (Proxy(0) is not a valid edge).
        let size_key = match size {
            RenderSize::Proxy(n) => n,
            RenderSize::Full => 0,
        };
        let key: DecodeKey = (path.to_path_buf(), fs_mtime_secs(path), size_key);
        let cached = self
            .decoded
            .lock()
            .map_err(|_| Error::InvalidInput("decode cache poisoned".into()))?
            .get(&key);
        let source = match cached {
            Some(source) => source,
            None => {
                cancel.check()?;
                let bytes = std::fs::read(path).map_err(|e| Error::Engine {
                    stage: "decode",
                    message: format!("cannot read {}: {e}", path.display()),
                })?;
                let edge = match size {
                    RenderSize::Proxy(n) => n as usize,
                    RenderSize::Full => full_proxy_target_edge(&bytes).unwrap_or(usize::MAX),
                };
                let (img, info) = self.decode(bytes, edge, cancel)?;
                let source = Arc::new(DecodeSource {
                    img: Arc::new(img),
                    info,
                    stages: lightcraft_pipeline::StageCache::default(),
                });
                self.decoded
                    .lock()
                    .map_err(|_| Error::InvalidInput("decode cache poisoned".into()))?
                    .put(key, Arc::clone(&source));
                source
            }
        };

        // The develop settings are user buffer state; the crop rides in them
        // so the develop math applies it identically at every size. The output
        // box is the decoded source's own size: no upscaling at any size.
        let mut develop = develop.clone();
        if let Some(crop) = crop {
            apply_crop(&mut develop, crop);
        }
        let pipeline_req = lightcraft_pipeline::RenderRequest {
            quality: match size {
                RenderSize::Proxy(_) => Quality::Draft,
                RenderSize::Full => Quality::Full,
            },
            ..lightcraft_pipeline::RenderRequest::fit(source.img.width, source.img.height)
        };
        cancel.check()?;
        let rendered = lightcraft_pipeline::render_cached(
            &source.img,
            &source.info,
            &develop,
            &pipeline_req,
            &source.stages,
        );
        cancel.check()?;
        bgra_frame(&rendered.image)
    }

    fn decode(
        &self,
        bytes: Vec<u8>,
        edge: usize,
        cancel: &CancelToken,
    ) -> Result<(lightcraft_raster::Rgb32f, lightcraft_pipeline::SourceInfo)> {
        let cow = Cow::Owned(bytes);
        cancel.check()?;
        if lightcraft_raw::probe(&cow).is_some() {
            lightcraft::load_bytes(cow, edge).map_err(|message| Error::Engine {
                stage: "decode",
                message,
            })
        } else {
            plain::decode_plain(&cow, edge)
                .map(|p| (p.image, p.info))
                .map_err(|message| Error::Engine {
                    stage: "decode",
                    message,
                })
        }
    }
    /// Seed-relevant facts about a source, derived with a container parse
    /// (never rendering pixels), for the develop session's white-point seed.
    pub fn source_facts(path: &Path) -> Option<SourceFacts> {
        let bytes = std::fs::read(path).ok()?;
        if lightcraft_raw::probe(&bytes).is_some() {
            lightcraft::source_facts(&bytes)
        } else {
            // Plain files carry relative WB; no seed needed.
            Some(SourceFacts::default())
        }
    }
}

impl DevelopEngine for RenderService {
    /// Resolve the item's photo path through the catalog and render.
    fn render(&self, req: RenderRequest, cancel: &CancelToken) -> Result<Frame> {
        let paths = self.paths.as_ref().ok_or_else(|| {
            Error::InvalidInput("render service has no item-path resolver".into())
        })?;
        let Some(path) = paths.photo_path(req.item)? else {
            return Err(Error::InvalidInput(format!(
                "item {} has no photo on disk",
                req.item.get()
            )));
        };
        self.render_path(&path, req.size, &req.develop, req.crop.as_ref(), cancel)
    }
}

/// Aspect-lock ratio (w, h) for a crop preset, or `None` for the free
/// (original-ratio) case.
fn aspect_w_h(aspect: Aspect) -> (u32, u32) {
    match aspect {
        Aspect::Original => (0, 0),
        Aspect::Square => (1, 1),
        Aspect::ThreeTwo => (3, 2),
        Aspect::TwoThree => (2, 3),
        Aspect::FourThree => (4, 3),
        Aspect::SixteenNine => (16, 9),
    }
}

/// Map a normalized crop (+ rotation) into the develop settings' own crop
/// model.
fn apply_crop(develop: &mut DevelopSettings, crop: &Crop) {
    let rect = crop.rect.clamped();
    let geometry = &mut develop.crop.geometry;
    geometry.rect = lightcraft_geom::Rect {
        x0: rect.x as f64,
        y0: rect.y as f64,
        x1: (rect.x + rect.w) as f64,
        y1: (rect.y + rect.h) as f64,
    };
    geometry.angle = crop.rotation_degrees as f64;
    develop.crop.aspect = crop
        .aspect
        .filter(|a| !matches!(a, Aspect::Original))
        .map(aspect_w_h);
}

/// Pipeline RGBA8 result → BGRA frame for `paint_image`.
fn bgra_frame(image: &lightcraft_raster::Rgba8) -> Result<Frame> {
    let mut pixels = image.as_bytes();
    for chunk in pixels.as_chunks_mut::<4>().0 {
        chunk.swap(0, 2);
    }
    let frame = Frame {
        width: image.width as u32,
        height: image.height as u32,
        stride: (image.width as u32)
            .checked_mul(4)
            .ok_or_else(|| Error::InvalidInput("pipeline result overflows stride".into()))?,
        pixels,
    };
    frame.validate()?;
    Ok(frame)
}

/// Seconds since the epoch for `path`, part of the decode-cache key so a
/// re-render picks up edits from disk.
fn fs_mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Long edge targeted by a full-proxy render: the file's own preview-grade
/// full size — the embedded camera preview's long edge for raws, native
/// resolution for plain files. Full sensor resolution stays export-only.
fn full_proxy_target_edge(bytes: &[u8]) -> Option<usize> {
    if lightcraft_raw::probe(bytes).is_some() {
        let jpeg = lightcraft_raw::embedded_preview(bytes)?;
        let (w, h) = jpeg_dimensions(&jpeg)?;
        Some(w.max(h))
    } else {
        // Plain files render at their own resolution.
        Some(usize::MAX)
    }
}

/// Dimensions with a header-only read.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(usize, usize)> {
    image::ImageReader::new(Cursor::new(bytes))
        .into_dimensions()
        .ok()
        .map(|(w, h)| (w as usize, h as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg(path: &Path, w: u32, h: u32, color: [u8; 3]) {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(color));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Mean of the G/B/R (BGRA: index 1/2/0... documented at the call sites)
    /// channels of a frame.
    fn mean_channels(frame: &Frame) -> [f32; 4] {
        let px = frame.pixels.as_chunks::<4>().0;
        let n = px.len() as f32;
        px.iter()
            .fold([0.0f32; 4], |acc, p| {
                let mut out = acc;
                for (i, v) in out.iter_mut().enumerate() {
                    *v += p[i] as f32;
                }
                out
            })
            .map(|v| v / n)
    }

    #[test]
    fn renders_bgra_frames_at_proxy_and_full_sizes() {
        let dir = std::env::temp_dir().join(format!("edit-render-sizes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sizes.jpg");
        tiny_jpeg(&path, 32, 20, [200, 100, 40]);
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let proxy = service
            .render_path(&path, RenderSize::Proxy(16), &develop, None, &cancel)
            .unwrap();
        let full = service
            .render_path(&path, RenderSize::Full, &develop, None, &cancel)
            .unwrap();
        // Fit never upscales, so the proxy stays at its 16-px long-edge
        // budget and the full render keeps the native 32-px edge.
        assert!(proxy.width <= 16 && proxy.height <= 16);
        assert_eq!(full.width, 32);
        assert_eq!(
            full.pixels.len(),
            full.stride as usize * full.height as usize
        );
        // BGRA layout: the render's color has red (200) far above blue (40),
        // so frame index 0 (B) must sit below index 2 (R) — the swizzle's
        // own signal.
        let mean = mean_channels(&full);
        assert!(mean[0] < mean[2], "frame is not BGRA: {mean:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exposure_slider_brightens_the_render_monotonically() {
        let dir = std::env::temp_dir().join(format!("edit-render-exposure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("exposure.jpg");
        tiny_jpeg(&path, 32, 20, [120, 120, 120]);
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let dark = service
            .render_path(&path, RenderSize::Full, &develop, None, &cancel)
            .unwrap();
        let mut bright = develop.clone();
        bright.light.exposure = 1.0;
        let bright = service
            .render_path(&path, RenderSize::Full, &bright, None, &cancel)
            .unwrap();
        assert_eq!(dark.width, bright.width);
        let dark_mean = mean_channels(&dark);
        let bright_mean = mean_channels(&bright);
        // Exposure is gain on scene-referred data: every channel mean rises.
        for i in 0..3 {
            assert!(
                bright_mean[i] > dark_mean[i],
                "exposure did not brighten channel {i}: {dark_mean:?} -> {bright_mean:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
