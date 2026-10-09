//! The render service: one decode → develop → retouch → crop path at every
//! size.
//!
//! Sliders set [`DevelopSettings`] and the service renders a downscaled proxy
//! while dragging and a bounded display proxy on release — the same code path
//! at every size (plan sections 1.5 and 2.3). [`RenderSize::Full`] is
//! reserved for export (E1): it decodes at native resolution and never banks
//! the source.
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
pub mod straighten;

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use edit_core::cancel::CancelToken;
use edit_core::error::{Error, Result};
use edit_core::render::{Frame, OutputColor, RenderRequest, RenderSize};
use edit_core::traits::{DevelopEngine, RetouchStage};
use edit_core::{Crop, DevelopSettings, ItemId, MaskPlane, RetouchRecipe};
use lightcraft_pipeline::Quality;

/// Long edge of the downscaled proxy rendered during a slider drag. The
/// proxy decode bins raws in milliseconds and keeps every slider frame in the
/// ~15–64 ms class the R0 memo measured.
pub const PROXY_DRAG_LONG_EDGE: u16 = 1600;

/// Long edge of the bounded display proxy rendered on slider release and on
/// selection: crisper than the drag edge, still display-grade — full
/// resolution stays export-only (E1).
pub const PROXY_RELEASE_LONG_EDGE: u16 = 2560;

/// Byte budget of the decoded-source cache, stage caches included. A
/// release-edge source is ~50 MB of f32 samples, so browsing high-MP raws
/// evicts by bytes rather than by entry count alone.
pub const DECODE_CACHE_BUDGET_BYTES: usize = 768 << 20;

/// Entry cap of the decoded-source cache: the current item's drag and release
/// edges plus the warmed neighbors (current ± 2).
const DECODE_CACHE_MAX_ENTRIES: usize = 8;

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
            .saturating_mul(std::mem::size_of::<[f32; 3]>())
            .saturating_add(self.stages.bytes())
    }
}

/// File identity plus decode edge: path, mtime (nanoseconds), size in bytes,
/// and the proxy long edge. A re-render after an edit on disk misses.
type DecodeKey = (PathBuf, i64, u64, u16);

/// How an in-flight decode ended, as its waiters see it.
#[derive(Clone)]
enum DecodeFailure {
    /// The owner was cancelled. Waiters that were not cancelled retry.
    Cancelled,
    Failed(String),
}

type DecodeOutcome = std::result::Result<Arc<DecodeSource>, DecodeFailure>;

/// A decode in flight: the owner publishes; concurrent misses of the same
/// key wait on the condvar instead of decoding again.
struct PendingDecode {
    out: Mutex<Option<DecodeOutcome>>,
    cond: Condvar,
}

impl PendingDecode {
    fn new() -> Self {
        Self {
            out: Mutex::new(None),
            cond: Condvar::new(),
        }
    }

    fn publish(&self, outcome: DecodeOutcome) {
        *self.out.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        self.cond.notify_all();
    }

    /// Wait for the owner's outcome, checking the cancel token between timed
    /// waits. `None` when the caller itself was cancelled.
    fn wait(&self, cancel: &CancelToken) -> Option<DecodeOutcome> {
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(outcome) = guard.as_ref() {
                return Some(outcome.clone());
            }
            if cancel.is_cancelled() {
                return None;
            }
            guard = self
                .cond
                .wait_timeout(guard, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// What a cache lookup asks of the caller.
enum Claim {
    Hit(Arc<DecodeSource>),
    /// The caller decodes and must publish (or abandon) the slot.
    Owner(Arc<PendingDecode>),
    /// Another caller is decoding this key; wait on its slot.
    Wait(Arc<PendingDecode>),
}

/// The decode cache: banked sources, in-flight decodes, and LRU eviction
/// bounded by entry count and by bytes, so browsing high-MP raws cannot grow
/// the process without limit.
struct DecodeCache {
    sources: HashMap<DecodeKey, Arc<DecodeSource>>,
    pending: HashMap<DecodeKey, Arc<PendingDecode>>,
    /// Banked keys, least recently used first.
    order: VecDeque<DecodeKey>,
    max_entries: usize,
    budget_bytes: usize,
}

impl DecodeCache {
    fn new(max_entries: usize, budget_bytes: usize) -> Self {
        Self {
            sources: HashMap::new(),
            pending: HashMap::new(),
            order: VecDeque::new(),
            max_entries: max_entries.max(1),
            budget_bytes,
        }
    }

    /// Bytes held across the bank, recomputed so warm stage caches growing
    /// after insertion are counted too.
    fn bytes_held(&self) -> usize {
        self.sources.values().map(|s| s.held_bytes()).sum()
    }

    /// Hit, wait on an in-flight decode, or become the owner of a new one.
    fn claim(&mut self, key: &DecodeKey) -> Claim {
        if let Some(hit) = self.sources.get(key).map(Arc::clone) {
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(key.clone());
            // Stage caches warm up between hits; enforce the byte budget on
            // the grown total, never evicting the entry just used.
            self.evict_to_budget(key);
            return Claim::Hit(hit);
        }
        if let Some(slot) = self.pending.get(key) {
            return Claim::Wait(Arc::clone(slot));
        }
        let slot = Arc::new(PendingDecode::new());
        self.pending.insert(key.clone(), Arc::clone(&slot));
        Claim::Owner(slot)
    }

    /// The owner finished: bank the source, evicting least recently used
    /// entries past the entry cap and the byte budget. The fresh entry is
    /// never evicted, even when it alone exceeds the budget.
    fn bank(&mut self, key: &DecodeKey, source: Arc<DecodeSource>) {
        self.pending.remove(key);
        while self.sources.len() >= self.max_entries && self.evict_oldest(key) {}
        self.sources.insert(key.clone(), source);
        self.order.push_back(key.clone());
        self.evict_to_budget(key);
    }

    /// The owner failed or was cancelled: free the key for a later caller.
    fn abandon(&mut self, key: &DecodeKey) {
        self.pending.remove(key);
    }

    /// Drop the least recently used entry other than `keep`. `false` when
    /// there is none.
    fn evict_oldest(&mut self, keep: &DecodeKey) -> bool {
        let Some(pos) = self.order.iter().position(|k| k != keep) else {
            return false;
        };
        if let Some(old) = self.order.remove(pos) {
            self.sources.remove(&old);
        }
        true
    }

    fn evict_to_budget(&mut self, keep: &DecodeKey) {
        while self.bytes_held() > self.budget_bytes && self.evict_oldest(keep) {}
    }
}

/// Settles an owned slot if the owner unwinds before publishing, so waiters
/// never hang on a key whose decode panicked.
struct OwnerGuard<'a> {
    cache: &'a Mutex<DecodeCache>,
    key: &'a DecodeKey,
    slot: Arc<PendingDecode>,
    settled: bool,
}

impl OwnerGuard<'_> {
    fn bank(mut self, source: Arc<DecodeSource>) {
        self.settled = true;
        lock(self.cache).bank(self.key, Arc::clone(&source));
        self.slot.publish(Ok(source));
    }

    fn abandon(mut self, failure: DecodeFailure) {
        self.settled = true;
        lock(self.cache).abandon(self.key);
        self.slot.publish(Err(failure));
    }
}

impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            lock(self.cache).abandon(self.key);
            self.slot
                .publish(Err(DecodeFailure::Failed("decode panicked".into())));
        }
    }
}

