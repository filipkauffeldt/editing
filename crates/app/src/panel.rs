//! The develop panel as data: one table of sections and rows in Lightroom
//! Classic's order, with every range, step and default read from
//! LightCraft's own control table. Adding a control is one row in
//! [`PANEL`]; the widgets, resets, sidecar mapping and tests follow.

use edit_core::DevelopSettings;
use lightcraft_develop::{ControlSpec, ToneCurve, WbMode, controls};

/// One row of a panel section.
#[derive(Clone, Copy, Debug)]
pub enum Item {
    /// A sub-heading inside a section (for example `Hue` in the mixer).
    Heading(&'static str),
    /// A numeric control: LightCraft control id and the label shown.
    Slider(&'static str, &'static str),
    /// An on/off setting.
    Toggle(&'static str, &'static str),
    /// The white balance preset picker.
    WhiteBalanceMode,
}

/// A collapsible group of rows.
#[derive(Clone, Copy, Debug)]
pub struct PanelSection {
    pub key: &'static str,
    pub title: &'static str,
    pub open: bool,
    pub items: &'static [Item],
}

use Item::{Heading, Slider, Toggle, WhiteBalanceMode};

/// Lightroom's order. Sections LightCraft cannot render yet are left out;
/// see [`SKIPPED`].
pub static PANEL: &[PanelSection] = &[
    PanelSection {
        key: "white-balance",
        title: "White balance",
        open: true,
        items: &[
            WhiteBalanceMode,
            Slider("wb.temp", "Temp"),
            Slider("wb.tint", "Tint"),
        ],
    },
    PanelSection {
        key: "tone",
        title: "Tone",
        open: true,
        items: &[
            Slider("light.exposure", "Exposure"),
            Slider("light.contrast", "Contrast"),
            Slider("light.highlights", "Highlights"),
            Slider("light.shadows", "Shadows"),
            Slider("light.whites", "Whites"),
            Slider("light.blacks", "Blacks"),
        ],
    },
    PanelSection {
        key: "presence",
        title: "Presence",
        open: true,
        items: &[
            Slider("effects.texture", "Texture"),
            Slider("effects.clarity", "Clarity"),
            Slider("effects.dehaze", "Dehaze"),
            Slider("color.vibrance", "Vibrance"),
            Slider("color.saturation", "Saturation"),
        ],
    },
    PanelSection {
        key: "tone-curve",
        title: "Tone curve",
        open: false,
        items: &[
            Slider("curve.highlights", "Highlights"),
            Slider("curve.lights", "Lights"),
            Slider("curve.darks", "Darks"),
            Slider("curve.shadows", "Shadows"),
            Heading("Region splits"),
            Slider("curve.splitShadows", "Shadows"),
            Slider("curve.splitMid", "Midtones"),
            Slider("curve.splitHighlights", "Highlights"),
        ],
    },
    PanelSection {
        key: "color-mixer",
        title: "Color mixer",
        open: false,
        items: &[
            Heading("Hue"),
            Slider("mixer.red.hue", "Red"),
            Slider("mixer.orange.hue", "Orange"),
            Slider("mixer.yellow.hue", "Yellow"),
            Slider("mixer.green.hue", "Green"),
            Slider("mixer.aqua.hue", "Aqua"),
            Slider("mixer.blue.hue", "Blue"),
            Slider("mixer.purple.hue", "Purple"),
            Slider("mixer.magenta.hue", "Magenta"),
            Heading("Saturation"),
            Slider("mixer.red.sat", "Red"),
            Slider("mixer.orange.sat", "Orange"),
            Slider("mixer.yellow.sat", "Yellow"),
            Slider("mixer.green.sat", "Green"),
            Slider("mixer.aqua.sat", "Aqua"),
            Slider("mixer.blue.sat", "Blue"),
            Slider("mixer.purple.sat", "Purple"),
            Slider("mixer.magenta.sat", "Magenta"),
            Heading("Luminance"),
            Slider("mixer.red.lum", "Red"),
            Slider("mixer.orange.lum", "Orange"),
            Slider("mixer.yellow.lum", "Yellow"),
            Slider("mixer.green.lum", "Green"),
            Slider("mixer.aqua.lum", "Aqua"),
            Slider("mixer.blue.lum", "Blue"),
            Slider("mixer.purple.lum", "Purple"),
            Slider("mixer.magenta.lum", "Magenta"),
        ],
    },
    PanelSection {
        key: "color-grading",
        title: "Color grading",
        open: false,
        items: &[
            Heading("Shadows"),
            Slider("grading.shadows.hue", "Hue"),
            Slider("grading.shadows.sat", "Saturation"),
            Slider("grading.shadows.lum", "Luminance"),
            Heading("Midtones"),
            Slider("grading.midtones.hue", "Hue"),
            Slider("grading.midtones.sat", "Saturation"),
            Slider("grading.midtones.lum", "Luminance"),
            Heading("Highlights"),
            Slider("grading.highlights.hue", "Hue"),
            Slider("grading.highlights.sat", "Saturation"),
            Slider("grading.highlights.lum", "Luminance"),
            Heading("Global"),
            Slider("grading.global.hue", "Hue"),
            Slider("grading.global.sat", "Saturation"),
            Slider("grading.global.lum", "Luminance"),
            Heading("Mix"),
            Slider("grading.blending", "Blending"),
            Slider("grading.balance", "Balance"),
        ],
    },
    PanelSection {
        key: "detail",
        title: "Detail",
        open: false,
        items: &[
            Heading("Sharpening"),
            Slider("detail.sharpenAmount", "Amount"),
            Slider("detail.sharpenMasking", "Masking"),
            Heading("Noise reduction"),
            Slider("detail.nrLuminance", "Luminance"),
            Slider("detail.nrDetail", "Detail"),
            Slider("detail.nrColor", "Color"),
            Slider("detail.nrColorDetail", "Color detail"),
            Slider("detail.nrColorSmoothness", "Smoothness"),
        ],
    },
    PanelSection {
        key: "effects",
        title: "Effects",
        open: false,
        items: &[
            Heading("Vignette"),
            Slider("vignette.amount", "Amount"),
            Slider("vignette.midpoint", "Midpoint"),
            Slider("vignette.roundness", "Roundness"),
            Slider("vignette.feather", "Feather"),
            Slider("vignette.highlights", "Highlights"),
            Heading("Grain"),
            Slider("grain.amount", "Amount"),
            Slider("grain.size", "Size"),
            Slider("grain.roughness", "Roughness"),
        ],
    },
    PanelSection {
        key: "calibration",
        title: "Calibration",
        open: false,
        items: &[
            Slider("calibration.shadowsTint", "Shadows tint"),
            Heading("Red primary"),
            Slider("calibration.redHue", "Hue"),
            Slider("calibration.redSat", "Saturation"),
            Heading("Green primary"),
            Slider("calibration.greenHue", "Hue"),
            Slider("calibration.greenSat", "Saturation"),
            Heading("Blue primary"),
            Slider("calibration.blueHue", "Hue"),
            Slider("calibration.blueSat", "Saturation"),
        ],
    },
    PanelSection {
        key: "lens-corrections",
        title: "Lens corrections",
        open: false,
        items: &[
            Toggle("optics.lensProfile", "Use lens profile"),
            Toggle("optics.removeCa", "Remove chromatic aberration"),
            Slider("optics.profileDistortion", "Distortion amount"),
            Slider("optics.profileVignetting", "Vignetting amount"),
            Heading("Manual"),
            Slider("optics.distortion", "Distortion"),
            Slider("optics.vignetting", "Vignetting"),
            Slider("optics.vignettingMidpoint", "Midpoint"),
            Slider("optics.caRed", "Red and cyan fringe"),
            Slider("optics.caBlue", "Blue and yellow fringe"),
            Heading("Defringe"),
            Slider("optics.defringePurple", "Purple amount"),
            Slider("optics.defringePurpleHueLo", "Purple hue low"),
            Slider("optics.defringePurpleHueHi", "Purple hue high"),
            Slider("optics.defringeGreen", "Green amount"),
            Slider("optics.defringeGreenHueLo", "Green hue low"),
            Slider("optics.defringeGreenHueHi", "Green hue high"),
        ],
    },
    PanelSection {
        key: "transform",
        title: "Transform",
        open: false,
        items: &[
            Slider("geometry.vertical", "Vertical"),
            Slider("geometry.horizontal", "Horizontal"),
            Slider("geometry.rotate", "Rotate"),
            Slider("geometry.aspect", "Aspect"),
            Slider("geometry.scale", "Scale"),
            Slider("geometry.offsetX", "X offset"),
            Slider("geometry.offsetY", "Y offset"),
        ],
    },
];

/// LightCraft controls the panel leaves out on purpose, with the reason.
/// The table test fails when a control is in neither [`PANEL`] nor here.
pub static SKIPPED: &[(&str, &str)] = &[
    ("profile.", "camera profile amount: no profile picker yet"),
    ("bw.", "black and white mixer: no treatment switch yet"),
    ("pointColor.", "point color: needs an on-image picker"),
    ("redEye.", "red eye: needs an on-image picker"),
    ("crop.", "straighten belongs to the crop tool"),
    ("detail.sharpenRadius", "LightCraft's render ignores it"),
    ("detail.sharpenDetail", "LightCraft's render ignores it"),
    ("detail.nrContrast", "LightCraft's render ignores it"),
    ("curve.refineSaturation", "advanced curve option"),
];

/// Every slider row in panel order: `(control id, label)`.
pub fn sliders() -> impl Iterator<Item = (&'static str, &'static str)> {
    PANEL.iter().flat_map(|section| {
        section.items.iter().filter_map(|item| match item {
            Slider(id, label) => Some((*id, *label)),
            _ => None,
        })
    })
}

/// LightCraft's spec for a panel control (range, step, default, decimals).
pub fn spec(id: &str) -> Option<&'static ControlSpec> {
    controls::find(id)
}

