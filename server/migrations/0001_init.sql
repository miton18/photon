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
