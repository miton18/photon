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
