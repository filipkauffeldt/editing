//! IPTC template model: phase-1 fields, variables, exiftool tag mapping.
//!
//! A template is a set of field values that may contain `{…}` variables.
//! At apply time each field expands per photo from its capture metadata
//! (plan sections 2.7 and 1.3; PM-style subset per the PLAN decision
//! table). The result is written through the resident exiftool writer as
//! a dual XMP + IIM write, the mapping Photo Mechanic does (research
//! `presets-metadata-ftp.md` section 2).
//!
//! The variable set for phase 1: `{date}`, `{time}`, `{yyyy}` `{yy}`
//! `{mm}` `{dd}`, `{hh}` `{mi}` `{ss}`, `{monthname}`, `{filename}`,
//! `{base}`, `{ext}`, `{session}`, `{seq}` / `{seq:N}` (batch sequence,
//! zero-padded), `{camera}`, `{lens}`, `{iso}`, `{aperture}`, `{shutter}`,
//! `{focal}`. Unknown tokens stay verbatim (what PM does with not-yet
//! entered replacements, so late variables never destroy typed text). A
//! field whose variables expand to nothing is skipped instead of wiping
//! what is already on the photo.
//!
//! [`expand_with_codes`] layers Photo Mechanic code replacements and hot
//! codes (see `code_replacements`) on top of the variables, so a field can
//! mix `{…}`, `=code=` and `\hot\` / `[hot]` invocations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::code_replacements::CodeReplacements;
use crate::fields::FIELDS;

/// Field values of a template or a form write, keyed by the ids of the
/// field table (`crate::fields::FIELDS`). A missing key means the field is
/// not part of the set. The keys of the first template version (`caption`,
/// `keywords`, `byline`, `copyright`, `rights`, ...) are ids in the table,
/// so stored templates keep loading; null and unknown keys are dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateFields(BTreeMap<String, String>);

impl TemplateFields {
    /// Builder: set one field.
    pub fn with(mut self, id: &str, value: impl Into<String>) -> Self {
        self.set(id, Some(value.into()));
        self
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.0.get(id).map(String::as_str)
    }

    /// Set or remove a field. Unknown ids are ignored.
    pub fn set(&mut self, id: &str, value: Option<String>) {
        if crate::fields::field(id).is_none() {
            return;
        }
        match value {
            Some(v) => self.0.insert(id.to_owned(), v),
            None => self.0.remove(id),
        };
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for TemplateFields {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TemplateFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, Option<String>>::deserialize(deserializer)?;
        let mut fields = TemplateFields::default();
        for (id, value) in raw {
            fields.set(&id, value);
        }
        Ok(fields)
    }
}

/// Per-photo values the field variables expand from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TemplateVars {
    /// Batch sequence number (1-based), for `{seq}`.
    pub sequence: u64,
    /// `DSC01234.JPG`.
    pub filename: String,
    /// `DSC01234`.
    pub base_name: String,
    /// `JPG` (no dot).
    pub ext: String,
    /// Parent directory's final segment, for `{session}`.
    pub session: String,
    pub taken_at: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub focal: Option<i64>,
}

impl TemplateVars {
    /// Fill a missing `taken_at` from the wall clock, so `{date}` still
    /// produces a sane value for photos without EXIF capture time.
    pub fn from_wall_clock(self, now_unix: i64) -> Self {
        Self {
            taken_at: self.taken_at.or(Some(now_unix)),
            ..self
        }
    }

    fn civil(&self) -> Option<Civil> {
        self.taken_at.map(Civil::from_unix)
    }
}

/// Y/M/D/H/M/S breakdown of a Unix-seconds timestamp (UTC). EXIF
/// datetimes are naive wall clock by spec; UTC is deterministic and fine
/// for captioning here.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
}

impl Civil {
    fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Civil {
            year,
            month,
            day,
            hour: (rem / 3_600) as u32,
            min: ((rem / 60) % 60) as u32,
            sec: (rem % 60) as u32,
        }
    }
}

/// Howard Hinnant's civil_from_days.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Outcome of one token lookup. A recognized token whose value is
/// unavailable this photo resolves to an empty string (never verbatim,
/// so an aggregate of known tokens still flattens correctly). `Unknown`
/// tokens are carried through as typed.
enum Resolved {
    Value(String),
    Unknown,
}

