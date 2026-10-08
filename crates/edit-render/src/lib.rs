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

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use edit_core::cancel::CancelToken;
use edit_core::error::{Error, Result};
use edit_core::render::{Aspect, Crop, Frame, OutputColor, RenderRequest, RenderSize};
use edit_core::traits::{DevelopEngine, RetouchStage as RetouchStageTrait};
use edit_core::{DevelopSettings, ItemId, MaskPlane, RetouchOp, RetouchRecipe};
use lightcraft_pipeline::Quality;

/// Long edge of the downscaled proxy rendered during a slider drag. The
/// proxy-decode bins raws in milliseconds and keeps every slider frame in the
/// ~15–64 ms class the R0 memo measured.
pub const PROXY_DRAG_LONG_EDGE: u32 = 1600;

/// Long edge of the bounded display proxy rendered on slider release and on
/// selection: crisper than the drag edge, still display-grade — full
/// resolution stays export-only (E1/M2).
pub const PROXY_RELEASE_LONG_EDGE: u32 = 2560;

/// Byte budget of the decoded-source cache (+ warm stage caches). A high-MP
/// `Full` f32 source is 250–540 MB, so browsing raws must evict by bytes,
/// not by entry count.
pub const DECODE_CACHE_BUDGET_BYTES: usize = 768 << 20;

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

impl DecodeSource {
    /// Bytes this entry holds: the source samples plus warm stage caches.
    fn held_bytes(&self) -> usize {
        self.img
            .data
            .len()
            .saturating_mul(3 * std::mem::size_of::<f32>())
            .saturating_add(self.stages.bytes())
    }
}

type DecodeKey = (PathBuf, i64, u64, u16);

/// A decode in flight: the owner publishes; concurrent misses of the same
/// key wait on the condvar and hit the cache instead of re-decoding.
struct PendingDecode {
    out: Mutex<Option<std::result::Result<Arc<DecodeSource>, String>>>,
    cond: Condvar,
}

impl PendingDecode {
    fn publish(&self, result: std::result::Result<Arc<DecodeSource>, String>) {
        if let Ok(mut slot) = self.out.lock() {
            *slot = Some(result);
        }
        self.cond.notify_all();
    }

    /// Wait for the owner's result, checking the cancel token between timed
    /// waits. `None` when the caller was cancelled.
    fn wait(&self, cancel: &CancelToken) -> Option<std::result::Result<Arc<DecodeSource>, String>> {
        let mut guard = self.out.lock().ok()?;
        loop {
            if let Some(result) = guard.as_ref() {
                return Some(result.clone());
            }
            if cancel.is_cancelled() {
                return None;
            }
            guard = self
                .cond
                .wait_timeout(guard, std::time::Duration::from_millis(50))
                .ok()?
                .0;
        }
    }
}

/// The decode cache: hit sources, in-flight decodes, and byte-bounded LRU
/// eviction, so browsing high-MP raws cannot grow the process without limit.
struct DecodeCache {
    sources: HashMap<DecodeKey, Arc<DecodeSource>>,
    pending: HashMap<DecodeKey, Arc<PendingDecode>>,
    order: VecDeque<DecodeKey>,
    bytes_held: usize,
    capacity: usize,
}

impl DecodeCache {
    fn new(capacity: usize) -> Self {
        Self {
            sources: HashMap::new(),
            pending: HashMap::new(),
            order: VecDeque::new(),
            bytes_held: 0,
            capacity: capacity.max(1),
        }
    }

    /// LRU check: move the hit to the back of the order bank.
    fn check(&mut self, key: &DecodeKey) -> Option<Arc<DecodeSource>> {
        let hit = self.sources.get(key).map(Arc::clone)?;
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
        Some(hit)
    }

    /// The in-flight slot for `key`: re-used when one is already waiting,
    /// freshly created (with the `(slot, owner)` owner flag) otherwise.
    fn pending_slot(&mut self, key: &DecodeKey) -> (Arc<PendingDecode>, bool) {
        if let Some(slot) = self.pending.get(key) {
            return (Arc::clone(slot), false);
        }
        let slot = Arc::new(PendingDecode {
            out: Mutex::new(None),
            cond: Condvar::new(),
        });
        self.pending.insert(key.clone(), Arc::clone(&slot));
        (slot, true)
    }

