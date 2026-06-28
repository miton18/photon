-- Background-job run history, surfaced in the admin console's "Run history".
-- Every job execution (cron OR on-demand) appends a row here on completion.
CREATE TABLE IF NOT EXISTS job_runs (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL,
    outcome     TEXT NOT NULL,          -- success | failed | partial
    items       BIGINT NOT NULL DEFAULT 0,
    started_at  TEXT NOT NULL,          -- RFC3339
    duration_ms BIGINT NOT NULL DEFAULT 0,
    trigger     TEXT NOT NULL           -- cron | manual
);
CREATE INDEX IF NOT EXISTS job_runs_started_idx ON job_runs (started_at DESC);
