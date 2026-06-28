//! Face detection (SCRFD) + embedding (ArcFace ResNet100) via ONNX Runtime
//! (`ort`). Both models come from the **AuraFace** pack (fal.ai, Apache-2.0),
//! which is intended for commercial use — unlike the InsightFace weights whose
//! research-only training datasets forbid it. The decode/preprocessing code here
//! is our own Rust implementation of the models' (publicly documented) I/O.
//!
//! Pipeline per image: SCRFD detects face boxes (+5 landmarks, ignored) → each
//! box is cropped to 112x112 → ArcFace produces a 512-d embedding, L2-normalized
//! (cosine-ready). Returns one entry per face as `{bbox:[x,y,w,h], embedding,
//! score}`, matching the `/faces` response shape.
//!
//! Loading is fallible and non-panicking: missing model files mean the capability
//! is unavailable and the endpoint returns 503.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

use crate::clip::first_f32_output;

/// SCRFD detector input size (square). 1280 (vs the usual 640) keeps faces in big
/// photos large enough to detect — a 6000px photo resized to 640 shrinks small
/// faces below the detector's reach. SCRFD is fully-convolutional, so a larger
/// input just yields larger feature maps (more compute, better small-face recall).
const DET_SIZE: i64 = 1280;
/// SCRFD feature-map strides.
const STRIDES: [i64; 3] = [8, 16, 32];
/// Anchors per location for SCRFD (2 for the standard models).
const NUM_ANCHORS: usize = 2;
/// Detection score threshold. Kept fairly strict: at 0.5 SCRFD occasionally
/// fires on face-like clutter (e.g. a chandelier) — 0.62 drops those false
/// positives while still keeping real, reasonably-frontal faces.
const SCORE_THRESH: f32 = 0.62;
/// NMS IoU threshold.
const NMS_THRESH: f32 = 0.4;
/// ArcFace (AuraFace) input size.
const ARC_SIZE: u32 = 112;
/// ArcFace (AuraFace) embedding dimension.
const ARC_DIM: usize = 512;

pub struct Faces {
    detector: Mutex<Session>,
    embedder: Mutex<Session>,
    /// Face embedding dimension (512 for AuraFace), reported by `/health`.
    pub dim: usize,
}

fn open_session(path: &Path) -> Result<Session> {
    Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        )?
        .commit_from_file(path)
        .with_context(|| format!("loading ONNX model {}", path.display()))
}

/// A detected face in the (letterboxed) detector space.
struct Detection {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
}

impl Faces {
    pub fn try_load(detection_path: &Path, recognition_path: &Path) -> Result<Option<Self>> {
        if !detection_path.exists() || !recognition_path.exists() {
            return Ok(None);
        }
        let detector = open_session(detection_path)?;
        let embedder = open_session(recognition_path)?;
        Ok(Some(Self {
            detector: Mutex::new(detector),
            embedder: Mutex::new(embedder),
            dim: ARC_DIM,
        }))
    }

    /// Detect faces and embed each one. Returns `(bbox[x,y,w,h], embedding, score)`.
    pub fn detect(&self, bytes: &[u8]) -> Result<Vec<([f32; 4], Vec<f32>, f32)>> {
        let img = image::load_from_memory(bytes)
            .context("decoding image")?
            .to_rgb8();
        let (ow, oh) = img.dimensions();

        // Letterbox to DET_SIZE x DET_SIZE preserving aspect ratio (image top-left).
        let scale = (DET_SIZE as f32 / ow as f32).min(DET_SIZE as f32 / oh as f32);
        let nw = ((ow as f32) * scale).round() as u32;
        let nh = ((oh as f32) * scale).round() as u32;
        let resized = image::imageops::resize(&img, nw.max(1), nh.max(1), FilterType::Triangle);
        let mut canvas =
            image::RgbImage::from_pixel(DET_SIZE as u32, DET_SIZE as u32, image::Rgb([0, 0, 0]));
        image::imageops::overlay(&mut canvas, &resized, 0, 0);

        let detections = self.run_detector(&canvas)?;

        let mut out = Vec::new();
        for d in detections {
            // Map box back to original-image pixel coordinates.
            let x1 = (d.x1 / scale).clamp(0.0, ow as f32);
            let y1 = (d.y1 / scale).clamp(0.0, oh as f32);
            let x2 = (d.x2 / scale).clamp(0.0, ow as f32);
            let y2 = (d.y2 / scale).clamp(0.0, oh as f32);
            let w = (x2 - x1).max(0.0);
            let h = (y2 - y1).max(0.0);
            if w < 1.0 || h < 1.0 {
                continue;
            }
            let emb = self.embed_face(&img, x1, y1, w, h)?;
            out.push(([x1, y1, w, h], emb, d.score));
        }
        Ok(out)
    }

