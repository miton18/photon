-- KINSHIP — directed family/social links between People (face clusters).
--
-- Each Person carries a JSONB array of `{person_id, relation}` edges pointing at
-- OTHER clusters of the same owner ("that person is this one's <relation>"),
-- kept reciprocal by the server. Person ids are regenerated on every re-cluster,
-- so the server remaps these edges by face-set overlap in memory (see
-- `AppState::cluster_faces`) and write-through persists the result here.
ALTER TABLE people
    ADD COLUMN IF NOT EXISTS relationships JSONB NOT NULL DEFAULT '[]'::jsonb;
