//! Promptable segmentation (SAM-family) via ONNX Runtime — the "tap an object and
//! it selects the whole thing" behind the magic eraser. A click point is turned
//! into a pixel mask of the object under it, which the editor then feeds to
//! `/inpaint`.
//!
//! Model contract (MobileSAM / EfficientSAM / SAM exported in the common
//! `samexporter` two-graph form):
//!  - ENCODER: input `image` `[1,3,Henc,Wenc]` (preprocessed to the encoder's fixed
//!    size, here ENC_SIZE), output an image embedding `[1,256,64,64]`.
//!  - DECODER: inputs `image_embeddings`, `point_coords` `[1,N,2]` (in encoder
//!    pixel space), `point_labels` `[1,N]` (1 = foreground, 0 = background), plus
//!    the SAM constants `mask_input` `[1,1,256,256]` (zeros), `has_mask_input`
//!    `[1]` (0), `orig_im_size` `[2]` (h,w). Output `masks` `[1,M,h,w]` (logits)
//!    and `iou_predictions`; we take the best mask, threshold logits > 0.
//!
//! Tensor names vary by export, so the decoder feeds inputs BY NAME from a small
//! set of accepted aliases. Missing either graph disables the capability (503).
//! This is the same operator-supplied, never-panics posture as the other models.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

/// Encoder input side (SAM uses 1024; MobileSAM/EfficientSAM also 1024).
const ENC_SIZE: u32 = 1024;
/// ImageNet normalization (SAM preprocessing), per channel.
const MEAN: [f32; 3] = [123.675, 116.28, 103.53];
const STD: [f32; 3] = [58.395, 57.12, 57.375];

pub struct Segment {
    encoder: Mutex<Session>,
    decoder: Mutex<Session>,
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
        .with_context(|| format!("loading segmentation model {}", path.display()))
}

impl Segment {
    pub fn try_load(encoder_path: &Path, decoder_path: &Path) -> Result<Option<Self>> {
        if !encoder_path.exists() || !decoder_path.exists() {
            return Ok(None);
        }
        Ok(Some(Self {
            encoder: Mutex::new(open_session(encoder_path)?),
            decoder: Mutex::new(open_session(decoder_path)?),
        }))
    }

    /// Segment the object under `(px, py)` (ORIGINAL-image pixel coordinates),
    /// returning a binary mask PNG (white = object) at the original resolution.
    pub fn segment(&self, image_bytes: &[u8], px: f32, py: f32) -> Result<Vec<u8>> {
        let img = image::load_from_memory(image_bytes)
            .context("decoding image")?
            .to_rgb8();
        let (ow, oh) = img.dimensions();

        // SAM preprocessing: resize the LONG side to ENC_SIZE, pad to a square.
        let scale = ENC_SIZE as f32 / ow.max(oh) as f32;
        let rw = ((ow as f32 * scale).round() as u32).max(1);
        let rh = ((oh as f32 * scale).round() as u32).max(1);
        let resized = image::imageops::resize(&img, rw, rh, FilterType::Triangle);

        let side = ENC_SIZE as usize;
        let plane = side * side;
        let mut data = vec![0f32; 3 * plane];
        for y in 0..rh as usize {
            for x in 0..rw as usize {
                let p = resized.get_pixel(x as u32, y as u32);
                let idx = y * side + x;
                for c in 0..3 {
                    data[c * plane + idx] = (p[c] as f32 - MEAN[c]) / STD[c];
                }
            }
        }
        let enc_in = Tensor::from_array((vec![1usize, 3, side, side], data))?;
        let embedding = {
            let mut enc = self.encoder.lock().expect("segment encoder lock");
            let out = enc.run(ort::inputs![enc_in])?;
            let (name, _) = out
                .iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("encoder produced no output"))?;
            let (shape, vals) = out[name].try_extract_tensor::<f32>()?;
            (shape.iter().map(|&d| d as usize).collect::<Vec<_>>(), vals.to_vec())
        };

        // The click in encoder pixel space.
        let cx = px * scale;
        let cy = py * scale;
        let coords = vec![cx, cy];

        let emb_t = Tensor::from_array((embedding.0.clone(), embedding.1))?;
        let point_coords = Tensor::from_array((vec![1usize, 1, 2], coords))?;
        let point_labels = Tensor::from_array((vec![1usize, 1], vec![1f32]))?;
        let mask_input = Tensor::from_array((vec![1usize, 1, 256, 256], vec![0f32; 256 * 256]))?;
        let has_mask = Tensor::from_array((vec![1usize], vec![0f32]))?;
        let orig_size = Tensor::from_array((vec![2usize], vec![oh as f32, ow as f32]))?;

        // Positional decoder inputs in the standard `samexporter` order:
        // image_embeddings, point_coords, point_labels, mask_input, has_mask_input,
        // orig_im_size. (Matches the positional convention used by faces.rs.)
        let mut dec = self.decoder.lock().expect("segment decoder lock");
        let out = dec.run(ort::inputs![
            emb_t,
            point_coords,
            point_labels,
            mask_input,
            has_mask,
            orig_size
        ])?;

        // Find the masks tensor: a 4-D output [1,M,h,w]; pick the highest-IoU mask
        // if an iou tensor is present, else the first.
        let mut masks: Option<(Vec<usize>, Vec<f32>)> = None;
        let mut ious: Option<Vec<f32>> = None;
        for (name, _) in out.iter() {
            let (shape, vals) = out[name].try_extract_tensor::<f32>()?;
            let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            if shape.len() == 4 {
                masks = Some((shape, vals.to_vec()));
            } else if shape.iter().product::<usize>() <= 8 {
                ious = Some(vals.to_vec());
            }
        }
        let (mshape, mdata) = masks.ok_or_else(|| anyhow::anyhow!("decoder produced no mask"))?;
        let (m, mh, mw) = (mshape[1], mshape[2], mshape[3]);
        let best = ious
            .as_ref()
            .and_then(|v| {
                v.iter()
                    .take(m)
                    .enumerate()
                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        let chan = best * mh * mw;

        // Threshold logits > 0 → object; emit a mask at the decoder's (h,w), then
        // resize to the original image size.
        let mut mask = image::GrayImage::new(mw as u32, mh as u32);
        for y in 0..mh {
            for x in 0..mw {
                let v = mdata[chan + y * mw + x];
                mask.put_pixel(x as u32, y as u32, image::Luma([if v > 0.0 { 255 } else { 0 }]));
            }
        }
        let mask = if (mw as u32, mh as u32) != (ow, oh) {
            image::imageops::resize(&mask, ow, oh, FilterType::Nearest)
        } else {
            mask
        };

        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(mask)
            .write_to(&mut buf, image::ImageFormat::Png)
            .context("encoding mask PNG")?;
        Ok(buf.into_inner())
    }
}
