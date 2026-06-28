//! Embedded Model Context Protocol (MCP) server for Photon.
//!
//! A self-contained, hand-rolled JSON-RPC 2.0 implementation served over HTTP at
//! `POST /mcp`. It exposes the WHOLE Photon REST surface as MCP *tools* an agent
//! can call, reusing the existing [`AppState`] business logic (no duplicated
//! rules). Every `tools/call` is authenticated and the resolved Photon user
//! becomes the actor, against which the SAME ownership/role rules as the REST
//! handlers are enforced.
//!
//! ## Protocol
//! Methods handled:
//! - `initialize` → protocolVersion / serverInfo / capabilities.
//! - `tools/list` → the full tool catalog (name, description, JSON-Schema input).
//! - `tools/call` → `{name, arguments}` → dispatch → MCP `content` block.
//! - `notifications/initialized` → accepted (no response for notifications).
//! - anything else → JSON-RPC method-not-found error.
//!
//! ## Auth (see [`resolve_actor`])
//! - `Authorization: Bearer <token>`.
//! - When OIDC is configured (env `OIDC_ISSUER` + `OIDC_AUDIENCE` + a key via
//!   `OIDC_JWKS_JSON` or `OIDC_HS256_SECRET`), the JWT is validated and its
//!   `email`/`sub` claim is mapped to a Photon user.
//! - Otherwise a Photon **session token** from `POST /api/login` is accepted
//!   (offline/demo fallback).

use axum::{Json, extract::State, http::HeaderMap, http::header};
use serde_json::{Value, json};

use crate::handlers::Shared;
use crate::state::AppState;

/// The MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

// ---------------------------------------------------------------------------
// JSON-RPC error codes
// ---------------------------------------------------------------------------
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
/// Implementation-defined: authentication required / failed.
const UNAUTHORIZED: i64 = -32001;

/// Build a JSON-RPC error object `{code, message}`.
fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

/// Build a JSON-RPC success result.
fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Wrap a tool result JSON into the MCP `content` block shape.
fn tool_content(result: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

// ---------------------------------------------------------------------------
// Authentication / actor resolution
// ---------------------------------------------------------------------------

/// The authenticated actor for an MCP request: the resolved Photon user id plus
/// whether they are an admin. Ownership/role checks run against this id exactly
/// as the REST handlers do for the equivalent operation.
#[derive(Debug, Clone)]
pub struct Actor {
    pub user_id: String,
    pub is_admin: bool,
}

/// OIDC configuration read from the environment, when present.
struct OidcConfig {
    issuer: String,
    audience: String,
    /// Offline/test HS256 shared secret, if configured.
    hs256_secret: Option<String>,
    /// Static JWKS JSON (for offline RSA/EC verification), if configured. In
    /// production the issuer's discovery document is fetched to obtain JWKS;
    /// here we accept a pre-fetched JWKS so no network is required at runtime.
    jwks_json: Option<String>,
}

impl OidcConfig {
    /// Read OIDC config from env. Returns `None` (=> session fallback) unless
    /// BOTH `OIDC_ISSUER` and `OIDC_AUDIENCE` are set.
    fn from_env() -> Option<Self> {
        let issuer = non_empty_env("OIDC_ISSUER")?;
        let audience = non_empty_env("OIDC_AUDIENCE")?;
        Some(OidcConfig {
            issuer,
            audience,
            hs256_secret: non_empty_env("OIDC_HS256_SECRET"),
            jwks_json: non_empty_env("OIDC_JWKS_JSON"),
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Standard OIDC/JWT claims we care about for actor mapping.
#[derive(serde::Deserialize)]
struct Claims {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    sub: Option<String>,
}

/// Extract a bearer token from `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let tok = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?
        .trim();
    if tok.is_empty() { None } else { Some(tok.to_string()) }
}

/// Resolve the request's actor.
///
/// 1. If OIDC is configured, validate the JWT (signature + `iss` + `aud` +
///    `exp`) and map its `email` (else `sub`) claim to a Photon user.
/// 2. Otherwise accept a Photon session token (`session_user`).
///
/// Returns `Err(message)` when no valid credential resolves to a known user.
pub fn resolve_actor(st: &AppState, headers: &HeaderMap) -> Result<Actor, String> {
    let token = bearer(headers).ok_or("missing Authorization: Bearer <token>")?;

    if let Some(cfg) = OidcConfig::from_env() {
        let claims = verify_oidc(&cfg, &token)?;
        // Map email first (case-insensitive), then fall back to sub == user id.
        if let Some(email) = claims.email.as_deref() {
            if let Some(u) = st
                .users
                .values()
                .find(|u| u.email.eq_ignore_ascii_case(email))
            {
                return Ok(Actor {
                    user_id: u.id.clone(),
                    is_admin: u.is_admin,
                });
            }
        }
        if let Some(sub) = claims.sub.as_deref() {
            if let Some(u) = st.users.get(sub) {
                return Ok(Actor {
                    user_id: u.id.clone(),
                    is_admin: u.is_admin,
                });
            }
        }
        return Err("valid OIDC token but no matching Photon user".to_string());
    }

    // Demo/offline fallback: a Photon login session token.
    match st.session_user(&token) {
        Some(uid) => {
            let is_admin = st.users.get(uid).map(|u| u.is_admin).unwrap_or(false);
            Ok(Actor {
                user_id: uid.to_string(),
                is_admin,
            })
        }
        None => Err("invalid or expired session token".to_string()),
    }
}

/// Validate a JWT per the OIDC config and return its claims. Supports an HS256
/// shared secret (offline/testing) or a static JWKS (RS256/ES256). Validates
/// signature + `iss` + `aud` + `exp`.
fn verify_oidc(cfg: &OidcConfig, token: &str) -> Result<Claims, String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

    let mut validation;
    let key: DecodingKey;

    if let Some(secret) = &cfg.hs256_secret {
        validation = Validation::new(Algorithm::HS256);
        key = DecodingKey::from_secret(secret.as_bytes());
    } else if let Some(jwks) = &cfg.jwks_json {
        // Static JWKS path: pick the JWK matching the token's `kid` (or the
        // first one) and build a DecodingKey from it.
        let header = decode_header(token).map_err(|e| format!("bad token header: {e}"))?;
        let set: jsonwebtoken::jwk::JwkSet =
            serde_json::from_str(jwks).map_err(|e| format!("OIDC_JWKS_JSON invalid: {e}"))?;
        // Require a matching `kid`; fall back to a sole key only when none is given.
        let jwk = match &header.kid {
            Some(kid) => set.find(kid).ok_or("no JWK matches the token kid")?,
            None if set.keys.len() == 1 => &set.keys[0],
            None => return Err("token has no kid and the JWKS has multiple keys".to_string()),
        };
        key = DecodingKey::from_jwk(jwk).map_err(|e| format!("JWK -> key failed: {e}"))?;
        // PIN to asymmetric algorithms — never accept a symmetric `HS*` (forgeable
        // with the public key) or `none` on the JWKS path. (F4)
        validation = Validation::new(Algorithm::RS256);
        validation.algorithms = vec![
            Algorithm::RS256, Algorithm::RS384, Algorithm::RS512,
            Algorithm::PS256, Algorithm::PS384, Algorithm::PS512,
            Algorithm::ES256, Algorithm::ES384, Algorithm::EdDSA,
        ];
    } else {
        return Err(
            "OIDC configured but no verification key (set OIDC_HS256_SECRET or OIDC_JWKS_JSON)"
                .to_string(),
        );
    }

    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_audience(&[cfg.audience.as_str()]);
    // `exp` is validated by default; be explicit for clarity.
    validation.validate_exp = true;

    decode::<Claims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| format!("JWT validation failed: {e}"))
}

// ---------------------------------------------------------------------------
// Tool registry
// ---------------------------------------------------------------------------

/// One MCP tool: a stable `name`, an agent-facing `description` (states what it
/// does, its auth/ownership requirements, and its args), a JSON-Schema
/// `input_schema`, and the dispatch `handler`.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON-Schema for `arguments`.
    pub input_schema: fn() -> Value,
    /// Async dispatch: `(shared, actor, arguments) -> Result<json, message>`.
    pub handler: ToolFn,
}

/// Boxed async tool handler.
type ToolFn = fn(Shared, Actor, Value) -> futures_box::BoxFut;

/// Tiny local boxing helper so we can store async fns without pulling a crate.
mod futures_box {
    use std::future::Future;
    use std::pin::Pin;
    pub type BoxFut =
        Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>;
    /// Box an async block into [`BoxFut`].
    pub fn boxed<F>(f: F) -> BoxFut
    where
        F: Future<Output = Result<serde_json::Value, String>> + Send + 'static,
    {
        Box::pin(f)
    }
}

/// Convenience: object schema with the given `(name, schema)` properties; the
/// listed `required` names are marked required.
fn obj_schema(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in props {
        map.insert((*k).to_string(), v.clone());
    }
    json!({
        "type": "object",
        "properties": Value::Object(map),
        "required": required,
        "additionalProperties": false
    })
}

fn s_string() -> Value {
    json!({ "type": "string" })
}
fn s_string_desc(d: &str) -> Value {
    json!({ "type": "string", "description": d })
}
fn s_bool() -> Value {
    json!({ "type": "boolean" })
}
fn s_int() -> Value {
    json!({ "type": "integer" })
}
fn s_str_array() -> Value {
    json!({ "type": "array", "items": { "type": "string" } })
}
fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

// Argument extraction helpers ------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}
fn arg_str_opt(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}
fn arg_bool_opt(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}
fn arg_str_vec(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let arr = args
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("missing required array argument '{key}'"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| format!("'{key}' must be an array of strings"))
        })
        .collect()
}