    /// Run SCRFD and return NMS-filtered detections in detector space.
    fn run_detector(&self, canvas: &image::RgbImage) -> Result<Vec<Detection>> {
        // SCRFD preprocessing: (px - 127.5) / 128, CHW, RGB.
        let side = DET_SIZE as usize;
        let plane = side * side;
        let mut data = vec![0f32; 3 * plane];
        for (i, px) in canvas.pixels().enumerate() {
            for c in 0..3 {
                data[c * plane + i] = (px[c] as f32 - 127.5) / 128.0;
            }
        }
        let input = Tensor::from_array((vec![1usize, 3, side, side], data))?;

        let mut sess = self.detector.lock().expect("face detector lock");
        let outputs = sess.run(ort::inputs![input])?;

        // SCRFD emits, per stride, a score map and a bbox-distance map (and
        // landmarks, ignored here). Collect outputs as (shape, data) keyed by
        // their declared name so we can pair scores with boxes by stride.
        let mut named: Vec<(String, Vec<usize>, Vec<f32>)> = Vec::new();
        for (name, _) in outputs.iter() {
            let (shape, vals) = outputs[name].try_extract_tensor::<f32>()?;
            named.push((
                name.to_string(),
                shape.iter().map(|&d| d as usize).collect(),
                vals.to_vec(),
            ));
        }

        let mut dets = decode_scrfd(&named, side as i64);
        nms(&mut dets, NMS_THRESH);
        Ok(dets)
    }

