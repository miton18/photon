-- Near-duplicate detection results, persisted so `GET /api/users/{id}/duplicates`
-- works under the Postgres-first model (handlers read a fresh DB snapshot per
-- request; the in-memory `duplicate_groups` cache is rebuilt from here on load).
-- One row per owner; `groups` is the JSON-encoded Vec<Vec<photo_id>> (each inner
-- array is a cluster of >= 2 near-duplicates). Recomputed by the daily job.
CREATE TABLE IF NOT EXISTS duplicate_groups (
    owner_id TEXT PRIMARY KEY,
    groups   JSONB NOT NULL DEFAULT '[]'::jsonb
);