/// Authorize that `actor` may act on `target` user-scoped data: the actor must
/// be that user OR an admin. Used for vault/timeline/search/storage scoping so
/// an MCP caller can't reach another user's private data.
fn require_self_or_admin(actor: &Actor, target: &str) -> Result<(), String> {
    if actor.user_id == target || actor.is_admin {
        Ok(())
    } else {
        Err(format!(
            "forbidden: actor '{}' may not access user '{}' data",
            actor.user_id, target
        ))
    }
}

/// Authorize an admin-only operation.
fn require_admin(actor: &Actor) -> Result<(), String> {
    if actor.is_admin {
        Ok(())
    } else {
        Err("forbidden: admin privileges required".to_string())
    }
}

fn to_value<T: serde::Serialize>(t: &T) -> Result<Value, String> {
    serde_json::to_value(t).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

/// The full tool catalog. Order is stable for deterministic `tools/list`.
pub fn tools() -> Vec<Tool> {
    use futures_box::boxed;
    vec![
        // ---- Users ----
        Tool {
            name: "list_users",
            description: "List all users (id, name, email, flags). Admin-only. \
                          Secrets are never returned.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let st = st.read().await;
                let mut users: Vec<_> = st.users.values().cloned().collect();
                users.sort_by(|a, b| a.id.cmp(&b.id));
                to_value(&users)
            }),
        },
        Tool {
            name: "get_user",
            description: "Get a single user by id. Args: {id}. The actor may fetch \
                          their own record; admins may fetch any.",
            input_schema: || obj_schema(&[("id", s_string_desc("user id, e.g. usr_alice"))], &["id"]),
            handler: |st, actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                require_self_or_admin(&actor, &id)?;
                let st = st.read().await;
                let u = st.users.get(&id).ok_or("user not found")?;
                to_value(u)
            }),
        },
        Tool {
            name: "create_user",
            description: "Create a passwordless user. Admin-only. Args: {name, email, \
                          is_admin?}. The user must later set a password via a reset token.",
            input_schema: || obj_schema(
                &[("name", s_string()), ("email", s_string()), ("is_admin", s_bool())],
                &["name", "email"],
            ),
            handler: |st, actor, a| boxed(async move {
                require_admin(&actor)?;
                let name = arg_str(&a, "name")?;
                let email = arg_str(&a, "email")?;
                let is_admin = arg_bool_opt(&a, "is_admin").unwrap_or(false);
                let mut st = st.write().await;
                let id = st.next_id("usr");
                let user = crate::models::User {
                    id: id.clone(), name, email,
                    avatar_url: String::new(),
                    password_hash: None, salt: String::new(), pepper: String::new(),
                    is_admin, disabled: false, quota_mb: None, partners: Vec::new(),
                    totp_secret: None,
                };
                st.users.insert(id.clone(), user.clone());
                st.persist_user(&id).await;
                to_value(&user)
            }),
        },
        Tool {
            name: "update_user",
            description: "Update a user's profile/flags (never the password) with an RFC 6902 \
                          JSON Patch. Admin-only. Args: {id, patch} where patch is an ARRAY of \
                          ops applied to {name, email, is_admin, disabled, quota_mb}, e.g. \
                          [{\"op\":\"replace\",\"path\":\"/name\",\"value\":\"Alice\"}, \
                          {\"op\":\"remove\",\"path\":\"/quota_mb\"}]. 'remove /quota_mb' clears \
                          the quota; unknown paths are ignored; invalid patch/type is rejected.",
            input_schema: || obj_schema(&[
                ("id", s_string()),
                ("patch", json!({
                    "type": "array",
                    "description": "RFC 6902 JSON Patch ops array",
                    "items": { "type": "object" }
                })),
            ], &["id", "patch"]),
            handler: |st, actor, a| boxed(async move {
                require_admin(&actor)?;
                let id = arg_str(&a, "id")?;
                let ops_val = a.get("patch").cloned().ok_or("missing required array argument 'patch'")?;
                let ops: json_patch::Patch = serde_json::from_value(ops_val)
                    .map_err(|e| format!("invalid JSON Patch: {e}"))?;
                let mut st = st.write().await;
                let user = st.users.get_mut(&id).ok_or("user not found")?;
                let mut doc = json!({
                    "name": user.name, "email": user.email, "is_admin": user.is_admin,
                    "disabled": user.disabled, "quota_mb": user.quota_mb,
                });
                json_patch::patch(&mut doc, &ops).map_err(|e| format!("patch failed: {e}"))?;
                if let Some(v) = doc.get("name") { user.name = v.as_str().ok_or("name must be string")?.to_string(); }
                if let Some(v) = doc.get("email") { user.email = v.as_str().ok_or("email must be string")?.to_string(); }
                if let Some(v) = doc.get("is_admin") { user.is_admin = v.as_bool().ok_or("is_admin must be bool")?; }
                if let Some(v) = doc.get("disabled") { user.disabled = v.as_bool().ok_or("disabled must be bool")?; }
                // quota_mb is Option: null or a removed (absent) key clears it.
                match doc.get("quota_mb") {
                    None => user.quota_mb = None,
                    Some(v) if v.is_null() => user.quota_mb = None,
                    Some(v) => user.quota_mb = Some(v.as_u64().ok_or("quota_mb must be integer")?),
                }
                let out = user.clone();
                st.persist_user(&id).await;
                to_value(&out)
            }),
        },
        Tool {
            name: "delete_user",
            description: "Delete a user and clean up references (group memberships, \
                          vault, prefs, album shares). Admin-only. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, actor, a| boxed(async move {
                require_admin(&actor)?;
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                if st.users.remove(&id).is_none() { return Err("user not found".into()); }
                st.vaults.remove(&id);
                st.prefs.remove(&id);
                let mut groups = Vec::new();
                for g in st.groups.values_mut() {
                    let before = g.member_ids.len();
                    g.member_ids.retain(|m| m != &id);
                    if g.member_ids.len() != before { groups.push(g.id.clone()); }
                }
                let mut albums = Vec::new();
                for al in st.albums.values_mut() {
                    let before = al.shares.len();
                    al.shares.retain(|s| s.target != crate::models::ShareTarget::User(id.clone()));
                    if al.shares.len() != before { albums.push(al.id.clone()); }
                }
                st.delete_user_row(&id).await;
                st.delete_vault_row(&id).await;
                st.delete_prefs_row(&id).await;
                for g in &groups { st.persist_group(g).await; }
                for al in &albums { st.persist_album(al).await; }
                Ok(json!({ "ok": true, "deleted": id }))
            }),
        },
        Tool {
            name: "set_user_password",
            description: "Set a user's OWN password using either their current password \
                          or a valid unused reset token. Args: {id, new_password, \
                          current_password?, reset_token?}. Only the user themself can set \
                          their password; admins never can.",
            input_schema: || obj_schema(&[
                ("id", s_string()), ("new_password", s_string()),
                ("current_password", s_string()), ("reset_token", s_string()),
            ], &["id", "new_password"]),
            handler: |st, actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                // Only the user themself (not even an admin) may set a password.
                if actor.user_id != id {
                    return Err("forbidden: only the user may set their own password".into());
                }
                let new_password = arg_str(&a, "new_password")?;
                let mut st = st.write().await;
                if !st.users.contains_key(&id) { return Err("user not found".into()); }
                let has_pw = st.users.get(&id).map(|u| u.password_hash.is_some()).unwrap_or(false);
                let mut authorized = false;
                let mut consume: Option<String> = None;
                if let Some(tok) = arg_str_opt(&a, "reset_token") {
                    if let Some(rt) = st.reset_tokens.get(&tok) {
                        if !rt.used && rt.user_id == id
                            && !crate::handlers::is_expired(&rt.created_at, crate::handlers::RESET_TTL_SECS)
                        { authorized = true; consume = Some(tok); }
                    }
                }
                let secret = st.password_secret().to_vec();
                if !authorized {
                    if let (Some(cur), true) = (arg_str_opt(&a, "current_password"), has_pw) {
                        authorized = st.users.get(&id).map(|u| u.verify_password(&secret, &cur)).unwrap_or(false);
                    }
                }
                if !authorized { return Err("forbidden: no valid credential".into()); }
                let pepper = st.new_pepper();
                if let Some(u) = st.users.get_mut(&id) { u.set_password(&secret, pepper, &new_password); }
                if let Some(tok) = consume {
                    if let Some(rt) = st.reset_tokens.get_mut(&tok) { rt.used = true; }
                    st.persist_reset_token(&tok).await;
                }
                st.persist_user(&id).await;
                Ok(json!({ "ok": true }))
            }),
        },
        Tool {
            name: "reset_user_password",
            description: "Admin action: mint a single-use password reset token for a user \
                          and email a reset link. Admin-only. Args: {id}. Never returns the \
                          password; returns the issued token for demo/automation.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, actor, a| boxed(async move {
                require_admin(&actor)?;
                let id = arg_str(&a, "id")?;
                let (email, token, mailer) = {
                    let mut st = st.write().await;
                    let email = st.users.get(&id).map(|u| u.email.clone()).ok_or("user not found")?;
                    let token = st.new_reset_token();
                    st.reset_tokens.insert(token.clone(), crate::models::ResetToken {
                        token: token.clone(), user_id: id.clone(),
                        created_at: crate::state::now_rfc3339(), used: false,
                    });
                    st.persist_reset_token(&token).await;
                    (email, token, st.mailer())
                };
                let _ = mailer.send(&email, "Reset your Photon password",
                    &format!("Use this token to set a new password: {token}")).await;
                Ok(json!({ "ok": true, "reset_token": token }))
            }),
        },
        // ---- Groups ----
        Tool {
            name: "list_groups",
            description: "List groups the actor owns or is a member of (id, name, owner, members). Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                let st = st.read().await;
                let mut g: Vec<_> = st.groups.values()
                    .filter(|g| g.owner_id == actor.user_id || g.member_ids.iter().any(|m| *m == actor.user_id))
                    .cloned().collect();
                g.sort_by(|a, b| a.id.cmp(&b.id));
                to_value(&g)
            }),
        },
        Tool {
            name: "create_group",
            description: "Create a group. Args: {name, owner_id, member_ids?}. The owner \
                          must be an existing user.",
            input_schema: || obj_schema(&[
                ("name", s_string()), ("owner_id", s_string()), ("member_ids", s_str_array()),
            ], &["name", "owner_id"]),
            handler: |st, _actor, a| boxed(async move {
                let name = arg_str(&a, "name")?;
                let owner_id = arg_str(&a, "owner_id")?;
                let member_ids = arg_str_vec(&a, "member_ids").unwrap_or_default();
                let mut st = st.write().await;
                if !st.users.contains_key(&owner_id) { return Err("owner not found".into()); }
                let id = st.next_id("grp");
                let group = crate::models::Group { id: id.clone(), name, owner_id, member_ids };
                st.groups.insert(id.clone(), group.clone());
                st.persist_group(&id).await;
                to_value(&group)
            }),
        },
        Tool {
            name: "get_group",
            description: "Get a group by id. Args: {id}. The actor must own or be a member \
                          of the group (admins may fetch any).",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let st = st.read().await;
                let g = st.groups.get(&id).ok_or("group not found")?;
                if !(actor.is_admin || g.owner_id == actor.user_id || g.member_ids.contains(&actor.user_id)) {
                    return Err("forbidden".into());
                }
                to_value(g)
            }),
        },
        Tool {
            name: "delete_group",
            description: "Delete a group and drop album shares targeting it. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                if st.groups.remove(&id).is_none() { return Err("group not found".into()); }
                let mut albums = Vec::new();
                for al in st.albums.values_mut() {
                    let before = al.shares.len();
                    al.shares.retain(|s| s.target != crate::models::ShareTarget::Group(id.clone()));
                    if al.shares.len() != before { albums.push(al.id.clone()); }
                }
                st.delete_group_row(&id).await;
                for al in &albums { st.persist_album(al).await; }
                Ok(json!({ "ok": true, "deleted": id }))
            }),
        },
        Tool {
            name: "add_group_member",
            description: "Add a user to a group. Args: {id (group), user_id}.",
            input_schema: || obj_schema(&[("id", s_string()), ("user_id", s_string())], &["id", "user_id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let user_id = arg_str(&a, "user_id")?;
                let mut st = st.write().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let g = st.groups.get_mut(&id).ok_or("group not found")?;
                if !g.member_ids.contains(&user_id) { g.member_ids.push(user_id); }
                let out = g.clone();
                st.persist_group(&id).await;
                to_value(&out)
            }),
        },
        Tool {
            name: "remove_group_member",
            description: "Remove a user from a group. Args: {id (group), user_id}.",
            input_schema: || obj_schema(&[("id", s_string()), ("user_id", s_string())], &["id", "user_id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let user_id = arg_str(&a, "user_id")?;
                let mut st = st.write().await;
                let g = st.groups.get_mut(&id).ok_or("group not found")?;
                g.member_ids.retain(|m| m != &user_id);
                let out = g.clone();
                st.persist_group(&id).await;
                to_value(&out)
            }),
        },
        // ---- Photos ----
        Tool {
            name: "list_photos",
            description: "List photos the actor may read (own — including non-live — plus \
                          photos granted via share/partner, which appear only while LIVE), \
                          newest first, as resolved views. For a user-scoped listing use \
                          get_timeline or search.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                let st = st.read().await;
                let mut p: Vec<_> = st.photos.values()
                    .filter(|p| st.allowed(&actor.user_id, &p.id)).collect();
                p.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
                to_value(&p.into_iter().map(|p| p.effective()).collect::<Vec<_>>())
            }),
        },
        Tool {
            name: "get_photo",
            description: "Get one photo's resolved view by id. Args: {id}. The owner may \
                          read their own photo in any state; a granted non-owner may read it \
                          only while LIVE (not trashed/archived/vaulted); admins may read any.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let st = st.read().await;
                let p = st.photos.get(&id).ok_or("photo not found")?;
                if !actor.is_admin {
                    if p.owner_id == actor.user_id {
                        // owner may read in any state
                    } else if st.allowed(&actor.user_id, &id)
                        && p.deleted_at.is_none()
                        && !p.archived
                        && !st.is_in_any_vault(&id)
                    {
                        // granted non-owner may read only while LIVE
                    } else {
                        return Err("forbidden".into());
                    }
                }
                to_value(&p.effective())
            }),
        },
        Tool {
            name: "patch_photo_metadata",
            description: "Patch a photo's metadata overrides with an RFC 6902 JSON Patch. \
                          Args: {id, patch} where patch is an ARRAY of ops, e.g. \
                          [{\"op\":\"replace\",\"path\":\"/title\",\"value\":\"Sunset\"}, \
                          {\"op\":\"remove\",\"path\":\"/city\"}]. The patch is applied to the \
                          override object (fields: taken_at, city, country, title, caption, \
                          rating 0-5, favorite, tags[], people[], lat, lng). 'replace'/'add' set \
                          an override; 'remove' clears it back to EXIF. Invalid patch/result is rejected.",
            input_schema: || obj_schema(&[
                ("id", s_string()),
                ("patch", json!({
                    "type": "array",
                    "description": "RFC 6902 JSON Patch ops array",
                    "items": { "type": "object" }
                })),
            ], &["id", "patch"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let ops_val = a.get("patch").cloned().ok_or("missing required array argument 'patch'")?;
                let ops: json_patch::Patch = serde_json::from_value(ops_val)
                    .map_err(|e| format!("invalid JSON Patch: {e}"))?;
                let mut st = st.write().await;
                let photo = st.photos.get_mut(&id).ok_or("photo not found")?;
                let mut doc = serde_json::to_value(&photo.overrides)
                    .map_err(|e| format!("serialize overrides failed: {e}"))?;
                json_patch::patch(&mut doc, &ops).map_err(|e| format!("patch failed: {e}"))?;
                let next: crate::models::MetadataOverride = serde_json::from_value(doc)
                    .map_err(|e| format!("patched overrides invalid: {e}"))?;
                if next.rating.is_some_and(|r| r > 5) {
                    return Err("rating must be 0-5".into());
                }
                photo.overrides = next;
                let view = photo.effective();
                st.persist_photo(&id).await;
                to_value(&view)
            }),
        },
        Tool {
            name: "trash_photo",
            description: "Soft-delete (move to trash) a photo. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                let p = st.photos.get_mut(&id).ok_or("photo not found")?;
                p.deleted_at = Some(crate::state::now_rfc3339());
                let view = p.effective();
                st.persist_photo(&id).await;
                to_value(&view)
            }),
        },
        Tool {
            name: "restore_photo",
            description: "Restore a photo from trash (clear deleted_at). Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                let p = st.photos.get_mut(&id).ok_or("photo not found")?;
                p.deleted_at = None;
                let view = p.effective();
                st.persist_photo(&id).await;
                to_value(&view)
            }),
        },
        Tool {
            name: "archive_photo",
            description: "Archive a photo (hidden from timeline/search, kept). Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                let p = st.photos.get_mut(&id).ok_or("photo not found")?;
                p.archived = true;
                let view = p.effective();
                st.persist_photo(&id).await;
                to_value(&view)
            }),
        },
        Tool {
            name: "unarchive_photo",
            description: "Unarchive a photo. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                let p = st.photos.get_mut(&id).ok_or("photo not found")?;
                p.archived = false;
                let view = p.effective();
                st.persist_photo(&id).await;
                to_value(&view)
            }),
        },
        Tool {
            name: "permanent_delete_photo",
            description: "Permanently delete a photo now (also removes it from albums). \
                          Irreversible. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                if st.photos.remove(&id).is_none() { return Err("photo not found".into()); }
                let mut albums = Vec::new();
                for al in st.albums.values_mut() {
                    let before = al.photo_ids.len();
                    al.photo_ids.retain(|pid| pid != &id);
                    if al.photo_ids.len() != before { albums.push(al.id.clone()); }
                }
                st.delete_photo_row(&id).await;
                for al in &albums { st.persist_album(al).await; }
                Ok(json!({ "ok": true, "deleted": id }))
            }),
        },
        Tool {
            name: "list_trash",
            description: "List the actor's OWN photos currently in trash (newest first). Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                let st = st.read().await;
                let mut p: Vec<_> = st.photos.values()
                    .filter(|p| p.deleted_at.is_some() && p.owner_id == actor.user_id).collect();
                p.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
                to_value(&p.into_iter().map(|p| p.effective()).collect::<Vec<_>>())
            }),
        },
        Tool {
            name: "list_archive",
            description: "List the actor's OWN archived (non-trashed) photos (newest first). Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                let st = st.read().await;
                let mut p: Vec<_> = st.photos.values()
                    .filter(|p| p.archived && p.deleted_at.is_none() && p.owner_id == actor.user_id).collect();
                p.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
                to_value(&p.into_iter().map(|p| p.effective()).collect::<Vec<_>>())
            }),
        },
        Tool {
            name: "analyze_photo",
            description: "Re-run the AI-analysis import stage (OCR / people / context tags) \
                          for a photo and return the updated view. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                if !st.analyze_photo(&id) { return Err("photo not found".into()); }
                let view = st.photos.get(&id).map(|p| p.effective()).ok_or("photo not found")?;
                st.persist_photo(&id).await;
                to_value(&view)
            }),
        },
        Tool {
            name: "list_people",
            description: "List a user's People (face-recognition clusters): each has \
                          person_id, name?, face_count, cover crop, sample_photo_ids, and \
                          kinship relationships[]. Never returns face embeddings. \
                          Args: {user_id}.",
            input_schema: || obj_schema(&[("user_id", s_string())], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let st = st.read().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                to_value(&st.people_views(&user_id))
            }),
        },
        Tool {
            name: "name_person",
            description: "Name (or rename) a face cluster; empty name clears it. Propagates \
                          the name into the photos' searchable people. Args: {person_id, name}.",
            input_schema: || obj_schema(&[("person_id", s_string()), ("name", s_string())], &["person_id", "name"]),
            handler: |st, actor, a| boxed(async move {
                let person_id = arg_str(&a, "person_id")?;
                let name = arg_str(&a, "name")?;
                let mut st = st.write().await;
                let owner = st.people.get(&person_id).map(|p| p.owner_id.clone())
                    .ok_or("person not found")?;
                require_self_or_admin(&actor, &owner)?;
                let owner = st.name_person(&person_id, &name).ok_or("person not found")?;
                st.persist_faces(&owner).await;
                let photo_ids: Vec<String> = st.photos.values()
                    .filter(|p| p.owner_id == owner).map(|p| p.id.clone()).collect();
                for pid in photo_ids { st.persist_photo(&pid).await; }
                Ok(json!({ "ok": true }))
            }),
        },
        Tool {
            name: "list_person_photos",
            description: "List the live photos a Person appears in (newest first; \
                          trash/archive/vault excluded). Args: {person_id}.",
            input_schema: || obj_schema(&[("person_id", s_string())], &["person_id"]),
            handler: |st, actor, a| boxed(async move {
                let person_id = arg_str(&a, "person_id")?;
                let st = st.read().await;
                let owner = st.people.get(&person_id).map(|p| p.owner_id.clone())
                    .ok_or("person not found")?;
                require_self_or_admin(&actor, &owner)?;
                let photos = st.person_photos(&owner, &person_id).ok_or("person not found")?;
                to_value(&photos)
            }),
        },
        Tool {
            name: "add_person_relationship",
            description: "Create a reciprocal KINSHIP link between two People of the same \
                          owner: other_person_id becomes person_id's `relation` (e.g. \
                          \"mother\", \"brother\", \"son\"); the inverse edge is added \
                          automatically. Args: {person_id, other_person_id, relation}.",
            input_schema: || obj_schema(&[
                ("person_id", s_string()), ("other_person_id", s_string()), ("relation", s_string()),
            ], &["person_id", "other_person_id", "relation"]),
            handler: |st, actor, a| boxed(async move {
                let person_id = arg_str(&a, "person_id")?;
                let other = arg_str(&a, "other_person_id")?;
                let relation = arg_str(&a, "relation")?;
                let mut st = st.write().await;
                if !st.people.contains_key(&person_id) || !st.people.contains_key(&other) {
                    return Err("person not found".into());
                }
                let owner = st.people.get(&person_id).map(|p| p.owner_id.clone())
                    .ok_or("person not found")?;
                require_self_or_admin(&actor, &owner)?;
                let owner = st.link_people(&person_id, &other, &relation)
                    .ok_or("invalid relationship (self-link, empty relation, or cross-owner)")?;
                st.persist_faces(&owner).await;
                Ok(json!({ "ok": true }))
            }),
        },
        Tool {
            name: "remove_person_relationship",
            description: "Remove the reciprocal kinship link between two People. \
                          Args: {person_id, other_person_id}.",
            input_schema: || obj_schema(&[
                ("person_id", s_string()), ("other_person_id", s_string()),
            ], &["person_id", "other_person_id"]),
            handler: |st, actor, a| boxed(async move {
                let person_id = arg_str(&a, "person_id")?;
                let other = arg_str(&a, "other_person_id")?;
                let mut st = st.write().await;
                let owner = st.people.get(&person_id).map(|p| p.owner_id.clone())
                    .ok_or("person not found")?;
                require_self_or_admin(&actor, &owner)?;
                let owner = st.unlink_people(&person_id, &other).ok_or("person not found")?;
                st.persist_faces(&owner).await;
                Ok(json!({ "ok": true }))
            }),
        },
        Tool {
            name: "render_photo",
            description: "Negotiate the best FORMAT + RESOLUTION render plan for a device. \
                          Args: {id, w?, h?, fmt? (jpeg|png|webp|avif|mp4...), supports? \
                          (comma-separated formats)}. Returns the plan + mime + cache_key.",
            input_schema: || obj_schema(&[
                ("id", s_string()), ("w", s_int()), ("h", s_int()),
                ("fmt", s_string()), ("supports", s_string()),
            ], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let st = st.read().await;
                let photo = st.photos.get(&id).ok_or("photo not found")?;
                let ext = photo.filename.rsplit('.').next().unwrap_or("");
                let source = crate::transcode::MediaFormat::from_ext(ext)
                    .unwrap_or(crate::transcode::MediaFormat::Jpeg);
                let (sw, sh) = (photo.exif.width.max(1), photo.exif.height.max(1));
                let fmt = arg_str_opt(&a, "fmt")
                    .and_then(|s| serde_json::from_value(json!(s)).ok());
                let device = crate::transcode::DeviceProfile::from_request(
                    None,
                    arg_str_opt(&a, "supports").as_deref(),
                    fmt,
                    a.get("w").and_then(|v| v.as_u64()).map(|n| n as u32),
                    a.get("h").and_then(|v| v.as_u64()).map(|n| n as u32),
                );
                let plan = crate::transcode::negotiate(source, sw, sh, &device);
                let cache_key = format!("{}_{}x{}.{}", id, plan.width, plan.height, plan.format.ext());
                let mime = plan.format.mime().to_string();
                Ok(json!({
                    "plan": to_value(&plan)?, "mime": mime, "cache_key": cache_key
                }))
            }),
        },
        // ---- Uploads ----
        Tool {
            name: "upload_raw",
            description: "Ingest raw image bytes (base64). EXIF/dimensions are extracted \
                          server-side (never trusting client metadata). Args: {owner_id, \
                          album_id?, files:[{filename, bytes(base64)}]}. Returns created \
                          photo views. Generates thumbnails + runs AI analysis.",
            input_schema: || obj_schema(&[
                ("owner_id", s_string()),
                ("album_id", s_string()),
                ("files", json!({
                    "type": "array",
                    "items": obj_schema(&[("filename", s_string()), ("bytes", s_string_desc("base64 file contents"))], &["filename", "bytes"])
                })),
            ], &["owner_id", "files"]),
            handler: |st, actor, a| boxed(async move {
                let owner_id = arg_str(&a, "owner_id")?;
                // A non-admin may only upload as themselves.
                require_self_or_admin(&actor, &owner_id)?;
                let album_id = arg_str_opt(&a, "album_id");
                let files_v = a.get("files").and_then(|v| v.as_array()).ok_or("missing files[]")?;
                use base64::Engine as _;
                let mut files: Vec<(String, String, Vec<u8>)> = Vec::new();
                for f in files_v {
                    let filename = f.get("filename").and_then(|v| v.as_str()).ok_or("file.filename required")?.to_string();
                    let b64 = f.get("bytes").and_then(|v| v.as_str()).ok_or("file.bytes required")?;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())
                        .map_err(|_| "file.bytes not valid base64")?;
                    let ext = filename.rsplit('.').next().filter(|e| *e != filename).unwrap_or("").to_lowercase();
                    files.push((filename, ext, bytes));
                }
                let mut st = st.write().await;
                if !st.users.contains_key(&owner_id) { return Err("owner not found".into()); }
                if let Some(al) = &album_id { if !st.albums.contains_key(al) { return Err("album not found".into()); } }
                let ids = st.ingest_upload_bytes(&owner_id, files, &crate::extract::ExifExtractor);
                st.store_thumbnails(&ids).await;
                // CLIP embeddings + OCR (no-op offline; uses thumbnail bytes here).
                st.embed_photos(&ids).await;
                st.ocr_photos(None, &ids).await;
                if let Some(al) = &album_id {
                    if let Some(album) = st.albums.get_mut(al) {
                        for pid in &ids { if !album.photo_ids.contains(pid) { album.photo_ids.push(pid.clone()); } }
                    }
                }
                let views: Vec<_> = ids.iter().filter_map(|id| st.photos.get(id).map(|p| p.effective())).collect();
                for id in &ids { st.persist_photo(id).await; }
                if let Some(al) = &album_id { st.persist_album(al).await; }
                to_value(&views)
            }),
        },
        // ---- Albums ----
        Tool {
            name: "list_albums",
            description: "List albums the actor owns or that are shared to them (id, name, \
                          owner, photo_ids, shares). Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                let st = st.read().await;
                let mut al: Vec<_> = st.albums.values()
                    .filter(|a| a.owner_id == actor.user_id || st.album_shared_to(a, &actor.user_id))
                    .cloned().collect();
                al.sort_by(|a, b| a.id.cmp(&b.id));
                to_value(&al)
            }),
        },
        Tool {
            name: "create_album",
            description: "Create an album. Args: {name, owner_id, photo_ids?}. Owner must exist.",
            input_schema: || obj_schema(&[
                ("name", s_string()), ("owner_id", s_string()), ("photo_ids", s_str_array()),
            ], &["name", "owner_id"]),
            handler: |st, _actor, a| boxed(async move {
                let name = arg_str(&a, "name")?;
                let owner_id = arg_str(&a, "owner_id")?;
                let photo_ids = arg_str_vec(&a, "photo_ids").unwrap_or_default();
                let mut st = st.write().await;
                if !st.users.contains_key(&owner_id) { return Err("owner not found".into()); }
                let id = st.next_id("alb");
                let cover_seed = photo_ids.first().and_then(|pid| st.photos.get(pid)).map(|p| p.seed).unwrap_or(0);
                let album = crate::models::Album { id: id.clone(), name, owner_id, cover_seed, photo_ids, shares: Vec::new() };
                st.albums.insert(id.clone(), album.clone());
                st.persist_album(&id).await;
                to_value(&album)
            }),
        },
        Tool {
            name: "get_album",
            description: "Get an album by id. Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let st = st.read().await;
                to_value(st.albums.get(&id).ok_or("album not found")?)
            }),
        },
        Tool {
            name: "delete_album",
            description: "Delete an album (photos themselves are NOT deleted). Args: {id}.",
            input_schema: || obj_schema(&[("id", s_string())], &["id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let mut st = st.write().await;
                if st.albums.remove(&id).is_none() { return Err("album not found".into()); }
                st.delete_album_row(&id).await;
                Ok(json!({ "ok": true, "deleted": id }))
            }),
        },
        Tool {
            name: "add_album_photos",
            description: "Add existing photos to an album the actor OWNS (admins: any album). \
                          Args: {id (album), photo_ids[]}. Every referenced photo must exist \
                          and, unless admin, be owned by the actor.",
            input_schema: || obj_schema(&[("id", s_string()), ("photo_ids", s_str_array())], &["id", "photo_ids"]),
            handler: |st, actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let photo_ids = arg_str_vec(&a, "photo_ids")?;
                let mut st = st.write().await;
                // Album-ownership gate (mirrors REST resource_authz: album mutations are owner-only).
                let album_owner = st.albums.get(&id).map(|a| a.owner_id.clone()).ok_or("album not found")?;
                if !actor.is_admin && album_owner != actor.user_id {
                    return Err("forbidden: not the album owner".into());
                }
                for pid in &photo_ids {
                    match st.photos.get(pid) {
                        None => return Err("photo not found".into()),
                        Some(p) if !actor.is_admin && p.owner_id != actor.user_id =>
                            return Err("photo not owned by actor".into()),
                        Some(_) => {}
                    }
                }
                let al = st.albums.get_mut(&id).ok_or("album not found")?;
                for pid in photo_ids { if !al.photo_ids.contains(&pid) { al.photo_ids.push(pid); } }
                let out = al.clone();
                st.persist_album(&id).await;
                to_value(&out)
            }),
        },
        Tool {
            name: "share_album",
            description: "Share an album with a user or group at a role. Args: {id (album), \
                          target_type ('user'|'group'), target_id, role ('viewer'|'contributor')}. \
                          Updates the role if the target is already shared. Emails recipients.",
            input_schema: || obj_schema(&[
                ("id", s_string()),
                ("target_type", json!({ "type": "string", "enum": ["user", "group"] })),
                ("target_id", s_string()),
                ("role", json!({ "type": "string", "enum": ["viewer", "contributor"] })),
            ], &["id", "target_type", "target_id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let target = parse_share_target(&a)?;
                let role = parse_share_role(&a);
                let (album, recipients, mailer, subject, message) = {
                    let mut st = st.write().await;
                    let al = st.albums.get_mut(&id).ok_or("album not found")?;
                    if let Some(existing) = al.shares.iter_mut().find(|s| s.target == target) {
                        existing.role = role;
                    } else {
                        al.shares.push(crate::models::Share { target: target.clone(), role });
                    }
                    let album = al.clone();
                    st.persist_album(&id).await;
                    let recipients = st.target_emails(&target);
                    (album.clone(), recipients, st.mailer(),
                     "An album was shared with you".to_string(),
                     format!("The album \"{}\" was shared with you on Photon.", album.name))
                };
                for to in recipients { let _ = mailer.send(&to, &subject, &message).await; }
                to_value(&album)
            }),
        },
        Tool {
            name: "unshare_album",
            description: "Remove an album share by target. Args: {id (album), target_type \
                          ('user'|'group'), target_id}.",
            input_schema: || obj_schema(&[
                ("id", s_string()),
                ("target_type", json!({ "type": "string", "enum": ["user", "group"] })),
                ("target_id", s_string()),
            ], &["id", "target_type", "target_id"]),
            handler: |st, _actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let target = parse_share_target(&a)?;
                let mut st = st.write().await;
                let al = st.albums.get_mut(&id).ok_or("album not found")?;
                al.shares.retain(|s| s.target != target);
                let out = al.clone();
                st.persist_album(&id).await;
                to_value(&out)
            }),
        },
        Tool {
            name: "contribute_to_album",
            description: "A Contributor (or owner) adds their OWN photos to an album. \
                          Args: {id (album), user_id, photo_ids[]}. Enforces that user_id \
                          may contribute and owns every photo. Ownership is not reassigned.",
            input_schema: || obj_schema(&[
                ("id", s_string()), ("user_id", s_string()), ("photo_ids", s_str_array()),
            ], &["id", "user_id", "photo_ids"]),
            handler: |st, actor, a| boxed(async move {
                let id = arg_str(&a, "id")?;
                let user_id = arg_str(&a, "user_id")?;
                // Only the contributing user (or an admin) may contribute on their behalf.
                require_self_or_admin(&actor, &user_id)?;
                let photo_ids = arg_str_vec(&a, "photo_ids")?;
                let mut st = st.write().await;
                if !st.albums.contains_key(&id) { return Err("album not found".into()); }
                if !st.can_contribute(&user_id, &id) { return Err("forbidden: not a contributor".into()); }
                for pid in &photo_ids {
                    match st.photos.get(pid) {
                        None => return Err("photo not found".into()),
                        Some(p) if p.owner_id != user_id => return Err("photo not owned by contributor".into()),
                        Some(_) => {}
                    }
                }
                let al = st.albums.get_mut(&id).ok_or("album not found")?;
                for pid in photo_ids { if !al.photo_ids.contains(&pid) { al.photo_ids.push(pid); } }
                let out = al.clone();
                st.persist_album(&id).await;
                to_value(&out)
            }),
        },
        // ---- Timeline prefs + timeline ----
        Tool {
            name: "get_timeline_prefs",
            description: "Get a user's timeline preferences (show_shared, per_album). \
                          Args: {user_id}. Actor must be that user or an admin.",
            input_schema: || obj_schema(&[("user_id", s_string())], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let st = st.read().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                to_value(&st.prefs.get(&user_id).cloned().unwrap_or_default())
            }),
        },
        Tool {
            name: "update_timeline_prefs",
            description: "Update a user's timeline preferences. Args: {user_id, show_shared?, \
                          per_album? (map album_id->bool)}. Actor must be that user or an admin.",
            input_schema: || obj_schema(&[
                ("user_id", s_string()), ("show_shared", s_bool()),
                ("per_album", json!({ "type": "object", "additionalProperties": { "type": "boolean" } })),
            ], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let mut st = st.write().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let prefs = st.prefs.entry(user_id.clone()).or_default();
                if let Some(v) = arg_bool_opt(&a, "show_shared") { prefs.show_shared = v; }
                if let Some(m) = a.get("per_album").and_then(|v| v.as_object()) {
                    let mut map = std::collections::HashMap::new();
                    for (k, v) in m { if let Some(b) = v.as_bool() { map.insert(k.clone(), b); } }
                    prefs.per_album = map;
                }
                let out = prefs.clone();
                st.persist_prefs(&user_id).await;
                to_value(&out)
            }),
        },
        Tool {
            name: "get_timeline",
            description: "Get a user's timeline grouped into date sections (own photos + \
                          shared albums per prefs; excludes trashed/archived/vaulted). \
                          Args: {user_id}. Actor must be that user or an admin (scoped).",
            input_schema: || obj_schema(&[("user_id", s_string())], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let st = st.read().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let photos = st.timeline_photos(&user_id);
                let mut sections: Vec<crate::models::TimelineSection> = Vec::new();
                for p in photos {
                    let view = p.effective();
                    let date = view.taken_at.get(0..10).unwrap_or("").to_string();
                    match sections.last_mut() {
                        Some(sec) if sec.date == date => sec.items.push(view),
                        _ => sections.push(crate::models::TimelineSection { label: date.clone(), date, items: vec![view] }),
                    }
                }
                to_value(&crate::models::Timeline { sections })
            }),
        },
        // ---- Search ----
        Tool {
            name: "search",
            description: "Search a user's accessible photos (own + photos of any album they \
                          can access; excludes trashed/archived/vaulted). Args: {user_id, q?, \
                          camera?, from? (YYYY-MM-DD), to? (YYYY-MM-DD), place?, near? \
                          ('lat,lng,radiusKm')}. Actor must be that user or an admin (scoped).",
            input_schema: || obj_schema(&[
                ("user_id", s_string()),
                ("q", s_string_desc("free-text query")),
                ("camera", s_string()), ("from", s_string()), ("to", s_string()),
                ("place", s_string()), ("near", s_string_desc("lat,lng,radiusKm")),
            ], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let st = st.read().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let near = arg_str_opt(&a, "near").and_then(|s| {
                    let mut it = s.split(',').map(|x| x.trim().parse::<f64>());
                    match (it.next(), it.next(), it.next()) {
                        (Some(Ok(la)), Some(Ok(lo)), Some(Ok(r))) => Some((la, lo, r)),
                        _ => None,
                    }
                });
                let filters = crate::state::SearchFilters {
                    q: arg_str_opt(&a, "q").unwrap_or_default(),
                    camera: arg_str_opt(&a, "camera").filter(|s| !s.is_empty()),
                    from: arg_str_opt(&a, "from").filter(|s| !s.is_empty()),
                    to: arg_str_opt(&a, "to").filter(|s| !s.is_empty()),
                    place: arg_str_opt(&a, "place").filter(|s| !s.is_empty()),
                    near,
                };
                to_value(&st.search_filtered(&user_id, &filters))
            }),
        },
        // ---- Per-user storage ----
        Tool {
            name: "get_user_storage",
            description: "Get a user's storage usage {used_mb, total_mb}. Args: {user_id}. \
                          Actor must be that user or an admin.",
            input_schema: || obj_schema(&[("user_id", s_string())], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let st = st.read().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let (used_mb, total_mb) = st.user_storage(&user_id);
                Ok(json!({ "used_mb": used_mb, "total_mb": total_mb }))
            }),
        },
        // ---- Vault (PIN-gated, owner-only) ----
        Tool {
            name: "get_vault_status",
            description: "Get a user's vault status {configured, count}. Never returns photos \
                          or the PIN. Args: {user_id}. Actor must be the vault owner (or admin).",
            input_schema: || obj_schema(&[("user_id", s_string())], &["user_id"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let st = st.read().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let (configured, count) = match st.vaults.get(&user_id) {
                    Some(v) => (v.pin_hash.is_some(), v.photo_ids.len()),
                    None => (false, 0),
                };
                Ok(json!({ "configured": configured, "count": count }))
            }),
        },
        Tool {
            name: "set_vault_pin",
            description: "Set or change the user's vault PIN. If already set, current_pin is \
                          required. Args: {user_id, pin, current_pin?}. Owner-only (or admin).",
            input_schema: || obj_schema(&[
                ("user_id", s_string()), ("pin", s_string()), ("current_pin", s_string()),
            ], &["user_id", "pin"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let pin = arg_str(&a, "pin")?;
                let mut st = st.write().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let already = st.vaults.get(&user_id).map(|v| v.pin_hash.is_some()).unwrap_or(false);
                if already {
                    let ok = match arg_str_opt(&a, "current_pin") { Some(cur) => st.verify_pin(&user_id, &cur), None => false };
                    if !ok { return Err("forbidden: wrong current_pin".into()); }
                }
                st.set_pin(&user_id, &pin);
                st.persist_vault(&user_id).await;
                Ok(json!({ "ok": true }))
            }),
        },
        Tool {
            name: "unlock_vault",
            description: "Verify the vault PIN and return its contents (photo views). \
                          Args: {user_id, pin}. Owner-only (or admin). Wrong PIN is rejected.",
            input_schema: || obj_schema(&[("user_id", s_string()), ("pin", s_string())], &["user_id", "pin"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let pin = arg_str(&a, "pin")?;
                let mut st = st.write().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                let key = format!("vault:{user_id}");
                if st.rate_locked(&key) { return Err("too many attempts; locked out".into()); }
                if !st.verify_pin(&user_id, &pin) { st.rate_fail(&key); return Err("wrong or unset PIN".into()); }
                st.rate_reset(&key);
                Ok(json!({ "photos": to_value(&st.vault_views(&user_id))? }))
            }),
        },
        Tool {
            name: "add_vault_photos",
            description: "Move the user's OWN photos into their vault (PIN required). \
                          Args: {user_id, pin, photo_ids[]}. Owner-only (or admin); every \
                          photo must be owned by the user. Returns the new count.",
            input_schema: || obj_schema(&[
                ("user_id", s_string()), ("pin", s_string()), ("photo_ids", s_str_array()),
            ], &["user_id", "pin", "photo_ids"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let pin = arg_str(&a, "pin")?;
                let photo_ids = arg_str_vec(&a, "photo_ids")?;
                let mut st = st.write().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                if !st.verify_pin(&user_id, &pin) { return Err("wrong or unset PIN".into()); }
                for pid in &photo_ids {
                    match st.photos.get(pid) {
                        None => return Err("photo not found".into()),
                        Some(p) if p.owner_id != user_id => return Err("photo not owned by user".into()),
                        Some(_) => {}
                    }
                }
                let vault = st.vaults.entry(user_id.clone()).or_default();
                for pid in photo_ids { if !vault.photo_ids.contains(&pid) { vault.photo_ids.push(pid); } }
                let count = vault.photo_ids.len();
                st.persist_vault(&user_id).await;
                Ok(json!({ "count": count }))
            }),
        },
        Tool {
            name: "remove_vault_photos",
            description: "Remove photos from the user's vault (PIN required). \
                          Args: {user_id, pin, photo_ids[]}. Owner-only (or admin). Returns count.",
            input_schema: || obj_schema(&[
                ("user_id", s_string()), ("pin", s_string()), ("photo_ids", s_str_array()),
            ], &["user_id", "pin", "photo_ids"]),
            handler: |st, actor, a| boxed(async move {
                let user_id = arg_str(&a, "user_id")?;
                require_self_or_admin(&actor, &user_id)?;
                let pin = arg_str(&a, "pin")?;
                let photo_ids = arg_str_vec(&a, "photo_ids")?;
                let mut st = st.write().await;
                if !st.users.contains_key(&user_id) { return Err("user not found".into()); }
                if !st.verify_pin(&user_id, &pin) { return Err("wrong or unset PIN".into()); }
                let vault = st.vaults.entry(user_id.clone()).or_default();
                vault.photo_ids.retain(|pid| !photo_ids.contains(pid));
                let count = vault.photo_ids.len();
                st.persist_vault(&user_id).await;
                Ok(json!({ "count": count }))
            }),
        },
        // ---- Storage settings + backup ----
        Tool {
            name: "get_storage",
            description: "Get global storage settings (S3 secrets REDACTED). Admin-only. Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let st = st.read().await;
                to_value(&st.storage.redacted())
            }),
        },
        Tool {
            name: "update_storage",
            description: "Update global storage settings. Admin-only. Args: {mode? \
                          (filesystem|s3_replacement), primary_s3?, backup?, \
                          trash_retention_days?}. An S3 secret equal to the redaction \
                          sentinel keeps the stored secret. Returns redacted settings.",
            input_schema: || obj_schema(&[
                ("mode", json!({ "type": "string", "enum": ["filesystem", "s3_replacement"] })),
                ("primary_s3", json!({ "type": "object" })),
                ("backup", json!({ "type": "object" })),
                ("trash_retention_days", s_int()),
            ], &[]),
            handler: |st, actor, a| boxed(async move {
                require_admin(&actor)?;
                let mut st = st.write().await;
                if let Some(m) = a.get("mode") {
                    st.storage.mode = serde_json::from_value(m.clone()).map_err(|e| e.to_string())?;
                }
                if let Some(s3v) = a.get("primary_s3") {
                    let mut s3: crate::models::S3Config = serde_json::from_value(s3v.clone()).map_err(|e| e.to_string())?;
                    preserve_secret(&mut s3.secret_access_key, st.storage.primary_s3.as_ref());
                    st.storage.primary_s3 = Some(s3);
                }
                if let Some(bv) = a.get("backup") {
                    let mut backup: crate::models::BackupConfig = serde_json::from_value(bv.clone()).map_err(|e| e.to_string())?;
                    if let Some(s3) = backup.s3.as_mut() {
                        preserve_secret(&mut s3.secret_access_key, st.storage.backup.s3.as_ref());
                    }
                    st.storage.backup = backup;
                }
                if let Some(d) = a.get("trash_retention_days").and_then(|v| v.as_u64()) {
                    st.storage.trash_retention_days = d;
                }
                let redacted = st.storage.redacted();
                st.persist_storage().await;
                to_value(&redacted)
            }),
        },
        Tool {
            name: "run_backup",
            description: "Trigger a backup pass now (pushes new photos to S3 if enabled). \
                          Admin-only. Args: none. Returns {count, last_backup_at}.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let mut st = st.write().await;
                let count = st.run_backup().await.map_err(|e| format!("backup failed: {e}"))?;
                let last = st.storage.backup.last_backup_at.clone();
                if count > 0 && st.is_persistent() {
                    st.persist_storage().await;
                    let ids: Vec<String> = st.photos.values().filter(|p| p.backed_up).map(|p| p.id.clone()).collect();
                    for id in &ids { st.persist_photo(id).await; }
                }
                Ok(json!({ "count": count, "last_backup_at": last }))
            }),
        },
        // ---- Admin stats + audit ----
        Tool {
            name: "admin_stats",
            description: "Get job run state + entity counts + storage estimates. Admin-only. Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let st = st.read().await;
                let mut jobs: Vec<_> = st.jobs.values().cloned().collect();
                jobs.sort_by(|a, b| a.name.cmp(&b.name));
                let trashed = st.photos.values().filter(|p| p.deleted_at.is_some()).count();
                let archived = st.photos.values().filter(|p| p.archived && p.deleted_at.is_none()).count();
                let live = st.photos.values().filter(|p| p.deleted_at.is_none()).count();
                let vault: usize = st.vaults.values().map(|v| v.photo_ids.len()).sum();
                Ok(json!({
                    "jobs": to_value(&jobs)?,
                    "counts": {
                        "photos": live, "albums": st.albums.len(), "users": st.users.len(),
                        "groups": st.groups.len(), "trashed": trashed, "archived": archived, "vault": vault
                    },
                    "storage": { "mode": to_value(&st.storage.mode)? }
                }))
            }),
        },
        Tool {
            name: "audit_access",
            description: "Run the runtime authorization self-audit: proves no read surface \
                          exposes a photo to a user without a legitimate grant (and no \
                          vault/archived/trashed leak). Admin-only. Args: none. Returns \
                          {pass, violations}.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let st = st.read().await;
                let v = st.audit_access();
                Ok(json!({ "pass": v.is_empty(), "violations": to_value(&v)? }))
            }),
        },
        // ---- SMTP ----
        Tool {
            name: "get_smtp",
            description: "Get the SMTP config (password REDACTED). Admin-only. Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let st = st.read().await;
                to_value(&st.smtp.clone().map(|c| c.redacted()).unwrap_or_default())
            }),
        },
        Tool {
            name: "update_smtp",
            description: "Set the SMTP config. Admin-only. Args: {host, port, username?, \
                          password?, from, tls?}. A redacted/empty password keeps the stored one. \
                          Returns redacted config.",
            input_schema: || obj_schema(&[
                ("host", s_string()), ("port", s_int()), ("username", s_string()),
                ("password", s_string()), ("from", s_string()), ("tls", s_bool()),
            ], &["host", "port", "from"]),
            handler: |st, actor, a| boxed(async move {
                require_admin(&actor)?;
                let host = arg_str(&a, "host")?;
                let port = a.get("port").and_then(|v| v.as_u64()).ok_or("port required")? as u16;
                let from = arg_str(&a, "from")?;
                let username = arg_str_opt(&a, "username").unwrap_or_default();
                let tls = arg_bool_opt(&a, "tls").unwrap_or(false);
                let mut password = arg_str_opt(&a, "password").unwrap_or_default();
                let mut st = st.write().await;
                if password == crate::models::REDACTED_SECRET || password.is_empty() {
                    password = st.smtp.as_ref().map(|c| c.password.clone()).unwrap_or_default();
                }
                let cfg = crate::models::SmtpConfig { host, port, username, password, from, tls };
                let redacted = cfg.redacted();
                st.smtp = Some(cfg);
                st.persist_smtp().await;
                to_value(&redacted)
            }),
        },
        // ---- Invites ----
        Tool {
            name: "create_invite",
            description: "Create an invite (generates a token, emails it). Args: {email, \
                          inviter_id}. Inviter must exist. Returns the invite incl. token.",
            input_schema: || obj_schema(&[("email", s_string()), ("inviter_id", s_string())], &["email", "inviter_id"]),
            handler: |st, _actor, a| boxed(async move {
                let email = arg_str(&a, "email")?;
                let inviter_id = arg_str(&a, "inviter_id")?;
                let (invite, mailer) = {
                    let mut st = st.write().await;
                    if !st.users.contains_key(&inviter_id) { return Err("inviter not found".into()); }
                    let token = st.new_invite_token();
                    let invite = crate::models::Invite {
                        token: token.clone(), email, inviter_id,
                        created_at: crate::state::now_rfc3339(), accepted: false,
                    };
                    st.invites.insert(token.clone(), invite.clone());
                    st.persist_invite(&token).await;
                    (invite, st.mailer())
                };
                let _ = mailer.send(&invite.email, "You're invited to Photon",
                    &format!("Use this token to accept: {}", invite.token)).await;
                to_value(&invite)
            }),
        },
        Tool {
            name: "list_invites",
            description: "List all invites (tokens included), oldest first. Admin-only. Args: none.",
            input_schema: empty_schema,
            handler: |st, actor, _a| boxed(async move {
                require_admin(&actor)?;
                let st = st.read().await;
                let mut inv: Vec<_> = st.invites.values().cloned().collect();
                inv.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                to_value(&inv)
            }),
        },
        Tool {
            name: "accept_invite",
            description: "Accept an invite, creating a user with the invited email. \
                          Args: {token, name}. Rejects unknown/already-accepted/expired tokens.",
            input_schema: || obj_schema(&[("token", s_string()), ("name", s_string())], &["token", "name"]),
            handler: |st, _actor, a| boxed(async move {
                let token = arg_str(&a, "token")?;
                let name = arg_str(&a, "name")?;
                let mut st = st.write().await;
                let email = {
                    let inv = st.invites.get(&token).ok_or("invite not found")?;
                    if inv.accepted { return Err("invite already accepted".into()); }
                    if crate::handlers::is_invite_expired(&inv.created_at) { return Err("invite expired".into()); }
                    let email = inv.email.clone();
                    st.invites.get_mut(&token).unwrap().accepted = true;
                    email
                };
                let n = st.next_id("usr");
                let user = crate::models::User {
                    id: n.clone(), name, email, avatar_url: String::new(),
                    password_hash: None, salt: String::new(), pepper: String::new(),
                    is_admin: false, disabled: false, quota_mb: None, partners: Vec::new(),
                    totp_secret: None,
                };
                st.users.insert(n.clone(), user.clone());
                st.persist_invite(&token).await;
                st.persist_user(&n).await;
                to_value(&user)
            }),
        },
    ]
}