/// Expand `{…}` variables in one template field value.
pub fn expand(text: &str, vars: &TemplateVars) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close_rel) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let token = rest[open + 1..open + close_rel].trim();
        let (name, spec) = token
            .split_once(':')
            .map(|(n, s)| (n.trim(), s.trim()))
            .unwrap_or((token, ""));
        match resolve(name, spec, vars) {
            Resolved::Value(value) => out.push_str(&value),
            Resolved::Unknown => out.push_str(&rest[open..open + close_rel + 1]),
        }
        rest = &rest[open + close_rel + 1..];
    }
    out.push_str(rest);
    out
}

/// Expand `{…}` variables plus, when `codes` is a non-empty replacement
/// set, Photo Mechanic code replacements and hot codes. The codes pass
/// owns the whole expansion because it evaluates variables inside a code
/// before lookup; without codes this is exactly [`expand`].
pub fn expand_with_codes(
    text: &str,
    vars: &TemplateVars,
    codes: Option<&CodeReplacements>,
) -> String {
    match codes {
        Some(codes) if !codes.is_empty() => codes.expand(text, vars),
        _ => expand(text, vars),
    }
}

fn resolve(name: &str, spec: &str, vars: &TemplateVars) -> Resolved {
    // Each arm maps to `Some(value)`, `None` = recognized but unavailable
    // this photo, or `Unknown` for a token outside the set.
    let value = match name {
        "date" => vars
            .civil()
            .map(|c| format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)),
        "time" => vars
            .civil()
            .map(|c| format!("{:02}-{:02}-{:02}", c.hour, c.min, c.sec)),
        // `year4` / `day` are Photo Mechanic's spellings of `yyyy` / `dd`,
        // used by hot-code generators and macro rows.
        "yyyy" | "year4" => vars.civil().map(|c| format!("{:04}", c.year)),
        "yy" => vars.civil().map(|c| format!("{:02}", c.year % 100)),
        "mm" => vars.civil().map(|c| format!("{:02}", c.month)),
        "dd" | "day" => vars.civil().map(|c| format!("{:02}", c.day)),
        "hh" => vars.civil().map(|c| format!("{:02}", c.hour)),
        "mi" => vars.civil().map(|c| format!("{:02}", c.min)),
        "ss" => vars.civil().map(|c| format!("{:02}", c.sec)),
        "monthname" => vars
            .civil()
            .map(|c| MONTH_NAMES[(c.month - 1) as usize].to_string()),
        // `fbas` is Photo Mechanic's shorthand for `filenamebase`; the
        // `:start,len` spec is a substring (hot-code generators use it to
        // pick the photographer initials out of the file name).
        "filename" => Some(substring(&vars.filename, spec)),
        "base" | "fbas" | "filenamebase" => Some(substring(&vars.base_name, spec)),
        "ext" => Some(vars.ext.clone()),
        "session" => Some(vars.session.clone()),
        "seq" | "iseq" => {
            let width: usize = spec.parse().unwrap_or(2);
            Some(format!(
                "{:0width$}",
                vars.sequence,
                width = width.clamp(1, 16)
            ))
        }
        "camera" => vars.camera.clone(),
        "lens" => vars.lens.clone(),
        "iso" => vars.iso.map(|v| v.to_string()),
        "aperture" => vars.aperture.map(trim_f64),
        "shutter" => vars.shutter.clone(),
        "focal" => vars.focal.map(|v| v.to_string()),
        _ => return Resolved::Unknown,
    };
    Resolved::Value(value.unwrap_or_default())
}

