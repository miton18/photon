# photon-ml — CLIP embedding + OCR + face sidecar (Rust)

A small, CPU-friendly **Rust** ([axum](https://github.com/tokio-rs/axum)) service
that gives Photon **open-vocabulary context recognition**: it turns images and
text into vectors in the **same** [CLIP](https://github.com/mlfoundations/open_clip)
embedding space, so a free-text query like `yellow car` / `voiture jaune` can be
matched against photo *content* by cosine similarity — no fixed label set.

It also does **OCR** (text extraction) so any text inside a photo (signs, labels,
documents) becomes searchable in Photon, and **face detection + embedding** so
the server can cluster faces into People.

This is a drop-in replacement for the former Python/FastAPI sidecar: the HTTP
contract (paths + request/response JSON) is **identical**, so the Rust server's
`MlClient` (`server/src/ml.rs`) is unchanged. The whole project is now Rust.

The server (`server/`) talks to this sidecar over HTTP and is gated entirely on
the `PHOTON_ML_URL` env var: when unset, the server never calls this service and
runs exactly as before (offline).

## Inference stack

| Capability | Library | Runtime | Models |
|------------|---------|---------|--------|
| CLIP image/text embeddings | [`ort`](https://crates.io/crates/ort) 2.0.0-rc.10 (ONNX Runtime), [`instant-clip-tokenizer`](https://crates.io/crates/instant-clip-tokenizer) | ONNX Runtime (bundled) | CLIP image + text encoder ONNX |
| OCR | [`ocrs`](https://crates.io/crates/ocrs) 0.12 (+ [`rten`](https://crates.io/crates/rten) 0.24) | rten (pure Rust) | ocrs detection + recognition `.rten` |
| Faces | [`ort`](https://crates.io/crates/ort) (AuraFace: SCRFD + ArcFace) | ONNX Runtime (bundled) | AuraFace pack (Apache-2.0) ONNX |

ONNX Runtime is fetched and **statically bundled into the binary** at build time
via `ort`'s `download-binaries` feature — no preinstalled `libonnxruntime` is
required at runtime. Image decoding/preprocessing uses the [`image`](https://crates.io/crates/image)
crate.

## Endpoints

| Method | Path           | Body                       | Response                          |
|--------|----------------|----------------------------|-----------------------------------|
| GET    | `/health`      | —                          | `{"status":"ok","model":…,"dim":512,"ocr":{…},"faces":{…},"clip_loaded":bool}` |
| POST   | `/embed/image` | raw image bytes            | `{"embedding": [f32; dim]}` (L2-normalized) |
| POST   | `/embed/text`  | `{"text": "yellow car"}`   | `{"embedding": [f32; dim]}` (L2-normalized) |
| POST   | `/ocr`         | raw image bytes            | `{"text": "joined\ntext", "lines": ["per","line"]}` |
| POST   | `/faces`       | raw image bytes            | `{"faces": [{"bbox":[x,y,w,h], "embedding":[f32; 512], "score":0.99}]}` (embeddings L2-normalized) |

Embeddings are L2-normalized, so a dot product equals cosine similarity. `/ocr`
returns the recognized text both joined (newline-separated) and per-line; an
image with no detectable text yields an empty string + empty list (HTTP 200).
`/faces` returns one entry per detected face (`bbox` is `[x,y,w,h]` in pixels); an
image with no faces yields an empty list (HTTP 200).

Face embeddings are **sensitive**: the server stores them server-side only and
never returns them in any API response. Clustering into People happens in the
Rust server.

**Resilience:** every endpoint is non-panicking. A missing model file does not
crash the service — that capability is simply disabled, `/health` reports it as
`loaded: false`, and the endpoint returns HTTP 503 with `{"error": …}`. An
undecodable image returns 400. The server's `MlClient` treats any non-200 as "ML
unavailable", so the exact error body is not load-bearing.

## Models & configuration

Models are loaded **once at startup** from a single directory (`PHOTON_ML_MODELS_DIR`,
default `/models`). Per-capability file names are overridable by env so models can
be swapped without code changes (same spirit as the original sidecar):

| Env var | Default | Purpose |
|---------|---------|---------|
| `PHOTON_ML_MODELS_DIR` | `/models` | directory all model files are read from |
| `PHOTON_CLIP_MODEL` | `ViT-B-32` | CLIP name reported by `/health` |
| `PHOTON_CLIP_IMAGE_MODEL` | `clip_image.onnx` | CLIP image encoder ONNX file |
| `PHOTON_CLIP_TEXT_MODEL` | `clip_text.onnx` | CLIP text encoder ONNX file |
| `PHOTON_OCR_LANGS` | `en,fr` | OCR langs reported by `/health` |
| `PHOTON_OCR_DETECTION_MODEL` | `text-detection.rten` | ocrs detection model |
| `PHOTON_OCR_RECOGNITION_MODEL` | `text-recognition.rten` | ocrs recognition model |
| `PHOTON_FACE_MODEL` | `auraface` | face pack name reported by `/health` |
| `PHOTON_FACE_DETECTION_MODEL` | `scrfd.onnx` | AuraFace SCRFD detector ONNX (Apache-2.0) |
| `PHOTON_FACE_RECOGNITION_MODEL` | `auraface.onnx` | AuraFace/ArcFace 512-d embedder ONNX (Apache-2.0) |
| `RUST_LOG` | `info` | tracing filter |

Expected model formats:

- **CLIP image encoder**: input `[1,3,224,224]` f32 (CLIP mean/std normalized,
  RGB, CHW), output `[1, dim]` (512 for ViT-B-32). `dim` is inferred at startup
  from the text encoder and reported by `/health`.
- **CLIP text encoder**: input `[1,77]` int32 token ids (CLIP BPE,
  `<start>` … `<end>`, padded to 77), output `[1, dim]`.
- **OCR**: ocrs `.rten` (or `.onnx`) detection + recognition models.
- **SCRFD** (detection, AuraFace): input `[1,3,640,640]` f32, `(px-127.5)/128`
  RGB CHW (letterboxed top-left). Score + bbox-distance heads over strides 8/16/32
  (2 anchors); `distance2bbox` decode, NMS-filtered, mapped back to source pixels.
- **AuraFace** (embedding, ArcFace ResNet100): input `[1,3,112,112]` f32,
  `(px-127.5)/127.5` RGB CHW, output `[1,512]`, L2-normalized here.

### Obtaining models

The service builds and starts with **no models present** (capabilities just stay
disabled). To enable them, either mount a `/models` volume or bake models in.
`fetch-models.sh` downloads the OCR models AND the face models (YuNet + SFace
from the OpenCV Zoo, permissive licenses) by default, plus any CLIP models whose
source URLs you provide:

```bash
PHOTON_ML_MODELS_DIR=./models ./fetch-models.sh          # OCR + faces by default
CLIP_IMAGE_URL=… CLIP_TEXT_URL=… \
  PHOTON_ML_MODELS_DIR=./models ./fetch-models.sh        # + CLIP
```

See the comments in `fetch-models.sh` for sources (ocrs-models for OCR; the fal.ai
AuraFace pack for faces; open_clip / ONNX exports for CLIP). Override
`FACE_DET_URL` / `FACE_REC_URL` to use a different (licensed) face pack.

## Build & run

```bash
cargo build --release          # bundles ONNX Runtime (needs network at build)
PHOTON_ML_MODELS_DIR=./models ./target/release/photon-ml
```

With Docker (also wired into the root `docker-compose.yml` as service `photon-ml`):

```bash
docker build -t photon-ml .
docker run -p 8000:8000 -v "$PWD/models:/models" photon-ml
```

> Note: `cargo build` / `docker build` need network access for `ort` to download
> the ONNX Runtime binaries. The downloaded runtime is bundled into the binary,
> so the runtime container itself needs no `libonnxruntime`.

Smoke test:

```bash
curl localhost:8000/health
curl -X POST localhost:8000/embed/text -H 'content-type: application/json' \
     -d '{"text":"yellow car"}'
curl -X POST localhost:8000/embed/image \
     -H 'content-type: application/octet-stream' --data-binary @photo.jpg
curl -X POST localhost:8000/ocr \
     -H 'content-type: application/octet-stream' --data-binary @photo.jpg
curl -X POST localhost:8000/faces \
     -H 'content-type: application/octet-stream' --data-binary @photo.jpg
```

To enable the feature in the server, set `PHOTON_ML_URL=http://photon-ml:8000`
(compose does this for you).
