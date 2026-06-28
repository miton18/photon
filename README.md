# Photon

**Your photos, on your server.** Photon is a self-hosted photo & video library —
a private home for a lifetime of memories that you, and only you, control.

![Photon timeline — a dense, justified photo grid with albums, people, places and search](docs/screenshot-timeline.jpg)

> [!WARNING]
> **Photon is alpha software — do not run it in production.**
> It is under active development: APIs, the database schema, and the storage
> layout **can still change without notice or a migration path**, and **not all
> advertised features are fully implemented yet**. Don't make it the only home for
> photos you can't afford to lose — keep your own backups. Expect rough edges,
> breaking changes, and the occasional missing piece. You've been warned. 🚧

## Why Photon?

Photos are among the most personal data we own — the faces of our families, where
we live, where we go. Yet most of us hand them to a handful of foreign cloud
providers in exchange for convenience, with no real say over how they're stored,
scanned, or monetised.

Photon is built in **France** as a sovereign, privacy-first alternative:

- **Your data stays yours.** Originals live on *your* filesystem or *your* S3
  bucket — never in a database, never on someone else's cloud. Originals are
  immutable; edits are stored as non-destructive overrides.
- **Privacy by design.** Face recognition, OCR and semantic search all run on
  **your own server** via a local ML sidecar — embeddings never leave the
  machine. Every AI feature is **off by default** and degrades gracefully when
  disabled, so nothing phones home unless you choose to turn it on.
- **No lock-in.** Open source, open formats, a documented REST + MCP API, and a
  subprocess plugin system so you can extend it without forking the core.
- **European sovereignty.** Built to be hosted in Europe, on infrastructure you
  trust, under rules you understand.

It's a love letter to the idea that self-hosting your memories should be as
pleasant as the big-cloud experience — a dense, fast timeline, a real editor,
albums and sharing, people and places — without giving anything up.

## What's in the box

Monorepo (not a Cargo workspace — each Rust crate is independent with path deps):

| Folder        | Stack                          | Description                          |
| ------------- | ------------------------------ | ------------------------------------ |
| `server/`     | Rust (axum + tokio + sqlx/PG)  | Backend API server                   |
| `ui/`         | Svelte 5 + Vite + TS           | Web frontend                         |
| `mobileapp/`  | Flutter (Dart)                 | Mobile application                   |
| `photon-ml/`  | Rust (axum + `ort`/ONNX)       | ML sidecar: CLIP, OCR, face recognition |
| `photon-plugin-sdk/`, `photon-plugin-proto/`, `plugins/` | Rust | Subprocess plugin SDK + examples |
| `companion/`  | Rust (desktop)                 | Watched-folder auto-backup companion |

## Architecture

- **Metadata** (users, groups, albums, shares & roles, photo EXIF + DB overrides,
  companions, timeline prefs, vault, trash/archive, storage config) lives in
  **PostgreSQL**. The server falls back to an in-memory store when `DATABASE_URL`
  is unset, so it runs out-of-the-box for demos.
- **Image & video blobs** live on the **filesystem** by default, or in an **S3**
  bucket — configurable from the UI (Storage settings). S3 has two modes: *backup*
  (hourly job copies new media to S3, filesystem stays source of truth) and
  *replacement* (S3 is the primary object store).
- **Originals are never modified.** The editor writes DB *metadata overrides* that
  win over the immutable EXIF. EXIF is extracted on upload with pure-Rust crates
  (`kamadak-exif` + `image`), never ImageMagick.
- **Companions:** uploading a JPG+RAW pair (same date + base name) collapses to one
  displayed photo; the RAW becomes a downloadable companion.

### Domain features

- Albums shared with individual users or groups; each share has a role
  (**viewer** read-only, or **contributor** who can add their own photos while
  keeping ownership). Search sees every photo of an album you can access.
- Per-user timeline visibility for shared albums (global + per-album override).
- **Vault:** per-user PIN-locked album, hidden from timeline and search; the PIN is
  required on every open.
- **Trash** with configurable retention (default 7 days) and **Archive** (hidden
  from timeline/search but kept).
- Device-aware **transcoding** of photos/videos (right resolution + format).

## Getting started

### Server (Rust)

```bash
cd server
cargo run          # http://0.0.0.0:3000  — GET /api/health
# optional Postgres: DATABASE_URL=postgres://user:pass@localhost/photon cargo run
```

Seed demo logins (password = first name): **alice/alice**, **bob/bob**,
carol/carol, dave/dave. Alice is the admin. Her demo vault PIN is `1234`.

#### MCP server (agent access)

