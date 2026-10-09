//! Smart previews: a persistent, downscaled, decoded working copy of a raw.
//!
//! The editing source for a raw is its decoded 2560 px proxy. Decoding the
//! raw takes seconds; loading this copy takes tens of milliseconds, so
//! sliders, presets and crop never touch the raw file again.
//!
//! File format (`.sp`, little endian):
//! `"SPV1"`, `u32` width, `u32` height, `u32` info length, the
//! [`SourceInfo`] as JSON, then an lz4 block (size-prepended) holding the
//! samples as IEEE half floats. The half floats keep linear light with
//! ~0.05% relative precision and the full highlight range, which is finer than
//! tone edits can show. Their bytes are stored as a low plane then a high
//! plane, which makes lz4 shrink them well.
//!
//! Entries are keyed by photo path, mtime (ns) and size: a changed file maps
//! to a new name and the stale entry for that path is dropped. The store is
//! bounded by a byte budget with least-recently-used eviction; a load counts
//! as a use (it touches the file's modification time).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use half::f16;
use lightcraft_pipeline::SourceInfo;
use lightcraft_raster::Rgb32f;
use serde::{Deserialize, Serialize};

/// Long edge of a smart preview.
pub const SMART_PREVIEW_LONG_EDGE: u16 = 2560;

/// Default disk budget of the store.
pub const SMART_PREVIEW_BUDGET_BYTES: u64 = 2 << 30;

const MAGIC: &[u8; 4] = b"SPV1";
const EXT: &str = "sp";

/// What identifies one smart preview: the file it was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartPreviewKey {
    pub path: PathBuf,
    pub mtime_nanos: i64,
    pub size: u64,
}

impl SmartPreviewKey {
    /// The key for the file as it is on disk now.
    pub fn for_file(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            mtime_nanos: crate::fs_mtime_nanos(path),
            size: crate::fs_size(path),
        }
    }

    fn path_tag(path: &Path) -> String {
        let hash = blake3::hash(path.as_os_str().as_encoded_bytes());
        hash.to_hex()[..24].to_string()
    }

    fn file_name(&self) -> String {
        format!(
            "{}-{}-{}.{EXT}",
            Self::path_tag(&self.path),
            self.mtime_nanos,
            self.size
        )
    }
}

/// A loaded smart preview.
pub struct SmartPreview {
    pub image: Rgb32f,
    pub info: SourceInfo,
}

/// The facts of [`SourceInfo`] in a serializable shape.
#[derive(Serialize, Deserialize)]
struct InfoWire {
    lens: Option<lightcraft_develop::EmbeddedLens>,
    raw: bool,
    as_shot_temp: f64,
    as_shot_tint: f64,
    relative_wb: bool,
    camera_tone: Option<lightcraft_pipeline::tone::CameraTone>,
}

impl From<&SourceInfo> for InfoWire {
    fn from(i: &SourceInfo) -> Self {
        Self {
            lens: i.lens,
            raw: i.raw,
            as_shot_temp: i.as_shot_temp,
            as_shot_tint: i.as_shot_tint,
            relative_wb: i.relative_wb,
            camera_tone: i.camera_tone,
        }
    }
}

impl From<InfoWire> for SourceInfo {
    fn from(w: InfoWire) -> Self {
        Self {
            lens: w.lens,
            raw: w.raw,
            as_shot_temp: w.as_shot_temp,
            as_shot_tint: w.as_shot_tint,
            relative_wb: w.relative_wb,
            camera_tone: w.camera_tone,
        }
    }
}

/// Whether the file extension names a camera raw, the only files that get a
/// smart preview (plain files already decode in milliseconds).
pub fn is_raw_path(path: &Path) -> bool {
    const RAW: &[&str] = &[
        "nef", "nrw", "cr2", "cr3", "crw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "pef",
        "srw", "rwl", "3fr", "erf", "kdc", "mef", "mrw", "x3f", "iiq",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| RAW.iter().any(|r| e.eq_ignore_ascii_case(r)))
}

