use std::sync::Arc;

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use tokio::sync::RwLock;

use crate::auth::{AuthUser, DbOptionExt, DbResultExt};

use crate::models::{
    AcceptInvite, AddMember, AddPhotos, Album, ContributeBody, CreateAlbum, CreateGroup,
    CreateInvite, CreateUser, Group, ImportAccepted, ImportBatch, ImportItem, ImportStage,
    MetadataOverride,
    ImportStatus, Invite, LoginBody, LoginResponse, PhotoView, RawUploadBody, RegisterBody,
    ResetToken,
    SetPasswordBody, SetPinBody, Share, ShareBody, SmtpConfig, StorageSettings, Timeline,
    TimelineSection, TimelinePrefs, TotpSetupResponse, TotpVerifyBody, UnlockBody, UpdatePrefs,
    UpdateSmtp, UpdateStorage,
    User, VaultContents, VaultCount, VaultPhotosBody, VaultStatus, REDACTED_SECRET,
};
use crate::state::{AccessViolation, AppState, JobStats, now_rfc3339};
use crate::transcode::{DeviceProfile, MediaFormat, RealTranscoder, TranscodePlan, Transcoder, negotiate};

pub type Shared = Arc<RwLock<AppState>>;

/// Build a per-request snapshot of domain state freshly loaded from Postgres
/// (the source of truth), carrying over runtime config. Complex read handlers
/// run their existing logic against this instead of any long-lived in-memory
/// cache — so reads are Postgres-first. Returns `None` when there's no DB
/// configured (the transitional test path), so callers fall back to the shared
/// in-memory state. The whole working set is loaded in one consistent read;
/// scoped queries are a later perf optimization.
pub async fn request_snapshot(shared: &Shared) -> Option<AppState> {
    let mut snap = AppState::default();
    {
        let st = shared.read().await;
        st.persistence.as_ref()?; // no DB → None → caller uses in-memory fallback
        let ctx = st.storage_ctx();
        snap.persistence = st.persistence.clone();
        snap.password_secret = st.password_secret.clone();
        snap.storage = ctx.storage;
        snap.smtp = st.smtp.clone();
        snap.data_dir = ctx.data_dir;
        snap.ml = st.ml.clone();
    }
    snap.load_from_db().await.ok()?;
    Some(snap)
}

/// Invite token time-to-live: invites older than this (by `created_at`) are
/// rejected at accept time. 72 hours.
pub const INVITE_TTL_SECS: i64 = 72 * 3600;

/// Password reset token time-to-live: reset tokens older than this (by
/// `created_at`) are rejected when setting a password. 24 hours.
pub const RESET_TTL_SECS: i64 = 24 * 3600;

/// Whether an RFC3339 `created_at` timestamp is older than `ttl_secs` relative to
/// now. A timestamp that fails to parse is treated as expired (fail-closed).
/// Public wrapper: is an invite's `created_at` older than [`INVITE_TTL_SECS`]?
/// Used by the MCP `accept_invite` tool to apply the same expiry rule.
pub fn is_invite_expired(created_at: &str) -> bool {
    is_expired(created_at, INVITE_TTL_SECS)
}

pub fn is_expired(created_at: &str, ttl_secs: i64) -> bool {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    match OffsetDateTime::parse(created_at, &Rfc3339) {
        Ok(t) => OffsetDateTime::now_utc().unix_timestamp() - t.unix_timestamp() >= ttl_secs,
        Err(_) => true,
    }
}

// ---- Users ----

pub async fn list_users(State(st): State<Shared>) -> Json<Vec<User>> {
    // Postgres-first: read users in a transaction; the RwLock is touched only to
    // grab the pool handle + apply config (gravatar). (Transitional in-memory
    // fallback for the not-yet-migrated test path with no DB.)
    let p = match st.read().await.persistence.clone() {
        Some(p) => p,
        None => return Json(Vec::new()),
    };
    let mut users: Vec<User> = p.load_users().await.unwrap_or_default();
    let st = st.read().await;
    let mut users: Vec<User> = users.drain(..).map(|u| st.public_user(&u)).collect();
    users.sort_by(|a, b| a.id.cmp(&b.id));
    Json(users)
}

pub async fn get_user(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<User>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let user = p.get_user(&id).await.or_500()?;
    let user = user.or_404()?;
    Ok(Json(st.read().await.public_user(&user)))
}

/// POST /api/users — admin creates a passwordless user (`usr_<n>`). The created
/// user has NO password yet; they must set one via a reset token (or accept).
/// The response never includes a password hash (skipped at serialization).
pub async fn create_user(
    State(st): State<Shared>,
    Json(body): Json<CreateUser>,
) -> Result<(StatusCode, Json<User>), StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let id = p.next_id("usr").await.or_500()?;
    let user = User {
        id,
        name: body.name,
        email: body.email,
        avatar_url: String::new(),
        password_hash: None,
        salt: String::new(),
        pepper: String::new(),
        is_admin: body.is_admin,
        disabled: false,
        quota_mb: None,
        partners: Vec::new(),
        totp_secret: None,
    };
    p.upsert_user(&user).await.or_500()?;
    Ok((StatusCode::CREATED, Json(user)))
}

/// POST /api/register — PUBLIC self-registration (no auth). Gated by the
/// `features.public_signup` flag:
/// - flag FALSE → `403 FORBIDDEN` (registration disabled).
/// - flag TRUE → create a non-admin, enabled user with the password set
///   immediately (argon2id with the server secret + a fresh per-user pepper),
///   mirroring `create_user`'s Postgres-first style. `409 CONFLICT` if the email
///   is already taken. Returns the created public `User` (no secrets).
pub async fn register(
    State(st): State<Shared>,
    Json(body): Json<RegisterBody>,
) -> Result<(StatusCode, Json<User>), StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Feature gate: read the flag straight from the settings singleton.
    let features = p.load_storage().await.ok().flatten().unwrap_or_default().features;
    if !features.public_signup {
        return Err(StatusCode::FORBIDDEN);
    }
    // Reject a duplicate email (case-insensitive), like a normal sign-up.
    let email_lc = body.email.trim().to_ascii_lowercase();
    let taken = p
        .load_users()
        .await
        .or_500()?
        .into_iter()
        .any(|u| u.email.to_ascii_lowercase() == email_lc);
    if taken {
        return Err(StatusCode::CONFLICT);
    }
    let id = p.next_id("usr").await.or_500()?;
    // Hash the password with the server-wide secret + a fresh CSPRNG pepper,
    // exactly as `set_user_password`/`User::set_password` do.
    let secret = st.read().await.password_secret().to_vec();
    let pepper = crate::state::random_hex(32);
    let mut user = User {
        id,
        name: body.name,
        email: body.email,
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
    user.set_password(&secret, pepper, &body.password);
    p.upsert_user(&user).await.or_500()?;
    // Return the PUBLIC user (secrets are `skip_serializing`, but mirror
    // `create_user`'s shape of returning the created `User`).
    Ok((StatusCode::CREATED, Json(user)))
}

/// PATCH /api/users/{id} — update profile/flags via an **RFC 6902 JSON Patch
/// document** (a JSON array of ops). Accept `Content-Type:
/// application/json-patch+json` or plain JSON. The patch is applied to a
/// patchable profile object built from the user's CURRENT
/// `{name, email, is_admin, disabled, quota_mb}`; only those five paths are then
/// written back. NEVER touches the password (it isn't part of the profile doc).
///
/// Type validation: `name`/`email` must be strings, `is_admin`/`disabled` bools,
/// `quota_mb` a non-negative integer or `null` (clears the quota). An invalid
/// patch (bad pointer/op) or a wrongly-typed value yields **422**. Unknown paths
/// produced by the patch are IGNORED (only the five known fields are read back).
/// Returns the updated `User` (secrets are never serialized).
pub async fn update_user(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(ops): Json<json_patch::Patch>,
) -> Result<Json<User>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first, ONE transaction: lock the row, apply the JSON Patch to
    // the 5 patchable profile fields, write back.
    use sqlx::Row as _;
    let mut tx = p.begin().await.or_500()?;
    let row = sqlx::query(
        "SELECT name, email, is_admin, disabled, quota_mb FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await
    .or_500()?
    .or_404()?;
    let mut doc = serde_json::json!({
        "name": row.get::<String, _>("name"),
        "email": row.get::<String, _>("email"),
        "is_admin": row.get::<bool, _>("is_admin"),
        "disabled": row.get::<bool, _>("disabled"),
        "quota_mb": row.get::<Option<i64>, _>("quota_mb"),
    });
    json_patch::patch(&mut doc, &ops).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    let name = doc.get("name").and_then(|v| v.as_str()).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let email = doc.get("email").and_then(|v| v.as_str()).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let is_admin = doc.get("is_admin").and_then(|v| v.as_bool()).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let disabled = doc.get("disabled").and_then(|v| v.as_bool()).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let quota: Option<i64> = match doc.get("quota_mb") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => Some(v.as_u64().ok_or(StatusCode::UNPROCESSABLE_ENTITY)? as i64),
    };
    sqlx::query(
        "UPDATE users SET name=$2, email=$3, is_admin=$4, disabled=$5, quota_mb=$6 WHERE id=$1",
    )
    .bind(&id)
    .bind(name)
    .bind(email)
    .bind(is_admin)
    .bind(disabled)
    .bind(quota)
    .execute(&mut *tx)
    .await
    .or_500()?;
    tx.commit().await.or_500()?;
    let user = p.get_user(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(user))
}

