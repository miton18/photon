//! Object removal (inpainting) via ONNX Runtime — a LaMa-style model. This is the
//! engine behind the editor's "magic eraser": paint over an object, and the model
//! reconstructs the masked region from its surroundings.
//!
//! Model contract: a single ONNX graph with TWO float32 inputs, in this order —
//!   `image` `[1,3,H,W]` in `[0,1]` (RGB) and `mask` `[1,1,H,W]` in `{0,1}`
//!   (1 = pixel to fill) — and ONE float32 output `[1,3,H,W]` (RGB). `H`/`W` must
//! be multiples of 8 (LaMa's downsampling). Tensor names vary by export, so we
//! feed them positionally (image first, mask second), matching the common
//! `big-lama` export. The decode auto-detects a `[0,1]` vs `[0,255]` output range.
//!
//! For speed AND quality we never inpaint the whole photo: we crop a padded
//! bounding box around the mask, run the model on that region (downscaled to at
//! most MAX_TILE), and composite the result back over ONLY the masked pixels — the
//! rest of the original is untouched. Loading is fallible/non-panicking: a missing
//! model file disables the capability (the endpoint returns 503).

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{GrayImage, Rgb, RgbImage};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

use crate::clip::first_f32_output;

/// Mask pixels strictly above this (0-255) are "fill me".
const MASK_THRESH: u8 = 127;
/// Default model input side. Most LaMa ONNX exports (e.g. Carve/LaMa-ONNX) have a
/// FIXED 512×512 input; the masked region is resized to this square for inference.
/// Override with `PHOTON_INPAINT_SIZE` for a model trained at another resolution.
const DEFAULT_SIZE: u32 = 512;

pub struct Inpaint {
    session: Mutex<Session>,
    /// The square side the model expects (the region is resized to this).
    size: u32,
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
        .with_context(|| format!("loading inpainting model {}", path.display()))
}

