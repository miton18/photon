//! WEBAUTHN / PASSKEYS — passwordless sign-in.
//!
//! Flow:
//! - After a normal password login the UI offers to **enroll** a passkey on the
//!   device (`POST /api/users/{id}/passkeys/register/{start,finish}`, self-only).
//! - On the login screen the user can then sign in with **no username**
//!   (`POST /api/login/passkey/{start,finish}`): a *discoverable* assertion whose
//!   credential carries the WebAuthn user handle, which we map back to a user.
//!
//! Offline-by-default like the ML sidecar/plugins: the relying-party instance is
//! built from `PHOTON_RP_ID`/`PHOTON_RP_ORIGIN` (localhost defaults) and stored as
//! `AppState.webauthn`. The short-lived begin-ceremony state is persisted in
//! Postgres (`webauthn_states`, single-use + TTL) so begin/finish is
//! multi-instance safe — mirroring the OIDC login flow. Credentials are stored in
//! `passkeys` and NEVER returned by any API.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::*;
use webauthn_rs::Webauthn;

use crate::auth::AuthUser;
use crate::handlers::Shared;
use crate::models::LoginResponse;

/// How long a begin-ceremony state stays valid (single-use anyway; this just
/// reaps abandoned flows and bounds replay of a stale challenge).
const STATE_TTL_SECS: i64 = 600;

/// Build the relying-party instance from the environment, or `None` (passkeys
/// disabled, every route degrades gracefully). `PHOTON_RP_ID` is the effective
/// domain (default `localhost`); `PHOTON_RP_ORIGIN` is the full page origin the
/// browser runs on (default the Vite dev origin `http://localhost:5173`). When
/// `rp_id` is `localhost` we also allow `http://localhost:3000` (the API origin)
/// for convenience; extra production origins go in `PHOTON_RP_ORIGIN_EXTRA`
/// (comma-separated). For production set `PHOTON_RP_ID=your.domain` and
/// `PHOTON_RP_ORIGIN=https://your.domain`.
pub fn build_webauthn() -> Option<Arc<Webauthn>> {
    let rp_id =
        std::env::var("PHOTON_RP_ID").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "localhost".into());
    let origin_str = std::env::var("PHOTON_RP_ORIGIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:5173".into());
    let origin = match Url::parse(&origin_str) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("passkeys: invalid PHOTON_RP_ORIGIN '{origin_str}': {e} — disabled");
            return None;
        }
    };
    let builder = match WebauthnBuilder::new(&rp_id, &origin) {
        Ok(b) => b.rp_name("Photon"),
        Err(e) => {
            tracing::warn!("passkeys: could not build relying party ({rp_id}, {origin_str}): {e}");
            return None;
        }
    };
    let mut builder = builder;
    // Dev convenience: when running on localhost, also accept the API origin.
    if rp_id == "localhost" {
        for extra in ["http://localhost:3000", "http://localhost:5173"] {
            if extra != origin_str {
                if let Ok(u) = Url::parse(extra) {
                    builder = builder.append_allowed_origin(&u);
                }
            }
        }
    }
    if let Ok(list) = std::env::var("PHOTON_RP_ORIGIN_EXTRA") {
        for o in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(u) = Url::parse(o) {
                builder = builder.append_allowed_origin(&u);
            }
        }
    }
    match builder.build() {
        Ok(w) => Some(Arc::new(w)),
        Err(e) => {
            tracing::warn!("passkeys: relying-party build failed: {e} — disabled");
            None
        }
    }
}

