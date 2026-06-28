-- Photon — consolidated initial schema.
-- Squashed from the original 0001-0018 migrations before the first release
-- (no deployed database existed yet, so the history was collapsed into one).

-- ===================================================================
-- from: 0001_init.sql
-- ===================================================================
-- Photon domain persistence (Postgres). Stores ALL domain data EXCEPT media
-- blobs (image/video bytes stay on the filesystem/S3 via the StorageBackend).
-- Plain, idempotent-friendly SQL: every object uses IF NOT EXISTS. Rich
-- sub-structures (exif/overrides/companions/shares/photo_ids/...) are stored as
-- jsonb to keep the schema tractable; scalar columns are used where queried.

CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    email           TEXT NOT NULL,
    avatar_url      TEXT NOT NULL DEFAULT '',
    password_hash   TEXT,
    salt            TEXT NOT NULL DEFAULT '',
    pepper          TEXT NOT NULL DEFAULT '',
    is_admin        BOOLEAN NOT NULL DEFAULT FALSE,
    disabled        BOOLEAN NOT NULL DEFAULT FALSE,
    quota_mb        BIGINT
);

CREATE TABLE IF NOT EXISTS groups (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    owner_id    TEXT NOT NULL,
    member_ids  JSONB NOT NULL DEFAULT '[]'::jsonb
);

