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

use serde::{Deserialize, Serialize};

/// Phase-1 IPTC field set (plan section 2.7): caption, keywords, headline,
/// byline and title, credit, source, copyright, instructions, edit status,
/// rights. `None` means the field is not part of the template.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateFields {
    pub caption: Option<String>,
    /// `;`-separated keywords, expanded as one string, split on write.
    pub keywords: Option<String>,
    pub headline: Option<String>,
    pub byline: Option<String>,
    pub byline_title: Option<String>,
    pub credit: Option<String>,
    pub source: Option<String>,
    pub copyright: Option<String>,
    pub instructions: Option<String>,
    /// IIM-only field (no XMP counterpart), e.g. `New`, `Normal`.
    pub edit_status: Option<String>,
    /// XMP-xmpRights only (IIM has no usage-terms slot).
    pub rights: Option<String>,
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
        "yyyy" => vars.civil().map(|c| format!("{:04}", c.year)),
        "yy" => vars.civil().map(|c| format!("{:02}", c.year % 100)),
        "mm" => vars.civil().map(|c| format!("{:02}", c.month)),
        "dd" => vars.civil().map(|c| format!("{:02}", c.day)),
        "hh" => vars.civil().map(|c| format!("{:02}", c.hour)),
        "mi" => vars.civil().map(|c| format!("{:02}", c.min)),
        "ss" => vars.civil().map(|c| format!("{:02}", c.sec)),
        "monthname" => vars
            .civil()
            .map(|c| MONTH_NAMES[(c.month - 1) as usize].to_string()),
        "filename" => Some(vars.filename.clone()),
        "base" => Some(vars.base_name.clone()),
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

impl TemplateFields {
    /// `TAG=value` pairs for the resident exiftool writer, dual XMP + IIM
    /// per field (tag names verified against exiftool 13.x `-listx` and a
    /// JPEG round-trip). List fields emit one repeat per value. A field
    /// that expands empty is skipped, so an unavailable variable cannot
    /// wipe existing metadata.
    pub fn to_tags(&self, vars: &TemplateVars) -> Vec<(String, String)> {
        let mut tags: Vec<(String, String)> = Vec::new();
        let pairs: [(&Option<String>, &[&str]); 10] = [
            (
                &self.caption,
                &["XMP-dc:Description", "IPTC:Caption-Abstract"],
            ),
            (&self.headline, &["XMP-photoshop:Headline", "IPTC:Headline"]),
            (&self.byline, &["XMP-dc:Creator", "IPTC:By-line"]),
            (
                &self.byline_title,
                &["XMP-photoshop:AuthorsPosition", "IPTC:By-lineTitle"],
            ),
            (&self.credit, &["XMP-photoshop:Credit", "IPTC:Credit"]),
            (&self.source, &["XMP-photoshop:Source", "IPTC:Source"]),
            (&self.copyright, &["XMP-dc:Rights", "IPTC:CopyrightNotice"]),
            (
                &self.instructions,
                &["XMP-photoshop:Instructions", "IPTC:SpecialInstructions"],
            ),
            (&self.edit_status, &["IPTC:EditStatus"]),
            (&self.rights, &["XMP-xmpRights:UsageTerms"]),
        ];
        for (raw, names) in pairs {
            let Some(text) = raw else { continue };
            let value = expand(text, vars).trim().to_owned();
            if value.is_empty() {
                continue;
            }
            for name in names {
                tags.push(((*name).to_owned(), value.clone()));
            }
        }
        for keyword in self.keywords_list(vars) {
            tags.push(("XMP-dc:Subject".into(), keyword.clone()));
            tags.push(("IPTC:Keywords".into(), keyword));
        }
        tags
    }

    /// Every field in the template with its expansion for one photo, in
    /// form order, for the editor's preview. An empty value is a field
    /// the write would skip.
    pub fn preview(&self, vars: &TemplateVars) -> Vec<(&'static str, String)> {
        let single = [
            ("Caption", &self.caption),
            ("Headline", &self.headline),
            ("Byline", &self.byline),
            ("Byline title", &self.byline_title),
            ("Credit", &self.credit),
            ("Source", &self.source),
            ("Copyright", &self.copyright),
            ("Instructions", &self.instructions),
            ("Edit status", &self.edit_status),
            ("Rights", &self.rights),
        ];
        let mut lines = Vec::new();
        if self.keywords.is_some() {
            lines.push(("Keywords", self.keywords_list(vars).join(" | ")));
        }
        for (label, raw) in single {
            if let Some(text) = raw {
                lines.push((label, expand(text, vars).trim().to_owned()));
            }
        }
        lines
    }

    /// XMP-only half of [`Self::to_tags`], for `.xmp` sidecar targets
    /// (raw files are stamped in a sidecar per the metadata-storage
    /// decision: XMP sidecar always for raws). IIM tags have no meaning
    /// in an XMP packet and are dropped; the IIM-only `EditStatus` and
    /// the IIM-only dialect pairs lose their IIM copy and keep the XMP
    /// side (or nothing, for `edit_status`).
    pub fn to_xmp_tags(&self, vars: &TemplateVars) -> Vec<(String, String)> {
        self.to_tags(vars)
            .into_iter()
            .filter(|(name, _)| name.starts_with("XMP-"))
            .collect()
    }

