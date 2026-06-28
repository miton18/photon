//! Authentication + coarse authorization enforced as an axum middleware layer.
//!
//! Every protected route runs through [`auth_middleware`], which resolves the
//! caller's session to an actor (user id) from — in order — an
//! `Authorization: Bearer <token>` header, a `?token=`/`?access_token=` query
//! parameter (so plain `<img src>` requests for thumbnails/renders can carry
//! auth), or a `photon_session` cookie. Missing/invalid sessions get `401`.
//!
//! On top of authentication it applies coarse, path-based AUTHORIZATION:
//!   * `/api/users/{id}/…` and `/api/users/{id}` — the actor must BE `{id}` or be
//!     an admin (a user can only touch their own timeline/vault/search/people/…).
//!   * `/api/admin/…`, `/api/audit/…`, `/api/smtp`, `/api/storage…`, `POST
//!     /api/invites`, `POST /api/users` — admin only.
//! Finer per-resource ownership (e.g. who may rename a given Person cluster) is
//! still enforced inside the individual handlers.

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::Response,
};

use crate::handlers::Shared;
use crate::state::AppState;

/// The authenticated caller, injected into request extensions by
/// [`auth_middleware`]. Handlers that must scope by the caller (list endpoints,
/// uploads, album/group creation, contribution) read it via `Extension<AuthUser>`
/// instead of trusting an id from the path or body.
#[derive(Clone, Debug)]
pub struct AuthUser(pub String);

/// Pull a session token from the request: bearer header, then `?token=` /
/// `?access_token=` query param.
///
/// Auth is token-in-`Authorization`-header (not ambient cookies), which is
/// structurally CSRF-safe. We deliberately do NOT read a session cookie: the server
/// never sets one, and reading one would reintroduce a CSRF surface with no
/// `SameSite`/anti-CSRF defense. (F8)
fn extract_token(req: &Request) -> Option<String> {
    // 1. Authorization: Bearer <token>
    if let Some(raw) = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(tok) = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer ")) {
            let tok = tok.trim();
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
    }
    // 2. ?token= / ?access_token= (for <img>/<a download> which can't set headers).
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if (k == "token" || k == "access_token") && !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Coarse path-based authorization. Returns `Ok` if `actor` (admin = `is_admin`)
/// may call `method path`, else the status to reject with. Authentication has
/// already happened; this is purely about WHICH authenticated user may proceed.
pub fn path_authz(method: &Method, path: &str, actor: &str, is_admin: bool) -> Result<(), StatusCode> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    // Helper: admin gate.
    let admin_only = || if is_admin { Ok(()) } else { Err(StatusCode::FORBIDDEN) };

    match seg.as_slice() {
        // Server-wide admin surfaces.
        ["api", "admin", ..] => admin_only(),
        ["api", "audit", ..] => admin_only(),
        ["api", "smtp", ..] => admin_only(),
        ["api", "storage", ..] => admin_only(),
        ["api", "settings", ..] => admin_only(),
        // Inviting users + creating users are admin actions. (Listing the user
        // directory — GET /api/users — is allowed to any signed-in user so the
        // sharing pickers work.)
        ["api", "invites", ..] => admin_only(),
        ["api", "users"] if method == Method::POST => admin_only(),
        // Editing or deleting a user RECORD is an admin action (a non-admin must
        // not be able to self-promote to admin or un-disable themselves via
        // `PATCH /api/users/{id}`). Matches exactly `/api/users/{id}`.
        ["api", "users", _] if *method == Method::PATCH || *method == Method::DELETE => admin_only(),
        // Minting a password-reset token + email is an admin action. (Self-service
        // reset uses the PUBLIC `POST /api/users/{id}/password` token flow.) This
        // closes the bypass where a signed-in user self-issues a reset to skip the
        // current-password requirement.
        ["api", "users", _, "reset"] => admin_only(),
        // Per-user data (GET /api/users/{id} + all subpaths): self or admin.
        ["api", "users", id, ..] => {
            if is_admin || *id == actor {
                Ok(())
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        }
        // Everything else: authenticated access is enough; per-RESOURCE ownership
        // (photos/albums/groups/people/import batches) is enforced by
        // [`resource_authz`], and list endpoints scope inside their handlers.
        _ => Ok(()),
    }
}

fn is_read(m: &Method) -> bool {
    m == Method::GET || m == Method::HEAD
}

