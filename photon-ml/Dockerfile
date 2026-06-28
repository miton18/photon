# Photon ML sidecar — pure-Rust CLIP + OCR + face service (ONNX Runtime via ort).
#
# Multi-stage: build a release binary, then ship it on a slim Debian runtime.
# ONNX Runtime is fetched and bundled at build time by `ort`'s `download-binaries`
# feature (statically linked into the binary), so the runtime image needs no
# preinstalled libonnxruntime.

# ---- build stage ----------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src

# `ort` downloads the onnxruntime binaries during the build, which needs network
# and TLS roots.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates pkg-config \
 && rm -rf /var/lib/apt/lists/*

# Cache dependencies first.
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
 && cargo build --release || true
RUN rm -rf src

# Build the real sources.
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- runtime stage --------------------------------------------------------
FROM debian:bookworm-slim AS runtime
WORKDIR /app

# Minimal runtime deps: TLS roots. libgcc/libstdc++ are already present in slim.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/photon-ml /usr/local/bin/photon-ml
# Optional model fetch helper (run at build, or mount /models via a volume).
COPY fetch-models.sh /usr/local/bin/fetch-models.sh
RUN chmod +x /usr/local/bin/fetch-models.sh

# Models are loaded from here at startup; mount a volume or bake them in.
ENV PHOTON_ML_MODELS_DIR=/models
# CLIP / OCR / face config (same swap-without-code spirit as the old sidecar).
ENV PHOTON_CLIP_MODEL=ViT-B-32 \
    PHOTON_OCR_LANGS=en,fr \
    PHOTON_FACE_MODEL=buffalo_l \
    RUST_LOG=info

EXPOSE 8000
CMD ["photon-ml"]