CREATE TABLE IF NOT EXISTS photos (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT NOT NULL,
    filename    TEXT NOT NULL,
    seed        BIGINT NOT NULL DEFAULT 0,
    kind        TEXT NOT NULL,
    exif        JSONB NOT NULL DEFAULT '{}'::jsonb,
    overrides   JSONB NOT NULL DEFAULT '{}'::jsonb,
    companions  JSONB NOT NULL DEFAULT '[]'::jsonb,
    archived    BOOLEAN NOT NULL DEFAULT FALSE,
    deleted_at  TEXT,
    backed_up   BOOLEAN NOT NULL DEFAULT FALSE,
    thumb_url   TEXT,
    size_mb     DOUBLE PRECISION NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS albums (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    owner_id    TEXT NOT NULL,
    cover_seed  BIGINT NOT NULL DEFAULT 0,
    photo_ids   JSONB NOT NULL DEFAULT '[]'::jsonb,
    shares      JSONB NOT NULL DEFAULT '[]'::jsonb
);

CREATE TABLE IF NOT EXISTS timeline_prefs (
    user_id     TEXT PRIMARY KEY,
    prefs       JSONB NOT NULL DEFAULT '{}'::jsonb
);

-- Single-row config tables. The lone row is keyed by a fixed id = 1.
CREATE TABLE IF NOT EXISTS storage_settings (
    id          INTEGER PRIMARY KEY DEFAULT 1,
    settings    JSONB NOT NULL,
    CONSTRAINT storage_settings_singleton CHECK (id = 1)
);

CREATE TABLE IF NOT EXISTS smtp_config (
    id          INTEGER PRIMARY KEY DEFAULT 1,
    config      JSONB,
    CONSTRAINT smtp_config_singleton CHECK (id = 1)
);

CREATE TABLE IF NOT EXISTS invites (
    token       TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    inviter_id  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    accepted    BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS reset_tokens (
    token       TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    used        BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS vaults (
    user_id     TEXT PRIMARY KEY,
    pin_hash    TEXT,
    salt        TEXT NOT NULL DEFAULT '',
    photo_ids   JSONB NOT NULL DEFAULT '[]'::jsonb
);

-- ===================================================================
-- from: 0002_ai_analysis.sql
-- ===================================================================
-- AI ANALYSIS (import stage 4): derived, non-authoritative photo metadata.
-- OCR text, machine context/scene tags, detected people, and an analyzed flag.
-- Vec fields are stored as jsonb arrays (mirrors companions/photo_ids/shares).
ALTER TABLE photos ADD COLUMN IF NOT EXISTS ocr_text  TEXT;
ALTER TABLE photos ADD COLUMN IF NOT EXISTS ai_tags   JSONB   NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE photos ADD COLUMN IF NOT EXISTS ai_people JSONB   NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE photos ADD COLUMN IF NOT EXISTS analyzed  BOOLEAN NOT NULL DEFAULT FALSE;

-- ===================================================================
-- from: 0003_embeddings.sql
-- ===================================================================
-- CONTEXT RECOGNITION (CLIP) — open-vocabulary semantic search embeddings.
--
-- The ML sidecar (photon-ml/) produces a 512-dim L2-normalized CLIP image
-- embedding per photo (over its thumbnail). We store it TWO ways:
--
--   1. `clip_embedding`  float8[]      — the PORTABLE source of truth that the
--      Rust server reads/writes via plain sqlx (Vec<f64> <-> Vec<f32>), with no
--      extra crate dependency. This is what `db.rs` upsert/load use, and what
--      backs the in-memory cosine ranking.
--
--   2. `clip_embedding_vec`  vector(512) — the PRODUCTION approximate-nearest-
--      neighbour index column (pgvector). Cosine ranking today happens in-memory
--      over `AppState.photos` (all rows are in memory via write-through), so the
--      server does not yet query this column; it is provisioned + indexed here so
--      a future DB-side `ORDER BY clip_embedding_vec <=> $query` can scale beyond
--      memory without another migration. Requires the pgvector image
--      (pgvector/pgvector:pg17 — see docker-compose.yml).
--
-- The whole feature is gated on PHOTON_ML_URL at runtime; with ML disabled these
-- columns simply stay NULL and nothing reads/writes them.

CREATE EXTENSION IF NOT EXISTS vector;

ALTER TABLE photos ADD COLUMN IF NOT EXISTS clip_embedding     float8[];
ALTER TABLE photos ADD COLUMN IF NOT EXISTS clip_embedding_vec vector(512);

-- ivfflat ANN index on the pgvector column for cosine distance. `lists` is small
-- here (suitable for modest libraries); tune upward for very large collections.
CREATE INDEX IF NOT EXISTS photos_clip_embedding_vec_idx
    ON photos USING ivfflat (clip_embedding_vec vector_cosine_ops)
    WITH (lists = 100);

-- ===================================================================
-- from: 0004_partners.sql
-- ===================================================================
-- PARTNER relationship (directed read grant). `partners` holds the user ids
-- this user has granted partner access to: when A lists B, B can read all of
-- A's LIVE photos (trash/archive/vault excluded) in B's timeline and search.
-- Stored as a jsonb array (mirrors groups.member_ids / album.photo_ids).
ALTER TABLE users ADD COLUMN IF NOT EXISTS partners JSONB NOT NULL DEFAULT '[]'::jsonb;

-- ===================================================================
-- from: 0005_faces.sql
-- ===================================================================
-- FACE RECOGNITION — detected faces + their People clusters.
--
-- Faces carry a SENSITIVE biometric embedding (ArcFace 512-d from the ML
-- sidecar). It is stored server-side ONLY and never returned by any API; here it
-- lives in two columns mirroring the CLIP-embedding approach (migration 0003):
--
--   1. `embedding`     float8[]    — PORTABLE source of truth the server reads/
--      writes via plain sqlx (Vec<f64> <-> Vec<f32>), no extra crate dependency.
--   2. `embedding_vec` vector(512) — provisioned + indexed pgvector column for a
--      future DB-side ANN match; clustering today runs in-memory over the loaded
--      faces (write-through keeps memory authoritative), so the server does not
--      query this column yet.
--
-- The whole feature is gated on PHOTON_ML_URL at runtime; with ML disabled these
-- tables simply stay empty and nothing reads/writes them.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS people (
    id              TEXT PRIMARY KEY,
    owner_id        TEXT NOT NULL,
    name            TEXT,
    face_ids        JSONB NOT NULL DEFAULT '[]'::jsonb,
    cover_photo_id  TEXT,
    cover_bbox      JSONB
);
CREATE INDEX IF NOT EXISTS people_owner_idx ON people (owner_id);

CREATE TABLE IF NOT EXISTS faces (
    id            TEXT PRIMARY KEY,
    photo_id      TEXT NOT NULL,
    owner_id      TEXT NOT NULL,
    bbox          JSONB NOT NULL,
    score         DOUBLE PRECISION NOT NULL DEFAULT 0,
    person_id     TEXT,
    embedding     float8[],
    embedding_vec vector(512)
);
CREATE INDEX IF NOT EXISTS faces_owner_idx ON faces (owner_id);
CREATE INDEX IF NOT EXISTS faces_photo_idx ON faces (photo_id);

-- ivfflat ANN index on the pgvector column for cosine distance (future DB-side
-- clustering / nearest-face lookups). `lists` is small here; tune up for scale.
CREATE INDEX IF NOT EXISTS faces_embedding_vec_idx
    ON faces USING ivfflat (embedding_vec vector_cosine_ops)
    WITH (lists = 100);

-- ===================================================================
-- from: 0006_kinship.sql
-- ===================================================================
-- KINSHIP — directed family/social links between People (face clusters).
--
-- Each Person carries a JSONB array of `{person_id, relation}` edges pointing at
-- OTHER clusters of the same owner ("that person is this one's <relation>"),
-- kept reciprocal by the server. Person ids are regenerated on every re-cluster,
-- so the server remaps these edges by face-set overlap in memory (see
-- `AppState::cluster_faces`) and write-through persists the result here.
ALTER TABLE people
    ADD COLUMN IF NOT EXISTS relationships JSONB NOT NULL DEFAULT '[]'::jsonb;

-- ===================================================================
-- from: 0007_sessions.sql
-- ===================================================================
-- SHARED SESSION STORE — bearer-token sessions live in Postgres so authentication
-- works across multiple Photon instances behind a load balancer (a token minted
-- on instance A must be valid on instance B). Each instance keeps an in-memory
-- cache but falls back to this table on a miss; logout deletes the row.
CREATE TABLE IF NOT EXISTS sessions (
    token      TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user_idx ON sessions (user_id);

-- ===================================================================
-- from: 0008_id_sequence.sql
-- ===================================================================
-- Multi-instance-safe id generation. The in-memory AtomicU64 counter is per
-- instance, so two instances would mint colliding ids (alb_5 on both). A shared
-- Postgres sequence makes every minted id unique across the cluster. Starts high
-- to avoid colliding with the small seed ids (alb_1, ph_3, …).
CREATE SEQUENCE IF NOT EXISTS photon_id_seq START 1000000;

-- ===================================================================
-- from: 0009_import_batches.sql
-- ===================================================================
-- Import-batch progress, persisted so `GET /api/uploads/{id}` works across
-- instances (the polling request may hit a different node than the upload).
-- Processing is synchronous per request now, so a batch is written once with its
-- final item states.
CREATE TABLE IF NOT EXISTS import_batches (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL,
    album_id   TEXT,
    items      JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TEXT NOT NULL
);

-- ===================================================================
-- from: 0010_duplicate_groups.sql
-- ===================================================================
-- Near-duplicate detection results, persisted so `GET /api/users/{id}/duplicates`
-- works under the Postgres-first model (handlers read a fresh DB snapshot per
-- request; the in-memory `duplicate_groups` cache is rebuilt from here on load).
-- One row per owner; `groups` is the JSON-encoded Vec<Vec<photo_id>> (each inner
-- array is a cluster of >= 2 near-duplicates). Recomputed by the daily job.
CREATE TABLE IF NOT EXISTS duplicate_groups (
    owner_id TEXT PRIMARY KEY,
    groups   JSONB NOT NULL DEFAULT '[]'::jsonb
);

-- ===================================================================
-- from: 0011_job_runs.sql
-- ===================================================================
-- Background-job run history, surfaced in the admin console's "Run history".
-- Every job execution (cron OR on-demand) appends a row here on completion.
CREATE TABLE IF NOT EXISTS job_runs (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL,
    outcome     TEXT NOT NULL,          -- success | failed | partial
    items       BIGINT NOT NULL DEFAULT 0,
    started_at  TEXT NOT NULL,          -- RFC3339
    duration_ms BIGINT NOT NULL DEFAULT 0,
    trigger     TEXT NOT NULL           -- cron | manual
);
CREATE INDEX IF NOT EXISTS job_runs_started_idx ON job_runs (started_at DESC);

-- ===================================================================
-- from: 0012_public_links.sql
-- ===================================================================
-- PUBLIC ALBUM LINKS — an album owner can mint a random token that grants
-- read-only, no-account access to that album and its live photos (gated by the
-- `features.public_links` flag). The mapping `token -> album_id` lives in
-- Postgres so any instance behind the load balancer can resolve a public link.
-- Revoking a link deletes its row.
CREATE TABLE IF NOT EXISTS public_links (
    token      TEXT PRIMARY KEY,
    album_id   TEXT NOT NULL,
    created_at TEXT
);
CREATE INDEX IF NOT EXISTS public_links_album_idx ON public_links (album_id);

-- ===================================================================
-- from: 0013_totp.sql
-- ===================================================================
-- TOTP two-factor auth. A NULL `totp_secret` means the user is NOT enrolled;
-- a non-NULL value is the user's base32-encoded TOTP secret (enrollment is
-- confirmed only after a code verifies — see handlers `2fa/verify`). The secret
-- is a credential and is NEVER serialized into any API response (like
-- password_hash); the API only ever exposes whether 2FA is enabled.
ALTER TABLE users ADD COLUMN IF NOT EXISTS totp_secret TEXT;

-- ===================================================================
-- from: 0014_oidc_states.sql
-- ===================================================================
-- OIDC LOGIN STATE STORE — the authorization-code (relying-party) login flow
-- generates a random `state` + `nonce` at `/api/auth/oidc/login` and must
-- validate them again when the IdP redirects back to `/api/auth/oidc/callback`.
-- Storing them in Postgres (not in a per-instance map) makes the flow
-- multi-instance-safe: the browser may be redirected back to a DIFFERENT Photon
-- instance behind the load balancer than the one that started the flow. Rows are
-- single-use (deleted when consumed) and short-lived (a cleanup drops anything
-- older than the TTL); `created_at` is an RFC 3339 string, matching the rest of
-- the schema's timestamp convention.
CREATE TABLE IF NOT EXISTS oidc_states (
    state      TEXT PRIMARY KEY,
    nonce      TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- ===================================================================
-- from: 0015_passkeys.sql
-- ===================================================================
-- WEBAUTHN / PASSKEYS. Each row is one registered passkey credential bound to a
-- user. `cred` is the serialized webauthn-rs `Passkey` (public key + signature
-- counter + metadata) — a credential, NEVER returned by any API. `wa_uid` is the
-- WebAuthn user handle (a UUID) we mint once per user and reuse for all their
-- passkeys, so a usernameless (discoverable) login can map the credential's
-- userHandle back to a user. `name` is a user-facing device label.
CREATE TABLE IF NOT EXISTS passkeys (
    id           TEXT PRIMARY KEY,          -- base64url credential id
    user_id      TEXT NOT NULL,
    wa_uid       TEXT NOT NULL,             -- WebAuthn user handle (UUID), stable per user
    name         TEXT,                      -- "MacBook Touch ID", "iPhone", …
    cred         JSONB NOT NULL,            -- serialized Passkey (credential — never exposed)
    created_at   TEXT NOT NULL,
    last_used_at TEXT
);
CREATE INDEX IF NOT EXISTS passkeys_user_idx ON passkeys (user_id);
CREATE INDEX IF NOT EXISTS passkeys_wa_uid_idx ON passkeys (wa_uid);

-- TRANSIENT WEBAUTHN CEREMONY STATE — the begin step of a registration or
-- (discoverable) authentication produces a server-side state that the finish step
-- must validate against. Stored here (not in a per-instance map) so the ceremony
-- is multi-instance safe, exactly like `oidc_states`. Rows are single-use (deleted
-- when consumed) and short-lived (a cleanup drops anything older than the TTL).
-- `state` is the serialized webauthn-rs PasskeyRegistration / DiscoverableAuthentication.
CREATE TABLE IF NOT EXISTS webauthn_states (
    id         TEXT PRIMARY KEY,            -- random handle returned to the client
    user_id    TEXT,                        -- set for registration; NULL for usernameless auth
    kind       TEXT NOT NULL,               -- 'reg' | 'auth'
    state      JSONB NOT NULL,
    created_at TEXT NOT NULL
);

-- ===================================================================
-- from: 0016_people_studio.sql
-- ===================================================================
-- PEOPLE STUDIO — manual face/person curation that must SURVIVE automatic
-- re-clustering. Person cluster ids regenerate on every re-cluster, but FACES are
-- stable, so the authoritative manual signals live on faces:
--
--   * `ignored`        — the user marked this detection a non-face / intruder.
--                        It is excluded from clustering AND from People entirely.
--   * `assigned_label` — a stable identity tag. All faces sharing a label are the
--                        SAME person, authoritatively (this is how "move face to
--                        person X" and "merge A into B" persist past re-clustering:
--                        clustering groups by label first, embedding second).
--
-- Person-level curation that isn't naturally pinned to one face is carried across
-- re-clusters by face-set overlap (like the existing name/relationships):
--   * `birthdate`    — ISO date; drives the displayed age.
--   * `hidden`       — keep the cluster but don't surface it in People.
--   * `cover_locked` — the user picked a cover face; don't auto-overwrite it.
ALTER TABLE faces  ADD COLUMN IF NOT EXISTS ignored        BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE faces  ADD COLUMN IF NOT EXISTS assigned_label TEXT;
ALTER TABLE people ADD COLUMN IF NOT EXISTS birthdate      TEXT;
ALTER TABLE people ADD COLUMN IF NOT EXISTS hidden         BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE people ADD COLUMN IF NOT EXISTS cover_locked   BOOLEAN NOT NULL DEFAULT false;

CREATE INDEX IF NOT EXISTS faces_assigned_label_idx ON faces (assigned_label);

-- ===================================================================
-- from: 0017_face_confirmed.sql
-- ===================================================================
-- FACE REVIEW — a low-confidence (suspicious) detection is flagged for review in
-- the People Studio. The user either DENIES it (an intruder → `ignored`, handled
-- by the existing ignore path) or APPROVES it: `confirmed = true` means a human
-- has confirmed this face belongs to its person, so it is no longer surfaced as
-- "needs review" regardless of the detector score. Confirmation is authoritative
-- and stable (faces persist), and approval also pins the face to its person's
-- identity label, so the decision survives a future re-cluster.
ALTER TABLE faces ADD COLUMN IF NOT EXISTS confirmed BOOLEAN NOT NULL DEFAULT false;

-- ===================================================================
-- from: 0018_totp_usage.sql
-- ===================================================================
-- TOTP REPLAY PROTECTION — record the last TOTP time-step a user successfully
-- authenticated with, so the same 6-digit code (or an earlier one) can't be
-- replayed within its validity window. A code corresponds to a monotonically
-- increasing step (unix_time / period); login rejects a step <= the stored one.
CREATE TABLE IF NOT EXISTS totp_usage (
    user_id   TEXT PRIMARY KEY,
    last_step BIGINT NOT NULL
);
