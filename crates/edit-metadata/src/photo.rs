//! EXIF and XMP read path for catalog fields.
//!
//! [`read_photo_metadata`] is the ingest-time entry point: it reads EXIF
//! with kamadak-exif and fills gaps from a same-name XMP sidecar with
//! quick-xml. Both reads are best-effort — a missing or corrupt source
//! yields empty fields, never an error.

use std::path::{Path, PathBuf};

use exif::{Reader, Value};

use crate::error::{Error, Result};

/// Catalog-facing capture metadata (mirrors the `photos` table columns).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhotoMetadata {
    pub taken_at: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub focal: Option<i64>,
    pub orientation: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

impl PhotoMetadata {
    /// Fill every `None` field from `other`.
    pub fn merge(&mut self, other: Self) {
        if self.taken_at.is_none() {
            self.taken_at = other.taken_at;
        }
        if self.camera.is_none() {
            self.camera = other.camera;
        }
        if self.lens.is_none() {
            self.lens = other.lens;
        }
        if self.iso.is_none() {
            self.iso = other.iso;
        }
        if self.aperture.is_none() {
            self.aperture = other.aperture;
        }
        if self.shutter.is_none() {
            self.shutter = other.shutter;
        }
        if self.focal.is_none() {
            self.focal = other.focal;
        }
        if self.orientation.is_none() {
            self.orientation = other.orientation;
        }
        if self.width.is_none() {
            self.width = other.width;
        }
        if self.height.is_none() {
            self.height = other.height;
        }
    }

    /// True when every field is `None`.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Read EXIF, then fill gaps from the XMP sidecar. Never fails: unreadable
/// sources contribute empty fields.
pub fn read_photo_metadata(path: &Path) -> PhotoMetadata {
    let mut meta = read_exif(path).unwrap_or_default();
    meta.merge(read_xmp_sidecar_for(path));
    meta
}

/// Read capture fields from embedded EXIF (kamadak-exif).
pub fn read_exif(path: &Path) -> Result<PhotoMetadata> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let exif = Reader::new().read_from_container(&mut reader)?;
    let mut meta = PhotoMetadata {
        camera: field_text(&exif, TAG_MODEL)
            .or_else(|| field_text(&exif, TAG_MAKE))
            .map(strip_nul),
        lens: field_text(&exif, TAG_LENS_MODEL).map(strip_nul),
        iso: field_i64(&exif, TAG_ISO),
        aperture: field_f64(&exif, TAG_FNUMBER),
        shutter: exposure_text(&exif),
        focal: field_f64(&exif, TAG_FOCAL_LENGTH).map(|f| f.round() as i64),
        orientation: field_i64(&exif, TAG_ORIENTATION),
        width: field_i64(&exif, TAG_PIXEL_X).or_else(|| field_i64(&exif, TAG_IMAGE_WIDTH)),
        height: field_i64(&exif, TAG_PIXEL_Y).or_else(|| field_i64(&exif, TAG_IMAGE_HEIGHT)),
        taken_at: field_text(&exif, TAG_DATETIME_ORIGINAL)
            .and_then(|s| parse_exif_datetime(&s))
            .or_else(|| field_text(&exif, TAG_DATETIME).and_then(|s| parse_exif_datetime(&s))),
    };
    if meta.width.is_none() || meta.height.is_none() {
        // JPEGs rarely carry dimension tags; the SOF marker always has them.
        if let Some((w, h)) = jpeg_dimensions(path) {
            meta.width = meta.width.or(Some(w));
            meta.height = meta.height.or(Some(h));
        }
    }
    Ok(meta)
}

/// Read the XMP sidecar next to `photo` (`photo.xmp`, then `<stem>.xmp`).
/// Missing or unparsable files yield empty metadata.
pub fn read_xmp_sidecar_for(photo: &Path) -> PhotoMetadata {
    for candidate in sidecar_candidates(photo) {
        if candidate.is_file() {
            return read_xmp_file(&candidate).unwrap_or_default();
        }
    }
    PhotoMetadata::default()
}