    fn keywords_list(&self, vars: &TemplateVars) -> Vec<String> {
        let Some(raw) = &self.keywords else {
            return vec![];
        };
        let expanded = expand(raw, vars);
        // Semicolons only: a comma is ordinary text (`Smith, John`), so
        // splitting on it would chop names apart.
        expanded
            .split(';')
            .map(str::trim)
            .filter(|kw| !kw.is_empty())
            .map(str::to_owned)
            .collect()
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
    fn to_tags_dual_writes_each_field() {
        let f = TemplateFields {
            caption: Some("{date} run".into()),
            keywords: Some("ice; {session}, water".into()),
            byline: Some("Anna Foto".into()),
            byline_title: Some("staff photographer".into()),
            credit: Some("SHL".into()),
            source: Some("Agency".into()),
            copyright: Some("(c) {yyyy} SHL".into()),
            edit_status: Some("New".into()),
            rights: Some("free use".into()),
            ..Default::default()
        };
        assert_eq!(
            f.to_tags(&vars()),
            vec![
                ("XMP-dc:Description".into(), "2026-10-07 run".into()),
                ("IPTC:Caption-Abstract".into(), "2026-10-07 run".into()),
                ("XMP-dc:Creator".into(), "Anna Foto".into()),
                ("IPTC:By-line".into(), "Anna Foto".into()),
                (
                    "XMP-photoshop:AuthorsPosition".into(),
                    "staff photographer".into()
                ),
                ("IPTC:By-lineTitle".into(), "staff photographer".into()),
                ("XMP-photoshop:Credit".into(), "SHL".into()),
                ("IPTC:Credit".into(), "SHL".into()),
                ("XMP-photoshop:Source".into(), "Agency".into()),
                ("IPTC:Source".into(), "Agency".into()),
                ("XMP-dc:Rights".into(), "(c) 2026 SHL".into()),
                ("IPTC:CopyrightNotice".into(), "(c) 2026 SHL".into()),
                ("IPTC:EditStatus".into(), "New".into()),
                ("XMP-xmpRights:UsageTerms".into(), "free use".into()),
                ("XMP-dc:Subject".into(), "ice".into()),
                ("IPTC:Keywords".into(), "ice".into()),
                ("XMP-dc:Subject".into(), "2026-10-07 run, water".into()),
                ("IPTC:Keywords".into(), "2026-10-07 run, water".into()),
            ]
        );
    }

    #[test]
    fn empty_expansions_are_skipped() {
        let f = TemplateFields {
            caption: Some("{camera}".into()),
            byline: Some(" ".into()),
            keywords: Some(" ; ".into()),
            ..Default::default()
        };
        let no_cam = TemplateVars {
            camera: None,
            ..vars()
        };
        assert!(f.to_tags(&no_cam).is_empty());
    }

    #[test]
    fn commas_are_ordinary_keyword_text() {
        let f = TemplateFields {
            keywords: Some("Smith, John; ice".into()),
            ..Default::default()
        };
        let pairs: Vec<String> = f.keywords_list(&vars()).into_iter().collect();
        assert_eq!(pairs, vec!["Smith, John", "ice"]);
    }

    #[test]
    fn preview_lists_every_set_field_expanded() {
        let f = TemplateFields {
            caption: Some("{base} #{seq}".into()),
            keywords: Some("ice; Smith, John".into()),
            credit: Some("{camera}".into()),
            ..Default::default()
        };
        let no_cam = TemplateVars {
            camera: None,
            ..vars()
        };
        assert_eq!(
            f.preview(&no_cam),
            vec![
                ("Keywords", "ice | Smith, John".to_owned()),
                ("Caption", "AHQ_0701 #07".to_owned()),
                ("Credit", String::new()),
            ]
        );
        assert!(TemplateFields::default().preview(&vars()).is_empty());
    }

    #[test]
    fn xmp_only_tags_drop_the_iim_dialect() {
        let f = TemplateFields {
            caption: Some("cap".into()),
            credit: Some("SHL".into()),
            edit_status: Some("New".into()),
            keywords: Some("ice".into()),
            ..Default::default()
        };
        assert_eq!(
            f.to_xmp_tags(&vars()),
            vec![
                ("XMP-dc:Description".into(), "cap".into()),
                ("XMP-photoshop:Credit".into(), "SHL".into()),
                ("XMP-dc:Subject".into(), "ice".into()),
            ]
        );
    }

    #[test]
    fn fields_json_round_trips() {
        let f = TemplateFields {
            credit: Some("SHL".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<TemplateFields>(&json).unwrap(), f);
        assert_eq!(
            serde_json::from_str::<TemplateFields>("{}").unwrap(),
            TemplateFields::default()
        );
    }
}
