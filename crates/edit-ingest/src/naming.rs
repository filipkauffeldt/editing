//! Folder and filename naming rules with a Photo-Mechanic-style variable
//! subset (plan section 2.8; the C2 variable-set decision).
//!
//! A rule is plain text with `{name}` tokens. Folder rules may contain `/`
//! to nest subfolders; filename rules must not. Variables are resolved from
//! [`NamingContext`] and sanitized so a value can never introduce a path
//! separator of its own.
//!
//! Supported variables:
//!
//! | Token | Value |
//! |---|---|
//! | `{session}` | session name |
//! | `{seq}` / `{seq:N}` | sequence number, zero-padded to `N` digits |
//! | `{filename}` | original file name with extension |
//! | `{filenamebase}` | original file name without extension |
//! | `{ext}` | lower-case extension without the dot |
//! | `{camera}` | camera model, or empty |
//! | `{yyyy}` / `{year4}` | capture year (4 digits) |
//! | `{yy}` | capture year (2 digits) |
//! | `{mm}` / `{month0}` | capture month, zero-padded |
//! | `{dd}` / `{day0}` | capture day, zero-padded |
//! | `{hh}` | capture hour, zero-padded |
//! | `{mi}` | capture minute, zero-padded |
//! | `{ss}` | capture second, zero-padded |
//! | `{datesort}` | capture date `YYYYMMDD` |
//! | `{datesortdash}` | capture date `YYYY-MM-DD` |

use std::path::{Path, PathBuf};

/// Folder rule the ingest dialog starts with (and an empty one falls back to).
pub const DEFAULT_DEST_RULE: &str = "~/Photos/{yyyy}/{mm}-{dd}";

/// Filename rule the ingest dialog starts with (and an empty one falls back
/// to). The original extension is appended at landing, so a RAW+JPEG pair
/// lands as `shoot_0001.NEF` / `shoot_0001.JPG`.
pub const DEFAULT_FILENAME_RULE: &str = "{session}_{seq:4}";

/// A rule that could not be resolved to a name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NamingError {
    /// A `{` token was never closed.
    #[error("unterminated variable token in rule")]
    UnterminatedToken,
    /// An empty `{}` token.
    #[error("empty variable token in rule")]
    EmptyToken,
    /// A variable name the rule engine does not know.
    #[error("unknown variable: {{{0}}}")]
    UnknownVariable(String),
    /// `{seq:N}` with a non-numeric or out-of-range width.
    #[error("invalid width for {{{name}}}: {value}")]
    InvalidWidth { name: String, value: String },
    /// A date variable with no capture time to read.
    #[error("variable {{{0}}} needs a capture time")]
    MissingCaptureTime(String),
    /// The rendered name was empty.
    #[error("rule rendered an empty name")]
    EmptyName,
    /// A filename rule rendered a path separator.
    #[error("filename rule rendered a path separator")]
    PathSeparator,
    /// A rule rendered a `.` or `..` path component.
    #[error("rule rendered a reserved path component: {0}")]
    ReservedComponent(String),
}

/// Values a rule resolves against for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamingContext {
    pub session: String,
    pub original_name: String,
    pub seq: u64,
    /// Capture time in Unix seconds; required by the date variables.
    pub taken_at: Option<i64>,
    pub camera: Option<String>,
}