/// Parse one XMP packet into catalog fields.
pub fn read_xmp_file(path: &Path) -> Result<PhotoMetadata> {
    let text = std::fs::read_to_string(path)?;
    Ok(photo_from_pairs(&xmp_pairs(&text)?))
}

/// Sidecar names in lookup order: `<photo>.xmp`, then `<stem>.xmp`.
fn sidecar_candidates(photo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    let mut with_ext = photo.as_os_str().to_owned();
    with_ext.push(".xmp");
    out.push(PathBuf::from(with_ext));
    if let (Some(parent), Some(stem)) = (photo.parent(), photo.file_stem()) {
        out.push(parent.join(stem).with_extension("xmp"));
    }
    out
}

// ---- EXIF field helpers ----

/// Tag numbers (EXIF 2.3). Matched by number, not by kamadak context:
/// writers disagree on whether sub-IFD tags live in IFD0 (`Tiff`) or the
/// EXIF sub-IFD (`Exif`) — PIL puts them in IFD0, cameras in the sub-IFD.
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_IMAGE_WIDTH: u16 = 0x0100;
const TAG_IMAGE_HEIGHT: u16 = 0x0101;
const TAG_DATETIME: u16 = 0x0132;
const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_FNUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_DATETIME_ORIGINAL: u16 = 0x9003;
const TAG_FOCAL_LENGTH: u16 = 0x920A;
const TAG_PIXEL_X: u16 = 0xA002;
const TAG_PIXEL_Y: u16 = 0xA003;
const TAG_LENS_MODEL: u16 = 0xA434;

/// First field with this tag number, across IFD0 and thumbnail.
fn find_field(exif: &exif::Exif, tag: u16) -> Option<&exif::Field> {
    exif.fields().find(|f| f.tag.1 == tag)
}

fn field_text(exif: &exif::Exif, tag: u16) -> Option<String> {
    let field = find_field(exif, tag)?;
    match &field.value {
        Value::Ascii(rows) => rows
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .map(|s| s.trim_matches(['\0', ' ', '\n', '\r', '\t']).to_owned())
            .find(|s| !s.is_empty()),
        _ => {
            let s = field.value.display_as(field.tag).to_string();
            let s = s.trim().to_owned();
            (!s.is_empty()).then_some(s)
        }
    }
}

fn value_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Byte(v) => v.first().map(|n| *n as f64),
        Value::Short(v) => v.first().map(|n| *n as f64),
        Value::Long(v) => v.first().map(|n| *n as f64),
        Value::Rational(v) => v.first().and_then(|r| div_f64(r.num, r.denom)),
        Value::SRational(v) => v.first().and_then(|r| div_f64(r.num, r.denom)),
        Value::Float(v) => v.first().map(|n| *n as f64),
        Value::Double(v) => v.first().copied(),
        Value::Ascii(rows) => rows.first().and_then(|b| {
            String::from_utf8_lossy(b)
                .trim_matches(['\0', ' '])
                .parse::<f64>()
                .ok()
        }),
        _ => None,
    }
}

/// `num / denom`, or `None` on a zero denominator.
fn div_f64(num: impl Into<f64>, denom: impl Into<f64>) -> Option<f64> {
    let denom = denom.into();
    if denom == 0.0 {
        return None;
    }
    Some(num.into() / denom)
}

fn field_f64(exif: &exif::Exif, tag: u16) -> Option<f64> {
    let field = find_field(exif, tag)?;
    value_f64(&field.value).or_else(|| {
        field
            .value
            .display_as(field.tag)
            .to_string()
            .trim()
            .parse::<f64>()
            .ok()
    })
}

fn field_i64(exif: &exif::Exif, tag: u16) -> Option<i64> {
    field_f64(exif, tag).map(|f| f.round() as i64)
}

