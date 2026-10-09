//! The one table that maps LightCraft control ids to Lightroom `crs:` keys.
//!
//! Writing the sidecar and reading it back both go through it, so a control
//! round-trips if and only if it has a row here. Key names come from
//! LightCraft's own `crs:` reader.

use std::sync::OnceLock;

const BANDS: [&str; 8] = [
    "red", "orange", "yellow", "green", "aqua", "blue", "purple", "magenta",
];

const FIXED: &[(&str, &str)] = &[
    ("light.exposure", "Exposure2012"),
    ("light.contrast", "Contrast2012"),
    ("light.highlights", "Highlights2012"),
    ("light.shadows", "Shadows2012"),
    ("light.whites", "Whites2012"),
    ("light.blacks", "Blacks2012"),
    ("effects.clarity", "Clarity2012"),
    ("effects.texture", "Texture"),
    ("effects.dehaze", "Dehaze"),
    ("curve.shadows", "ParametricShadows"),
    ("curve.darks", "ParametricDarks"),
    ("curve.lights", "ParametricLights"),
    ("curve.highlights", "ParametricHighlights"),
    ("curve.splitShadows", "ParametricShadowSplit"),
    ("curve.splitMid", "ParametricMidtoneSplit"),
    ("curve.splitHighlights", "ParametricHighlightSplit"),
    ("color.vibrance", "Vibrance"),
    ("color.saturation", "Saturation"),
    ("grading.shadows.hue", "SplitToningShadowHue"),
    ("grading.shadows.sat", "SplitToningShadowSaturation"),
    ("grading.shadows.lum", "ColorGradeShadowLum"),
    ("grading.midtones.hue", "ColorGradeMidtoneHue"),
    ("grading.midtones.sat", "ColorGradeMidtoneSat"),
    ("grading.midtones.lum", "ColorGradeMidtoneLum"),
    ("grading.highlights.hue", "SplitToningHighlightHue"),
    ("grading.highlights.sat", "SplitToningHighlightSaturation"),
    ("grading.highlights.lum", "ColorGradeHighlightLum"),
    ("grading.global.hue", "ColorGradeGlobalHue"),
    ("grading.global.sat", "ColorGradeGlobalSat"),
    ("grading.global.lum", "ColorGradeGlobalLum"),
    ("grading.blending", "ColorGradeBlending"),
    ("grading.balance", "SplitToningBalance"),
    ("detail.sharpenAmount", "Sharpness"),
    ("detail.sharpenRadius", "SharpenRadius"),
    ("detail.sharpenDetail", "SharpenDetail"),
    ("detail.sharpenMasking", "SharpenEdgeMasking"),
    ("detail.nrLuminance", "LuminanceSmoothing"),
    ("detail.nrDetail", "LuminanceNoiseReductionDetail"),
    ("detail.nrContrast", "LuminanceNoiseReductionContrast"),
    ("detail.nrColor", "ColorNoiseReduction"),
    ("detail.nrColorDetail", "ColorNoiseReductionDetail"),
    ("detail.nrColorSmoothness", "ColorNoiseReductionSmoothness"),
    ("vignette.amount", "PostCropVignetteAmount"),
    ("vignette.midpoint", "PostCropVignetteMidpoint"),
    ("vignette.roundness", "PostCropVignetteRoundness"),
    ("vignette.feather", "PostCropVignetteFeather"),
    ("vignette.highlights", "PostCropVignetteHighlightContrast"),
    ("grain.amount", "GrainAmount"),
    ("grain.size", "GrainSize"),
    ("grain.roughness", "GrainFrequency"),
    ("optics.distortion", "LensManualDistortionAmount"),
    ("optics.vignetting", "VignetteAmount"),
    ("optics.vignettingMidpoint", "VignetteMidpoint"),
    ("optics.defringePurple", "DefringePurpleAmount"),
    ("optics.defringePurpleHueLo", "DefringePurpleHueLo"),
    ("optics.defringePurpleHueHi", "DefringePurpleHueHi"),
    ("optics.defringeGreen", "DefringeGreenAmount"),
    ("optics.defringeGreenHueLo", "DefringeGreenHueLo"),
    ("optics.defringeGreenHueHi", "DefringeGreenHueHi"),
    ("geometry.vertical", "PerspectiveVertical"),
    ("geometry.horizontal", "PerspectiveHorizontal"),
    ("geometry.rotate", "PerspectiveRotate"),
    ("geometry.aspect", "PerspectiveAspect"),
    ("geometry.scale", "PerspectiveScale"),
    ("geometry.offsetX", "PerspectiveX"),
    ("geometry.offsetY", "PerspectiveY"),
    ("calibration.shadowsTint", "ShadowTint"),
    ("calibration.redHue", "RedHue"),
    ("calibration.redSat", "RedSaturation"),
    ("calibration.greenHue", "GreenHue"),
    ("calibration.greenSat", "GreenSaturation"),
    ("calibration.blueHue", "BlueHue"),
    ("calibration.blueSat", "BlueSaturation"),
];

/// Every `(control id, crs key)` pair: the fixed rows plus the 24 colour
/// mixer rows (`mixer.red.hue` -> `HueAdjustmentRed`, ...).
pub fn crs_controls() -> &'static [(String, String)] {
    static TABLE: OnceLock<Vec<(String, String)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table: Vec<(String, String)> = FIXED
            .iter()
            .map(|(id, key)| ((*id).to_owned(), (*key).to_owned()))
            .collect();
        for band in BANDS {
            let mut chars = band.chars();
            let name: String = chars
                .next()
                .map(|c| c.to_ascii_uppercase())
                .into_iter()
                .chain(chars)
                .collect();
            for (field, prefix) in [
                ("hue", "HueAdjustment"),
                ("sat", "SaturationAdjustment"),
                ("lum", "LuminanceAdjustment"),
            ] {
                table.push((format!("mixer.{band}.{field}"), format!("{prefix}{name}")));
            }
        }
        table
    })
}