/// DELETE /api/users/{id} — remove the user and clean up references: drop their
/// groups membership, vault, prefs, and any album shares targeting them.
pub async fn delete_user(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first: delete the user + its vault/prefs, then strip the user
    // from group memberships and album shares.
    if p.get_user(&id).await.or_500()?.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let _ = p.delete_user(&id).await;
    let _ = p.delete_vault(&id).await;
    let _ = p.delete_prefs(&id).await;
    for mut g in p.load_groups().await.unwrap_or_default() {
        if g.member_ids.iter().any(|m| m == &id) {
            g.member_ids.retain(|m| m != &id);
            let _ = p.upsert_group(&g).await;
        }
    }
    for mut a in p.load_albums().await.unwrap_or_default() {
        let before = a.shares.len();
        a.shares
            .retain(|s| s.target != crate::models::ShareTarget::User(id.clone()));
        if a.shares.len() != before {
            let _ = p.upsert_album(&a).await;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/users/{id}/partners — user `{id}` grants `partner_id` partner access
/// to all of `{id}`'s LIVE photos (directed grant). Dedups; 404 if either user is
/// missing; 400 if `partner_id == id`. Returns the updated grantor `User`.
pub async fn add_partner(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<crate::models::AddPartner>,
) -> Result<Json<User>, StatusCode> {
    if body.partner_id == id {
        return Err(StatusCode::BAD_REQUEST);
    }
    let p = st.read().await.persistence.clone().or_500()?;
    if p.get_user(&id).await.ok().flatten().is_none()
        || p.get_user(&body.partner_id).await.ok().flatten().is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT partners FROM users WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut partners: Vec<String> =
        serde_json::from_value(row.get("partners")).unwrap_or_default();
    if !partners.contains(&body.partner_id) {
        partners.push(body.partner_id.clone());
    }
    sqlx::query("UPDATE users SET partners = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&partners).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let user = p.get_user(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(user))
}

/// DELETE /api/users/{id}/partners/{partner_id} — revoke the partner grant from
/// `{id}` to `partner_id`. 404 if user `{id}` is missing. Returns the updated
/// grantor `User` (removing a non-grant is a no-op success).
pub async fn remove_partner(
    State(st): State<Shared>,
    Path((id, partner_id)): Path<(String, String)>,
) -> Result<Json<User>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT partners FROM users WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut partners: Vec<String> =
        serde_json::from_value(row.get("partners")).unwrap_or_default();
    partners.retain(|x| x != &partner_id);
    sqlx::query("UPDATE users SET partners = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&partners).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let user = p.get_user(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(user))
}

/// POST /api/users/{id}/password — the USER sets their OWN password. Requires
/// EITHER a correct `current_password` (when one is already set) OR a valid
/// unused `reset_token` for this user. An admin can NEVER set it directly.
/// 403 if neither credential validates. Returns `{ ok: true }`.
pub async fn set_user_password(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<SetPasswordBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let has_password = st
        .users
        .get(&id)
        .map(|u| u.password_hash.is_some())
        .unwrap_or(false);

    // Authorize via reset token first, else via current password.
    let mut authorized = false;
    let mut consume_token: Option<String> = None;
    if let Some(tok) = &body.reset_token {
        if let Some(rt) = st.reset_tokens.get(tok) {
            // Valid only if unused, bound to this user, AND not expired.
            if !rt.used && rt.user_id == id && !is_expired(&rt.created_at, RESET_TTL_SECS) {
                authorized = true;
                consume_token = Some(tok.clone());
            }
        }
    }
    let secret = st.password_secret().to_vec();
    if !authorized {
        match &body.current_password {
            // A correct current password authorizes the change.
            Some(cur) if has_password => {
                authorized = st
                    .users
                    .get(&id)
                    .map(|u| u.verify_password(&secret, cur))
                    .unwrap_or(false);
            }
            _ => {}
        }
    }
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let pepper = st.new_pepper();
    if let Some(u) = st.users.get_mut(&id) {
        u.set_password(&secret, pepper, &body.new_password);
    }
    if let Some(tok) = consume_token {
        if let Some(rt) = st.reset_tokens.get_mut(&tok) {
            rt.used = true;
        }
    }
    st.persist_user(&id).await;
    if let Some(tok) = body.reset_token.as_ref() {
        st.persist_reset_token(tok).await;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/users/{id}/reset — admin action: mint a single-use reset token and
/// EMAIL a reset link to the user. NEVER returns or sets the password.
/// Returns `{ ok: true }`.
pub async fn reset_user_password(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Gather email + mailer + token under the lock, then send after dropping it.
    let (email, token, mailer) = {
        let mut snap = request_snapshot(&st).await.or_500()?;
        let st: &mut AppState = &mut snap;
        let email = match st.users.get(&id) {
            Some(u) => u.email.clone(),
            None => return Err(StatusCode::NOT_FOUND),
        };
        let token = st.new_reset_token();
        st.reset_tokens.insert(
            token.clone(),
            ResetToken {
                token: token.clone(),
                user_id: id.clone(),
                created_at: now_rfc3339(),
                used: false,
            },
        );
        st.persist_reset_token(&token).await;
        (email, token, st.mailer())
    };

    let subject = "Reset your Photon password".to_string();
    let message = format!(
        "A password reset was requested for your Photon account. \
         Use this token to set a new password: {token}\n\
         Reset link: /reset?token={token}"
    );
    // Swallow send errors (LogMailer path always succeeds in demo/tests).
    if let Err(e) = mailer.send(&email, &subject, &message).await {
        tracing::warn!("password reset email to {email} failed: {e}");
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- Groups ----

pub async fn list_groups(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
) -> Json<Vec<Group>> {
    // Groups the caller owns or belongs to. Postgres-first read.
    let all: Vec<Group> = match st.read().await.persistence.clone() {
        Some(p) => p.load_groups().await.unwrap_or_default(),
        None => return Json(Vec::new()),
    };
    let mut groups: Vec<Group> = all
        .into_iter()
        .filter(|g| g.owner_id == actor.0 || g.member_ids.contains(&actor.0))
        .collect();
    groups.sort_by(|a, b| a.id.cmp(&b.id));
    Json(groups)
}

pub async fn create_group(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Json(body): Json<CreateGroup>,
) -> Result<(StatusCode, Json<Group>), StatusCode> {
    // A caller may only create groups owned by themselves.
    if body.owner_id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }
    let p = st.read().await.persistence.clone().or_500()?;
    if p.get_user(&body.owner_id).await.or_500()?.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let id = p.next_id("grp").await.or_500()?;
    let group = Group {
        id,
        name: body.name,
        owner_id: body.owner_id,
        member_ids: body.member_ids,
    };
    p.upsert_group(&group).await.or_500()?;
    Ok((StatusCode::CREATED, Json(group)))
}

pub async fn get_group(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Group>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let group = p.get_group(&id).await.or_500()?;
    group.map(Json).or_404()
}

pub async fn add_group_member(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<AddMember>,
) -> Result<Json<Group>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    if p.get_user(&body.user_id).await.ok().flatten().is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT member_ids FROM groups WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut members: Vec<String> =
        serde_json::from_value(row.get("member_ids")).unwrap_or_default();
    if !members.contains(&body.user_id) {
        members.push(body.user_id);
    }
    sqlx::query("UPDATE groups SET member_ids = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&members).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let group = p.get_group(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(group))
}

pub async fn remove_group_member(
    State(st): State<Shared>,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<Json<Group>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT member_ids FROM groups WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut members: Vec<String> =
        serde_json::from_value(row.get("member_ids")).unwrap_or_default();
    members.retain(|m| m != &user_id);
    sqlx::query("UPDATE groups SET member_ids = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&members).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let group = p.get_group(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(group))
}

pub async fn delete_group(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    if st.groups.remove(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Drop any album shares that targeted this group.
    let mut touched_albums: Vec<String> = Vec::new();
    for album in st.albums.values_mut() {
        let before = album.shares.len();
        album
            .shares
            .retain(|s| s.target != crate::models::ShareTarget::Group(id.clone()));
        if album.shares.len() != before {
            touched_albums.push(album.id.clone());
        }
    }
    st.delete_group_row(&id).await;
    for aid in &touched_albums {
        st.persist_album(aid).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- Photos ----

pub async fn list_photos(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
) -> Json<Vec<PhotoView>> {
    let snap = request_snapshot(&st).await;
    let Some(st) = snap.as_ref() else { return Json(Vec::new()); };
    // Only photos the caller legitimately may read (own / shared album / partner).
    let mut photos: Vec<&crate::models::Photo> =
        st.photos.values().filter(|p| st.allowed(&actor.0, &p.id)).collect();
    photos.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
    Json(photos.into_iter().map(|p| p.effective()).collect())
}

pub async fn get_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PhotoView>, StatusCode> {
    // Postgres-first: read the photo in a transaction (one SELECT). Lock touched
    // only to grab the pool handle; in-memory fallback for the no-DB test path.
    let p = st.read().await.persistence.clone().or_500()?;
    let photo = p.get_photo(&id).await.or_500()?;
    photo.map(|p| Json(p.effective())).or_404()
}

/// PATCH /api/photos/{id}/metadata
/// Body is an **RFC 6902 JSON Patch document** — a JSON array of ops, applied to
/// the photo's CURRENT [`MetadataOverride`] serialized as a JSON object. Accept
/// `Content-Type: application/json-patch+json` or plain JSON (the body is the ops
/// array). Semantics on the override object:
/// - `replace`/`add` `/title` (etc.) SETS that override field;
/// - `remove /city` CLEARS that override (so the EXIF value shows again).
///
/// The patched object is deserialized back into a `MetadataOverride`; an invalid
/// patch (bad pointer, type mismatch, e.g. rating > 5 or a non-string title)
/// yields **422 Unprocessable Entity**. Returns the updated effective view.
pub async fn patch_photo_metadata(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(ops): Json<json_patch::Patch>,
) -> Result<Json<PhotoView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first, ONE transaction: lock the row, apply the JSON Patch to
    // its overrides, write back, commit.
    use sqlx::Row as _;
    let mut tx = p.begin().await.or_500()?;
    let row = sqlx::query("SELECT overrides FROM photos WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?;
    let row = row.or_404()?;
    let mut doc: serde_json::Value = row.get("overrides");
    json_patch::patch(&mut doc, &ops).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    let next: MetadataOverride =
        serde_json::from_value(doc).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    if next.rating.is_some_and(|r| r > 5) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    sqlx::query("UPDATE photos SET overrides = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&next).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let photo = p.get_photo(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(photo.effective()))
}

/// Background enrichment for an import batch: STAGES 3-5 (Thumbnail → Analysis →
/// Finalize). Runs after `upload_raw` has already done EXIF+Create and stored the
/// ORIGINALS durably, then returned 202. Persists the batch to Postgres after each
/// stage so a polling `GET /api/uploads/{id}` shows live progression on ANY
/// instance. Small inter-stage pauses let the client's ~350 ms poll catch each.
///
/// The image decode/encode (the heavy CPU work) runs on the blocking pool so it
/// never stalls the async runtime. The `snap` owns the pending bytes + the freshly
/// created photos + the DB pool, so it's self-contained.
async fn enrich_import(mut snap: AppState, batch_id: String, created: Vec<String>) {
    // STAGE 3 — THUMBNAIL: decode → resize → encode each primary's original into a
    // webp thumbnail (on the blocking pool). Persist the batch after EACH item so a
    // polling client watches files complete their thumbnail one by one, rather than
    // the whole batch jumping at the end of the phase.
    let takes = snap.import_thumbnail_take(&batch_id);
    snap.persist_import_batch(&batch_id).await; // all now Thumbnail/Processing
    for (file_id, photo_id, bytes) in takes {
        let thumb = tokio::task::spawn_blocking(move || AppState::render_thumbnail_bytes(&bytes))
            .await
            .ok()
            .flatten();
        snap.import_thumbnail_apply(&batch_id, &file_id, &photo_id, thumb);
        snap.persist_photo(&photo_id).await;
        snap.persist_import_batch(&batch_id).await;
    }

    // STAGE 4 — ANALYSIS (AI: OCR / people / context tags — no-op offline). Per
    // item so the gauge advances photo-by-photo when the ML sidecar is enabled.
    for (file_id, photo_id) in snap.import_analysis_primaries(&batch_id) {
        snap.import_set_item_stage(
            &batch_id,
            &file_id,
            crate::models::ImportStage::Analysis,
            crate::models::ImportStatus::Processing,
        );
        snap.persist_import_batch(&batch_id).await;
        snap.analyze_photo(&photo_id);
        snap.import_set_item_stage(
            &batch_id,
            &file_id,
            crate::models::ImportStage::Done,
            crate::models::ImportStatus::Ok,
        );
        snap.persist_photo(&photo_id).await;
        snap.persist_import_batch(&batch_id).await;
    }

    // STAGE 5 — FINALIZE: CLIP/face enrichment, push thumbnails to the backend,
    // attach the photos to the target album, and write-through-persist. (Originals
    // were already stored synchronously in the POST.)
    snap.import_phase_finalize(&batch_id, &created).await;
    snap.persist_import_batch(&batch_id).await;
}

/// POST /api/uploads/raw — STAGE 1 (Upload) of the async multi-stage import.
///
/// Validates owner/album, base64-decodes each file (the only synchronous work),
/// creates an [`ImportBatch`] (one [`ImportItem`] per file: stage `Upload`,
/// status `Ok`; later stages `Pending`), stashes the decoded bytes in an
/// in-memory pending store (NOT in the batch returned to clients), spawns the
/// async worker that drives EXIF → Thumbnail → Analysis → Done, and returns
/// **202 Accepted** with `{ batch_id, items }` WITHOUT blocking on processing.
/// Clients poll `GET /api/uploads/{batch_id}` for per-photo per-stage progress.
pub async fn upload_raw(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Json(body): Json<RawUploadBody>,
) -> Result<(StatusCode, Json<ImportAccepted>), StatusCode> {
    use base64::Engine as _;
    // A caller may only import into their own library.
    if body.owner_id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }

    let p = st.read().await.persistence.clone().or_500()?;
    {
        // Postgres-first, STAGED import. The POST does the fast + durable part
        // synchronously — STAGE 1 Upload + STAGE 2 EXIF/Create, then stores the
        // ORIGINALS to the backend so no uploaded bytes are lost before we ack —
        // and returns 202 quickly. The slow stages (Thumbnail, Analysis, Finalize)
        // run in a background task that persists the batch after each stage, so the
        // polling `GET /api/uploads/{id}` shows real per-stage progression. Ids come
        // from a DB-reserved block so they're cluster-unique.
        let mut snap = request_snapshot(&st).await.or_500()?;
        let base = p.next_id_base().await.or_500()?;
        snap.seed_id_counter(base);
        if !snap.users.contains_key(&body.owner_id) {
            return Err(StatusCode::NOT_FOUND);
        }
        if let Some(alb) = &body.album_id {
            if !snap.albums.contains_key(alb) {
                return Err(StatusCode::NOT_FOUND);
            }
            // Only the album owner or a Contributor may add photos to it.
            if !snap.can_contribute(&body.owner_id, alb) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        let batch_id = snap.next_id("imp");
        let mut items: Vec<ImportItem> = Vec::with_capacity(body.files.len());
        let mut pending: Vec<(String, String, String, Vec<u8>)> = Vec::new();
        for f in body.files {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(f.bytes.as_bytes())
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            let ext = f.filename.rsplit('.').next().filter(|e| *e != f.filename).unwrap_or("").to_lowercase();
            let file_id = snap.next_id("file");
            items.push(ImportItem {
                file_id: file_id.clone(),
                filename: f.filename.clone(),
                stage: ImportStage::Upload,
                status: ImportStatus::Ok,
                photo_id: None,
                error: None,
            });
            pending.push((file_id, f.filename, ext, bytes));
        }
        let batch = ImportBatch {
            id: batch_id.clone(),
            owner_id: body.owner_id.clone(),
            album_id: body.album_id.clone(),
            items,
            created_at: now_rfc3339(),
        };
        for (file_id, filename, ext, bytes) in pending {
            snap.pending_bytes.insert((batch_id.clone(), file_id), (filename, ext, bytes));
        }
        snap.imports.insert(batch_id.clone(), batch);

        // STAGE 2 (EXIF → Create) synchronously: extract EXIF, create the photo
        // rows + companion grouping, then DURABLY store the originals to the backend
        // (filesystem/S3) and persist the rows + batch. After this returns the
        // uploaded bytes are safe even if enrichment never finishes.
        snap.import_phase_exif(&batch_id);
        let created = snap.import_phase_create(&batch_id);
        snap.store_originals(&created).await;
        snap.store_companions(&created).await;
        for pid in &created {
            snap.persist_photo(pid).await;
        }
        snap.persist_import_batch(&batch_id).await;
        let items = snap
            .imports
            .get(&batch_id)
            .map(|b| b.items.clone())
            .unwrap_or_default();

        // STAGES 3-5 in the background: Thumbnail → Analysis → Finalize. The
        // snapshot (carrying the pending bytes + created photos + DB pool) moves
        // into the task; it persists the batch after each stage for live polling.
        tokio::spawn(enrich_import(snap, batch_id.clone(), created));

        Ok((StatusCode::ACCEPTED, Json(ImportAccepted { batch_id, items })))
    }
}

/// Body for [`upload_file`]: ONE file, base64-encoded.
#[derive(serde::Deserialize)]
pub struct UploadFileBody {
    pub owner_id: String,
    #[serde(default)]
    pub album_id: Option<String>,
    pub filename: String,
    pub bytes: String,
}

/// POST /api/uploads/file — upload ONE file (the front uploads files individually
/// with its own parallelism; there is no batch payload). The server pairs a RAW
/// with its same-base-name primary as a companion — any arrival order, serialized
/// per base via a Postgres advisory lock so a concurrent JPG+RAW don't both create
/// a photo. Returns the resulting photo so the client adds it to the timeline
/// directly. Face detection runs in the BACKGROUND (the response isn't blocked by
/// the ~10s detector; a process-wide semaphore keeps detections serial/reliable).
pub async fn upload_file(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Json(body): Json<UploadFileBody>,
) -> Result<(StatusCode, Json<PhotoView>), StatusCode> {
    if body.owner_id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body.bytes.as_bytes())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let pool = st.read().await.persistence.clone().or_500()?;
    let pool2 = pool.clone();
    let filename = body.filename.clone();
    let ext = filename
        .rsplit('.')
        .next()
        .filter(|e| *e != filename.as_str())
        .unwrap_or("")
        .to_lowercase();
    let base = crate::state::base_name(&filename);
    let album_id = body.album_id.clone();
    let owner = body.owner_id.clone();

    // AUTHZ: a photo may only be attached to an album the actor OWNS or can
    // CONTRIBUTE to — central authz doesn't fire on `/api/uploads`, so enforce it
    // here, before the heavy ingest, to stop album injection into a foreign album.
    if let Some(al) = &album_id {
        let snap0 = request_snapshot(&st).await.or_500()?;
        if !snap0.albums.contains_key(al) {
            return Err(StatusCode::NOT_FOUND);
        }
        if !snap0.can_contribute(&owner, al) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Pairing is serialized per (owner, base name).
    let result: Option<(String, PhotoView, bool, bool)> = pool
        .with_base_lock(&owner, &base, async {
            let mut snap = request_snapshot(&st).await?;
            if let Ok(b) = pool2.next_id_base().await {
                snap.seed_id_counter(b);
            }
            let (id, needs_faces) = snap
                .ingest_single_file(&owner, &filename, &ext, bytes, &crate::extract::ExifExtractor)
                .await;
            if let Some(al) = &album_id {
                // Re-check on the locked snapshot (defense-in-depth vs. the pre-check).
                if snap.can_contribute(&owner, al) {
                    if let Some(a) = snap.albums.get_mut(al) {
                        if !a.photo_ids.contains(&id) {
                            a.photo_ids.push(id.clone());
                        }
                        snap.persist_album(al).await;
                    }
                }
            }
            snap.persist_photo(&id).await;
            let feat = snap.storage.features.faces;
            let view = snap.photos.get(&id).map(|p| p.effective())?;
            Some((id, view, needs_faces, feat))
        })
        .await;
    let (id, view, needs_faces, feat_faces) = result.or_500()?;

    // Detect faces off the request path. Prefer a DURABLE graphile job (retried,
    // survives a restart, claimed once across the cluster); fall back to an inline
    // background task only when there's no worker queue (offline/tests).
    if needs_faces && feat_faces {
        let utils = st.read().await.worker_utils.clone();
        match utils {
            Some(u) => {
                if let Err(e) = u
                    .add_job(
                        crate::jobs::DetectFaces { photo_id: id.clone(), owner_id: owner.clone() },
                        graphile_worker::JobSpec::default(),
                    )
                    .await
                {
                    tracing::warn!("could not enqueue durable face detection for {id}: {e}");
                }
            }
            None => {
                let st2 = st.clone();
                tokio::spawn(async move {
                    crate::jobs::detect_faces_for(&st2, &id, &owner).await;
                });
            }
        }
    }

    Ok((StatusCode::CREATED, Json(view)))
}

/// GET /api/uploads/{batch_id} — the polling endpoint. Returns the current
/// [`ImportBatch`] (items with stage/status/photo_id). 404 if unknown.
pub async fn get_import(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path(batch_id): Path<String>,
) -> Result<Json<ImportBatch>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let batch = p.get_import_batch(&batch_id).await.or_500()?;
    let batch = batch.or_404()?;
    // Ownership check IN the handler: import batches aren't loaded into the
    // request snapshot, so `resource_authz`'s `/api/uploads/{id}` arm fails open
    // (sees an empty `imports` map). Enforce here — only the batch owner (or an
    // admin) may read its progress. Mirrors `resource_authz`'s intent.
    let is_admin = p
        .get_user(&actor.0)
        .await
        .ok()
        .flatten()
        .map(|u| u.is_admin)
        .unwrap_or(false);
    if batch.owner_id != actor.0 && !is_admin {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(batch))
}


/// GET /api/photos/{id}/thumb — return the stored thumbnail bytes with the right
/// Content-Type, or 404 when none exists (e.g. the demo seed has no thumb bytes).
pub async fn get_thumb(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let st = st.read().await;
    match st.load_thumb(&id).await {
        Some((bytes, ct)) => Ok(([(header::CONTENT_TYPE, ct)], bytes)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// POST /api/photos/{id}/analyze — re-run the AI-analysis import stage (OCR /
/// people / context tags) for the photo and return the updated view. 404 if
/// the photo is unknown.
pub async fn analyze_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PhotoView>, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    if !st.analyze_photo(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let view = st.photos.get(&id).map(|p| p.effective()).or_404()?;
    st.persist_photo(&id).await;
    Ok(Json(view))
}

// ---- Photo lifecycle: trash + archive ----

/// DELETE /api/photos/{id} — soft-delete (move to trash). Returns the view.
pub async fn trash_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PhotoView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let photo = p.set_photo_deleted_at(&id, Some(now_rfc3339())).await
        .or_500()?
        .or_404()?;
    Ok(Json(photo.effective()))
}

/// POST /api/photos/{id}/restore — clear deleted_at (recover from trash).
pub async fn restore_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PhotoView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let photo = p.set_photo_deleted_at(&id, None).await
        .or_500()?
        .or_404()?;
    Ok(Json(photo.effective()))
}

/// POST /api/photos/{id}/archive — archived = true.
pub async fn archive_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PhotoView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let photo = p.set_photo_archived(&id, true).await
        .or_500()?
        .or_404()?;
    Ok(Json(photo.effective()))
}

/// POST /api/photos/{id}/unarchive — archived = false.
pub async fn unarchive_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PhotoView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let photo = p.set_photo_archived(&id, false).await
        .or_500()?
        .or_404()?;
    Ok(Json(photo.effective()))
}

/// DELETE /api/photos/{id}/permanent — hard remove now (also from albums).
pub async fn permanent_delete_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    if st.photos.remove(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut touched_albums: Vec<String> = Vec::new();
    for album in st.albums.values_mut() {
        let before = album.photo_ids.len();
        album.photo_ids.retain(|pid| pid != &id);
        if album.photo_ids.len() != before {
            touched_albums.push(album.id.clone());
        }
    }
    st.delete_photo_row(&id).await;
    for aid in &touched_albums {
        st.persist_album(aid).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/trash — photos currently in trash (deleted_at is some).
pub async fn list_trash(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
) -> Json<Vec<PhotoView>> {
    // Trash is private to the owner. Postgres-first read.
    let all: Vec<crate::models::Photo> = match st.read().await.persistence.clone() {
        Some(p) => p.load_photos().await.unwrap_or_default(),
        None => return Json(Vec::new()),
    };
    let mut photos: Vec<crate::models::Photo> = all
        .into_iter()
        .filter(|p| p.deleted_at.is_some() && p.owner_id == actor.0)
        .collect();
    photos.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
    Json(photos.iter().map(|p| p.effective()).collect())
}

/// GET /api/archive — archived photos that are not trashed.
pub async fn list_archive(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
) -> Json<Vec<PhotoView>> {
    // Archive is private to the owner. Postgres-first read.
    let all: Vec<crate::models::Photo> = match st.read().await.persistence.clone() {
        Some(p) => p.load_photos().await.unwrap_or_default(),
        None => return Json(Vec::new()),
    };
    let mut photos: Vec<crate::models::Photo> = all
        .into_iter()
        .filter(|p| p.archived && p.deleted_at.is_none() && p.owner_id == actor.0)
        .collect();
    photos.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
    Json(photos.iter().map(|p| p.effective()).collect())
}

// ---- Albums ----

pub async fn list_albums(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
) -> Json<Vec<Album>> {
    // Albums the caller owns or that are shared to them (directly or via a group).
    // Postgres-first: load albums + groups, reuse the share-resolution logic on a
    // transient view (no long-lived in-memory state).
    let p = match st.read().await.persistence.clone() {
        Some(p) => p,
        None => return Json(Vec::new()),
    };
    use crate::models::ShareTarget;
    let albums = p.load_albums().await.unwrap_or_default();
    let groups = p.load_groups().await.unwrap_or_default();
    // The groups the caller belongs to (for group-targeted shares).
    let my_groups: std::collections::HashSet<String> = groups
        .iter()
        .filter(|g| g.member_ids.contains(&actor.0))
        .map(|g| g.id.clone())
        .collect();
    let shared_to_me = |a: &Album| {
        a.shares.iter().any(|s| match &s.target {
            ShareTarget::User(u) => *u == actor.0,
            ShareTarget::Group(g) => my_groups.contains(g),
        })
    };
    let mut albums: Vec<Album> = albums
        .into_iter()
        .filter(|a| a.owner_id == actor.0 || shared_to_me(a))
        .collect();
    albums.sort_by(|a, b| a.id.cmp(&b.id));
    Json(albums)
}

pub async fn create_album(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Json(body): Json<CreateAlbum>,
) -> Result<(StatusCode, Json<Album>), StatusCode> {
    // A caller may only create albums owned by themselves.
    if body.owner_id != actor.0 {
        return Err(StatusCode::FORBIDDEN);
    }
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first: validate + mint id + insert (one INSERT = one tx).
    if p.get_user(&body.owner_id).await.or_500()?.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let id = p.next_id("alb").await.or_500()?;
    let cover_seed = match body.photo_ids.first() {
        Some(pid) => p.get_photo(pid).await.ok().flatten().map(|ph| ph.seed).unwrap_or(0),
        None => 0,
    };
    let album = Album {
        id,
        name: body.name,
        owner_id: body.owner_id,
        cover_seed,
        photo_ids: body.photo_ids,
        shares: Vec::new(),
    };
    p.upsert_album(&album).await.or_500()?;
    Ok((StatusCode::CREATED, Json(album)))
}

pub async fn get_album(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Album>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let album = p.get_album(&id).await.or_500()?;
    album.map(Json).or_404()
}

pub async fn delete_album(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    if p.get_album(&id).await.or_500()?.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    p.delete_album(&id).await.or_500()?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_album_photos(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<AddPhotos>,
) -> Result<Json<Album>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first, ONE transaction: validate photos, lock the album, append
    // the new (distinct) photo ids, write back.
    //
    // SECURITY: the album owner may only add photos THEY own. Without this, an
    // owner could inject another user's (enumerable) photo id into their own album
    // and thereby fabricate a grant to it — reading it via /render, /original and
    // search. Mirrors `contribute_to_album`'s per-photo ownership check.
    let album_owner = p.get_album(&id).await.or_500()?.or_404()?.owner_id;
    for pid in &body.photo_ids {
        match p.get_photo(pid).await.or_500()? {
            None => return Err(StatusCode::NOT_FOUND),
            Some(ph) if ph.owner_id != album_owner => return Err(StatusCode::BAD_REQUEST),
            Some(_) => {}
        }
    }
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT photo_ids FROM albums WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut photo_ids: Vec<String> =
        serde_json::from_value(row.get("photo_ids")).unwrap_or_default();
    for pid in body.photo_ids {
        if !photo_ids.contains(&pid) {
            photo_ids.push(pid);
        }
    }
    sqlx::query("UPDATE albums SET photo_ids = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&photo_ids).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let album = p.get_album(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(album))
}

pub async fn add_album_share(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<ShareBody>,
) -> Result<Json<Album>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first: update the album's shares in one transaction, then notify.
    let target = body.target.clone();
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT shares FROM albums WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut shares: Vec<Share> = serde_json::from_value(row.get("shares")).unwrap_or_default();
    if let Some(existing) = shares.iter_mut().find(|s| s.target == target) {
        existing.role = body.role;
    } else {
        shares.push(Share { target: target.clone(), role: body.role });
    }
    sqlx::query("UPDATE albums SET shares = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&shares).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let album = p.get_album(&id).await
        .or_500()?
        .or_404()?;
    // Recipients (target user, or every group member) resolved from a DB snapshot.
    let recipients = match request_snapshot(&st).await {
        Some(s) => s.target_emails(&target),
        None => Vec::new(),
    };
    let mailer = { st.read().await.mailer() };
    let message = format!("The album \"{}\" was shared with you on Photon.", album.name);
    notify_all(mailer, recipients, "An album was shared with you".to_string(), message).await;
    Ok(Json(album))
}

/// DELETE /api/albums/{id}/shares — remove a share by TARGET (role ignored).
pub async fn remove_album_share(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<ShareBody>,
) -> Result<Json<Album>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT shares FROM albums WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut shares: Vec<Share> = serde_json::from_value(row.get("shares")).unwrap_or_default();
    shares.retain(|s| s.target != body.target);
    sqlx::query("UPDATE albums SET shares = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&shares).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let album = p.get_album(&id).await
        .or_500()?
        .or_404()?;
    Ok(Json(album))
}

/// POST /api/albums/{id}/contribute — a Contributor (or the owner) adds their
/// OWN photos to the album. Ownership is NOT reassigned.
/// 403 if the user may not contribute; 404 if album/photos missing; 400 if any
/// photo is not owned by the contributor.
pub async fn contribute_to_album(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ContributeBody>,
) -> Result<Json<Album>, StatusCode> {
    // The contributor is the AUTHENTICATED caller — never a client-supplied id
    // (which would let one user contribute "as" another).
    let user_id = actor.0.clone();

    let p = st.read().await.persistence.clone().or_500()?;
    // Authz + validation against a DB snapshot, append in one transaction.
    let snap = request_snapshot(&st).await.or_500()?;
    if !snap.albums.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if !snap.can_contribute(&user_id, &id) {
        return Err(StatusCode::FORBIDDEN);
    }
    for pid in &body.photo_ids {
        match snap.photos.get(pid) {
            None => return Err(StatusCode::NOT_FOUND),
            Some(ph) if ph.owner_id != user_id => return Err(StatusCode::BAD_REQUEST),
            Some(_) => {}
        }
    }
    let mut tx = p.begin().await.or_500()?;
    use sqlx::Row as _;
    let row = sqlx::query("SELECT photo_ids FROM albums WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .or_500()?
        .or_404()?;
    let mut photo_ids: Vec<String> =
        serde_json::from_value(row.get("photo_ids")).unwrap_or_default();
    let mut added = 0usize;
    for pid in body.photo_ids {
        if !photo_ids.contains(&pid) {
            photo_ids.push(pid);
            added += 1;
        }
    }
    sqlx::query("UPDATE albums SET photo_ids = $2 WHERE id = $1")
        .bind(&id)
        .bind(serde_json::to_value(&photo_ids).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .or_500()?;
    tx.commit().await.or_500()?;
    let album = p.get_album(&id).await
        .or_500()?
        .or_404()?;
    if added > 0 {
        let actor_name = snap.users.get(&user_id).map(|u| u.name.clone()).unwrap_or_else(|| user_id.clone());
        let mut recipients: Vec<String> = Vec::new();
        if album.owner_id != user_id {
            if let Some(owner) = snap.users.get(&album.owner_id) {
                recipients.push(owner.email.clone());
            }
        }
        let mailer = { st.read().await.mailer() };
        let message = format!(
            "{} added {} photo(s) to the album \"{}\".",
            actor_name, added, album.name
        );
        notify_all(mailer, recipients, "New photos in a shared album".to_string(), message).await;
    }
    Ok(Json(album))
}

// ---- Public album links (no-account read-only sharing) ----

/// Response of `POST /api/albums/{id}/public-link`: the minted `token` and a
/// ready-to-share relative `url`.
#[derive(serde::Serialize)]
pub struct PublicLinkResponse {
    pub token: String,
    pub url: String,
}

/// The public album view served at `GET /api/public/albums/{token}`: the album
/// metadata plus its LIVE photos (no secrets, no auth).
#[derive(serde::Serialize)]
pub struct PublicAlbumView {
    pub album: Album,
    pub photos: Vec<PhotoView>,
}

/// POST /api/albums/{id}/public-link — owner/admin mints a public link for the
/// album (album ownership is already enforced by `resource_authz` on the
/// `/api/albums/{id}/...` path). Gated by `features.public_links`: when the flag
/// is OFF this returns `403 FORBIDDEN`. Otherwise it generates a CSPRNG token,
/// durably stores the `token -> album_id` mapping, and returns `{ token, url }`.
pub async fn create_public_link(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<PublicLinkResponse>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let features = p.load_storage().await.ok().flatten().unwrap_or_default().features;
    if !features.public_links {
        return Err(StatusCode::FORBIDDEN);
    }
    // The album must exist (resource_authz already proved the caller owns it).
    if p.get_album(&id).await.or_500()?.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Unpredictable token from the OS CSPRNG (same primitive as session/reset
    // tokens): 32 random bytes, hex-encoded.
    let token = crate::state::random_hex(32);
    p.upsert_public_link(&token, &id)
        .await
        .or_500()?;
    let url = format!("/api/public/albums/{token}");
    Ok(Json(PublicLinkResponse { token, url }))
}

/// DELETE /api/albums/{id}/public-link/{token} — owner revokes a public link by
/// deleting its mapping. Idempotent (`204` even if the token was already gone).
pub async fn revoke_public_link(
    State(st): State<Shared>,
    Path((_id, token)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    p.delete_public_link(&token)
        .await
        .or_500()?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve a public-link `token` to its album id, enforcing the
/// `features.public_links` flag. Returns 404 (NOT the album) when the flag is
/// off OR the token is unknown — a uniform "no such public album" so a disabled
/// feature is indistinguishable from a bad token.
async fn resolve_public_album(p: &crate::db::Persistence, token: &str) -> Result<String, StatusCode> {
    let features = p.load_storage().await.ok().flatten().unwrap_or_default().features;
    if !features.public_links {
        return Err(StatusCode::NOT_FOUND);
    }
    p.get_public_link(token)
        .await
        .or_500()?
        .or_404()
}

/// GET /api/public/albums/{token} — PUBLIC (no auth). Returns the album metadata
/// plus its LIVE photos (`deleted_at.is_none() && !archived`) as `PhotoView`s,
/// only when `features.public_links` is on and the token is known; otherwise 404.
pub async fn public_album(
    State(st): State<Shared>,
    Path(token): Path<String>,
) -> Result<Json<PublicAlbumView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let album_id = resolve_public_album(&p, &token).await?;
    let album = p
        .get_album(&album_id)
        .await
        .or_500()?
        .or_404()?;
    // Fetch each member photo, keeping only LIVE ones (not trashed, not archived).
    let mut photos: Vec<PhotoView> = Vec::new();
    for pid in &album.photo_ids {
        if let Some(photo) = p.get_photo(pid).await.or_500()? {
            // Live (not trashed/archived) AND not in anyone's vault.
            if photo.deleted_at.is_none() && !photo.archived && !p.is_photo_vaulted(pid).await.or_500()? {
                photos.push(photo.effective());
            }
        }
    }
    Ok(Json(PublicAlbumView { album, photos }))
}

/// Confirm `photo_id` is a LIVE member of the token's album, returning the loaded
/// `Photo`. 404 if the public link is invalid/disabled, the photo isn't in the
/// album, or the photo isn't live. Shared by the public thumb + render endpoints.
async fn public_album_photo(
    p: &crate::db::Persistence,
    token: &str,
    photo_id: &str,
) -> Result<crate::models::Photo, StatusCode> {
    let album_id = resolve_public_album(p, token).await?;
    let album = p
        .get_album(&album_id)
        .await
        .or_500()?
        .or_404()?;
    if !album.photo_ids.iter().any(|pid| pid == photo_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let photo = p
        .get_photo(photo_id)
        .await
        .or_500()?
        .or_404()?;
    if photo.deleted_at.is_some() || photo.archived || p.is_photo_vaulted(photo_id).await.or_500()? {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(photo)
}

/// GET /api/public/albums/{token}/photos/{id}/thumb — PUBLIC thumbnail bytes for
/// a photo that belongs to the token's album. 404 unless the flag is on, the
/// token is known, and the photo is a live member of that album.
pub async fn public_album_thumb(
    State(st): State<Shared>,
    Path((token, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Membership + liveness check before serving any bytes.
    public_album_photo(&p, &token, &id).await?;
    let st = st.read().await;
    match st.load_thumb(&id).await {
        Some((bytes, ct)) => Ok(([(header::CONTENT_TYPE, ct)], bytes)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// GET /api/public/albums/{token}/photos/{id}/render?w=&h= — PUBLIC screen render
/// for a photo belonging to the token's album. Same gating as the public thumb;
/// reuses the image-render path. Video originals are never publicly transcoded
/// (404 — the public surface is read-only image viewing).
pub async fn public_album_render(
    State(st): State<Shared>,
    Path((token, id)): Path<(String, String)>,
    Query(q): Query<RenderQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let photo = public_album_photo(&p, &token, &id).await?;
    // Public links are read-only IMAGE viewing; videos aren't publicly served.
    let src_ext = photo.filename.rsplit('.').next().unwrap_or("");
    if photo.kind == "video"
        || MediaFormat::from_ext(src_ext).map(|f| f.is_video()).unwrap_or(false)
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let (bytes, ct) = {
        let cfg = st.read().await.storage_ctx();
        cfg.load_original_blob(&photo).await.or_404()?
    };
    let source = MediaFormat::from_mime(&ct);
    let target = match q.fmt {
        Some(f) if f.is_image() => f,
        _ => match source {
            Some(MediaFormat::Jpeg) => MediaFormat::Jpeg,
            Some(MediaFormat::Png) => MediaFormat::Png,
            _ => MediaFormat::Webp,
        },
    };
    if !target.is_image() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (orig_w, orig_h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((RENDER_MAX_EDGE, RENDER_MAX_EDGE));
    let width = q.w.unwrap_or(RENDER_MAX_EDGE).min(orig_w).max(1);
    let height = q.h.unwrap_or(RENDER_MAX_EDGE).min(orig_h).max(1);
    let plan = TranscodePlan {
        format: target,
        width,
        height,
        source_format: source.unwrap_or(MediaFormat::Jpeg),
        needs_transcode: true,
    };
    let out = tokio::task::spawn_blocking(move || RealTranscoder.transcode_image(&bytes, &plan))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|e| {
            tracing::warn!("public render of {id} failed: {e}");
            StatusCode::UNPROCESSABLE_ENTITY
        })?;
    Ok(([(header::CONTENT_TYPE, target.mime())], out))
}

/// Send `message` to every recipient through `mailer`. Sending failures are
/// logged and swallowed — they must NEVER fail the originating request.
async fn notify_all(
    mailer: Box<dyn crate::mailer::Notification>,
    recipients: Vec<String>,
    subject: String,
    message: String,
) {
    for to in recipients {
        if let Err(e) = mailer.send(&to, &subject, &message).await {
            tracing::warn!("notification email to {to} failed: {e}");
        }
    }
}

// ---- Search ----

/// GET /api/users/{id}/search?q=<term> — photos reachable by the user via
/// search (own photos + every photo of any album they can access), excluding
/// archived/trashed/vault photos, filtered by `q`.
#[derive(serde::Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    pub camera: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub place: Option<String>,
    /// "lat,lng,radiusKm" for geo-radius search.
    pub near: Option<String>,
    /// CONTEXT RECOGNITION (CLIP): opt into open-vocabulary semantic ranking of
    /// the (facet-filtered) candidates against `q` via CLIP embeddings. When
    /// omitted it defaults to ON whenever the ML sidecar is configured, so
    /// queries like "yellow car" / "voiture jaune" just work; set `semantic=false`
    /// to force the legacy keyword/substring search. With ML disabled this flag is
    /// ignored entirely (keyword search is always used).
    pub semantic: Option<bool>,
}

fn parse_near(s: &str) -> Option<(f64, f64, f64)> {
    let mut it = s.split(',').map(|x| x.trim().parse::<f64>());
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(lat)), Some(Ok(lng)), Some(Ok(r))) => Some((lat, lng, r)),
        _ => None,
    }
}

pub async fn search_photos(
    State(st): State<Shared>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SearchQuery>,
) -> Result<Json<Vec<PhotoView>>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let q = query.q.clone();
    let filters = crate::state::SearchFilters {
        q: query.q,
        camera: query.camera.filter(|s| !s.is_empty()),
        from: query.from.filter(|s| !s.is_empty()),
        to: query.to.filter(|s| !s.is_empty()),
        place: query.place.filter(|s| !s.is_empty()),
        near: query.near.as_deref().and_then(parse_near),
    };

    // CONTEXT RECOGNITION (CLIP): when there is a text query, ML is configured,
    // and the caller didn't opt out (`semantic=false`), rank by CLIP cosine
    // similarity instead of keyword substring match. We build the candidate set
    // from the SAME access scope + facet filters (camera/date/place/geo) but with
    // the free-text term cleared, then let `semantic_rank` order them by meaning.
    // Any failure (no embeddings, query embed fails) falls back to keyword search.
    let want_semantic = query.semantic.unwrap_or(true) && st.ml.is_some() && !q.trim().is_empty();
    if want_semantic {
        let facet_only = crate::state::SearchFilters {
            q: String::new(),
            camera: filters.camera.clone(),
            from: filters.from.clone(),
            to: filters.to.clone(),
            place: filters.place.clone(),
            near: filters.near,
        };
        let candidates = st.search_filtered(&id, &facet_only);
        // 0.2 is a permissive CLIP cosine floor: high enough to drop unrelated
        // photos, low enough to keep loose-but-plausible matches.
        if let Some(ranked) = st.semantic_rank(candidates, &q, 0.2).await {
            return Ok(Json(ranked));
        }
    }

    Ok(Json(st.search_filtered(&id, &filters)))
}

/// GET /api/users/{id}/duplicates response: groups of >= 2 near-duplicate photos.
#[derive(Debug, serde::Serialize)]
pub struct DuplicatesView {
    pub groups: Vec<Vec<PhotoView>>,
}

/// GET /api/users/{id}/duplicates — near-duplicate photo groups for the user,
/// as computed by the daily `duplicates` job (perceptual hashing over thumbnails).
/// Only LIVE photos are returned. 404 if the user is missing.
pub async fn get_duplicates(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<DuplicatesView>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(DuplicatesView {
        groups: st.duplicate_views(&id),
    }))
}

// ---- Face recognition (People) ----

/// GET /api/users/{id}/people — the user's face clusters (People), as built by
/// face detection + clustering. Each item is a summary: `person_id`, `name`,
/// `face_count`, a `cover` crop ({photo_id, bbox}), and `sample_photo_ids`.
/// NEVER includes face embeddings (sensitive). 404 if the user is missing.
pub async fn list_people(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::models::PersonView>>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(st.people_views(&id)))
}

/// POST /api/people/{person_id}/name — name (or rename) a face cluster. Empty
/// name clears it. Propagates the name into the affected photos' `ai_people` (so
/// search by name works). 404 if the person is unknown.
pub async fn name_person(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::NamePersonBody>,
) -> Result<StatusCode, StatusCode> {
    // Postgres-first: rename in a DB snapshot, write faces + affected photos back.
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.name_person(&person_id, &body.name).or_404()?;
    snap.persist_faces(&owner).await;
    let photo_ids: Vec<String> = snap
        .photos
        .values()
        .filter(|p| p.owner_id == owner)
        .map(|p| p.id.clone())
        .collect();
    for pid in photo_ids {
        snap.persist_photo(&pid).await;
    }
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/relationships — link this person cluster to
/// another (`{other_person_id, relation}`) with a kinship label; the reciprocal
/// edge is created automatically. Both clusters must belong to the same owner.
/// 400 on a self-link / empty relation / cross-owner; 404 if a person is unknown.
pub async fn add_relationship(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::RelationshipBody>,
) -> Result<StatusCode, StatusCode> {
    let _p = st.read().await.persistence.clone().or_500()?;
    let mut snap = request_snapshot(&st).await.or_500()?;
    // Distinguish "unknown person" (404) from "invalid link" (400).
    if !snap.people.contains_key(&person_id) || !snap.people.contains_key(&body.other_person_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let owner = snap
        .link_people(&person_id, &body.other_person_id, &body.relation)
        .ok_or(StatusCode::BAD_REQUEST)?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// DELETE /api/people/{person_id}/relationships/{other_person_id} — remove the
/// reciprocal kinship link between two People. 404 if either is unknown.
pub async fn remove_relationship(
    State(st): State<Shared>,
    Path((person_id, other_person_id)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    let _p = st.read().await.persistence.clone().or_500()?;
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap
        .unlink_people(&person_id, &other_person_id)
        .or_404()?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// GET /api/people/{person_id}/photos — the live photos a Person appears in,
/// newest-first. Scoped: the requester must equal the person's owner (passed as
/// `?owner=` ; defaults to the person's own owner). 404 if the person is unknown.
pub async fn person_photos(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Query(q): Query<PersonPhotosQuery>,
) -> Result<Json<Vec<PhotoView>>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    // The owner scope is the person's owner; an optional `?owner=` must match it.
    let person_owner = st
        .people
        .get(&person_id)
        .map(|p| p.owner_id.clone())
        .or_404()?;
    if let Some(req) = q.owner.filter(|s| !s.is_empty()) {
        if req != person_owner {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let photos = st.person_photos(&person_owner, &person_id).or_404()?;
    Ok(Json(photos))
}

#[derive(Debug, serde::Deserialize)]
pub struct PersonPhotosQuery {
    #[serde(default)]
    pub owner: Option<String>,
}

// ---- People Studio curation endpoints ----
//
// All scoped by central authz on `/api/people/{id}/..` (owner-or-grant; writes
// owner-only). Each loads a fresh DB snapshot, mutates IN PLACE (person ids stay
// stable), then writes the owner's faces+people back. The decisions persist past
// a future full re-cluster because they're recorded on the stable faces.

/// GET /api/people/{person_id}/faces — every face of a person (crop bbox + source
/// dims + confidence) for the studio grid. 404 if unknown. No embeddings.
pub async fn person_faces(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
) -> Result<Json<crate::models::PersonFacesResponse>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    snap.person_faces(&person_id).map(Json).or_404()
}

/// POST /api/people/{person_id}/birthdate — set/clear the date of birth.
pub async fn set_person_birthdate(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::BirthdateBody>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.set_person_birthdate(&person_id, body.birthdate).or_404()?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/cover — pin a face as the cover (locks it).
pub async fn set_person_cover(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::CoverBody>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.set_person_cover(&person_id, &body.face_id).or_404()?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/approve — confirm low-confidence faces belong to
/// this person (they stop being flagged for review and are pinned to the identity).
pub async fn approve_faces(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::FaceIdsBody>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.approve_faces(&person_id, &body.face_ids).or_404()?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/ignore — mark faces as intruders / non-faces.
pub async fn ignore_faces(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::FaceIdsBody>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.ignore_faces(&person_id, &body.face_ids).or_404()?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/move — move faces to another person.
pub async fn move_faces(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::MoveFacesBody>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap
        .move_faces(&person_id, &body.face_ids, &body.to_person_id)
        .ok_or(StatusCode::BAD_REQUEST)?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/merge — merge this person into another.
pub async fn merge_people(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
    Json(body): Json<crate::models::MergeBody>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.merge_people(&person_id, &body.into_person_id).ok_or(StatusCode::BAD_REQUEST)?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

/// POST /api/people/{person_id}/hide — hide a person from the People surface.
pub async fn hide_person(
    State(st): State<Shared>,
    Path(person_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let owner = snap.hide_person(&person_id).or_404()?;
    snap.persist_faces(&owner).await;
    Ok(StatusCode::OK)
}

#[derive(serde::Serialize)]
pub struct UserStorageView {
    pub used_mb: f64,
    pub total_mb: f64,
}

/// GET /api/users/{id}/storage — used vs total storage for the user. Total is the
/// user's quota if set, else the filesystem capacity (or the S3 default quota).
pub async fn get_user_storage(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<UserStorageView>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let (used_mb, total_mb) = st.user_storage(&id);
    Ok(Json(UserStorageView { used_mb, total_mb }))
}

// ---- Vault ----

/// GET /api/users/{id}/vault — status only (configured + count). Never returns
/// photos or the PIN hash.
pub async fn get_vault(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<VaultStatus>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let (configured, count) = match st.vaults.get(&id) {
        Some(v) => (v.pin_hash.is_some(), v.photo_ids.len()),
        None => (false, 0),
    };
    Ok(Json(VaultStatus { configured, count }))
}

/// PUT /api/users/{id}/vault/pin — set or change the PIN. If a PIN is already
/// configured, a correct `current_pin` is required (else 403).
pub async fn set_vault_pin(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<SetPinBody>,
) -> Result<StatusCode, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let mut snap = request_snapshot(&st).await.or_500()?;
    if !snap.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let already_set = snap.vaults.get(&id).map(|v| v.pin_hash.is_some()).unwrap_or(false);
    if already_set {
        let ok = body.current_pin.as_ref().map(|cur| snap.verify_pin(&id, cur)).unwrap_or(false);
        if !ok {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    snap.set_pin(&id, &body.pin);
    let vault = snap.vaults.get(&id).cloned().unwrap_or_default();
    p.upsert_vault(&id, &vault).await.or_500()?;
    Ok(StatusCode::OK)
}

/// POST /api/users/{id}/vault/unlock — verify the PIN and return the contents.
/// 401 on wrong/unset PIN. Does NOT persist any "unlocked" state.
pub async fn unlock_vault(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<UnlockBody>,
) -> Result<Json<VaultContents>, StatusCode> {
    let _p = st.read().await.persistence.clone().or_500()?;
    let snap = request_snapshot(&st).await.or_500()?;
    if !snap.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    // Brute-force lockout state is still per-instance (kept on the shared map).
    let key = format!("vault:{id}");
    {
        let mut g = st.write().await;
        if g.rate_locked(&key) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        if !snap.verify_pin(&id, &body.pin) {
            g.rate_fail(&key);
            return Err(StatusCode::UNAUTHORIZED);
        }
        g.rate_reset(&key);
    }
    Ok(Json(VaultContents { photos: snap.vault_views(&id) }))
}

/// POST /api/users/{id}/vault/photos — move the user's OWN photos into the
/// vault. PIN required (401 else); 400 if any photo is not owned by the user.
pub async fn add_vault_photos(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<VaultPhotosBody>,
) -> Result<Json<VaultCount>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let snap = request_snapshot(&st).await.or_500()?;
    if !snap.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if !snap.verify_pin(&id, &body.pin) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    for pid in &body.photo_ids {
        match snap.photos.get(pid) {
            None => return Err(StatusCode::NOT_FOUND),
            Some(ph) if ph.owner_id != id => return Err(StatusCode::BAD_REQUEST),
            Some(_) => {}
        }
    }
    let mut vault = snap.vaults.get(&id).cloned().unwrap_or_default();
    for pid in body.photo_ids {
        if !vault.photo_ids.contains(&pid) {
            vault.photo_ids.push(pid);
        }
    }
    let count = vault.photo_ids.len();
    p.upsert_vault(&id, &vault).await.or_500()?;
    Ok(Json(VaultCount { count }))
}

/// DELETE /api/users/{id}/vault/photos — remove photos from the vault. PIN
/// required (401 else). Returns the remaining count.
pub async fn remove_vault_photos(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<VaultPhotosBody>,
) -> Result<Json<VaultCount>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    let snap = request_snapshot(&st).await.or_500()?;
    if !snap.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if !snap.verify_pin(&id, &body.pin) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut vault = snap.vaults.get(&id).cloned().unwrap_or_default();
    vault.photo_ids.retain(|pid| !body.photo_ids.contains(pid));
    let count = vault.photo_ids.len();
    p.upsert_vault(&id, &vault).await.or_500()?;
    Ok(Json(VaultCount { count }))
}

// ---- Timeline prefs ----

pub async fn get_prefs(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<TimelinePrefs>, StatusCode> {
    // Postgres-first: read prefs from a fresh DB snapshot.
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let prefs = st.prefs.get(&id).cloned().unwrap_or_default();
    Ok(Json(prefs))
}

pub async fn update_prefs(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<UpdatePrefs>,
) -> Result<Json<TimelinePrefs>, StatusCode> {
    // Postgres-first: mutate prefs on a DB snapshot; persist_prefs writes back.
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let prefs = st.prefs.entry(id.clone()).or_default();
    if let Some(show_shared) = body.show_shared {
        prefs.show_shared = show_shared;
    }
    if let Some(per_album) = body.per_album {
        prefs.per_album = per_album;
    }
    let out = prefs.clone();
    st.persist_prefs(&id).await;
    Ok(Json(out))
}

// ---- Timeline ----

pub async fn get_timeline(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Timeline>, StatusCode> {
    // Postgres-first: run the timeline logic against a fresh DB snapshot.
    let snap = request_snapshot(&st).await.or_500()?;
    let st: &AppState = &snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let photos = st.timeline_photos(&id);

    // Group into date sections by calendar day (date portion of taken_at).
    // photos are already sorted newest-first.
    let mut sections: Vec<TimelineSection> = Vec::new();
    for p in photos {
        let view = p.effective();
        let date = view.taken_at.get(0..10).unwrap_or("").to_string();
        match sections.last_mut() {
            Some(sec) if sec.date == date => sec.items.push(view),
            _ => sections.push(TimelineSection {
                label: date.clone(),
                date,
                items: vec![view],
            }),
        }
    }

    Ok(Json(Timeline { sections }))
}

// ---- Storage settings ----

/// GET /api/storage — current settings with S3 secrets REDACTED.
pub async fn get_storage(State(st): State<Shared>) -> Json<StorageSettings> {
    let pool = { st.read().await.persistence.clone() };
    let storage = match &pool {
        Some(p) => p.load_storage().await.ok().flatten().unwrap_or_default(),
        None => st.read().await.storage.clone(),
    };
    Json(storage.redacted())
}

/// PUT /api/storage — update mode / primary_s3 / backup / trash_retention_days.
/// Accepts secrets on input; returns the REDACTED settings. An incoming S3
/// secret equal to the redaction sentinel keeps the previously stored secret
/// (so the UI can round-trip a redacted GET without wiping the secret).
pub async fn update_storage(
    State(st): State<Shared>,
    Json(body): Json<UpdateStorage>,
) -> Json<StorageSettings> {
    let mut snap_opt = if st.read().await.persistence.is_some() {
        request_snapshot(&st).await
    } else {
        None
    };
    let mut guard;
    let st: &mut AppState = match snap_opt.as_mut() {
        Some(s) => s,
        None => {
            guard = st.write().await;
            &mut guard
        }
    };

    if let Some(mode) = body.mode {
        st.storage.mode = mode;
    }
    if let Some(mut s3) = body.primary_s3 {
        preserve_secret(&mut s3.secret_access_key, st.storage.primary_s3.as_ref());
        st.storage.primary_s3 = Some(s3);
    }
    if let Some(mut backup) = body.backup {
        if let Some(s3) = backup.s3.as_mut() {
            preserve_secret(&mut s3.secret_access_key, st.storage.backup.s3.as_ref());
        }
        st.storage.backup = backup;
    }
    if let Some(days) = body.trash_retention_days {
        st.storage.trash_retention_days = days;
    }

    let redacted = st.storage.redacted();
    st.persist_storage().await;
    Json(redacted)
}

/// App-wide settings exposed to the admin UI: the Gravatar flag + the feature
/// toggles (ML + security/media).
#[derive(serde::Serialize)]
pub struct AppSettingsView {
    pub gravatar_enabled: bool,
    pub features: crate::models::FeatureFlags,
}

impl AppSettingsView {
    fn of(s: &StorageSettings) -> Self {
        Self { gravatar_enabled: s.gravatar_enabled, features: s.features.clone() }
    }
}

/// The patchable settings document (RFC 6902 operates on this shape):
/// `{ gravatar_enabled, features: { faces, clip, ocr, geocode, transcode,
/// public_signup, public_links, require_2fa } }`.
fn settings_doc(s: &StorageSettings) -> serde_json::Value {
    serde_json::json!({
        "gravatar_enabled": s.gravatar_enabled,
        "features": s.features,
    })
}

/// Apply a patched settings doc back onto `s` (typed, so a bad value is rejected
/// by the caller via the returned Option).
fn apply_settings_doc(s: &mut StorageSettings, doc: &serde_json::Value) -> Option<()> {
    s.gravatar_enabled = doc.get("gravatar_enabled")?.as_bool()?;
    s.features = serde_json::from_value(doc.get("features")?.clone()).ok()?;
    Some(())
}

/// GET /api/settings — app-wide settings (admin only via path_authz).
pub async fn get_settings(State(st): State<Shared>) -> Json<AppSettingsView> {
    let pool = { st.read().await.persistence.clone() };
    let settings = match &pool {
        Some(p) => p.load_storage().await.ok().flatten().unwrap_or_default(),
        None => st.read().await.storage.clone(),
    };
    Json(AppSettingsView::of(&settings))
}

/// PATCH /api/settings — RFC 6902 patch over the settings doc (gravatar + feature
/// toggles). Admin only. 422 on a bad patch or a wrongly-typed value.
pub async fn patch_settings(
    State(st): State<Shared>,
    Json(ops): Json<json_patch::Patch>,
) -> Result<Json<AppSettingsView>, StatusCode> {
    let p = st.read().await.persistence.clone().or_500()?;
    // Postgres-first, ONE transaction over the settings singleton row.
    use sqlx::Row as _;
    let mut tx = p.begin().await.or_500()?;
    let row = sqlx::query("SELECT settings FROM storage_settings WHERE id = 1 FOR UPDATE")
        .fetch_optional(&mut *tx)
        .await
        .or_500()?;
    let mut settings: StorageSettings = row
        .map(|r| serde_json::from_value(r.get("settings")).unwrap_or_default())
        .unwrap_or_default();
    let mut doc = settings_doc(&settings);
    json_patch::patch(&mut doc, &ops).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    apply_settings_doc(&mut settings, &doc).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    sqlx::query(
        "INSERT INTO storage_settings (id, settings) VALUES (1, $1) ON CONFLICT (id) DO UPDATE SET settings = $1",
    )
    .bind(serde_json::to_value(&settings).unwrap_or_default())
    .execute(&mut *tx)
    .await
    .or_500()?;
    tx.commit().await.or_500()?;
    Ok(Json(AppSettingsView::of(&settings)))
}

/// If `incoming` is the redaction sentinel, replace it with the existing
/// stored secret (if any) so a redacted GET can be re-PUT without data loss.
fn preserve_secret(incoming: &mut String, existing: Option<&crate::models::S3Config>) {
    if incoming == REDACTED_SECRET {
        *incoming = existing
            .map(|c| c.secret_access_key.clone())
            .unwrap_or_default();
    }
}

#[derive(serde::Serialize)]
pub struct BackupRunResult {
    pub count: u64,
    pub last_backup_at: Option<String>,
}

/// POST /api/storage/backup/run — trigger a backup pass now.
pub async fn run_backup_now(
    State(st): State<Shared>,
) -> Result<Json<BackupRunResult>, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    let count = st.run_backup().await.map_err(|e| {
        tracing::warn!("manual backup run failed: {e}");
        StatusCode::BAD_GATEWAY
    })?;
    let last_backup_at = st.storage.backup.last_backup_at.clone();
    // Write-through: backup flips `backed_up` on photos + updates storage stats.
    if count > 0 && st.is_persistent() {
        st.persist_storage().await;
        let ids: Vec<String> = st
            .photos
            .values()
            .filter(|p| p.backed_up)
            .map(|p| p.id.clone())
            .collect();
        for id in &ids {
            st.persist_photo(id).await;
        }
    }
    Ok(Json(BackupRunResult {
        count,
        last_backup_at,
    }))
}

// ---- Transcoding: device-aware render plan + real image transcode ----

/// Query params for GET /api/photos/{id}/render and POST /api/transcode/image.
#[derive(serde::Deserialize)]
pub struct RenderQuery {
    #[serde(default)]
    pub w: Option<u32>,
    #[serde(default)]
    pub h: Option<u32>,
    #[serde(default)]
    pub fmt: Option<MediaFormat>,
    #[serde(default)]
    pub supports: Option<String>,
}

/// The negotiated render descriptor returned by GET /api/photos/{id}/render.
#[derive(serde::Serialize)]
pub struct RenderPlan {
    #[serde(flatten)]
    pub plan: TranscodePlan,
    pub mime: String,
    /// Stable cache key, e.g. "ph_0001_800x533.webp".
    pub cache_key: String,
}

fn accept_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// GET /api/photos/{id}/render-plan — negotiate the best FORMAT + RESOLUTION for
/// the requesting device and return the plan as JSON (a descriptor, no bytes).
///
/// The source `MediaFormat` is derived from the photo's filename extension
/// (falling back to its `kind`), and its dimensions come from EXIF. A
/// [`DeviceProfile`] is built from the `?supports=`/`?fmt=`/`?w=`/`?h=` query
/// params and the `Accept` header, then [`negotiate`] runs.
///
/// The actual byte-producing render is `GET /api/photos/{id}/render`
/// ([`render_photo`]); this descriptor endpoint is kept for clients that want the
/// negotiated plan (cache key / chosen format+dims) without fetching pixels.
pub async fn render_plan(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<RenderQuery>,
    headers: HeaderMap,
) -> Result<Json<RenderPlan>, StatusCode> {
    let pool = st.read().await.persistence.clone().or_500()?;
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;
    let photo = &photo;

    // Source format: prefer the filename extension, else map from `kind`.
    let ext = photo.filename.rsplit('.').next().unwrap_or("");
    let source = MediaFormat::from_ext(ext).unwrap_or(match photo.kind.as_str() {
        "video" => MediaFormat::Mp4,
        _ => MediaFormat::Jpeg,
    });
    let (src_w, src_h) = (photo.exif.width.max(1), photo.exif.height.max(1));

    let device = DeviceProfile::from_request(
        accept_header(&headers).as_deref(),
        q.supports.as_deref(),
        q.fmt,
        q.w,
        q.h,
    );
    let plan = negotiate(source, src_w, src_h, &device);
    let cache_key = format!(
        "{}_{}x{}.{}",
        id,
        plan.width,
        plan.height,
        plan.format.ext()
    );
    let mime = plan.format.mime().to_string();
    Ok(Json(RenderPlan {
        plan,
        mime,
        cache_key,
    }))
}

/// POST /api/transcode/image — accepts a raw image body (bytes) plus `?w=&h=&fmt=`
/// and ACTUALLY transcodes it via [`RealTranscoder`], returning the transcoded
/// bytes with the correct `Content-Type`. Proves the engine end-to-end.
pub async fn transcode_image(
    Query(q): Query<RenderQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, StatusCode> {
    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Target format defaults to webp; dims default to a large bound so an
    // absent w/h means "keep source size".
    let target = q.fmt.unwrap_or(MediaFormat::Webp);
    if !target.is_image() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Plan: we don't know the source dims here; let the encoder thumbnail to the
    // requested bound (u32::MAX = unbounded -> no downscale).
    let plan = TranscodePlan {
        format: target,
        width: q.w.unwrap_or(u32::MAX),
        height: q.h.unwrap_or(u32::MAX),
        source_format: target,
        needs_transcode: true,
    };
    let out = RealTranscoder
        .transcode_image(&body, &plan)
        .map_err(|e| {
            tracing::warn!("image transcode failed: {e}");
            StatusCode::UNPROCESSABLE_ENTITY
        })?;
    Ok(([(header::CONTENT_TYPE, target.mime())], out))
}

/// Largest edge (px) the render endpoint will serve when no `w`/`h` is given, so
/// an unbounded request can't decode+re-encode an arbitrarily huge original.
const RENDER_MAX_EDGE: u32 = 4000;

/// GET /api/photos/{id}/original — return the stored ORIGINAL upload bytes with
/// their content type. 404 when no original is stored for the photo (e.g. the
/// demo seed). Bytes come from the in-memory originals store (the demo
/// convenience mirror of the `StorageBackend` blob at `originals/{id}.{ext}`).
pub async fn get_original(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let (pool, cfg) = {
        let g = st.read().await;
        (g.persistence.clone().or_500()?, g.storage_ctx())
    };
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;
    match cfg.load_original_blob(&photo).await {
        Some((bytes, ct)) => Ok(([(header::CONTENT_TYPE, ct)], bytes)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// GET /api/photos/{id}/companions/{ext}/download — download a kept companion
/// file (e.g. the RAW/.ARW sidecar) of a photo. `ext` is matched
/// case-insensitively. Returns the stored bytes with the companion's MIME type
/// and a `Content-Disposition: attachment; filename="<original filename>"`
/// header so a browser saves it under its real name. 404 if no such companion
/// was kept. The in-memory map is a demo convenience; the StorageBackend
/// (`companions/{id}.{ext}`) is the authoritative store.
pub async fn download_companion(
    State(st): State<Shared>,
    Path((id, ext)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let (pool, cfg) = {
        let g = st.read().await;
        (g.persistence.clone().or_500()?, g.storage_ctx())
    };
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;
    let (bytes, filename, mime) = cfg
        .load_companion_blob(&photo, &ext)
        .await
        .or_404()?;
    let disposition = format!("attachment; filename=\"{filename}\"");
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    ))
}

/// One detected face for the per-photo overlay. The biometric embedding is NEVER
/// exposed. Person identity (`person_id`/name/label) is included ONLY for the
/// photo's OWNER — clusters + names are the owner's private data; other viewers
/// who can see the photo get the boxes only.
#[derive(serde::Serialize)]
pub struct PhotoFace {
    pub id: String,
    /// `[x, y, w, h]` in SOURCE-image pixels (scale by `source_width/height`).
    pub bbox: [f32; 4],
    pub score: f32,
    pub person_id: Option<String>,
    pub person_name: Option<String>,
    /// Stable per-cluster label ("Person N") so unnamed people are still
    /// distinguishable — fulfills "who is who, even without names".
    pub person_label: Option<String>,
}

#[derive(serde::Serialize)]
pub struct PhotoFacesResponse {
    pub source_width: u32,
    pub source_height: u32,
    pub faces: Vec<PhotoFace>,
}

/// A stable human label for an (unnamed) cluster from its id: `person_7` → `Person 7`.
fn person_label(person_id: &str) -> String {
    let n = person_id.rsplit('_').next().unwrap_or(person_id);
    format!("Person {n}")
}

/// GET /api/photos/{id}/faces — bounding boxes of every detected face in the
/// photo, each tagged (for the OWNER) with its Person cluster: the assigned name
/// if any, else a stable per-cluster label. `source_width`/`source_height` let the
/// client scale boxes onto the displayed image. Read access is the standard
/// photo-read authz (owner, or a valid grant for a live photo).
pub async fn photo_faces(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<PhotoFacesResponse>, StatusCode> {
    let pool = st.read().await.persistence.clone().or_500()?;
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;
    let is_owner = photo.owner_id == actor.0;
    let faces = pool
        .faces_for_photo(&id)
        .await
        .or_500()?;

    // Resolve cluster names for the owner only (small distinct set of person ids).
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if is_owner {
        let mut seen = std::collections::HashSet::new();
        for f in &faces {
            if let Some(pid) = &f.person_id {
                if seen.insert(pid.clone()) {
                    if let Ok(Some(p)) = pool.get_person(pid).await {
                        if let Some(n) = p.name {
                            names.insert(pid.clone(), n);
                        }
                    }
                }
            }
        }
    }

    let faces = faces
        .into_iter()
        .map(|f| {
            let person_id = if is_owner { f.person_id.clone() } else { None };
            let person_name = person_id.as_ref().and_then(|p| names.get(p).cloned());
            let person_label = person_id.as_ref().map(|p| person_label(p));
            PhotoFace { id: f.id, bbox: f.bbox, score: f.score, person_id, person_name, person_label }
        })
        .collect();

    Ok(Json(PhotoFacesResponse {
        source_width: photo.exif.width,
        source_height: photo.exif.height,
        faces,
    }))
}

/// GET /api/photos/{id}/render?w=&h=&fmt= — load the photo's ORIGINAL, decode it,
/// resize to FIT within `w`x`h` while preserving aspect ratio (NEVER upscaling
/// beyond the original size), encode, and return the bytes with the right
/// `Content-Type`. This is the screen-adapted image the lightbox requests.
///
/// - `fmt`: target format; honored when given (`MediaFormat::from_ext`-style,
///   e.g. `?fmt=jpeg|webp|png`). Defaults to a web-friendly format derived from
///   the original (webp/jpeg/png; non-encodable or AVIF originals fall back to
///   webp).
/// - `w`/`h`: max box. Either may be omitted (the missing bound is unbounded).
///   When BOTH are omitted the original is returned clamped to a sane max edge
///   ([`RENDER_MAX_EDGE`]).
///
/// 404 when no original is stored for the photo; 422 if the original can't be
/// decoded/encoded.
pub async fn render_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<RenderQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    // Fetch the single photo row, then load its original bytes + content type,
    // then release the lock before doing the (CPU-bound) decode/resize/encode.
    let (bytes, ct, is_video, features) = {
        let (pool, cfg) = {
            let g = st.read().await;
            (g.persistence.clone().or_500()?, g.storage_ctx())
        };
        let photo = pool
            .get_photo(&id)
            .await
            .or_500()?
            .or_404()?;
        // A photo is a VIDEO when its `kind` says so OR its source format is a
        // video container (filename/mime). Image rendering is unaffected below.
        let src_ext = photo.filename.rsplit('.').next().unwrap_or("");
        let is_video = photo.kind == "video"
            || MediaFormat::from_ext(src_ext).map(|f| f.is_video()).unwrap_or(false);
        let features = pool.load_storage().await.ok().flatten().unwrap_or_default().features;
        // Prefer the plugin-EDITED version when one exists (the original stays
        // available via GET /original); fall back to the original otherwise.
        let (bytes, ct) = cfg.load_display_blob(&photo).await.or_404()?;
        (bytes, ct, is_video, features)
    };

    // VIDEO TRANSCODING GATE (`features.transcode`): video originals go through
    // the ffmpeg path, which is DISABLED when the flag is off. We return 403 BEFORE
    // touching ffmpeg, so a flag-disabled 403 is always distinguishable from a
    // "ffmpeg missing" 5xx (which only happens when the flag is ON).
    if is_video {
        if !features.transcode {
            tracing::info!("video render of {id} blocked: features.transcode is disabled");
            return Err(StatusCode::FORBIDDEN);
        }
        return render_video(&id, &ct, &bytes, &q).await;
    }

    // Choose the target format: an explicit `?fmt=` wins; else keep a
    // web-friendly format based on the original's content type (webp/jpeg/png),
    // mapping anything non-encodable (or AVIF) down to webp.
    let source = MediaFormat::from_mime(&ct);
    let target = match q.fmt {
        Some(f) if f.is_image() => f,
        _ => match source {
            Some(MediaFormat::Jpeg) => MediaFormat::Jpeg,
            Some(MediaFormat::Png) => MediaFormat::Png,
            _ => MediaFormat::Webp,
        },
    };
    if !target.is_image() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Resize box. Default each missing bound to the sane max edge, THEN clamp the
    // box to the original's own pixel size so we FIT-within without ever upscaling
    // beyond the original. Read the dimensions from the JPEG/PNG HEADER ONLY
    // (`into_dimensions`) — do NOT fully decode here, or we'd decode the (possibly
    // 20 MP) original TWICE (once for dims, once in the transcoder). On failure,
    // fall back to the max edge; the transcoder will still error out cleanly.
    let (orig_w, orig_h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((RENDER_MAX_EDGE, RENDER_MAX_EDGE));
    let width = q.w.unwrap_or(RENDER_MAX_EDGE).min(orig_w).max(1);
    let height = q.h.unwrap_or(RENDER_MAX_EDGE).min(orig_h).max(1);
    let plan = TranscodePlan {
        format: target,
        width,
        height,
        source_format: source.unwrap_or(MediaFormat::Jpeg),
        needs_transcode: true,
    };
    // Decode/resize/encode is CPU-bound — run it on the blocking pool so it never
    // stalls the async runtime (and concurrent renders use multiple cores).
    let out = tokio::task::spawn_blocking(move || RealTranscoder.transcode_image(&bytes, &plan))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|e| {
            tracing::warn!("render of {id} failed: {e}");
            StatusCode::UNPROCESSABLE_ENTITY
        })?;
    Ok(([(header::CONTENT_TYPE, target.mime().to_string())], out))
}

/// Transcode a VIDEO original to a streamable rendition via the ffmpeg-backed
/// [`RealTranscoder::transcode_video`]. Only reached from [`render_photo`] AFTER
/// the `features.transcode` flag has been confirmed ON (a disabled flag returns
/// 403 there, never here). The original bytes are written to a temp file (ffmpeg
/// reads a path, not stdin), then transcoded to an MP4 box. Errors map so the
/// caller can tell apart the toolchain being absent (503) from a genuine encode
/// failure (422):
/// - [`TranscodeError::VideoToolMissing`] → `503 SERVICE_UNAVAILABLE`.
/// - any other transcode error → `422 UNPROCESSABLE_ENTITY`.
async fn render_video(
    id: &str,
    ct: &str,
    bytes: &[u8],
    q: &RenderQuery,
) -> Result<([(header::HeaderName, String); 1], Vec<u8>), StatusCode> {
    use crate::transcode::TranscodeError;
    // Target a web-friendly MP4 container by default; honor an explicit video
    // `?fmt=`. (Image targets are nonsensical for a video source.)
    let target = match q.fmt {
        Some(f) if f.is_video() => f,
        _ => MediaFormat::Mp4,
    };
    let source = MediaFormat::from_mime(ct).unwrap_or(MediaFormat::Mp4);
    let width = q.w.unwrap_or(RENDER_MAX_EDGE).max(1);
    let height = q.h.unwrap_or(RENDER_MAX_EDGE).max(1);
    let plan = TranscodePlan {
        format: target,
        width,
        height,
        source_format: source,
        needs_transcode: true,
    };

    // ffmpeg reads from a path; stage the original to a unique temp file.
    let tmp = std::env::temp_dir().join(format!(
        "photon-video-{}-{}.in",
        std::process::id(),
        id.replace('/', "_")
    ));
    let tmp_for_task = tmp.clone();
    let bytes = bytes.to_vec();
    let id_owned = id.to_string();
    let result = tokio::task::spawn_blocking(move || {
        std::fs::write(&tmp_for_task, &bytes)
            .map_err(|e| TranscodeError::Io(e.to_string()))?;
        let out = RealTranscoder.transcode_video(&tmp_for_task.to_string_lossy(), &plan);
        let _ = std::fs::remove_file(&tmp_for_task);
        out
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match result {
        Ok(out) => Ok(([(header::CONTENT_TYPE, target.mime().to_string())], out)),
        Err(TranscodeError::VideoToolMissing) => {
            tracing::warn!("video render of {id_owned}: ffmpeg not on PATH");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(e) => {
            tracing::warn!("video render of {id_owned} failed: {e}");
            Err(StatusCode::UNPROCESSABLE_ENTITY)
        }
    }
}

// ---- SMTP config ----

/// GET /api/smtp — current SMTP config with the password REDACTED. Returns an
/// empty/default config when none is set yet.
pub async fn get_smtp(State(st): State<Shared>) -> Json<SmtpConfig> {
    let pool = { st.read().await.persistence.clone() };
    let smtp = match &pool {
        Some(p) => p.load_smtp().await.ok().flatten(),
        None => st.read().await.smtp.clone(),
    };
    Json(smtp.map(|c| c.redacted()).unwrap_or_default())
}

/// PUT /api/smtp — set the SMTP config. A password equal to the redaction
/// sentinel (or empty) preserves the previously stored password.
pub async fn update_smtp(
    State(st): State<Shared>,
    Json(body): Json<UpdateSmtp>,
) -> Json<SmtpConfig> {
    let mut snap_opt = if st.read().await.persistence.is_some() {
        request_snapshot(&st).await
    } else {
        None
    };
    let mut guard;
    let st: &mut AppState = match snap_opt.as_mut() {
        Some(s) => s,
        None => {
            guard = st.write().await;
            &mut guard
        }
    };
    let mut password = body.password;
    if password == REDACTED_SECRET || password.is_empty() {
        if let Some(existing) = &st.smtp {
            password = existing.password.clone();
        } else {
            password = String::new();
        }
    }
    let cfg = SmtpConfig {
        host: body.host,
        port: body.port,
        username: body.username,
        password,
        from: body.from,
        tls: body.tls,
    };
    let redacted = cfg.redacted();
    st.smtp = Some(cfg);
    st.persist_smtp().await;
    Json(redacted)
}

// ---- Invites ----

/// POST /api/invites — create an invite with a generated token, email it via the
/// mailer, and return the invite (token included).
pub async fn create_invite(
    State(st): State<Shared>,
    Json(body): Json<CreateInvite>,
) -> Result<(StatusCode, Json<Invite>), StatusCode> {
    let (invite, mailer) = {
        let mut snap = request_snapshot(&st).await.or_500()?;
        let st: &mut AppState = &mut snap;
        if !st.users.contains_key(&body.inviter_id) {
            return Err(StatusCode::NOT_FOUND);
        }
        let token = st.new_invite_token();
        let invite = Invite {
            token: token.clone(),
            email: body.email,
            inviter_id: body.inviter_id,
            created_at: now_rfc3339(),
            accepted: false,
        };
        st.invites.insert(token.clone(), invite.clone());
        st.persist_invite(&token).await;
        (invite, st.mailer())
    };

    let subject = "You're invited to Photon".to_string();
    let message = format!(
        "You have been invited to Photon. Use this token to accept: {}",
        invite.token
    );
    if let Err(e) = mailer.send(&invite.email, &subject, &message).await {
        tracing::warn!("invite email to {} failed: {e}", invite.email);
    }

    Ok((StatusCode::CREATED, Json(invite)))
}

/// GET /api/invites — list all invites (tokens included).
pub async fn list_invites(State(st): State<Shared>) -> Json<Vec<Invite>> {
    let pool = { st.read().await.persistence.clone() };
    let mut invites: Vec<Invite> = match &pool {
        Some(p) => p.load_invites().await.unwrap_or_default(),
        None => st.read().await.invites.values().cloned().collect(),
    };
    invites.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Json(invites)
}

/// POST /api/invites/accept — mark the invite accepted and create a User with
/// the invited email. 404 on an unknown token, 409 if already accepted (replay),
/// 410 if expired (older than [`INVITE_TTL_SECS`]).
pub async fn accept_invite(
    State(st): State<Shared>,
    Json(body): Json<AcceptInvite>,
) -> Result<Json<User>, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    let email = {
        let invite = st.invites.get(&body.token).or_404()?;
        // Reject replay: an already-accepted invite can't be reused.
        if invite.accepted {
            return Err(StatusCode::CONFLICT);
        }
        // Reject expired invites (created_at older than the TTL).
        if is_expired(&invite.created_at, INVITE_TTL_SECS) {
            return Err(StatusCode::GONE);
        }
        let email = invite.email.clone();
        let invite = st.invites.get_mut(&body.token).expect("invite present");
        invite.accepted = true;
        email
    };
    let n = st.next_id("usr");
    // next_id yields "usr_<n>"; use it directly as the user id.
    let user = User {
        id: n.clone(),
        name: body.name,
        email,
        avatar_url: String::new(),
        // Invited users have no password yet; they set one via reset/accept.
        password_hash: None,
        salt: String::new(),
        pepper: String::new(),
        is_admin: false,
        disabled: false,
        quota_mb: None,
        partners: Vec::new(),
        totp_secret: None,
    };
    st.users.insert(n.clone(), user.clone());
    st.persist_invite(&body.token).await;
    st.persist_user(&n).await;
    Ok(Json(user))
}

// ---- Admin stats (Feature 3) ----

#[derive(serde::Serialize)]
pub struct StatsCounts {
    /// Live (non-trashed, non-archived... see note) photo count. Here `photos`
    /// counts total LIVE non-trashed photos (archived included, vault included);
    /// `trashed` and `archived` are reported separately for clarity.
    pub photos: usize,
    pub albums: usize,
    pub users: usize,
    pub groups: usize,
    pub trashed: usize,
    pub archived: usize,
    pub vault: usize,
}

#[derive(serde::Serialize)]
pub struct StatsStorage {
    pub mode: crate::models::StorageMode,
    /// Estimated local disk usage in MB = sum of live photo source sizes. The
    /// in-memory demo has no real bytes, so we use a nominal per-photo estimate.
    pub disk_used_mb: u64,
    /// Estimated S3 usage in MB from backed-up photos when an S3 config exists,
    /// else 0.
    pub s3_used_mb: u64,
    /// Configured quota in MB (constant).
    pub quota_mb: u64,
}

/// Live host metrics for the Overview health gauges (real, via `sysinfo`).
#[derive(serde::Serialize)]
pub struct StatsSystem {
    /// Overall CPU utilization, 0-100.
    pub cpu_percent: u32,
    /// Used RAM as a percentage of total, 0-100.
    pub mem_percent: u32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    /// Host uptime in seconds.
    pub uptime_secs: u64,
    /// Number of logical CPUs.
    pub cpus: usize,
}

/// Sample host CPU + memory + uptime. CPU needs two refreshes spaced by at least
/// `MINIMUM_CPU_UPDATE_INTERVAL`, so we sample, wait, and re-sample.
async fn sample_system() -> StatsSystem {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_cpu_usage();
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let cpu = sys.global_cpu_usage().round().clamp(0.0, 100.0) as u32;
    let total = sys.total_memory(); // bytes
    let used = sys.used_memory();
    let mem_percent = if total > 0 { ((used as f64 / total as f64) * 100.0).round() as u32 } else { 0 };
    StatsSystem {
        cpu_percent: cpu,
        mem_percent,
        mem_used_mb: used / (1024 * 1024),
        mem_total_mb: total / (1024 * 1024),
        uptime_secs: System::uptime(),
        cpus: sys.cpus().len(),
    }
}

#[derive(serde::Serialize)]
pub struct AdminStats {
    pub jobs: Vec<JobStats>,
    pub counts: StatsCounts,
    pub storage: StatsStorage,
    pub system: StatsSystem,
    /// Recent background-job runs (newest first) for the "Run history" view.
    pub history: Vec<crate::models::JobRun>,
}

/// POST /api/admin/jobs/{name}/run — trigger a background job on demand (admin
/// only via `path_authz`). Runs synchronously and returns the recorded run.
/// Names: trash_purge, s3_backup, ai_analysis, duplicates, rebuild_thumbnails,
/// recluster_faces, reextract_metadata.
/// Whether `name` is a runnable job: either a built-in ([`crate::jobs::is_job`])
/// or a job owned by a registered subprocess plugin. Used to gate the on-demand
/// run endpoint so plugin jobs are accepted too.
pub async fn job_exists(st: &Shared, name: &str) -> bool {
    if crate::jobs::is_job(name) {
        return true;
    }
    let host = st.read().await.plugins.clone();
    match host {
        Some(h) => h.has_job(name).await,
        None => false,
    }
}

pub async fn run_job(
    State(st): State<Shared>,
    Path(name): Path<String>,
) -> Result<Json<crate::models::JobRun>, StatusCode> {
    if !job_exists(&st, &name).await {
        return Err(StatusCode::NOT_FOUND);
    }
    // Prefer DURABLE execution: enqueue and return immediately, so a heavy pass
    // (e.g. `reset_faces` re-detecting the whole library, minutes of work) doesn't
    // block the request or get cancelled by a client timeout. The worker claims it
    // once across the cluster and records the real JobRun in history when it
    // finishes — the admin console's next stats poll shows the outcome. Falls back
    // to inline execution when there's no queue (offline/tests), preserving the
    // synchronous result there.
    let utils = st.read().await.worker_utils.clone();
    if let Some(u) = utils {
        match u
            .add_job(
                crate::jobs::MaintenanceJob { job: name.clone() },
                graphile_worker::JobSpec::default(),
            )
            .await
        {
            Ok(_) => {
                st.write().await.job_running(&name);
                return Ok(Json(crate::models::JobRun {
                    name,
                    outcome: "queued".to_string(),
                    items: 0,
                    started_at: crate::state::now_rfc3339(),
                    duration_ms: 0,
                    trigger: "manual".to_string(),
                }));
            }
            Err(e) => tracing::warn!("could not enqueue job {name} durably, running inline: {e}"),
        }
    }
    match crate::jobs::run_named(&st, &name, "manual").await {
        Some(run) => Ok(Json(run)),
        None => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// GET /api/plugins — the route-capable plugins (id, label, UI entry path) for the
/// UI's tools list. Empty when plugins are disabled. Any authenticated user.
pub async fn list_route_plugins(
    State(st): State<Shared>,
) -> Json<Vec<crate::plugins::RoutePluginInfo>> {
    let host = st.read().await.plugins.clone();
    match host {
        Some(h) => Json(h.route_plugins().await),
        None => Json(vec![]),
    }
}

/// ANY /api/plugins/{name}/{*rest} — catch-all proxy to a Route plugin over
/// gRPC. The `auth_middleware` already authenticated the caller and injected
/// `AuthUser`; `path_authz` lets `/api/plugins/..` through to any signed-in user
/// (per-plugin authz is the plugin's job, helped by the forwarded `is_admin`).
///
/// Resolves the plugin host, forwards the request (filling `actor`/`is_admin`),
/// and maps the plugin's response back to axum. Unknown/non-route plugin → 404;
/// plugin error/timeout/crash → 502 (the server stays up).
pub async fn plugin_proxy(
    State(st): State<Shared>,
    Extension(actor): Extension<AuthUser>,
    Path((name, rest)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, StatusCode> {
    use photon_plugin_proto::pb;

    let host = st.read().await.plugins.clone();
    let host = host.or_404()?;
    if !host.has_route(&name).await {
        return Err(StatusCode::NOT_FOUND);
    }

    // Resolve the actor's admin flag with a targeted lookup (like `get_import`),
    // so the plugin can make its own authorization decisions.
    let is_admin = {
        let p = st.read().await.persistence.clone();
        match p {
            Some(p) => p
                .get_user(&actor.0)
                .await
                .ok()
                .flatten()
                .map(|u| u.is_admin)
                .unwrap_or(false),
            None => false,
        }
    };

    // Copy request headers into the proto map, skipping non-UTF8 values.
    let mut hdrs = std::collections::HashMap::new();
    for (k, v) in headers.iter() {
        if let Ok(val) = v.to_str() {
            hdrs.insert(k.as_str().to_string(), val.to_string());
        }
    }

    let req = pb::HttpRequest {
        method: method.to_string(),
        path: format!("/{rest}"),
        query: uri.query().unwrap_or("").to_string(),
        headers: hdrs,
        body: body.to_vec(),
        actor: actor.0.clone(),
        is_admin,
    };

    match host.route_handle(&name, req).await {
        Some(resp) => {
            // Default to 200 if the plugin left status 0 / out of range.
            let code = u16::try_from(resp.status)
                .ok()
                .and_then(|c| StatusCode::from_u16(c).ok())
                .unwrap_or(StatusCode::OK);
            let mut out = axum::response::Response::builder().status(code);
            for (k, v) in resp.headers.iter() {
                if let (Ok(name), Ok(val)) = (
                    axum::http::HeaderName::from_bytes(k.as_bytes()),
                    axum::http::HeaderValue::from_str(v),
                ) {
                    out = out.header(name, val);
                }
            }
            out.body(axum::body::Body::from(resp.body))
                .map_err(|_| StatusCode::BAD_GATEWAY)
        }
        None => Err(StatusCode::BAD_GATEWAY),
    }
}

/// GET /api/plugins/editor/ops — the combined catalog of editor operations across
/// all Editor-capable plugins (empty when plugins are disabled). Any signed-in
/// user may read it (it's just a UI catalog).
pub async fn plugin_editor_ops(
    State(st): State<Shared>,
) -> Json<Vec<crate::plugins::EditorOpInfo>> {
    let host = st.read().await.plugins.clone();
    let ops = match host {
        Some(h) => h.editor_ops().await,
        None => vec![],
    };
    Json(ops)
}

/// Query flags for [`apply_plugin_edit`].
#[derive(serde::Deserialize, Default)]
pub struct PluginEditQuery {
    /// When true, PERSIST the result as the photo's edited version (a companion),
    /// regenerate its thumbnail, and prefer it for display. Otherwise preview-only.
    #[serde(default)]
    pub save: bool,
}

/// POST /api/photos/{id}/plugin-edit/{plugin}/{op}[?save=true] — always edits from
/// the UNTOUCHED ORIGINAL, hands its bytes to the Editor `plugin`'s `op` (JSON body
/// = params), and returns the edited image bytes. With `?save=true` it also
/// persists the result as the reserved `edited` companion (original kept,
/// re-editing overwrites), regenerates the thumbnail, and the edit becomes the
/// preferred display everywhere. Owner-only (central `/api/photos/{id}/..` authz).
/// 404 if plugins are off / no original; 502 if the plugin errors/times out.
pub async fn apply_plugin_edit(
    State(st): State<Shared>,
    Path((id, plugin, op)): Path<(String, String, String)>,
    Query(q): Query<PluginEditQuery>,
    params: Option<Json<std::collections::HashMap<String, String>>>,
) -> Result<impl IntoResponse, StatusCode> {
    let (host, pool, cfg) = {
        let g = st.read().await;
        (
            g.plugins.clone().or_404()?,
            g.persistence.clone().or_500()?,
            g.storage_ctx(),
        )
    };

    // Always edit from the ORIGINAL (non-destructive; re-edit cleanly overwrites).
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;
    let (bytes, ct) = cfg.load_original_blob(&photo).await.or_404()?;

    let params = params.map(|Json(p)| p).unwrap_or_default();
    let (out, out_ct) = host
        .editor_apply(&plugin, &op, bytes, &ct, params)
        .await
        .ok_or(StatusCode::BAD_GATEWAY)?;

    // Persist the edited version (companion + thumbnail) when asked to save.
    if q.save {
        let edited = cfg
            .store_edited_version(photo, &out)
            .await
            .or_500()?;
        pool.upsert_photo(&edited).await.or_500()?;
    }

    Ok(([(header::CONTENT_TYPE, out_ct)], out))
}

/// Body for [`rotate_photo`]: the NET geometry to bake from the untouched
/// original — `degrees` ∈ {0,90,180,270} clockwise, then optional horizontal flip.
#[derive(Debug, serde::Deserialize)]
pub struct RotateBody {
    pub degrees: i32,
    #[serde(default)]
    pub flip: bool,
}

/// Apply a 90°-step rotation (+ optional H-flip) to raw image bytes, returning PNG.
fn rotate_image_bytes(bytes: &[u8], degrees: i32, flip: bool) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut img = match degrees.rem_euclid(360) {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    };
    if flip {
        img = img.fliph();
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// POST /api/photos/{id}/rotate — bake a 90°-step rotation (+ optional flip) into
/// the reserved `edited` companion, ALWAYS from the untouched original so re-edits
/// compose cleanly (the original is never modified). Regenerates the thumbnail and
/// returns the updated photo. Owner-only via central authz on `/api/photos/{id}/..`.
pub async fn rotate_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<RotateBody>,
) -> Result<Json<PhotoView>, StatusCode> {
    let (pool, cfg) = {
        let g = st.read().await;
        (g.persistence.clone().or_500()?, g.storage_ctx())
    };
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;

    // No-op geometry → drop any edit and revert to the original. Checked BEFORE
    // loading the original bytes (mirrors `adjust_photo`).
    let net = body.degrees.rem_euclid(360);
    if net == 0 && !body.flip {
        let reverted = cfg.clear_edited_version(photo).await.or_500()?;
        pool.upsert_photo(&reverted).await.or_500()?;
        return Ok(Json(reverted.effective()));
    }

    let (bytes, _ct) = cfg.load_original_blob(&photo).await.or_404()?;
    let out = tokio::task::spawn_blocking(move || rotate_image_bytes(&bytes, net, body.flip))
        .await
        .ok()
        .flatten()
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let edited = cfg.store_edited_version(photo, &out).await.or_500()?;
    pool.upsert_photo(&edited).await.or_500()?;
    Ok(Json(edited.effective()))
}

/// Body for [`adjust_photo`]: the tonal sliders from the editor's Light/Color
/// tabs. All optional (default 0 = no change). These mirror the CSS-filter math
/// the editor previews with (`filterFor` in `Editor.svelte`) so the baked pixels
/// match what the user saw.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AdjustBody {
    #[serde(default)]
    pub exposure: f32,
    #[serde(default)]
    pub brightness: f32,
    #[serde(default)]
    pub contrast: f32,
    #[serde(default)]
    pub highlights: f32,
    #[serde(default)]
    pub shadows: f32,
    #[serde(default)]
    pub saturation: f32,
    #[serde(default)]
    pub vibrance: f32,
    #[serde(default)]
    pub warmth: f32,
    #[serde(default)]
    pub tint: f32,
}

impl AdjustBody {
    fn all_zero(&self) -> bool {
        [
            self.exposure, self.brightness, self.contrast, self.highlights, self.shadows,
            self.saturation, self.vibrance, self.warmth, self.tint,
        ]
        .iter()
        .all(|v| *v == 0.0)
    }
}

/// Apply the editor's tonal adjustments to raw image bytes, returning PNG. The
/// coefficient math and operation ORDER mirror the browser's CSS filter chain
/// (`brightness → contrast → saturate → sepia → hue-rotate`) so the saved copy
/// matches the live preview.
fn adjust_image_bytes(bytes: &[u8], a: &AdjustBody) -> Option<Vec<u8>> {
    let bright = 1.0 + (a.exposure * 0.6 + a.brightness + a.shadows * 0.25 - a.highlights * 0.18) / 200.0;
    let contrast = 1.0 + (a.contrast + a.highlights * 0.2 - a.shadows * 0.15) / 130.0;
    let sat = 1.0 + (a.saturation + a.vibrance * 0.7) / 110.0;
    let sepia = if a.warmth > 0.0 { a.warmth / 240.0 } else { 0.0 };
    let hue_deg = (if a.warmth < 0.0 { a.warmth * 0.35 } else { 0.0 }) + a.tint * 0.45;

    // saturate() matrix (W3C filter-effects), parameterized by `sat`.
    let sr = 0.2126;
    let sg = 0.7152;
    let sb = 0.0722;
    let sat_m = [
        sr + (1.0 - sr) * sat, sg - sg * sat, sb - sb * sat,
        sr - sr * sat, sg + (1.0 - sg) * sat, sb - sb * sat,
        sr - sr * sat, sg - sg * sat, sb + (1.0 - sb) * sat,
    ];
    // sepia() matrix, parameterized by `sepia` amount (0 = identity).
    let inv = 1.0 - sepia;
    let sep_m = [
        0.393 + 0.607 * inv, 0.769 - 0.769 * inv, 0.189 - 0.189 * inv,
        0.349 - 0.349 * inv, 0.686 + 0.314 * inv, 0.168 - 0.168 * inv,
        0.272 - 0.272 * inv, 0.534 - 0.534 * inv, 0.131 + 0.869 * inv,
    ];
    // hue-rotate() matrix.
    let rad = hue_deg.to_radians();
    let (cos, sin) = (rad.cos(), rad.sin());
    let hue_m = [
        0.213 + cos * 0.787 - sin * 0.213, 0.715 - cos * 0.715 - sin * 0.715, 0.072 - cos * 0.072 + sin * 0.928,
        0.213 - cos * 0.213 + sin * 0.143, 0.715 + cos * 0.285 + sin * 0.140, 0.072 - cos * 0.072 - sin * 0.283,
        0.213 - cos * 0.213 - sin * 0.787, 0.715 - cos * 0.715 + sin * 0.715, 0.072 + cos * 0.928 + sin * 0.072,
    ];

    let mul = |m: &[f32; 9], r: f32, g: f32, b: f32| -> (f32, f32, f32) {
        (
            m[0] * r + m[1] * g + m[2] * b,
            m[3] * r + m[4] * g + m[5] * b,
            m[6] * r + m[7] * g + m[8] * b,
        )
    };

    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img.to_rgba8();
    for px in rgba.pixels_mut() {
        let mut r = px[0] as f32 / 255.0;
        let mut g = px[1] as f32 / 255.0;
        let mut b = px[2] as f32 / 255.0;
        // brightness
        r *= bright; g *= bright; b *= bright;
        // contrast (pivot at 0.5)
        r = (r - 0.5) * contrast + 0.5;
        g = (g - 0.5) * contrast + 0.5;
        b = (b - 0.5) * contrast + 0.5;
        // saturate
        let (r1, g1, b1) = mul(&sat_m, r, g, b);
        // sepia
        let (r2, g2, b2) = mul(&sep_m, r1, g1, b1);
        // hue-rotate
        let (r3, g3, b3) = if hue_deg != 0.0 { mul(&hue_m, r2, g2, b2) } else { (r2, g2, b2) };
        px[0] = (r3.clamp(0.0, 1.0) * 255.0).round() as u8;
        px[1] = (g3.clamp(0.0, 1.0) * 255.0).round() as u8;
        px[2] = (b3.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// POST /api/photos/{id}/adjust — bake the editor's Light/Color tonal sliders into
/// the reserved `edited` companion, ALWAYS from the untouched original (re-edits
/// compose cleanly; the original is never modified). All-zero sliders revert to the
/// original. Regenerates the thumbnail; returns the updated photo. Owner-only via
/// central authz on `/api/photos/{id}/..`.
pub async fn adjust_photo(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<AdjustBody>,
) -> Result<Json<PhotoView>, StatusCode> {
    let (pool, cfg) = {
        let g = st.read().await;
        (g.persistence.clone().or_500()?, g.storage_ctx())
    };
    let photo = pool
        .get_photo(&id)
        .await
        .or_500()?
        .or_404()?;

    // No-op adjustments → drop any edit and revert to the original.
    if body.all_zero() {
        let reverted = cfg.clear_edited_version(photo).await.or_500()?;
        pool.upsert_photo(&reverted).await.or_500()?;
        return Ok(Json(reverted.effective()));
    }

    let (bytes, _ct) = cfg.load_original_blob(&photo).await.or_404()?;
    let out = tokio::task::spawn_blocking(move || adjust_image_bytes(&bytes, &body))
        .await
        .ok()
        .flatten()
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let edited = cfg.store_edited_version(photo, &out).await.or_500()?;
    pool.upsert_photo(&edited).await.or_500()?;
    Ok(Json(edited.effective()))
}

/// GET /api/admin/stats — job run state + entity counts + storage estimates.
pub async fn admin_stats(State(st): State<Shared>) -> Json<AdminStats> {
    // Job telemetry is per-instance runtime state (not persisted domain data), so
    // it lives on the live state — read it BEFORE switching to the DB snapshot.
    let mut jobs: Vec<JobStats> = { st.read().await.jobs.values().cloned().collect() };
    jobs.sort_by(|a, b| a.name.cmp(&b.name));

    let snap = request_snapshot(&st).await;
    let guard;
    let st: &AppState = match &snap {
        Some(s) => s,
        None => {
            guard = st.read().await;
            &guard
        }
    };

    let trashed = st.photos.values().filter(|p| p.deleted_at.is_some()).count();
    let archived = st
        .photos
        .values()
        .filter(|p| p.archived && p.deleted_at.is_none())
        .count();
    // `photos` = total live (non-trashed) photos.
    let live = st.photos.values().filter(|p| p.deleted_at.is_none()).count();
    let vault: usize = st.vaults.values().map(|v| v.photo_ids.len()).sum();

    let counts = StatsCounts {
        photos: live,
        albums: st.albums.len(),
        users: st.users.len(),
        groups: st.groups.len(),
        trashed,
        archived,
        vault,
    };

    // Nominal per-photo size estimate (MB) for the in-memory demo (no real
    // bytes). disk_used = live photos * estimate; s3_used = backed-up live
    // photos * estimate when an S3 config exists.
    const NOMINAL_MB_PER_PHOTO: u64 = 8;
    const QUOTA_MB: u64 = 512_000;
    let disk_used_mb = live as u64 * NOMINAL_MB_PER_PHOTO;
    let has_s3 = st.storage.primary_s3.is_some() || st.storage.backup.s3.is_some();
    let s3_used_mb = if has_s3 {
        st.photos
            .values()
            .filter(|p| p.backed_up && p.deleted_at.is_none())
            .count() as u64
            * NOMINAL_MB_PER_PHOTO
    } else {
        0
    };

    let storage = StatsStorage {
        mode: st.storage.mode,
        disk_used_mb,
        s3_used_mb,
        quota_mb: QUOTA_MB,
    };

    let history = match &st.persistence {
        Some(p) => p.load_job_runs(30).await.unwrap_or_default(),
        None => Vec::new(),
    };

    Json(AdminStats {
        jobs,
        counts,
        storage,
        system: sample_system().await,
        history,
    })
}

// ---- Authorization audit (Feature 4) ----

#[derive(serde::Serialize)]
pub struct AuditResult {
    pub pass: bool,
    pub violations: Vec<AccessViolation>,
}

/// GET /api/audit/access — runtime self-audit proving no read surface exposes a
/// photo to a user without a legitimate grant (and no vault/archived/trashed
/// leak). `pass = true` with an empty list means the system is sound.
pub async fn audit_access(State(st): State<Shared>) -> Json<AuditResult> {
    let snap = request_snapshot(&st).await;
    let guard;
    let st: &AppState = match &snap {
        Some(s) => s,
        None => {
            guard = st.read().await;
            &guard
        }
    };
    let violations = st.audit_access();
    Json(AuditResult {
        pass: violations.is_empty(),
        violations,
    })
}

// ---- Authentication (opt-in login primitive) ----
//
// These routes establish a real session primitive WITHOUT yet gating the
// existing per-user/data handlers on it. Enforcing auth across all routes
// remains a documented follow-up so the current demo UI + tests keep working.

/// Extract a bearer token from an `Authorization: Bearer <token>` header, if present.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// POST /api/login — authenticate by email OR username (`name`) + password. The
/// identifier match is case-insensitive (so the demo `alice`/`alice` works as
/// well as `alice@photon.app`). On success returns a bearer `token` and the
/// public `User`. 401 on unknown identifier, wrong password, or a disabled
/// account.
pub async fn login(
    State(st): State<Shared>,
    Json(body): Json<LoginBody>,
) -> axum::response::Response {
    // Rate-limit on the NORMALIZED identifier — the same value the user lookup uses
    // — so `alice`/`Alice`/`ALICE`/" alice" share one lockout bucket and per-account
    // throttling can't be bypassed by varying case/whitespace (F3).
    let ident = body.email.trim().to_ascii_lowercase();
    let key = format!("login:{ident}");
    // Rate-limit is per-instance ephemeral security state (not domain data).
    if st.read().await.rate_locked(&key) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    // Look up the user in Postgres (the source of truth), not a stale cache.
    let (user, ok) = {
        let snap = request_snapshot(&st).await;
        let guard;
        let lookup: &AppState = match &snap {
            Some(s) => s,
            None => {
                guard = st.read().await;
                &guard
            }
        };
        let secret = lookup.password_secret().to_vec();
        // Accept either the email or the username (name), case-insensitively.
        let user = lookup
            .users
            .values()
            .find(|u| {
                u.email.to_ascii_lowercase() == ident || u.name.to_ascii_lowercase() == ident
            })
            .cloned();
        // Constant-work: ALWAYS run one argon2 verification. A real user runs the
        // genuine check; an unknown or disabled account runs a dummy check of the
        // same cost, so login timing can't reveal which accounts exist (F2).
        let ok = match &user {
            Some(u) if !u.disabled => u.verify_password(&secret, &body.password),
            _ => {
                crate::models::verify_dummy_password(&secret, &body.password);
                false
            }
        };
        (user, ok)
    };
    if !ok {
        // Wrong password / unknown / disabled: count the failure and 401.
        st.write().await.rate_fail(&key);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let user = user.expect("checked ok");

    // SECOND FACTOR — enforced AFTER a correct password, BEFORE minting a session.
    // A user with an enrolled `totp_secret` ALWAYS needs a valid code (enrollment
    // alone enforces 2FA for that user, independent of the global `require_2fa`
    // org-policy flag). When `require_2fa` is on but the user is NOT yet enrolled,
    // login is still allowed (the UI nudges enrollment — never hard-lock a user
    // out before they can enroll).
    let mut consumed_step: Option<i64> = None;
    if let Some(secret_b32) = user.totp_secret.as_deref() {
        let code = match body.totp.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => c,
            None => {
                // Enrolled but no code supplied: signal the UI to prompt for one.
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": "totp_required" })),
                )
                    .into_response();
            }
        };
        let totp_invalid = || {
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "totp_invalid" })))
                .into_response()
        };
        let step = build_totp(secret_b32, &user.email).and_then(|t| totp_step_for(&t, code));
        let Some(step) = step else {
            st.write().await.rate_fail(&key);
            return totp_invalid();
        };
        let step = step as i64;
        // REPLAY: reject a code whose time-step was already used (or any earlier
        // step), so a captured code can't be reused within its validity window.
        let pool = st.read().await.persistence.clone();
        if let Some(p) = &pool {
            let last = p.totp_last_step(&user.id).await.ok().flatten();
            if last.map(|l| step <= l).unwrap_or(false) {
                st.write().await.rate_fail(&key);
                return totp_invalid();
            }
        }
        consumed_step = Some(step);
    }

    // Mint the session + reset rate-limit under a brief write lock, then clone the
    // pool handle and DROP the guard BEFORE any DB await — never hold the global
    // write lock across persistence (mirrors `cast_dlna` / the login path).
    let (token, user_out, pool) = {
        let mut st = st.write().await;
        st.rate_reset(&key);
        let token = st.create_session(&user.id);
        let user_out = st.public_user(&user);
        let pool = st.persistence.clone();
        (token, user_out, pool)
    };
    if let Some(p) = &pool {
        // Record the consumed TOTP step so it (and earlier ones) can't be replayed.
        if let Some(s) = consumed_step {
            let _ = p.set_totp_last_step(&user.id, s).await;
        }
        // Write the session through to Postgres so other instances honor it.
        if let Err(e) = p.upsert_session(&token, &user.id, &now_rfc3339()).await {
            tracing::warn!("persist_session failed: {e}");
        }
    }
    Json(LoginResponse { token, user: user_out }).into_response()
}

/// GET /api/me — return the user for the bearer token in the `Authorization`
/// header. 401 when the header is missing/invalid or the session is unknown.
pub async fn me(
    State(st): State<Shared>,
    headers: HeaderMap,
) -> Result<Json<User>, StatusCode> {
    let token = bearer_token(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    // Postgres-first: resolve the session + load the user from a fresh DB snapshot.
    let snap = request_snapshot(&st).await;
    let guard;
    let st: &AppState = match &snap {
        Some(s) => s,
        None => {
            guard = st.read().await;
            &guard
        }
    };
    let uid = st.resolve_session(&token).await.ok_or(StatusCode::UNAUTHORIZED)?;
    st.users.get(&uid).map(|u| Json(st.public_user(u))).ok_or(StatusCode::UNAUTHORIZED)
}

/// POST /api/logout — drop the session for the bearer token. Idempotent: always
/// returns `{ ok: true }` (even if no such session existed).
pub async fn logout(
    State(st): State<Shared>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    if let Some(token) = bearer_token(&headers) {
        // Drop the session under a brief write lock, clone the pool handle, then
        // DROP the guard BEFORE awaiting the shared-store delete (never hold the
        // global write lock across persistence).
        let pool = {
            let mut st = st.write().await;
            st.end_session(&token);
            st.persistence.clone()
        };
        // Invalidate it in the shared store too, not just this instance's cache.
        if let Some(p) = &pool {
            if let Err(e) = p.delete_session(&token).await {
                tracing::warn!("delete_session failed: {e}");
            }
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

// ---- TOTP two-factor auth (RFC 6238) ----

/// The authenticator-app issuer label, shown in the OTP app and embedded in the
/// `otpauth://` URI.
const TOTP_ISSUER: &str = "Photon";

/// Build a configured RFC-6238 [`TOTP`] from a base32-encoded `secret` and the
/// user's `email` (the account label in the `otpauth://` URI / authenticator
/// app). Standard parameters: SHA1, 6 digits, 30s step, ±1 step skew (so a code
/// from the adjacent window still verifies, tolerating clock drift). Returns
/// `None` when the secret is malformed or too short (RFC requires ≥128 bits).
fn build_totp(secret_b32: &str, email: &str) -> Option<totp_rs::TOTP> {
    use totp_rs::{Algorithm, Secret, TOTP};
    let bytes = Secret::Encoded(secret_b32.to_string()).to_bytes().ok()?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some(TOTP_ISSUER.to_string()),
        email.to_string(),
    )
    .ok()
}

/// Constant-time string equality (no early-out by length or content) for
/// comparing a presented OTP against an expected one.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// The EXACT TOTP time-step a `code` corresponds to within the skew window, or
/// `None` if it doesn't verify. The clock-drift tolerance (±`skew`) governs WHEN a
/// code is accepted, but the returned step is the code's TRUE step — we match each
/// candidate window's freshly generated code WITHOUT the library's inner skew, so
/// the same code always maps to the same step. That is what makes replay
/// protection sound: the consumed step is persisted, and a replay of the same code
/// resolves to the same step (`<= last`) and is rejected, while the next code maps
/// to a higher step and still passes. (Fixes the F6 double-skew, finding N1.)
fn totp_step_for(totp: &totp_rs::TOTP, code: &str) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let step = totp.step.max(1);
    let cur = now / step;
    let skew = totp.skew as u64;
    // Iterate the whole window (constant work, no early return) and record a match.
    let mut found: Option<u64> = None;
    for s in cur.saturating_sub(skew)..=cur + skew {
        let expected = totp.generate(s * step);
        if ct_eq(&expected, code) {
            found = Some(s);
        }
    }
    found
}

/// POST /api/users/{id}/2fa/setup — begin TOTP enrollment (self or admin, via
/// `path_authz`). Generates a fresh base32 secret and returns it plus the
/// `otpauth://` URI for QR display. The secret is NOT persisted yet; the client
/// must confirm it via `2fa/verify` (proving the authenticator works) before
/// 2FA is enabled. 404 if the user is unknown.
pub async fn totp_setup(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<TotpSetupResponse>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let email = snap.users.get(&id).map(|u| u.email.clone()).or_404()?;
    // Fresh 160-bit secret (RFC-4226 recommended length) from the OS CSPRNG,
    // base32-encoded for QR/manual entry into authenticator apps.
    let mut raw = [0u8; 20];
    getrandom::getrandom(&mut raw).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let secret = totp_rs::Secret::Raw(raw.to_vec()).to_encoded().to_string();
    let totp = build_totp(&secret, &email).or_500()?;
    let otpauth_uri = totp.get_url();
    Ok(Json(TotpSetupResponse { secret, otpauth_uri }))
}

/// POST /api/users/{id}/2fa/verify — finish enrollment. Verifies `code` against
/// the candidate `secret` (from `2fa/setup`); on success persists
/// `users.totp_secret = secret` (the user is now enrolled) and returns
/// `{ enabled: true }`. 401 if the code is wrong/expired, 404 if unknown user.
pub async fn totp_verify(
    State(st): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<TotpVerifyBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    let email = st.users.get(&id).map(|u| u.email.clone()).or_404()?;
    let totp = build_totp(&body.secret, &email).ok_or(StatusCode::UNAUTHORIZED)?;
    // `check_current` only errors if the system clock is before the UNIX epoch.
    let ok = totp.check_current(&body.code).unwrap_or(false);
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if let Some(u) = st.users.get_mut(&id) {
        u.totp_secret = Some(body.secret.clone());
    }
    st.persist_user(&id).await;
    Ok(Json(serde_json::json!({ "enabled": true })))
}

/// DELETE /api/users/{id}/2fa — disable TOTP (clear `totp_secret`). Idempotent;
/// returns `{ enabled: false }`. 404 if the user is unknown.
pub async fn totp_disable(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut snap = request_snapshot(&st).await.or_500()?;
    let st: &mut AppState = &mut snap;
    if !st.users.contains_key(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(u) = st.users.get_mut(&id) {
        u.totp_secret = None;
    }
    st.persist_user(&id).await;
    Ok(Json(serde_json::json!({ "enabled": false })))
}

/// GET /api/users/{id}/2fa — report whether TOTP is enabled for the UI. NEVER
/// returns the secret. 404 if the user is unknown.
pub async fn totp_status(
    State(st): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let snap = request_snapshot(&st).await.or_500()?;
    let enabled = snap
        .users
        .get(&id)
        .map(|u| u.totp_secret.is_some())
        .or_404()?;
    Ok(Json(serde_json::json!({ "enabled": enabled })))
}

// ---- DLNA / UPnP casting ----

/// How long `GET /api/cast/devices` spends on SSDP discovery. Short enough to
/// keep the endpoint snappy, long enough for LAN renderers to answer M-SEARCH.
const CAST_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// One entry of the `GET /api/cast/devices` response.
#[derive(serde::Serialize)]
pub struct CastDeviceView {
    pub id: String,
    pub name: String,
    /// Casting backend kind; always "dlna" for now.
    pub kind: &'static str,
}

/// POST /api/cast/dlna body: cast image `url` (with `title`) to `device_id`.
#[derive(serde::Deserialize)]
pub struct CastDlnaBody {
    pub device_id: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
}

/// GET /api/cast/devices — discover DLNA/UPnP MediaRenderers on the LAN (SSDP
/// `M-SEARCH`, ~2s), CACHE them on `AppState` keyed by id (so the follow-up
/// `POST /api/cast/dlna` can resolve `device_id`), and return the list. Returns
/// an empty array when nothing is found / offline; never errors. Browsers can't
/// do DLNA, so discovery must run here on the server.
pub async fn cast_devices(State(st): State<Shared>) -> Json<Vec<CastDeviceView>> {
    // Discover WITHOUT holding the lock (it does network I/O), then refresh the
    // cache under a short write lock.
    let devices = crate::dlna::discover(CAST_DISCOVERY_TIMEOUT).await;
    let mut guard = st.write().await;
    guard.dlna_devices = devices
        .iter()
        .map(|d| (d.id.clone(), d.clone()))
        .collect();
    let mut out: Vec<CastDeviceView> = guard
        .dlna_devices
        .values()
        .map(|d| CastDeviceView {
            id: d.id.clone(),
            name: d.name.clone(),
            kind: "dlna",
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Json(out)
}

/// POST /api/cast/dlna — resolve `device_id` against the cache populated by
/// `GET /api/cast/devices` and cast the image `url` to it via UPnP AVTransport
/// (SetAVTransportURI + Play). 404 when the device id is unknown (re-discover
/// first), 502 when the renderer rejects/cannot be reached, 200 `{ ok: true }`
/// on success.
pub async fn cast_dlna(
    State(st): State<Shared>,
    Json(body): Json<CastDlnaBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // SSRF guard: only let the renderer be pointed at plain HTTP(S) media URLs,
    // never internal schemes (file:, gopher:, …) or schemeless internal paths.
    let url = body.url.trim().to_ascii_lowercase();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Clone the cached device out from under the lock; casting does network I/O.
    let device = {
        let guard = st.read().await;
        guard
            .dlna_devices
            .get(&body.device_id)
            .cloned()
            .or_404()?
    };
    crate::dlna::cast(&device, &body.url, &body.title)
        .await
        .map_err(|e| {
            tracing::warn!("DLNA cast to {} failed: {e}", device.id);
            StatusCode::BAD_GATEWAY
        })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- OIDC web login (relying-party / authorization-code flow) ----

/// TTL for a pending OIDC login `state`: the user has this long between hitting
/// `/login` and the IdP redirecting back to `/callback`.
const OIDC_STATE_TTL_SECS: i64 = 600; // 10 minutes

/// Query params on the IdP's redirect back to `/api/auth/oidc/callback`.
#[derive(serde::Deserialize)]
pub struct OidcCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    /// Some IdPs send `error`/`error_description` instead of a code on denial.
    pub error: Option<String>,
}

/// GET /api/auth/oidc/available — PUBLIC. Reports whether OIDC web login is
/// configured so the UI can show/hide the "Continue with OpenID" button. Always
/// 200 (never leaks why it's off).
pub async fn oidc_available(State(st): State<Shared>) -> Json<serde_json::Value> {
    let available = st.read().await.oidc_login.is_some();
    Json(serde_json::json!({ "available": available }))
}

/// GET /api/auth/oidc/login — PUBLIC. Begins the auth-code flow: mint a CSPRNG
/// `state`+`nonce`, persist them (DB, so the callback may land on any instance),
/// then 302 to the IdP's authorization endpoint. 404 when the feature is inert.
pub async fn oidc_login_start(State(st): State<Shared>) -> axum::response::Response {
    let (oidc, persistence) = {
        let g = st.read().await;
        (g.oidc_login.clone(), g.persistence.clone())
    };
    let oidc = match oidc {
        Some(o) => o,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let state = crate::state::random_hex(24);
    let nonce = crate::state::random_hex(24);

    // Persist the state/nonce so the callback (possibly on another instance) can
    // validate it. Without a DB we cannot safely complete the flow → 503.
    let Some(p) = persistence else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if let Err(e) = p.insert_oidc_state(&state, &nonce, &now_rfc3339()).await {
        tracing::warn!("insert_oidc_state failed: {e}");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // Best-effort housekeeping of abandoned/expired states.
    if let Ok(cutoff) = oidc_state_cutoff() {
        let _ = p.cleanup_oidc_states(&cutoff).await;
    }

    let url = oidc.authorize_url(&state, &nonce);
    axum::response::Redirect::to(&url).into_response()
}

/// The RFC3339 cutoff before which an OIDC state is considered expired.
fn oidc_state_cutoff() -> Result<String, ()> {
    use time::format_description::well_known::Rfc3339;
    (time::OffsetDateTime::now_utc() - time::Duration::seconds(OIDC_STATE_TTL_SECS))
        .format(&Rfc3339)
        .map_err(|_| ())
}

/// GET /api/auth/oidc/callback — PUBLIC. The IdP redirects here with `code`+`state`.
/// Validates `state` (single-use + TTL), exchanges the code, validates the
/// `id_token` (incl. `nonce`), maps email→Photon user (find-or-create), mints a
/// session, and 302s to the SPA with `?token=`. Any failure redirects to
/// `/?oidc_error=1` (never panics, never 500s the browser).
pub async fn oidc_callback(
    State(st): State<Shared>,
    Query(q): Query<OidcCallbackQuery>,
) -> axum::response::Response {
    let (oidc, persistence) = {
        let g = st.read().await;
        (g.oidc_login.clone(), g.persistence.clone())
    };
    let oidc = match oidc {
        Some(o) => o,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let Some(p) = persistence else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    // Run the fallible flow; on ANY error redirect to the SPA's error path.
    match oidc_callback_inner(&st, &oidc, &p, q).await {
        Ok(token) => {
            // Deliver the session in the URL **fragment**, not the query: fragments
            // are never sent to the server (no access logs) nor in the `Referer`
            // header, unlike `?token=`. The SPA reads it from `location.hash`. (F7)
            let target = format!("/#token={}", urlencode(&token));
            axum::response::Redirect::to(&target).into_response()
        }
        Err(e) => {
            tracing::warn!("OIDC callback failed: {e}");
            axum::response::Redirect::to("/?oidc_error=1").into_response()
        }
    }
}

/// The fallible body of [`oidc_callback`], factored out so the outer handler maps
/// every error to a single redirect.
async fn oidc_callback_inner(
    st: &Shared,
    oidc: &crate::oidc::OidcLogin,
    p: &crate::db::Persistence,
    q: OidcCallbackQuery,
) -> Result<String, String> {
    if let Some(err) = q.error {
        return Err(format!("IdP returned error: {err}"));
    }
    let code = q.code.ok_or("missing code")?;
    let state = q.state.ok_or("missing state")?;

    // Validate state: single-use lookup + TTL.
    let (nonce, created_at) = p
        .take_oidc_state(&state)
        .await
        .map_err(|e| format!("take_oidc_state failed: {e}"))?
        .ok_or("unknown or already-used state")?;
    if is_expired(&created_at, OIDC_STATE_TTL_SECS) {
        return Err("state expired".to_string());
    }

    // Exchange the code, then validate the returned id_token.
    let tokens = oidc.exchange_code(&code).await?;
    let claims = oidc.verify_id_token(&tokens.id_token).await?;

    // The id_token's nonce MUST match the one we issued (replay / token-injection
    // defense).
    match claims.nonce.as_deref() {
        Some(n) if n == nonce => {}
        _ => return Err("id_token nonce mismatch".to_string()),
    }

    let email = claims.email.as_deref().ok_or("id_token has no email claim")?;
    tracing::info!(
        "OIDC login: sub={:?} email={email}",
        claims.sub.as_deref().unwrap_or("?")
    );
    crate::oidc::login_or_create_session(st, email, claims.name.as_deref()).await
}

/// Minimal percent-encoding for a session token placed in a redirect query.
fn urlencode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Persistence;
    use crate::models::{ShareTarget, ShareRole};
    use crate::state::seed;
    use sqlx::PgPool;

    /// Build a tiny solid-color PNG for the pixel-bake tests.
    fn solid_png(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([r, g, b, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn adjust_all_zero_is_detected() {
        assert!(AdjustBody::default().all_zero());
        assert!(!AdjustBody { exposure: 10.0, ..Default::default() }.all_zero());
        assert!(!AdjustBody { tint: -5.0, ..Default::default() }.all_zero());
    }

    #[test]
    fn adjust_brightens_pixels() {
        let png = solid_png(100, 100, 100);
        let out = adjust_image_bytes(&png, &AdjustBody { exposure: 100.0, brightness: 50.0, ..Default::default() })
            .expect("bake");
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        let px = img.get_pixel(0, 0);
        // A positive exposure/brightness must raise the luminance.
        assert!(px[0] > 100, "expected brighter, got {}", px[0]);
        assert_eq!(px[3], 255, "alpha preserved");
    }

    #[test]
    fn adjust_warmth_pushes_toward_red() {
        let png = solid_png(120, 120, 120);
        let out = adjust_image_bytes(&png, &AdjustBody { warmth: 80.0, ..Default::default() }).expect("bake");
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        let px = img.get_pixel(0, 0);
        // Sepia warmth makes a neutral grey warmer: red channel ends above blue.
        assert!(px[0] > px[2], "warmth should make red ({}) exceed blue ({})", px[0], px[2]);
    }

    #[test]
    fn ct_eq_is_correct() {
        assert!(ct_eq("123456", "123456"));
        assert!(!ct_eq("123456", "123457"));
        assert!(!ct_eq("12345", "123456")); // length mismatch
        assert!(ct_eq("", ""));
    }

    #[test]
    fn totp_step_for_resolves_the_codes_true_step_not_a_skewed_one() {
        // 160-bit base32 secret (RFC requires >= 128 bits).
        let totp = build_totp("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP", "a@x").expect("totp");
        let step = totp.step;
        let cur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / step;
        // A code generated for the CURRENT step must resolve to exactly `cur`
        // (not `cur-1` — the bug N1 fixed: the persisted step must equal the real
        // step so a replay maps to `<= last` and is rejected).
        let code_cur = totp.generate(cur * step);
        assert_eq!(totp_step_for(&totp, &code_cur), Some(cur));
        // A code from the previous step still verifies (drift tolerance) but maps to
        // its OWN true step, so it is strictly older and a replay is detectable.
        let code_prev = totp.generate((cur - 1) * step);
        assert_eq!(totp_step_for(&totp, &code_prev), Some(cur - 1));
        // Garbage never verifies.
        assert!(totp_step_for(&totp, "abcdef").is_none());
    }

    /// A Photon instance backed by the test's isolated, freshly-migrated Postgres
    /// (`#[sqlx::test]`). The demo seed is written to the DB so handlers — which are
    /// Postgres-first — read it back per request. Postgres is the source of truth;
    /// there is no in-memory mode.
    async fn shared(pool: PgPool) -> Shared {
        let mut st = seed();
        // Each `#[sqlx::test]` gets an isolated DB but they all share the filesystem
        // blob backend. Photo ids come from a DB-reserved block, so parallel tests
        // in separate DBs can mint the SAME `ph_<n>` and clobber each other's blobs
        // under the default `data/` dir. Give every test a UNIQUE data dir so blob
        // writes can't collide (LocalFs `put_object` creates parent dirs on demand).
        static TEST_DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        st.data_dir = format!(
            "{}/photon-test-{}-{}",
            std::env::temp_dir().display(),
            std::process::id(),
            n
        );
        st.persistence = Some(Persistence::from_pool(pool));
        st.persist_seed().await;
        Arc::new(RwLock::new(st))
    }

    /// Refresh the in-memory maps from Postgres. Postgres-first handlers write the
    /// DB, not the live `AppState`, so a test that peeks into `st.read().await.<map>`
    /// after a mutating handler must reload first to see the change.
    async fn reload(st: &Shared) {
        st.write().await.load_from_db().await.expect("reload from db");
    }

    /// Drive `login` and decode its `Response` into `(status, json_body)`. `login`
    /// returns a raw `axum::response::Response` (it needs distinct JSON bodies for
    /// the `totp_required` case), so tests collect the body to assert on it.
    async fn do_login(st: &Shared, body: LoginBody) -> (StatusCode, serde_json::Value) {
        use axum::body::to_bytes;
        let resp = login(State(st.clone()), Json(body)).await;
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    /// Poll `get_import` until every item reaches `Done` (the background enrichment
    /// task — Thumbnail → Analysis → Finalize — runs after `upload_raw` returns 202).
    async fn await_import(st: &Shared, batch_id: &str) -> crate::models::ImportBatch {
        // Generous budget: the background enrichment runs on the shared blocking
        // pool, so under parallel `#[sqlx::test]` load it can take a while.
        for _ in 0..1200 {
            let b = get_import(
                State(st.clone()),
                Extension(crate::auth::AuthUser("usr_alice".to_string())),
                Path(batch_id.to_string()),
            )
            .await
            .expect("batch present");
            if b.0.items.iter().all(|i| i.stage == ImportStage::Done) {
                return b.0;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("import {batch_id} did not complete");
    }

    /// Casting to an unknown device id (cache empty / id not discovered) is a
    /// 404 BEFORE any network is touched — offline-safe.
    #[sqlx::test(migrations = "./migrations")]
    async fn cast_dlna_unknown_device_is_404(pool: PgPool) {
        let st = shared(pool).await;
        let res = cast_dlna(
            State(st.clone()),
            Json(CastDlnaBody {
                device_id: "dlna_deadbeef".to_string(),
                url: "http://host/api/photos/ph_1/render".to_string(),
                title: "x".to_string(),
            }),
        )
        .await;
        assert_eq!(res.err(), Some(StatusCode::NOT_FOUND));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn smtp_redaction_round_trip(pool: PgPool) {
        let st = shared(pool).await;
        // PUT a config with a real password.
        let put = update_smtp(
            State(st.clone()),
            Json(UpdateSmtp {
                host: "smtp.example.com".to_string(),
                port: 587,
                username: "user".to_string(),
                password: "super-secret".to_string(),
                from: "noreply@photon.app".to_string(),
                tls: false,
            }),
        )
        .await;
        // Returned config is redacted.
        assert_eq!(put.0.password, REDACTED_SECRET);

        // GET hides the password.
        let got = get_smtp(State(st.clone())).await;
        assert_eq!(got.0.password, REDACTED_SECRET);
        assert_eq!(got.0.host, "smtp.example.com");

        // PUT with the sentinel preserves the stored password.
        let _ = update_smtp(
            State(st.clone()),
            Json(UpdateSmtp {
                host: "smtp2.example.com".to_string(),
                port: 25,
                username: "user".to_string(),
                password: REDACTED_SECRET.to_string(),
                from: "noreply@photon.app".to_string(),
                tls: true,
            }),
        )
        .await;
        reload(&st).await;
        let st_read = st.read().await;
        assert_eq!(st_read.smtp.as_ref().unwrap().password, "super-secret");
        assert_eq!(st_read.smtp.as_ref().unwrap().host, "smtp2.example.com");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn invite_create_stores_token_and_log_mailer_succeeds(pool: PgPool) {
        let st = shared(pool).await; // no SMTP -> LogMailer path
        let (code, invite) = create_invite(
            State(st.clone()),
            Json(CreateInvite {
                email: "new@photon.app".to_string(),
                inviter_id: "usr_alice".to_string(),
            }),
        )
        .await
        .expect("invite created");
        assert_eq!(code, StatusCode::CREATED);
        assert!(!invite.0.token.is_empty());
        assert!(!invite.0.accepted);

        // Stored, and listable.
        let list = list_invites(State(st.clone())).await;
        assert_eq!(list.0.len(), 1);
        assert_eq!(list.0[0].token, invite.0.token);

        // Unknown inviter -> 404.
        let err = create_invite(
            State(st.clone()),
            Json(CreateInvite {
                email: "x@y.z".to_string(),
                inviter_id: "usr_nope".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accept_invite_creates_user(pool: PgPool) {
        let st = shared(pool).await;
        let (_c, invite) = create_invite(
            State(st.clone()),
            Json(CreateInvite {
                email: "new@photon.app".to_string(),
                inviter_id: "usr_alice".to_string(),
            }),
        )
        .await
        .unwrap();

        let user = accept_invite(
            State(st.clone()),
            Json(AcceptInvite {
                token: invite.0.token.clone(),
                name: "Newbie".to_string(),
            }),
        )
        .await
        .expect("accepted");
        assert_eq!(user.0.email, "new@photon.app");
        assert_eq!(user.0.name, "Newbie");
        assert!(user.0.id.starts_with("usr_"));

        // The user now exists in Postgres, the invite is marked accepted.
        reload(&st).await;
        let st_read = st.read().await;
        assert!(st_read.users.contains_key(&user.0.id));
        assert!(st_read.invites.get(&invite.0.token).unwrap().accepted);
        drop(st_read);

        // Bad token -> 404.
        let err = accept_invite(
            State(st.clone()),
            Json(AcceptInvite {
                token: "bogus".to_string(),
                name: "X".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accept_invite_rejects_replay_and_expired(pool: PgPool) {
        let st = shared(pool).await;
        let (_c, invite) = create_invite(
            State(st.clone()),
            Json(CreateInvite {
                email: "replay@photon.app".to_string(),
                inviter_id: "usr_alice".to_string(),
            }),
        )
        .await
        .unwrap();
        let token = invite.0.token.clone();

        // First accept succeeds.
        let _ = accept_invite(
            State(st.clone()),
            Json(AcceptInvite {
                token: token.clone(),
                name: "First".to_string(),
            }),
        )
        .await
        .expect("first accept ok");

        // Replay (already accepted) -> 409 Conflict.
        let err = accept_invite(
            State(st.clone()),
            Json(AcceptInvite {
                token: token.clone(),
                name: "Replay".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::CONFLICT);

        // A fresh-but-stale invite (created_at older than the TTL) -> 410 Gone.
        let stale_token = {
            let mut w = st.write().await;
            let stale = w.new_invite_token();
            // 73h ago, beyond the 72h INVITE_TTL_SECS.
            let old = time::OffsetDateTime::now_utc() - time::Duration::hours(73);
            let created_at = old
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            w.invites.insert(
                stale.clone(),
                Invite {
                    token: stale.clone(),
                    email: "stale@photon.app".to_string(),
                    inviter_id: "usr_alice".to_string(),
                    created_at,
                    accepted: false,
                },
            );
            // Persist so the Postgres-first `accept_invite` (which reads the DB)
            // sees this stale invite.
            w.persist_invite(&stale).await;
            stale
        };
        let err = accept_invite(
            State(st.clone()),
            Json(AcceptInvite {
                token: stale_token,
                name: "Stale".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::GONE);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn expired_reset_token_is_rejected(pool: PgPool) {
        let st = shared(pool).await;
        let (_c, user) = create_user(
            State(st.clone()),
            Json(CreateUser {
                name: "Helen".to_string(),
                email: "helen@photon.app".to_string(),
                is_admin: false,
            }),
        )
        .await
        .unwrap();
        let uid = user.0.id.clone();

        // Issue a reset token, then back-date it past the 24h RESET_TTL_SECS.
        let _ = reset_user_password(State(st.clone()), Path(uid.clone()))
            .await
            .unwrap();
        reload(&st).await;
        let token = {
            let mut w = st.write().await;
            let tok = w
                .reset_tokens
                .values()
                .find(|t| t.user_id == uid && !t.used)
                .map(|t| t.token.clone())
                .unwrap();
            let old = time::OffsetDateTime::now_utc() - time::Duration::hours(25);
            w.reset_tokens.get_mut(&tok).unwrap().created_at = old
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            // Persist the back-dated token so the Postgres-first handler sees it expired.
            w.persist_reset_token(&tok).await;
            tok
        };

        // The expired token must NOT authorize a password change.
        let err = set_user_password(
            State(st.clone()),
            Path(uid.clone()),
            Json(SetPasswordBody {
                current_password: None,
                new_password: "secret1".to_string(),
                reset_token: Some(token),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    // ---- Auth: login / me / logout (opt-in primitive) ----

    #[sqlx::test(migrations = "./migrations")]
    async fn login_success_then_me_then_logout(pool: PgPool) {
        let st = shared(pool).await;
        // Seed demo login: alice / "alice".
        let (status, body) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "alice".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = body["token"].as_str().expect("token").to_string();
        assert!(!token.is_empty());
        assert_eq!(body["user"]["id"], "usr_alice");
        // The serialized user never leaks secrets.
        assert!(body["user"].as_object().unwrap().get("password_hash").is_none());
        assert!(body["user"].as_object().unwrap().get("totp_secret").is_none());

        // GET /api/me with the bearer token returns the same user.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let who = me(State(st.clone()), headers.clone()).await.expect("me ok");
        assert_eq!(who.0.id, "usr_alice");

        // Logout drops the session; /api/me then 401s.
        let _ = logout(State(st.clone()), headers.clone()).await;
        let err = me(State(st.clone()), headers).await.unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    // ---- OIDC web login (relying-party flow) ----

    /// With no OIDC env configured (the test default), the feature is inert:
    /// `/available` reports false and `/login` 404s.
    #[sqlx::test(migrations = "./migrations")]
    async fn oidc_inert_when_unconfigured(pool: PgPool) {
        let st = shared(pool).await;
        let avail = oidc_available(State(st.clone())).await;
        assert_eq!(avail.0["available"], serde_json::json!(false));

        let resp = oidc_login_start(State(st.clone())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The callback is likewise inert (404) — no IdP, no state minted.
        let resp = oidc_callback(
            State(st.clone()),
            Query(OidcCallbackQuery { code: Some("x".into()), state: Some("y".into()), error: None }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// The `oidc_states` migration applies and the DB store round-trips: insert
    /// then take returns the nonce, and a SECOND take returns None (single-use).
    #[sqlx::test(migrations = "./migrations")]
    async fn oidc_state_store_roundtrip_and_single_use(pool: PgPool) {
        let p = Persistence::from_pool(pool);
        p.insert_oidc_state("st-1", "nonce-1", &now_rfc3339()).await.expect("insert");
        let got = p.take_oidc_state("st-1").await.expect("take");
        let (nonce, _created) = got.expect("present");
        assert_eq!(nonce, "nonce-1");
        // Single-use: a second take finds nothing.
        assert!(p.take_oidc_state("st-1").await.expect("take2").is_none());
        // Unknown state is None too.
        assert!(p.take_oidc_state("nope").await.expect("take3").is_none());
    }

    /// The claims→user mapping: a NEW email creates a passwordless non-admin user
    /// and mints a session; the SAME email reuses the existing user.
    #[sqlx::test(migrations = "./migrations")]
    async fn oidc_login_or_create_session_maps_users(pool: PgPool) {
        let st = shared(pool).await;

        // New email -> creates a user + returns a usable session token.
        let token = crate::oidc::login_or_create_session(&st, "newcomer@photon.app", Some("New Comer"))
            .await
            .expect("create");
        assert!(!token.is_empty());
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        let who = me(State(st.clone()), headers).await.expect("me ok");
        assert_eq!(who.0.email, "newcomer@photon.app");
        assert_eq!(who.0.name, "New Comer");
        assert!(!who.0.is_admin);
        let new_id = who.0.id.clone();

        // Existing email (alice, case-insensitive) -> reuses the seed user, no dup.
        let token2 = crate::oidc::login_or_create_session(&st, "ALICE@photon.app", None)
            .await
            .expect("reuse");
        let mut headers2 = HeaderMap::new();
        headers2.insert(header::AUTHORIZATION, format!("Bearer {token2}").parse().unwrap());
        let who2 = me(State(st.clone()), headers2).await.expect("me ok");
        assert_eq!(who2.0.id, "usr_alice");
        assert_ne!(who2.0.id, new_id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn login_failure_and_me_without_token(pool: PgPool) {
        let st = shared(pool).await;
        // Wrong password -> 401.
        let (status, _) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "wrong".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Unknown email -> 401.
        let (status, _) = do_login(
            &st,
            LoginBody {
                email: "nobody@photon.app".to_string(),
                password: "x".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Disabled user -> 401 even with the correct password.
        {
            let mut w = st.write().await;
            w.users.get_mut("usr_bob").unwrap().disabled = true;
            w.persist_user("usr_bob").await;
        }
        let (status, _) = do_login(
            &st,
            LoginBody {
                email: "bob@photon.app".to_string(),
                password: "bob".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // /api/me without an Authorization header -> 401.
        let err = me(State(st.clone()), HeaderMap::new()).await.unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);

        // /api/me with a bogus token -> 401.
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer nope".parse().unwrap());
        let err = me(State(st.clone()), headers).await.unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn login_accepts_username_case_insensitively(pool: PgPool) {
        let st = shared(pool).await;
        // Demo convention: password = first name; log in by USERNAME, mixed case.
        let (status, body) = do_login(
            &st,
            LoginBody { email: "Alice".to_string(), password: "alice".to_string(), totp: None },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["id"], "usr_alice");
        // The email path still works too.
        let (status, body) = do_login(
            &st,
            LoginBody { email: "bob@photon.app".to_string(), password: "bob".to_string(), totp: None },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["id"], "usr_bob");
    }

    /// Full TOTP enrollment lifecycle: setup -> verify (enroll) -> status -> login
    /// enforcement (missing/valid/wrong code) -> disable -> login again without code.
    #[sqlx::test(migrations = "./migrations")]
    async fn totp_enroll_enforce_and_disable(pool: PgPool) {
        let st = shared(pool).await;
        let uid = "usr_alice".to_string();

        // 1) setup returns a base32 secret + an otpauth:// URI for QR display.
        let setup = totp_setup(State(st.clone()), Path(uid.clone()))
            .await
            .expect("setup ok");
        let secret = setup.0.secret.clone();
        assert!(!secret.is_empty());
        assert!(setup.0.otpauth_uri.starts_with("otpauth://totp/"));
        assert!(setup.0.otpauth_uri.contains("issuer=Photon"));

        // Not enrolled until verified: GET /2fa shows disabled, login needs no code.
        let status = totp_status(State(st.clone()), Path(uid.clone())).await.expect("status");
        assert_eq!(status.0["enabled"], false);

        // 2) verify with the WRONG code -> 401, still not enrolled.
        let bad = totp_verify(
            State(st.clone()),
            Path(uid.clone()),
            Json(TotpVerifyBody { secret: secret.clone(), code: "000000".to_string() }),
        )
        .await
        .unwrap_err();
        assert_eq!(bad, StatusCode::UNAUTHORIZED);

        // 2b) verify with the CORRECT current code (generated from the secret) -> enrolled.
        let totp = build_totp(&secret, "alice@photon.app").expect("totp");
        let code = totp.generate_current().expect("code");
        let ok = totp_verify(
            State(st.clone()),
            Path(uid.clone()),
            Json(TotpVerifyBody { secret: secret.clone(), code: code.clone() }),
        )
        .await
        .expect("verify ok");
        assert_eq!(ok.0["enabled"], true);

        // 3) GET /2fa now reports enabled.
        let status = totp_status(State(st.clone()), Path(uid.clone())).await.expect("status");
        assert_eq!(status.0["enabled"], true);

        // 4) login WITHOUT a code -> 401 { error: "totp_required" }.
        let (st_code, body) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "alice".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(st_code, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "totp_required");

        // 4b) login WITH a wrong code -> 401 { error: "totp_invalid" }.
        let (st_code, body) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "alice".to_string(),
                totp: Some("000000".to_string()),
            },
        )
        .await;
        assert_eq!(st_code, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "totp_invalid");

        // 4c) login WITH a valid code -> 200 + token.
        let code = totp.generate_current().expect("code");
        let (st_code, body) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "alice".to_string(),
                totp: Some(code),
            },
        )
        .await;
        assert_eq!(st_code, StatusCode::OK);
        assert!(!body["token"].as_str().unwrap().is_empty());

        // 5) DELETE /2fa disables; status flips and login no longer needs a code.
        let disabled = totp_disable(State(st.clone()), Path(uid.clone())).await.expect("disable");
        assert_eq!(disabled.0["enabled"], false);
        let status = totp_status(State(st.clone()), Path(uid.clone())).await.expect("status");
        assert_eq!(status.0["enabled"], false);

        let (st_code, _) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "alice".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(st_code, StatusCode::OK);
    }

    /// The global `require_2fa` flag does NOT hard-block a not-yet-enrolled user
    /// (no lockout): with the flag on but no `totp_secret`, login still succeeds.
    #[sqlx::test(migrations = "./migrations")]
    async fn require_2fa_flag_does_not_lock_out_unenrolled(pool: PgPool) {
        let st = shared(pool).await;
        // Flip features.require_2fa = true via the settings PATCH (RFC 6902).
        let ops: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": "/features/require_2fa", "value": true }
        ]))
        .unwrap();
        let _ = patch_settings(State(st.clone()), Json(ops)).await.expect("patch settings");

        // Alice has no totp_secret yet -> login still works (non-blocking policy).
        let (status, body) = do_login(
            &st,
            LoginBody {
                email: "alice@photon.app".to_string(),
                password: "alice".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body["token"].as_str().unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn share_succeeds_end_to_end_with_log_mailer(pool: PgPool) {
        let st = shared(pool).await; // no SMTP configured -> LogMailer, never fails
        let album = add_album_share(
            State(st.clone()),
            Path("alb_chamonix".to_string()),
            Json(ShareBody {
                target: ShareTarget::Group("grp_family".to_string()),
                role: ShareRole::Viewer,
            }),
        )
        .await
        .expect("share ok");
        assert!(album
            .0
            .shares
            .iter()
            .any(|s| s.target == ShareTarget::Group("grp_family".to_string())));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn contribute_succeeds_end_to_end_with_log_mailer(pool: PgPool) {
        let st = shared(pool).await;
        // Bob is a Contributor on alb_summer (owned by Alice) per the seed.
        // Find a bob-owned photo not yet in the album.
        let pid = {
            let r = st.read().await;
            r.photos
                .values()
                .find(|p| p.owner_id == "usr_bob")
                .map(|p| p.id.clone())
                .unwrap()
        };
        let album = contribute_to_album(
            State(st.clone()),
            Extension(crate::auth::AuthUser("usr_bob".to_string())),
            Path("alb_summer".to_string()),
            Json(ContributeBody {
                photo_ids: vec![pid.clone()],
            }),
        )
        .await
        .expect("contribute ok");
        assert!(album.0.photo_ids.contains(&pid));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn render_plan_returns_negotiated_plan(pool: PgPool) {
        let st = shared(pool).await;
        let pid = {
            let r = st.read().await;
            r.photos.keys().next().cloned().unwrap()
        };
        let plan = render_plan(
            State(st.clone()),
            Path(pid.clone()),
            Query(RenderQuery {
                w: Some(800),
                h: None,
                fmt: None,
                supports: Some("webp,jpeg".to_string()),
            }),
            HeaderMap::new(),
        )
        .await
        .expect("render plan");
        // cache key reflects chosen dims + ext.
        assert!(plan.0.cache_key.starts_with(&pid));
        assert!(plan.0.plan.width <= 800);
        assert!(!plan.0.mime.is_empty());
    }

    // ---- Feature 1: admin user mgmt + password reset flow ----

    #[sqlx::test(migrations = "./migrations")]
    async fn admin_create_makes_passwordless_user(pool: PgPool) {
        let st = shared(pool).await;
        let (code, user) = create_user(
            State(st.clone()),
            Json(CreateUser {
                name: "Frank".to_string(),
                email: "frank@photon.app".to_string(),
                is_admin: false,
            }),
        )
        .await
        .expect("user created");
        assert_eq!(code, StatusCode::CREATED);
        assert!(user.0.id.starts_with("usr_"));
        // Newly created user has no password (read back from Postgres).
        reload(&st).await;
        let r = st.read().await;
        assert!(r.users.get(&user.0.id).unwrap().password_hash.is_none());
        // The serialized response never exposes the hash/salt.
        let v = serde_json::to_value(&user.0).unwrap();
        assert!(!v.as_object().unwrap().contains_key("password_hash"));
        assert!(!v.as_object().unwrap().contains_key("salt"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn reset_token_flow_lets_user_set_password(pool: PgPool) {
        let st = shared(pool).await; // LogMailer path
        // Admin triggers a reset for a passwordless user.
        let (_code, user) = create_user(
            State(st.clone()),
            Json(CreateUser {
                name: "Gina".to_string(),
                email: "gina@photon.app".to_string(),
                is_admin: false,
            }),
        )
        .await
        .unwrap();
        let uid = user.0.id.clone();

        let _ = reset_user_password(State(st.clone()), Path(uid.clone()))
            .await
            .expect("reset issued");
        // Fetch the issued token from Postgres.
        reload(&st).await;
        let token = {
            let r = st.read().await;
            r.reset_tokens
                .values()
                .find(|t| t.user_id == uid && !t.used)
                .map(|t| t.token.clone())
                .expect("token exists")
        };

        // Setting a password without credentials is forbidden.
        let err = set_user_password(
            State(st.clone()),
            Path(uid.clone()),
            Json(SetPasswordBody {
                current_password: None,
                new_password: "secret1".to_string(),
                reset_token: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);

        // With the reset token it succeeds.
        let _ = set_user_password(
            State(st.clone()),
            Path(uid.clone()),
            Json(SetPasswordBody {
                current_password: None,
                new_password: "secret1".to_string(),
                reset_token: Some(token.clone()),
            }),
        )
        .await
        .expect("password set");

        reload(&st).await;
        let r = st.read().await;
        let secret = r.password_secret().to_vec();
        assert!(r.users.get(&uid).unwrap().verify_password(&secret, "secret1"));
        assert!(!r.users.get(&uid).unwrap().verify_password(&secret, "wrong"));
        // Token is now marked used and cannot be reused.
        assert!(r.reset_tokens.get(&token).unwrap().used);
        drop(r);

        let err = set_user_password(
            State(st.clone()),
            Path(uid.clone()),
            Json(SetPasswordBody {
                current_password: None,
                new_password: "secret2".to_string(),
                reset_token: Some(token.clone()),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    // ---- Feature 2: thumbnail generation ----

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        use std::io::Cursor;
        let mut img = image::RgbImage::new(w, h);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, 120, 200]);
        }
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn raw_upload_generates_small_thumbnail(pool: PgPool) {
        let st = shared(pool).await;
        // 1000x800 source -> thumbnail long edge must be <= ~320.
        let bytes = png_bytes(1000, 800);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        // STAGE 1 (Upload): the POST now returns 202 + a batch, NOT the photos.
        let (code, accepted) = upload_raw(
            State(st.clone()),
            Extension(crate::auth::AuthUser("usr_alice".to_string())),
            Json(RawUploadBody {
                owner_id: "usr_alice".to_string(),
                album_id: None,
                files: vec![crate::models::RawUploadedFile {
                    filename: "shot.png".to_string(),
                    bytes: b64,
                }],
            }),
        )
        .await
        .expect("upload ok");
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(accepted.0.items.len(), 1);
        // The POST did STAGE 1-2 synchronously; the item is Ok and ready to enrich.
        assert_eq!(accepted.0.items[0].status, ImportStatus::Ok);
        let batch_id = accepted.0.batch_id.clone();

        // Wait for the background enrichment (Thumbnail → Analysis → Finalize).
        let batch = await_import(&st, &batch_id).await;
        let item = &batch.items[0];
        assert_eq!(item.stage, ImportStage::Done);
        assert_eq!(item.status, ImportStatus::Ok);
        let id = item.photo_id.clone().expect("photo created");

        // The created photo has a thumb_url.
        let view = get_photo(State(st.clone()), Path(id.clone()))
            .await
            .expect("photo present");
        assert!(view.0.thumb_url.is_some());

        // The thumb endpoint returns bytes (served from the backend); decode + check
        // dims <= source & <= 320.
        use axum::response::IntoResponse as _;
        let resp = get_thumb(State(st.clone()), Path(id.clone()))
            .await
            .expect("thumb served")
            .into_response();
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "image/webp");
        let thumb_bytes = body_bytes(resp).await;
        let decoded = image::load_from_memory(&thumb_bytes).expect("decode thumb");
        use image::GenericImageView;
        let (tw, th) = decoded.dimensions();
        assert!(tw <= 1000 && th <= 800, "thumb larger than source");
        assert!(tw.max(th) <= 320, "long edge {} exceeds 320", tw.max(th));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn uploaded_photo_appears_in_owner_timeline(pool: PgPool) {
        // Regression: an imported image must show up in the uploader's gallery
        // once the async import finishes.
        let st = shared(pool).await;
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes(640, 480));
        let (_code, accepted) = upload_raw(
            State(st.clone()),
            Extension(crate::auth::AuthUser("usr_alice".to_string())),
            Json(RawUploadBody {
                owner_id: "usr_alice".to_string(),
                album_id: None,
                files: vec![crate::models::RawUploadedFile {
                    filename: "imported.png".to_string(),
                    bytes: b64,
                }],
            }),
        )
        .await
        .expect("upload ok");
        let batch_id = accepted.0.batch_id.clone();

        // Wait for enrichment to finish, then the photo must exist in the timeline.
        let batch = await_import(&st, &batch_id).await;
        let new_id = batch.items[0].photo_id.clone().expect("photo created");

        let tl = get_timeline(State(st.clone()), Path("usr_alice".to_string()))
            .await
            .expect("timeline ok");
        let ids: Vec<&str> = tl.0.sections.iter().flat_map(|s| s.items.iter()).map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&new_id.as_str()), "uploaded photo missing from owner timeline");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn companion_jpg_arw_collapses_in_import(pool: PgPool) {
        // A JPG+ARW pair (same base name + capture date) collapses to ONE photo:
        // the JPG is the primary, the ARW item ends Done/Duplicate referencing the
        // SAME photo_id (merged as a companion). ARW carries no decodable image
        // bytes, so we give it the JPG's bytes purely so EXIF dims read the same
        // capture day (companion grouping keys on date + base name).
        let st = shared(pool).await;
        use base64::Engine as _;
        let jpg = base64::engine::general_purpose::STANDARD.encode(png_bytes(800, 600));
        let arw = jpg.clone();
        let (code, accepted) = upload_raw(
            State(st.clone()),
            Extension(crate::auth::AuthUser("usr_alice".to_string())),
            Json(RawUploadBody {
                owner_id: "usr_alice".to_string(),
                album_id: None,
                files: vec![
                    crate::models::RawUploadedFile {
                        filename: "IMG_9001.ARW".to_string(),
                        bytes: arw,
                    },
                    crate::models::RawUploadedFile {
                        filename: "IMG_9001.jpg".to_string(),
                        bytes: jpg,
                    },
                ],
            }),
        )
        .await
        .expect("upload ok");
        assert_eq!(code, StatusCode::ACCEPTED);
        let batch_id = accepted.0.batch_id.clone();
        let batch = await_import(&st, &batch_id).await;
        let items = &batch.items;
        let jpg_item = items.iter().find(|i| i.filename == "IMG_9001.jpg").unwrap();
        let arw_item = items.iter().find(|i| i.filename == "IMG_9001.ARW").unwrap();
        // JPG is the primary: Done/Ok with a photo_id.
        assert_eq!(jpg_item.stage, ImportStage::Done);
        assert_eq!(jpg_item.status, ImportStatus::Ok);
        let primary_id = jpg_item.photo_id.clone().expect("primary photo id");
        // ARW collapsed into the primary: Done/Duplicate, SAME photo_id.
        assert_eq!(arw_item.stage, ImportStage::Done);
        assert_eq!(arw_item.status, ImportStatus::Duplicate);
        assert_eq!(arw_item.photo_id.as_deref(), Some(primary_id.as_str()));
        assert!(arw_item.error.as_deref().unwrap_or("").contains("merged"));
        // Exactly one photo created, with the ARW as a downloadable companion.
        let photo = get_photo(State(st.clone()), Path(primary_id.clone()))
            .await
            .expect("primary photo")
            .0;
        assert_eq!(photo.filename, "IMG_9001.jpg");
        assert_eq!(photo.companions.len(), 1);
        assert_eq!(photo.companions[0].ext, "arw");
        // The kept companion is marked downloadable.
        assert!(photo.companions[0].downloadable);

        // The ARW companion bytes are retrievable via the download endpoint, with
        // a Content-Disposition filename matching the original .ARW filename.
        let resp = download_companion(
            State(st.clone()),
            Path((primary_id.clone(), "arw".to_string())),
        )
        .await
        .expect("companion download ok")
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let cd = resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .expect("content-disposition")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(cd, "attachment; filename=\"IMG_9001.ARW\"");
        // The served bytes equal the uploaded companion (the ARW carried the same
        // png bytes), streamed back from the storage backend.
        let kept = png_bytes(800, 600);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert_eq!(body.as_ref(), kept.as_slice());
        assert!(!body.is_empty());

        // An unknown extension is a 404.
        let err = download_companion(
            State(st.clone()),
            Path((primary_id.clone(), "cr2".to_string())),
        )
        .await
        .err()
        .expect("unknown ext 404");
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn patch_metadata_replace_sets_override_remove_clears_it(pool: PgPool) {
        // RFC 6902 JSON Patch on /api/photos/{id}/metadata: `replace`/`add` set an
        // override; `remove` clears it back to EXIF.
        let st = shared(pool).await;
        // Pick a seeded photo and give it a known EXIF city so `remove` is visible.
        let id = {
            let mut w = st.write().await;
            let id = w.photos.keys().next().cloned().expect("a seeded photo");
            let p = w.photos.get_mut(&id).unwrap();
            p.exif.city = Some("ExifTown".to_string());
            p.overrides = MetadataOverride::default();
            // Persist so the Postgres-first patch handler reads this EXIF back.
            w.persist_photo(&id).await;
            id
        };

        // `replace /title` + `replace /city` set the overrides.
        let ops: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": "/title", "value": "Sunset" },
            { "op": "replace", "path": "/city", "value": "Paris" }
        ]))
        .unwrap();
        let view = patch_photo_metadata(State(st.clone()), Path(id.clone()), Json(ops))
            .await
            .expect("patch ok");
        assert_eq!(view.0.title.as_deref(), Some("Sunset"));
        assert_eq!(view.0.city.as_deref(), Some("Paris"));
        assert_eq!(view.0.overrides.city.as_deref(), Some("Paris"));

        // `remove /city` clears the city override → effective falls back to EXIF.
        let ops: json_patch::Patch =
            serde_json::from_value(serde_json::json!([{ "op": "remove", "path": "/city" }]))
                .unwrap();
        let view = patch_photo_metadata(State(st.clone()), Path(id.clone()), Json(ops))
            .await
            .expect("remove ok");
        assert!(view.0.overrides.city.is_none(), "override cleared");
        assert_eq!(view.0.city.as_deref(), Some("ExifTown"), "back to EXIF");
        // The title override survived the second patch (only /city was removed).
        assert_eq!(view.0.title.as_deref(), Some("Sunset"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn patch_metadata_rejects_bad_patch(pool: PgPool) {
        // A type-mismatched value (title must be a string) is a 422.
        let st = shared(pool).await;
        let id = {
            let r = st.read().await;
            r.photos.keys().next().cloned().expect("a seeded photo")
        };
        let ops: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": "/title", "value": 123 }
        ]))
        .unwrap();
        let err = patch_photo_metadata(State(st.clone()), Path(id), Json(ops))
            .await
            .err()
            .expect("invalid patch rejected");
        assert_eq!(err, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn update_user_json_patch_replace_and_remove(pool: PgPool) {
        // PATCH /api/users/{id} as an RFC 6902 JSON Patch: replace a field, and
        // `remove /quota_mb` to clear the quota.
        let st = shared(pool).await;
        let uid = {
            let mut w = st.write().await;
            let uid = w.users.keys().next().cloned().expect("a seeded user");
            w.users.get_mut(&uid).unwrap().quota_mb = Some(500);
            uid
        };
        let ops: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": "/name", "value": "Renamed" },
            { "op": "remove", "path": "/quota_mb" }
        ]))
        .unwrap();
        let out = update_user(State(st.clone()), Path(uid.clone()), Json(ops))
            .await
            .expect("update ok");
        assert_eq!(out.0.name, "Renamed");
        assert!(out.0.quota_mb.is_none(), "quota cleared by remove");
    }

    // ---- Feature 3: admin stats ----

    #[sqlx::test(migrations = "./migrations")]
    async fn admin_stats_counts_match_seed(pool: PgPool) {
        let st = shared(pool).await;
        let stats = admin_stats(State(st.clone())).await;
        // 4 seed users, 3 seed albums, 2 seed groups.
        assert_eq!(stats.0.counts.users, 4);
        assert_eq!(stats.0.counts.albums, 3);
        assert_eq!(stats.0.counts.groups, 2);
        // 36 live (non-trashed) seed photos.
        assert_eq!(stats.0.counts.photos, 36);
        assert_eq!(stats.0.counts.vault, 2); // alice's vault has 2 photos
        // Every canonical job (see `jobs::JOB_NAMES`) is tracked in the stats.
        let names: Vec<&str> = stats.0.jobs.iter().map(|j| j.name.as_str()).collect();
        for n in crate::jobs::JOB_NAMES {
            assert!(names.contains(n), "missing job {n}");
        }
        assert_eq!(stats.0.storage.quota_mb, 512_000);
    }

    // ---- Feature 4: audit endpoint ----

    #[sqlx::test(migrations = "./migrations")]
    async fn audit_endpoint_passes_on_seed(pool: PgPool) {
        let st = shared(pool).await;
        let res = audit_access(State(st.clone())).await;
        assert!(res.0.pass, "violations: {:?}", res.0.violations);
        assert!(res.0.violations.is_empty());
    }

    // ---- FEATURE A: partner endpoints ----

    #[sqlx::test(migrations = "./migrations")]
    async fn add_and_remove_partner_endpoint(pool: PgPool) {
        let st = shared(pool).await;
        // Alice grants carol partner access.
        let updated = add_partner(
            State(st.clone()),
            Path("usr_alice".to_string()),
            Json(crate::models::AddPartner { partner_id: "usr_carol".to_string() }),
        )
        .await
        .expect("add ok");
        assert!(updated.0.partners.contains(&"usr_carol".to_string()));

        // Dedup: a second add doesn't duplicate.
        let again = add_partner(
            State(st.clone()),
            Path("usr_alice".to_string()),
            Json(crate::models::AddPartner { partner_id: "usr_carol".to_string() }),
        )
        .await
        .expect("add ok");
        assert_eq!(again.0.partners.iter().filter(|p| *p == "usr_carol").count(), 1);

        // self-grant -> 400.
        let bad = add_partner(
            State(st.clone()),
            Path("usr_alice".to_string()),
            Json(crate::models::AddPartner { partner_id: "usr_alice".to_string() }),
        )
        .await;
        assert_eq!(bad.unwrap_err(), StatusCode::BAD_REQUEST);

        // missing partner -> 404.
        let missing = add_partner(
            State(st.clone()),
            Path("usr_alice".to_string()),
            Json(crate::models::AddPartner { partner_id: "usr_nope".to_string() }),
        )
        .await;
        assert_eq!(missing.unwrap_err(), StatusCode::NOT_FOUND);

        // Remove revokes.
        let removed = remove_partner(
            State(st.clone()),
            Path(("usr_alice".to_string(), "usr_carol".to_string())),
        )
        .await
        .expect("remove ok");
        assert!(!removed.0.partners.contains(&"usr_carol".to_string()));
    }

    // ---- FEATURE B: duplicates endpoint ----

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicates_endpoint_returns_groups(pool: PgPool) {
        let st = shared(pool).await;
        // Inject two identical-thumbnail photos owned by alice, then run detection.
        {
            let mut w = st.write().await;
            let dup = png_bytes(64, 64);
            for id in ["ph_dupe1", "ph_dupe2"] {
                let mut p = w.photos.get("ph_0001").unwrap().clone();
                p.id = id.to_string();
                p.owner_id = "usr_alice".to_string();
                p.deleted_at = None;
                p.archived = false;
                w.photos.insert(id.to_string(), p);
                w.thumbs.insert(id.to_string(), (dup.clone(), "image/png".to_string()));
            }
            let found = w.detect_duplicates();
            assert_eq!(found, 2);
            // Persist the injected photos + detected groups so the Postgres-first
            // endpoint (which reads a fresh DB snapshot) sees them.
            w.persist_photo("ph_dupe1").await;
            w.persist_photo("ph_dupe2").await;
            w.persist_duplicates().await;
        }
        let res = get_duplicates(State(st.clone()), Path("usr_alice".to_string()))
            .await
            .expect("ok");
        assert_eq!(res.0.groups.len(), 1);
        assert_eq!(res.0.groups[0].len(), 2);

        // Unknown user -> 404.
        let err = get_duplicates(State(st.clone()), Path("usr_nope".to_string())).await;
        assert_eq!(err.unwrap_err(), StatusCode::NOT_FOUND);
    }

    // ---- Originals + screen-adapted render ----

    /// Drive a single-file PNG upload through the whole import pipeline and return
    /// the created photo id.
    async fn upload_one_png(st: &Shared, filename: &str, w: u32, h: u32) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes(w, h));
        let (_code, accepted) = upload_raw(
            State(st.clone()),
            Extension(crate::auth::AuthUser("usr_alice".to_string())),
            Json(RawUploadBody {
                owner_id: "usr_alice".to_string(),
                album_id: None,
                files: vec![crate::models::RawUploadedFile {
                    filename: filename.to_string(),
                    bytes: b64,
                }],
            }),
        )
        .await
        .expect("upload ok");
        let batch_id = accepted.0.batch_id.clone();
        let batch = await_import(st, &batch_id).await;
        batch.items[0].photo_id.clone().expect("photo created")
    }

    /// Collect a handler's `IntoResponse` body into bytes.
    async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body")
            .to_vec()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upload_stores_original_and_render_fits_box(pool: PgPool) {
        use axum::response::IntoResponse;
        let st = shared(pool).await;
        let src = png_bytes(1000, 800);
        let id = upload_one_png(&st, "orig.png", 1000, 800).await;

        // full_url is surfaced on the view (a stored original → renderable).
        let view = get_photo(State(st.clone()), Path(id.clone()))
            .await
            .expect("photo");
        assert_eq!(view.0.full_url.as_deref(), Some(format!("/api/photos/{id}/render").as_str()));

        // GET /original returns exactly the same bytes + content type.
        let resp = get_original(State(st.clone()), Path(id.clone()))
            .await
            .expect("original ok")
            .into_response();
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(body_bytes(resp).await, src);

        // GET /render?w=64 -> decoded width <= 64 AND <= the original width.
        let resp = render_photo(
            State(st.clone()),
            Path(id.clone()),
            Query(RenderQuery { w: Some(64), h: None, fmt: None, supports: None }),
        )
        .await
        .expect("render ok")
        .into_response();
        let out = body_bytes(resp).await;
        let decoded = image::load_from_memory(&out).expect("decode render");
        use image::GenericImageView;
        let (rw, _rh) = decoded.dimensions();
        assert!(rw <= 64, "render width {rw} exceeds requested 64");
        assert!(rw <= 1000, "render width {rw} exceeds original 1000 (upscaled!)");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn render_never_upscales_beyond_original(pool: PgPool) {
        use axum::response::IntoResponse;
        let st = shared(pool).await;
        let id = upload_one_png(&st, "small.png", 100, 80).await;
        // Ask for a 4000px box from a 100px-wide original: must NOT upscale.
        let resp = render_photo(
            State(st.clone()),
            Path(id.clone()),
            Query(RenderQuery { w: Some(4000), h: Some(4000), fmt: None, supports: None }),
        )
        .await
        .expect("render ok")
        .into_response();
        let out = body_bytes(resp).await;
        let decoded = image::load_from_memory(&out).expect("decode render");
        use image::GenericImageView;
        let (rw, rh) = decoded.dimensions();
        assert!(rw <= 100 && rh <= 80, "render {rw}x{rh} upscaled beyond 100x80");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn render_honors_explicit_fmt(pool: PgPool) {
        use axum::response::IntoResponse;
        let st = shared(pool).await;
        let id = upload_one_png(&st, "fmt.png", 200, 150).await;
        let resp = render_photo(
            State(st.clone()),
            Path(id.clone()),
            Query(RenderQuery { w: Some(100), h: None, fmt: Some(MediaFormat::Jpeg), supports: None }),
        )
        .await
        .expect("render ok")
        .into_response();
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn seed_photo_has_no_original_or_render(pool: PgPool) {
        // A seed photo has no stored original: /original and /render 404, and its
        // PhotoView.full_url is None.
        let st = shared(pool).await;
        let pid = {
            let r = st.read().await;
            assert!(r.photos.contains_key("ph_0001"));
            "ph_0001".to_string()
        };
        let view = get_photo(State(st.clone()), Path(pid.clone()))
            .await
            .expect("photo");
        assert!(view.0.full_url.is_none(), "seed photo must have no full_url");

        let err = get_original(State(st.clone()), Path(pid.clone())).await;
        assert!(matches!(err, Err(StatusCode::NOT_FOUND)));

        let err = render_photo(
            State(st.clone()),
            Path(pid.clone()),
            Query(RenderQuery { w: Some(64), h: None, fmt: None, supports: None }),
        )
        .await;
        assert!(matches!(err, Err(StatusCode::NOT_FOUND)));
    }

    // ---- Feature-flag helpers ----

    /// Flip one boolean under `/features/...` via the real `patch_settings`
    /// handler (RFC 6902), mirroring how the admin UI toggles a flag.
    async fn set_feature(st: &Shared, name: &str, value: bool) {
        let ops: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": format!("/features/{name}"), "value": value }
        ]))
        .expect("valid patch");
        let _ = patch_settings(State(st.clone()), Json(ops))
            .await
            .expect("patch settings");
    }

    // ---- Task 3: features.transcode gates VIDEO rendering ----

    /// Inject a VIDEO photo owned by alice with a stored original blob, returning
    /// its id. Reuses a seed photo's shape, overriding kind/filename and pushing
    /// raw bytes to the backend so `render_photo`'s video branch is reachable.
    async fn inject_video(st: &Shared, id: &str) -> String {
        let mut w = st.write().await;
        let mut p = w.photos.get("ph_0001").unwrap().clone();
        p.id = id.to_string();
        p.owner_id = "usr_alice".to_string();
        p.kind = "video".to_string();
        p.filename = format!("{id}.mp4");
        p.deleted_at = None;
        p.archived = false;
        // Bogus but non-empty bytes: the flag-gate returns 403 BEFORE ffmpeg, and
        // when enabled the ffmpeg-missing path returns 503 — neither decodes these.
        w.originals
            .insert(id.to_string(), (b"not-a-real-mp4".to_vec(), "video/mp4".to_string()));
        w.store_originals(&[id.to_string()]).await;
        w.photos.insert(id.to_string(), p);
        w.persist_photo(id).await;
        id.to_string()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn transcode_flag_off_blocks_video_render_403(pool: PgPool) {
        let st = shared(pool).await;
        let id = inject_video(&st, "ph_vid_off").await;
        set_feature(&st, "transcode", false).await;
        let err = render_photo(
            State(st.clone()),
            Path(id.clone()),
            Query(RenderQuery { w: Some(640), h: Some(360), fmt: None, supports: None }),
        )
        .await
        .err();
        // Disabled by flag → 403, distinct from a 5xx ffmpeg-missing failure.
        assert_eq!(err, Some(StatusCode::FORBIDDEN));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn transcode_flag_on_allows_video_path(pool: PgPool) {
        let st = shared(pool).await;
        let id = inject_video(&st, "ph_vid_on").await;
        set_feature(&st, "transcode", true).await;
        let res = render_photo(
            State(st.clone()),
            Path(id.clone()),
            Query(RenderQuery { w: Some(640), h: Some(360), fmt: None, supports: None }),
        )
        .await;
        // With the flag ON we PROCEED into the ffmpeg path. It must NOT be the
        // flag-disabled 403. In CI ffmpeg is typically absent → 503; if present it
        // would fail to decode our bogus bytes → 422; or (real video) succeed → 2xx.
        match res {
            Ok(_) => {}
            Err(code) => assert_ne!(
                code,
                StatusCode::FORBIDDEN,
                "flag is ON but render was blocked as if disabled"
            ),
        }
    }

    // ---- Task 4: features.public_signup gates POST /api/register ----

    #[sqlx::test(migrations = "./migrations")]
    async fn register_disabled_is_403(pool: PgPool) {
        let st = shared(pool).await;
        // Default flag is false (conservative); register must be forbidden.
        let err = register(
            State(st.clone()),
            Json(RegisterBody {
                name: "Newbie".to_string(),
                email: "newbie@photon.app".to_string(),
                password: "hunter2pass".to_string(),
            }),
        )
        .await
        .err();
        assert_eq!(err, Some(StatusCode::FORBIDDEN));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn register_enabled_creates_loginable_user(pool: PgPool) {
        let st = shared(pool).await;
        set_feature(&st, "public_signup", true).await;
        let (code, user) = register(
            State(st.clone()),
            Json(RegisterBody {
                name: "Newbie".to_string(),
                email: "newbie@photon.app".to_string(),
                password: "hunter2pass".to_string(),
            }),
        )
        .await
        .expect("registered");
        assert_eq!(code, StatusCode::CREATED);
        assert!(user.0.id.starts_with("usr_"));
        assert!(!user.0.is_admin);
        assert!(!user.0.disabled);

        // The new user can log in with the password set during registration.
        let (status, body) = do_login(
            &st,
            LoginBody {
                email: "newbie@photon.app".to_string(),
                password: "hunter2pass".to_string(),
                totp: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body["token"].as_str().unwrap().is_empty());
        assert_eq!(body["user"]["id"], user.0.id);

        // A duplicate email → 409 CONFLICT.
        let dup = register(
            State(st.clone()),
            Json(RegisterBody {
                name: "Imposter".to_string(),
                email: "NEWBIE@photon.app".to_string(),
                password: "another-pass".to_string(),
            }),
        )
        .await
        .err();
        assert_eq!(dup, Some(StatusCode::CONFLICT));
    }

    // ---- Task 5: features.public_links gates public album sharing ----

    /// Create an album owned by alice containing the given live photo ids.
    async fn make_album(st: &Shared, name: &str, photo_ids: Vec<String>) -> String {
        let (_c, album) = create_album(
            State(st.clone()),
            Extension(crate::auth::AuthUser("usr_alice".to_string())),
            Json(CreateAlbum {
                name: name.to_string(),
                owner_id: "usr_alice".to_string(),
                photo_ids,
            }),
        )
        .await
        .expect("album created");
        album.0.id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn public_links_disabled_mint_403_and_get_404(pool: PgPool) {
        let st = shared(pool).await;
        let album_id = make_album(&st, "Trip", vec![]).await;
        // Mint with the flag OFF → 403.
        let err = create_public_link(State(st.clone()), Path(album_id.clone()))
            .await
            .err();
        assert_eq!(err, Some(StatusCode::FORBIDDEN));
        // Public GET for a bogus token (flag off) → 404.
        let err = public_album(State(st.clone()), Path("deadbeef".to_string()))
            .await
            .err();
        assert_eq!(err, Some(StatusCode::NOT_FOUND));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn public_links_enabled_full_flow(pool: PgPool) {
        use axum::response::IntoResponse as _;
        let st = shared(pool).await;
        // A real, renderable photo + an unrelated photo NOT in the album.
        let in_album = upload_one_png(&st, "inside.png", 120, 90).await;
        let outsider = upload_one_png(&st, "outside.png", 60, 40).await;
        let album_id = make_album(&st, "Public Trip", vec![in_album.clone()]).await;

        set_feature(&st, "public_links", true).await;

        // Mint returns a token + url.
        let minted = create_public_link(State(st.clone()), Path(album_id.clone()))
            .await
            .expect("minted")
            .0;
        assert!(!minted.token.is_empty());
        assert_eq!(minted.url, format!("/api/public/albums/{}", minted.token));

        // Public GET returns the album + its live photos (no auth).
        let view = public_album(State(st.clone()), Path(minted.token.clone()))
            .await
            .expect("public album")
            .0;
        assert_eq!(view.album.id, album_id);
        assert_eq!(view.photos.len(), 1);
        assert_eq!(view.photos[0].id, in_album);

        // Public thumb serves bytes.
        let resp = public_album_thumb(
            State(st.clone()),
            Path((minted.token.clone(), in_album.clone())),
        )
        .await
        .expect("thumb served")
        .into_response();
        assert!(!body_bytes(resp).await.is_empty());

        // Public render serves bytes.
        let resp = public_album_render(
            State(st.clone()),
            Path((minted.token.clone(), in_album.clone())),
            Query(RenderQuery { w: Some(64), h: None, fmt: None, supports: None }),
        )
        .await
        .expect("render served")
        .into_response();
        assert!(!body_bytes(resp).await.is_empty());

        // A photo NOT in the album → 404 on both blob endpoints.
        let err = public_album_thumb(
            State(st.clone()),
            Path((minted.token.clone(), outsider.clone())),
        )
        .await
        .err();
        assert_eq!(err, Some(StatusCode::NOT_FOUND));
        let err = public_album_render(
            State(st.clone()),
            Path((minted.token.clone(), outsider.clone())),
            Query(RenderQuery { w: Some(64), h: None, fmt: None, supports: None }),
        )
        .await
        .err();
        assert_eq!(err, Some(StatusCode::NOT_FOUND));

        // Revoke removes the mapping → public GET 404 again.
        let code = revoke_public_link(
            State(st.clone()),
            Path((album_id.clone(), minted.token.clone())),
        )
        .await
        .expect("revoked");
        assert_eq!(code, StatusCode::NO_CONTENT);
        let err = public_album(State(st.clone()), Path(minted.token.clone()))
            .await
            .err();
        assert_eq!(err, Some(StatusCode::NOT_FOUND));
    }

    // ---- Targeted per-resource authorization (auth::authorize) ----

    /// Build the minimal [`StorageCtx`] from a `Shared`, matching what
    /// `auth_middleware` clones out before issuing its targeted authz queries.
    async fn authz_config(st: &Shared) -> crate::state::StorageCtx {
        st.read().await.storage_ctx()
    }

    /// The targeted `auth::authorize` helper (the core of `auth_middleware`) must
    /// reproduce `resource_authz`'s semantics for `/api/photos/{id}` using ONLY
    /// by-id queries + small grant collections — never a full DB snapshot:
    ///   * owner (a non-admin) → allowed for GET and PATCH;
    ///   * a different non-admin → FORBIDDEN for GET and PATCH;
    ///   * admin → allowed (bypass);
    ///   * a photo SHARED to the non-admin via an album shared-to-them → readable
    ///     (GET), but still NOT mutable (PATCH owner-only).
    #[sqlx::test(migrations = "./migrations")]
    async fn targeted_authz_photo_owner_nonowner_admin_and_share(pool: PgPool) {
        use crate::auth::authorize;
        use axum::http::Method;
        let st = shared(pool).await;
        let cfg = authz_config(&st).await;
        let pers = st.read().await.persistence.clone().expect("db");

        // `ph_0004` is a LIVE seed photo owned by the non-admin `usr_bob`.
        let owner = "usr_bob";
        let other = "usr_carol"; // a different non-admin
        let admin = "usr_alice"; // seed admin
        let pid = "ph_0004";
        {
            let p = pers.get_photo(pid).await.expect("q").expect("seed photo present");
            assert_eq!(p.owner_id, owner);
            assert!(p.deleted_at.is_none() && !p.archived, "must be live");
        }
        let get = Method::GET;
        let patch = Method::PATCH;
        let path = format!("/api/photos/{pid}");

        // Owner: full access, reads + mutations.
        assert!(authorize(&pers, &cfg, owner, false, &get, &path).await.is_ok());
        assert!(authorize(&pers, &cfg, owner, false, &patch, &path).await.is_ok());

        // Non-owner non-admin with NO grant: forbidden for both GET and PATCH.
        assert_eq!(
            authorize(&pers, &cfg, other, false, &get, &path).await,
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            authorize(&pers, &cfg, other, false, &patch, &path).await,
            Err(StatusCode::FORBIDDEN)
        );

        // Admin bypasses per-resource authz entirely.
        assert!(authorize(&pers, &cfg, admin, true, &get, &path).await.is_ok());
        assert!(authorize(&pers, &cfg, admin, true, &patch, &path).await.is_ok());

        // Now SHARE the photo to `other` via an album (owned by bob) shared-to-carol.
        let album = Album {
            id: "alb_shared_authz".to_string(),
            name: "Shared".to_string(),
            owner_id: owner.to_string(),
            cover_seed: 1,
            photo_ids: vec![pid.to_string()],
            shares: vec![Share {
                target: ShareTarget::User(other.to_string()),
                role: ShareRole::Viewer,
            }],
        };
        pers.upsert_album(&album).await.expect("album persisted");

        // The granted non-owner may now READ the live shared photo...
        assert!(authorize(&pers, &cfg, other, false, &get, &path).await.is_ok());
        // ...but a mutation stays owner-only.
        assert_eq!(
            authorize(&pers, &cfg, other, false, &patch, &path).await,
            Err(StatusCode::FORBIDDEN)
        );

        // A granted non-owner must NOT read a non-LIVE (archived) shared photo.
        let mut p = pers.get_photo(pid).await.expect("q").expect("present");
        p.archived = true;
        pers.upsert_photo(&p).await.expect("archive persisted");
        assert_eq!(
            authorize(&pers, &cfg, other, false, &get, &path).await,
            Err(StatusCode::FORBIDDEN)
        );
        // ...while the owner still reads their archived photo.
        assert!(authorize(&pers, &cfg, owner, false, &get, &path).await.is_ok());

        // Unknown ids / non-resource paths fall through to Ok (handler 404s).
        assert!(authorize(&pers, &cfg, other, false, &get, "/api/photos/ph_missing")
            .await
            .is_ok());
        assert!(authorize(&pers, &cfg, other, false, &get, "/api/photos")
            .await
            .is_ok());
    }

    /// End-to-end through `auth_middleware` (via an `axum::Router` with the real
    /// middleware layered on a 200 route): a real bearer session for a non-owner is
    /// 403'd on another user's photo, the owner is allowed, the admin is allowed,
    /// and a bogus token is 401'd — proving the MIDDLEWARE (not just the helper)
    /// enforces the boundary, including resolving sessions/users by targeted query.
    #[sqlx::test(migrations = "./migrations")]
    async fn auth_middleware_enforces_photo_ownership_e2e(pool: PgPool) {
        use axum::body::Body;
        use axum::extract::Request;
        use axum::http::Method;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt; // oneshot

        let st = shared(pool).await;
        // Mint + persist a session token per user so `pool.get_session` resolves it.
        async fn mk_token(st: &Shared, uid: &str) -> String {
            let tok = st.write().await.create_session(uid);
            st.read().await.persist_session(&tok).await;
            tok
        }
        let bob_tok = mk_token(&st, "usr_bob").await;
        let carol_tok = mk_token(&st, "usr_carol").await;
        let alice_tok = mk_token(&st, "usr_alice").await; // admin

        // A route that 200s with the SAME path the middleware guards; if the
        // middleware lets the request through, this handler answers 200.
        fn app(st: Shared) -> Router {
            Router::new()
                .route(
                    "/api/photos/{id}",
                    get(|| async { StatusCode::OK }).patch(|| async { StatusCode::OK }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    st.clone(),
                    crate::auth::auth_middleware,
                ))
                .with_state(st)
        }

        async fn call(st: &Shared, tok: &str, method: Method) -> StatusCode {
            let req = Request::builder()
                .method(method)
                .uri("/api/photos/ph_0004")
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .body(Body::empty())
                .unwrap();
            app(st.clone()).oneshot(req).await.unwrap().status()
        }

        // Owner bob: GET + PATCH allowed (200, handler reached).
        assert_eq!(call(&st, &bob_tok, Method::GET).await, StatusCode::OK);
        assert_eq!(call(&st, &bob_tok, Method::PATCH).await, StatusCode::OK);
        // Non-owner carol: GET + PATCH forbidden (403) before the handler.
        assert_eq!(call(&st, &carol_tok, Method::GET).await, StatusCode::FORBIDDEN);
        assert_eq!(call(&st, &carol_tok, Method::PATCH).await, StatusCode::FORBIDDEN);
        // Admin alice: allowed (200).
        assert_eq!(call(&st, &alice_tok, Method::GET).await, StatusCode::OK);
        // Bogus token: 401.
        assert_eq!(call(&st, "bogus_token", Method::GET).await, StatusCode::UNAUTHORIZED);
    }
}

/// Postgres-first integration tests. Each `#[sqlx::test]` gets its OWN isolated,
/// freshly-migrated database, so these exercise the REAL persistence path end to
/// end — proving the "1 HTTP request = 1 SQL transaction, Postgres is the single
/// source of truth" contract, including across two independent instances sharing
/// one database (the multi-node-behind-a-load-balancer case).
///
/// Requires a reachable Postgres: `DATABASE_URL` must point at a server where the
/// test user can CREATE/DROP databases (sqlx provisions an ephemeral DB per test).
#[cfg(test)]
mod pg_tests {
    use super::*;
    use crate::db::Persistence;
    use crate::state::seed;
    use sqlx::PgPool;

    /// Build a Photon "instance" (its own `AppState`/`Shared`) backed by `pool`.
    /// When `seed_db` is true the demo seed is written to the (empty, migrated) DB
    /// first; pass false for a second instance sharing the same database.
    async fn instance(pool: PgPool, seed_db: bool) -> Shared {
        let mut st = seed();
        st.persistence = Some(Persistence::from_pool(pool));
        if seed_db {
            st.persist_seed().await;
        }
        Arc::new(RwLock::new(st))
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    /// A user created on instance A is immediately readable on instance B — the
    /// write lands in Postgres and B reads it straight back. No in-memory sync.
    #[sqlx::test(migrations = "./migrations")]
    async fn create_user_on_a_is_visible_on_b(pool: PgPool) {
        let a = instance(pool.clone(), true).await;
        let b = instance(pool.clone(), false).await;

        let (code, created) = create_user(
            State(a.clone()),
            Json(crate::models::CreateUser {
                name: "Zoe".into(),
                email: "zoe@photon.app".into(),
                is_admin: false,
            }),
        )
        .await
        .expect("create_user on A");
        assert_eq!(code, StatusCode::CREATED);

        let got = get_user(State(b.clone()), Path(created.id.clone()))
            .await
            .expect("get_user on B");
        assert_eq!(got.0.email, "zoe@photon.app");
    }

    /// A session minted by logging in on instance A authenticates `/api/me` on
    /// instance B — the session row is in Postgres, resolved per request.
    #[sqlx::test(migrations = "./migrations")]
    async fn session_minted_on_a_is_valid_on_b(pool: PgPool) {
        let a = instance(pool.clone(), true).await;
        let b = instance(pool.clone(), false).await;

        let resp = login(
            State(a.clone()),
            Json(crate::models::LoginBody {
                email: "alice".into(),
                password: "alice".into(),
                totp: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let token = serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")["token"]
            .as_str()
            .expect("token")
            .to_string();

        let me_res = me(State(b.clone()), bearer(&token))
            .await
            .expect("me on B with A's token");
        assert_eq!(me_res.0.email, "alice@photon.app");
    }

    /// A metadata PATCH (RFC 6902) committed on instance A is visible to a read on
    /// instance B — one request, one transaction, durable in Postgres.
    #[sqlx::test(migrations = "./migrations")]
    async fn patch_on_a_is_visible_on_b(pool: PgPool) {
        let a = instance(pool.clone(), true).await;
        let b = instance(pool.clone(), false).await;

        // Pick any seeded photo.
        let pid = {
            let g = a.read().await;
            g.photos.keys().next().cloned().expect("seed has photos")
        };

        let patch: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": "/caption", "value": "hello from A" }
        ]))
        .unwrap();
        let _ = patch_photo_metadata(State(a.clone()), Path(pid.clone()), Json(patch))
            .await
            .expect("patch on A");

        let got = get_photo(State(b.clone()), Path(pid.clone()))
            .await
            .expect("get_photo on B");
        assert_eq!(got.0.caption.as_deref(), Some("hello from A"));
    }

    /// GET /api/photos/{id}/faces returns each face's box, with cluster identity
    /// for the OWNER and boxes-only for everyone else.
    #[sqlx::test(migrations = "./migrations")]
    async fn photo_faces_lists_boxes_and_owner_only_identity(pool: PgPool) {
        let st = instance(pool, true).await;
        let pid = {
            st.read()
                .await
                .photos
                .values()
                .find(|p| p.owner_id == "usr_alice")
                .map(|p| p.id.clone())
                .expect("alice has a seed photo")
        };
        // Detect a face on that photo, cluster it, name the person, persist.
        {
            let mut g = st.write().await;
            g.faces.insert(
                "face_test_1".to_string(),
                crate::models::Face {
                    id: "face_test_1".to_string(),
                    photo_id: pid.clone(),
                    owner_id: "usr_alice".to_string(),
                    bbox: [10.0, 20.0, 100.0, 120.0],
                    embedding: vec![1.0, 0.0, 0.0],
                    score: 0.97,
                    person_id: None,
                    ignored: false,
                    assigned_label: None,
                    confirmed: false,
                },
            );
            g.cluster_faces("usr_alice");
            let person_id = g
                .faces
                .get("face_test_1")
                .and_then(|f| f.person_id.clone())
                .expect("face was clustered into a person");
            g.name_person(&person_id, "Alice");
            g.persist_faces("usr_alice").await;
        }

        // OWNER: box + identity (named cluster).
        let owner = photo_faces(
            State(st.clone()),
            Extension(AuthUser("usr_alice".to_string())),
            Path(pid.clone()),
        )
        .await
        .expect("owner faces ok")
        .0;
        assert_eq!(owner.faces.len(), 1);
        assert_eq!(owner.faces[0].bbox, [10.0, 20.0, 100.0, 120.0]);
        assert_eq!(owner.faces[0].person_name.as_deref(), Some("Alice"));
        assert!(owner.faces[0].person_label.is_some());
        assert!(owner.source_width > 0 && owner.source_height > 0);

        // NON-OWNER: box only, NO cluster identity leaked.
        let other = photo_faces(
            State(st.clone()),
            Extension(AuthUser("usr_bob".to_string())),
            Path(pid.clone()),
        )
        .await
        .expect("non-owner faces ok")
        .0;
        assert_eq!(other.faces.len(), 1);
        assert!(other.faces[0].person_id.is_none());
        assert!(other.faces[0].person_name.is_none());
        assert!(other.faces[0].person_label.is_none());
    }
}
