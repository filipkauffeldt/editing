//! tract + YuNet (OpenCV Zoo `face_detection_yunet_2023mar.onnx`) runner.
//!
//! The model is a ~230 KB ONNX file (Apache-2.0, OpenCV Zoo). It is
//! downloaded by hand — see `docs/research/retouch-r0-memo.md` — and is never
//! committed. This mirrors OpenCV's `FaceDetectorYN` preprocessing and
//! postprocessing (modules/objdetect/src/face_detect.cpp): 640×640 BGR uint8
//! input [0, 255], `score = sqrt(cls * obj)` clamped to [0, 1], boxes decoded
//! from anchor deltas per stride, then scaled back to the source image.

use tract_onnx::prelude::*;

use crate::detect_common::{BBox, nms};

/// input resolution of the YuNet model (the 2023mar ONNX is fixed at 640×640)
pub const INPUT: usize = 640;

/// detection strides and their anchor grids
const STRIDES: [usize; 3] = [8, 16, 32];

pub struct Yunet {
    runnable: SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>,
}

impl Yunet {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let model = tract_onnx::onnx().model_for_path(path).map_err(|e| format!("tract model load: {e}"))?;
        let n_out = model.output_outlets().map_err(|e| format!("tract outputs: {e}"))?.len();
        if n_out < 12 {
            return Err(format!("YuNet ONNX has {n_out} outputs, expected 12 (cls/obj/bbox/kps × 3 strides)"));
        }
        let runnable = model.into_optimized().map_err(|e| format!("tract optimize: {e}"))?.into_runnable().map_err(|e| format!("tract runnable: {e}"))?;
        Ok(Yunet { runnable })
    }

    /// Detect faces in an RGB8 image (`rgb`: w*h*3, tightly packed). Boxes are
    /// in image pixels.
    pub fn detect_rgb8(&mut self, rgb: &[u8], w: usize, h: usize, score_threshold: f32, nms_threshold: f32) -> Result<Vec<BBox>, String> {
        let img = image::RgbImage::from_raw(w as u32, h as u32, rgb.to_vec()).ok_or("bad rgb buffer")?;
        let resized = image::imageops::resize(&img, INPUT as u32, INPUT as u32, image::imageops::FilterType::Triangle);
        let mut data = Vec::with_capacity(3 * INPUT * INPUT);
        // YuNet was trained on OpenCV BGR, raw uint8 [0, 255].
        for c in [2usize, 1, 0] {
            for p in resized.pixels() {
                data.push(p[c] as f32);
            }
        }
        let input = tract_ndarray::Array4::from_shape_vec((1, 3, INPUT, INPUT), data).map_err(|e| format!("input tensor: {e}"))?;
        let result = self.runnable.run(tvec!(Tensor::from(input).into())).map_err(|e| format!("tract run: {e}"))?;
        let outs: Vec<(usize, usize, Vec<f32>)> = (0..result.len())
            .map(|i| {
                let v = result.get(i).ok_or_else(|| "missing output".to_string())?;
                let arr = v.to_array_view::<f32>().map_err(|e| format!("output {i}: {e}"))?;
                let cols = arr.shape().last().copied().unwrap_or(0);
                let rows = arr.len() / cols.max(1);
                Ok::<_, String>((cols, rows, arr.iter().copied().collect()))
            })
            .collect::<Result<_, _>>()?;
        // The 2023mar ONNX emits 12 outputs: cls, obj, bbox (4) and kps (10)
        // per stride, each split across the three strides (8/16/32 → 6400/1600/
        // 400 anchors). Reassemble by stride, largest first.
        let pick = |cols: usize| -> Vec<Vec<f32>> {
            let mut v: Vec<Vec<f32>> = outs
                .iter()
                .filter(|(c, _, _)| *c == cols)
                .map(|(_, rows, d)| d.chunks_exact(cols).take(*rows).flatten().copied().collect::<Vec<f32>>())
                .collect();
            v.sort_by(|a, b| b.len().cmp(&a.len()));
            v
        };
        let scores = pick(1); // [cls8, obj8, cls16, obj16, cls32, obj32]
        let bbox = pick(4); // [bbox8, bbox16, bbox32]
        if scores.len() < 6 || bbox.len() < 3 {
            return Err(format!(
                "YuNet outputs: {} score tensors, {} bbox tensors (expected 6 and 3)",
                scores.len(),
                bbox.len()
            ));
        }
        let scale_x = w as f32 / INPUT as f32;
        let scale_y = h as f32 / INPUT as f32;
        let mut dets = Vec::new();
        for (g, &stride) in STRIDES.iter().enumerate() {
            let cls = &scores[2 * g];
            let obj = &scores[2 * g + 1];
            let b = &bbox[g];
            let cols = INPUT / stride;
            let count = cols * cols;
            if cls.len() != count || obj.len() != count || b.len() != count * 4 {
                return Err(format!(
                    "YuNet stride {stride}: expected {count} anchors, got cls {} obj {} bbox {}",
                    cls.len(),
                    obj.len(),
                    b.len() / 4
                ));
            }
            for idx in 0..count {
                let c = (idx % cols) as f32;
                let r = (idx / cols) as f32;
                let s = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                if s < score_threshold {
                    continue;
                }
                let cx = (c + b[idx * 4]) * stride as f32;
                let cy = (r + b[idx * 4 + 1]) * stride as f32;
                let bw = b[idx * 4 + 2].exp() * stride as f32;
                let bh = b[idx * 4 + 3].exp() * stride as f32;
                dets.push(BBox { x: (cx - bw / 2.0) * scale_x, y: (cy - bh / 2.0) * scale_y, w: bw * scale_x, h: bh * scale_y, score: s });
            }
        }
        Ok(nms(dets, nms_threshold))
    }
}
