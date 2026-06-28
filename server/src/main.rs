mod analyze;
mod auth;
mod db;
mod jobs;
mod dlna;
mod extract;
mod handlers;
mod mailer;
mod mcp;
mod ml;
mod models;
mod oidc;
mod plugins;
mod state;
mod storage;
mod transcode;
mod webauthn;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::{HeaderName, HeaderValue, Method, header},
    routing::{any, delete, get, patch, post, put},
};
use serde::Serialize;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use handlers::Shared;

/// Tracing span for an HTTP request that records the method + PATH only (never the
/// query string), so credentials passed as `?token=`/`?access_token=` are never
/// logged. Generic over the body so it satisfies `tower_http`'s `MakeSpan`.
fn request_span<B>(req: &axum::http::Request<B>) -> tracing::Span {
    tracing::info_span!("request", method = %req.method(), path = %req.uri().path())
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        service: "photon-server",
    })
}

/// Explicit CORS allow-list for the dev UI origins. Production origins should be
/// added to this list (do NOT fall back to `CorsLayer::permissive()`).
fn cors_layer() -> CorsLayer {
    let origins: Vec<HeaderValue> = [
        "http://localhost:5173",
        "http://localhost:8080",
        "http://127.0.0.1:5173",
    ]
    .iter()
    .filter_map(|o| o.parse().ok())
    .collect();

    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
}