impl NamingContext {
    /// Original file name without its extension.
    pub fn base_name(&self) -> &str {
        Path::new(&self.original_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(self.original_name.as_str())
    }

    /// Lower-case extension without the dot (empty when there is none).
    pub fn ext(&self) -> String {
        Path::new(&self.original_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }
}

/// Render a rule to a string. Variables are sanitized; literal text passes
/// through unchanged.
pub fn render(template: &str, ctx: &NamingContext) -> Result<String, NamingError> {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return Err(NamingError::UnterminatedToken);
        };
        let token = &after[..end];
        if token.is_empty() {
            return Err(NamingError::EmptyToken);
        }
        out.push_str(&resolve(token, ctx)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Append the original extension to a rendered name unless it is already
/// present. Photo Mechanic and Lightroom keep the extension when renaming,
/// so the dialog's `{session}_{seq:4}` rule lands `shoot_0001.NEF`.
pub fn with_original_extension(name: &str, original_name: &str) -> String {
    let Some(ext) = Path::new(original_name)
        .extension()
        .and_then(|e| e.to_str())
    else {
        return name.to_string();
    };
    if name
        .to_ascii_lowercase()
        .ends_with(&format!(".{}", ext.to_ascii_lowercase()))
    {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    }
}

/// Render a filename rule: resolve, then reject separators, empty names, and
/// reserved components.
pub fn render_filename(template: &str, ctx: &NamingContext) -> Result<String, NamingError> {
    let name = render(template, ctx)?;
    if name.is_empty() {
        return Err(NamingError::EmptyName);
    }
    if name.contains(['/', '\\', '\0']) {
        return Err(NamingError::PathSeparator);
    }
    if name == "." || name == ".." {
        return Err(NamingError::ReservedComponent(name));
    }
    Ok(name)
}

/// Render a folder rule to a path. Literal `/` nests subfolders; variable
/// values never introduce separators, and `..` components are rejected.
pub fn render_dir(template: &str, ctx: &NamingContext) -> Result<PathBuf, NamingError> {
    let rendered = render(template, ctx)?;
    if rendered.is_empty() {
        return Err(NamingError::EmptyName);
    }
    let path = expand_tilde(&rendered);
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(NamingError::ReservedComponent("..".to_string()));
        }
    }
    Ok(path)
}

/// Expand a leading `~` to `$HOME`. A rule with no `~` is returned as-is
/// (lexically, so it never touches the filesystem).
pub fn expand_tilde(path: &str) -> PathBuf {
    expand_tilde_with(path, std::env::var_os("HOME").as_deref())
}

/// `expand_tilde` with an explicit home, so the rule is testable without
/// mutating the process environment.
fn expand_tilde_with(path: &str, home: Option<&std::ffi::OsStr>) -> PathBuf {
    let home = home.map(PathBuf::from);
    if path == "~"
        && let Some(home) = home
    {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// The literal prefix of a folder rule, before its first variable: the
/// stable destination root the contact sheet can open. For a rule with no
/// variables, the whole rule (minus a trailing separator) is the prefix.
pub fn static_prefix(template: &str) -> String {
    match template.find('{') {
        None => template.trim_end_matches(['/', '\\']).to_string(),
        Some(index) => {
            let before = &template[..index];
            match before.rfind(['/', '\\']) {
                Some(sep) => before[..sep].to_string(),
                None => String::new(),
            }
        }
    }
}

/// Validate every token in a rule without a context, so the dialog can
/// reject a bad rule before ingest starts.
pub fn validate(template: &str) -> Result<(), NamingError> {
    let ctx = NamingContext {
        session: String::new(),
        original_name: "x.jpg".to_string(),
        seq: 1,
        taken_at: Some(0),
        camera: None,
    };
    render(template, &ctx).map(|_| ())
}

/// Reject a filename rule that cannot name a file, before any batch exists.
///
/// [`validate`] only checks the tokens; this also renders the rule against a
/// sample file (with every optional value present) and applies the
/// [`render_filename`] checks, so a rule that renders empty (`""`, spaces
/// only) or with a path separator is caught up front rather than failing
/// every file on the card.
pub fn validate_filename_rule(template: &str) -> Result<(), NamingError> {
    let ctx = NamingContext {
        session: "session".to_string(),
        original_name: "IMG_0001.JPG".to_string(),
        seq: 1,
        taken_at: Some(0),
        camera: Some("camera".to_string()),
    };
    render_filename(template, &ctx).map(|_| ())
}

/// Resolve one `{name}` or `{name:width}` token.
fn resolve(token: &str, ctx: &NamingContext) -> Result<String, NamingError> {
    let (name, width) = match token.split_once(':') {
        Some((name, raw)) => {
            let width = raw
                .parse::<usize>()
                .ok()
                .filter(|w| (1..=9).contains(w))
                .ok_or_else(|| NamingError::InvalidWidth {
                    name: name.to_string(),
                    value: raw.to_string(),
                })?;
            (name, Some(width))
        }
        None => (token, None),
    };
    let value = match name {
        "session" => sanitize(&ctx.session),
        "seq" => format_seq(ctx.seq, width.unwrap_or(1)),
        "filename" => sanitize(&ctx.original_name),
        "filenamebase" => sanitize(ctx.base_name()),
        "ext" => sanitize(&ctx.ext()),
        "camera" => sanitize(ctx.camera.as_deref().unwrap_or("")),
        "yyyy" | "year4" => date_part(ctx, name, |d| format!("{:04}", d.year))?,
        "yy" => date_part(ctx, name, |d| format!("{:02}", d.year.rem_euclid(100)))?,
        "mm" | "month0" => date_part(ctx, name, |d| format!("{:02}", d.month))?,
        "dd" | "day0" => date_part(ctx, name, |d| format!("{:02}", d.day))?,
        "hh" => date_part(ctx, name, |d| format!("{:02}", d.hour))?,
        "mi" => date_part(ctx, name, |d| format!("{:02}", d.minute))?,
        "ss" => date_part(ctx, name, |d| format!("{:02}", d.second))?,
        "datesort" => date_part(ctx, name, |d| {
            format!("{:04}{:02}{:02}", d.year, d.month, d.day)
        })?,
        "datesortdash" => date_part(ctx, name, |d| {
            format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
        })?,
        other => return Err(NamingError::UnknownVariable(other.to_string())),
    };
    Ok(value)
}

fn date_part(
    ctx: &NamingContext,
    name: &str,
    format: impl Fn(Civil) -> String,
) -> Result<String, NamingError> {
    let ts = ctx
        .taken_at
        .ok_or_else(|| NamingError::MissingCaptureTime(name.to_string()))?;
    Ok(format(civil_from_unix(ts)))
}

/// Zero-pad the sequence to `width` digits (`seq=7, width=4` -> `0007`).
fn format_seq(seq: u64, width: usize) -> String {
    format!("{seq:0width$}")
}

/// Replace characters that would break a path component. Values are
/// sanitized before substitution, so only literal rule text can nest.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect()
}

/// Calendar fields of a Unix timestamp (UTC; EXIF capture times are naive).
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

fn civil_from_unix(ts: i64) -> Civil {
    let days = ts.div_euclid(86_400);
    let rem = ts.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (rem / 3_600) as u32,
        minute: ((rem % 3_600) / 60) as u32,
        second: (rem % 60) as u32,
    }
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to `(y, m, d)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> NamingContext {
        NamingContext {
            session: "shoot".into(),
            original_name: "DSC01234.NEF".into(),
            seq: 7,
            // 2026-10-07 12:34:56 UTC.
            taken_at: Some(1_791_376_496),
            camera: Some("Canon EOS R6".into()),
        }
    }

    #[test]
    fn folder_rule_nests_date_folders() {
        let dir = render_dir("{yyyy}/{mm}-{dd}", &ctx()).unwrap();
        assert_eq!(dir, PathBuf::from("2026/10-07"));
    }

    #[test]
    fn filename_rule_uses_session_and_padded_sequence() {
        assert_eq!(
            render_filename("{session}_{seq:4}", &ctx()).unwrap(),
            "shoot_0007"
        );
        assert_eq!(render_filename("{seq}", &ctx()).unwrap(), "7");
    }

    #[test]
    fn original_name_variables_resolve() {
        assert_eq!(
            render_filename("{filename}", &ctx()).unwrap(),
            "DSC01234.NEF"
        );
        assert_eq!(
            render_filename("{filenamebase}_{ext}", &ctx()).unwrap(),
            "DSC01234_nef"
        );
    }

    #[test]
    fn original_extension_is_preserved_on_rename() {
        assert_eq!(
            with_original_extension("shoot_0001", "DSC01234.NEF"),
            "shoot_0001.NEF"
        );
        // A rule that already carries the extension is left alone.
        assert_eq!(
            with_original_extension("DSC01234.NEF", "DSC01234.NEF"),
            "DSC01234.NEF"
        );
        assert_eq!(
            with_original_extension("shoot_0001.JPG", "DSC01234.JPG"),
            "shoot_0001.JPG"
        );
    }

    #[test]
    fn date_variables_cover_compact_forms() {
        assert_eq!(
            render_filename("{datesort}_{datesortdash}", &ctx()).unwrap(),
            "20261007_2026-10-07"
        );
        assert_eq!(
            render_filename("{yy}{month0}{day0}_{hh}{mi}{ss}", &ctx()).unwrap(),
            "261007_123456"
        );
        assert_eq!(render_filename("{year4}", &ctx()).unwrap(), "2026");
    }

    #[test]
    fn literal_folder_text_is_preserved() {
        let dir = render_dir("/photos/{yyyy}/{mm}", &ctx()).unwrap();
        assert_eq!(dir, PathBuf::from("/photos/2026/10"));
    }

    #[test]
    fn unknown_and_malformed_tokens_error() {
        assert!(validate("{nope}").unwrap_err().to_string().contains("nope"));
        assert_eq!(
            validate("{session").unwrap_err(),
            NamingError::UnterminatedToken
        );
        assert_eq!(validate("{}").unwrap_err(), NamingError::EmptyToken);
        assert!(matches!(
            validate("{seq:x}").unwrap_err(),
            NamingError::InvalidWidth { .. }
        ));
        assert!(matches!(
            validate("{seq:0}").unwrap_err(),
            NamingError::InvalidWidth { .. }
        ));
    }

    #[test]
    fn missing_capture_time_errors_for_date_variables() {
        let mut c = ctx();
        c.taken_at = None;
        assert_eq!(
            render("{yyyy}", &c).unwrap_err(),
            NamingError::MissingCaptureTime("yyyy".into())
        );
        // Non-date variables still resolve.
        assert_eq!(render_filename("{session}", &c).unwrap(), "shoot");
    }

    #[test]
    fn filename_rule_rejects_separators_and_reserved_names() {
        let mut c = ctx();
        c.session = "../evil".into();
        // The separator inside the value is sanitized, so no escape.
        assert_eq!(render_filename("{session}", &c).unwrap(), ".._evil");
        // A literal separator in the filename rule is rejected.
        assert_eq!(
            render_filename("a/b", &ctx()).unwrap_err(),
            NamingError::PathSeparator
        );
        assert_eq!(
            render_filename(
                "{session}",
                &NamingContext {
                    session: ".".into(),
                    ..ctx()
                }
            )
            .unwrap_err(),
            NamingError::ReservedComponent(".".into())
        );
    }

    #[test]
    fn folder_rule_rejects_parent_dir_components() {
        assert_eq!(
            render_dir("a/../b", &ctx()).unwrap_err(),
            NamingError::ReservedComponent("..".into())
        );
    }

    #[test]
    fn static_prefix_stops_at_first_variable() {
        assert_eq!(static_prefix("/photos/{yyyy}/{mm}-{dd}"), "/photos");
        assert_eq!(static_prefix("/photos/2026"), "/photos/2026");
        assert_eq!(static_prefix("{yyyy}"), "");
        assert_eq!(static_prefix("/photos/"), "/photos");
    }

    #[test]
    fn filename_rules_that_cannot_name_a_file_are_rejected_up_front() {
        assert_eq!(validate_filename_rule(""), Err(NamingError::EmptyName));
        assert_eq!(
            validate_filename_rule("a/b"),
            Err(NamingError::PathSeparator)
        );
        assert!(matches!(
            validate_filename_rule("{nope}"),
            Err(NamingError::UnknownVariable(_))
        ));
        assert!(validate_filename_rule(DEFAULT_FILENAME_RULE).is_ok());
        // A rule that only renders for files that carry a camera still passes.
        assert!(validate_filename_rule("{camera}_{seq}").is_ok());
    }

    #[test]
    fn a_tilde_destination_resolves_under_home() {
        let ctx = NamingContext {
            session: "s".into(),
            original_name: "A.NEF".into(),
            seq: 1,
            taken_at: Some(0),
            camera: None,
        };
        assert_eq!(
            static_prefix("~/Downloads/test_app"),
            "~/Downloads/test_app"
        );
        let dir = render_dir("~/Downloads/test_app", &ctx).unwrap();
        assert_eq!(dir, expand_tilde("~/Downloads/test_app"));
        if std::env::var_os("HOME").is_some() {
            assert!(!dir.starts_with("~"), "{dir:?}");
            assert!(dir.ends_with("Downloads/test_app"));
        }
        assert!(
            render_dir(DEFAULT_DEST_RULE, &ctx)
                .unwrap()
                .ends_with("Photos/1970/01-01")
        );
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::ffi::OsStr::new("/home/tester");
        assert_eq!(
            expand_tilde_with("~/Photos", Some(home)),
            PathBuf::from("/home/tester/Photos")
        );
        assert_eq!(
            expand_tilde_with("~", Some(home)),
            PathBuf::from("/home/tester")
        );
        assert_eq!(expand_tilde_with("/abs", Some(home)), PathBuf::from("/abs"));
        // No home: the rule is returned unchanged.
        assert_eq!(
            expand_tilde_with("~/Photos", None),
            PathBuf::from("~/Photos")
        );
    }
}
