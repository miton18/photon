//! ML SIDECAR CLIENT — open-vocabulary context recognition via CLIP embeddings.
//!
//! Photon stays dependency-light and build-anywhere (no heavy ML toolchain in
//! the Rust binary). Instead, a small Python FastAPI sidecar (`photon-ml/`)
//! serves CLIP image/text embeddings over HTTP and this module is the thin
//! client.
//!
//! OFFLINE-FIRST: the client is gated entirely on the `PHOTON_ML_URL`
//! environment variable. When it is unset (the default for demos, the test
//! suite, and any offline build) [`MlClient::from_env`] returns `None` and NO
//! network is ever touched — behavior is exactly as before this feature. Every
//! call returns `Option`/`Result` and treats any failure (unset URL, connection
//! refused, non-200, bad body) as simply "ML unavailable": it logs and returns
//! `None`, never panics, never fails a request.

use serde::{Deserialize, Serialize};

/// Env var that points at the ML sidecar base URL (e.g. `http://photon-ml:8000`).
/// When unset, ML features are disabled and no network is used.
pub const ML_URL_ENV: &str = "PHOTON_ML_URL";

/// Thin HTTP client for the CLIP embedding sidecar. Construct with
/// [`MlClient::from_env`]; `None` means ML is disabled (offline).
#[derive(Clone)]
pub struct MlClient {
    base_url: String,
    http: reqwest::Client,
    /// Serializes `/faces` calls: the sidecar processes one detection at a time
    /// (mutex on the model), so firing many concurrently under a big import just
    /// causes contention + dropped/timed-out requests → photos with no faces. One
    /// permit keeps detection sequential and reliable (slower, but no drops).
    faces_sem: std::sync::Arc<tokio::sync::Semaphore>,
}

#[derive(Serialize)]
struct EmbedTextRequest<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct OcrResponse {
    /// Joined, newline-separated recognized text. May be empty (no text found).
    text: String,
}

/// One detected face from the sidecar's `POST /faces`: a bounding box
/// (`[x, y, w, h]` in source-image pixels), its L2-normalized embedding (kept
/// server-side only — NEVER serialized to API responses), and the detector
/// confidence `score`.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedFace {
    pub bbox: [f32; 4],
    pub embedding: Vec<f32>,
    pub score: f32,
}

#[derive(Deserialize)]
struct FacesResponse {
    faces: Vec<DetectedFace>,
}

/// Log a non-success ML response. A 503 just means that capability isn't loaded
/// in the sidecar (its model file is absent) — expected, so debug-level, NOT a
/// warning spammed once per photo. Anything else is a genuine warning.
fn log_ml_status(endpoint: &str, status: reqwest::StatusCode) {
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        tracing::debug!("ML sidecar {endpoint}: capability not loaded ({status})");
    } else {
        tracing::warn!("ML sidecar {endpoint} returned status {status}");
    }
}