fn trim_f64(v: f64) -> String {
    let s = format!("{v:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned();
    if s.is_empty() { "0".to_string() } else { s }
}

/// `{value:start,len}` substring extraction (`chars`, so non-ASCII file
/// names stay intact). No spec, or a malformed one, returns the value.
fn substring(value: &str, spec: &str) -> String {
    if spec.is_empty() {
        return value.to_string();
    }
    let Some((start, len)) = spec.split_once(',') else {
        return value.to_string();
    };
    let (Ok(start), Ok(len)) = (start.trim().parse::<usize>(), len.trim().parse::<usize>()) else {
        return value.to_string();
    };
    value.chars().skip(start).take(len).collect()
}

impl TemplateFields {
    /// `TAG=value` pairs for the resident exiftool writer, dual XMP + IIM
    /// per field (tag names from the field table). List fields emit one
    /// repeat per value. A field that expands empty is skipped, so an
    /// unavailable variable cannot wipe existing metadata.
    pub fn to_tags(&self, vars: &TemplateVars) -> Vec<(String, String)> {
        self.to_tags_with(vars, None)
    }

    /// [`Self::to_tags`] with code replacements and hot codes expanded.
    pub fn to_tags_with(
        &self,
        vars: &TemplateVars,
        codes: Option<&CodeReplacements>,
    ) -> Vec<(String, String)> {
        self.to_tags_opts(vars, codes, false)
    }

    /// [`Self::to_tags_with`]; with `clear_empty` a field that expands
    /// empty clears its tags (a form write that blanked the field) instead
    /// of being skipped.
    pub fn to_tags_opts(
        &self,
        vars: &TemplateVars,
        codes: Option<&CodeReplacements>,
        clear_empty: bool,
    ) -> Vec<(String, String)> {
        let mut tags: Vec<(String, String)> = Vec::new();
        for spec in FIELDS {
            let Some(raw) = self.get(spec.id) else {
                continue;
            };
            let values = field_values(spec.is_list(), raw, vars, codes);
            if values.is_empty() {
                if clear_empty {
                    tags.extend(spec.tags().map(|name| (name.to_owned(), String::new())));
                }
                continue;
            }
            for value in values {
                for name in spec.tags() {
                    tags.push((name.to_owned(), value.clone()));
                }
            }
        }
        tags
    }

    /// Every field in the set with its expansion for one photo, in table
    /// order, for the editor's preview. An empty value is a field the
    /// write would skip.
    pub fn preview(&self, vars: &TemplateVars) -> Vec<(&'static str, String)> {
        self.preview_with(vars, None)
    }

    /// [`Self::preview`] with code replacements and hot codes expanded.
    pub fn preview_with(
        &self,
        vars: &TemplateVars,
        codes: Option<&CodeReplacements>,
    ) -> Vec<(&'static str, String)> {
        FIELDS
            .iter()
            .filter_map(|spec| {
                let raw = self.get(spec.id)?;
                let values = field_values(spec.is_list(), raw, vars, codes);
                Some((spec.label, values.join(" | ")))
            })
            .collect()
    }

    /// XMP-only half of [`Self::to_tags`], for `.xmp` sidecar targets
    /// (raw files are stamped in a sidecar per the metadata-storage
    /// decision: XMP sidecar always for raws). IIM tags have no meaning
    /// in an XMP packet and are dropped, so IIM-only fields write nothing.
    pub fn to_xmp_tags(&self, vars: &TemplateVars) -> Vec<(String, String)> {
        self.to_xmp_tags_with(vars, None)
    }

    /// [`Self::to_xmp_tags`] with code replacements and hot codes expanded.
    pub fn to_xmp_tags_with(
        &self,
        vars: &TemplateVars,
        codes: Option<&CodeReplacements>,
    ) -> Vec<(String, String)> {
        xmp_only(self.to_tags_with(vars, codes))
    }
}

/// Keep the XMP tags of a tag list.
pub fn xmp_only(tags: Vec<(String, String)>) -> Vec<(String, String)> {
    tags.into_iter()
        .filter(|(name, _)| name.starts_with("XMP-"))
        .collect()
}

/// Expanded values of one field: one for a plain field, one per `;` part
/// for a list. Semicolons only: a comma is ordinary text (`Smith, John`).
fn field_values(
    list: bool,
    raw: &str,
    vars: &TemplateVars,
    codes: Option<&CodeReplacements>,
) -> Vec<String> {
    let expanded = expand_with_codes(raw, vars, codes);
    if list {
        expanded
            .split(';')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        let value = expanded.trim();
        if value.is_empty() {
            vec![]
        } else {
            vec![value.to_owned()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        TemplateVars {
            sequence: 7,
            filename: "AHQ_0701.JPG".into(),
            base_name: "AHQ_0701".into(),
            ext: "JPG".into(),
            session: "2026-10-07 run".into(),
            taken_at: Some(1_791_374_400), // 2026-10-07 12:00:00 UTC
            camera: Some("EOS R6".into()),
            lens: Some("RF24-105mm".into()),
            iso: Some(6400),
            aperture: Some(2.8),
            shutter: Some("1/250".into()),
            focal: Some(24),
        }
    }

    #[test]
    fn date_tokens_expand_utc() {
        let v = vars();
        assert_eq!(expand("{date}", &v), "2026-10-07");
        assert_eq!(expand("{time}", &v), "12-00-00");
        assert_eq!(
            expand("{yyyy}/{mm}/{dd} {hh}:{mi}:{ss}", &v),
            "2026/10/07 12:00:00"
        );
        assert_eq!(expand("{yy}.{mm}.{monthname}", &v), "26.10.Oct");
        assert_eq!(expand("{ly}", &v), "{ly}", "unknown stays verbatim");
    }

    #[test]
    fn file_tokens_expand() {
        let v = vars();
        assert_eq!(expand("{filename}", &v), "AHQ_0701.JPG");
        assert_eq!(expand("{base}.{ext}", &v), "AHQ_0701.JPG");
        assert_eq!(expand("{session}", &v), "2026-10-07 run");
    }

    #[test]
    fn seq_pads_to_spec_digit_count() {
        let v = vars();
        assert_eq!(expand("{seq}", &v), "07");
        assert_eq!(expand("{seq:4}", &v), "0007");
        assert_eq!(expand("{iseq:1}", &v), "7");
    }

    #[test]
    fn camera_tokens_expand_and_miss_to_empty() {
        let v = vars();
        assert_eq!(expand("{camera} {lens}", &v), "EOS R6 RF24-105mm");
        assert_eq!(
            expand("ISO {iso} f{aperture} {shutter} {focal}mm", &v),
            "ISO 6400 f2.8 1/250 24mm"
        );
        let no_cam = TemplateVars {
            camera: None,
            ..vars()
        };
        assert_eq!(expand("{camera}", &no_cam), "");
    }

    #[test]
    fn unknown_tokens_stay() {
        let v = vars();
        assert_eq!(expand("{hockey}:{seq}", &v), "{hockey}:07");
        assert_eq!(expand("{date", &v), "{date");
    }

    #[test]
    fn wall_clock_fills_missing_taken_at() {
        let v = TemplateVars::default().from_wall_clock(1_791_374_400);
        assert_eq!(expand("{date}", &v), "2026-10-07");
    }

    #[test]
    fn substring_variables_slice_the_file_name() {
        let v = vars();
        assert_eq!(expand("{fbas:0,3}", &v), "AHQ");
        assert_eq!(expand("{fbas:1,3}", &v), "HQ_");
        assert_eq!(expand("{filenamebase}", &v), "AHQ_0701");
        assert_eq!(expand("{day}/{year4}", &v), "07/2026");
    }

    #[test]
    fn code_replacements_expand_in_tags_and_preview() {
        use crate::code_replacements::CodeReplacements;

        let codes = CodeReplacements::parse("h1\tSHL\n");
        let f = TemplateFields::default()
            .with("caption", "\\h1\\ {date}")
            .with("credit", "=h1=");
        assert_eq!(
            f.to_tags_with(&vars(), Some(&codes)),
            vec![
                ("XMP-dc:Description".into(), "SHL 2026-10-07".into()),
                ("IPTC:Caption-Abstract".into(), "SHL 2026-10-07".into()),
                ("XMP-photoshop:Credit".into(), "SHL".into()),
                ("IPTC:Credit".into(), "SHL".into()),
            ]
        );
        assert_eq!(
            f.preview_with(&vars(), Some(&codes)),
            vec![
                ("Description", "SHL 2026-10-07".to_owned()),
                ("Credit line", "SHL".to_owned()),
            ]
        );
    }

    #[test]
    fn to_tags_dual_writes_each_field() {
        let f = TemplateFields::default()
            .with("caption", "{date} run")
            .with("keywords", "ice; {session}, water")
            .with("byline", "Anna Foto")
            .with("edit_status", "New")
            .with("rights", "free use")
            .with("contact_email", "a@b.c");
        assert_eq!(
            f.to_tags(&vars()),
            vec![
                ("XMP-dc:Description".into(), "2026-10-07 run".into()),
                ("IPTC:Caption-Abstract".into(), "2026-10-07 run".into()),
                ("XMP-dc:Subject".into(), "ice".into()),
                ("IPTC:Keywords".into(), "ice".into()),
                ("XMP-dc:Subject".into(), "2026-10-07 run, water".into()),
                ("IPTC:Keywords".into(), "2026-10-07 run, water".into()),
                ("IPTC:EditStatus".into(), "New".into()),
                ("XMP-dc:Creator".into(), "Anna Foto".into()),
                ("IPTC:By-line".into(), "Anna Foto".into()),
                ("XMP-xmpRights:UsageTerms".into(), "free use".into()),
                ("XMP-iptcCore:CreatorWorkEmail".into(), "a@b.c".into()),
            ]
        );
    }

    #[test]
    fn empty_expansions_are_skipped_or_cleared() {
        let f = TemplateFields::default()
            .with("caption", "{camera}")
            .with("byline", " ")
            .with("keywords", " ; ");
        let no_cam = TemplateVars {
            camera: None,
            ..vars()
        };
        assert!(f.to_tags(&no_cam).is_empty());
        let cleared = f.to_tags_opts(&no_cam, None, true);
        assert!(cleared.contains(&("XMP-dc:Description".into(), String::new())));
        assert!(cleared.contains(&("IPTC:Keywords".into(), String::new())));
        assert_eq!(cleared.len(), 6);
    }

    #[test]
    fn commas_are_ordinary_keyword_text() {
        let f = TemplateFields::default().with("keywords", "Smith, John; ice");
        let tags = f.to_tags(&vars());
        let subjects: Vec<&str> = tags
            .iter()
            .filter(|(n, _)| n == "XMP-dc:Subject")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(subjects, vec!["Smith, John", "ice"]);
    }

    #[test]
    fn preview_lists_every_set_field_expanded() {
        let f = TemplateFields::default()
            .with("caption", "{base} #{seq}")
            .with("keywords", "ice; Smith, John")
            .with("credit", "{camera}");
        let no_cam = TemplateVars {
            camera: None,
            ..vars()
        };
        assert_eq!(
            f.preview(&no_cam),
            vec![
                ("Description", "AHQ_0701 #07".to_owned()),
                ("Keywords", "ice | Smith, John".to_owned()),
                ("Credit line", String::new()),
            ]
        );
        assert!(TemplateFields::default().preview(&vars()).is_empty());
    }

    #[test]
    fn xmp_only_tags_drop_the_iim_dialect() {
        let f = TemplateFields::default()
            .with("caption", "cap")
            .with("credit", "SHL")
            .with("edit_status", "New")
            .with("keywords", "ice");
        assert_eq!(
            f.to_xmp_tags(&vars()),
            vec![
                ("XMP-dc:Description".into(), "cap".into()),
                ("XMP-dc:Subject".into(), "ice".into()),
                ("XMP-photoshop:Credit".into(), "SHL".into()),
            ]
        );
    }

    #[test]
    fn fields_json_round_trips() {
        let f = TemplateFields::default().with("credit", "SHL");
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<TemplateFields>(&json).unwrap(), f);
        assert_eq!(
            serde_json::from_str::<TemplateFields>("{}").unwrap(),
            TemplateFields::default()
        );
    }

    #[test]
    fn stored_version_one_templates_still_load() {
        let old = r#"{"caption":"c","keywords":null,"headline":null,"byline":"B",
            "byline_title":null,"credit":"SHL","source":null,"copyright":"(c)",
            "instructions":null,"edit_status":"New","rights":"free","gone":"x"}"#;
        let f: TemplateFields = serde_json::from_str(old).unwrap();
        assert_eq!(f.get("caption"), Some("c"));
        assert_eq!(f.get("byline"), Some("B"));
        assert_eq!(f.get("copyright"), Some("(c)"));
        assert_eq!(f.get("rights"), Some("free"));
        assert_eq!(f.get("keywords"), None);
        assert_eq!(f.get("gone"), None);
    }
}