/// Per-resource ownership check for routes that carry a resource id in the path
/// (photos, albums, groups, people, import batches). Runs AFTER [`path_authz`],
/// with read access to state so it can resolve owners / share grants. A missing
/// resource returns `Ok` so the handler can answer `404`. Admins bypass all
/// checks. List endpoints (no id) fall through to `Ok` and scope in-handler.
pub fn resource_authz(
    st: &AppState,
    method: &Method,
    path: &str,
    actor: &str,
    is_admin: bool,
) -> Result<(), StatusCode> {
    if is_admin {
        return Ok(());
    }
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let forbid = || Err(StatusCode::FORBIDDEN);
    match seg.as_slice() {
        // Photos by id: reads need a legitimate grant; mutations need ownership.
        ["api", "photos", id, ..] => match st.photos.get(*id) {
            None => Ok(()),
            Some(p) => {
                if !is_read(method) {
                    // Mutations are owner-only.
                    return if p.owner_id == actor { Ok(()) } else { forbid() };
                }
                // Reads: the owner may fetch their own photo in any state; a
                // granted NON-owner (share/partner) may only read LIVE photos —
                // never trashed/archived/vaulted (mirrors timeline/search scope).
                if p.owner_id == actor {
                    Ok(())
                } else if st.allowed(actor, id)
                    && p.deleted_at.is_none()
                    && !p.archived
                    && !st.is_in_any_vault(id)
                {
                    Ok(())
                } else {
                    forbid()
                }
            }
        },
        // Albums by id: owner-or-shared to read; contribute needs contributor;
        // every other mutation (delete, add photos, shares) is owner-only.
        ["api", "albums", id, rest @ ..] => match st.albums.get(*id) {
            None => Ok(()),
            Some(a) => {
                let is_owner = a.owner_id == actor;
                match rest {
                    ["contribute"] if *method == Method::POST => {
                        if is_owner || st.can_contribute(actor, id) { Ok(()) } else { forbid() }
                    }
                    [] if is_read(method) => {
                        if is_owner || st.album_shared_to(a, actor) { Ok(()) } else { forbid() }
                    }
                    _ => if is_owner { Ok(()) } else { forbid() },
                }
            }
        },
        // Groups by id: owner or member to read; owner-only to mutate.
        ["api", "groups", id, ..] => match st.groups.get(*id) {
            None => Ok(()),
            Some(g) => {
                let is_owner = g.owner_id == actor;
                if is_read(method) {
                    if is_owner || g.member_ids.iter().any(|m| m == actor) { Ok(()) } else { forbid() }
                } else if is_owner {
                    Ok(())
                } else {
                    forbid()
                }
            }
        },
        // People (face clusters) by id: owner-only for name/photos/relationships.
        ["api", "people", pid, ..] => match st.people.get(*pid) {
            None => Ok(()),
            Some(p) => if p.owner_id == actor { Ok(()) } else { forbid() },
        },
        // Import batch polling: only the batch's owner may read its progress.
        // (`/api/uploads/raw` is the create endpoint, not a batch id — allow it.)
        ["api", "uploads", bid] if *bid != "raw" => match st.imports.get(*bid) {
            None => Ok(()),
            Some(b) => if b.owner_id == actor { Ok(()) } else { forbid() },
        },
        _ => Ok(()),
    }
}

/// Does `path` carry a guarded per-resource id (the shapes [`resource_authz`]
/// inspects)? If so, return `(kind, id)`; otherwise `None` so the caller can skip
/// any resource fetch and authorize by [`path_authz`] alone. Mirrors
/// `resource_authz`'s `match` arms exactly so the two never diverge on WHICH
/// requests are resource-guarded.
fn guarded_resource(path: &str) -> Option<(&'static str, String)> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match seg.as_slice() {
        ["api", "photos", id, ..] => Some(("photo", id.to_string())),
        ["api", "albums", id, ..] => Some(("album", id.to_string())),
        ["api", "groups", id, ..] => Some(("group", id.to_string())),
        ["api", "people", pid, ..] => Some(("person", pid.to_string())),
        ["api", "uploads", bid] if *bid != "raw" => Some(("import", bid.to_string())),
        _ => None,
    }
}