pub fn section(key: &str) -> Option<&'static PanelSection> {
    PANEL.iter().find(|section| section.key == key)
}

/// Read an on/off setting.
pub fn toggle_get(settings: &DevelopSettings, id: &str) -> bool {
    match id {
        "optics.lensProfile" => settings.optics.lens_profile,
        "optics.removeCa" => settings.optics.remove_ca,
        _ => false,
    }
}

pub fn toggle_set(settings: &mut DevelopSettings, id: &str, on: bool) {
    match id {
        "optics.lensProfile" => settings.optics.lens_profile = on,
        "optics.removeCa" => settings.optics.remove_ca = on,
        _ => {}
    }
}

/// Moving temp or tint takes the white balance off its preset.
pub fn set_slider(settings: &mut DevelopSettings, id: &str, value: f64) {
    controls::set(settings, id, value);
    if matches!(id, "wb.temp" | "wb.tint") {
        settings.wb.mode = WbMode::Custom;
    }
}

/// Put one section back to its defaults.
pub fn reset_section(settings: &mut DevelopSettings, key: &str) {
    let Some(section) = section(key) else { return };
    for item in section.items {
        match item {
            Slider(id, _) => {
                if let Some(spec) = spec(id) {
                    controls::set(settings, id, spec.default);
                }
            }
            Toggle(id, _) => toggle_set(settings, id, false),
            WhiteBalanceMode => settings.wb.mode = WbMode::AsShot,
            Heading(_) => {}
        }
    }
    if key == "tone-curve" {
        settings.curve = ToneCurve::default();
    }
    if key == "white-balance" {
        settings.wb = DevelopSettings::default().wb;
    }
}