/// The on-disk store of smart previews.
#[derive(Clone, Debug)]
pub struct SmartPreviewStore {
    dir: PathBuf,
    budget_bytes: u64,
}

impl SmartPreviewStore {
    pub fn new(dir: impl Into<PathBuf>, budget_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            budget_bytes,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file_for(&self, key: &SmartPreviewKey) -> PathBuf {
        self.dir.join(key.file_name())
    }

    /// Whether a current smart preview exists for the file.
    pub fn contains(&self, key: &SmartPreviewKey) -> bool {
        self.file_for(key).is_file()
    }

    /// Load the smart preview, marking it recently used. `None` when absent
    /// or unreadable (a damaged entry is removed).
    pub fn load(&self, key: &SmartPreviewKey) -> Option<SmartPreview> {
        let file = self.file_for(key);
        let bytes = fs::read(&file).ok()?;
        match decode(&bytes) {
            Some(preview) => {
                if let Ok(f) = fs::OpenOptions::new().write(true).open(&file) {
                    f.set_modified(SystemTime::now()).ok();
                }
                Some(preview)
            }
            None => {
                fs::remove_file(&file).ok();
                None
            }
        }
    }

    /// Write the smart preview atomically, drop older entries of the same
    /// photo, and evict down to the budget.
    pub fn store(
        &self,
        key: &SmartPreviewKey,
        image: &Rgb32f,
        info: &SourceInfo,
    ) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let target = self.file_for(key);
        let tmp = self
            .dir
            .join(format!("{}.{}.tmp", key.file_name(), std::process::id()));
        let bytes = encode(image, info)?;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all().ok();
        drop(f);
        fs::rename(&tmp, &target)?;
        let tag = SmartPreviewKey::path_tag(&key.path);
        let keep = key.file_name();
        for (name, _, _) in self.entries() {
            if name != keep && name.starts_with(&tag) {
                fs::remove_file(self.dir.join(name)).ok();
            }
        }
        self.evict();
        Ok(())
    }

    /// Remove every smart preview of a photo (its row was removed).
    pub fn remove_path(&self, path: &Path) {
        let tag = SmartPreviewKey::path_tag(path);
        for (name, _, _) in self.entries() {
            if name.starts_with(&tag) {
                fs::remove_file(self.dir.join(name)).ok();
            }
        }
    }

    /// Remove every smart preview whose photo is not in `live`. Returns how
    /// many were removed.
    pub fn retain_paths<'a>(&self, live: impl IntoIterator<Item = &'a Path>) -> usize {
        let tags: std::collections::HashSet<String> =
            live.into_iter().map(SmartPreviewKey::path_tag).collect();
        let mut removed = 0;
        for (name, _, _) in self.entries() {
            let tag = name.split('-').next().unwrap_or_default();
            if !tags.contains(tag) && fs::remove_file(self.dir.join(&name)).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Bytes on disk.
    pub fn total_bytes(&self) -> u64 {
        self.entries().iter().map(|(_, _, len)| len).sum()
    }

    /// Delete least recently used entries until the store fits its budget.
    /// Returns the evicted file names in eviction order.
    pub fn evict(&self) -> Vec<String> {
        let mut entries = self.entries();
        let mut total: u64 = entries.iter().map(|e| e.2).sum();
        entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let mut evicted = Vec::new();
        for (name, _, len) in entries {
            if total <= self.budget_bytes {
                break;
            }
            if fs::remove_file(self.dir.join(&name)).is_ok() {
                total = total.saturating_sub(len);
                evicted.push(name);
            }
        }
        evicted
    }

    /// `(file name, last use, length)` of every entry.
    fn entries(&self) -> Vec<(String, SystemTime, u64)> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            if !name.ends_with(&format!(".{EXT}")) {
                return None;
            }
            let meta = e.metadata().ok()?;
            Some((name, meta.modified().ok()?, meta.len()))
        })
        .collect()
    }
}