fn lock(cache: &Mutex<DecodeCache>) -> MutexGuard<'_, DecodeCache> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

/// The develop engine. State carries only decoded sources; every render is
/// pure math, so concurrent callers share one instance safely.
pub struct RenderService {
    decoded: Mutex<DecodeCache>,
    paths: Option<Arc<dyn ItemPhotoPaths>>,
    /// Decodes actually run, for the cache-sharing tests.
    decodes: AtomicUsize,
}

impl Default for RenderService {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderService {
    /// A service whose callers always pass paths.
    pub fn new() -> Self {
        Self::with_cache(DECODE_CACHE_MAX_ENTRIES, DECODE_CACHE_BUDGET_BYTES)
    }

    /// A service that resolves items to paths for the DevelopEngine contract.
    pub fn with_paths(paths: Arc<dyn ItemPhotoPaths>) -> Self {
        Self {
            paths: Some(paths),
            ..Self::new()
        }
    }

    fn with_cache(max_entries: usize, budget_bytes: usize) -> Self {
        Self {
            decoded: Mutex::new(DecodeCache::new(max_entries, budget_bytes)),
            paths: None,
            decodes: AtomicUsize::new(0),
        }
    }

    /// One render, from source file to BGRA frame. Proxy renders reuse decoded
    /// sources and warm stage caches; concurrent misses of one key share a
    /// single decode. `Full` decodes at native resolution and drops the source
    /// with the frame. `DisplaySrgb` and `ExportSrgb` share the pixel math;
    /// the file side of an export (JPEG encode, IPTC) lands in E1.
    pub fn render_path(
        &self,
        path: &Path,
        size: RenderSize,
        develop: &DevelopSettings,
        crop: Option<&Crop>,
        cancel: &CancelToken,
    ) -> Result<Frame> {
        cancel.check()?;
        let source = match size {
            RenderSize::Proxy(edge) => self.proxy_source(path, edge, cancel)?,
            RenderSize::Full => Arc::new(self.decode_source(path, usize::MAX, cancel)?),
        };

        // The develop settings are user buffer state; the crop rides in them
        // so the develop math applies it identically at every size. The output
        // box is the crop's own size in decoded source pixels: no upscaling
        // at any size, and the same framing at every size.
        let mut develop = develop.clone();
        apply_crop(&mut develop, crop);
        let (box_w, box_h) = crop_box(&develop, source.img.width, source.img.height);
        let pipeline_req = lightcraft_pipeline::RenderRequest {
            quality: match size {
                RenderSize::Proxy(edge) if edge <= PROXY_DRAG_LONG_EDGE => Quality::Draft,
                _ => Quality::Full,
            },
            ..lightcraft_pipeline::RenderRequest::fit(box_w, box_h)
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

    /// Decode a proxy source into the cache without rendering, so the next
    /// render at this edge hits (plan section 2.3 neighbor preload).
    pub fn prewarm(&self, path: &Path, edge: u16, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        self.proxy_source(path, edge, cancel).map(|_| ())
    }

    /// The cached source for `(file, edge)`: a hit, the shared outcome of a
    /// decode already in flight, or a fresh decode this caller owns.
    fn proxy_source(
        &self,
        path: &Path,
        edge: u16,
        cancel: &CancelToken,
    ) -> Result<Arc<DecodeSource>> {
        if edge == 0 {
            return Err(Error::InvalidInput("proxy edge must be nonzero".into()));
        }
        let key: DecodeKey = (
            path.to_path_buf(),
            fs_mtime_nanos(path),
            fs_size(path),
            edge,
        );
        loop {
            let claim = lock(&self.decoded).claim(&key);
            match claim {
                Claim::Hit(source) => return Ok(source),
                Claim::Owner(slot) => {
                    let guard = OwnerGuard {
                        cache: &self.decoded,
                        key: &key,
                        slot,
                        settled: false,
                    };
                    return match self.decode_source(path, edge as usize, cancel) {
                        Ok(source) => {
                            let source = Arc::new(source);
                            guard.bank(Arc::clone(&source));
                            Ok(source)
                        }
                        Err(e) => {
                            guard.abandon(match &e {
                                Error::Cancelled => DecodeFailure::Cancelled,
                                other => DecodeFailure::Failed(other.to_string()),
                            });
                            Err(e)
                        }
                    };
                }
                Claim::Wait(slot) => match slot.wait(cancel) {
                    Some(Ok(source)) => return Ok(source),
                    Some(Err(DecodeFailure::Failed(message))) => {
                        return Err(Error::Engine {
                            stage: "decode",
                            message,
                        });
                    }
                    // The owner was cancelled and this caller was not: claim
                    // the key again.
                    Some(Err(DecodeFailure::Cancelled)) => {}
                    None => return Err(Error::Cancelled),
                },
            }
        }
    }

    /// The decode itself: read the file, then decode at `edge`.
    fn decode_source(
        &self,
        path: &Path,
        edge: usize,
        cancel: &CancelToken,
    ) -> Result<DecodeSource> {
        self.decodes.fetch_add(1, Ordering::Relaxed);
        cancel.check()?;
        let bytes = std::fs::read(path).map_err(|e| Error::Engine {
            stage: "decode",
            message: format!("cannot read {}: {e}", path.display()),
        })?;
        let (img, info) = decode_bytes(bytes, edge, cancel).map_err(|e| {
            // The bridge reports cancellation as text; restore the variant.
            if cancel.is_cancelled() {
                Error::Cancelled
            } else {
                e
            }
        })?;
        Ok(DecodeSource {
            img: Arc::new(img),
            info,
            stages: lightcraft_pipeline::StageCache::default(),
        })
    }
}

fn decode_bytes(
    bytes: Vec<u8>,
    edge: usize,
    cancel: &CancelToken,
) -> Result<(lightcraft_raster::Rgb32f, lightcraft_pipeline::SourceInfo)> {
    let cow = Cow::Owned(bytes);
    cancel.check()?;
    let engine = |message| Error::Engine {
        stage: "decode",
        message,
    };
    if lightcraft_raw::probe(&cow).is_some() {
        lightcraft::load_bytes(cow, edge, cancel).map_err(engine)
    } else {
        plain::decode_plain(&cow, edge, cancel)
            .map(|p| (p.image, p.info))
            .map_err(engine)
    }
}

impl DevelopEngine for RenderService {
    /// Resolve the item's photo path through the resolver and render. The
    /// export path (E1) and re-rendering a retouch recipe (E3) are not built
    /// yet, so a request that asks for either is rejected, never dropped.
    fn render(&self, req: RenderRequest, cancel: &CancelToken) -> Result<Frame> {
        if req.color == OutputColor::ExportSrgb {
            return Err(Error::InvalidInput("export renders land in E1".into()));
        }
        if req
            .retouch
            .as_ref()
            .is_some_and(|recipe| !recipe.is_empty())
        {
            return Err(Error::InvalidInput("retouch re-renders land in E3".into()));
        }
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

/// The documented no-op retouch stage: frames pass through unretouched until
/// R2 applies the real mosaic/gaussian ops post-develop.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRetouch;

impl RetouchStage for NoopRetouch {
    fn apply(&self, _frame: &mut Frame, _recipe: &RetouchRecipe, _mask: &MaskPlane) -> Result<()> {
        Ok(())
    }
}

/// Map the app's crop into the develop settings' own crop model.
///
/// The two share one basis (see [`Crop`]): the engine turns the oriented
/// image by `angle` clockwise about its centre on a canvas of the same size
/// and crops the normalized `rect` out of that canvas. So the mapping is a
/// copy, with no angle conversion. The crop is the only crop model: it
/// replaces whatever the develop settings carry, and `None` means no crop.
///
/// The frame is not pulled inside the rotated image here, because the crop
/// canvas renders the whole straightened frame on purpose. Callers editing a
/// frame constrain it with [`edit_core::crop::constrain_rect`]. The aspect
/// lock is app state; the engine does not read it.
fn apply_crop(develop: &mut DevelopSettings, crop: Option<&Crop>) {
    let geometry = &mut develop.crop.geometry;
    develop.crop.aspect = None;
    let Some(crop) = crop else {
        *geometry = Default::default();
        return;
    };
    let min = edit_core::crop::MIN_EDGE;
    let mut rect = crop.rect.clamped();
    rect.w = rect.w.max(min).min(1.0);
    rect.h = rect.h.max(min).min(1.0);
    rect.x = rect.x.min(1.0 - rect.w);
    rect.y = rect.y.min(1.0 - rect.h);
    geometry.rect = lightcraft_geom::Rect {
        x0: rect.x as f64,
        y0: rect.y as f64,
        x1: (rect.x + rect.w) as f64,
        y1: (rect.y + rect.h) as f64,
    };
    geometry.angle = (crop.rotation_degrees as f64).clamp(
        -(edit_core::crop::MAX_ANGLE as f64),
        edit_core::crop::MAX_ANGLE as f64,
    );
}

/// The crop frame's size in pixels of a `width × height` source: the largest
/// output the render may produce without upscaling. An uncropped render is
/// the source's own size.
fn crop_box(develop: &DevelopSettings, width: usize, height: usize) -> (usize, usize) {
    let rect = develop.crop.geometry.rect;
    let px = |span: f64, edge: usize| ((span * edge as f64).round() as usize).max(1);
    (px(rect.x1 - rect.x0, width), px(rect.y1 - rect.y0, height))
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

/// Nanoseconds since the epoch for `path`: sub-second identity in the decode
/// key, so an edit saved right after a render still misses.
fn fs_mtime_nanos(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// File size in bytes, part of the decode key (mtime alone can repeat).
fn fs_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    fn write_png(path: &Path, img: &image::RgbImage) {
        img.save_with_format(path, image::ImageFormat::Png).unwrap();
    }

    /// Render `path` at full size with `crop`.
    fn render_full(path: &Path, crop: Option<&Crop>) -> Frame {
        RenderService::new()
            .render_path(
                path,
                RenderSize::Full,
                &DevelopSettings::default(),
                crop,
                &CancelToken::new(),
            )
            .unwrap()
    }

    /// Brightness of a BGRA pixel, 0..255.
    fn bright(frame: &Frame, x: u32, y: u32) -> f32 {
        let p = frame.pixel(x, y).unwrap();
        (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0
    }

    /// Mean y of the dark pixels in the columns `x0..x1`, weighted by how
    /// dark they are against a bright background.
    fn line_y(frame: &Frame, x0: u32, x1: u32) -> f32 {
        let (mut sum, mut weight) = (0.0, 0.0);
        for x in x0..x1 {
            for y in 0..frame.height {
                let w = (255.0 - bright(frame, x, y)).max(0.0);
                sum += w * y as f32;
                weight += w;
            }
        }
        assert!(weight > 0.0, "no line in columns {x0}..{x1}");
        sum / weight
    }

    #[test]
    fn the_crop_rect_selects_the_source_region() {
        let dir = scratch("crop-rect");
        let path = dir.join("halves.png");
        // Left half dark, right half bright.
        let (w, h) = (48u32, 32u32);
        let img = image::RgbImage::from_fn(w, h, |x, _| {
            image::Rgb([if x < w / 2 { 40 } else { 220 }; 3])
        });
        write_png(&path, &img);

        let full = render_full(&path, None);
        assert_eq!((full.width, full.height), (48, 32));
        let right = Crop {
            rect: edit_core::Rect::new(0.5, 0.0, 0.5, 1.0),
            ..Crop::default()
        };
        let cropped = render_full(&path, Some(&right));
        assert_eq!((cropped.width, cropped.height), (24, 32));
        assert!(bright(&cropped, 2, 16) > 180.0 && bright(&cropped, 21, 16) > 180.0);
        let left = Crop {
            rect: edit_core::Rect::new(0.0, 0.0, 0.5, 1.0),
            ..Crop::default()
        };
        let cropped = render_full(&path, Some(&left));
        assert!(bright(&cropped, 2, 16) < 90.0 && bright(&cropped, 21, 16) < 90.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_crop_means_no_crop_even_when_the_develop_settings_carry_one() {
        let dir = scratch("crop-none");
        let path = dir.join("plain.png");
        write_png(
            &path,
            &image::RgbImage::from_pixel(40, 30, image::Rgb([90; 3])),
        );
        let mut stale = DevelopSettings::default();
        stale.crop.geometry.rect = lightcraft_geom::Rect {
            x0: 0.25,
            y0: 0.25,
            x1: 0.5,
            y1: 0.5,
        };
        stale.crop.geometry.angle = 9.0;
        let frame = RenderService::new()
            .render_path(&path, RenderSize::Full, &stale, None, &CancelToken::new())
            .unwrap();
        assert_eq!((frame.width, frame.height), (40, 30));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_positive_angle_levels_a_horizon_that_rises_to_the_right() {
        // A dark 3 px line on white, rising to the right at 6°. A clockwise
        // (positive) straighten turns it level; a negative one doubles the
        // tilt. This pins the engine's sign to the one `Crop` documents.
        let dir = scratch("crop-sign");
        let path = dir.join("horizon.png");
        let (w, h) = (240u32, 160u32);
        let slope = 6f32.to_radians().tan();
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let line_y = 80.0 - slope * (x as f32 - 120.0);
            let dark = (y as f32 + 0.5 - line_y).abs() <= 1.5;
            image::Rgb([if dark { 0 } else { 255 }; 3])
        });
        write_png(&path, &img);
        let frame = |angle: f32| {
            render_full(
                &path,
                Some(&Crop {
                    rect: edit_core::Rect::new(0.2, 0.3, 0.6, 0.4),
                    rotation_degrees: angle,
                    aspect: None,
                }),
            )
        };
        // Left vs right end of the line, in output rows.
        let tilt = |f: &Frame| line_y(f, f.width - 6, f.width - 2) - line_y(f, 2, 6);
        let level = frame(6.0);
        assert!(
            tilt(&level).abs() < 2.0,
            "+6° left a tilt of {}",
            tilt(&level)
        );
        let flat = frame(0.0);
        assert!(tilt(&flat) < -10.0, "0° keeps the rise: {}", tilt(&flat));
        let worse = frame(-6.0);
        assert!(
            tilt(&worse) < -25.0,
            "-6° doubles the rise: {}",
            tilt(&worse)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_rendered_frame_corners_are_the_ones_the_geometry_predicts() {
        // Mark the source point behind each corner of a rotated frame, as
        // `source_corners` predicts it, and look for the marks in the
        // matching output corners. The rect, the angle and the corner order
        // all have to agree between the pure model and the engine.
        let dir = scratch("crop-corners");
        let path = dir.join("corners.png");
        let (w, h) = (240u32, 160u32);
        let crop = Crop {
            rect: edit_core::Rect::new(0.2, 0.25, 0.6, 0.5),
            rotation_degrees: 12.0,
            aspect: None,
        };
        let corners =
            edit_core::crop::source_corners(crop.rect, crop.rotation_degrees, (w as f64, h as f64));
        let colors = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
        let mut img = image::RgbImage::from_pixel(w, h, image::Rgb([30; 3]));
        for (&(cx, cy), color) in corners.iter().zip(colors) {
            for y in 0..h {
                for x in 0..w {
                    if (x as f64 + 0.5 - cx).abs() <= 6.0 && (y as f64 + 0.5 - cy).abs() <= 6.0 {
                        img.put_pixel(x, y, image::Rgb(color));
                    }
                }
            }
        }
        write_png(&path, &img);

        let frame = render_full(&path, Some(&crop));
        // The frame is 0.6 × 0.5 of the source, at the source's resolution.
        assert_eq!((frame.width, frame.height), (144, 80));
        let at = |x: u32, y: u32| {
            let p = frame.pixel(x, y).unwrap();
            [p[2], p[1], p[0]]
        };
        let near = |got: [u8; 3], want: [u8; 3]| {
            got.iter()
                .zip(want)
                .all(|(g, w)| (*g as i32 - w as i32).abs() < 60)
        };
        let (fw, fh) = (frame.width - 3, frame.height - 3);
        assert!(near(at(2, 2), colors[0]), "TL {:?}", at(2, 2));
        assert!(near(at(fw, 2), colors[1]), "TR {:?}", at(fw, 2));
        assert!(near(at(fw, fh), colors[2]), "BR {:?}", at(fw, fh));
        assert!(near(at(2, fh), colors[3]), "BL {:?}", at(2, fh));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_straighten_preview_matches_the_pipeline_crop() {
        // The live preview turns the rendered image; the pipeline turns the
        // source. Cropping the preview to the same frame has to give the same
        // picture, or the frame would jump when the accurate render lands.
        let dir = scratch("straighten-agree");
        let path = dir.join("scene.png");
        let (w, h) = (640u32, 400u32);
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let (dx, dy) = (x as f32 - 400.0, y as f32 - 150.0);
            let blob = (1.0 - (dx * dx + dy * dy).sqrt() / 70.0).clamp(0.0, 1.0);
            let base = 50.0 + 60.0 * x as f32 / w as f32 + 40.0 * y as f32 / h as f32;
            image::Rgb([(base + 140.0 * blob).min(255.0) as u8; 3])
        });
        write_png(&path, &img);
        let crop = Crop {
            rect: edit_core::Rect::new(0.25, 0.25, 0.5, 0.5),
            rotation_degrees: 7.0,
            aspect: None,
        };
        let engine = render_full(&path, Some(&crop));
        assert_eq!((engine.width, engine.height), (320, 200));

        let rgba = image::DynamicImage::ImageRgb8(img).to_rgba8();
        let turned = straighten::straighten_preview(&rgba, 7.0);
        let (x0, y0) = (160u32, 100u32);
        let mut diff = 0.0f32;
        for y in 0..200 {
            for x in 0..320 {
                let p = turned.get_pixel(x0 + x, y0 + y)[0] as f32;
                diff += (p - bright(&engine, x, y)).abs();
            }
        }
        diff /= (320 * 200) as f32;
        assert!(diff < 8.0, "preview and pipeline differ by {diff} levels");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn proxy_and_full_renders_of_a_rotated_crop_agree() {
        // A soft gradient with one bright blob, cropped and straightened. The
        // proxy and the final render run the same path at two sizes, so the
        // frame aspect, the blob position and the pixels have to line up.
        let dir = scratch("crop-proxy-full");
        let path = dir.join("scene.png");
        let (w, h) = (640u32, 400u32);
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let (dx, dy) = (x as f32 - 400.0, y as f32 - 150.0);
            let blob = (1.0 - (dx * dx + dy * dy).sqrt() / 60.0).clamp(0.0, 1.0);
            let base = 60.0 + 60.0 * x as f32 / w as f32 + 30.0 * y as f32 / h as f32;
            image::Rgb([(base + 140.0 * blob).min(255.0) as u8; 3])
        });
        write_png(&path, &img);
        let crop = Crop {
            rect: edit_core::Rect::new(0.25, 0.15, 0.6, 0.6),
            rotation_degrees: 7.0,
            aspect: Some(edit_core::Aspect::ThreeTwo),
        };
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let full = service
            .render_path(&path, RenderSize::Full, &develop, Some(&crop), &cancel)
            .unwrap();
        let proxy = service
            .render_path(
                &path,
                RenderSize::Proxy(200),
                &develop,
                Some(&crop),
                &cancel,
            )
            .unwrap();
        assert_eq!((full.width, full.height), (384, 240));
        assert!(proxy.width.max(proxy.height) <= 200);

        let ratio = |f: &Frame| f.width as f32 / f.height as f32;
        assert!((ratio(&full) - ratio(&proxy)).abs() < 0.03);

        // Brightness-weighted centroid of the blob, as a share of the frame.
        let centroid = |f: &Frame| {
            let (mut sx, mut sy, mut weight) = (0.0, 0.0, 0.0);
            for y in 0..f.height {
                for x in 0..f.width {
                    let v = (bright(f, x, y) - 190.0).max(0.0);
                    sx += v * (x as f32 + 0.5);
                    sy += v * (y as f32 + 0.5);
                    weight += v;
                }
            }
            assert!(weight > 0.0, "no blob in a {}x{} frame", f.width, f.height);
            (sx / weight / f.width as f32, sy / weight / f.height as f32)
        };
        let (fc, pc) = (centroid(&full), centroid(&proxy));
        assert!(
            (fc.0 - pc.0).abs() < 0.02 && (fc.1 - pc.1).abs() < 0.02,
            "blob moved between sizes: {fc:?} vs {pc:?}"
        );

        // Pixel agreement once the final render is brought down to proxy size.
        let rgba =
            image::RgbaImage::from_raw(full.width, full.height, full.pixels.clone()).unwrap();
        let down = image::imageops::resize(
            &rgba,
            proxy.width,
            proxy.height,
            image::imageops::FilterType::Triangle,
        );
        let diff: f32 = down
            .pixels()
            .zip(proxy.pixels.as_chunks::<4>().0)
            .map(|(a, b)| (a[0] as f32 - b[0] as f32).abs())
            .sum::<f32>()
            / (proxy.width * proxy.height) as f32;
        assert!(diff < 6.0, "proxy and final differ by {diff} levels");
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
    fn raw_fixture_develops_proxy_and_embedded_preview_fallback() {
        // A camera raw (CC0 sample from raw.pixls.us; multi-MB camera data
        // that is not committed): EDIT_RENDER_RAW_FIXTURE, else
        // tests/fixtures/sample.nef. Skips cleanly when absent.
        let fixture = std::env::var_os("EDIT_RENDER_RAW_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.nef")
            });
        if !fixture.is_file() {
            eprintln!("skip: no raw fixture at {}", fixture.display());
            return;
        }
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        let drag = service
            .render_path(
                &fixture,
                RenderSize::Proxy(PROXY_DRAG_LONG_EDGE),
                &develop,
                None,
                &cancel,
            )
            .unwrap();
        assert!(drag.width.max(drag.height) <= PROXY_DRAG_LONG_EDGE as u32);
        let release = service
            .render_path(
                &fixture,
                RenderSize::Proxy(PROXY_RELEASE_LONG_EDGE),
                &develop,
                None,
                &cancel,
            )
            .unwrap();
        assert!(release.width.max(release.height) <= PROXY_RELEASE_LONG_EDGE as u32);
        assert!(release.width >= drag.width);
        // The embedded camera preview is the fallback for undecodable raws.
        let bytes = std::fs::read(&fixture).unwrap();
        let (preview, _) = lightcraft::load_embedded_preview(&bytes, 800)
            .expect("fixture carries an embedded preview");
        assert!(preview.width.max(preview.height) <= 800);
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("edit-render-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn jpeg_bytes(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(color));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        bytes
    }

    /// A little-endian TIFF the raw prober takes for an undecodable CFA
    /// container (`OtherTiff`), carrying `jpeg` as its JPEGInterchangeFormat
    /// preview.
    fn tiff_with_preview(jpeg: &[u8]) -> Vec<u8> {
        let entry = |tag: u16, kind: u16, count: u32, value: u32| {
            let mut e = Vec::new();
            e.extend(tag.to_le_bytes());
            e.extend(kind.to_le_bytes());
            e.extend(count.to_le_bytes());
            e.extend(value.to_le_bytes());
            e
        };
        let ifd_len = 2 + 4 * 12 + 4;
        let make_at = 8 + ifd_len as u32;
        let jpeg_at = make_at + 6;
        let mut out = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        out.extend(4u16.to_le_bytes());
        // Tags ascending: Compression (a CFA-only codec), Make, JPEG offset/len.
        out.extend(entry(0x0103, 3, 1, 34713));
        out.extend(entry(0x010F, 2, 5, make_at));
        out.extend(entry(0x0201, 4, 1, jpeg_at));
        out.extend(entry(0x0202, 4, 1, jpeg.len() as u32));
        out.extend(0u32.to_le_bytes());
        out.extend(b"TEST\0\0");
        out.extend(jpeg);
        out
    }

    fn test_key(path: &Path, edge: u16) -> DecodeKey {
        (
            path.to_path_buf(),
            fs_mtime_nanos(path),
            fs_size(path),
            edge,
        )
    }

    fn fake_source(w: usize, h: usize) -> Arc<DecodeSource> {
        Arc::new(DecodeSource {
            img: Arc::new(lightcraft_raster::Rgb32f::new(w, h)),
            info: lightcraft_pipeline::SourceInfo::default(),
            stages: lightcraft_pipeline::StageCache::default(),
        })
    }

    fn key(n: u16) -> DecodeKey {
        (PathBuf::from("/photos/a.jpg"), 1, 1, n)
    }

    #[test]
    fn unsupported_raw_falls_back_to_the_embedded_preview() {
        let dir = scratch("preview-fallback");
        let path = dir.join("undecodable.nef");
        let tiff = tiff_with_preview(&jpeg_bytes(64, 40, [200, 100, 40]));
        std::fs::write(&path, &tiff).unwrap();
        assert!(
            lightcraft_raw::probe(&tiff).is_some(),
            "fixture is not a raw"
        );
        assert!(matches!(
            lightcraft_raw::decode(&tiff),
            Err(lightcraft_raw::RawError::Unsupported(_))
        ));

        let service = RenderService::new();
        let frame = service
            .render_path(
                &path,
                RenderSize::Proxy(32),
                &DevelopSettings::default(),
                None,
                &CancelToken::new(),
            )
            .unwrap();
        assert!(frame.width <= 32 && frame.height <= 32);
        let mean = mean_channels(&frame);
        assert!(
            mean[0] < mean[2],
            "preview did not carry its color: {mean:?}"
        );
        // Full takes the same fallback at the preview's own size.
        let full = service
            .render_path(
                &path,
                RenderSize::Full,
                &DevelopSettings::default(),
                None,
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!((full.width, full.height), (64, 40));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn proxy_zero_is_rejected() {
        let dir = scratch("proxy-zero");
        let path = dir.join("zero.jpg");
        tiny_jpeg(&path, 8, 8, [10, 10, 10]);
        let result = RenderService::new().render_path(
            &path,
            RenderSize::Proxy(0),
            &DevelopSettings::default(),
            None,
            &CancelToken::new(),
        );
        assert!(matches!(result, Err(Error::InvalidInput(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_misses_share_one_decode() {
        let dir = scratch("shared-decode");
        let path = dir.join("shared.jpg");
        std::fs::write(&path, jpeg_bytes(640, 480, [90, 120, 150])).unwrap();
        let service = RenderService::new();
        let barrier = std::sync::Barrier::new(6);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        service.render_path(
                            &path,
                            RenderSize::Proxy(200),
                            &DevelopSettings::default(),
                            None,
                            &CancelToken::new(),
                        )
                    })
                })
                .collect();
            for handle in handles {
                assert!(handle.join().unwrap().is_ok());
            }
        });
        assert_eq!(service.decodes.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_waiter_retries_when_the_owner_is_cancelled() {
        let dir = scratch("owner-cancelled");
        let path = dir.join("retry.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let service = RenderService::new();
        let key = test_key(&path, 16);
        let Claim::Owner(slot) = lock(&service.decoded).claim(&key) else {
            panic!("first claim must own the decode");
        };
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                service.render_path(
                    &path,
                    RenderSize::Proxy(16),
                    &DevelopSettings::default(),
                    None,
                    &CancelToken::new(),
                )
            });
            std::thread::sleep(Duration::from_millis(60));
            lock(&service.decoded).abandon(&key);
            slot.publish(Err(DecodeFailure::Cancelled));
            // The waiter was not cancelled, so it claims the key and decodes.
            assert!(waiter.join().unwrap().is_ok());
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn waiters_receive_the_owners_banked_source() {
        let dir = scratch("owner-banks");
        let path = dir.join("bank.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let service = RenderService::new();
        let key = test_key(&path, 16);
        let Claim::Owner(slot) = lock(&service.decoded).claim(&key) else {
            panic!("first claim must own the decode");
        };
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                service.render_path(
                    &path,
                    RenderSize::Proxy(16),
                    &DevelopSettings::default(),
                    None,
                    &CancelToken::new(),
                )
            });
            std::thread::sleep(Duration::from_millis(60));
            let source = fake_source(32, 20);
            lock(&service.decoded).bank(&key, Arc::clone(&source));
            slot.publish(Ok(source));
            assert!(waiter.join().unwrap().is_ok());
        });
        // The waiter used the owner's source; nothing decoded again.
        assert_eq!(service.decodes.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cancelled_waiter_stops_waiting() {
        let dir = scratch("waiter-cancelled");
        let path = dir.join("wait.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let service = RenderService::new();
        let key = test_key(&path, 16);
        let Claim::Owner(_slot) = lock(&service.decoded).claim(&key) else {
            panic!("first claim must own the decode");
        };
        let token = CancelToken::new();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                service.render_path(
                    &path,
                    RenderSize::Proxy(16),
                    &DevelopSettings::default(),
                    None,
                    &token,
                )
            });
            std::thread::sleep(Duration::from_millis(30));
            token.cancel();
            assert_eq!(waiter.join().unwrap(), Err(Error::Cancelled));
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cancelled_decode_reports_cancelled_not_a_failure() {
        let dir = scratch("decode-cancelled");
        let path = dir.join("cancel.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let token = CancelToken::new();
        token.cancel();
        let result = RenderService::new().decode_source(&path, 16, &token);
        assert!(matches!(result, Err(Error::Cancelled)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_decode_frees_its_key_for_a_retry() {
        let service = RenderService::new();
        let missing = std::env::temp_dir().join("edit-render-missing-file.jpg");
        let result = service.render_path(
            &missing,
            RenderSize::Proxy(16),
            &DevelopSettings::default(),
            None,
            &CancelToken::new(),
        );
        assert!(matches!(
            result,
            Err(Error::Engine {
                stage: "decode",
                ..
            })
        ));
        let cache = lock(&service.decoded);
        assert!(cache.pending.is_empty() && cache.sources.is_empty());
    }

    #[test]
    fn full_renders_are_not_banked_and_proxies_are() {
        let dir = scratch("full-not-banked");
        let path = dir.join("full.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let service = RenderService::new();
        let cancel = CancelToken::new();
        let develop = DevelopSettings::default();
        service
            .render_path(&path, RenderSize::Full, &develop, None, &cancel)
            .unwrap();
        assert!(lock(&service.decoded).sources.is_empty());
        for _ in 0..2 {
            service
                .render_path(&path, RenderSize::Proxy(16), &develop, None, &cancel)
                .unwrap();
        }
        assert_eq!(lock(&service.decoded).sources.len(), 1);
        // Full + one proxy decode; the second proxy render hit the bank.
        assert_eq!(service.decodes.load(Ordering::Relaxed), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prewarm_makes_the_next_render_a_hit() {
        let dir = scratch("prewarm");
        let path = dir.join("warm.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let service = RenderService::new();
        let cancel = CancelToken::new();
        service.prewarm(&path, 16, &cancel).unwrap();
        service
            .render_path(
                &path,
                RenderSize::Proxy(16),
                &DevelopSettings::default(),
                None,
                &cancel,
            )
            .unwrap();
        assert_eq!(service.decodes.load(Ordering::Relaxed), 1);
        assert!(service.prewarm(&path, 0, &cancel).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_decode_cache_is_bounded_by_bytes() {
        // Each source holds 100 × 100 × 12 = 120 kB; the budget fits two.
        let one = fake_source(100, 100).held_bytes();
        assert_eq!(one, 100 * 100 * 12);
        let mut cache = DecodeCache::new(8, one * 2 + one / 2);
        for n in 1..=3 {
            assert!(matches!(cache.claim(&key(n)), Claim::Owner(_)));
            cache.bank(&key(n), fake_source(100, 100));
        }
        assert_eq!(cache.sources.len(), 2);
        assert!(cache.bytes_held() <= one * 2 + one / 2);
        // The least recently used entry went; the fresh one stayed.
        assert!(!cache.sources.contains_key(&key(1)));
        assert!(cache.sources.contains_key(&key(3)));
        // One oversize entry still banks; everything else makes room.
        assert!(matches!(cache.claim(&key(4)), Claim::Owner(_)));
        cache.bank(&key(4), fake_source(200, 200));
        assert_eq!(cache.sources.len(), 1);
        assert!(cache.sources.contains_key(&key(4)));
    }

    #[test]
    fn the_decode_cache_evicts_least_recently_used_past_the_entry_cap() {
        let mut cache = DecodeCache::new(2, usize::MAX);
        for n in 1..=2 {
            assert!(matches!(cache.claim(&key(n)), Claim::Owner(_)));
            cache.bank(&key(n), fake_source(4, 4));
        }
        // A hit on 1 makes 2 the eviction candidate.
        assert!(matches!(cache.claim(&key(1)), Claim::Hit(_)));
        assert!(matches!(cache.claim(&key(3)), Claim::Owner(_)));
        // A second claim on the same in-flight key waits.
        assert!(matches!(cache.claim(&key(3)), Claim::Wait(_)));
        cache.bank(&key(3), fake_source(4, 4));
        assert!(cache.sources.contains_key(&key(1)));
        assert!(!cache.sources.contains_key(&key(2)));
        assert!(cache.sources.contains_key(&key(3)));
        assert!(cache.pending.is_empty());
    }

    #[test]
    fn pending_decode_hands_the_outcome_to_waiters() {
        let slot = Arc::new(PendingDecode::new());
        let waiter = Arc::clone(&slot);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            slot.publish(Err(DecodeFailure::Failed("disk full".to_string())));
        });
        let observed = waiter.wait(&CancelToken::new());
        handle.join().unwrap();
        assert!(matches!(
            observed,
            Some(Err(DecodeFailure::Failed(message))) if message == "disk full"
        ));
    }

    struct FixedPath(Option<PathBuf>);

    impl ItemPhotoPaths for FixedPath {
        fn photo_path(&self, _item: ItemId) -> Result<Option<PathBuf>> {
            Ok(self.0.clone())
        }
    }

    fn request(item: i64) -> RenderRequest {
        RenderRequest {
            item: ItemId::new(item),
            size: RenderSize::Proxy(16),
            develop: DevelopSettings::default(),
            crop: None,
            retouch: None,
            color: OutputColor::DisplaySrgb,
        }
    }

    #[test]
    fn the_develop_engine_renders_and_rejects_what_it_cannot_do() {
        let dir = scratch("engine-contract");
        let path = dir.join("engine.jpg");
        tiny_jpeg(&path, 32, 20, [50, 60, 70]);
        let engine = RenderService::with_paths(Arc::new(FixedPath(Some(path))));
        let cancel = CancelToken::new();
        assert!(engine.render(request(1), &cancel).is_ok());

        let mut export = request(1);
        export.color = OutputColor::ExportSrgb;
        assert!(matches!(
            engine.render(export, &cancel),
            Err(Error::InvalidInput(m)) if m.contains("E1")
        ));

        let mut retouched = request(1);
        retouched.retouch = Some(RetouchRecipe::new(vec![edit_core::RetouchOp::Mosaic {
            cell_px: 12,
        }]));
        assert!(matches!(
            engine.render(retouched, &cancel),
            Err(Error::InvalidInput(m)) if m.contains("E3")
        ));
        // An empty recipe is the same as none.
        let mut empty = request(1);
        empty.retouch = Some(RetouchRecipe::default());
        assert!(engine.render(empty, &cancel).is_ok());

        let nothing = RenderService::with_paths(Arc::new(FixedPath(None)));
        assert!(matches!(
            nothing.render(request(2), &cancel),
            Err(Error::InvalidInput(m)) if m.contains("no photo")
        ));
        assert!(matches!(
            RenderService::new().render(request(1), &cancel),
            Err(Error::InvalidInput(m)) if m.contains("resolver")
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_noop_retouch_stage_leaves_pixels_alone() {
        let mut frame = Frame::zeroed(2, 2).unwrap();
        frame
            .pixels
            .iter_mut()
            .enumerate()
            .for_each(|(i, p)| *p = i as u8);
        let before = frame.clone();
        let recipe = RetouchRecipe::new(vec![edit_core::RetouchOp::Gaussian { radius_px: 4.0 }]);
        let mask = MaskPlane::new(2, 2, vec![255; 4]).unwrap();
        NoopRetouch.apply(&mut frame, &recipe, &mask).unwrap();
        assert_eq!(frame, before);
    }
}
