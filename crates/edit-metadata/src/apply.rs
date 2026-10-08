//! `metadata_write` job body: expand an IPTC template per photo and write
//! the tags through the resident exiftool writer (plan section 1.2).
//!
//! Job grain is one item: the payload carries the item id (for badges),
//! every file path of the item (a RAW+JPEG pair is stamped on both sides),
//! the batch sequence number, the enqueue wall clock, and the raw
//! `fields_json` of the template row, so variables expand inside the
//! worker against each photo's real capture metadata — read from the same
//! file that gets written.
//!
//! Raw sides are never rewritten in place: the metadata-storage decision
//! is XMP sidecar always for raws. The worker routes a raw path to its
//! `<stem>.xmp` sidecar and writes XMP-only tags there, creating a
//! minimal XMP packet first when the sidecar does not exist yet. Plain
//! stills (JPEG masters) keep Photo-Mechanic-style in-place IIM+XMP.

use std::path::{Path, PathBuf};

use edit_catalog::db::now_unix;
use edit_catalog::{ClaimedJob, JobEvent, JobEventKind};
use flume::Sender;

use crate::CodeReplacements;
use crate::read_photo_metadata;
use crate::template::{TemplateFields, TemplateVars};
use crate::writer::Writer;

/// Raw extensions stamped through the XMP sidecar instead of in place
/// (kept in sync with the browser's raw list, `app/src/browse.rs`).
const RAW_EXTS: &[&str] = &[
    "NEF", "NRW", "ARW", "SRF", "SR2", "CR2", "CR3", "RAF", "ORF", "RW2", "DNG", "PEF",
];

/// Minimal XMP packet a first template write can start from: the
/// resident exiftool appends the proper namespaces onto it.
const IPTC_SIDECAR_STUB: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""/>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>
"#;

/// Payload of a `metadata_write` job.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataWritePayload {
    /// Item the write belongs to (drives per-item badges in the UI).
    pub item_id: i64,
    /// Every file of the item gets the template; one entry per photo
    /// side, so a RAW+JPEG pair is stamped on both.
    pub paths: Vec<String>,
    /// 1-based batch sequence for `{seq}`.
    pub sequence: u64,
    /// Enqueue wall clock: the fallback `{date}` has one value for the
    /// apply batch, so a crash-resumed run expands like the original.
    #[serde(default)]
    pub captured_at: i64,
    /// `{session}` override: an ingest names its session, which the
    /// landed file's folder (a date rule, say) does not carry. `None`
    /// keeps the folder-name default.
    #[serde(default)]
    pub session: Option<String>,
    /// The template row's `fields_json` verbatim.
    pub fields_json: String,
    /// Parsed code replacement file at apply time, so a crash-resumed job
    /// expands `=code=` / hot codes without re-reading the file.
    #[serde(default)]
    pub replacements: Option<CodeReplacements>,
}

/// True when `path` is a raw file (sidecar-stamped, never in place).
pub fn is_raw(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase());
    match ext {
        Some(ext) => RAW_EXTS.contains(&ext.as_str()),
        None => false,
    }
}

/// The XMP sidecar a raw would be stamped into (`<stem>.xmp`, the
/// preferred lookup in crate::photo).
fn sidecar_for(path: &Path) -> Option<PathBuf> {
    let mut sidecar = path.to_owned();
    sidecar.set_extension("xmp");
    Some(sidecar)
}

