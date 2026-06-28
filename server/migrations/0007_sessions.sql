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
