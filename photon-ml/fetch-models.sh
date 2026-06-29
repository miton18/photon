#!/usr/bin/env sh
# Fetch the ONNX/rten model files Photon ML needs into PHOTON_ML_MODELS_DIR.
#
# This is OPTIONAL: models can also be mounted via a volume (see docker-compose).
# The service runs without any models present — each missing model just disables
# its capability (/health reports which are loaded) and that endpoint returns 503.
#
# Capabilities + expected default file names (override via env, see README):
#   CLIP   : clip_image.onnx        (image encoder, ViT-B-32, 512-d)
#            clip_text.onnx         (text encoder,  ViT-B-32, 512-d)
#   OCR    : text-detection.rten    (ocrs detection model)
#            text-recognition.rten  (ocrs recognition model)
#   FACES  : scrfd.onnx             (AuraFace SCRFD detector, 640x640)
#            auraface.onnx          (AuraFace/ArcFace embedder, 112x112, 512-d)
#
# Sources to obtain these (no single canonical mirror; pick what you trust):
#   - OCR (ocrs): the official robertknight/ocrs-models release assets, e.g.
#       https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten
#       https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten
#   - CLIP ViT-B-32 image/text ONNX: export from open_clip / Hugging Face
#       (e.g. the `Qdrant/clip-ViT-B-32-vision` / `...-text` ONNX exports) or
#       export yourself with torch.onnx; ensure the image encoder takes
#       [1,3,224,224] f32 (CLIP mean/std) and the text encoder takes [1,77] int32.
#   - FACES: the AuraFace pack (fal.ai, Apache-2.0, commercial-OK) — SCRFD
#       detector + ArcFace ResNet100 512-d embedder, fetched BY DEFAULT
#       (detector -> scrfd.onnx, embedder -> auraface.onnx). Override
#       FACE_DET_URL / FACE_REC_URL for a different pack.
#
# Usage:  PHOTON_ML_MODELS_DIR=/models ./fetch-models.sh
set -eu

DIR="${PHOTON_ML_MODELS_DIR:-/models}"
mkdir -p "$DIR"

dl() {
  url="$1"; out="$2"
  if [ -f "$DIR/$out" ]; then
    echo "exists: $out"
    return 0
  fi
  echo "fetching: $out"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$DIR/$out"
  else
    wget -qO "$DIR/$out" "$url"
  fi
}

OCR_DET_URL="${OCR_DET_URL:-https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten}"
OCR_REC_URL="${OCR_REC_URL:-https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten}"

# FACE models — the full AuraFace pack (fal.ai, Apache-2.0, commercial-OK):
# SCRFD detector + ArcFace ResNet100 512-d embedder, both ONNX on Hugging Face.
FACE_DET_URL="${FACE_DET_URL:-https://huggingface.co/fal/AuraFace-v1/resolve/main/scrfd_10g_bnkps.onnx}"
FACE_REC_URL="${FACE_REC_URL:-https://huggingface.co/fal/AuraFace-v1/resolve/main/glintr100.onnx}"

# OCR + FACE models have stable public mirrors — fetch them by default.
dl "$OCR_DET_URL" "${PHOTON_OCR_DETECTION_MODEL:-text-detection.rten}"
dl "$OCR_REC_URL" "${PHOTON_OCR_RECOGNITION_MODEL:-text-recognition.rten}"
dl "$FACE_DET_URL" "${PHOTON_FACE_DETECTION_MODEL:-scrfd.onnx}"
dl "$FACE_REC_URL" "${PHOTON_FACE_RECOGNITION_MODEL:-auraface.onnx}"

# CLIP ONNX exports vary by source/license; set the *_URL envs (to a source you
# are licensed to use) to enable. No default URL is shipped for CLIP.
[ -n "${CLIP_IMAGE_URL:-}" ] && dl "$CLIP_IMAGE_URL" "${PHOTON_CLIP_IMAGE_MODEL:-clip_image.onnx}" || true
[ -n "${CLIP_TEXT_URL:-}" ]  && dl "$CLIP_TEXT_URL"  "${PHOTON_CLIP_TEXT_MODEL:-clip_text.onnx}"   || true

# INPAINT (magic eraser) — a LaMa-style ONNX model. Inputs (in order): image
# [1,3,H,W] in [0,1] + mask [1,1,H,W] in {0,1}; output [1,3,H,W]; H/W multiple of 8.
# Licensing varies (e.g. LaMa weights are Places2-trained), so NO default URL is
# shipped — set INPAINT_MODEL_URL to a model you are licensed to use.
[ -n "${INPAINT_MODEL_URL:-}" ] && dl "$INPAINT_MODEL_URL" "${PHOTON_INPAINT_MODEL:-inpaint.onnx}" || true

# SEGMENT (tap-to-select) — a SAM-family two-graph ONNX export (encoder + decoder),
# e.g. MobileSAM/EfficientSAM (Apache-2.0). No default URL is shipped; set
# SEGMENT_ENCODER_URL / SEGMENT_DECODER_URL to enable.
[ -n "${SEGMENT_ENCODER_URL:-}" ] && dl "$SEGMENT_ENCODER_URL" "${PHOTON_SEGMENT_ENCODER:-sam_encoder.onnx}" || true
[ -n "${SEGMENT_DECODER_URL:-}" ] && dl "$SEGMENT_DECODER_URL" "${PHOTON_SEGMENT_DECODER:-sam_decoder.onnx}" || true

echo "done. models in $DIR:"
ls -la "$DIR"
