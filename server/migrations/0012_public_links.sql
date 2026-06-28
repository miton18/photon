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