/// TARGETED per-resource authorization for the request path. This replaces the
/// old "load the ENTIRE DB into an [`AppState`] snapshot per request, then call
/// [`resource_authz`]" approach with two tiers:
///
///   * **Owner / admin fast-path** (the common case — every owner timeline,
///     thumbnail, render): at most two targeted row reads (`get_user` already
///     happened in [`auth_middleware`]; here a single `get_<resource>` by id).
///     The owner is authorized WITHOUT loading any collection.
///   * **Non-owner case** (shares/partners/groups/vaults): only then do we load
///     the SMALL grant collections (`users`, `albums`, `groups`, `vaults`) into a
///     minimal config-only [`AppState`], insert the single fetched target, and
///     hand off to the EXISTING [`resource_authz`] so the grant/share/vault logic
///     runs verbatim. We NEVER load photos/faces/embeddings/duplicate-groups/
///     import-batches.
///
/// Semantics are identical to running `resource_authz` against a full snapshot:
/// a missing target ⇒ `Ok` (handler answers 404); admins bypass; the non-owner
/// photo LIVE-only rule and the fail-open import-batch arm are preserved.
pub async fn authorize(
    pool: &crate::db::Persistence,
    config: &AppState,
    actor: &str,
    is_admin: bool,
    method: &Method,
    path: &str,
) -> Result<(), StatusCode> {
    // Admins bypass all per-resource checks (matches `resource_authz`).
    if is_admin {
        return Ok(());
    }
    let (kind, id) = match guarded_resource(path) {
        Some(r) => r,
        // Not a resource-guarded shape ⇒ `path_authz` already decided; allow.
        None => return Ok(()),
    };

    // Build a minimal config-only `AppState` carrying just what `resource_authz`
    // and its helpers (`allowed`/`is_in_any_vault`/…) read. The target resource
    // (and, only for non-owner cases, the small grant collections) are inserted
    // before the hand-off.
    let mut min = AppState::default();
    min.data_dir = config.data_dir.clone();
    min.storage = config.storage.clone();
    min.password_secret = config.password_secret.clone();

    match kind {
        "photo" => {
            let p = match pool.get_photo(&id).await.map_err(db_err)? {
                None => return Ok(()), // absent ⇒ handler 404s
                Some(p) => p,
            };
            // Owner fast-path: full access in any state, reads + mutations.
            if p.owner_id == actor {
                return Ok(());
            }
            // Non-owner: the grant/partner/album/vault logic must run. Load the
            // SMALL collections only (never the full photo/face working set).
            min.users = load_map(pool.load_users().await.map_err(db_err)?, |u| u.id.clone());
            min.albums = load_map(pool.load_albums().await.map_err(db_err)?, |a| a.id.clone());
            min.vaults = pool.load_vaults().await.map_err(db_err)?.into_iter().collect();
            min.photos.insert(p.id.clone(), p);
        }
        "album" => {
            let a = match pool.get_album(&id).await.map_err(db_err)? {
                None => return Ok(()),
                Some(a) => a,
            };
            // Owner is always authorized for every album arm (read, contribute,
            // and every other mutation are owner-allowed in `resource_authz`).
            if a.owner_id == actor {
                return Ok(());
            }
            // Non-owner: read needs a share; contribute needs a Contributor share;
            // any other mutation is owner-only ⇒ forbidden. `resource_authz` reads
            // `groups` (group-target shares) and `users` is harmless to include.
            min.groups = load_map(pool.load_groups().await.map_err(db_err)?, |g| g.id.clone());
            min.albums.insert(a.id.clone(), a);
        }
        "group" => {
            let g = match pool.get_group(&id).await.map_err(db_err)? {
                None => return Ok(()),
                Some(g) => g,
            };
            // Owner is authorized for read + mutate.
            if g.owner_id == actor {
                return Ok(());
            }
            // Non-owner: read needs membership; mutate is owner-only. The single
            // fetched group carries its own `member_ids`, so no collection needed.
            min.groups.insert(g.id.clone(), g);
        }
        "person" => {
            let p = match pool.get_person(&id).await.map_err(db_err)? {
                None => return Ok(()),
                Some(p) => p,
            };
            // People are owner-only for everything; non-owner ⇒ forbidden via the
            // verbatim `resource_authz` arm. The single fetched person suffices.
            min.people.insert(p.id.clone(), p);
        }
        "import" => {
            // Import-batch arm FAILS OPEN here (the batch is ephemeral, not in
            // Postgres); `get_import` enforces ownership at the handler. Preserve
            // that: no `imports` insert ⇒ `resource_authz` sees `None` ⇒ `Ok`.
        }
        _ => return Ok(()),
    }

    // Run the EXISTING grant/share/vault logic verbatim against the minimal state.
    resource_authz(&min, method, path, actor, is_admin)
}