/// Parse `{target_type, target_id}` into a [`ShareTarget`].
fn parse_share_target(a: &Value) -> Result<crate::models::ShareTarget, String> {
    let ty = arg_str(a, "target_type")?;
    let id = arg_str(a, "target_id")?;
    match ty.as_str() {
        "user" => Ok(crate::models::ShareTarget::User(id)),
        "group" => Ok(crate::models::ShareTarget::Group(id)),
        _ => Err("target_type must be 'user' or 'group'".to_string()),
    }
}

/// Parse the optional `role` (defaults to viewer).
fn parse_share_role(a: &Value) -> crate::models::ShareRole {
    match a.get("role").and_then(|v| v.as_str()) {
        Some("contributor") => crate::models::ShareRole::Contributor,
        _ => crate::models::ShareRole::Viewer,
    }
}

/// Mirror of the REST handler's secret-preservation rule for S3 secrets.
fn preserve_secret(incoming: &mut String, existing: Option<&crate::models::S3Config>) {
    if incoming == crate::models::REDACTED_SECRET {
        *incoming = existing.map(|c| c.secret_access_key.clone()).unwrap_or_default();
    }
}

// ---------------------------------------------------------------------------
// Route handler + dispatch
// ---------------------------------------------------------------------------

/// Build the `tools/list` result from the catalog.
fn tools_list_result() -> Value {
    let list: Vec<Value> = tools()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.input_schema)(),
            })
        })
        .collect();
    json!({ "tools": list })
}

