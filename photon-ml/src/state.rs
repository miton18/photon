//! Shared application state: the loaded (or absent) model capabilities.
//!
//! Each capability is loaded once at startup. A missing model file leaves the
//! capability as `None` so the service still starts and `/health` reports which
//! capabilities are available; the corresponding endpoint then returns 503.

use std::sync::Arc;

use crate::clip::Clip;
use crate::config::Config;
use crate::faces::Faces;
use crate::inpaint::Inpaint;
use crate::ocr::Ocr;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub config: Config,
    pub clip: Option<Clip>,
    pub ocr: Option<Ocr>,
    pub faces: Option<Faces>,
    pub inpaint: Option<Inpaint>,
}

impl AppState {
    /// Load every capability from the configured models directory. Never panics:
    /// a load error for one capability is logged and that capability is disabled.
    pub fn load(config: Config) -> Self {
        let clip = match Clip::try_load(
            &config.model_path(&config.clip_image_file),
            &config.model_path(&config.clip_text_file),
        ) {
            Ok(Some(c)) => {
                tracing::info!(dim = c.dim, "CLIP encoders loaded");
                Some(c)
            }
            Ok(None) => {
                tracing::warn!(
                    image = %config.model_path(&config.clip_image_file).display(),
                    text = %config.model_path(&config.clip_text_file).display(),
                    "CLIP model files absent — /embed/* will return 503"
                );
                None
            }
            Err(e) => {
                tracing::error!("CLIP load failed: {e:#}");
                None
            }
        };

        let ocr = match Ocr::try_load(
            &config.model_path(&config.ocr_detection_file),
            &config.model_path(&config.ocr_recognition_file),
        ) {
            Ok(Some(o)) => {
                tracing::info!("OCR engine loaded");
                Some(o)
            }
            Ok(None) => {
                tracing::warn!("OCR model files absent — /ocr will return 503");
                None
            }
            Err(e) => {
                tracing::error!("OCR load failed: {e:#}");
                None
            }
        };

        let faces = match Faces::try_load(
            &config.model_path(&config.face_detection_file),
            &config.model_path(&config.face_recognition_file),
        ) {
            Ok(Some(f)) => {
                tracing::info!(dim = f.dim, "Face models loaded");
                Some(f)
            }
            Ok(None) => {
                tracing::warn!("Face model files absent — /faces will return 503");
                None
            }
            Err(e) => {
                tracing::error!("Face load failed: {e:#}");
                None
            }
        };

        let inpaint = match Inpaint::try_load(&config.model_path(&config.inpaint_file)) {
            Ok(Some(i)) => {
                tracing::info!("Inpainting (magic eraser) model loaded");
                Some(i)
            }
            Ok(None) => {
                tracing::warn!("Inpainting model file absent — /inpaint will return 503");
                None
            }
            Err(e) => {
                tracing::error!("Inpaint load failed: {e:#}");
                None
            }
        };

        Self {
            inner: Arc::new(Inner {
                config,
                clip,
                ocr,
                faces,
                inpaint,
            }),
        }
    }
}
