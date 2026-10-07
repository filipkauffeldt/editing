//! Shared detector plumbing for the bake-off: box math, NMS, IoU matching,
//! WIDER-Face ground-truth parsing, face-size buckets and recall reporting.

#[derive(Clone, Copy, Debug)]
pub struct BBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
}

impl BBox {
    pub fn long_edge(&self) -> f32 {
        self.w.max(self.h)
    }
    pub fn area(&self) -> f32 {
        self.w * self.h
    }
}

pub fn iou(a: &BBox, b: &BBox) -> f32 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let union = a.area() + b.area() - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Greedy NMS by descending score, IoU threshold.
pub fn nms(mut boxes: Vec<BBox>, threshold: f32) -> Vec<BBox> {
    boxes.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut keep: Vec<BBox> = Vec::new();
    for b in boxes {
        if !keep.iter().any(|k| iou(k, &b) > threshold) {
            keep.push(b);
        }
    }
    keep
}

/// Face-size buckets by long edge in image pixels (WIDER-style, coarse).
#[derive(Clone, Copy)]
pub enum Bucket {
    /// long edge < 24 px (at/below the ~10-20 px detector minimum)
    Tiny,
    /// 24-50 px
    Small,
    /// 50-150 px
    Medium,
    /// > 150 px
    Large,
}

impl Bucket {
    pub fn of(long_edge: f32) -> Bucket {
        if long_edge < 24.0 {
            Bucket::Tiny
        } else if long_edge < 50.0 {
            Bucket::Small
        } else if long_edge <= 150.0 {
            Bucket::Medium
        } else {
            Bucket::Large
        }
    }
}

pub const BUCKET_NAMES: [&str; 4] = ["tiny (<24px)", "small (24-50px)", "medium (50-150px)", "large (>150px)"];

/// WIDER-Face ground truth: image path → face boxes (x, y, w, h), ignoring
/// the invalid/parody flags.
pub fn parse_wider_gt(text: &str) -> Option<Vec<(String, Vec<(i32, i32, i32, i32)>)>> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let path = (*lines.get(i)?).to_string();
        i += 1;
        let n: usize = lines.get(i)?.parse().ok()?;
        i += 1;
        let mut boxes = Vec::new();
        for _ in 0..n {
            let parts: Vec<i32> = lines.get(i)?.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            i += 1;
            let (x, y, w, h) = (*parts.first()?, *parts.get(1)?, *parts.get(2)?, *parts.get(3)?);
            if w > 0 && h > 0 {
                boxes.push((x, y, w, h));
            }
        }
        out.push((path, boxes));
    }
    Some(out)
}

/// Match detections against ground truth (one GT face = one detection, IoU
/// >= 0.5). Returns per-bucket (matched, total) and the number of unmatched
/// detections (false positives).
pub fn evaluate(gt: &[(i32, i32, i32, i32)], dets: &[BBox], iou_threshold: f32) -> ([(usize, usize); 4], usize) {
    let mut buckets = [(0usize, 0usize); 4];
    let mut taken = vec![false; dets.len()];
    let mut fps = 0usize;
    for (x, y, w, h) in gt {
        let g = BBox { x: *x as f32, y: *y as f32, w: *w as f32, h: *h as f32, score: 0.0 };
        let b = Bucket::of(g.long_edge()) as usize;
        buckets[b].1 += 1;
        let mut best: Option<(f32, usize)> = None;
        for (di, d) in dets.iter().enumerate() {
            if taken[di] {
                continue;
            }
            let v = iou(d, &g);
            if v >= iou_threshold && best.is_none_or(|(bv, _)| v > bv) {
                best = Some((v, di));
            }
        }
        if let Some((_, di)) = best {
            taken[di] = true;
            buckets[b].0 += 1;
        }
    }
    for (di, d) in dets.iter().enumerate() {
        if !taken[di] {
            fps += usize::from(d.score > 0.0);
        }
    }
    (buckets, fps)
}

/// Per-image result of the bake-off.
pub struct ImageResult {
    pub detections: usize,
    pub buckets: [(usize, usize); 4],
    pub false_positives: usize,
    pub ms: f64,
}

pub fn format_bucket_table(rows: &[ImageResult], total_ms: f64) -> String {
    let mut s = String::new();
    for bi in 0..4 {
        let matched: usize = rows.iter().map(|r| r.buckets[bi].0).sum();
        let total: usize = rows.iter().map(|r| r.buckets[bi].1).sum();
        let recall = if total > 0 { matched as f64 / total as f64 * 100.0 } else { 0.0 };
        s.push_str(&format!("  {:<18} {matched:>5}/{total:<5} recall {recall:>6.2} %\n", BUCKET_NAMES[bi]));
    }
    let g_matched: usize = rows.iter().map(|r| r.buckets.iter().map(|x| x.0).sum::<usize>()).sum();
    let g_total: usize = rows.iter().map(|r| r.buckets.iter().map(|x| x.1).sum::<usize>()).sum();
    let fps: usize = rows.iter().map(|r| r.false_positives).sum();
    let recall = if g_total > 0 { g_matched as f64 / g_total as f64 * 100.0 } else { 0.0 };
    s.push_str(&format!(
        "  overall          {g_matched:>5}/{g_total:<5} recall {recall:.2} %, false positives {fps}, CPU {total_ms:.1} ms over {} images\n",
        rows.len()
    ));
    s
}
