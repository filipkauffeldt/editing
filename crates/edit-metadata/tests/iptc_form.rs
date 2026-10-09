//! The IPTC form against real exiftool: every table tag is accepted, every
//! select option survives a write and read, and form writes (JPEG in
//! place, raw through its sidecar) round trip non-ASCII and multi-line text
//! with the IIM and XMP twins.

use std::path::{Path, PathBuf};
use std::process::Command;

use edit_catalog::{ClaimedJob, JobId, JobKind};
use edit_metadata::{
    FIELDS, Kind, MetadataWritePayload, TemplateFields, Writer, merge_values, read_fields,
    run_metadata_write,
};

fn bin() -> Option<PathBuf> {
    Writer::binary_for_tests()
}

fn jpeg(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/photo.jpg");
    std::fs::copy(fixture, &path).unwrap();
    path
}

fn write(writer: &Writer, path: &Path, fields: &TemplateFields, clear_empty: bool) {
    try_write(writer, path, fields, clear_empty).unwrap();
}

fn try_write(
    writer: &Writer,
    path: &Path,
    fields: &TemplateFields,
    clear_empty: bool,
) -> Result<(), String> {
    let payload = MetadataWritePayload {
        item_id: 1,
        paths: vec![path.to_string_lossy().into_owned()],
        sequence: 1,
        captured_at: 1_791_374_400,
        session: None,
        fields_json: serde_json::to_string(fields).unwrap(),
        replacements: None,
        clear_empty,
    };
    let job = ClaimedJob {
        id: JobId(1),
        kind: JobKind::MetadataWrite,
        payload_json: serde_json::to_string(&payload).unwrap(),
        attempts: 1,
    };
    let (tx, _rx) = flume::unbounded();
    run_metadata_write(&job, writer, &tx)
}

fn exif(bin: &Path, file: &Path, tag: &str) -> Vec<String> {
    let out = Command::new(bin)
        .args(["-b", "-a", "-sep", "\n"])
        .arg(format!("-{tag}"))
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_table_tag_is_accepted_by_exiftool() {
    let Some(bin) = bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let file = jpeg(dir.path(), "t.jpg");
    for spec in FIELDS {
        for tag in spec.tags() {
            let out = Command::new(&bin)
                .args(["-overwrite_original", "-charset", "iptc=UTF8"])
                .arg(format!(
                    "-{tag}={}",
                    match spec.kind {
                        Kind::Date => "2024-05-06",
                        Kind::CountryCode => "SWE",
                        Kind::Select(options) => options[1].1,
                        _ => "1",
                    }
                ))
                .arg(&file)
                .output()
                .unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !text.contains("Warning") && !text.contains("Error"),
                "{} / {tag}: {text}",
                spec.id
            );
        }
    }
}

#[test]
fn select_options_survive_a_round_trip() {
    let Some(bin) = bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::resident(&bin).unwrap();
    let file = jpeg(dir.path(), "sel.jpg");
    for spec in FIELDS {
        let Kind::Select(options) = spec.kind else {
            continue;
        };
        for (label, value) in options.iter().filter(|(_, v)| !v.is_empty()) {
            let fields = TemplateFields::default().with(spec.id, *value);
            try_write(&writer, &file, &fields, true)
                .unwrap_or_else(|e| panic!("{} option {label}: {e}", spec.id));
            let read = read_fields(&bin, std::slice::from_ref(&file)).unwrap();
            let got = read[0].get(spec.id).map(String::as_str);
            assert_eq!(got, Some(*value), "{} option {label}", spec.id);
        }
    }
}

