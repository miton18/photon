-- TOTP two-factor auth. A NULL `totp_secret` means the user is NOT enrolled;
-- a non-NULL value is the user's base32-encoded TOTP secret (enrollment is
-- confirmed only after a code verifies — see handlers `2fa/verify`). The secret
-- is a credential and is NEVER serialized into any API response (like
-- password_hash); the API only ever exposes whether 2FA is enabled.
ALTER TABLE users ADD COLUMN IF NOT EXISTS totp_secret TEXT;
