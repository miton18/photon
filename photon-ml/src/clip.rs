//! CLIP image + text encoders via ONNX Runtime (`ort`).
//!
//! Produces L2-normalized embeddings in a shared image/text space so the Photon
//! server can match free-text queries against photo content by cosine
//! similarity. Loading is fallible and non-panicking: if a model file is absent
//! the capability simply reports "not loaded" and the endpoint returns 503.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use instant_clip_tokenizer::{Token, Tokenizer};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

/// CLIP preprocessing constants (OpenAI/open_clip convention).
const CLIP_SIZE: u32 = 224;
const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_72];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];
/// CLIP text context length (ViT-B-32 and friends).
const CONTEXT_LEN: usize = 77;

/// Loaded CLIP encoders. Sessions are wrapped in a `Mutex` because `ort` session
/// `run` takes `&mut self`; inference is CPU-bound and serialized per request.
pub struct Clip {
    image: Mutex<Session>,
    text: Mutex<Session>,
    tokenizer: Tokenizer,
    /// Embedding dimension, inferred from the text encoder's first run.
    pub dim: usize,
}

fn open_session(path: &Path) -> Result<Session> {
    Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(num_threads())?
        .commit_from_file(path)
        .with_context(|| format!("loading ONNX model {}", path.display()))
}

fn num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

impl Clip {
    /// Try to load both encoders. Returns `Ok(None)` (capability unavailable)
    /// when either model file is missing, `Err` only on a genuine load failure.
    pub fn try_load(image_path: &Path, text_path: &Path) -> Result<Option<Self>> {
        if !image_path.exists() || !text_path.exists() {
            return Ok(None);
        }
        let image = open_session(image_path)?;
        let text = open_session(text_path)?;
        let tokenizer = Tokenizer::new();
        let clip = Self {
            image: Mutex::new(image),
            text: Mutex::new(text),
            tokenizer,
            // Re-derived from the first text run below.
            dim: 0,
        };
        // Probe the text encoder once to learn the embedding dimension, exactly
        // as the Python sidecar did.
        let probe = clip.embed_text("dim probe")?;
        Ok(Some(Self {
            dim: probe.len(),
            ..clip
        }))
    }

    /// Embed raw image bytes into the CLIP space (L2-normalized).
    pub fn embed_image(&self, bytes: &[u8]) -> Result<Vec<f32>> {
        let img = image::load_from_memory(bytes)
            .context("decoding image")?
            .to_rgb8();
        // CLIP preprocessing: resize shortest side to 224 then center-crop 224.
        let resized = resize_center_crop(&img, CLIP_SIZE);
        // CHW f32, normalized.
        let mut data = vec![0f32; 3 * (CLIP_SIZE * CLIP_SIZE) as usize];
        let plane = (CLIP_SIZE * CLIP_SIZE) as usize;
        for (i, px) in resized.pixels().enumerate() {
            for c in 0..3 {
                let v = px[c] as f32 / 255.0;
                data[c * plane + i] = (v - CLIP_MEAN[c]) / CLIP_STD[c];
            }
        }
        let input = Tensor::from_array((
            vec![1usize, 3, CLIP_SIZE as usize, CLIP_SIZE as usize],
            data,
        ))?;
        let mut sess = self.image.lock().expect("clip image lock");
        let outputs = sess.run(ort::inputs![input])?;
        let (_shape, out) = first_f32_output(&outputs)?;
        Ok(l2_normalize(&out))
    }

    /// Embed a free-text query into the same CLIP space (L2-normalized).
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let tokens = self.tokenize(text);
        // ONNX CLIP text encoders take int32 token ids of shape [1, 77].
        let input = Tensor::from_array((vec![1usize, CONTEXT_LEN], tokens))?;
        let mut sess = self.text.lock().expect("clip text lock");
        let outputs = sess.run(ort::inputs![input])?;
        let (_shape, out) = first_f32_output(&outputs)?;
        Ok(l2_normalize(&out))
    }

    /// CLIP BPE tokenization: `<start>` + bpe + `<end>`, padded/truncated to 77.
    fn tokenize(&self, text: &str) -> Vec<i32> {
        let mut tokens: Vec<Token> = Vec::new();
        self.tokenizer.encode(text, &mut tokens);
        let mut ids: Vec<i32> = Vec::with_capacity(CONTEXT_LEN);
        ids.push(self.tokenizer.start_of_text().to_u16() as i32);
        for t in tokens {
            if ids.len() >= CONTEXT_LEN - 1 {
                break;
            }
            ids.push(t.to_u16() as i32);
        }
        ids.push(self.tokenizer.end_of_text().to_u16() as i32);
        ids.resize(CONTEXT_LEN, 0);
        ids
    }
}

/// Resize so the shorter side is `size`, then center-crop `size`x`size`.
fn resize_center_crop(
    img: &image::RgbImage,
    size: u32,
) -> image::RgbImage {
    let (w, h) = img.dimensions();
    let scale = size as f32 / w.min(h) as f32;
    let nw = ((w as f32) * scale).round().max(size as f32) as u32;
    let nh = ((h as f32) * scale).round().max(size as f32) as u32;
    let resized = image::imageops::resize(img, nw, nh, FilterType::CatmullRom);
    let x = (nw - size) / 2;
    let y = (nh - size) / 2;
    image::imageops::crop_imm(&resized, x, y, size, size).to_image()
}

/// L2-normalize a vector; a zero vector is returned unchanged.
fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

/// Extract the first tensor output as an owned `(shape, data)` pair.
pub fn first_f32_output(
    outputs: &ort::session::SessionOutputs,
) -> Result<(Vec<usize>, Vec<f32>)> {
    let (name, _) = outputs
        .iter()
        .next()
        .ok_or_else(|| anyhow!("model produced no outputs"))?;
    let (shape, data) = outputs[name].try_extract_tensor::<f32>()?;
    Ok((shape.iter().map(|&d| d as usize).collect(), data.to_vec()))
}
