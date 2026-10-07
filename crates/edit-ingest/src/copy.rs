//! Verified copy: card -> temp -> (backup) -> atomic rename.
//!
//! The pipeline mirrors RPD (see `docs/research/workflow-apps-survey.md`):
//! the card file is streamed into a random temp name **in the destination
//! folder**, hashed with BLAKE3 while copying, re-read and re-hashed to prove
//! the bytes reached disk, then renamed atomically into the folder/filename
//! rule. A backup copy fans out **from the temp file**, never from the card.
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
    let (temp_path, mut temp_file) = create_temp(&req.dest_dir)?;

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

        let backup_path = match &req.backup_dir {
            Some(dir) => {
                on_stage(CopyStage::BackingUp);
                fs::create_dir_all(dir)?;
                let target = unique_path(dir, &req.filename)?;
                // Never re-read the card: the backup fans out from the
                // freshly verified temp file.
                fs::copy(&temp_path, &target)?;
                let backed = hash_file(&target)?;
                if backed != hash {
                    let _ = fs::remove_file(&target);
                    return Err(CopyError::VerifyMismatch {
                        path: target,
                        expected: hash,
                        actual: backed,
                    });
                }
                Some(target)
            }
            None => None,
        };

        on_stage(CopyStage::Renaming);
        let dest_path = unique_path(&req.dest_dir, &req.filename)?;
        fs::rename(&temp_path, &dest_path)?;

        Ok(CopyOutcome {
            dest_path,
            backup_path,
            bytes,
            blake3: hash,
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
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

/// `dir/filename`, or `dir/stem-N.ext` when that name is taken. Landing
/// twice under one rule must never clobber the first file.
pub fn unique_path(dir: &Path, filename: &str) -> io::Result<PathBuf> {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 1..=UNIQUE_ATTEMPTS {
        let name = match ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no unique name for {filename} in {}", dir.display()),
    ))
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
        let name = format!(".ingest-tmp-{}-{stamp}-{n}", std::process::id());
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
        let dir = std::env::temp_dir().join(format!(
            "edit-ingest-copy-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
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
            .filter(|e| e.file_name().to_string_lossy().starts_with(".ingest-tmp-"))
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
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_file_matches_known_blake3() {
        let dir = scratch("hash");
        let path = dir.join("x");
        write(&path, b"abc");
        // BLAKE3("abc") is a fixed vector.
        assert_eq!(
            hash_file(&path).unwrap(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
