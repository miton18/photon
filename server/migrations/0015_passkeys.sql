-- WEBAUTHN / PASSKEYS. Each row is one registered passkey credential bound to a
-- user. `cred` is the serialized webauthn-rs `Passkey` (public key + signature
-- counter + metadata) — a credential, NEVER returned by any API. `wa_uid` is the
-- WebAuthn user handle (a UUID) we mint once per user and reuse for all their
-- passkeys, so a usernameless (discoverable) login can map the credential's
-- userHandle back to a user. `name` is a user-facing device label.
CREATE TABLE IF NOT EXISTS passkeys (
    id           TEXT PRIMARY KEY,          -- base64url credential id
    user_id      TEXT NOT NULL,
    wa_uid       TEXT NOT NULL,             -- WebAuthn user handle (UUID), stable per user
    name         TEXT,                      -- "MacBook Touch ID", "iPhone", …
    cred         JSONB NOT NULL,            -- serialized Passkey (credential — never exposed)
    created_at   TEXT NOT NULL,
    last_used_at TEXT
);
CREATE INDEX IF NOT EXISTS passkeys_user_idx ON passkeys (user_id);
CREATE INDEX IF NOT EXISTS passkeys_wa_uid_idx ON passkeys (wa_uid);

-- TRANSIENT WEBAUTHN CEREMONY STATE — the begin step of a registration or
-- (discoverable) authentication produces a server-side state that the finish step
-- must validate against. Stored here (not in a per-instance map) so the ceremony
-- is multi-instance safe, exactly like `oidc_states`. Rows are single-use (deleted
-- when consumed) and short-lived (a cleanup drops anything older than the TTL).
-- `state` is the serialized webauthn-rs PasskeyRegistration / DiscoverableAuthentication.
CREATE TABLE IF NOT EXISTS webauthn_states (
    id         TEXT PRIMARY KEY,            -- random handle returned to the client
    user_id    TEXT,                        -- set for registration; NULL for usernameless auth
    kind       TEXT NOT NULL,               -- 'reg' | 'auth'
    state      JSONB NOT NULL,
    created_at TEXT NOT NULL
);
