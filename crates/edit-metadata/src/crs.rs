//! Minimal `crs:` sidecar write from [`DevelopSettings`].
//!
//! Best-effort Lightroom interop (plan section 1.3, research
//! `lr-edit-math-and-roundtrip.md` section 4): `ProcessVersion=11.0`,
//! `Exposure2012`, the `*2012` sliders present in the model, WB,
//! `ToneCurvePV2012` when set, and crop. The packet is written directly
//! with quick-xml — deterministic, no subprocess — so sidecars work even
//! when exiftool is missing. Tag names were verified readable by
//! `exiftool -X` (see the B2 demo).
//!
//! ExifTool naming gotcha: the WB temperature attribute is `crs:Temperature`
//! in the file; exiftool *displays* it as `ColorTemperature` and only
//! accepts `-XMP-crs:ColorTemperature=` on the command line.

use std::path::Path;

use edit_core::{Crop, DevelopSettings};

use crate::error::Result;

/// `crs:` attribute name for the process version every sidecar carries.
pub const PROCESS_VERSION: &str = "11.0";

/// Scalar `crs:` attributes for one edit, in write order.
pub fn develop_attributes(develop: &DevelopSettings) -> Vec<(String, String)> {
    let light = &develop.light;
    let effects = &develop.effects;
    vec![
        ("ProcessVersion".into(), PROCESS_VERSION.into()),
        ("WhiteBalance".into(), develop.wb.mode.label().to_owned()),
        (
            "Temperature".into(),
            format!("{}", develop.wb.temp.round() as i64),
        ),
        ("Tint".into(), format!("{:+.0}", develop.wb.tint)),
        ("Exposure2012".into(), format!("{:+.2}", light.exposure)),
        ("Contrast2012".into(), int_tag(light.contrast)),
        ("Highlights2012".into(), int_tag(light.highlights)),
        ("Shadows2012".into(), int_tag(light.shadows)),
        ("Whites2012".into(), int_tag(light.whites)),
        ("Blacks2012".into(), int_tag(light.blacks)),
        ("Clarity2012".into(), int_tag(effects.clarity)),
        ("Texture".into(), int_tag(effects.texture)),
        ("Dehaze".into(), int_tag(effects.dehaze)),
    ]
}

/// `crs:` crop attributes. Always includes `HasCrop`; edges and angle only
/// when cropped. `CropAngle` carries the app rotation as-is — the sign
/// convention against Lightroom is still unverified (M2 crop work owns it).
pub fn crop_attributes(crop: Option<&Crop>) -> Vec<(String, String)> {
    let Some(crop) = crop else {
        return vec![("HasCrop".into(), "False".into())];
    };
    let rect = crop.rect.clamped();
    let cropped = rect != edit_core::Rect::full() || crop.rotation_degrees != 0.0;
    if !cropped {
        return vec![("HasCrop".into(), "False".into())];
    }
    vec![
        ("HasCrop".into(), "True".into()),
        ("CropLeft".into(), num(rect.x)),
        ("CropTop".into(), num(rect.y)),
        ("CropRight".into(), num(rect.x + rect.w)),
        ("CropBottom".into(), num(rect.y + rect.h)),
        ("CropAngle".into(), num(crop.rotation_degrees)),
    ]
}

/// Point-curve points in LR `0, 0` … `255, 255` form, or `None` when the
/// master curve is identity (empty or a no-op diagonal).
pub fn tone_curve_points(develop: &DevelopSettings) -> Option<Vec<(u8, u8)>> {
    let master = &develop.curve.master;
    if master.is_empty() || master.iter().all(|p| (p.x - p.y).abs() < 1e-9) {
        return None;
    }
    Some(
        master
            .iter()
            .map(|p| {
                (
                    (p.x * 255.0).round().clamp(0.0, 255.0) as u8,
                    (p.y * 255.0).round().clamp(0.0, 255.0) as u8,
                )
            })
            .collect(),
    )
}

