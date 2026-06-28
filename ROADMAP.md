# Photon — Roadmap

Living backlog. Everything below is **not yet done**; see git history / CLAUDE.md
for what's shipped. Ranked by priority.

## A. Approved, not yet built

### A1 — Editor "full bake" pipeline (approved)
Today only a **plugin op** result is baked into the edited companion
(`POST /api/photos/{id}/plugin-edit/{plugin}/{op}?save=true`). The editor's own
develop params (crop, orientation/flip, light, color) are still **CSS-preview
only**, and the non-plugin "Save copy" button is a placeholder.

Goal: a server-side bake that applies, in order, to the **untouched original**:
1. Geometry — crop + rotate + flip (`image` crate).
2. Tonal — brightness / contrast / saturation / sepia / hue, re-implementing the
   UI's `filterFor()` math in Rust so the baked result matches the preview.
3. Plugin ops — chained via gRPC.
…then stores the result as the reserved `edited` companion (original kept,
re-edit overwrites), regenerates the thumbnail, and `load_display_blob` prefers it
everywhere. Plugin ops are just one parameter of the editor, baked with the rest.

### A2 — Plugin admin introspection (#5)
`GET /api/admin/plugins` (list, health, capabilities, routes), hot-reload (drop a
binary → rescan), enable/disable from the admin console. Discovery is
startup-only today.

### A3 — Plugin token hardening (#6)
Scope the plugin service account below admin (least privilege) instead of admin,
and revoke its old sessions on restart/shutdown (they currently accumulate).

## B. Blocked on input

### B1 — Live face detection
The face pipeline (detect → cluster → People → per-photo `/faces` + Lightbox/
InfoPanel overlay) is built and tested. Models chosen (permissive / commercial-OK):
**YuNet** detector (MIT, OpenCV Zoo) + **AuraFace** recognizer (Apache-2.0, fal.ai
ArcFace r100, 512-d, trained on commercial-clean data). `faces.rs` is wired +
compiles; `fetch-models.sh` pulls both by default. **Not yet live-verified** — run
the sidecar (release build + `fetch-models.sh` + a real face image, `PHOTON_ML_URL`
on the server) and confirm boxes/embeddings; adjust the YuNet decode if a tensor
name/shape differs. Blocked locally by disk (~99%).

## C. Polish / optional

- CI step that builds the example plugin binaries before the server tests (the
  plugin e2e tests silently skip when the binaries aren't built).
- Per-plugin request body-size limit on the route proxy (only the global 512 MB
  `DefaultBodyLimit` applies today).
- Verify the MCP tool catalog exposes the new endpoints (faces, editor plugins).
- Run a real `docker compose build` end-to-end (config is validated only).