/// Collect a loaded `Vec<T>` into a keyed `HashMap` using `key`.
fn load_map<T, F: Fn(&T) -> String>(items: Vec<T>, key: F) -> std::collections::HashMap<String, T> {
    items.into_iter().map(|i| (key(&i), i)).collect()
}

/// Map a Postgres error during authorization to a 500 (fail CLOSED: an authz
/// query that errors must never silently allow the request).
fn db_err(_e: sqlx::Error) -> StatusCode {
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Axum middleware: authenticate the caller and apply [`path_authz`] +
/// [`authorize`]. CORS preflight (`OPTIONS`) is always allowed through
/// unauthenticated. Because the per-user authz check ties the actor to the `{id}`
/// path segment, the downstream handlers' existing use of that path id is a
/// TRUSTED identity.
pub async fn auth_middleware(
    State(st): State<Shared>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if req.method() == Method::OPTIONS {
        return Ok(next.run(req).await);
    }
    let token = extract_token(&req).ok_or(StatusCode::UNAUTHORIZED)?;

    // POSTGRES-FIRST, but TARGETED: authenticate + authorize with a handful of
    // by-id queries instead of loading the ENTIRE DB into a per-request snapshot.
    // Clone the cheap config (pool handle + secret + storage/data_dir) out of the
    // shared state under a brief read lock, then release it before issuing DB I/O.
    let (pool, config) = {
        let guard = st.read().await;
        let pool = guard.persistence.clone().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let mut config = AppState::default();
        config.data_dir = guard.data_dir.clone();
        config.storage = guard.storage.clone();
        config.password_secret = guard.password_secret.clone();
        (pool, config)
    };

    // Actor: targeted `sessions` lookup, then targeted `users` lookup.
    let actor = pool.get_session(&token).await.map_err(db_err)?.ok_or(StatusCode::UNAUTHORIZED)?;
    let user = pool.get_user(&actor).await.map_err(db_err)?.ok_or(StatusCode::UNAUTHORIZED)?;
    // A disabled account's existing session must stop working immediately.
    if user.disabled {
        return Err(StatusCode::FORBIDDEN);
    }
    let is_admin = user.is_admin;
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Coarse path authz (pure), then targeted per-resource authz.
    path_authz(&method, &path, &actor, is_admin)?;
    authorize(&pool, &config, &actor, is_admin, &method, &path).await?;

    // Hand the verified caller to downstream handlers that scope by actor.
    req.extensions_mut().insert(AuthUser(actor));
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req(uri: &str) -> Request {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[test]
    fn extract_token_reads_header_and_query_but_not_cookie() {
        // Bearer header.
        let r = Request::builder()
            .uri("/api/photos")
            .header(header::AUTHORIZATION, "Bearer tok_hdr")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&r).as_deref(), Some("tok_hdr"));
        // Query param (for <img>/<a download>).
        assert_eq!(extract_token(&req("/api/photos/p1/render?w=800&token=tok_q")).as_deref(), Some("tok_q"));
        assert_eq!(extract_token(&req("/x?access_token=tok_a")).as_deref(), Some("tok_a"));
        // A `photon_session` cookie is DELIBERATELY ignored (CSRF hardening, F8):
        // the server never sets one, so reading it would only add an attack surface.
        let r = Request::builder()
            .uri("/api/photos")
            .header(header::COOKIE, "theme=dark; photon_session=tok_ck")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&r), None);
        // None present.
        assert_eq!(extract_token(&req("/api/photos")), None);
    }

    #[test]
    fn per_user_paths_require_matching_actor_or_admin() {
        let get = Method::GET;
        // Alice may read her own timeline; not bob's.
        assert!(path_authz(&get, "/api/users/usr_alice/timeline", "usr_alice", false).is_ok());
        assert_eq!(
            path_authz(&get, "/api/users/usr_bob/timeline", "usr_alice", false),
            Err(StatusCode::FORBIDDEN)
        );
        // An admin may read anyone's.
        assert!(path_authz(&get, "/api/users/usr_bob/vault", "usr_admin", true).is_ok());
    }

    #[test]
    fn admin_surfaces_are_admin_only() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(path_authz(&get, "/api/admin/stats", "usr_bob", false), Err(StatusCode::FORBIDDEN));
        assert!(path_authz(&get, "/api/admin/stats", "usr_admin", true).is_ok());
        assert_eq!(path_authz(&get, "/api/storage", "usr_bob", false), Err(StatusCode::FORBIDDEN));
        assert_eq!(path_authz(&post, "/api/invites", "usr_bob", false), Err(StatusCode::FORBIDDEN));
        // Creating a user is admin-only; listing users is open to any signed-in user.
        assert_eq!(path_authz(&post, "/api/users", "usr_bob", false), Err(StatusCode::FORBIDDEN));
        assert!(path_authz(&get, "/api/users", "usr_bob", false).is_ok());
    }

    #[test]
    fn ordinary_routes_allow_any_authenticated_user() {
        let get = Method::GET;
        let post = Method::POST;
        assert!(path_authz(&get, "/api/photos", "usr_bob", false).is_ok());
        assert!(path_authz(&post, "/api/albums", "usr_bob", false).is_ok());
        assert!(path_authz(&get, "/api/people/person_1/photos", "usr_bob", false).is_ok());
    }

    #[test]
    fn user_record_edits_are_admin_only() {
        let patch = Method::PATCH;
        let del = Method::DELETE;
        let get = Method::GET;
        // A user may NOT PATCH/DELETE even their own record (blocks self-promotion).
        assert_eq!(path_authz(&patch, "/api/users/usr_bob", "usr_bob", false), Err(StatusCode::FORBIDDEN));
        assert_eq!(path_authz(&del, "/api/users/usr_bob", "usr_bob", false), Err(StatusCode::FORBIDDEN));
        // Admins can; and a user can still GET their own record + subpaths.
        assert!(path_authz(&patch, "/api/users/usr_bob", "usr_admin", true).is_ok());
        assert!(path_authz(&get, "/api/users/usr_bob", "usr_bob", false).is_ok());
    }

    #[test]
    fn resource_authz_enforces_photo_ownership() {
        let st = crate::state::seed();
        // Any seeded photo + a user who is not its owner.
        let (pid, owner) = st
            .photos
            .values()
            .map(|p| (p.id.clone(), p.owner_id.clone()))
            .next()
            .expect("seed has photos");
        let other = st
            .users
            .keys()
            .find(|u| **u != owner)
            .expect("a second user")
            .clone();
        let del = Method::DELETE;
        let path = format!("/api/photos/{pid}");

        // Mutation is owner-only, regardless of any share/partner grants.
        assert!(resource_authz(&st, &del, &path, &owner, false).is_ok());
        assert_eq!(resource_authz(&st, &del, &path, &other, false), Err(StatusCode::FORBIDDEN));
        // Admins bypass; unknown photo ids fall through to the handler (Ok → 404).
        assert!(resource_authz(&st, &del, &path, &other, true).is_ok());
        assert!(resource_authz(&st, &del, "/api/photos/ph_missing", &other, false).is_ok());
    }

    #[test]
    fn resource_authz_hides_nonlive_photos_from_granted_nonowner() {
        let st0 = crate::state::seed();
        // A LIVE photo (not trashed/archived/vaulted) so the "while live" case is
        // deterministic regardless of HashMap iteration order.
        let (pid, owner) = st0
            .photos
            .values()
            .filter(|p| p.deleted_at.is_none() && !p.archived && !st0.is_in_any_vault(&p.id))
            .map(|p| (p.id.clone(), p.owner_id.clone()))
            .next()
            .expect("seed has a live photo");
        let mut st = st0;
        let other = st.users.keys().find(|u| **u != owner).expect("2nd user").clone();
        // Grant `other` partner access to `owner`'s live photos.
        st.users.get_mut(&owner).unwrap().partners.push(other.clone());
        let get = Method::GET;
        let path = format!("/api/photos/{pid}");
        // Live: the granted non-owner may read it.
        assert!(resource_authz(&st, &get, &path, &other, false).is_ok());
        // Archived: granted non-owner is blocked, but the owner still reads it.
        st.photos.get_mut(&pid).unwrap().archived = true;
        assert_eq!(resource_authz(&st, &get, &path, &other, false), Err(StatusCode::FORBIDDEN));
        assert!(resource_authz(&st, &get, &path, &owner, false).is_ok());
    }

    #[test]
    fn password_reset_is_admin_only() {
        let post = Method::POST;
        assert_eq!(
            path_authz(&post, "/api/users/usr_bob/reset", "usr_bob", false),
            Err(StatusCode::FORBIDDEN)
        );
        assert!(path_authz(&post, "/api/users/usr_bob/reset", "usr_admin", true).is_ok());
    }
}