/// Put every panel control back to its default.
pub fn reset_all(settings: &mut DevelopSettings) {
    for section in PANEL {
        reset_section(settings, section.key);
    }
}

/// Whether a section differs from its defaults (enables its reset).
pub fn section_is_edited(settings: &DevelopSettings, key: &str) -> bool {
    let mut reset = settings.clone();
    reset_section(&mut reset, key);
    reset != *settings
}

/// A preset: the controls it sets and nothing else.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresetValues {
    pub values: Vec<(String, f64)>,
    pub white_balance: Option<WbMode>,
}

impl PresetValues {
    /// The controls of `settings` that differ from the defaults.
    pub fn from_settings(settings: &DevelopSettings) -> Self {
        let defaults = DevelopSettings::default();
        let values = sliders()
            .filter_map(|(id, _)| {
                let value = controls::get(settings, id)?;
                (value != controls::get(&defaults, id)?).then(|| (id.to_owned(), value))
            })
            .collect();
        let white_balance = (settings.wb.mode != WbMode::AsShot).then_some(settings.wb.mode);
        Self {
            values,
            white_balance,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.white_balance.is_none()
    }

    /// Set the preset's controls on `settings`; everything else is kept.
    pub fn apply(&self, settings: &mut DevelopSettings) {
        for (id, value) in &self.values {
            set_slider(settings, id, *value);
        }
        if let Some(mode) = self.white_balance {
            settings.wb.mode = mode;
        }
    }
}

/// A starter preset shipped with the app.
pub struct BuiltinPreset {
    pub name: &'static str,
    pub values: &'static [(&'static str, f64)],
}