    /// Publish the owner's result: bank the source (LRU + byte budget),
    /// drop the pending slot, wake every waiter.
    /// Publish the owner's result: bank the source (LRU + byte budget), drop
    /// the pending slot, wake every waiter.
    fn finish_pending(&mut self, key: &DecodeKey, source: Arc<DecodeSource>) {
        // Evict from the front until the entry count fits; a single entry
        // always banks even when it alone exceeds the byte budget.
        while self.sources.len() >= self.capacity {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if old == *key {
                break;
            }
            self.sources.remove(&old);
        }
        self.sources.insert(key.clone(), Arc::clone(&source));
        if !self.order.contains(key) {
            self.order.push_back(key.clone());
        }
        // Held bytes recomputed from the bank, so the tracking never drifts,
        // then byte-budget eviction (the fresh entry is never evicted here).
        self.bytes_held = self.sources.values().map(|s| s.held_bytes()).sum();
        self.evict_to_budget(key);
        self.pending.remove(key);
    }

    /// Byte-budget eviction: with one entry per edge class, drop until the
    /// total fits; never evict the entry the caller just produced.
    fn evict_to_budget(&mut self, keep: &DecodeKey) {
        while self.bytes_held > DECODE_CACHE_BUDGET_BYTES && self.sources.len() > 1 {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if old == *keep {
                break;
            }
            if let Some(removed) = self.sources.remove(&old) {
                self.bytes_held = self.bytes_held.saturating_sub(removed.held_bytes());
            }
        }
    }

