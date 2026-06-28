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