impl BuiltinPreset {
    pub fn to_values(&self) -> PresetValues {
        PresetValues {
            values: self
                .values
                .iter()
                .map(|(id, v)| ((*id).to_owned(), *v))
                .collect(),
            white_balance: None,
        }
    }
}

pub static BUILTIN_PRESETS: &[BuiltinPreset] = &[
    BuiltinPreset {
        name: "Punchy",
        values: &[
            ("light.contrast", 25.0),
            ("effects.clarity", 15.0),
            ("color.vibrance", 30.0),
            ("color.saturation", 10.0),
        ],
    },
    BuiltinPreset {
        name: "Soft portrait",
        values: &[
            ("light.contrast", -15.0),
            ("light.highlights", -20.0),
            ("light.shadows", 20.0),
            ("effects.texture", -20.0),
            ("effects.clarity", -15.0),
        ],
    },
    BuiltinPreset {
        name: "Faded film",
        values: &[
            ("light.contrast", -15.0),
            ("light.blacks", 30.0),
            ("color.saturation", -15.0),
            ("grain.amount", 25.0),
        ],
    },
    BuiltinPreset {
        name: "Moody",
        values: &[
            ("light.exposure", -0.3),
            ("light.contrast", 20.0),
            ("light.highlights", -40.0),
            ("light.shadows", 10.0),
            ("color.saturation", -10.0),
            ("effects.dehaze", 10.0),
            ("vignette.amount", -25.0),
        ],
    },
    BuiltinPreset {
        name: "Crisp",
        values: &[
            ("detail.sharpenAmount", 60.0),
            ("detail.sharpenMasking", 20.0),
            ("effects.texture", 15.0),
            ("effects.clarity", 10.0),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_row_names_a_lightcraft_control_and_none_repeat() {
        let mut seen = HashSet::new();
        for (id, label) in sliders() {
            let spec = spec(id).unwrap_or_else(|| panic!("{id} is not a LightCraft control"));
            assert!(!label.is_empty());
            assert!(seen.insert(id), "{id} appears twice");
            assert!(spec.min < spec.max && spec.step > 0.0, "{id}");
            assert!(spec.min <= spec.default && spec.default <= spec.max, "{id}");
            let defaults = DevelopSettings::default();
            assert_eq!(controls::get(&defaults, id), Some(spec.default), "{id}");
        }
        for section in PANEL {
            for item in section.items {
                if let Toggle(id, _) = item {
                    let mut s = DevelopSettings::default();
                    assert!(!toggle_get(&s, id), "{id}");
                    toggle_set(&mut s, id, true);
                    assert!(toggle_get(&s, id), "{id} is not wired");
                }
            }
        }
        let keys: HashSet<_> = PANEL.iter().map(|s| s.key).collect();
        assert_eq!(keys.len(), PANEL.len());
    }

    #[test]
    fn every_lightcraft_control_is_shown_or_skipped_with_a_reason() {
        let shown: HashSet<_> = sliders().map(|(id, _)| id).collect();
        for spec in lightcraft_develop::CONTROLS {
            let skipped = SKIPPED
                .iter()
                .any(|(prefix, _)| spec.id.starts_with(prefix));
            assert!(
                shown.contains(spec.id) || skipped,
                "{} is in neither the panel nor SKIPPED",
                spec.id
            );
        }
    }

    #[test]
    fn open_by_default_is_basic_only() {
        let open: Vec<_> = PANEL.iter().filter(|s| s.open).map(|s| s.key).collect();
        assert_eq!(open, ["white-balance", "tone", "presence"]);
    }

    #[test]
    fn panel_follows_lightroom_order() {
        let keys: Vec<_> = PANEL.iter().map(|s| s.key).collect();
        assert_eq!(
            &keys[..4],
            ["white-balance", "tone", "presence", "tone-curve"]
        );
        assert_eq!(keys.last(), Some(&"transform"));
    }

    #[test]
    fn preset_sets_only_its_controls_and_keeps_the_rest() {
        let mut settings = DevelopSettings::default();
        controls::set(&mut settings, "light.exposure", 1.0);
        controls::set(&mut settings, "light.contrast", -50.0);
        let preset = BuiltinPreset {
            name: "t",
            values: &[("light.contrast", 20.0), ("color.vibrance", 40.0)],
        };
        preset.to_values().apply(&mut settings);
        assert_eq!(controls::get(&settings, "light.exposure"), Some(1.0));
        assert_eq!(controls::get(&settings, "light.contrast"), Some(20.0));
        assert_eq!(controls::get(&settings, "color.vibrance"), Some(40.0));
    }

    #[test]
    fn preset_values_clamp_and_taking_temp_makes_wb_custom() {
        let values = PresetValues {
            values: vec![("light.contrast".into(), 900.0), ("wb.temp".into(), 7000.0)],
            white_balance: None,
        };
        let mut settings = DevelopSettings::default();
        values.apply(&mut settings);
        assert_eq!(controls::get(&settings, "light.contrast"), Some(100.0));
        assert_eq!(settings.wb.mode, WbMode::Custom);
    }

    #[test]
    fn builtin_presets_are_valid_and_change_something() {
        for preset in BUILTIN_PRESETS {
            let mut settings = DevelopSettings::default();
            for (id, value) in preset.values {
                let spec = spec(id).unwrap_or_else(|| panic!("{}: {id}", preset.name));
                assert_eq!(
                    spec.clamp(*value),
                    *value,
                    "{}: {id} out of range",
                    preset.name
                );
            }
            preset.to_values().apply(&mut settings);
            assert!(!settings.is_unedited(), "{}", preset.name);
        }
    }

    #[test]
    fn saved_preset_is_the_difference_from_defaults() {
        let mut settings = DevelopSettings::default();
        controls::set(&mut settings, "effects.texture", 30.0);
        controls::set(&mut settings, "vignette.amount", -20.0);
        let saved = PresetValues::from_settings(&settings);
        assert_eq!(saved.values.len(), 2);
        let mut other = DevelopSettings::default();
        controls::set(&mut other, "light.exposure", 0.5);
        saved.apply(&mut other);
        assert_eq!(controls::get(&other, "light.exposure"), Some(0.5));
        assert_eq!(controls::get(&other, "effects.texture"), Some(30.0));
    }

    #[test]
    fn section_and_global_reset() {
        let mut settings = DevelopSettings::default();
        controls::set(&mut settings, "light.exposure", 1.0);
        controls::set(&mut settings, "grain.amount", 30.0);
        settings.optics.lens_profile = true;
        assert!(section_is_edited(&settings, "tone"));
        assert!(!section_is_edited(&settings, "calibration"));
        reset_section(&mut settings, "tone");
        assert_eq!(controls::get(&settings, "light.exposure"), Some(0.0));
        assert_eq!(controls::get(&settings, "grain.amount"), Some(30.0));
        reset_all(&mut settings);
        assert!(settings.is_unedited());
    }
}