fn int_tag(value: f64) -> String {
    format!("{}", value.round() as i64)
}

fn num(value: f32) -> String {
    // Normalized crop edges: up to 6 decimals, no trailing noise.
    let s = format!("{value:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Write (atomically) an LR-readable `.xmp` sidecar for one edit.
pub fn write_sidecar(path: &Path, develop: &DevelopSettings, crop: Option<&Crop>) -> Result<()> {
    let packet = sidecar_packet(develop, crop);
    write_atomic(path, packet.as_bytes())?;
    Ok(())
}

/// Render the full XMP packet without touching the filesystem.
pub fn sidecar_packet(develop: &DevelopSettings, crop: Option<&Crop>) -> String {
    use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
    use quick_xml::writer::Writer;

    let mut attributes = develop_attributes(develop);
    attributes.extend(crop_attributes(crop));
    if tone_curve_points(develop).is_some() {
        attributes.push(("ToneCurveName2012".into(), "Custom".into()));
    }

    let mut out = Writer::new(Vec::new());
    out.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("in-memory write");
    out.write_event(Event::Text(BytesText::from_escaped("\n")))
        .expect("in-memory write");

    let mut meta = BytesStart::new("x:xmpmeta");
    meta.push_attribute(("xmlns:x", "adobe:ns:meta/"));
    meta.push_attribute(("x:xmptk", "editing"));
    out.write_event(Event::Start(meta))
        .expect("in-memory write");

    let mut rdf = BytesStart::new("rdf:RDF");
    rdf.push_attribute(("xmlns:rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"));
    out.write_event(Event::Start(rdf)).expect("in-memory write");

    let mut desc = BytesStart::new("rdf:Description");
    desc.push_attribute(("rdf:about", ""));
    desc.push_attribute(("xmlns:crs", "http://ns.adobe.com/camera-raw-settings/1.0/"));
    for (name, value) in &attributes {
        desc.push_attribute((format!("crs:{name}").as_str(), value.as_str()));
    }
    let with_curve = tone_curve_points(develop);
    if let Some(points) = with_curve {
        out.write_event(Event::Start(desc))
            .expect("in-memory write");
        let curve = BytesStart::new("crs:ToneCurvePV2012");
        out.write_event(Event::Start(curve))
            .expect("in-memory write");
        let seq = BytesStart::new("rdf:Seq");
        out.write_event(Event::Start(seq)).expect("in-memory write");
        for (x, y) in points {
            let li = BytesStart::new("rdf:li");
            out.write_event(Event::Start(li)).expect("in-memory write");
            out.write_event(Event::Text(BytesText::new(&format!("{x}, {y}"))))
                .expect("in-memory write");
            out.write_event(Event::End(BytesEnd::new("rdf:li")))
                .expect("in-memory write");
        }
        out.write_event(Event::End(BytesEnd::new("rdf:Seq")))
            .expect("in-memory write");
        out.write_event(Event::End(BytesEnd::new("crs:ToneCurvePV2012")))
            .expect("in-memory write");
        out.write_event(Event::End(BytesEnd::new("rdf:Description")))
            .expect("in-memory write");
    } else {
        out.write_event(Event::Empty(desc))
            .expect("in-memory write");
    }

    out.write_event(Event::End(BytesEnd::new("rdf:RDF")))
        .expect("in-memory write");
    out.write_event(Event::End(BytesEnd::new("x:xmpmeta")))
        .expect("in-memory write");

    let bytes = out.into_inner();
    let mut packet = String::from_utf8(bytes).expect("writer emits UTF-8");
    packet.push('\n');
    packet
}

/// Write `bytes` to `path` via temp-file + rename so a crash never leaves a
/// half-written sidecar.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("xmp.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        crate::error::Error::Io(e)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use edit_core::Rect;
    use serde_json::json;

    fn develop_with(patch: serde_json::Value) -> DevelopSettings {
        let mut base = serde_json::to_value(DevelopSettings::default()).unwrap();
        for (k, v) in patch.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn develop_attributes_carry_the_2012_set() {
        let mut d = DevelopSettings::default();
        d.light.exposure = 0.7;
        d.light.highlights = -40.0;
        d.light.shadows = 25.0;
        d.light.whites = 10.0;
        d.light.blacks = -8.0;
        d.light.contrast = 10.0;
        d.effects.clarity = 15.0;
        d.effects.texture = 20.0;
        d.effects.dehaze = 5.0;
        let attrs: std::collections::HashMap<_, _> = develop_attributes(&d).into_iter().collect();
        assert_eq!(attrs["ProcessVersion"], "11.0");
        assert_eq!(attrs["Exposure2012"], "+0.70");
        assert_eq!(attrs["Contrast2012"], "10");
        assert_eq!(attrs["Highlights2012"], "-40");
        assert_eq!(attrs["Shadows2012"], "25");
        assert_eq!(attrs["Whites2012"], "10");
        assert_eq!(attrs["Blacks2012"], "-8");
        assert_eq!(attrs["Clarity2012"], "15");
        assert_eq!(attrs["Texture"], "20");
        assert_eq!(attrs["Dehaze"], "5");
    }

    #[test]
    fn crop_attributes_mark_uncropped_false() {
        assert_eq!(
            crop_attributes(None),
            vec![("HasCrop".to_owned(), "False".to_owned())]
        );
        let full = Crop {
            rect: Rect::full(),
            rotation_degrees: 0.0,
            aspect: None,
        };
        assert_eq!(
            crop_attributes(Some(&full)),
            vec![("HasCrop".to_owned(), "False".to_owned())]
        );
    }

    #[test]
    fn crop_attributes_emit_edges_when_cropped() {
        let crop = Crop {
            rect: Rect::new(0.05, 0.1, 0.9, 0.8),
            rotation_degrees: 0.0,
            aspect: None,
        };
        let attrs: std::collections::HashMap<_, _> =
            crop_attributes(Some(&crop)).into_iter().collect();
        assert_eq!(attrs["HasCrop"], "True");
        assert_eq!(attrs["CropLeft"], "0.05");
        assert_eq!(attrs["CropTop"], "0.1");
        assert_eq!(attrs["CropRight"], "0.95");
        assert_eq!(attrs["CropBottom"], "0.9");
    }

    #[test]
    fn identity_curve_emits_no_tone_curve() {
        let d = DevelopSettings::default();
        assert_eq!(tone_curve_points(&d), None);
        assert!(!sidecar_packet(&d, None).contains("ToneCurvePV2012"));
    }

    #[test]
    fn custom_curve_emits_seq_points() {
        let d = develop_with(json!({"curve": {"master": [
            {"x": 0.0, "y": 0.0},
            {"x": 0.5, "y": 0.6},
            {"x": 1.0, "y": 1.0},
        ]}}));
        let points = tone_curve_points(&d).unwrap();
        assert_eq!(points, vec![(0, 0), (128, 153), (255, 255)]);
        let packet = sidecar_packet(&d, None);
        assert!(packet.contains("ToneCurveName2012=\"Custom\""));
        assert!(packet.contains("<rdf:li>128, 153</rdf:li>"));
    }

    #[test]
    fn packet_is_well_formed_xml() {
        let packet = sidecar_packet(
            &develop_with(json!({"wb": {"mode": "custom", "temp": 5600.0, "tint": 5.0}})),
            None,
        );
        let mut reader = quick_xml::reader::Reader::from_str(&packet);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("malformed packet: {e}"),
            }
        }
        assert!(packet.contains("crs:ProcessVersion=\"11.0\""));
        assert!(packet.contains("crs:WhiteBalance=\"Custom\""));
        assert!(packet.contains("crs:Temperature=\"5600\""));
    }
}
