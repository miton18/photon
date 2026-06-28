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
