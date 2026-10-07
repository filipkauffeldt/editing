//! `metadata_write` job body: expand an IPTC template per photo and write
//! the tags through the resident exiftool writer (plan section 1.2).
//!
//! Job grain is one item: the payload carries the item id (for badges),
//! every file path of the item (a RAW+JPEG pair is stamped on both sides),
//! the batch sequence number, and the raw `fields_json` of the template
//! row, so variables expand inside the worker against each photo's real
//! capture metadata — read from the same file that gets written.

use std::path::Path;

use edit_catalog::db::now_unix;
use edit_catalog::{ClaimedJob, JobEvent, JobEventKind};
use flume::Sender;

use crate::read_photo_metadata;
use crate::template::{TemplateFields, TemplateVars, expand};
use crate::writer::Writer;

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
    /// The template row's `fields_json` verbatim.
    pub fields_json: String,
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

/// Preview one field expansion for the UI (plan section 2.7: resolved
/// text before apply). Never fails: unreadable sources expand against
/// defaults.
pub fn preview_field(path: &Path, text: &str, sequence: u64, now_unix: i64) -> String {
    expand(text, &vars_for(path, sequence, now_unix))
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

    let now = now_unix();
    let total = payload.paths.len();
    let mut failed: Vec<String> = Vec::new();
    for (done, path_str) in payload.paths.iter().enumerate() {
        let path = Path::new(path_str);
        let vars = vars_for(path, payload.sequence, now);
        let tags = fields.to_tags(&vars);
        if tags.is_empty() {
            failed.push(format!(
                "{}: every field expanded empty; nothing written",
                path_str
            ));
            continue;
        }
        let tag_refs: Vec<(&str, &str)> =
            tags.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        if let Err(e) = writer.write_tags(path, &tag_refs) {
            failed.push(format!("{}: {e}", path_str));
            continue;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn preview_resolves_tokens() {
        let text = "{base} #{seq:3}";
        assert_eq!(preview_field(Path::new("/x/A1.JPG"), text, 2, 0), "A1 #002");
    }

    #[test]
    fn run_metadata_write_expands_and_sends_progress_per_path() {
        use crate::writer::{ScriptStep, ScriptTransport};
        use flume::unbounded;

        let fields = serde_json::json!({"credit": "{session}", "keywords": "swim; {base}"});
        let payload = serde_json::json!({
            "item_id": 5,
            "paths": ["/photos/2026 run/A1.JPG", "/photos/2026 run/A2.NEF"],
            "sequence": 1,
            "fields_json": fields.to_string(),
        });
        // Bytes and paths depend on the fixture EXIF, so the transport
        // skips exact-arg assertions; the exiftool integration test
        // (`tests/apply_exiftool.rs`) covers the real tag set.
        let writer = Writer::with_transport(ScriptTransport::new(vec![
            ScriptStep {
                want_args: None,
                reply: Ok("    1 image files updated\n".into()),
            },
            ScriptStep {
                want_args: None,
                reply: Ok("    1 image files updated\n".into()),
            },
        ]));
        let (tx, rx) = unbounded();
        let job = ClaimedJob {
            id: edit_catalog::JobId(11),
            kind: edit_catalog::JobKind::MetadataWrite,
            payload_json: payload.to_string(),
            attempts: 1,
        };
        run_metadata_write(&job, &writer, &tx).unwrap();

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
    fn run_metadata_write_fails_an_empty_expansion_like_a_write_error() {
        use crate::writer::ScriptTransport;

        let fields = serde_json::json!({"credit": "{camera}"});
        let payload = serde_json::json!({
            "item_id": 5,
            "paths": ["/photo-scratch/NO_CAMERA.NEF"],
            "sequence": 1,
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
}