fn encode(image: &Rgb32f, info: &SourceInfo) -> io::Result<Vec<u8>> {
    let bad = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
    let info_json = serde_json::to_vec(&InfoWire::from(info)).map_err(|e| bad(e.to_string()))?;
    let n = image.data.len() * 3;
    let mut shuffled = vec![0u8; n * 2];
    let (lo, hi) = shuffled.split_at_mut(n);
    for (i, v) in image.data.iter().flatten().enumerate() {
        let bits = f16::from_f32(*v).to_bits();
        lo[i] = bits as u8;
        hi[i] = (bits >> 8) as u8;
    }
    let packed = lz4_flex::block::compress_prepend_size(&shuffled);
    let mut out = Vec::with_capacity(packed.len() + info_json.len() + 16);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(image.width as u32).to_le_bytes());
    out.extend_from_slice(&(image.height as u32).to_le_bytes());
    out.extend_from_slice(&(info_json.len() as u32).to_le_bytes());
    out.extend_from_slice(&info_json);
    out.extend_from_slice(&packed);
    Ok(out)
}

fn decode(bytes: &[u8]) -> Option<SmartPreview> {
    let mut r = bytes;
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).ok()?;
    if &magic != MAGIC {
        return None;
    }
    let mut word = || -> Option<u32> {
        let mut b = [0u8; 4];
        r.read_exact(&mut b).ok()?;
        Some(u32::from_le_bytes(b))
    };
    let (w, h, info_len) = (word()? as usize, word()? as usize, word()? as usize);
    if r.len() < info_len || w == 0 || h == 0 {
        return None;
    }
    let (info_json, packed) = r.split_at(info_len);
    let info: SourceInfo = serde_json::from_slice::<InfoWire>(info_json).ok()?.into();
    let shuffled = lz4_flex::block::decompress_size_prepended(packed).ok()?;
    let n = w.checked_mul(h)?.checked_mul(3)?;
    if shuffled.len() != n * 2 {
        return None;
    }
    let (lo, hi) = shuffled.split_at(n);
    let mut data = vec![[0f32; 3]; w * h];
    // Rows are independent: convert them on a few threads.
    let threads = std::thread::available_parallelism()
        .map_or(1, |p| p.get())
        .min(8);
    let rows_per = h.div_ceil(threads);
    std::thread::scope(|s| {
        for (t, chunk) in data.chunks_mut(rows_per * w).enumerate() {
            let base = t * rows_per * w * 3;
            s.spawn(move || {
                for (i, px) in chunk.iter_mut().enumerate() {
                    for (c, v) in px.iter_mut().enumerate() {
                        let k = base + i * 3 + c;
                        *v = f16::from_bits(u16::from(lo[k]) | (u16::from(hi[k]) << 8)).to_f32();
                    }
                }
            });
        }
    });
    Some(SmartPreview {
        image: Rgb32f {
            width: w,
            height: h,
            data,
        },
        info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sp-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&d).ok();
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn gradient(w: usize, h: usize) -> Rgb32f {
        Rgb32f {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let t = i as f32 / (w * h) as f32;
                    [
                        t * t * 4.0,
                        (1.0 - t) * 0.8 + 0.001,
                        0.0005 + t * 12.0 - 0.02,
                    ]
                })
                .collect(),
        }
    }

    fn key(path: &str, mtime: i64, size: u64) -> SmartPreviewKey {
        SmartPreviewKey {
            path: PathBuf::from(path),
            mtime_nanos: mtime,
            size,
        }
    }

    #[test]
    fn round_trip_keeps_samples_and_source_info_within_half_float_precision() {
        let dir = temp_dir("rt");
        let store = SmartPreviewStore::new(&dir, 1 << 30);
        let img = gradient(64, 48);
        let info = SourceInfo {
            raw: true,
            as_shot_temp: 5200.0,
            as_shot_tint: 7.5,
            ..SourceInfo::default()
        };
        let k = key("/p/a.nef", 5, 9);
        store.store(&k, &img, &info).unwrap();
        let back = store.load(&k).unwrap();
        assert_eq!(back.info, info);
        assert_eq!((back.image.width, back.image.height), (64, 48));
        // Half floats carry 11 significant bits: relative error <= 2^-11,
        // and tiny values fall back to an absolute error below 1e-7.
        for (a, b) in img
            .data
            .iter()
            .flatten()
            .zip(back.image.data.iter().flatten())
        {
            let tol = a.abs() * 4.9e-4 + 1e-7;
            assert!((a - b).abs() <= tol, "{a} vs {b}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_changed_mtime_or_size_misses_and_the_stale_entry_is_replaced() {
        let dir = temp_dir("key");
        let store = SmartPreviewStore::new(&dir, 1 << 30);
        let img = gradient(8, 8);
        let info = SourceInfo::default();
        let old = key("/p/a.nef", 1, 100);
        store.store(&old, &img, &info).unwrap();
        assert!(store.contains(&old));
        assert!(!store.contains(&key("/p/a.nef", 2, 100)));
        assert!(!store.contains(&key("/p/a.nef", 1, 101)));
        assert!(!store.contains(&key("/p/b.nef", 1, 100)));
        let newer = key("/p/a.nef", 2, 100);
        store.store(&newer, &img, &info).unwrap();
        assert!(store.contains(&newer));
        assert!(!store.contains(&old), "stale entry must be dropped");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eviction_removes_least_recently_used_first_and_respects_the_budget() {
        let dir = temp_dir("lru");
        let img = gradient(32, 32);
        let info = SourceInfo::default();
        let unbounded = SmartPreviewStore::new(&dir, u64::MAX);
        let keys: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|n| key(&format!("/p/{n}.nef"), 1, 1))
            .collect();
        for k in &keys {
            unbounded.store(k, &img, &info).unwrap();
        }
        // Recency: b oldest, then c, then a (a was just used).
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for (k, age) in [(&keys[1], 1), (&keys[2], 2), (&keys[0], 3)] {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(unbounded.file_for(k))
                .unwrap();
            f.set_modified(base + std::time::Duration::from_secs(age))
                .unwrap();
        }
        let one = unbounded.total_bytes() / 3;
        let store = SmartPreviewStore::new(&dir, one * 2 + one / 2);
        let evicted = store.evict();
        assert_eq!(
            evicted,
            vec![
                unbounded
                    .file_for(&keys[1])
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            ]
        );
        assert!(store.total_bytes() <= one * 2 + one / 2);
        assert!(!store.contains(&keys[1]) && store.contains(&keys[2]) && store.contains(&keys[0]));
        // A load refreshes recency: c becomes newest, so a goes next.
        assert!(store.load(&keys[2]).is_some());
        let tight = SmartPreviewStore::new(&dir, one + one / 2);
        tight.evict();
        assert!(tight.contains(&keys[2]) && !tight.contains(&keys[0]));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_a_photo_and_retaining_live_paths_clean_up() {
        let dir = temp_dir("clean");
        let store = SmartPreviewStore::new(&dir, 1 << 30);
        let img = gradient(8, 8);
        let info = SourceInfo::default();
        let (a, b, c) = (
            key("/p/a.nef", 1, 1),
            key("/p/b.nef", 1, 1),
            key("/p/c.nef", 1, 1),
        );
        for k in [&a, &b, &c] {
            store.store(k, &img, &info).unwrap();
        }
        store.remove_path(&a.path);
        assert!(!store.contains(&a));
        let live = [b.path.as_path()];
        assert_eq!(store.retain_paths(live), 1);
        assert!(store.contains(&b) && !store.contains(&c));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn damaged_entries_are_treated_as_missing() {
        let dir = temp_dir("bad");
        let store = SmartPreviewStore::new(&dir, 1 << 30);
        let k = key("/p/a.nef", 1, 1);
        store
            .store(&k, &gradient(8, 8), &SourceInfo::default())
            .unwrap();
        let file = store.file_for(&k);
        let mut bytes = fs::read(&file).unwrap();
        bytes.truncate(bytes.len() / 2);
        fs::write(&file, bytes).unwrap();
        assert!(store.load(&k).is_none());
        assert!(!store.contains(&k));
        fs::remove_dir_all(&dir).ok();
    }
}
