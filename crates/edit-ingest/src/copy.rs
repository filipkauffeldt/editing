//! Verified copy: card -> temp -> (backup) -> atomic, no-clobber rename.
//!
//! The pipeline mirrors RPD (see `docs/research/workflow-apps-survey.md`):
//! the card file is streamed into a random temp name **in the destination
//! folder**, hashed with BLAKE3 while copying, re-read and re-hashed to prove
//! the bytes reached disk, then renamed atomically into the folder/filename
//! rule. A backup copy fans out **from the temp file**, never from the card.
//!
//! The rename never overwrites an existing file: on Linux it uses
//! `renameat2(RENAME_NOREPLACE)` and retries the next unique name on
//! `EEXIST`, so two parallel card jobs sharing a rule cannot clobber each
//! other's work. Verified destination and backup files, and their parent
//! directories, are fsynced before the rename is considered done.
//!
//! The verification hash is the identity of the landed file; the duplicate
//! check and the catalog store it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Copy buffer size: large enough that a full-frame RAW is a handful of
/// syscalls, small enough to keep the resident set flat.
const CHUNK: usize = 1 << 20;

/// Temp-name collision retries before giving up (practically never hit).
const TEMP_ATTEMPTS: u32 = 16;
/// Disambiguation suffix attempts before a unique name gives up.
const UNIQUE_ATTEMPTS: u32 = 999;
/// Prefix of the in-destination temp files, swept on the next run.
const TEMP_PREFIX: &str = ".ingest-tmp-";

/// A stage of the copy, reported so the dialog can show per-file state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyStage {
    Copying,
    Verifying,
    BackingUp,
    Renaming,
}

/// One file to land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRequest {
    pub source: PathBuf,
    /// Destination folder (already resolved from the folder rule).
    pub dest_dir: PathBuf,
    /// Destination file name (already resolved from the filename rule).
    pub filename: String,
    /// Optional backup folder; the same filename lands there.
    pub backup_dir: Option<PathBuf>,
}

/// What the pipeline produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOutcome {
    pub dest_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub bytes: u64,
    /// BLAKE3 of the verified bytes, lower-case hex.
    pub blake3: String,
}

/// A copy or verification failure.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("verify failed for {}: expected {expected}, got {actual}", path.display())]
    VerifyMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

/// Copy, verify, back up, and atomically rename one file.
pub fn copy_verified(req: &CopyRequest) -> Result<CopyOutcome, CopyError> {
    copy_verified_with(req, &mut |_| {})
}

/// [`copy_verified`] with a stage callback, so the caller can record
/// per-file progress (the ingest dialog's copying/verifying/backup/renaming).
pub fn copy_verified_with(
    req: &CopyRequest,
    on_stage: &mut dyn FnMut(CopyStage),
) -> Result<CopyOutcome, CopyError> {
    fs::create_dir_all(&req.dest_dir)?;
    sweep_temp_files(&req.dest_dir);
    let (temp_path, mut temp_file) = create_temp(&req.dest_dir)?;
    // Cleanup bookkeeping runs whatever path the inner closure takes.
    let mut backup_target: Option<PathBuf> = None;

    on_stage(CopyStage::Copying);
    let result = (|| -> Result<CopyOutcome, CopyError> {
        let (bytes, hash) = copy_hashed(&req.source, &mut temp_file)?;

        on_stage(CopyStage::Verifying);
        let actual = hash_file(&temp_path)?;
        if actual != hash {
            return Err(CopyError::VerifyMismatch {
                path: temp_path.clone(),
                expected: hash,
                actual,
            });
        }

        if let Some(dir) = &req.backup_dir {
            on_stage(CopyStage::BackingUp);
            let target = copy_backup(&temp_path, dir, &req.filename, &hash)?;
            backup_target = Some(target);
        }

        on_stage(CopyStage::Renaming);
        let dest_path = place_file(&temp_path, &req.dest_dir, &req.filename)?;

        Ok(CopyOutcome {
            dest_path,
            backup_path: backup_target.clone(),
            bytes,
            blake3: hash,
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
        if let Some(target) = backup_target {
            let _ = fs::remove_file(&target);
        }
    }
    result
}

/// BLAKE3 of a file, lower-case hex.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Remove leftover `.ingest-tmp-*` files in `dir`.
///
/// A crash mid-copy leaves a hidden temp corpse; the next run sweeps the
/// directory before writing new temps so they cannot accumulate.
pub fn sweep_temp_files(dir: &Path) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `dir/filename`, or `dir/stem-N.ext` when that name is taken. A starting
/// hint only: [`place_file`] still enforces no-clobber at rename time.
pub fn unique_path(dir: &Path, filename: &str) -> io::Result<PathBuf> {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return Ok(candidate);
    }
    for path in candidate_suffixes(dir, filename) {
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no unique name for {filename} in {}", dir.display()),
    ))
}

/// Rename `temp` into `dest_dir/filename`, disambiguating with `-N` on
/// collision. The rename itself is no-replace, so this is safe even when
/// another job lands the same name between the name choice and the rename.
fn place_file(temp: &Path, dest_dir: &Path, filename: &str) -> Result<PathBuf, CopyError> {
    let first = dest_dir.join(filename);
    if rename_noreplace(temp, &first).is_ok() {
        sync_dir(dest_dir);
        return Ok(first);
    }
    for candidate in candidate_suffixes(dest_dir, filename) {
        match rename_noreplace(temp, &candidate) {
            Ok(()) => {
                sync_dir(dest_dir);
                return Ok(candidate);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(CopyError::Io(e)),
        }
    }
    Err(CopyError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no unique name for {filename} in {}", dest_dir.display()),
    )))
}