impl MlClient {
    /// Build a client from `PHOTON_ML_URL`. Returns `None` when the var is unset
    /// or empty (ML disabled — no network). A short request timeout keeps a slow
    /// or wedged sidecar from blocking an upload/search indefinitely.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var(ML_URL_ENV).ok().filter(|s| !s.is_empty())?;
        // Generous timeout: full-res face detection on a large photo can take ~10s
        // (CPU), and CLIP/OCR are quicker. Too short would drop slow detections.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .ok()?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            faces_sem: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    /// Embed raw image bytes (the thumbnail) into the CLIP space. Returns an
    /// L2-normalized vector, or `None` on any failure (treated as "unavailable").
    pub async fn embed_image(&self, bytes: Vec<u8>) -> Option<Vec<f32>> {
        let url = format!("{}/embed/image", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await;
        Self::parse(resp).await
    }

    /// Embed a free-text query (e.g. "yellow car" / "voiture jaune") into the
    /// same CLIP space. Returns an L2-normalized vector, or `None` on failure.
    pub async fn embed_text(&self, text: &str) -> Option<Vec<f32>> {
        let url = format!("{}/embed/text", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&EmbedTextRequest { text })
            .send()
            .await;
        Self::parse(resp).await
    }

    /// OCR raw image bytes via the sidecar's `POST /ocr`, returning the joined
    /// recognized text. Returns `None` on any failure (unset URL, connection
    /// refused, non-200, bad body) OR when the recognized text is empty/blank —
    /// the same resilient, never-panics pattern as [`Self::embed_image`]. A
    /// `None` result means "leave the existing `ocr_text` untouched".
    pub async fn ocr(&self, bytes: Vec<u8>) -> Option<String> {
        let url = format!("{}/ocr", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<OcrResponse>().await {
                Ok(body) => {
                    let text = body.text.trim();
                    if text.is_empty() {
                        None
                    } else {
                        Some(text.to_string())
                    }
                }
                Err(e) => {
                    tracing::warn!("ML sidecar /ocr response decode failed: {e}");
                    None
                }
            },
            Ok(r) => {
                log_ml_status("/ocr", r.status());
                None
            }
            Err(e) => {
                tracing::warn!("ML sidecar /ocr request failed: {e}");
                None
            }
        }
    }

    /// MAGIC ERASER: inpaint `image` over the white region of `mask` via the
    /// sidecar's `POST /inpaint`. The body is length-framed (no multipart dep):
    /// `[u32 BE mask_len][mask bytes][image bytes]`. Returns the inpainted PNG
    /// bytes, or `None` on any failure (unset URL, model not loaded → 503,
    /// connection error, non-200) so the caller can degrade gracefully.
    pub async fn inpaint(&self, image: Vec<u8>, mask: Vec<u8>) -> Option<Vec<u8>> {
        let url = format!("{}/inpaint", self.base_url);
        let mut body = Vec::with_capacity(4 + mask.len() + image.len());
        body.extend_from_slice(&(mask.len() as u32).to_be_bytes());
        body.extend_from_slice(&mask);
        body.extend_from_slice(&image);
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) => Some(b.to_vec()),
                Err(e) => {
                    tracing::warn!("ML sidecar /inpaint body read failed: {e}");
                    None
                }
            },
            Ok(r) => {
                log_ml_status("/inpaint", r.status());
                None
            }
            Err(e) => {
                tracing::warn!("ML sidecar /inpaint request failed: {e}");
                None
            }
        }
    }

    /// TAP-TO-SELECT: segment the object under `(x, y)` (ORIGINAL-image pixels)
    /// via the sidecar's `POST /segment?x=&y=`, returning a binary mask PNG (white
    /// = object). `None` on any failure (unset URL, model not loaded → 503,
    /// connection error, non-200) so the caller degrades gracefully.
    pub async fn segment(&self, image: Vec<u8>, x: f32, y: f32) -> Option<Vec<u8>> {
        let url = format!("{}/segment?x={x}&y={y}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(image)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => r.bytes().await.ok().map(|b| b.to_vec()),
            Ok(r) => {
                log_ml_status("/segment", r.status());
                None
            }
            Err(e) => {
                tracing::warn!("ML sidecar /segment request failed: {e}");
                None
            }
        }
    }

    /// FACE DETECTION + EMBEDDING: detect faces in raw image bytes via the
    /// sidecar's `POST /faces`, returning one [`DetectedFace`] per face (bbox +
    /// L2-normalized embedding + score). Returns `None` on any failure (unset
    /// URL — there is no client, connection refused, non-200, bad body), the
    /// same resilient never-panics pattern as [`Self::embed_image`]. An image
    /// with no faces returns `Some(vec![])`. The embeddings are sensitive and
    /// are only ever stored server-side, never returned by any API.
    pub async fn faces(&self, bytes: Vec<u8>) -> Option<Vec<DetectedFace>> {
        // Serialize detection across the whole process (the sidecar can only do one
        // at a time anyway). The permit is released when `_permit` drops.
        let _permit = self.faces_sem.acquire().await.ok()?;
        let url = format!("{}/faces", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<FacesResponse>().await {
                Ok(body) => Some(body.faces),
                Err(e) => {
                    tracing::warn!("ML sidecar /faces response decode failed: {e}");
                    None
                }
            },
            Ok(r) => {
                log_ml_status("/faces", r.status());
                None
            }
            Err(e) => {
                tracing::warn!("ML sidecar /faces request failed: {e}");
                None
            }
        }
    }

    /// Decode an embedding response, logging and swallowing any error.
    async fn parse(resp: Result<reqwest::Response, reqwest::Error>) -> Option<Vec<f32>> {
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<EmbedResponse>().await {
                Ok(body) if !body.embedding.is_empty() => Some(body.embedding),
                Ok(_) => {
                    tracing::warn!("ML sidecar returned an empty embedding");
                    None
                }
                Err(e) => {
                    tracing::warn!("ML sidecar response decode failed: {e}");
                    None
                }
            },
            Ok(r) => {
                log_ml_status("/embed/image", r.status());
                None
            }
            Err(e) => {
                tracing::warn!("ML sidecar request failed: {e}");
                None
            }
        }
    }
}

/// Cosine similarity between two equal-length vectors. Returns `None` when the
/// lengths differ or either vector is empty / zero-norm (so callers can skip
/// uncomparable candidates rather than rank them with a bogus score).
///
/// Embeddings from the sidecar are already L2-normalized, so this is effectively
/// a dot product, but we normalize defensively so synthetic test vectors and any
/// non-normalized input still rank correctly.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        let c = cosine_similarity(&v, &v).unwrap();
        assert!((c - 1.0).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).unwrap().abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite_is_negative() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!(cosine_similarity(&a, &b).unwrap() < 0.0);
    }

    #[test]
    fn cosine_length_mismatch_is_none() {
        assert!(cosine_similarity(&[1.0, 2.0], &[1.0]).is_none());
        assert!(cosine_similarity(&[], &[]).is_none());
    }

    #[test]
    fn cosine_zero_vector_is_none() {
        assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]).is_none());
    }

    /// With ML disabled (the offline default) the client must be absent and no
    /// network is ever attempted — so the OCR path (which only exists on an
    /// `MlClient`) is structurally inert: there is no client to call.
    #[test]
    fn from_env_unset_is_none_ocr_inert() {
        // SAFETY: single-threaded test; we restore nothing because the suite
        // never sets PHOTON_ML_URL.
        unsafe {
            std::env::remove_var(ML_URL_ENV);
        }
        let client = MlClient::from_env();
        assert!(client.is_none(), "no ML client ⇒ ocr() is never reachable");
    }
}