/// `ExposureTime` as the catalog `shutter` string (`1/250`, `2.5`, …).
fn exposure_text(exif: &exif::Exif) -> Option<String> {
    let field = find_field(exif, TAG_EXPOSURE_TIME)?;
    match &field.value {
        Value::Rational(v) => v.first().and_then(|r| format_shutter(r.num, r.denom)),
        Value::SRational(v) => v
            .first()
            .and_then(|r| format_shutter_i64(r.num as i64, r.denom as i64)),
        _ => {
            let s = field.value.display_as(field.tag).to_string();
            let s = s.trim().to_owned();
            (!s.is_empty()).then_some(s)
        }
    }
}

fn format_shutter(num: u32, denom: u32) -> Option<String> {
    format_shutter_i64(num as i64, denom as i64)
}

fn format_shutter_i64(num: i64, denom: i64) -> Option<String> {
    if denom == 0 {
        return None;
    }
    if num == 1 {
        return Some(format!("1/{denom}"));
    }
    let value = num as f64 / denom as f64;
    if value >= 1.0 {
        Some(trim_float(value, 1))
    } else {
        Some(format!("{num}/{denom}"))
    }
}

fn trim_float(value: f64, precision: usize) -> String {
    let s = format!("{value:.precision$}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn strip_nul(s: String) -> String {
    s.trim_matches(['\0', ' ', '\n', '\r', '\t']).to_owned()
}

/// JPEG frame dimensions from the first SOF marker. EXIF dimension tags
/// are often absent (encoders put them in the SOF instead), so this is the
/// fallback that makes `width`/`height` work for plain JPEGs.
fn jpeg_dimensions(path: &Path) -> Option<(i64, i64)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        // SOF0-SOF15 carry dimensions, except DHT/DAC/JPGn markers.
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if len < 7 || i + 2 + len > bytes.len() {
                return None;
            }
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]);
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]);
            return Some((i64::from(w), i64::from(h)));
        }
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
        } else {
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if len < 2 || i + 2 + len > bytes.len() {
                return None;
            }
            if marker == 0xDA {
                return None; // Scan data: SOF always precedes it.
            }
            i += 2 + len;
        }
    }
    None
}

// ---- datetime parsing (no chrono dependency) ----

/// `YYYY:MM:DD HH:MM:SS` (EXIF) to Unix seconds. No timezone handling:
/// EXIF datetimes are naive wall-clock by spec.
fn parse_exif_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split(':');
    let (y, mo, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (h, mi, sec): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.split(['.', '+', '-']).next()?.parse().ok()?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, day) * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// ISO 8601 (`2026-10-07T12:34:56Z`, with `±HH:MM` offset or naive) to Unix.
fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim().replace('T', " ");
    let (rest, offset) = split_offset(&s);
    let naive = parse_exif_dash_datetime(rest)?;
    Some(naive - offset)
}

fn split_offset(s: &str) -> (&str, i64) {
    if let Some(rest) = s.strip_suffix(['Z', 'z']) {
        return (rest, 0);
    }
    // A timezone offset is the last +HH:MM / -HHMM / -HH:MM after the time.
    if let Some(i) = s.rfind(['+', '-'])
        && i + 1 < s.len()
        && s.len() - i >= 3
        && s[i + 1..].chars().take(2).all(|c| c.is_ascii_digit())
    {
        let sign = if s.as_bytes()[i] == b'+' { 1 } else { -1 };
        let digits: String = s[i + 1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 4 {
            let h: i64 = digits[..2].parse().unwrap_or(0);
            let m: i64 = digits[2..4].parse().unwrap_or(0);
            return (s[..i].trim(), sign * (h * 3_600 + m * 60));
        }
    }
    (s, 0)
}

fn parse_exif_dash_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (h, mi, sec): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.split('.').next()?.parse().ok()?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, day) * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// Howard Hinnant's days-from-civil: days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---- XMP parsing (quick-xml) ----

