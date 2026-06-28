-- FACE REVIEW — a low-confidence (suspicious) detection is flagged for review in
-- the People Studio. The user either DENIES it (an intruder → `ignored`, handled
-- by the existing ignore path) or APPROVES it: `confirmed = true` means a human
-- has confirmed this face belongs to its person, so it is no longer surfaced as
-- "needs review" regardless of the detector score. Confirmation is authoritative
-- and stable (faces persist), and approval also pins the face to its person's
-- identity label, so the decision survives a future re-cluster.
ALTER TABLE faces ADD COLUMN IF NOT EXISTS confirmed BOOLEAN NOT NULL DEFAULT false;
