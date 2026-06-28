-- OIDC LOGIN STATE STORE — the authorization-code (relying-party) login flow
-- generates a random `state` + `nonce` at `/api/auth/oidc/login` and must
-- validate them again when the IdP redirects back to `/api/auth/oidc/callback`.
-- Storing them in Postgres (not in a per-instance map) makes the flow
-- multi-instance-safe: the browser may be redirected back to a DIFFERENT Photon
-- instance behind the load balancer than the one that started the flow. Rows are
-- single-use (deleted when consumed) and short-lived (a cleanup drops anything
-- older than the TTL); `created_at` is an RFC 3339 string, matching the rest of
-- the schema's timestamp convention.
CREATE TABLE IF NOT EXISTS oidc_states (
    state      TEXT PRIMARY KEY,
    nonce      TEXT NOT NULL,
    created_at TEXT NOT NULL
);
