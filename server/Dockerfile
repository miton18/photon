# Build context is the REPO ROOT (see docker-compose `build.context: .`), because
# the server crate path-depends on ../photon-plugin-proto and we also build the
# example plugin binaries here. Plugins are independent crates (no workspace), so
# each is built in its own dir and its binary copied into /plugins.

# ---- build server + plugins ----
FROM rust:1-bookworm AS build
WORKDIR /app

# Shared plugin contract + SDK (path deps), the server, and the example plugins.
COPY photon-plugin-proto ./photon-plugin-proto
COPY photon-plugin-sdk ./photon-plugin-sdk
COPY server ./server
COPY plugins ./plugins

# Server (depends on ../photon-plugin-proto; migrations are embedded at compile
# time via sqlx::migrate!("./migrations")).
RUN cd server && cargo build --release

# Example plugin binaries → /plugins (used as PHOTON_PLUGINS_DIR at runtime).
RUN mkdir -p /plugins \
 && for p in example-hello-job example-stats-route example-watermark-editor; do \
      (cd "plugins/$p" && cargo build --release) \
      && cp "plugins/$p/target/release/$p" /plugins/ ; \
    done

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
# ca-certificates: TLS for S3/SMTP (rustls roots) · ffmpeg: video transcoding (optional)
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/server/target/release/photon-server /usr/local/bin/photon-server
# Baked-in example plugins. The server discovers + launches these on startup
# because PHOTON_PLUGINS_DIR points here. Mount your own dir over /plugins (or
# unset PHOTON_PLUGINS_DIR in compose) to change the set.
COPY --from=build /plugins /plugins
# local filesystem object store lives here (mount a volume)
RUN mkdir -p /data
ENV PHOTON_DATA_DIR=/data
ENV PHOTON_PLUGINS_DIR=/plugins
EXPOSE 3000
CMD ["photon-server"]