impl Inpaint {
    pub fn try_load(model_path: &Path) -> Result<Option<Self>> {
        if !model_path.exists() {
            return Ok(None);
        }
        let size = std::env::var("PHOTON_INPAINT_SIZE")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&n| n >= 64)
            .unwrap_or(DEFAULT_SIZE);
        Ok(Some(Self {
            session: Mutex::new(open_session(model_path)?),
            size,
        }))
    }

    /// Inpaint `image_bytes` over the white region of `mask_bytes`, returning PNG.
    /// The mask is decoded as grayscale and resized to the image if needed.
    pub fn inpaint(&self, image_bytes: &[u8], mask_bytes: &[u8]) -> Result<Vec<u8>> {
        let mut img = image::load_from_memory(image_bytes)
            .context("decoding image")?
            .to_rgb8();
        let (iw, ih) = img.dimensions();

        let mask_in = image::load_from_memory(mask_bytes)
            .context("decoding mask")?
            .to_luma8();
        let mask = if mask_in.dimensions() == (iw, ih) {
            mask_in
        } else {
            image::imageops::resize(&mask_in, iw, ih, FilterType::Nearest)
        };

        // Region of interest = padded bounding box of the masked pixels.
        let Some((mx0, my0, mx1, my1)) = mask_bbox(&mask) else {
            return encode_png(&img); // nothing to erase
        };
        let bw = mx1 - mx0 + 1;
        let bh = my1 - my0 + 1;
        let margin_x = (bw / 2).clamp(16, 256);
        let margin_y = (bh / 2).clamp(16, 256);
        let rx0 = mx0.saturating_sub(margin_x);
        let ry0 = my0.saturating_sub(margin_y);
        let rx1 = (mx1 + margin_x).min(iw - 1);
        let ry1 = (my1 + margin_y).min(ih - 1);
        let rw = rx1 - rx0 + 1;
        let rh = ry1 - ry0 + 1;

        let roi_img = image::imageops::crop_imm(&img, rx0, ry0, rw, rh).to_image();
        let roi_mask = image::imageops::crop_imm(&mask, rx0, ry0, rw, rh).to_image();

        // The model expects a fixed square input: resize the cropped region (and its
        // mask) to it. This is the "crop" high-res strategy — only the masked area is
        // ever sent to the model, so cost is independent of the full image size.
        let (tw, th) = (self.size, self.size);
        let in_img = image::imageops::resize(&roi_img, tw, th, FilterType::Triangle);
        let in_mask = image::imageops::resize(&roi_mask, tw, th, FilterType::Nearest);

        let filled = self.run_tile(&in_img, &in_mask)?;
        // Bring the filled tile back to ROI resolution.
        let filled = if filled.dimensions() != (rw, rh) {
            image::imageops::resize(&filled, rw, rh, FilterType::Triangle)
        } else {
            filled
        };

        // Composite: replace ONLY the masked pixels with the model output.
        for y in 0..rh {
            for x in 0..rw {
                if roi_mask.get_pixel(x, y)[0] > MASK_THRESH {
                    img.put_pixel(rx0 + x, ry0 + y, *filled.get_pixel(x, y));
                }
            }
        }
        encode_png(&img)
    }

    /// Run the model on one aligned tile, returning the RGB output at the tile size.
    fn run_tile(&self, img: &RgbImage, mask: &GrayImage) -> Result<RgbImage> {
        let (w, h) = img.dimensions();
        let plane = (w * h) as usize;
        let mut idata = vec![0f32; 3 * plane];
        for (i, px) in img.pixels().enumerate() {
            for c in 0..3 {
                idata[c * plane + i] = px[c] as f32 / 255.0;
            }
        }
        let mut mdata = vec![0f32; plane];
        for (i, px) in mask.pixels().enumerate() {
            mdata[i] = if px[0] > MASK_THRESH { 1.0 } else { 0.0 };
        }
        let image_t = Tensor::from_array((vec![1usize, 3, h as usize, w as usize], idata))?;
        let mask_t = Tensor::from_array((vec![1usize, 1, h as usize, w as usize], mdata))?;

        let mut sess = self.session.lock().expect("inpaint session lock");
        let outputs = sess.run(ort::inputs![image_t, mask_t])?;
        let (shape, data) = first_f32_output(&outputs)?;

        let (oh, ow) = if shape.len() == 4 {
            (shape[2], shape[3])
        } else {
            (h as usize, w as usize)
        };
        let oplane = ow * oh;
        if data.len() < 3 * oplane {
            anyhow::bail!("inpaint output too small: {} for {}x{}", data.len(), ow, oh);
        }
        // Auto-detect output range: LaMa exports emit either [0,1] or [0,255].
        let maxv = data.iter().copied().fold(0f32, f32::max);
        let scale255 = if maxv > 1.5 { 1.0 } else { 255.0 };
        let mut out = RgbImage::new(ow as u32, oh as u32);
        for i in 0..oplane {
            let r = (data[i] * scale255).clamp(0.0, 255.0) as u8;
            let g = (data[oplane + i] * scale255).clamp(0.0, 255.0) as u8;
            let b = (data[2 * oplane + i] * scale255).clamp(0.0, 255.0) as u8;
            out.put_pixel((i % ow) as u32, (i / ow) as u32, Rgb([r, g, b]));
        }
        Ok(if (ow as u32, oh as u32) != (w, h) {
            image::imageops::resize(&out, w, h, FilterType::Triangle)
        } else {
            out
        })
    }
}

/// Tight bounding box (x0,y0,x1,y1 inclusive) of pixels above the mask threshold.
fn mask_bbox(mask: &GrayImage) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = mask.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x, y)[0] > MASK_THRESH {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    any.then_some((x0, y0, x1, y1))
}

fn encode_png(img: &RgbImage) -> Result<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img.clone())
        .write_to(&mut buf, image::ImageFormat::Png)
        .context("encoding inpainted PNG")?;
    Ok(buf.into_inner())
}