#[test]
fn jpeg_round_trip_keeps_twins_unicode_and_lines() {
    let Some(bin) = bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::resident(&bin).unwrap();
    let file = jpeg(dir.path(), "a.jpg");
    let caption = "Línea uno — 中文\nSecond line";
    let fields = TemplateFields::default()
        .with("caption", caption)
        .with("headline", "Överraskning")
        .with("keywords", "ice; Smith, John; 水")
        .with("byline", "Åsa Foto")
        .with("urgency", "3")
        .with("date_created", "2024-05-06")
        .with("contact_city", "Malmö")
        .with("person_shown", "Ann; Bo");
    write(&writer, &file, &fields, true);

    let read = read_fields(&bin, std::slice::from_ref(&file))
        .unwrap()
        .remove(0);
    assert_eq!(read["caption"], caption);
    assert_eq!(read["headline"], "Överraskning");
    assert_eq!(read["keywords"], "ice; Smith, John; 水");
    assert_eq!(read["date_created"], "2024-05-06");
    assert_eq!(read["contact_city"], "Malmö");
    assert_eq!(read["person_shown"], "Ann; Bo");

    assert_eq!(
        exif(&bin, &file, "XMP-photoshop:Headline"),
        ["Överraskning"]
    );
    assert_eq!(exif(&bin, &file, "IPTC:Headline"), ["Överraskning"]);
    assert_eq!(exif(&bin, &file, "IPTC:By-line"), ["Åsa Foto"]);
    assert_eq!(exif(&bin, &file, "XMP-dc:Creator"), ["Åsa Foto"]);
    assert_eq!(exif(&bin, &file, "IPTC:Urgency"), ["3"]);
    assert_eq!(exif(&bin, &file, "XMP-photoshop:Urgency"), ["3"]);
    assert_eq!(
        exif(&bin, &file, "IPTC:Keywords"),
        ["ice", "Smith, John", "水"]
    );
    assert_eq!(
        exif(&bin, &file, "IPTC:Caption-Abstract").join("\n"),
        caption
    );

    // Blanking a field clears both twins; untouched fields stay.
    let cleared = TemplateFields::default()
        .with("headline", "")
        .with("keywords", "");
    write(&writer, &file, &cleared, true);
    assert!(exif(&bin, &file, "IPTC:Headline").is_empty());
    assert!(exif(&bin, &file, "XMP-photoshop:Headline").is_empty());
    assert!(exif(&bin, &file, "IPTC:Keywords").is_empty());
    let read = read_fields(&bin, &[file]).unwrap().remove(0);
    assert_eq!(read["byline"], "Åsa Foto");
    assert!(!read.contains_key("headline"));
}

#[test]
fn raw_is_stamped_in_its_sidecar_and_read_merged() {
    let Some(bin) = bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::resident(&bin).unwrap();
    // A stand-in raw: never rewritten, so its bytes only need to exist.
    let raw = dir.path().join("DSC0001.NEF");
    std::fs::write(&raw, b"not really a raw").unwrap();
    let fields = TemplateFields::default()
        .with("caption", "Raw — rad\nTvå")
        .with("credit", "Åsa")
        .with("edit_status", "New");
    write(&writer, &raw, &fields, true);

    assert_eq!(std::fs::read(&raw).unwrap(), b"not really a raw");
    let sidecar = dir.path().join("DSC0001.xmp");
    assert!(sidecar.is_file());
    assert_eq!(exif(&bin, &sidecar, "XMP-photoshop:Credit"), ["Åsa"]);

    let read = read_fields(&bin, &[raw]).unwrap().remove(0);
    assert_eq!(read["caption"], "Raw — rad\nTvå");
    assert_eq!(read["credit"], "Åsa");
    assert!(
        !read.contains_key("edit_status"),
        "IIM only: not in a sidecar"
    );
}

#[test]
fn selection_merges_to_mixed_where_photos_differ() {
    let Some(bin) = bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::resident(&bin).unwrap();
    let a = jpeg(dir.path(), "a.jpg");
    let b = jpeg(dir.path(), "b.jpg");
    write(
        &writer,
        &a,
        &TemplateFields::default()
            .with("credit", "X")
            .with("source", "S"),
        true,
    );
    write(
        &writer,
        &b,
        &TemplateFields::default()
            .with("credit", "Y")
            .with("source", "S"),
        true,
    );
    let read = read_fields(&bin, &[a, b]).unwrap();
    let merged = merge_values(&read);
    assert_eq!(merged["credit"], edit_metadata::Merged::Mixed);
    assert_eq!(merged["source"], edit_metadata::Merged::Same("S".into()));
}