/// Flatten an XMP packet into `(local-name, value)` pairs from attributes
/// (`tiff:Model="…"`) and simple elements (`<exif:FNumber>5.6</…>`).
fn xmp_pairs(text: &str) -> Result<Vec<(String, String)>> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut pairs = Vec::new();
    let mut element: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                collect_attributes(&e, &mut pairs);
                element = local_name(&e.name());
            }
            Ok(Event::Empty(e)) => {
                collect_attributes(&e, &mut pairs);
                element = None;
            }
            Ok(Event::Text(e)) => {
                if let (Some(name), Ok(value)) = (element.clone(), e.decode()) {
                    let value = value.trim().to_owned();
                    if !value.is_empty() {
                        pairs.push((name, value));
                    }
                }
            }
            Ok(Event::End(_)) => element = None,
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::Xml(e)),
            _ => {}
        }
    }
    Ok(pairs)
}

fn local_name(name: &quick_xml::name::QName) -> Option<String> {
    let raw = std::str::from_utf8(name.as_ref()).ok()?;
    raw.split(':').next_back().map(|s| s.to_ascii_lowercase())
}

fn collect_attributes(e: &quick_xml::events::BytesStart, pairs: &mut Vec<(String, String)>) {
    for attr in e.attributes().flatten() {
        let Some(name) = std::str::from_utf8(attr.key.as_ref())
            .ok()
            .and_then(|k| k.split(':').next_back())
            .map(|s| s.to_ascii_lowercase())
        else {
            continue;
        };
        if name == "about" || name == "parseType" || name == "resource" {
            continue;
        }
        // Attribute values in sidecars are UTF-8 by spec; lossy decode
        // keeps the read path total.
        let value = String::from_utf8_lossy(attr.value.as_ref())
            .trim()
            .to_owned();
        if !value.is_empty() {
            pairs.push((name, value));
        }
    }
}

/// Fold flattened XMP pairs into catalog fields.
fn photo_from_pairs(pairs: &[(String, String)]) -> PhotoMetadata {
    let mut meta = PhotoMetadata::default();
    let mut make: Option<&str> = None;
    for (name, value) in pairs {
        match name.as_str() {
            "model" => meta.camera = Some(value.clone()),
            "make" => make = Some(value),
            "lensmodel" | "lens" if meta.lens.is_none() => {
                meta.lens = Some(value.clone());
            }
            "photographicsensitivity" | "isospeedratings" | "iso" if meta.iso.is_none() => {
                meta.iso = leading_number(value).map(|f| f.round() as i64);
            }
            "fnumber" if meta.aperture.is_none() => {
                meta.aperture = leading_number(value);
            }
            "exposuretime" | "shutterspeed" if meta.shutter.is_none() => {
                meta.shutter = Some(value.clone());
            }
            "focallength" if meta.focal.is_none() => {
                meta.focal = leading_number(value).map(|f| f.round() as i64);
            }
            "datetimeoriginal" if meta.taken_at.is_none() => {
                meta.taken_at = parse_exif_datetime(value).or_else(|| parse_iso8601(value));
            }
            "createdate" | "metadatadate" | "date" | "datetime" if meta.taken_at.is_none() => {
                meta.taken_at = parse_iso8601(value).or_else(|| parse_exif_datetime(value));
            }
            "orientation" if meta.orientation.is_none() => {
                meta.orientation = value.trim().parse::<i64>().ok();
            }
            "pixelxdimension" | "imagewidth" | "width" if meta.width.is_none() => {
                meta.width = leading_number(value).map(|f| f.round() as i64);
            }
            "pixelydimension" | "imagelength" | "imageheight" | "height"
                if meta.height.is_none() =>
            {
                meta.height = leading_number(value).map(|f| f.round() as i64);
            }
            _ => {}
        }
    }
    if meta.camera.is_none() {
        meta.camera = make.map(str::to_owned);
    }
    meta
}