/// The `stem-1.ext`, `stem-2.ext`, … candidates for a taken name.
fn candidate_suffixes(dir: &Path, filename: &str) -> impl Iterator<Item = PathBuf> {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename)
        .to_string();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_string);
    let dir = dir.to_path_buf();
    (1..=UNIQUE_ATTEMPTS).map(move |n| match &ext {
        Some(ext) => dir.join(format!("{stem}-{n}.{ext}")),
        None => dir.join(format!("{stem}-{n}")),
    })
}

/// Copy the verified temp into the backup folder, verifying the backup bytes
/// and removing a partial target on every failure path.
fn copy_backup(
    temp: &Path,
    dir: &Path,
    filename: &str,
    expected_hash: &str,
) -> Result<PathBuf, CopyError> {
    fs::create_dir_all(dir)?;
    sweep_temp_files(dir);
    let target = unique_path(dir, filename)?;
    let inner = || -> Result<(), CopyError> {
        // Never re-read the card: the backup fans out from the verified temp.
        fs::copy(temp, &target)?;
        File::open(&target)?.sync_all()?;
        let backed = hash_file(&target)?;
        if backed != expected_hash {
            return Err(CopyError::VerifyMismatch {
                path: target.clone(),
                expected: expected_hash.to_string(),
                actual: backed,
            });
        }
        Ok(())
    };
    match inner() {
        Ok(()) => {
            sync_dir(dir);
            Ok(target)
        }
        Err(error) => {
            let _ = fs::remove_file(&target);
            Err(error)
        }
    }
}

/// Rename without replacing an existing destination file.
///
/// Linux uses `renameat2(RENAME_NOREPLACE)`, which is atomic and reports
/// `EEXIST` when the target exists. Other platforms emulate it with a hard
/// link (also no-replace) and remove the temp; if the filesystem has no hard
/// links, a checked rename is the last resort.
#[cfg(target_os = "linux")]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has a NUL byte"))?;
    let dst = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has a NUL byte"))?;
    // SAFETY: both pointers are valid NUL-terminated C strings for the call.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// See the Linux implementation. Hard link is no-replace; removing the temp
/// leaves the linked destination. A checked rename is the fallback.
#[cfg(not(target_os = "linux"))]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::hard_link(src, dst) {
        Ok(()) => fs::remove_file(src),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if dst.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists", dst.display()),
                ));
            }
            fs::rename(src, dst)
        }
    }
}

/// fsync a directory so a just-renamed entry survives a crash. Best effort:
/// opening a directory for sync is Unix-only and may fail on some mounts.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        let _ = File::open(dir).and_then(|file| file.sync_all());
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Stream `source` into `sink`, hashing as it goes, and fsync it so the
/// verification re-read sees durable bytes.
fn copy_hashed(source: &Path, sink: &mut File) -> io::Result<(u64, String)> {
    let mut source = File::open(source)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n])?;
        total += n as u64;
    }
    sink.flush()?;
    sink.sync_all()?;
    Ok((total, hasher.finalize().to_hex().to_string()))
}