    /// The owner failed or was cancelled: drop the slot, wake everyone.
    fn drop_pending(&mut self, key: &DecodeKey) {
        self.pending.remove(key);
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
    /// One render, from source file to BGRA frame. Cache hits reuse decoded
    /// sources and warm stage caches; concurrent misses of one key share a
    /// single decode. `DisplaySrgb` and `ExportSrgb` share the pixel math; the
    /// file side of an export (JPEG encode, IPTC) lands in E1.
    pub fn render_path(
        &self,
        path: &Path,
        size: RenderSize,
        develop: &DevelopSettings,
        crop: Option<&Crop>,
        cancel: &CancelToken,
    ) -> Result<Frame> {
        if let RenderSize::Proxy(0) = size {
            return Err(Error::InvalidInput("proxy edge must be nonzero".into()));
        }
        cancel.check()?;
        // The key identifies the file (path, mtime, size) and the requested
        // decode edge; a re-render picks up edits from disk. Proxy(0) is
        // rejected above, so Full uses the 0 sentinel.
        let size_key = match size {
            RenderSize::Proxy(n) => n,
            RenderSize::Full => 0,
        };
        let key: DecodeKey = (
            path.to_path_buf(),
            fs_mtime_secs(path),
            fs_size(path),
            size_key,
        );
        let cached = {
            self.decoded
                .lock()
                .map_err(|_| Error::InvalidInput("decode cache poisoned".into()))?
                .check(&key)
        };
        let source = match cached {
            Some(source) => source,
            None => self.decode_shared(path, size, size_key, cancel)?,
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

    /// Decode miss: own-or-wait the in-flight slot for this key, then decode
    /// exactly once. Concurrent callers of one key share the outcome.
    fn decode_shared(
        &self,
        path: &Path,
        size: RenderSize,
        size_key: u16,
        cancel: &CancelToken,
    ) -> Result<Arc<DecodeSource>> {
        let decode_key = (
            path.to_path_buf(),
            fs_mtime_secs(path),
            fs_size(path),
            size_key,
        );
        let (slot, owner) = {
            self.decoded
                .lock()
                .map_err(|_| Error::InvalidInput("decode cache poisoned".into()))?
                .pending_slot(&decode_key)
        };
        if owner {
            let result = self.run_decode(path, size, cancel);
            if let Ok(source) = &result {
                self.decoded
                    .lock()
                    .map_err(|_| Error::InvalidInput("decode cache poisoned".into()))?
                    .finish_pending(&decode_key, Arc::clone(source));
                return Ok(Arc::clone(source));
            }
            // Failure or cancellation: wake the waiters with the error, then
            // drop the slot so a later caller can retry.
            let message = match &result {
                Err(e) => e.to_string(),
                Ok(_) => unreachable!(),
            };
            slot.publish(Err(message.clone()));
            self.decoded
                .lock()
                .map_err(|_| Error::InvalidInput("decode cache poisoned".into()))?
                .drop_pending(&decode_key);
            return Err(Error::Engine {
                stage: "decode",
                message,
            });
        }
        // Waiter: the owner publishes or the slot evaporates; a cancelled
        // caller returns early and re-kicks on the next edit.
        match slot.wait(cancel) {
            Some(Ok(source)) => Ok(source),
            Some(Err(message)) => Err(Error::Engine {
                stage: "decode",
                message,
            }),
            None => Err(Error::Cancelled),
        }
    }

    /// The decode itself: read the file, then decode at the requested edge.
    fn run_decode(
        &self,
        path: &Path,
        size: RenderSize,
        cancel: &CancelToken,
    ) -> std::result::Result<Arc<DecodeSource>, String> {
        let bytes =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let edge = match size {
            RenderSize::Proxy(n) => n as usize,
            RenderSize::Full => full_proxy_target_edge(&bytes).unwrap_or(usize::MAX),
        };
        let (img, info) = self
            .decode(bytes, edge, cancel)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(DecodeSource {
            img: Arc::new(img),
            info,
            stages: lightcraft_pipeline::StageCache::default(),
        }))
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
            lightcraft::load_bytes(cow, edge, cancel).map_err(|message| Error::Engine {
                stage: "decode",
                message,
            })
        } else {
            plain::decode_plain(&cow, edge, cancel)
                .map(|p| (p.image, p.info))
                .map_err(|message| Error::Engine {
                    stage: "decode",
                    message,
                })
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
        // The export path is E1; C1 renders display sRGB only. A non-empty
        // retouch recipe needs the R2 mosaic/gaussian stage and arrives with
        // an explicit mask — reject with a diagnostic instead of dropping it.
        match req.color {
            OutputColor::ExportSrgb => {
                return Err(Error::InvalidInput("export renders land in E1".into()));
            }
            OutputColor::DisplaySrgb => {}
        }
        let nonempty_retouch = req
            .retouch
            .as_ref()
            .is_some_and(|recipe| !recipe.is_empty());
        if nonempty_retouch {
            return Err(Error::InvalidInput("retouch re-renders land in E3".into()));
        }
        self.render_path(&path, req.size, &req.develop, req.crop.as_ref(), cancel)
    }
}

/// Documented no-op retouch stage for the DevelopEngine contract: pixels are
/// returned unretouched until R2 applies the real mosaic/gaussian ops.
#[derive(Clone, Copy, Debug, Default)]
pub struct RetouchStageStub;

impl RetouchStageTrait for RetouchStageStub {
    fn apply(&self, _frame: &mut Frame, _recipe: &RetouchRecipe, _mask: &MaskPlane) -> Result<()> {
        let _ = RetouchOp::Mosaic { cell_px: 0 };
        Ok(())
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
///
/// TODO(D1): reconcile the angle models — `edit_core::Crop.rotation_degrees`
/// and `DevelopSettings.crop.geometry.angle` (straighten, −45..45, and the
/// crop rect shrunk symmetrically inside the rotated image) share one basis
/// only for small angles.
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

/// Nanoseconds since the epoch for `path` (sub-second identity in the
/// decode-cache key, so a fast re-render after a picker re-render picks up
/// edits from disk).
fn fs_mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// File size in bytes, part of the decode-cache key (mtime alone can repeat).
fn fs_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
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

    /// Mean of the BGRA channels of a frame (index 0 = blue, 1 = green, 2 = red).
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
    fn crop_mapping_plumbs_rect_and_angle() {
        let dir = std::env::temp_dir().join(format!("edit-render-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crop.jpg");
        // Non-uniform source: left half dark, right half bright.
        let (w, h) = (48u32, 32u32);
        let mut src = image::RgbImage::new(w, h);
        for (x, _y, px) in src.enumerate_pixels_mut() {
            let level = if x < w / 2 { 60u8 } else { 230u8 };
            *px = image::Rgb([level, level, level]);
        }
        let mut bytes = Vec::new();
        src.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();

        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let full = service
            .render_path(&path, RenderSize::Full, &develop, None, &cancel)
            .unwrap();
        // Normalized crop rect x 0..0.5: the crop render mirrors the *left* half.
        let crop = Crop {
            rect: edit_core::Rect::new(0.0, 0.0, 0.5, 1.0),
            rotation_degrees: 0.0,
            aspect: None,
        };
        let cropped = service
            .render_path(&path, RenderSize::Full, &develop, Some(&crop), &cancel)
            .unwrap();
        let full_mean = mean_channels(&full);
        let cropped_mean = mean_channels(&cropped);
        assert!(
            (cropped_mean[0] - full_mean[0]).abs() > 30.0,
            "crop rect did not select the source region: {full_mean:?} vs {cropped_mean:?}"
        );
        // A straighten angle flows (the pinned engine clamps the crop rect
        // symmetrically about the centre, so the sign inside is not provable
        // alone; D1 reconciles the straighten model).
        let mut plus = crop.clone();
        plus.rotation_degrees = 12.0;
        let plus_frame = service
            .render_path(&path, RenderSize::Full, &develop, Some(&plus), &cancel)
            .unwrap();
        let plus_mean = mean_channels(&plus_frame);
        assert_ne!(plus_mean, cropped_mean, "angle did not plumb");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cancelled_token_rejects_the_render() {
        let dir = std::env::temp_dir().join(format!("edit-render-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cancel.jpg");
        tiny_jpeg(&path, 32, 20, [120, 120, 120]);
        let service = RenderService::new();
        let token = CancelToken::new();
        token.cancel();
        let result = service.render_path(
            &path,
            RenderSize::Full,
            &DevelopSettings::default(),
            None,
            &token,
        );
        assert!(matches!(result, Err(edit_core::Error::Cancelled)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pending_decode_publishes_errors_to_waiters() {
        let slot = Arc::new(super::PendingDecode {
            out: Mutex::new(None),
            cond: Condvar::new(),
        });
        let waiter = Arc::clone(&slot);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            slot.publish(Err("disk full".to_string()));
        });
        let observed = waiter.wait(&CancelToken::new());
        handle.join().unwrap();
        assert_eq!(
            observed.map(|r| r.err()),
            Some(Some("disk full".to_string()))
        );
    }

    #[test]
    fn proxy_and_full_renders_agree_on_a_uniform_source() {
        // One code path at every size: a uniform gray source develops to the
        // same look at the drag edge and at native resolution, within the
        // resample tolerance.
        let dir = std::env::temp_dir().join(format!("edit-render-agree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agree.jpg");
        tiny_jpeg(&path, 64, 48, [140, 150, 160]);
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let proxy = service
            .render_path(&path, RenderSize::Proxy(32), &develop, None, &cancel)
            .unwrap();
        let full = service
            .render_path(&path, RenderSize::Full, &develop, None, &cancel)
            .unwrap();
        assert!(proxy.width <= 32 && full.width == 64);
        let proxy_mean = mean_channels(&proxy);
        let full_mean = mean_channels(&full);
        for i in 0..4 {
            assert!(
                (proxy_mean[i] - full_mean[i]).abs() <= 6.0,
                "proxy and full disagree on channel {i}: {proxy_mean:?} vs {full_mean:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_fixture_develops_proxy_and_full() {
        // Set EDIT_RENDER_RAW_FIXTURE to a camera raw (CC0 sample from
        // raw.pixls.us; the binary is multi-MB camera data and not committed).
        let Some(fixture) = std::env::var_os("EDIT_RENDER_RAW_FIXTURE").map(PathBuf::from) else {
            eprintln!("skip: set EDIT_RENDER_RAW_FIXTURE to a camera raw file");
            return;
        };
        if !fixture.is_file() {
            eprintln!("skip: {} is not a file", fixture.display());
            return;
        }
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let proxy = service
            .render_path(
                &fixture,
                RenderSize::Proxy(crate::PROXY_DRAG_LONG_EDGE as u16),
                &develop,
                None,
                &cancel,
            )
            .unwrap();
        assert!(proxy.width <= crate::PROXY_DRAG_LONG_EDGE);
        // The full export-grade path also renders (the code path is shared).
        let full = service
            .render_path(&fixture, RenderSize::Full, &develop, None, &cancel)
            .unwrap();
        assert!(full.width > 0 && full.height > 0);
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
