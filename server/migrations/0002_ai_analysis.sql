-- AI ANALYSIS (import stage 4): derived, non-authoritative photo metadata.
-- OCR text, machine context/scene tags, detected people, and an analyzed flag.
-- Vec fields are stored as jsonb arrays (mirrors companions/photo_ids/shares).
ALTER TABLE photos ADD COLUMN IF NOT EXISTS ocr_text  TEXT;
ALTER TABLE photos ADD COLUMN IF NOT EXISTS ai_tags   JSONB   NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE photos ADD COLUMN IF NOT EXISTS ai_people JSONB   NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE photos ADD COLUMN IF NOT EXISTS analyzed  BOOLEAN NOT NULL DEFAULT FALSE;
