//! OIDC WEB LOGIN — the relying-party (RP) side of OpenID Connect.
//!
//! [`crate::mcp`] already implements the RESOURCE-SERVER side (validate a token a
//! client *presents*). This module implements the LOGIN side: obtain a token for
//! a browser user via the standard authorization-code flow.
//!
//! ## Config-gated / inert-by-default (mirrors [`crate::ml::MlClient`])
//! [`OidcLogin::from_env`] returns `None` unless ALL of `OIDC_ISSUER`,
//! `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET`, `OIDC_REDIRECT_URI` are set AND the
//! issuer's discovery document (`{issuer}/.well-known/openid-configuration`)
//! resolves the authorization/token/jwks endpoints. When `None`, the whole
//! feature is inert: `/api/auth/oidc/available` reports `false`, `/login` 404s,
//! and existing behavior/tests are unchanged (no network is ever touched).
//!
//! ## Flow
//! 1. `GET /api/auth/oidc/login` → mint `state`+`nonce`, persist them (DB, so the
//!    callback can land on any instance), 302 to [`OidcLogin::authorize_url`].
//! 2. IdP authenticates the user, redirects to `redirect_uri?code=&state=`.
//! 3. `GET /api/auth/oidc/callback` → validate `state`, exchange `code` at the
//!    token endpoint, validate the returned `id_token` (signature via JWKS, `iss`,
//!    `aud`, `nonce`, `exp`), map the email claim to a Photon user, mint a session.
//!
//! The token exchange + JWKS fetch are the only live-IdP steps and are NOT
//! exercised by automated tests (no IdP); the testable PIECES — config gating,
//! `authorize_url`, and the claims→user mapping — are unit-tested.

use serde::Deserialize;

/// Resolved OIDC relying-party configuration + endpoints, built by
/// [`OidcLogin::from_env`]. `None` (the default) means the login feature is off.
#[derive(Clone)]
pub struct OidcLogin {
    pub issuer: String,
    pub client_id: String,
    client_secret: String,
    pub redirect_uri: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    http: reqwest::Client,
}

/// The subset of the OIDC discovery document we need.
#[derive(Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// The token endpoint's response. We only require `id_token`.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub id_token: String,
}

/// Validated `id_token` claims we map onto a Photon user.
#[derive(Debug, Deserialize)]
pub struct IdTokenClaims {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Echoed `nonce` — compared against the value we stored at `/login`.
    #[serde(default)]
    pub nonce: Option<String>,
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// URL-encode a value for a query string (percent-encode everything that isn't an
/// unreserved char per RFC 3986). Small and dependency-free.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl OidcLogin {
    /// Build the RP config from env, fetching the issuer's discovery document.
    /// Returns `None` (feature inert) when any of issuer / client id / secret /
    /// redirect are missing OR discovery fails — never panics, never blocks
    /// startup on a hard error.
    pub async fn from_env() -> Option<Self> {
        let issuer = non_empty_env("OIDC_ISSUER")?;
        let client_id = non_empty_env("OIDC_CLIENT_ID")?;
        let client_secret = non_empty_env("OIDC_CLIENT_SECRET")?;
        let redirect_uri = non_empty_env("OIDC_REDIRECT_URI")?;

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .ok()?;

        let well_known = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let disco: Discovery = match http.get(&well_known).send().await {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("OIDC discovery decode failed ({e}); login disabled");
                    return None;
                }
            },
            Ok(r) => {
                tracing::warn!("OIDC discovery returned {}; login disabled", r.status());
                return None;
            }
            Err(e) => {
                tracing::warn!("OIDC discovery fetch failed ({e}); login disabled");
                return None;
            }
        };

        Some(Self {
            issuer,
            client_id,
            client_secret,
            redirect_uri,
            authorization_endpoint: disco.authorization_endpoint,
            token_endpoint: disco.token_endpoint,
            jwks_uri: disco.jwks_uri,
            http,
        })
    }

    /// Build the IdP authorization redirect URL for the given `state`/`nonce`.
    /// Requests `openid email profile` so the `id_token` carries an email to map.
    pub fn authorize_url(&self, state: &str, nonce: &str) -> String {
        format!(
            "{auth}?response_type=code&client_id={cid}&redirect_uri={redir}\
             &scope=openid%20email%20profile&state={state}&nonce={nonce}",
            auth = self.authorization_endpoint,
            cid = enc(&self.client_id),
            redir = enc(&self.redirect_uri),
            state = enc(state),
            nonce = enc(nonce),
        )
    }

    /// Exchange an authorization `code` for tokens at the token endpoint, using
    /// HTTP Basic client authentication. Returns the parsed [`TokenResponse`] or a
    /// human error (never panics).
    pub async fn exchange_code(&self, code: &str) -> Result<TokenResponse, String> {
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
        ];
        let resp = self
            .http
            .post(&self.token_endpoint)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&params)
            .send()
            .await
            .map_err(|e| format!("token endpoint request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("token endpoint returned {}", resp.status()));
        }
        resp.json::<TokenResponse>()
            .await
            .map_err(|e| format!("token response decode failed: {e}"))
    }

    /// Validate an `id_token`: signature (against the issuer's JWKS, fetched live),
    /// `iss == issuer`, `aud == client_id`, and `exp`. Returns the decoded claims.
    pub async fn verify_id_token(&self, id_token: &str) -> Result<IdTokenClaims, String> {
        use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

        let header = decode_header(id_token).map_err(|e| format!("bad id_token header: {e}"))?;
        let jwks: jsonwebtoken::jwk::JwkSet = self
            .http
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|e| format!("JWKS fetch failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JWKS decode failed: {e}"))?;
        // Require a matching `kid` (no "first key" fallback when a kid is present but
        // unmatched). Only fall back to a sole key when the token omits a kid AND the
        // IdP publishes exactly one.
        let jwk = match &header.kid {
            Some(kid) => jwks.find(kid).ok_or("no JWK matches the id_token kid")?,
            None if jwks.keys.len() == 1 => &jwks.keys[0],
            None => return Err("id_token has no kid and the JWKS has multiple keys".into()),
        };
        let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("JWK -> key failed: {e}"))?;

        // PIN to ASYMMETRIC signature algorithms — never trust the token header's
        // `alg`. Accepting a symmetric `HS*` here would let an attacker forge a token
        // using the (public) JWKS key as the HMAC secret; `none` is rejected by the
        // enum. (F4)
        let mut validation = Validation::new(Algorithm::RS256);
        validation.algorithms = vec![
            Algorithm::RS256, Algorithm::RS384, Algorithm::RS512,
            Algorithm::PS256, Algorithm::PS384, Algorithm::PS512,
            Algorithm::ES256, Algorithm::ES384, Algorithm::EdDSA,
        ];
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.client_id.as_str()]);
        validation.validate_exp = true;

        decode::<IdTokenClaims>(id_token, &key, &validation)
            .map(|d| d.claims)
            .map_err(|e| format!("id_token validation failed: {e}"))
    }
}