/// Dispatch a `tools/call` to the matching tool, enforcing auth first.
async fn dispatch_call(shared: Shared, actor: Actor, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return tool_content(&json!({ "error": "missing tool name" }), true),
    };
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let catalog = tools();
    let tool = match catalog.into_iter().find(|t| t.name == name) {
        Some(t) => t,
        None => return tool_content(&json!({ "error": format!("unknown tool '{name}'") }), true),
    };

    match (tool.handler)(shared, actor, arguments).await {
        Ok(v) => tool_content(&v, false),
        Err(e) => tool_content(&json!({ "error": e }), true),
    }
}

/// `POST /mcp` — the MCP JSON-RPC endpoint. Handles a single JSON-RPC request
/// object (the transport used by HTTP MCP clients).
pub async fn mcp_endpoint(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Json<Value> {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Json(rpc_error(Value::Null, PARSE_ERROR, "invalid JSON")),
    };

    // Basic JSON-RPC envelope validation.
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Json(rpc_error(id, INVALID_REQUEST, "missing method")),
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));

    match method {
        "initialize" => Json(rpc_result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": { "name": "photon", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": {} }
            }),
        )),
        // Notifications carry no id and expect no response; accept silently.
        "notifications/initialized" | "initialized" => Json(json!({ "jsonrpc": "2.0" })),
        "tools/list" => Json(rpc_result(id, tools_list_result())),
        "tools/call" => {
            // POSTGRES-FIRST: dispatch against a FRESH DB snapshot (the long-lived
            // in-memory state holds only seed data now). Wrapping the snapshot as a
            // throwaway `Shared` lets the existing dispatcher's reads hit DB data and
            // its writes persist through the snapshot's pool — no dispatcher changes.
            let work: Shared = match crate::handlers::request_snapshot(&shared).await {
                Some(s) => std::sync::Arc::new(tokio::sync::RwLock::new(s)),
                None => shared.clone(), // no DB (tests) → the in-memory state
            };
            // Authenticate + resolve the actor before dispatching.
            let actor = {
                let st = work.read().await;
                resolve_actor(&st, &headers)
            };
            match actor {
                Ok(actor) => {
                    let result = dispatch_call(work.clone(), actor, &params).await;
                    Json(rpc_result(id, result))
                }
                Err(msg) => Json(rpc_error(id, UNAUTHORIZED, format!("unauthorized: {msg}"))),
            }
        }
        other => Json(rpc_error(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method '{other}'"),
        )),
    }
}

