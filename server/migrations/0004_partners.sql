-- PARTNER relationship (directed read grant). `partners` holds the user ids
-- this user has granted partner access to: when A lists B, B can read all of
-- A's LIVE photos (trash/archive/vault excluded) in B's timeline and search.
-- Stored as a jsonb array (mirrors groups.member_ids / album.photo_ids).
ALTER TABLE users ADD COLUMN IF NOT EXISTS partners JSONB NOT NULL DEFAULT '[]'::jsonb;