    /// Crop the face box, resize to 112x112, run ArcFace, L2-normalize.
    fn embed_face(
        &self,
        img: &image::RgbImage,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> Result<Vec<f32>> {
        let (iw, ih) = img.dimensions();
        let cx0 = x.max(0.0) as u32;
        let cy0 = y.max(0.0) as u32;
        let cw = (w as u32).min(iw.saturating_sub(cx0)).max(1);
        let ch = (h as u32).min(ih.saturating_sub(cy0)).max(1);
        let crop = image::imageops::crop_imm(img, cx0, cy0, cw, ch).to_image();
        let face = image::imageops::resize(&crop, ARC_SIZE, ARC_SIZE, FilterType::Triangle);

        // ArcFace/AuraFace preprocessing: (px - 127.5) / 127.5, RGB, CHW
        // (InsightFace `get_feat` uses scale 1/127.5, mean 127.5, swapRB=true).
        let plane = (ARC_SIZE * ARC_SIZE) as usize;
        let mut data = vec![0f32; 3 * plane];
        for (i, px) in face.pixels().enumerate() {
            for c in 0..3 {
                data[c * plane + i] = (px[c] as f32 - 127.5) / 127.5;
            }
        }
        let input = Tensor::from_array((
            vec![1usize, 3, ARC_SIZE as usize, ARC_SIZE as usize],
            data,
        ))?;
        let mut sess = self.embedder.lock().expect("face embedder lock");
        let outputs = sess.run(ort::inputs![input])?;
        let (_shape, emb) = first_f32_output(&outputs)?;
        let norm = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
        Ok(if norm > 0.0 {
            emb.iter().map(|v| v / norm).collect()
        } else {
            emb
        })
    }
}

/// Decode SCRFD outputs into detections (detector-space coords).
///
/// SCRFD produces 6 or 9 outputs. We identify, per stride, the score tensor
/// (last dim == NUM_ANCHORS or 1) and the bbox tensor (last dim == 4*NUM_ANCHORS
/// or 4). Pairing is done by descending spatial size: stride 8 has the most
/// anchors, stride 32 the fewest. Landmark tensors (10 / 10*anchors) are ignored.
fn decode_scrfd(named: &[(String, Vec<usize>, Vec<f32>)], input_side: i64) -> Vec<Detection> {
    let mut scores: Vec<&(String, Vec<usize>, Vec<f32>)> = Vec::new();
    let mut bboxes: Vec<&(String, Vec<usize>, Vec<f32>)> = Vec::new();
    for t in named {
        let feat = *t.1.last().unwrap_or(&0);
        if feat == 1 || feat == NUM_ANCHORS {
            scores.push(t);
        } else if feat == 4 || feat == 4 * NUM_ANCHORS {
            bboxes.push(t);
        }
    }
    scores.sort_by_key(|t| std::cmp::Reverse(t.2.len()));
    bboxes.sort_by_key(|t| std::cmp::Reverse(t.2.len()));

    let mut dets = Vec::new();
    for (idx, stride) in STRIDES.iter().enumerate() {
        let (score_t, bbox_t) = match (scores.get(idx), bboxes.get(idx)) {
            (Some(s), Some(b)) => (s, b),
            _ => continue,
        };
        let feat_w = (input_side / stride) as usize;
        let feat_h = feat_w;
        let score = &score_t.2;
        let bbox = &bbox_t.2;

        for y in 0..feat_h {
            for x in 0..feat_w {
                for a in 0..NUM_ANCHORS {
                    let cell = (y * feat_w + x) * NUM_ANCHORS + a;
                    if cell >= score.len() {
                        continue;
                    }
                    let s = score[cell];
                    if s < SCORE_THRESH {
                        continue;
                    }
                    let bi = cell * 4;
                    if bi + 3 >= bbox.len() {
                        continue;
                    }
                    // Anchor center in input space.
                    let acx = (x as f32) * (*stride as f32);
                    let acy = (y as f32) * (*stride as f32);
                    // Distances are stride-scaled (SCRFD distance2bbox).
                    let l = bbox[bi] * (*stride as f32);
                    let t = bbox[bi + 1] * (*stride as f32);
                    let r = bbox[bi + 2] * (*stride as f32);
                    let b = bbox[bi + 3] * (*stride as f32);
                    dets.push(Detection {
                        x1: acx - l,
                        y1: acy - t,
                        x2: acx + r,
                        y2: acy + b,
                        score: s,
                    });
                }
            }
        }
    }
    dets
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let xx1 = a.x1.max(b.x1);
    let yy1 = a.y1.max(b.y1);
    let xx2 = a.x2.min(b.x2);
    let yy2 = a.y2.min(b.y2);
    let w = (xx2 - xx1).max(0.0);
    let h = (yy2 - yy1).max(0.0);
    let inter = w * h;
    let area_a = (a.x2 - a.x1).max(0.0) * (a.y2 - a.y1).max(0.0);
    let area_b = (b.x2 - b.x1).max(0.0) * (b.y2 - b.y1).max(0.0);
    let union = area_a + area_b - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Greedy non-max suppression, in place.
fn nms(dets: &mut Vec<Detection>, thresh: f32) {
    dets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut keep: Vec<Detection> = Vec::new();
    'outer: for d in dets.drain(..) {
        for k in &keep {
            if iou(&d, k) > thresh {
                continue 'outer;
            }
        }
        keep.push(d);
    }
    *dets = keep;
}
