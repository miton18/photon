//! OCR via the pure-Rust `ocrs` crate (text detection + recognition ONNX models
//! served by its own `rten` runtime).
//!
//! Returns the joined text plus per-line strings, matching the original Python
//! `/ocr` shape. Loading is fallible and non-panicking: missing model files mean
//! the capability is simply unavailable and the endpoint returns 503.

use std::path::Path;

use anyhow::{Context, Result};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;

/// Loaded OCR engine. `OcrEngine` is `Send + Sync` and stateless per call.
pub struct Ocr {
    engine: OcrEngine,
}

fn load_model(path: &Path) -> Result<Model> {
    Model::load_file(path).with_context(|| format!("loading OCR model {}", path.display()))
}

impl Ocr {
    /// Try to build the engine. Returns `Ok(None)` when either model file is
    /// absent; `Err` only on a genuine load failure.
    pub fn try_load(detection_path: &Path, recognition_path: &Path) -> Result<Option<Self>> {
        if !detection_path.exists() || !recognition_path.exists() {
            return Ok(None);
        }
        let detection_model = load_model(detection_path)?;
        let recognition_model = load_model(recognition_path)?;
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(detection_model),
            recognition_model: Some(recognition_model),
            ..Default::default()
        })
        .context("building OCR engine")?;
        Ok(Some(Self { engine }))
    }

    /// Recognize text in raw image bytes. Returns per-line strings in reading
    /// order (empty when no text is found — never an error for a blank image).
    pub fn recognize(&self, bytes: &[u8]) -> Result<Vec<String>> {
        let img = image::load_from_memory(bytes)
            .context("decoding image")?
            .to_rgb8();
        let (w, h) = img.dimensions();
        let source = ImageSource::from_bytes(img.as_raw(), (w, h))
            .context("building OCR image source")?;
        let input = self
            .engine
            .prepare_input(source)
            .context("preparing OCR input")?;
        let words = self.engine.detect_words(&input).context("detecting words")?;
        let line_rects = self.engine.find_text_lines(&input, &words);
        let lines = self
            .engine
            .recognize_text(&input, &line_rects)
            .context("recognizing text")?;
        Ok(lines
            .into_iter()
            .flatten()
            .map(|l| l.to_string().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }
}
