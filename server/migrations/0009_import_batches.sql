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