/// Attach static security headers to every response. These are constant API
/// headers (no per-request logic), layered via `SetResponseHeaderLayer`.
fn security_headers(router: Router<Shared>) -> Router<Shared> {
    const CSP: &str = "default-src 'self'; img-src 'self' data: https:; \
                       style-src 'self' 'unsafe-inline'; script-src 'self'";
    let headers: [(HeaderName, &'static str); 5] = [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, CSP),
        (
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000",
        ),
    ];
    let mut router = router;
    for (name, value) in headers {
        router = router.layer(SetResponseHeaderLayer::overriding(
            name,
            HeaderValue::from_static(value),
        ));
    }
    router
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=debug".into()),
        )
        .init();

    // ---- Build initial state, honoring env config ----
    let mut initial = state::seed();

    // argon2id server-wide secret key for PASSWORDS and vault PINs (env
    // PHOTON_PASSWORD_SALT). A documented dev default is used when unset (insecure
    // — production MUST set it). Vault PINs reuse this same secret with a per-vault
    // CSPRNG salt.
    match std::env::var("PHOTON_PASSWORD_SALT") {
        Ok(s) if !s.is_empty() => {
            initial.password_secret = s.into_bytes();
        }
        _ => {
            tracing::warn!(
                "PHOTON_PASSWORD_SALT is unset; using the INSECURE dev-default password secret. \
                 Set PHOTON_PASSWORD_SALT in production."
            );
        }
    }

    // LocalFs object-store root (env PHOTON_DATA_DIR, default "data").
    if let Ok(dir) = std::env::var("PHOTON_DATA_DIR") {
        if !dir.is_empty() {
            initial.data_dir = dir;
        }
    }

    // CONTEXT RECOGNITION (CLIP): wire the ML embedding sidecar client from
    // PHOTON_ML_URL. When unset (offline default) this stays `None`: no network
    // is used and semantic search/embedding are silently disabled.
    initial.ml = ml::MlClient::from_env();
    match &initial.ml {
        Some(_) => tracing::info!(
            "context recognition: CLIP sidecar enabled (PHOTON_ML_URL set)"
        ),
        None => tracing::info!(
            "context recognition: disabled (PHOTON_ML_URL unset; keyword search only)"
        ),
    }

    // WEBAUTHN / PASSKEYS: build the relying-party instance from
    // PHOTON_RP_ID/PHOTON_RP_ORIGIN (localhost defaults). `None` ⇒ passkeys are
    // disabled and every passkey route degrades gracefully.
    initial.webauthn = webauthn::build_webauthn();
    match &initial.webauthn {
        Some(_) => tracing::info!("passkeys: WebAuthn enabled (set PHOTON_RP_ID/PHOTON_RP_ORIGIN for prod)"),
        None => tracing::info!("passkeys: disabled (invalid RP config)"),
    }

    // NOTE: subprocess plugins are launched LATER (after the DB is connected) so
    // the host can mint the plugin service-account callback token in Postgres.

    // OIDC WEB LOGIN (relying-party / authorization-code flow). Inert unless
    // OIDC_ISSUER + OIDC_CLIENT_ID/SECRET + OIDC_REDIRECT_URI are all set AND the
    // issuer's discovery document resolves. `None` ⇒ `/api/auth/oidc/*` is off and
    // the UI's "Continue with OpenID" button stays hidden (no IdP is contacted).
    initial.oidc_login = oidc::OidcLogin::from_env().await;
    match &initial.oidc_login {
        Some(_) => tracing::info!("OIDC web login: enabled (OIDC_* configured + discovery ok)"),
        None => tracing::info!(
            "OIDC web login: disabled (OIDC_* unset/incomplete or discovery failed)"
        ),
    }

    // Postgres is MANDATORY: it is the single source of truth. Every request reads
    // and writes it directly (1 HTTP request = 1 SQL transaction); there is no
    // in-memory mode. Refuse to start without a working DATABASE_URL.
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            tracing::error!(
                "DATABASE_URL is required: Photon stores all domain data in Postgres. \
                 Set DATABASE_URL (e.g. postgres://user:pass@host:5432/photon) and retry."
            );
            std::process::exit(1);
        }
    };
    match db::Persistence::connect(&database_url).await {
        Ok(p) => {
            let empty = p.is_empty().await.unwrap_or(true);
            initial.persistence = Some(p);
            if empty {
                tracing::info!("postgres: empty DB, persisting seed");
                initial.persist_seed().await;
            } else {
                tracing::info!("postgres: existing DB");
            }
            tracing::info!("persistence: postgres (source of truth, per-request transactions)");
        }
        Err(e) => {
            tracing::error!("DATABASE_URL set but connection/migration failed: {e}");
            std::process::exit(1);
        }
    }

    // SUBPROCESS PLUGINS (go-plugin style): launch + handshake every binary in
    // PHOTON_PLUGINS_DIR over a gRPC Unix socket and register the jobs/routes/ops
    // they declare. Done HERE (after the DB is up) so the host can mint a callback
    // token for the plugin service account and inject it + our base URL into each
    // child's environment. When PHOTON_PLUGINS_DIR is unset (offline default) this
    // stays `None`: no child is launched, no token minted (mirrors the ML sidecar).
    let port = std::env::var("PHOTON_PORT").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "3000".to_string());
    let api_base_url = std::env::var("PHOTON_PUBLIC_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    initial.plugins = crate::plugins::PluginHost::from_env(&initial, api_base_url).await;
    match &initial.plugins {
        Some(host) => {
            let names = host.plugin_names().await;
            tracing::info!("plugins: enabled ({} discovered: [{}])", names.len(), names.join(", "));
        }
        None => tracing::info!("plugins: disabled (PHOTON_PLUGINS_DIR unset)"),
    }

    let shared: Shared = Arc::new(RwLock::new(initial));

    // ---- Durable job queue (graphile_worker) in DB mode ----
    // When Postgres is configured, start the worker: background tasks (import
    // enrichment, trash purge, S3 backup, AI-analysis, duplicate detection) become
    // durable, retried jobs processed by ANY instance (cron claimed once across the
    // cluster). The in-process tokio interval jobs below are then SKIPPED so work
    // isn't duplicated per instance. Without a DB, we fall back to those intervals.
    let durable_jobs = {
        let has_db = { shared.read().await.persistence.is_some() };
        match (has_db, std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty())) {
            (true, Some(url)) => match jobs::start_worker(shared.clone(), &url).await {
                Ok(utils) => {
                    shared.write().await.worker_utils = Some(utils);
                    tracing::info!("graphile_worker started (durable background jobs + cron)");
                    true
                }
                Err(e) => {
                    tracing::error!("graphile_worker failed to start ({e}); using inline jobs");
                    false
                }
            },
            _ => false,
        }
    };

    // ===== Scheduled PLUGIN jobs (NON-durable fallback) =====
    // When the durable queue is up, scheduled plugin jobs are registered as
    // graphile CRON entries inside `start_worker` (claimed once across the
    // cluster — multi-instance safe). Only WITHOUT a durable queue do we fall back
    // to a per-instance tokio interval here (same `run_named(.., "cron")` path, so
    // it still lands in the JobRun history). The first immediate tick is consumed
    // so we don't fire every scheduled job at startup.
    if !durable_jobs {
        if let Some(host) = { shared.read().await.plugins.clone() } {
            for (job, secs) in host.scheduled_jobs().await {
                let shared = shared.clone();
                tracing::info!("plugins: scheduling job {job} every {secs}s (inline interval)");
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_secs(secs as u64));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    tick.tick().await; // consume the immediate first tick
                    loop {
                        tick.tick().await;
                        jobs::run_named(&shared, &job, "cron").await;
                    }
                });
            }
        }
    }

    // NOTE: there is no cross-instance cache-coherence task. Every request reads
    // and writes Postgres directly (1 HTTP request = 1 SQL transaction), so the
    // durable DB is the single source of truth — instances never hold a divergent
    // in-memory copy that would need NOTIFY/poll reconciliation.

    // ===== Inline interval jobs (only when there's no durable queue) =====
    // Each tick runs the SAME Postgres-first job body as the durable worker
    // (`jobs::run_*`): load a fresh DB snapshot, do the work, write results back.
    if !durable_jobs {
        // Purge expired trash once per hour.
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    tick.tick().await;
                    jobs::run_purge(&shared).await;
                }
            });
        }

        // Hourly (configurable) S3 backup pass. Reads the current interval each
        // loop so config changes via PUT /api/storage apply.
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                loop {
                    let interval = { shared.read().await.storage.backup.interval_secs.max(1) };
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    jobs::run_backup(&shared).await;
                }
            });
        }

        // AI-analysis safety-net pass (import stage 4) every 5 minutes.
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(300));
                loop {
                    tick.tick().await;
                    jobs::run_ai_analysis(&shared).await;
                }
            });
        }

        // Daily near-duplicate detection + face re-clustering.
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(86_400));
                loop {
                    tick.tick().await;
                    jobs::run_duplicates(&shared).await;
                }
            });
        }
    } // end: inline interval jobs (no durable queue)

    // PUBLIC routes (no session required). Login establishes a session; logout
    // and me read their own bearer token; password reset is token/old-password
    // authorized inside the handler; invite acceptance happens pre-account; MCP
    // carries its own OIDC/session auth. Everything else is gated by the auth
    // middleware on the `protected` router below.
    let public = Router::new()
        .route("/", get(|| async { "Photon server" }))
        .route("/api/health", get(health))
        .route("/api/login", post(handlers::login))
        // Usernameless passkey sign-in (discoverable assertion). PUBLIC: these
        // ESTABLISH a session, like password login. Inert (503) when passkeys are
        // disabled (no RP configured).
        .route("/api/login/passkey/start", post(webauthn::login_start))
        .route("/api/login/passkey/finish", post(webauthn::login_finish))
        .route("/api/logout", post(handlers::logout))
        .route("/api/users/{id}/password", post(handlers::set_user_password))
        .route("/api/invites/accept", post(handlers::accept_invite))
        // public self-registration (gated by features.public_signup in the handler)
        .route("/api/register", post(handlers::register))
        // public album links (no account; gated by features.public_links): album
        // metadata + live photos, plus per-photo thumb/render bytes.
        .route("/api/public/albums/{token}", get(handlers::public_album))
        .route(
            "/api/public/albums/{token}/photos/{id}/thumb",
            get(handlers::public_album_thumb),
        )
        .route(
            "/api/public/albums/{token}/photos/{id}/render",
            get(handlers::public_album_render),
        )
        // OIDC web login (relying-party / authorization-code flow). PUBLIC: these
        // ESTABLISH a session, so they precede the auth middleware. All inert
        // (404/503) when OIDC is unconfigured.
        .route("/api/auth/oidc/available", get(handlers::oidc_available))
        .route("/api/auth/oidc/login", get(handlers::oidc_login_start))
        .route("/api/auth/oidc/callback", get(handlers::oidc_callback))
        .route("/mcp", post(mcp::mcp_endpoint));

    let protected = Router::new()
        // users
        .route(
            "/api/users",
            get(handlers::list_users).post(handlers::create_user),
        )
        .route(
            "/api/users/{id}",
            get(handlers::get_user)
                .patch(handlers::update_user)
                .delete(handlers::delete_user),
        )
        .route("/api/users/{id}/reset", post(handlers::reset_user_password))
        // TOTP 2FA enrollment (self or admin, via path_authz `/api/users/{id}/..`).
        .route("/api/users/{id}/2fa", get(handlers::totp_status).delete(handlers::totp_disable))
        .route("/api/users/{id}/2fa/setup", post(handlers::totp_setup))
        .route("/api/users/{id}/2fa/verify", post(handlers::totp_verify))
        // Passkey enrollment + management (self-only, via path_authz `/api/users/{id}/..`).
        .route("/api/users/{id}/passkeys", get(webauthn::list))
        .route("/api/users/{id}/passkeys/register/start", post(webauthn::register_start))
        .route("/api/users/{id}/passkeys/register/finish", post(webauthn::register_finish))
        .route("/api/users/{id}/passkeys/{cred_id}", delete(webauthn::delete))
        // partner relationship (directed read grant)
        .route("/api/users/{id}/partners", post(handlers::add_partner))
        .route(
            "/api/users/{id}/partners/{partner_id}",
            delete(handlers::remove_partner),
        )
        // groups
        .route(
            "/api/groups",
            get(handlers::list_groups).post(handlers::create_group),
        )
        .route(
            "/api/groups/{id}",
            get(handlers::get_group).delete(handlers::delete_group),
        )
        .route(
            "/api/groups/{id}/members",
            post(handlers::add_group_member),
        )
        .route(
            "/api/groups/{id}/members/{user_id}",
            delete(handlers::remove_group_member),
        )
        // photos
        .route("/api/photos", get(handlers::list_photos))
        .route(
            "/api/photos/{id}",
            get(handlers::get_photo).delete(handlers::trash_photo),
        )
        .route(
            "/api/photos/{id}/metadata",
            patch(handlers::patch_photo_metadata),
        )
        .route("/api/photos/{id}/thumb", get(handlers::get_thumb))
        // AI analysis (import stage 4): re-run analysis for a photo.
        .route("/api/photos/{id}/analyze", post(handlers::analyze_photo))
        // photo lifecycle: trash + archive
        .route("/api/photos/{id}/restore", post(handlers::restore_photo))
        .route("/api/photos/{id}/archive", post(handlers::archive_photo))
        .route(
            "/api/photos/{id}/unarchive",
            post(handlers::unarchive_photo),
        )
        .route(
            "/api/photos/{id}/permanent",
            delete(handlers::permanent_delete_photo),
        )
        .route("/api/trash", get(handlers::list_trash))
        .route("/api/archive", get(handlers::list_archive))
        // uploads: single file at a time — the front parallelizes N calls and the
        // server pairs companions (JPG + sidecar RAW) by (owner, base-name).
        .route("/api/uploads", post(handlers::upload_file))
        // async multi-stage import: POST starts it (202), GET polls progress.
        .route("/api/uploads/raw", post(handlers::upload_raw))
        .route("/api/uploads/{batch_id}", get(handlers::get_import))
        // storage settings + backup
        .route(
            "/api/storage",
            get(handlers::get_storage).put(handlers::update_storage),
        )
        .route("/api/storage/backup/run", post(handlers::run_backup_now))
        // originals + screen-adapted render (lightbox / full image)
        .route("/api/photos/{id}/original", get(handlers::get_original))
        // companion (RAW/.ARW sidecar) download
        .route(
            "/api/photos/{id}/companions/{ext}/download",
            get(handlers::download_companion),
        )
        .route("/api/photos/{id}/render", get(handlers::render_photo))
        // detected face boxes + per-cluster identity for the overlay
        .route("/api/photos/{id}/faces", get(handlers::photo_faces))
        // apply an Editor PLUGIN op to a photo's original → edited bytes
        // (owner-only: the `/api/photos/{id}/..` authz rule enforces it).
        .route(
            "/api/photos/{id}/plugin-edit/{plugin}/{op}",
            post(handlers::apply_plugin_edit),
        )
        // transcoding: device-aware render plan (descriptor) + real image transcode
        .route("/api/photos/{id}/render-plan", get(handlers::render_plan))
        // Bake a 90°-step rotation (+ optional flip) into the `edited` companion.
        .route("/api/photos/{id}/rotate", post(handlers::rotate_photo))
        // Bake the editor's Light/Color tonal sliders into the `edited` companion.
        .route("/api/photos/{id}/adjust", post(handlers::adjust_photo))
        .route("/api/transcode/image", post(handlers::transcode_image))
        // SMTP config + invites (email notifications)
        .route(
            "/api/smtp",
            get(handlers::get_smtp).put(handlers::update_smtp),
        )
        .route(
            "/api/invites",
            get(handlers::list_invites).post(handlers::create_invite),
        )
        // albums
        .route(
            "/api/albums",
            get(handlers::list_albums).post(handlers::create_album),
        )
        .route(
            "/api/albums/{id}",
            get(handlers::get_album).delete(handlers::delete_album),
        )
        .route("/api/albums/{id}/photos", post(handlers::add_album_photos))
        // public-link mint/revoke (owner-only via resource_authz; gated by the flag)
        .route(
            "/api/albums/{id}/public-link",
            post(handlers::create_public_link),
        )
        .route(
            "/api/albums/{id}/public-link/{token}",
            delete(handlers::revoke_public_link),
        )
        .route(
            "/api/albums/{id}/shares",
            post(handlers::add_album_share).delete(handlers::remove_album_share),
        )
        .route(
            "/api/albums/{id}/contribute",
            post(handlers::contribute_to_album),
        )
        // timeline prefs + timeline
        .route(
            "/api/users/{id}/timeline-prefs",
            get(handlers::get_prefs).put(handlers::update_prefs),
        )
        .route("/api/users/{id}/timeline", get(handlers::get_timeline))
        .route("/api/users/{id}/storage", get(handlers::get_user_storage))
        // search (wider scope than timeline)
        .route("/api/users/{id}/search", get(handlers::search_photos))
        // duplicate detection (daily job result)
        .route("/api/users/{id}/duplicates", get(handlers::get_duplicates))
        // face recognition (People): clusters, naming, per-person photos
        .route("/api/users/{id}/people", get(handlers::list_people))
        .route("/api/people/{person_id}/name", post(handlers::name_person))
        .route(
            "/api/people/{person_id}/photos",
            get(handlers::person_photos),
        )
        // kinship between People (reciprocal directed edges)
        .route(
            "/api/people/{person_id}/relationships",
            post(handlers::add_relationship),
        )
        .route(
            "/api/people/{person_id}/relationships/{other_person_id}",
            delete(handlers::remove_relationship),
        )
        // People Studio — face/person curation (owner-scoped via central authz)
        .route("/api/people/{person_id}/faces", get(handlers::person_faces))
        .route("/api/people/{person_id}/birthdate", post(handlers::set_person_birthdate))
        .route("/api/people/{person_id}/cover", post(handlers::set_person_cover))
        .route("/api/people/{person_id}/approve", post(handlers::approve_faces))
        .route("/api/people/{person_id}/ignore", post(handlers::ignore_faces))
        .route("/api/people/{person_id}/move", post(handlers::move_faces))
        .route("/api/people/{person_id}/merge", post(handlers::merge_people))
        .route("/api/people/{person_id}/hide", post(handlers::hide_person))
        // per-user PIN vault
        .route("/api/users/{id}/vault", get(handlers::get_vault))
        .route("/api/users/{id}/vault/pin", put(handlers::set_vault_pin))
        .route(
            "/api/users/{id}/vault/unlock",
            post(handlers::unlock_vault),
        )
        .route(
            "/api/users/{id}/vault/photos",
            post(handlers::add_vault_photos).delete(handlers::remove_vault_photos),
        )
        // server-side DLNA/UPnP casting (browsers can't do DLNA): discover LAN
        // MediaRenderers, then cast a photo URL to one via AVTransport.
        .route("/api/cast/devices", get(handlers::cast_devices))
        .route("/api/cast/dlna", post(handlers::cast_dlna))
        // admin stats + authorization self-audit
        .route("/api/admin/stats", get(handlers::admin_stats))
        // trigger a background/maintenance job on demand (admin only)
        .route("/api/admin/jobs/{name}/run", post(handlers::run_job))
        // Editor plugins: cross-plugin op catalog (static path wins over the
        // catch-all below). Apply is on the photo (owner-only via central authz).
        .route("/api/plugins/editor/ops", get(handlers::plugin_editor_ops))
        // list route plugins for the UI tools section (static path wins over the
        // catch-all below).
        .route("/api/plugins", get(handlers::list_route_plugins))
        // catch-all proxy to Route plugins (any signed-in user; per-plugin authz
        // is the plugin's job via the forwarded actor/is_admin).
        .route("/api/plugins/{name}/{*rest}", any(handlers::plugin_proxy))
        .route("/api/audit/access", get(handlers::audit_access))
        // app-wide settings (Gravatar toggle, …)
        .route(
            "/api/settings",
            get(handlers::get_settings).patch(handlers::patch_settings),
        )
        // current session's user (reads its own bearer token)
        .route("/api/me", get(handlers::me))
        // Enforce authentication + coarse authorization on every route above.
        .route_layer(axum::middleware::from_fn_with_state(
            shared.clone(),
            auth::auth_middleware,
        ));

    let app = public
        .merge(protected)
        // Allow large uploads (photos/videos base64) — default axum limit is 2 MB.
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024))
        .layer(cors_layer())
        // Trace requests by method + PATH ONLY — never the query string — so a
        // session token passed as `?token=` (media URLs) is never written to logs. (F7)
        .layer(TraceLayer::new_for_http().make_span_with(request_span));

    // Static security headers on every API response. These are simple constants
    // suitable for a JSON API; the CSP allows self plus inline styles and
    // data:/https: images for the demo UI.
    let app = security_headers(app).with_state(shared);

    // Bind port is configurable (PHOTON_PORT) so multiple instances can run behind
    // a load balancer on one host; defaults to 3000. (`port` was read above to
    // build the plugin callback base URL.)
    let addr = format!("0.0.0.0:{port}");
    let addr = addr.as_str();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("photon-server listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
