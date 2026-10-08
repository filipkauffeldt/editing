//! Read the IPTC form fields of photos through exiftool.
//!
//! One exiftool run covers a whole selection (`-j`, only the tags of the
//! field table). A raw is read from its embedded metadata with the `.xmp`
//! sidecar laid over it, the same layering the writer produces (raws are
//! stamped in the sidecar only). Callers run this off the UI thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::apply::is_raw;
use crate::error::{Error, Result};
use crate::fields::{FIELDS, normalize_read};

/// Field id to form text. Lists are joined with `"; "`. Absent fields are
/// not in the map.
pub type FieldValues = BTreeMap<&'static str, String>;

fn sidecar(path: &Path) -> PathBuf {
    path.with_extension("xmp")
}

/// Read the form fields of every path; the result has one entry per input,
/// in order. A path exiftool cannot read yields an empty map.
pub fn read_fields(bin: &Path, paths: &[PathBuf]) -> Result<Vec<FieldValues>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut files: Vec<&Path> = Vec::new();
    let sidecars: Vec<Option<PathBuf>> = paths
        .iter()
        .map(|p| (is_raw(p) && sidecar(p).is_file()).then(|| sidecar(p)))
        .collect();
    for (path, side) in paths.iter().zip(&sidecars) {
        files.push(path);
        if let Some(side) = side {
            files.push(side);
        }
    }
    let mut cmd = Command::new(bin);
    cmd.args([
        "-j",
        "-G1",
        "-a",
        "-charset",
        "iptc=UTF8",
        "-charset",
        "filename=UTF8",
    ]);
    for spec in FIELDS {
        for tag in spec.tags() {
            cmd.arg(format!("-{tag}"));
        }
    }
    cmd.arg("--");
    cmd.args(&files);
    let out = cmd
        .output()
        .map_err(|e| Error::Unavailable(format!("{}: {e}", bin.display())))?;
    // exiftool exits 1 when some file is unreadable but still prints the rest.
    let rows: Vec<Value> = if out.stdout.iter().all(u8::is_ascii_whitespace) {
        Vec::new()
    } else {
        serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::ExifTool(format!("unreadable exiftool output: {e}")))?
    };
    let mut by_file: BTreeMap<String, &Value> = BTreeMap::new();
    for row in &rows {
        if let Some(src) = row.get("SourceFile").and_then(Value::as_str) {
            by_file.insert(src.to_owned(), row);
        }
    }
    let lookup = |p: &Path| by_file.get(p.to_string_lossy().as_ref()).copied();
    Ok(paths
        .iter()
        .zip(&sidecars)
        .map(|(path, side)| {
            let mut values = lookup(path).map(values_of).unwrap_or_default();
            if let Some(over) = side.as_deref().and_then(lookup) {
                values.extend(values_of(over));
            }
            values
        })
        .collect())
}

fn values_of(row: &Value) -> FieldValues {
    let mut out = FieldValues::new();
    for spec in FIELDS {
        let found = spec
            .tags()
            .find_map(|tag| row.get(tag).and_then(text_of).filter(|t| !t.is_empty()));
        if let Some(text) = found {
            out.insert(spec.id, normalize_read(spec, &text));
        }
    }
    out
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(text_of)
                .collect::<Vec<_>>()
                .join("; "),
        ),
        _ => None,
    }
}

/// Merge per-photo values for the sidebar: a value when every photo agrees
/// (a missing field counts as empty), `None` when they differ.
pub fn merge_values(all: &[FieldValues]) -> BTreeMap<&'static str, Merged> {
    let mut out = BTreeMap::new();
    for spec in FIELDS {
        let mut texts = all
            .iter()
            .map(|v| v.get(spec.id).map(String::as_str).unwrap_or(""));
        let Some(first) = texts.next() else { continue };
        let merged = if texts.all(|t| t == first) {
            Merged::Same(first.to_owned())
        } else {
            Merged::Mixed
        };
        out.insert(spec.id, merged);
    }
    out
}

/// One field across the selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merged {
    Same(String),
    Mixed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&'static str, &str)]) -> FieldValues {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn merge_shows_agreement_and_mixed() {
        let a = vals(&[("headline", "Same"), ("credit", "A")]);
        let b = vals(&[("headline", "Same"), ("credit", "B"), ("source", "S")]);
        let m = merge_values(&[a, b]);
        assert_eq!(m["headline"], Merged::Same("Same".into()));
        assert_eq!(m["credit"], Merged::Mixed);
        assert_eq!(m["source"], Merged::Mixed, "present on one photo only");
        assert_eq!(m["title"], Merged::Same(String::new()));
    }

    #[test]
    fn single_photo_is_never_mixed() {
        let m = merge_values(&[vals(&[("credit", "A")])]);
        assert_eq!(m["credit"], Merged::Same("A".into()));
    }

    #[test]
    fn json_values_become_text() {
        let row = serde_json::json!({
            "XMP-dc:Subject": ["a", "b"],
            "IPTC:Urgency": 5,
            "XMP-xmpRights:Marked": "True",
            "XMP-photoshop:DateCreated": "2024:01:02 10:00:00",
            "IPTC:Caption-Abstract": "x"
        });
        let v = values_of(&row);
        assert_eq!(v["keywords"], "a; b");
        assert_eq!(v["urgency"], "5");
        assert_eq!(v["copyright_status"], "True");
        assert_eq!(v["date_created"], "2024-01-02");
        assert_eq!(v["caption"], "x", "IIM twin is the fallback");
    }
}