/// Create a fresh random temp file in `dir` (`.ingest-tmp-<pid>-<time>-<n>`).
fn create_temp(dir: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..TEMP_ATTEMPTS {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("{TEMP_PREFIX}{}-{stamp}-{n}", std::process::id());
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("could not create a temp file in {}", dir.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        crate::testutil::scratch_dir(&format!("copy-{name}"))
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn copies_verifies_and_renames_into_place() {
        let dir = scratch("copy");
        let card = dir.join("card");
        fs::create_dir_all(&card).unwrap();
        let source = card.join("DSC0001.NEF");
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        write(&source, &payload);

        let dest = dir.join("Photos/2026/10-07");
        let mut stages = Vec::new();
        let outcome = copy_verified_with(
            &CopyRequest {
                source: source.clone(),
                dest_dir: dest.clone(),
                filename: "shoot_0001.NEF".into(),
                backup_dir: None,
            },
            &mut |stage| stages.push(stage),
        )
        .unwrap();

        assert_eq!(outcome.dest_path, dest.join("shoot_0001.NEF"));
        assert_eq!(fs::read(&outcome.dest_path).unwrap(), payload);
        assert_eq!(outcome.bytes, payload.len() as u64);
        assert_eq!(outcome.blake3, hash_file(&source).unwrap());
        assert_eq!(outcome.backup_path, None);
        assert_eq!(
            stages,
            vec![
                CopyStage::Copying,
                CopyStage::Verifying,
                CopyStage::Renaming
            ]
        );
        // No temp file survives.
        let leftovers: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_fans_out_from_the_temp_copy() {
        let dir = scratch("backup");
        let source = dir.join("DSC0002.JPG");
        write(&source, b"backup me");
        let dest = dir.join("Photos");
        let backup = dir.join("Backup/2026");

        let outcome = copy_verified(&CopyRequest {
            source: source.clone(),
            dest_dir: dest.clone(),
            filename: "shoot_0002.JPG".into(),
            backup_dir: Some(backup.clone()),
        })
        .unwrap();

        let backup_path = outcome.backup_path.clone().expect("backup");
        assert_eq!(backup_path, backup.join("shoot_0002.JPG"));
        assert_eq!(
            fs::read(&backup_path).unwrap(),
            fs::read(&outcome.dest_path).unwrap()
        );
        assert_eq!(outcome.blake3, hash_file(&backup_path).unwrap());
        // The card is untouched and was the only source read.
        assert_eq!(fs::read(&source).unwrap(), b"backup me");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn landing_never_clobbers_an_existing_file() {
        let dir = scratch("noclobber");
        let source = dir.join("a.NEF");
        write(&source, b"incoming");
        let dest = dir.join("Photos");
        fs::create_dir_all(&dest).unwrap();
        // A pre-existing file at the exact rule name must not be replaced.
        write(&dest.join("shoot_0001.NEF"), b"existing");
        let outcome = copy_verified(&CopyRequest {
            source: source.clone(),
            dest_dir: dest.clone(),
            filename: "shoot_0001.NEF".into(),
            backup_dir: None,
        })
        .unwrap();
        assert_eq!(outcome.dest_path, dest.join("shoot_0001-1.NEF"));
        assert_eq!(fs::read(dest.join("shoot_0001.NEF")).unwrap(), b"existing");
        assert_eq!(fs::read(&outcome.dest_path).unwrap(), b"incoming");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sweep_removes_stale_temp_corpses_only() {
        let dir = scratch("sweep");
        write(&dir.join(".ingest-tmp-1-2-3"), b"corpse");
        write(&dir.join("keep.NEF"), b"photo");
        sweep_temp_files(&dir);
        assert!(!dir.join(".ingest-tmp-1-2-3").exists());
        assert!(dir.join("keep.NEF").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_never_clobbers() {
        let dir = scratch("unique");
        write(&dir.join("a.NEF"), b"one");
        assert_eq!(unique_path(&dir, "a.NEF").unwrap(), dir.join("a-1.NEF"));
        write(&dir.join("a-1.NEF"), b"two");
        assert_eq!(unique_path(&dir, "a.NEF").unwrap(), dir.join("a-2.NEF"));
        assert_eq!(unique_path(&dir, "b.NEF").unwrap(), dir.join("b.NEF"));
        // A missing source is an I/O error, not a panic.
        let err = copy_verified(&CopyRequest {
            source: dir.join("missing"),
            dest_dir: dir.clone(),
            filename: "x".into(),
            backup_dir: None,
        })
        .unwrap_err();
        assert!(matches!(err, CopyError::Io(_)));
        // The failed copy left no temp corpse.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_file_matches_known_blake3() {
        let dir = scratch("hash");
        let path = dir.join("x");
        write(&path, b"abc");
        assert_eq!(
            hash_file(&path).unwrap(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