/// Pull the pool + relying party out of shared state. `503` when passkeys are
/// disabled (no RP) or there's no DB.
async fn deps(st: &Shared) -> Result<(crate::db::Persistence, Arc<Webauthn>), StatusCode> {
    let g = st.read().await;
    let pool = g.persistence.clone().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let wan = g.webauthn.clone().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok((pool, wan))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---- request/response shapes ----

#[derive(Serialize)]
pub struct StartResponse<T> {
    /// Opaque handle for the matching `finish` call (keys the server-side state).
    handle: String,
    /// The WebAuthn options to hand to `navigator.credentials.{create,get}`.
    options: T,
}

#[derive(Deserialize)]
pub struct FinishRegisterBody {
    handle: String,
    credential: RegisterPublicKeyCredential,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
pub struct FinishAuthBody {
    handle: String,
    credential: PublicKeyCredential,
}

/// Passkey metadata for the management UI (NEVER the credential material).
#[derive(Serialize)]
pub struct PasskeyView {
    id: String,
    name: Option<String>,
    created_at: String,
    last_used_at: Option<String>,
}

// Wrapper persisted in `webauthn_states.state`: the serialized ceremony state
// plus the WebAuthn user handle (registration only; auth derives it from the
// credential). Keeping the handle here means finish reuses the exact uid begin
// minted, without a deterministic-hash dependency.
#[derive(Serialize, Deserialize)]
struct StateEnvelope {
    st: serde_json::Value,
    #[serde(default)]
    uid: Option<String>,
}

// ---- registration (enroll a passkey on this device; self-only) ----

/// POST /api/users/{id}/passkeys/register/start
pub async fn register_start(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<StartResponse<serde_json::Value>>, StatusCode> {
    // A passkey is bound to the caller's own device — only self may enroll.
    if id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }
    let (pool, wan) = deps(&st).await?;
    let user = pool
        .get_user(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Reuse the user's existing WebAuthn handle, else mint a fresh random one.
    let uid: Uuid = match pool.wa_uid_for_user(&id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)? {
        Some(s) => Uuid::parse_str(&s).unwrap_or_else(|_| random_uuid()),
        None => random_uuid(),
    };

    // Don't offer to register a credential the device already holds.
    let exclude: Vec<CredentialID> = pool
        .passkeys_for_user(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Passkey>(v).ok())
        .map(|pk| pk.cred_id().clone())
        .collect();

    let (ccr, reg_state) = wan
        .start_passkey_registration(uid, &user.name, &user.name, Some(exclude))
        .map_err(|e| {
            tracing::warn!("passkey register start failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let handle = crate::state::random_hex(24);
    let envelope = StateEnvelope {
        st: serde_json::to_value(&reg_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        uid: Some(uid.to_string()),
    };
    persist_state(&pool, &handle, Some(&id), "reg", &envelope).await?;

    // webauthn-rs requests `residentKey: discouraged` by default, but a
    // usernameless (discoverable) sign-in needs a RESIDENT credential (one that
    // carries the user handle). Override the options we hand the browser to ask
    // for a resident key. The finish step validates the challenge/attestation and
    // doesn't care how the key was stored, so this is safe.
    let mut options = serde_json::to_value(&ccr).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(sel) = options.pointer_mut("/publicKey/authenticatorSelection") {
        sel["residentKey"] = serde_json::json!("required");
        sel["requireResidentKey"] = serde_json::json!(true);
    }
    Ok(Json(StartResponse { handle, options }))
}

/// POST /api/users/{id}/passkeys/register/finish
pub async fn register_finish(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<FinishRegisterBody>,
) -> Result<Json<PasskeyView>, StatusCode> {
    if id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }
    let (pool, wan) = deps(&st).await?;
    let (uid_owner, kind, env) = take_state(&pool, &body.handle).await?;
    if kind != "reg" || uid_owner.as_deref() != Some(id.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let reg_state: PasskeyRegistration =
        serde_json::from_value(env.st).map_err(|_| StatusCode::BAD_REQUEST)?;
    let wa_uid = env.uid.ok_or(StatusCode::BAD_REQUEST)?;

    let passkey = wan.finish_passkey_registration(&body.credential, &reg_state).map_err(|e| {
        tracing::warn!("passkey register finish failed: {e}");
        StatusCode::BAD_REQUEST
    })?;

    let cred_id = b64(passkey.cred_id().as_ref());
    let now = crate::state::now_rfc3339();
    let name = body.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let cred = serde_json::to_value(&passkey).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    pool.insert_passkey(&cred_id, &id, &wa_uid, name, cred, &now)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PasskeyView {
        id: cred_id,
        name: name.map(str::to_string),
        created_at: now,
        last_used_at: None,
    }))
}

/// GET /api/users/{id}/passkeys — list the user's registered passkeys (metadata).
pub async fn list(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<PasskeyView>>, StatusCode> {
    if id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }
    let pool = st.read().await.persistence.clone().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = pool.list_passkeys_meta(&id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name, created_at, last_used_at)| PasskeyView { id, name, created_at, last_used_at })
            .collect(),
    ))
}

/// DELETE /api/users/{id}/passkeys/{cred_id} — revoke one passkey.
pub async fn delete(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path((id, cred_id)): Path<(String, String)>,
) -> StatusCode {
    if id != actor.0 {
        return StatusCode::FORBIDDEN;
    }
    let pool = match st.read().await.persistence.clone() {
        Some(p) => p,
        None => return StatusCode::INTERNAL_SERVER_ERROR,
    };
    match pool.delete_passkey(&id, &cred_id).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ---- usernameless login (discoverable assertion) ----

/// POST /api/login/passkey/start — begin a usernameless passkey sign-in. PUBLIC.
pub async fn login_start(
    State(st): State<Shared>,
) -> Result<Json<StartResponse<RequestChallengeResponse>>, StatusCode> {
    let (pool, wan) = deps(&st).await?;
    let (rcr, auth_state) = wan.start_discoverable_authentication().map_err(|e| {
        tracing::warn!("passkey login start failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let handle = crate::state::random_hex(24);
    let envelope = StateEnvelope {
        st: serde_json::to_value(&auth_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        uid: None,
    };
    persist_state(&pool, &handle, None, "auth", &envelope).await?;
    Ok(Json(StartResponse { handle, options: rcr }))
}

/// POST /api/login/passkey/finish — complete a usernameless sign-in, minting a
/// session for the resolved user. PUBLIC. Returns the same `LoginResponse` shape
/// as password login.
pub async fn login_finish(
    State(st): State<Shared>,
    Json(body): Json<FinishAuthBody>,
) -> axum::response::Response {
    let (pool, wan) = match deps(&st).await {
        Ok(d) => d,
        Err(c) => return c.into_response(),
    };
    let (_owner, kind, env) = match take_state(&pool, &body.handle).await {
        Ok(t) => t,
        Err(c) => return c.into_response(),
    };
    if kind != "auth" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let auth_state: DiscoverableAuthentication = match serde_json::from_value(env.st) {
        Ok(s) => s,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // The credential carries the WebAuthn user handle — resolve it to our user
    // and that user's stored credentials.
    let (uid, _cred_id) = match wan.identify_discoverable_authentication(&body.credential) {
        Ok(v) => v,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let rows = match pool.passkeys_for_wa_uid(&uid.to_string()).await {
        Ok(r) if !r.is_empty() => r,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let user_id = rows[0].0.clone();
    let mut stored: Vec<Passkey> =
        rows.into_iter().filter_map(|(_, v)| serde_json::from_value(v).ok()).collect();
    let disco: Vec<DiscoverableKey> = stored.iter().map(DiscoverableKey::from).collect();

    let result = match wan.finish_discoverable_authentication(&body.credential, auth_state, &disco) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("passkey login finish failed: {e}");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Persist the bumped signature counter on whichever credential was used.
    let now = crate::state::now_rfc3339();
    for pk in stored.iter_mut() {
        if pk.update_credential(&result).is_some() {
            let cred_id = b64(pk.cred_id().as_ref());
            if let Ok(v) = serde_json::to_value(&*pk) {
                let _ = pool.touch_passkey(&cred_id, v, &now).await;
            }
            break;
        }
    }

    // Mint a session exactly like password login.
    let user = match pool.get_user(&user_id).await {
        Ok(Some(u)) if !u.disabled => u,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let mut guard = st.write().await;
    let token = guard.create_session(&user.id);
    guard.persist_session(&token).await;
    let user = guard.public_user(&user);
    Json(LoginResponse { token, user }).into_response()
}

// ---- helpers ----

fn random_uuid() -> Uuid {
    let mut bytes = [0u8; 16];
    // OS CSPRNG (same primitive as session tokens). Fall back to a hash of a fresh
    // token if getrandom ever fails (never expected).
    if getrandom::getrandom(&mut bytes).is_err() {
        let h = crate::state::random_hex(16);
        for (i, b) in h.as_bytes().iter().take(16).enumerate() {
            bytes[i] = *b;
        }
    }
    Uuid::from_bytes(bytes)
}

async fn persist_state(
    pool: &crate::db::Persistence,
    handle: &str,
    user_id: Option<&str>,
    kind: &str,
    env: &StateEnvelope,
) -> Result<(), StatusCode> {
    let v = serde_json::to_value(env).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    pool.insert_webauthn_state(handle, user_id, kind, v, &crate::state::now_rfc3339())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Consume a begin-ceremony state (single-use) and reject it if expired.
async fn take_state(
    pool: &crate::db::Persistence,
    handle: &str,
) -> Result<(Option<String>, String, StateEnvelope), StatusCode> {
    // Best-effort: reap stale states so the table can't grow unbounded.
    let cutoff = crate::state::rfc3339_secs_ago(STATE_TTL_SECS);
    let _ = pool.cleanup_webauthn_states(&cutoff).await;

    let (user_id, kind, raw) = pool
        .take_webauthn_state(handle)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let env: StateEnvelope = serde_json::from_value(raw).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((user_id, kind, env))
}
