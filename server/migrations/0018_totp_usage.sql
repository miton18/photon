-- TOTP REPLAY PROTECTION — record the last TOTP time-step a user successfully
-- authenticated with, so the same 6-digit code (or an earlier one) can't be
-- replayed within its validity window. A code corresponds to a monotonically
-- increasing step (unix_time / period); login rejects a step <= the stored one.
CREATE TABLE IF NOT EXISTS totp_usage (
    user_id   TEXT PRIMARY KEY,
    last_step BIGINT NOT NULL
);
