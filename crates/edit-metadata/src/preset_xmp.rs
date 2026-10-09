//! Lightroom `.xmp` presets: read a preset (or a photo's own sidecar) into
//! develop settings, and write develop settings as a preset Lightroom can
//! load.
//!
//! A preset is an `rdf:Description` in the `crs:` namespace. Settings are
//! attributes named like the sidecar's (see [`crate::crs_keys`]); the preset's
//! name and group are `rdf:Alt` elements (Lightroom) or plain attributes
//! (older tools), and both are read. Keys the control table does not know are
//! not applied and are listed in [`ParsedPreset::skipped`].

use edit_core::DevelopSettings;
use lightcraft_develop::controls;
use quick_xml::events::Event;

use crate::crs;
use crate::crs_keys::crs_controls;

/// Version stamped on exported presets.
const PRESET_VERSION: &str = "14.0";

/// The identity of a preset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresetInfo {
    pub name: Option<String>,
    pub group: Option<String>,
    pub uuid: Option<String>,
}

/// A preset file read from disk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedPreset {
    pub info: PresetInfo,
    /// Every `crs:` setting attribute that is not preset metadata.
    pub attrs: Vec<(String, String)>,
    /// `crs:` keys and elements the app has no control for.
    pub skipped: Vec<String>,
}

impl ParsedPreset {
    /// The settings the preset stores, over the defaults.
    pub fn settings(&self) -> DevelopSettings {
        let mut settings = DevelopSettings::default();
        crs::apply_attributes(&mut settings, &self.attrs);
        settings
    }
}

fn is_metadata(key: &str) -> bool {
    key.starts_with("Supports")
        || matches!(
            key,
            "PresetType"
                | "Cluster"
                | "UUID"
                | "Name"
                | "ShortName"
                | "SortName"
                | "Group"
                | "Version"
                | "ProcessVersion"
                | "CompatibleVersion"
                | "HasSettings"
                | "CameraModelRestriction"
                | "Copyright"
                | "ContactInfo"
                | "ToneCurveName2012"
        )
}

fn is_setting(key: &str) -> bool {
    matches!(
        key,
        "WhiteBalance" | "Temperature" | "Tint" | "LensProfileEnable" | "AutoLateralCA"
    ) || crs_controls().iter().any(|(_, k)| k == key)
}

