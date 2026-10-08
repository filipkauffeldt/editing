//! `crs:` sidecar for one photo, verified the way Lightroom would see it:
//! the packet is written from `DevelopSettings` + crop JSON, then every
//! tag is read back through `exiftool -X`. The exiftool half gates on an
//! exiftool binary (`$EXIFTOOL`, else `PATH`); the packet half always runs.

use std::process::Command;

use edit_core::{Crop, DevelopSettings, Rect};
use edit_metadata::{Writer, write_sidecar};

/// Scratch-test edit: +0.7 EV, a 3:2-ish crop, custom WB and a lifted curve.
fn demo_develop() -> DevelopSettings {
    let json = serde_json::json!({
        "wb": {"mode": "custom", "temp": 5600.0, "tint": 5.0},
        "light": {
            "exposure": 0.7, "contrast": 10.0, "highlights": -40.0,
            "shadows": 25.0, "whites": 10.0, "blacks": -8.0,
        },
        "effects": {"texture": 20.0, "clarity": 15.0, "dehaze": 5.0},
        "curve": {"master": [
            {"x": 0.0, "y": 0.0},
            {"x": 0.149, "y": 0.102},
            {"x": 0.729, "y": 0.698},
            {"x": 1.0, "y": 0.988},
        ]},
    });
    let mut base = serde_json::to_value(DevelopSettings::default()).unwrap();
    for (k, v) in json.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

fn demo_crop() -> Crop {
    Crop {
        rect: Rect::new(0.05, 0.1, 0.9, 0.8),
        rotation_degrees: 0.0,
        aspect: None,
    }
}

#[test]
fn sidecar_packet_carries_the_crs_tag_set() {
    let dir = tempfile::TempDir::new().unwrap();
    let sidecar = dir.path().join("DSC00001.xmp");
    write_sidecar(&sidecar, &demo_develop(), Some(&demo_crop())).unwrap();

    let packet = std::fs::read_to_string(&sidecar).unwrap();
    for tag in [
        "crs:ProcessVersion=\"11.0\"",
        "crs:WhiteBalance=\"Custom\"",
        "crs:Temperature=\"5600\"",
        "crs:Tint=\"+5\"",
        "crs:Exposure2012=\"+0.70\"",
        "crs:Contrast2012=\"10\"",
        "crs:Highlights2012=\"-40\"",
        "crs:Shadows2012=\"25\"",
        "crs:Whites2012=\"10\"",
        "crs:Blacks2012=\"-8\"",
        "crs:Clarity2012=\"15\"",
        "crs:Texture=\"20\"",
        "crs:Dehaze=\"5\"",
        "crs:ToneCurveName2012=\"Custom\"",
        "crs:HasCrop=\"True\"",
        "crs:CropLeft=\"0.05\"",
        "crs:CropTop=\"0.1\"",
        "crs:CropRight=\"0.95\"",
        "crs:CropBottom=\"0.9\"",
        "<rdf:li>38, 26</rdf:li>",
    ] {
        assert!(packet.contains(tag), "packet misses {tag}\n{packet}");
    }
}

#[test]
fn exiftool_reads_every_sidecar_tag() {
    let Some(bin) = Writer::binary_for_tests() else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let sidecar = dir.path().join("DSC00001.xmp");
    write_sidecar(&sidecar, &demo_develop(), Some(&demo_crop())).unwrap();

    let out = Command::new(&bin)
        .args(["-X", "-XMP-crs:all"])
        .arg(&sidecar)
        .output()
        .expect("exiftool -X runs");
    assert!(out.status.success());
    let xml = String::from_utf8(out.stdout).unwrap();
    println!("{xml}");
    // Note: exiftool normalizes unsigned integers (`+10` reads back as
    // `10`); only Exposure2012 and Tint keep their explicit `+` sign.
    for (tag, value) in [
        ("ProcessVersion", "11.0"),
        ("WhiteBalance", "Custom"),
        ("ColorTemperature", "5600"),
        ("Tint", "+5"),
        ("Exposure2012", "+0.70"),
        ("Contrast2012", "10"),
        ("Highlights2012", "-40"),
        ("Shadows2012", "25"),
        ("Whites2012", "10"),
        ("Blacks2012", "-8"),
        ("Clarity2012", "15"),
        ("Texture", "20"),
        ("Dehaze", "5"),
        ("ToneCurveName2012", "Custom"),
        ("HasCrop", "True"),
        ("CropLeft", "0.05"),
        ("CropTop", "0.1"),
        ("CropRight", "0.95"),
        ("CropBottom", "0.9"),
        ("CropAngle", "0"),
    ] {
        assert!(
            xml.contains(&format!("<XMP-crs:{tag}>{value}</XMP-crs:{tag}>")),
            "exiftool misses {tag}={value}\n{xml}"
        );
    }
    assert!(xml.contains("<rdf:li>38, 26</rdf:li>"));
}