// Keep the unused-constant lints quiet for codes reserved for completeness.
#[allow(dead_code)]
const _RESERVED_CODES: [i64; 2] = [INVALID_PARAMS, INTERNAL_ERROR];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::seed;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn shared() -> Shared {
        Arc::new(RwLock::new(seed()))
    }

    /// Acquire the process-wide OIDC env guard (shared with [`crate::oidc`]'s
    /// tests, since both read the same `OIDC_*` vars) and ensure OIDC_* are unset
    /// for the closure's duration; restores nothing (callers that set them must
    /// clean up before the guard drops). Returned guard must be held for the test.
    async fn clean_env_guard() -> tokio::sync::MutexGuard<'static, ()> {
        let g = crate::state::oidc_env_guard().lock().await;
        unsafe {
            std::env::remove_var("OIDC_ISSUER");
            std::env::remove_var("OIDC_AUDIENCE");
            std::env::remove_var("OIDC_HS256_SECRET");
            std::env::remove_var("OIDC_JWKS_JSON");
        }
        g
    }

    fn auth_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        h
    }

    async fn call(shared: &Shared, body: Value) -> Value {
        let bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());
        mcp_endpoint(State(shared.clone()), HeaderMap::new(), bytes).await.0
    }

    async fn call_auth(shared: &Shared, headers: HeaderMap, body: Value) -> Value {
        let bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());
        mcp_endpoint(State(shared.clone()), headers, bytes).await.0
    }

    /// Log in a seed user and return their session token.
    async fn session_for(shared: &Shared, user_id: &str) -> String {
        let mut st = shared.write().await;
        st.create_session(user_id)
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let st = shared();
        let resp = call(&st, json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        })).await;
        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], "photon");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_full_catalog() {
        let st = shared();
        let resp = call(&st, json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list"
        })).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Representative subset across every category.
        for n in [
            "list_users", "create_user", "set_user_password",
            "list_groups", "add_group_member",
            "list_photos", "patch_photo_metadata", "trash_photo", "archive_photo",
            "list_albums", "share_album", "contribute_to_album",
            "get_timeline", "update_timeline_prefs",
            "search", "upload_raw",
            "get_vault_status", "set_vault_pin", "unlock_vault", "add_vault_photos",
            "get_storage", "run_backup", "admin_stats", "audit_access",
            "get_smtp", "create_invite", "accept_invite", "render_photo",
            "get_user_storage",
        ] {
            assert!(names.contains(&n), "missing tool {n}");
        }
        // Every tool advertises an object inputSchema + non-empty description.
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().unwrap().len() > 10);
        }
        // Full catalog is sizeable.
        assert!(tools.len() >= 40, "catalog too small: {}", tools.len());
    }

    #[tokio::test]
    async fn unknown_method_is_error() {
        let st = shared();
        let resp = call(&st, json!({
            "jsonrpc": "2.0", "id": 3, "method": "no/such"
        })).await;
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notifications_initialized_is_tolerated() {
        let st = shared();
        let resp = call(&st, json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })).await;
        // No error returned.
        assert!(resp.get("error").is_none());
    }

    #[tokio::test]
    async fn call_without_token_is_rejected() {
        let _g = clean_env_guard().await;
        let st = shared();
        let resp = call(&st, json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "get_timeline", "arguments": { "user_id": "usr_alice" } }
        })).await;
        assert_eq!(resp["error"]["code"], UNAUTHORIZED);
    }

    #[tokio::test]
    async fn search_with_session_token_succeeds() {
        let _g = clean_env_guard().await;
        let st = shared();
        let token = session_for(&st, "usr_alice").await;
        let resp = call_auth(&st, auth_headers(&token), json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "search", "arguments": { "user_id": "usr_alice", "q": "" } }
        })).await;
        let content = &resp["result"]["content"][0]["text"];
        let text = content.as_str().unwrap();
        let results: Value = serde_json::from_str(text).unwrap();
        assert!(results.is_array());
        assert!(!results.as_array().unwrap().is_empty(), "alice should have photos");
        assert_eq!(resp["result"]["isError"], false);
    }

    #[tokio::test]
    async fn get_timeline_with_session_token_succeeds() {
        let _g = clean_env_guard().await;
        let st = shared();
        let token = session_for(&st, "usr_alice").await;
        let resp = call_auth(&st, auth_headers(&token), json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "get_timeline", "arguments": { "user_id": "usr_alice" } }
        })).await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let tl: Value = serde_json::from_str(text).unwrap();
        assert!(tl["sections"].is_array());
        assert!(!tl["sections"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn actor_cannot_read_another_users_vault() {
        let _g = clean_env_guard().await;
        let st = shared();
        // Bob authenticates but tries to read Alice's vault status.
        let token = session_for(&st, "usr_bob").await;
        let resp = call_auth(&st, auth_headers(&token), json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "get_vault_status", "arguments": { "user_id": "usr_alice" } }
        })).await;
        // tools/call returns a result with isError=true (authz enforced in handler).
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("forbidden"));
    }

    #[tokio::test]
    async fn non_admin_cannot_list_users() {
        let _g = clean_env_guard().await;
        let st = shared();
        let token = session_for(&st, "usr_bob").await; // bob is not admin
        let resp = call_auth(&st, auth_headers(&token), json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": { "name": "list_users", "arguments": {} }
        })).await;
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn oidc_hs256_token_maps_email_to_user() {
        use jsonwebtoken::{EncodingKey, Header, encode};
        // Hold the global env guard so no session-token test runs while OIDC_*
        // is set (and vice versa). The guard starts with OIDC_* cleared.
        let _g = clean_env_guard().await;
        unsafe {
            std::env::set_var("OIDC_ISSUER", "https://issuer.test");
            std::env::set_var("OIDC_AUDIENCE", "photon");
            std::env::set_var("OIDC_HS256_SECRET", "test-secret");
        }

        #[derive(serde::Serialize)]
        struct C<'a> { iss: &'a str, aud: &'a str, email: &'a str, exp: usize }
        let exp = (time::OffsetDateTime::now_utc() + time::Duration::hours(1)).unix_timestamp() as usize;
        let token = encode(
            &Header::default(),
            &C { iss: "https://issuer.test", aud: "photon", email: "alice@photon.app", exp },
            &EncodingKey::from_secret(b"test-secret"),
        ).unwrap();

        let st = shared();
        let resp = call_auth(&st, auth_headers(&token), json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": "search", "arguments": { "user_id": "usr_alice", "q": "" } }
        })).await;

        // Clean up env so other tests/runs are unaffected.
        unsafe {
            std::env::remove_var("OIDC_ISSUER");
            std::env::remove_var("OIDC_AUDIENCE");
            std::env::remove_var("OIDC_HS256_SECRET");
        }

        assert_eq!(resp["result"]["isError"], false, "resp: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let results: Value = serde_json::from_str(text).unwrap();
        assert!(results.is_array());
    }

    #[tokio::test]
    async fn oidc_invalid_signature_is_rejected() {
        let _g = clean_env_guard().await;
        unsafe {
            std::env::set_var("OIDC_ISSUER", "https://issuer.test");
            std::env::set_var("OIDC_AUDIENCE", "photon");
            std::env::set_var("OIDC_HS256_SECRET", "the-right-secret");
        }
        use jsonwebtoken::{EncodingKey, Header, encode};
        #[derive(serde::Serialize)]
        struct C<'a> { iss: &'a str, aud: &'a str, email: &'a str, exp: usize }
        let exp = (time::OffsetDateTime::now_utc() + time::Duration::hours(1)).unix_timestamp() as usize;
        // Signed with the WRONG secret.
        let token = encode(
            &Header::default(),
            &C { iss: "https://issuer.test", aud: "photon", email: "alice@photon.app", exp },
            &EncodingKey::from_secret(b"wrong-secret"),
        ).unwrap();
        let st = shared();
        let resp = call_auth(&st, auth_headers(&token), json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": { "name": "search", "arguments": { "user_id": "usr_alice" } }
        })).await;
        unsafe {
            std::env::remove_var("OIDC_ISSUER");
            std::env::remove_var("OIDC_AUDIENCE");
            std::env::remove_var("OIDC_HS256_SECRET");
        }
        assert_eq!(resp["error"]["code"], UNAUTHORIZED);
    }
}