/// Read a preset or a sidecar packet. Never fails: unreadable XML gives an
/// empty preset.
pub fn parse_preset(xml: &str) -> ParsedPreset {
    let mut out = ParsedPreset::default();
    let mut reader = quick_xml::Reader::from_str(xml);
    // Element whose text is the value of a metadata field.
    let mut capture: Option<String> = None;
    let mut buffer = String::new();
    while let Ok(event) = reader.read_event() {
        let opens = matches!(event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if let Some(name) = tag.strip_prefix("crs:") {
                    match name {
                        "Name" | "Group" | "UUID" => {
                            if opens {
                                capture = Some(name.to_owned());
                            }
                        }
                        _ if !is_metadata(name) => skip(&mut out, name),
                        _ => {}
                    }
                }
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
                    let Some(name) = key.strip_prefix("crs:") else {
                        continue;
                    };
                    let Ok(value) = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    else {
                        continue;
                    };
                    let value = value.into_owned();
                    match name {
                        "Name" => out.info.name = Some(value),
                        "Group" => out.info.group = Some(value),
                        "UUID" => out.info.uuid = Some(value),
                        _ if is_setting(name) => out.attrs.push((name.to_owned(), value)),
                        _ if is_metadata(name) => {}
                        _ => skip(&mut out, name),
                    }
                }
            }
            Event::Text(text) => {
                if capture.is_some()
                    && let Ok(text) = text.decode()
                {
                    buffer.push_str(&text);
                }
            }
            Event::GeneralRef(entity) => {
                if capture.is_some()
                    && let Ok(name) = entity.decode()
                {
                    buffer.push_str(match name.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    });
                }
            }
            Event::End(e) => {
                if e.name().as_ref().starts_with(b"crs:")
                    && let Some(field) = capture.take()
                {
                    let text = buffer.trim();
                    if !text.is_empty() {
                        let text = Some(text.to_owned());
                        match field.as_str() {
                            "Name" => out.info.name = text,
                            "Group" => out.info.group = text,
                            _ => out.info.uuid = text,
                        }
                    }
                    buffer.clear();
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

fn skip(out: &mut ParsedPreset, name: &str) {
    if !out.skipped.iter().any(|s| s == name) {
        out.skipped.push(name.to_owned());
    }
}

/// A stable UUID (upper case, 8-4-4-4-12) for a preset name and group.
pub fn preset_uuid(name: &str, group: Option<&str>) -> String {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }
    let key = format!("{}\u{1f}{name}", group.unwrap_or_default());
    let a = fnv(0xcbf2_9ce4_8422_2325, key.as_bytes());
    let b = fnv(0x8422_2325_cbf2_9ce4, key.as_bytes());
    let hex = format!("{a:016X}{b:016X}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The settings of `develop` that differ from the defaults, as `crs:`
/// attributes. Controls at their default are left out so the preset changes
/// only what it names.
fn changed_attributes(develop: &DevelopSettings) -> Vec<(String, String)> {
    let defaults = DevelopSettings::default();
    let table = crs_controls();
    crs::develop_attributes(develop)
        .into_iter()
        .filter(|(key, _)| match key.as_str() {
            "ProcessVersion" | "HasSettings" => false,
            "WhiteBalance" => develop.wb.mode != defaults.wb.mode,
            _ => table
                .iter()
                .find(|(_, k)| k == key)
                .map(|(id, _)| controls::get(develop, id) != controls::get(&defaults, id))
                .unwrap_or(true),
        })
        .collect()
}

/// A Lightroom preset for `develop` named by `info`.
pub fn preset_packet(develop: &DevelopSettings, info: &PresetInfo) -> String {
    let name = info.name.clone().unwrap_or_else(|| "Preset".into());
    let group = info.group.clone();
    let uuid = info
        .uuid
        .clone()
        .unwrap_or_else(|| preset_uuid(&name, group.as_deref()));
    let mut attrs: Vec<(String, String)> = vec![
        ("PresetType".into(), "Normal".into()),
        ("Cluster".into(), String::new()),
        ("UUID".into(), uuid),
        ("SupportsAmount".into(), "False".into()),
        ("SupportsColor".into(), "True".into()),
        ("SupportsMonochrome".into(), "True".into()),
        ("SupportsHighDynamicRange".into(), "True".into()),
        ("SupportsNormalDynamicRange".into(), "True".into()),
        ("SupportsSceneReferred".into(), "True".into()),
        ("SupportsOutputReferred".into(), "True".into()),
        ("Version".into(), PRESET_VERSION.into()),
        ("ProcessVersion".into(), crs::PROCESS_VERSION.into()),
        ("HasSettings".into(), "True".into()),
    ];
    attrs.extend(changed_attributes(develop));

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"editing\">\n");
    xml.push_str(" <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n");
    xml.push_str("  <rdf:Description rdf:about=\"\"\n");
    xml.push_str("    xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"");
    for (key, value) in &attrs {
        xml.push_str(&format!("\n   crs:{key}=\"{}\"", escape(value)));
    }
    xml.push_str(">\n");
    let alt = |tag: &str, text: &str| {
        format!(
            "   <crs:{tag}>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </crs:{tag}>\n",
            escape(text)
        )
    };
    xml.push_str(&alt("Name", &name));
    xml.push_str(&alt("ShortName", ""));
    xml.push_str(&alt("SortName", ""));
    xml.push_str(&alt("Group", group.as_deref().unwrap_or("")));
    xml.push_str("  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edited() -> DevelopSettings {
        let mut s = DevelopSettings::default();
        controls::set(&mut s, "light.exposure", 0.5);
        controls::set(&mut s, "light.contrast", -20.0);
        controls::set(&mut s, "mixer.blue.sat", 15.0);
        controls::set(&mut s, "grain.amount", 30.0);
        s.optics.lens_profile = true;
        s
    }

    #[test]
    fn export_then_import_gives_identical_settings() {
        let settings = edited();
        let info = PresetInfo {
            name: Some("Evening & dusk".into()),
            group: Some("Mine".into()),
            uuid: None,
        };
        let parsed = parse_preset(&preset_packet(&settings, &info));
        assert_eq!(parsed.info.name.as_deref(), Some("Evening & dusk"));
        assert_eq!(parsed.info.group.as_deref(), Some("Mine"));
        assert_eq!(parsed.settings(), settings);
        assert!(parsed.skipped.is_empty(), "{:?}", parsed.skipped);
    }

    #[test]
    fn export_carries_the_keys_lightroom_needs() {
        let xml = preset_packet(&edited(), &PresetInfo::default());
        assert!(xml.contains("xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\""));
        assert!(xml.contains("crs:PresetType=\"Normal\""));
        assert!(xml.contains("crs:Version=\"14.0\""));
        assert!(xml.contains("crs:ProcessVersion=\"11.0\""));
        assert!(xml.contains("crs:Exposure2012=\"+0.50\""));
        assert!(xml.contains("<crs:Name>"));
        assert!(xml.contains("<crs:Group>"));
        // Controls at their default stay out so the preset changes only
        // what it names.
        assert!(!xml.contains("crs:Highlights2012"));
        assert!(!xml.contains("crs:WhiteBalance"));
    }

    #[test]
    fn the_uuid_is_stable_and_formatted() {
        let a = preset_uuid("Warm", Some("Mine"));
        assert_eq!(a, preset_uuid("Warm", Some("Mine")));
        assert_ne!(a, preset_uuid("Warm", None));
        let parts: Vec<_> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
    }

    #[test]
    fn unknown_keys_are_skipped_and_listed_once() {
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="r">
<rdf:Description rdf:about="" xmlns:crs="c" crs:Exposure2012="+1.00"
  crs:ConvertToGrayscale="True" crs:GrayMixerRed="-10" crs:LookTable="abc"
  crs:Contrast2012="10"><crs:ToneCurvePV2012><rdf:Seq><rdf:li>0, 0</rdf:li></rdf:Seq></crs:ToneCurvePV2012>
</rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let parsed = parse_preset(xml);
        assert_eq!(parsed.attrs.len(), 2);
        for key in [
            "ConvertToGrayscale",
            "GrayMixerRed",
            "LookTable",
            "ToneCurvePV2012",
        ] {
            assert!(parsed.skipped.iter().any(|s| s == key), "{key}");
        }
        let settings = parsed.settings();
        assert_eq!(controls::get(&settings, "light.exposure"), Some(1.0));
        assert_eq!(controls::get(&settings, "light.contrast"), Some(10.0));
    }

    #[test]
    fn a_photo_sidecar_reads_as_a_preset_without_a_name() {
        let settings = edited();
        let sidecar = crs::sidecar_packet(&settings, None);
        let parsed = parse_preset(&sidecar);
        assert_eq!(parsed.info.name, None);
        assert_eq!(parsed.settings(), settings);
    }

    #[test]
    fn name_and_group_read_from_attributes_too() {
        let parsed = parse_preset(
            r#"<rdf:Description xmlns:crs="c" crs:Name="Old" crs:Group="Pack" crs:UUID="U"/>"#,
        );
        assert_eq!(parsed.info.name.as_deref(), Some("Old"));
        assert_eq!(parsed.info.group.as_deref(), Some("Pack"));
        assert_eq!(parsed.info.uuid.as_deref(), Some("U"));
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use std::path::Path;

    fn load(rel: &str) -> ParsedPreset {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/presets");
        parse_preset(&std::fs::read_to_string(path.join(rel)).unwrap())
    }

    #[test]
    fn a_lightroom_preset_with_white_balance_reads_fully() {
        let p = load("Warm portrait.xmp");
        assert_eq!(p.info.name.as_deref(), Some("Warm portrait"));
        assert_eq!(p.info.group, None);
        assert!(p.skipped.is_empty());
        let s = p.settings();
        assert_eq!(s.wb.mode, lightcraft_develop::WbMode::Custom);
        assert_eq!(controls::get(&s, "wb.temp"), Some(6200.0));
        assert_eq!(controls::get(&s, "wb.tint"), Some(8.0));
        assert_eq!(controls::get(&s, "light.exposure"), Some(0.35));
        assert_eq!(controls::get(&s, "light.highlights"), Some(-30.0));
        assert_eq!(controls::get(&s, "effects.clarity"), Some(-10.0));
    }

    #[test]
    fn a_grouped_preset_reads_every_known_section() {
        let p = load("Portraits/Studio/Grouped settings.xmp");
        assert_eq!(p.info.group.as_deref(), Some("Portraits"));
        assert!(p.skipped.is_empty());
        let s = p.settings();
        for (id, v) in [
            ("effects.dehaze", 10.0),
            ("effects.texture", 15.0),
            ("detail.nrLuminance", 20.0),
            ("detail.sharpenAmount", 45.0),
            ("mixer.orange.hue", -5.0),
            ("mixer.blue.sat", -20.0),
            ("grading.global.hue", 40.0),
            ("calibration.redHue", -6.0),
        ] {
            assert_eq!(controls::get(&s, id), Some(v), "{id}");
        }
        assert!(s.optics.lens_profile && s.optics.remove_ca);
    }

    #[test]
    fn a_preset_with_unknown_keys_applies_the_known_ones_and_lists_the_rest() {
        let p = load("Film/Unknown keys.xmp");
        assert_eq!(
            p.skipped,
            [
                "ConvertToGrayscale",
                "GrayMixerRed",
                "LookTable",
                "ToneCurvePV2012"
            ]
        );
        let s = p.settings();
        assert_eq!(controls::get(&s, "light.exposure"), Some(-0.2));
        assert_eq!(controls::get(&s, "light.contrast"), Some(18.0));
    }

    #[test]
    fn every_fixture_survives_export_and_import() {
        for rel in [
            "Warm portrait.xmp",
            "Film/Faded film.xmp",
            "Film/Unknown keys.xmp",
            "Portraits/Studio/Grouped settings.xmp",
        ] {
            let p = load(rel);
            let again = parse_preset(&preset_packet(&p.settings(), &p.info));
            assert_eq!(again.settings(), p.settings(), "{rel}");
            assert_eq!(again.info.name, p.info.name, "{rel}");
        }
    }
}