/// Ensure the sidecar exists (creating the minimal packet when it does
/// not) and return its path. `sidecar_for` never returns `None` for a
/// real path (set_extension always keeps a name), so the error arm is
/// handled even if it becomes unreachable for exotic inputs.
fn ready_sidecar(path: &Path) -> Result<PathBuf, String> {
    let Some(sidecar) = sidecar_for(path) else {
        return Err(format!("{path:?} has no sidecar name"));
    };
    // `create_new` so a sidecar another writer made in the meantime (a
    // `crs:` edit sidecar, say) is never overwritten with the stub.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sidecar)
    {
        Ok(mut file) => std::io::Write::write_all(&mut file, IPTC_SIDECAR_STUB.as_bytes())
            .map_err(|e| format!("sidecar {}: {e}", sidecar.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("sidecar {}: {e}", sidecar.display())),
    }
    Ok(sidecar)
}

/// Expand `fields` for one photo from its own capture metadata.
///
/// Session/file names come from the path; capture fields come from the
/// file's EXIF plus any same-name XMP sidecar. A missing EXIF capture
/// time falls back to the write wall clock, so `{date}` keeps working
/// for scans.
pub fn vars_for(path: &Path, sequence: u64, now_unix: i64) -> TemplateVars {
    let meta = read_photo_metadata(path);
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base_name = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = photo_dir_session(path);
    TemplateVars {
        sequence,
        filename,
        base_name,
        ext,
        session,
        taken_at: meta.taken_at,
        camera: meta.camera,
        lens: meta.lens,
        iso: meta.iso,
        aperture: meta.aperture,
        shutter: meta.shutter,
        focal: meta.focal,
    }
    .from_wall_clock(now_unix)
}

/// Parent directory's final segment (`{session}`); empty for bare names.
fn photo_dir_session(path: &Path) -> String {
    path.parent()
        .and_then(|dir| dir.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Preview every field of a template against one photo (plan section
/// 2.7: resolved text before apply). Never fails: unreadable sources
/// expand against defaults.
pub fn preview_fields(
    path: &Path,
    fields: &TemplateFields,
    sequence: u64,
    now_unix: i64,
) -> Vec<(&'static str, String)> {
    preview_fields_with(path, fields, sequence, now_unix, None)
}

/// [`preview_fields`] with code replacements and hot codes expanded.
pub fn preview_fields_with(
    path: &Path,
    fields: &TemplateFields,
    sequence: u64,
    now_unix: i64,
    replacements: Option<&CodeReplacements>,
) -> Vec<(&'static str, String)> {
    fields.preview_with(&vars_for(path, sequence, now_unix), replacements)
}

/// Run one claimed `metadata_write` job against a shared
/// [`Writer`]. Emits one [`JobEventKind::Progress`] per written file.
/// One item per job: any path failing fails the whole job, so the item's
/// badge goes red and the durable row keeps the diagnostic.
pub fn run_metadata_write(
    job: &ClaimedJob,
    writer: &Writer,
    tx: &Sender<JobEvent>,
) -> std::result::Result<(), String> {
    let payload: MetadataWritePayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| format!("bad metadata_write payload: {e}"))?;
    let fields: TemplateFields = serde_json::from_str(&payload.fields_json)
        .map_err(|e| format!("bad template fields: {e}"))?;
    if fields == TemplateFields::default() {
        return Err("template has no fields to write".into());
    }

    let now = if payload.captured_at > 0 {
        payload.captured_at
    } else {
        now_unix()
    };
    let total = payload.paths.len();
    let mut failed: Vec<String> = Vec::new();
    for (done, path_str) in payload.paths.iter().enumerate() {
        let path = Path::new(path_str);
        let mut vars = vars_for(path, payload.sequence, now);
        if let Some(session) = &payload.session {
            vars.session = session.clone();
        }
        // Decision (metadata storage): XMP sidecar always for raws — the
        // raw container itself is never rewritten in place. Plain stills
        // keep Photo-Mechanic-style in-place IIM+XMP.
        let raw = is_raw(path);
        let codes = payload.replacements.as_ref();
        let tags = if raw {
            fields.to_xmp_tags_with(&vars, codes)
        } else {
            fields.to_tags_with(&vars, codes)
        };
        let outcome = if tags.is_empty() {
            Err("every field expanded empty; nothing written".to_owned())
        } else if raw {
            ready_sidecar(path).and_then(|sidecar| write_tags(writer, &sidecar, tags))
        } else {
            write_tags(writer, path, tags)
        };
        match outcome {
            Ok(()) => {}
            Err(e) => {
                failed.push(format!("{}: {e}", path_str));
                continue;
            }
        }
        tx.send(JobEvent {
            id: job.id,
            kind: JobEventKind::Progress {
                done: (done + 1) as u64,
                total: total as u64,
            },
        })
        .ok();
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// Map the owning pairs to write_tags' borrowed form; empty pairs are a
/// diagnostic (nothing to write, most often all variables unavailable).
fn write_tags(writer: &Writer, path: &Path, tags: Vec<(String, String)>) -> Result<(), String> {
    if tags.is_empty() {
        return Err(format!(
            "{}: every field expanded empty; nothing written",
            path.display()
        ));
    }
    let tag_refs: Vec<(&str, &str)> = tags.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    writer
        .write_tags(path, &tag_refs)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::expand;

    #[test]
    fn vars_read_capture_fields_from_exif_fixture() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("photo.jpg");
        let vars = vars_for(&fixture, 3, 0);
        assert_eq!(vars.sequence, 3);
        assert_eq!(vars.filename, "photo.jpg");
        assert_eq!(vars.base_name, "photo");
        assert_eq!(vars.ext, "jpg");
        assert_eq!(vars.session, "fixtures");
        assert_eq!(vars.iso, Some(400));
        assert_eq!(vars.camera.as_deref(), Some("TestCam 1"));
    }

    #[test]
    fn vars_for_missing_path_is_safe() {
        let vars = vars_for(Path::new("/nonexistent/ACME01.NEF"), 1, 1_791_374_400);
        assert_eq!(vars.base_name, "ACME01");
        assert_eq!(vars.session, "nonexistent");
        assert_eq!(expand("{date} {seq}", &vars), "2026-10-07 01");
    }

    #[test]
    fn preview_resolves_tokens_for_every_field() {
        let fields = TemplateFields {
            caption: Some("{base} #{seq:3}".into()),
            credit: Some("{session}".into()),
            ..Default::default()
        };
        assert_eq!(
            preview_fields(Path::new("/x/A1.JPG"), &fields, 2, 0),
            vec![
                ("Caption", "A1 #002".to_owned()),
                ("Credit", "x".to_owned())
            ]
        );
    }

    #[test]
    fn run_metadata_write_expands_and_sends_progress_per_path() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        // The pair lives in one session folder; variables that come from
        // the path (`{session}`, `{base}`) match it.
        let pair_dir = dir.path().join("2026 run");
        std::fs::create_dir_all(&pair_dir).unwrap();
        let jpeg = pair_dir.join("A1.JPG");
        let raw = pair_dir.join("A2.NEF");
        std::fs::write(&jpeg, b"jpeg bytes").unwrap();
        std::fs::write(&raw, b"raw bytes").unwrap();

        let fields = serde_json::json!({"credit": "{session}", "keywords": "swim; {base}"});
        let payload = serde_json::json!({
            "item_id": 5,
            "paths": [jpeg.to_string_lossy(), raw.to_string_lossy()],
            "sequence": 1,
            "captured_at": 1_791_374_400,
            "fields_json": fields.to_string(),
        });
        // The A2 side is a raw: the write must target the sidecar, which
        // starts from the stub packet. Capture the args to verify.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        struct Cap(std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>);
        impl crate::Transport for Cap {
            fn execute(&mut self, args: &[String]) -> crate::Result<String> {
                self.0.lock().unwrap().push(args.to_vec());
                Ok("    1 image files updated\n".into())
            }
        }
        let writer = Writer::with_transport(Cap(std::sync::Arc::clone(&captured)));
        let (tx, rx) = flume::unbounded();
        let job = ClaimedJob {
            id: edit_catalog::JobId(11),
            kind: edit_catalog::JobKind::MetadataWrite,
            payload_json: payload.to_string(),
            attempts: 1,
        };
        run_metadata_write(&job, &writer, &tx).unwrap();

        let blocks = captured.lock().unwrap().clone();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0].last().unwrap(),
            &jpeg.to_string_lossy().to_string()
        );
        let sidecar_arg = blocks[1].last().unwrap();
        assert_eq!(
            sidecar_arg,
            &pair_dir.join("A2.xmp").to_string_lossy().to_string(),
            "raw side writes the sidecar, not the raw"
        );
        // Only XMP tags reach the sidecar.
        assert!(
            blocks[1].iter().all(|arg| !arg.starts_with("-IPTC:")),
            "sidecar write carries XMP only: {blocks:?}"
        );
        // The stub packet exists for exiftool to expand.
        assert!(sidecar_arg.ends_with(".xmp"));
        let stub = std::fs::read_to_string(pair_dir.join("A2.xmp")).unwrap();
        assert!(stub.contains("<x:xmpmeta"), "{stub}");

        let done: Vec<u64> = rx
            .drain()
            .filter_map(|event: JobEvent| match event.kind {
                JobEventKind::Progress { done, total } => Some((done, total)),
                _ => None,
            })
            .map(|(done, _)| done)
            .collect();
        assert_eq!(done, vec![1, 2], "one progress per path");
    }

    #[test]
    fn run_metadata_write_uses_the_payload_session_over_the_folder_name() {
        let fields = serde_json::json!({"credit": "{session}"});
        let payload = serde_json::json!({
            "item_id": 5,
            "paths": ["/photo-scratch/10-07/A1.JPG"],
            "sequence": 1,
            "captured_at": 1_791_374_400,
            "session": "Cup final",
            "fields_json": fields.to_string(),
        });
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        struct Cap(std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>);
        impl crate::Transport for Cap {
            fn execute(&mut self, args: &[String]) -> crate::Result<String> {
                self.0.lock().unwrap().push(args.to_vec());
                Ok("    1 image files updated\n".into())
            }
        }
        let writer = Writer::with_transport(Cap(std::sync::Arc::clone(&captured)));
        let (tx, _rx) = flume::unbounded();
        let job = ClaimedJob {
            id: edit_catalog::JobId(14),
            kind: edit_catalog::JobKind::MetadataWrite,
            payload_json: payload.to_string(),
            attempts: 1,
        };
        run_metadata_write(&job, &writer, &tx).unwrap();
        let args = captured.lock().unwrap()[0].join(" ");
        assert!(args.contains("-XMP-photoshop:Credit=Cup final"), "{args}");
        assert!(!args.contains("Credit=10-07"), "{args}");
    }

    #[test]
    fn run_metadata_write_fails_an_empty_expansion_like_a_write_error() {
        use crate::writer::ScriptTransport;

        let fields = serde_json::json!({"credit": "{camera}"});
        let payload = serde_json::json!({
            "item_id": 5,
            "paths": ["/photo-scratch/NO_CAMERA.JPG"],
            "sequence": 1,
            "captured_at": 1_791_374_400,
            "fields_json": fields.to_string(),
        });
        let writer = Writer::with_transport(ScriptTransport::new(vec![]));
        let (tx, _rx) = flume::unbounded();
        let job = ClaimedJob {
            id: edit_catalog::JobId(12),
            kind: edit_catalog::JobKind::MetadataWrite,
            payload_json: payload.to_string(),
            attempts: 1,
        };
        let err = run_metadata_write(&job, &writer, &tx).unwrap_err();
        assert!(err.contains("every field expanded empty"), "{err}");
    }

    #[test]
    fn run_metadata_write_rejects_an_empty_template() {
        use crate::writer::ScriptTransport;

        let payload = serde_json::json!({
            "item_id": 5,
            "paths": ["/p/A1.NEF"],
            "sequence": 1,
            "captured_at": 0,
            "fields_json": "{}",
        });
        let writer = Writer::with_transport(ScriptTransport::new(vec![]));
        let (tx, _rx) = flume::unbounded();
        let job = ClaimedJob {
            id: edit_catalog::JobId(13),
            kind: edit_catalog::JobKind::MetadataWrite,
            payload_json: payload.to_string(),
            attempts: 1,
        };
        let err = run_metadata_write(&job, &writer, &tx).unwrap_err();
        assert!(err.contains("no fields"), "{err}");
    }

    #[test]
    fn run_metadata_write_expands_code_replacements_from_the_payload() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let jpeg = dir.path().join("A1.JPG");
        std::fs::write(&jpeg, b"jpeg bytes").unwrap();

        let fields = serde_json::json!({"caption": "\\h1\\ watch"});
        let replacements = crate::CodeReplacements::parse("h1\tSHL\n");
        let payload = serde_json::to_string(&MetadataWritePayload {
            item_id: 5,
            paths: vec![jpeg.to_string_lossy().into_owned()],
            sequence: 1,
            captured_at: 1_791_374_400,
            fields_json: fields.to_string(),
            replacements: Some(replacements),
        })
        .unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        struct Cap(std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>);
        impl crate::Transport for Cap {
            fn execute(&mut self, args: &[String]) -> crate::Result<String> {
                self.0.lock().unwrap().push(args.to_vec());
                Ok("    1 image files updated\n".into())
            }
        }
        let writer = Writer::with_transport(Cap(std::sync::Arc::clone(&captured)));
        let (tx, _rx) = flume::unbounded();
        let job = ClaimedJob {
            id: edit_catalog::JobId(14),
            kind: edit_catalog::JobKind::MetadataWrite,
            payload_json: payload,
            attempts: 1,
        };
        run_metadata_write(&job, &writer, &tx).unwrap();
        let blocks = captured.lock().unwrap().clone();
        assert!(
            blocks[0]
                .iter()
                .any(|arg| arg == "-XMP-dc:Description=SHL watch"),
            "hot code expanded in the caption: {blocks:?}"
        );
    }
}