The server embeds a **Model Context Protocol** endpoint at `POST /mcp`
(JSON-RPC 2.0) exposing the whole REST surface as MCP tools an agent can call,
secured with OIDC (with a Photon-session fallback for offline/demo). See
[`server/docs/MCP.md`](server/docs/MCP.md) for the endpoint, OIDC env vars, the
demo login-token fallback, and the full tool catalog (`tools/list` is the
source of truth).

#### Plugins (subprocess, go-plugin style)

The server can be extended with **standalone plugin binaries** launched beside
it and driven over gRPC on a Unix socket — adding **jobs**, **routes**
(`/api/plugins/{id}/…`), or **photo-editing ops**. A plugin author implements
**one trait** and calls `serve(...)`; the SDK hides the handshake, socket, gRPC
server, logging, and the API client. Plugins log with `tracing` (logs flow back
into the server's logger), report multi-step job progress, and can call back
into the Photon API via `PhotonClient` using a token the host injects.

**Off by default**, gated on `PHOTON_PLUGINS_DIR` (like the ML sidecar). A
crashing plugin degrades that one call and never takes the server down.

```bash
# Plugins are independent crates (no workspace) — build each in its own dir.
(cd plugins/example-hello-job && cargo build --release)
mkdir -p /opt/photon/plugins && cp plugins/example-hello-job/target/release/example-hello-job /opt/photon/plugins/
(cd server && PHOTON_PLUGINS_DIR=/opt/photon/plugins cargo run)
```

See [`photon-plugin-sdk/README.md`](photon-plugin-sdk/README.md) for the full
guide, and `plugins/example-*` for building examples.

### UI (Svelte + Vite)

```bash
cd ui
pnpm install
pnpm dev           # http://localhost:5173  (expects the server on :3000)
```

Override the API base with `VITE_API_URL` (e.g. `VITE_API_URL=http://host:3000 pnpm dev`).
The UI is a faithful port of the Photon design system (Slate direction): dense
timeline with a justified grid, lightbox + non-destructive editor, selection,
sharing/groups, vault, and storage settings. Use the sidebar avatar to switch the
demo account and see per-user timelines/sharing.

### Mobile app (Flutter)

Flutter is not yet installed on this machine. After installing the Flutter SDK:

```bash
cd mobileapp
flutter create .   # generates android/ ios/ and platform scaffolding
flutter pub get
flutter run
```

> The Android emulator reaches the host server at `http://10.0.2.2:3000`.
> Override with `--dart-define=SERVER_URL=...`.

## Context recognition (CLIP)

Photon supports **open-vocabulary** semantic photo search — natural-language
queries like `yellow car` / `voiture jaune` matched against photo *content*, not
just keywords/tags. It uses **CLIP** embeddings served by a small **ML sidecar**
(`photon-ml/`, a pure-**Rust** axum service running ONNX models via
[`ort`](https://github.com/pykeio/ort); CLIP ViT-B-32, dim 512, CPU-friendly).
The same sidecar also powers OCR and on-device **face recognition** (detect →
cluster → name) — all locally, embeddings never leave your server.

**How it works**

- On upload, after thumbnail generation, the server sends the thumbnail to the
  sidecar `POST /embed/image` and stores the returned 512-dim L2-normalized
  vector on the photo (`clip_embedding`, server-side only).
- On search, `GET /api/users/{id}/search?q=...` embeds the query text via
  `POST /embed/text` and ranks the user's access-scoped (and facet-filtered)
  candidates by **cosine similarity** to their image embeddings, best match
  first. Add `&semantic=false` to force the legacy keyword/substring search.
- In Postgres mode the vector is persisted as a portable `float8[]` column
  (`clip_embedding`); migration `0003_embeddings.sql` also provisions a
  `vector(512)` column + ivfflat index (pgvector) as the production ANN index.

**Offline by default.** The whole feature is gated on the `PHOTON_ML_URL` env
var. When it is **unset** the server never calls the sidecar, no network is used,
and behavior is identical to keyword-only search — so builds/tests run fully
offline. `docker compose up` wires it automatically
(`PHOTON_ML_URL=http://photon-ml:8000`, db image `pgvector/pgvector:pg17`).
See `photon-ml/README.md` for the sidecar API and how to run it standalone.

## Contributing

Photon is early and **maintained by a single author**, so contribution is
intentionally lightweight for now:

- 🐞 **Found a bug?** Open a [GitHub issue](../../issues) with steps to reproduce.
- 💡 **Want a feature?** Start a [GitHub Discussion](../../discussions) — that's
  where ideas are gathered and prioritised before any code is written.

Please **don't open pull requests for new features** without discussing them
first; an unsolicited PR may not be mergeable if it doesn't fit the direction.
Small, obvious fixes (typos, clear bugs) are always welcome.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the details.

## License

To be announced. Until a `LICENSE` file is added, all rights are reserved by the
author — you may read and self-host the code, but redistribution terms are not
yet finalised.
