//! Photon ML sidecar — open-vocabulary context recognition (CLIP) + OCR + faces.
//!
//! A small, CPU-friendly Rust HTTP service (axum) that turns images and text into
//! vectors in the SAME CLIP embedding space, so the Photon server can match
//! free-text queries against photo content by cosine similarity. It also performs
//! OCR (text extraction) and face detection + embedding.
//!
//! This is a drop-in replacement for the former Python/FastAPI sidecar: the HTTP
//! contract (paths, request/response JSON) is byte-for-byte identical, so the
//! server's `MlClient` is unchanged.
//!
//! Inference runs on ONNX Runtime via the `ort` crate (CLIP + faces) and the
//! pure-Rust `ocrs` crate (OCR). Models load once at startup from
//! `PHOTON_ML_MODELS_DIR`; a missing model file disables only that capability.

mod clip;
mod config;
mod faces;
mod inpaint;
mod ocr;
mod state;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::state::AppState;

/// Max request body for image uploads (full-res photos can be 15-40 MB).
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Initialize the ONNX Runtime environment once. Failure here is fatal because
    // nothing ML-related can work without it; it does NOT require model files.
    if let Err(e) = ort::init().with_name("photon-ml").commit() {
        tracing::error!("failed to initialize ONNX Runtime: {e:#}");
    }

    let config = Config::from_env();
    tracing::info!(?config, "loading models");
    let state = AppState::load(config);

    let app = Router::new()
        .route("/health", get(health))
        .route("/embed/image", post(embed_image))
        .route("/embed/text", post(embed_text))
        .route("/ocr", post(ocr))
        .route("/faces", post(faces))
        .route("/inpaint", post(inpaint))
        // Two limits must agree: the tower-http layer AND axum's per-extractor
        // DefaultBodyLimit (2 MB by default) which the `Bytes` extractor enforces
        // and which would otherwise 413 large full-res photos before they're read.
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = "0.0.0.0:8000";
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("photon-ml listening on {addr}");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("server error: {e}");
        std::process::exit(1);
    }
}

/// JSON error body matching the resilient `{error|detail}` convention. The
/// server's `MlClient` treats any non-200 as "ML unavailable", so the exact body
/// is not load-bearing — we return a clear `error` field.
fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Response {
    let inner = &state.inner;
    let cfg = &inner.config;
    let dim = inner.clip.as_ref().map(|c| c.dim).unwrap_or(0);
    let face_dim = inner.faces.as_ref().map(|f| f.dim).unwrap_or(512);
    Json(json!({
        "status": "ok",
        "model": cfg.clip_model_name,
        "dim": dim,
        "ocr": {
            "engine": "ocrs",
            "langs": cfg.ocr_langs,
            "loaded": inner.ocr.is_some(),
        },
        "faces": {
            "engine": "auraface (scrfd+arcface)",
            "model": cfg.face_model_name,
            "dim": face_dim,
            "loaded": inner.faces.is_some(),
        },
        "clip_loaded": inner.clip.is_some(),
        "inpaint": {
            "engine": "lama (onnx)",
            "loaded": inner.inpaint.is_some(),
        },
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /embed/image  (raw image bytes) -> {"embedding": [f32; dim]}
// ---------------------------------------------------------------------------

async fn embed_image(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty image body");
    }
    let Some(clip) = state.inner.clip.as_ref() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "CLIP model not loaded");
    };
    match clip.embed_image(&body) {
        Ok(embedding) => Json(json!({ "embedding": embedding })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("invalid image: {e}")),
    }
}

// ---------------------------------------------------------------------------
// POST /embed/text  ({"text": "..."}) -> {"embedding": [f32; dim]}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TextRequest {
    text: String,
}

async fn embed_text(State(state): State<AppState>, payload: Json<TextRequest>) -> Response {
    let text = payload.text.trim();
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty text");
    }
    let Some(clip) = state.inner.clip.as_ref() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "CLIP model not loaded");
    };
    match clip.embed_text(text) {
        Ok(embedding) => Json(json!({ "embedding": embedding })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("embed failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// POST /ocr  (raw image bytes) -> {"text": "...", "lines": [...]}
// ---------------------------------------------------------------------------

async fn ocr(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty image body");
    }
    let Some(engine) = state.inner.ocr.as_ref() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "OCR model not loaded");
    };
    match engine.recognize(&body) {
        Ok(lines) => {
            let text = lines.join("\n");
            Json(json!({ "text": text, "lines": lines })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("invalid image: {e}")),
    }
}

// ---------------------------------------------------------------------------
// POST /faces  (raw image bytes) -> {"faces": [{bbox, embedding, score}]}
// ---------------------------------------------------------------------------

async fn faces(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty image body");
    }
    let Some(engine) = state.inner.faces.as_ref() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "face model not loaded");
    };
    match engine.detect(&body) {
        Ok(found) => {
            let faces: Vec<_> = found
                .into_iter()
                .map(|f| {
                    json!({ "bbox": f.bbox, "embedding": f.embedding, "score": f.score })
                })
                .collect();
            Json(json!({ "faces": faces })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("invalid image: {e}")),
    }
}

// ---------------------------------------------------------------------------
// POST /inpaint  — magic eraser. Body is a length-framed blob (no multipart dep):
//   [u32 BE: mask_len][mask bytes (PNG)][image bytes].
// Returns the inpainted image as raw `image/png` bytes.
// ---------------------------------------------------------------------------

async fn inpaint(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(engine) = state.inner.inpaint.as_ref() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "inpainting model not loaded");
    };
    if body.len() < 4 {
        return err(StatusCode::BAD_REQUEST, "body too short");
    }
    let mask_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + mask_len {
        return err(StatusCode::BAD_REQUEST, "truncated mask frame");
    }
    let mask = &body[4..4 + mask_len];
    let image = &body[4 + mask_len..];
    if mask.is_empty() || image.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty mask or image");
    }
    match engine.inpaint(image, mask) {
        Ok(png) => ([(axum::http::header::CONTENT_TYPE, "image/png")], png).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("inpaint failed: {e}")),
    }
}
