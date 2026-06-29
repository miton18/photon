//! Runtime configuration, read from the environment once at startup.
//!
//! Mirrors the env-swappable spirit of the original Python sidecar: capability
//! model files live under a single models directory and individual file names /
//! variants can be overridden without code changes. Nothing here touches the
//! network or the filesystem — it only resolves paths and names.

use std::path::PathBuf;

/// Default directory the models are loaded from. Overridable via
/// `PHOTON_ML_MODELS_DIR`.
const DEFAULT_MODELS_DIR: &str = "/models";

/// Resolved configuration for all three capabilities.
#[derive(Clone, Debug)]
pub struct Config {
    pub models_dir: PathBuf,

    /// Human-readable CLIP model name reported by `/health` (e.g. `ViT-B-32`).
    pub clip_model_name: String,
    /// CLIP image encoder ONNX file name (under `models_dir`).
    pub clip_image_file: String,
    /// CLIP text encoder ONNX file name (under `models_dir`).
    pub clip_text_file: String,

    /// OCR recognition languages reported by `/health` (informational; the ocrs
    /// default models are language-agnostic Latin-script).
    pub ocr_langs: Vec<String>,
    /// OCR text-detection model file (ocrs `.rten` or `.onnx`).
    pub ocr_detection_file: String,
    /// OCR text-recognition model file (ocrs `.rten` or `.onnx`).
    pub ocr_recognition_file: String,

    /// Face model pack name reported by `/health`.
    pub face_model_name: String,
    /// Face-detection ONNX file name (SCRFD by default).
    pub face_detection_file: String,
    /// Face-embedding ONNX file name (AuraFace/ArcFace by default).
    pub face_recognition_file: String,

    /// Inpainting (magic-eraser) ONNX file name (LaMa-style). Operator-supplied;
    /// absent by default so the capability is simply disabled.
    pub inpaint_file: String,

    /// Promptable segmentation (SAM-family) ONNX files for tap-to-select.
    /// Operator-supplied; both absent by default ⇒ capability disabled.
    pub segment_encoder_file: String,
    pub segment_decoder_file: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl Config {
    /// Resolve configuration from the environment, applying sensible defaults so
    /// the service runs even before any env var is set.
    pub fn from_env() -> Self {
        let models_dir = PathBuf::from(env_or("PHOTON_ML_MODELS_DIR", DEFAULT_MODELS_DIR));

        let clip_model_name = env_or("PHOTON_CLIP_MODEL", "ViT-B-32");
        let clip_image_file = env_or("PHOTON_CLIP_IMAGE_MODEL", "clip_image.onnx");
        let clip_text_file = env_or("PHOTON_CLIP_TEXT_MODEL", "clip_text.onnx");

        let ocr_langs = env_or("PHOTON_OCR_LANGS", "en,fr")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let ocr_langs = if ocr_langs.is_empty() {
            vec!["en".to_string()]
        } else {
            ocr_langs
        };
        let ocr_detection_file = env_or("PHOTON_OCR_DETECTION_MODEL", "text-detection.rten");
        let ocr_recognition_file = env_or("PHOTON_OCR_RECOGNITION_MODEL", "text-recognition.rten");

        // AuraFace pack (fal.ai, Apache-2.0, commercial-OK): SCRFD detector +
        // ArcFace ResNet100 embedder. Overridable for a different pack.
        let face_model_name = env_or("PHOTON_FACE_MODEL", "auraface");
        let face_detection_file = env_or("PHOTON_FACE_DETECTION_MODEL", "scrfd.onnx");
        let face_recognition_file = env_or("PHOTON_FACE_RECOGNITION_MODEL", "auraface.onnx");

        // Inpainting (magic eraser): LaMa-style ONNX. No public default URL ships
        // (license diligence is the operator's), so the file is absent unless set.
        let inpaint_file = env_or("PHOTON_INPAINT_MODEL", "inpaint.onnx");

        // Promptable segmentation (MobileSAM/EfficientSAM/SAM), two-graph export.
        let segment_encoder_file = env_or("PHOTON_SEGMENT_ENCODER", "sam_encoder.onnx");
        let segment_decoder_file = env_or("PHOTON_SEGMENT_DECODER", "sam_decoder.onnx");

        Self {
            models_dir,
            clip_model_name,
            clip_image_file,
            clip_text_file,
            ocr_langs,
            ocr_detection_file,
            ocr_recognition_file,
            face_model_name,
            face_detection_file,
            face_recognition_file,
            inpaint_file,
            segment_encoder_file,
            segment_decoder_file,
        }
    }

    /// Absolute path to a model file under the models directory.
    pub fn model_path(&self, file: &str) -> PathBuf {
        self.models_dir.join(file)
    }
}