/// Leading decimal of strings like `5600`, `f/2.8`, `24.0 mm`.
fn leading_number(s: &str) -> Option<f64> {
    let s = s.trim().trim_start_matches("f/");
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '+' && c != '-')
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    s[..end].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_datetime_parses_wall_clock() {
        // 2026-10-07 12:00:00 UTC.
        assert_eq!(
            parse_exif_datetime("2026:10:07 12:00:00"),
            Some(1_791_374_400)
        );
        assert_eq!(parse_exif_datetime("1970:01:01 00:00:00"), Some(0));
        assert_eq!(parse_exif_datetime("not a date"), None);
        assert_eq!(parse_exif_datetime("2026:13:01 00:00:00"), None);
        assert_eq!(parse_exif_datetime("2026:10:07"), None);
    }

    #[test]
    fn iso8601_parses_offsets() {
        assert_eq!(
            parse_iso8601("2026-10-07T12:00:00Z"),
            parse_exif_datetime("2026:10:07 12:00:00")
        );
        assert_eq!(
            parse_iso8601("2026-10-07T14:00:00+02:00"),
            parse_exif_datetime("2026:10:07 12:00:00")
        );
        assert_eq!(parse_iso8601("garbage"), None);
    }

    #[test]
    fn shutter_formats_exposure_rational() {
        assert_eq!(format_shutter(1u32, 250u32), Some("1/250".into()));
        assert_eq!(format_shutter(1u32, 1u32), Some("1/1".into()));
        assert_eq!(format_shutter(5u32, 2u32), Some("2.5".into()));
        assert_eq!(format_shutter(1u32, 0u32), None);
    }

    #[test]
    fn xmp_pairs_cover_attributes_and_elements() {
        let xml = r#"<?xpacket begin=""?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
   xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
   xmlns:exif="http://ns.adobe.com/exif/1.0/"
   tiff:Model="EOS R6" tiff:Make="Canon" exif:LensModel="RF24-105mm"
   exif:PhotographicSensitivity="400" tiff:Orientation="6">
   <exif:FNumber>5.6</exif:FNumber>
   <exif:ExposureTime>1/250</exif:ExposureTime>
   <exif:DateTimeOriginal>2026:10:07 12:00:00</exif:DateTimeOriginal>
   <tiff:ImageWidth>5472</tiff:ImageWidth>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        let meta = photo_from_pairs(&xmp_pairs(xml).unwrap());
        assert_eq!(meta.camera.as_deref(), Some("EOS R6"));
        assert_eq!(meta.lens.as_deref(), Some("RF24-105mm"));
        assert_eq!(meta.iso, Some(400));
        assert_eq!(meta.aperture, Some(5.6));
        assert_eq!(meta.shutter.as_deref(), Some("1/250"));
        assert_eq!(meta.taken_at, Some(1_791_374_400));
        assert_eq!(meta.orientation, Some(6));
        assert_eq!(meta.width, Some(5472));
    }

    #[test]
    fn xmp_make_falls_back_when_model_missing() {
        let meta = photo_from_pairs(&[("make".into(), "Canon".into())]);
        assert_eq!(meta.camera.as_deref(), Some("Canon"));
    }

    #[test]
    fn merge_fills_gaps_only() {
        let mut a = PhotoMetadata {
            iso: Some(100),
            ..PhotoMetadata::default()
        };
        a.merge(PhotoMetadata {
            iso: Some(400),
            camera: Some("X".into()),
            ..PhotoMetadata::default()
        });
        assert_eq!(a.iso, Some(100));
        assert_eq!(a.camera.as_deref(), Some("X"));
    }

    #[test]
    fn missing_sidecar_reads_empty() {
        let meta = read_xmp_sidecar_for(Path::new("/nonexistent/photo.NEF"));
        assert!(meta.is_empty());
    }

    #[test]
    fn jpeg_sof_parser_reads_frame_size() {
        // Minimal JPEG: SOI, APP0, SOF0 (8x6), EOI.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        bytes.extend([0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x06, 0x00, 0x08]);
        bytes.extend([0x01, 0x01, 0x11, 0x00]);
        bytes.extend([0xFF, 0xD9]);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sof.jpg");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(super::jpeg_dimensions(&path), Some((8, 6)));
        assert_eq!(super::jpeg_dimensions(Path::new("/nonexistent.jpg")), None);
    }
}