/// Map validated `id_token` claims onto a Photon user inside the live state and
/// mint a persisted session, returning the session bearer token.
///
/// Find-or-create by EMAIL (case-insensitive). A new email creates a passwordless,
/// non-admin user (name from the `name` claim, else the email's local-part). This
/// is the testable core of the callback — it takes already-validated claims so it
/// can be exercised without a live IdP.
pub async fn login_or_create_session(
    st: &crate::handlers::Shared,
    email: &str,
    name: Option<&str>,
) -> Result<String, String> {
    let email = email.trim();
    if email.is_empty() {
        return Err("id_token has no email claim".to_string());
    }
    let mut st = st.write().await;

    // Find an existing user by email (case-insensitive).
    let existing = st
        .users
        .values()
        .find(|u| u.email.eq_ignore_ascii_case(email))
        .map(|u| u.id.clone());

    let user_id = match existing {
        Some(id) => id,
        None => {
            let id = st.next_id("usr");
            let display = name
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| email.split('@').next().unwrap_or(email).to_string());
            let user = crate::models::User {
                id: id.clone(),
                name: display,
                email: email.to_string(),
                avatar_url: String::new(),
                password_hash: None,
                salt: String::new(),
                pepper: String::new(),
                is_admin: false,
                disabled: false,
                quota_mb: None,
                partners: Vec::new(),
                totp_secret: None,
            };
            st.users.insert(id.clone(), user);
            st.persist_user(&id).await;
            id
        }
    };

    // A disabled account must not receive a session, even via OIDC (password and
    // passkey logins already refuse disabled users).
    if st.users.get(&user_id).map(|u| u.disabled).unwrap_or(false) {
        return Err("account is disabled".to_string());
    }

    let token = st.create_session(&user_id);
    st.persist_session(&token).await;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acquire the process-wide OIDC env guard (shared with [`crate::mcp`]'s
    /// tests). Both modules read the same `OIDC_*` vars, so they must serialize
    /// against ONE lock, not two.
    async fn clean_env() -> tokio::sync::MutexGuard<'static, ()> {
        let g = crate::state::oidc_env_guard().lock().await;
        unsafe {
            std::env::remove_var("OIDC_ISSUER");
            std::env::remove_var("OIDC_CLIENT_ID");
            std::env::remove_var("OIDC_CLIENT_SECRET");
            std::env::remove_var("OIDC_REDIRECT_URI");
        }
        g
    }

    #[tokio::test]
    async fn from_env_unset_is_none() {
        let _g = clean_env().await;
        // No OIDC_* set ⇒ feature inert, no network attempted.
        assert!(OidcLogin::from_env().await.is_none());
    }

    #[tokio::test]
    async fn from_env_partial_is_none() {
        let _g = clean_env().await;
        unsafe {
            std::env::set_var("OIDC_ISSUER", "https://issuer.test");
            std::env::set_var("OIDC_CLIENT_ID", "photon");
            // missing secret + redirect ⇒ still inert (returns before any fetch)
        }
        let r = OidcLogin::from_env().await;
        unsafe {
            std::env::remove_var("OIDC_ISSUER");
            std::env::remove_var("OIDC_CLIENT_ID");
        }
        assert!(r.is_none());
    }

    #[test]
    fn authorize_url_has_required_params() {
        let cfg = OidcLogin {
            issuer: "https://issuer.test".into(),
            client_id: "photon client".into(),
            client_secret: "secret".into(),
            redirect_uri: "http://localhost:3000/api/auth/oidc/callback".into(),
            authorization_endpoint: "https://issuer.test/authorize".into(),
            token_endpoint: "https://issuer.test/token".into(),
            jwks_uri: "https://issuer.test/jwks".into(),
            http: reqwest::Client::new(),
        };
        let url = cfg.authorize_url("st-123", "no-nce");
        assert!(url.starts_with("https://issuer.test/authorize?"));
        assert!(url.contains("response_type=code"));
        // client_id is percent-encoded (space -> %20).
        assert!(url.contains("client_id=photon%20client"));
        assert!(url.contains("scope=openid%20email%20profile"));
        assert!(url.contains("state=st-123"));
        assert!(url.contains("nonce=no-nce"));
        assert!(url.contains(
            "redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fapi%2Fauth%2Foidc%2Fcallback"
        ));
    }
}
