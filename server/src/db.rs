//! PostgreSQL persistence plumbing.
//!
//! Architecture: the in-memory `AppState` maps stay the working set. This module
//! adds an OPTIONAL write-through `Persistence` backend (a `PgPool`). When
//! `DATABASE_URL` is set the server connects, runs migrations, and either seeds
//! the DB (first run) or loads every row back into the in-memory maps. After
//! each mutation, `AppState` calls a `persist_*` / `delete_*` helper which
//! upserts/deletes the affected entity. When the pool is `None` (the default for
//! demos/tests) every helper is a no-op, so the pure in-memory path is unchanged.
//!
//! Postgres stores ALL domain data EXCEPT media blobs (image/video bytes live on
//! the filesystem/S3 via the `StorageBackend`). Rich sub-structures are stored as
//! `jsonb`. Only the RUNTIME query API (`sqlx::query`, `bind`) is used — never the
//! compile-time `query!` macros — so building/testing never needs a live database.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

use crate::models::{
    Album, Face, Group, Invite, Person, Photo, ResetToken, SmtpConfig, StorageSettings,
    TimelinePrefs, User, Vault,
};

/// Absolute session lifetime: a bearer token stops authenticating this long after
/// it was minted (30 days), bounding the damage of a leaked token.
const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 30;

/// The Postgres persistence backend: a connection pool plus write-through
/// upsert/delete helpers. Held as `Option<Persistence>` on `AppState`.
#[derive(Clone)]
pub struct Persistence {
    pool: PgPool,
}

impl Persistence {
    /// Connect a pool to `database_url` and run the embedded migrations.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            // Higher cap: parallel single-file uploads each briefly hold a
            // connection for the per-base advisory lock during companion pairing.
            .max_connections(25)
            .connect(database_url)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Wrap an already-connected pool (used by the `#[sqlx::test]` harness, which
    /// hands each test an isolated, already-migrated database).
    #[cfg(test)]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run `op` while holding a per-`(owner, base-name)` advisory lock, so two
    /// concurrent single-file uploads of the same base (a JPG and its RAW) serialize
    /// their companion-pairing decision instead of both creating a photo. The lock
    /// is released right after `op` completes. Best-effort: with no spare
    /// connection, `op` runs unlocked (a benign rare double-create at worst).
    pub async fn with_base_lock<T, F>(&self, owner: &str, base: &str, op: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        owner.hash(&mut h);
        base.to_ascii_lowercase().hash(&mut h);
        let key = h.finish() as i64;
        let mut conn = match self.pool.acquire().await {
            Ok(c) => c,
            Err(_) => return op.await,
        };
        let _ = sqlx::query("SELECT pg_advisory_lock($1)").bind(key).execute(&mut *conn).await;
        let out = op.await;
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(key).execute(&mut *conn).await;
        out
    }

    /// Whether the DB is empty (no users yet) — drives the seed-vs-load decision.
    pub async fn is_empty(&self) -> Result<bool, sqlx::Error> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM users")
            .fetch_one(&self.pool)
            .await?;
        let n: i64 = row.get("n");
        Ok(n == 0)
    }

    // ---- Upserts (write-through) ----

    pub async fn upsert_user(&self, u: &User) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO users (id, name, email, avatar_url, password_hash, salt, pepper, is_admin, disabled, quota_mb, partners, totp_secret) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (id) DO UPDATE SET name=$2, email=$3, avatar_url=$4, \
               password_hash=$5, salt=$6, pepper=$7, is_admin=$8, disabled=$9, quota_mb=$10, partners=$11, totp_secret=$12",
        )
        .bind(&u.id)
        .bind(&u.name)
        .bind(&u.email)
        .bind(&u.avatar_url)
        .bind(&u.password_hash)
        .bind(&u.salt)
        .bind(&u.pepper)
        .bind(u.is_admin)
        .bind(u.disabled)
        .bind(u.quota_mb.map(|q| q as i64))
        .bind(serde_json::to_value(&u.partners).unwrap_or_default())
        .bind(&u.totp_secret)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_user(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_group(&self, g: &Group) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO groups (id, name, owner_id, member_ids) VALUES ($1,$2,$3,$4) \
             ON CONFLICT (id) DO UPDATE SET name=$2, owner_id=$3, member_ids=$4",
        )
        .bind(&g.id)
        .bind(&g.name)
        .bind(&g.owner_id)
        .bind(serde_json::to_value(&g.member_ids).unwrap_or_default())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_group(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM groups WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_photo(&self, p: &Photo) -> Result<(), sqlx::Error> {
        // CONTEXT RECOGNITION (CLIP): the embedding is stored as a portable
        // `float8[]` column (`clip_embedding`) that maps cleanly to/from
        // `Vec<f32>` via sqlx's runtime API without the `pgvector` crate. The
        // companion `vector(512)` column + ivfflat index (see migration 0003) is
        // the production ANN index; this `float8[]` is the source of truth the
        // server reads back. `f32 -> f64` widening is lossless.
        let embedding: Option<Vec<f64>> = p
            .clip_embedding
            .as_ref()
            .map(|v| v.iter().map(|&x| x as f64).collect());
        sqlx::query(
            "INSERT INTO photos (id, owner_id, filename, seed, kind, exif, overrides, companions, archived, deleted_at, backed_up, thumb_url, size_mb, ocr_text, ai_tags, ai_people, analyzed, clip_embedding) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18) \
             ON CONFLICT (id) DO UPDATE SET owner_id=$2, filename=$3, seed=$4, kind=$5, \
               exif=$6, overrides=$7, companions=$8, archived=$9, deleted_at=$10, backed_up=$11, thumb_url=$12, size_mb=$13, \
               ocr_text=$14, ai_tags=$15, ai_people=$16, analyzed=$17, clip_embedding=$18",
        )
        .bind(&p.id)
        .bind(&p.owner_id)
        .bind(&p.filename)
        .bind(p.seed as i64)
        .bind(&p.kind)
        .bind(serde_json::to_value(&p.exif).unwrap_or_default())
        .bind(serde_json::to_value(&p.overrides).unwrap_or_default())
        .bind(serde_json::to_value(&p.companions).unwrap_or_default())
        .bind(p.archived)
        .bind(&p.deleted_at)
        .bind(p.backed_up)
        .bind(&p.thumb_url)
        .bind(p.size_mb)
        .bind(&p.ocr_text)
        .bind(serde_json::to_value(&p.ai_tags).unwrap_or_default())
        .bind(serde_json::to_value(&p.ai_people).unwrap_or_default())
        .bind(p.analyzed)
        .bind(embedding)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_photo(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM photos WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_album(&self, a: &Album) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO albums (id, name, owner_id, cover_seed, photo_ids, shares) VALUES ($1,$2,$3,$4,$5,$6) \
             ON CONFLICT (id) DO UPDATE SET name=$2, owner_id=$3, cover_seed=$4, photo_ids=$5, shares=$6",
        )
        .bind(&a.id)
        .bind(&a.name)
        .bind(&a.owner_id)
        .bind(a.cover_seed as i64)
        .bind(serde_json::to_value(&a.photo_ids).unwrap_or_default())
        .bind(serde_json::to_value(&a.shares).unwrap_or_default())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_album(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM albums WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_prefs(&self, user_id: &str, prefs: &TimelinePrefs) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO timeline_prefs (user_id, prefs) VALUES ($1,$2) \
             ON CONFLICT (user_id) DO UPDATE SET prefs=$2",
        )
        .bind(user_id)
        .bind(serde_json::to_value(prefs).unwrap_or_default())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_prefs(&self, user_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM timeline_prefs WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_storage(&self, s: &StorageSettings) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO storage_settings (id, settings) VALUES (1, $1) \
             ON CONFLICT (id) DO UPDATE SET settings=$1",
        )
        .bind(serde_json::to_value(s).unwrap_or_default())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_smtp(&self, cfg: Option<&SmtpConfig>) -> Result<(), sqlx::Error> {
        let value = cfg.map(|c| serde_json::to_value(c).unwrap_or_default());
        sqlx::query(
            "INSERT INTO smtp_config (id, config) VALUES (1, $1) \
             ON CONFLICT (id) DO UPDATE SET config=$1",
        )
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_invite(&self, inv: &Invite) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO invites (token, email, inviter_id, created_at, accepted) VALUES ($1,$2,$3,$4,$5) \
             ON CONFLICT (token) DO UPDATE SET email=$2, inviter_id=$3, created_at=$4, accepted=$5",
        )
        .bind(&inv.token)
        .bind(&inv.email)
        .bind(&inv.inviter_id)
        .bind(&inv.created_at)
        .bind(inv.accepted)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_reset_token(&self, rt: &ResetToken) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO reset_tokens (token, user_id, created_at, used) VALUES ($1,$2,$3,$4) \
             ON CONFLICT (token) DO UPDATE SET user_id=$2, created_at=$3, used=$4",
        )
        .bind(&rt.token)
        .bind(&rt.user_id)
        .bind(&rt.created_at)
        .bind(rt.used)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Bearer-token sessions (shared across instances) ----

    pub async fn upsert_session(
        &self,
        token: &str,
        user_id: &str,
        created_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO sessions (token, user_id, created_at) VALUES ($1,$2,$3) \
             ON CONFLICT (token) DO UPDATE SET user_id=$2, created_at=$3",
        )
        .bind(token)
        .bind(user_id)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve a token to its user id straight from the DB (the per-request auth
    /// path). Enforces an **absolute session TTL**: a token older than
    /// [`SESSION_TTL_SECS`] no longer authenticates, even if its row lingers, so a
    /// leaked token can't be used forever (F5). `created_at` is an RFC3339 UTC
    /// string, so the lexicographic `>` is a chronological comparison.
    pub async fn get_session(&self, token: &str) -> Result<Option<String>, sqlx::Error> {
        let cutoff = crate::state::rfc3339_secs_ago(SESSION_TTL_SECS);
        let row = sqlx::query("SELECT user_id FROM sessions WHERE token = $1 AND created_at > $2")
            .bind(token)
            .bind(&cutoff)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("user_id")))
    }

    /// Delete sessions older than the TTL — housekeeping so expired rows don't
    /// accumulate. Best-effort.
    pub async fn cleanup_sessions(&self) -> Result<(), sqlx::Error> {
        let cutoff = crate::state::rfc3339_secs_ago(SESSION_TTL_SECS);
        sqlx::query("DELETE FROM sessions WHERE created_at < $1")
            .bind(&cutoff)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_session(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE token = $1")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Public album links (no-account read-only album sharing) ----

    /// Store (or refresh) a `token -> album_id` public-link mapping.
    pub async fn upsert_public_link(
        &self,
        token: &str,
        album_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO public_links (token, album_id, created_at) VALUES ($1,$2,$3) \
             ON CONFLICT (token) DO UPDATE SET album_id=$2, created_at=$3",
        )
        .bind(token)
        .bind(album_id)
        .bind(crate::state::now_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve a public-link token to its album id, or `None` if unknown.
    pub async fn get_public_link(&self, token: &str) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT album_id FROM public_links WHERE token = $1")
            .bind(token)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("album_id")))
    }

    /// Revoke a public link by deleting its mapping (idempotent).
    pub async fn delete_public_link(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM public_links WHERE token = $1")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- OIDC login-flow state (authorization-code / relying-party) ----

    /// Store a freshly-generated `state -> (nonce, created_at)` row for the OIDC
    /// login flow. `state` is a CSPRNG value the IdP echoes back; `created_at` is
    /// an RFC 3339 string used to enforce a short TTL on the callback.
    pub async fn insert_oidc_state(
        &self,
        state: &str,
        nonce: &str,
        created_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO oidc_states (state, nonce, created_at) VALUES ($1,$2,$3) \
             ON CONFLICT (state) DO UPDATE SET nonce=$2, created_at=$3",
        )
        .bind(state)
        .bind(nonce)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically consume an OIDC `state`: delete the row and return its
    /// `(nonce, created_at)` if it existed. SINGLE-USE — a second call for the
    /// same `state` returns `None` (the `DELETE ... RETURNING` guarantees only one
    /// caller wins, even across instances), which defeats replay of a code.
    pub async fn take_oidc_state(
        &self,
        state: &str,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        let row =
            sqlx::query("DELETE FROM oidc_states WHERE state = $1 RETURNING nonce, created_at")
                .bind(state)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| (r.get("nonce"), r.get("created_at"))))
    }

    /// Drop OIDC login states created before `cutoff` (an RFC 3339 string),
    /// reaping abandoned/expired flows. Best-effort housekeeping.
    pub async fn cleanup_oidc_states(&self, cutoff: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM oidc_states WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- TOTP replay protection (last successfully-used time-step) ----

    /// The last TOTP step this user authenticated with, if any.
    pub async fn totp_last_step(&self, user_id: &str) -> Result<Option<i64>, sqlx::Error> {
        let row = sqlx::query("SELECT last_step FROM totp_usage WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("last_step")))
    }

    /// Record the TOTP step just consumed (monotonic; defeats replay).
    pub async fn set_totp_last_step(&self, user_id: &str, step: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO totp_usage (user_id, last_step) VALUES ($1,$2) \
             ON CONFLICT (user_id) DO UPDATE SET last_step = $2",
        )
        .bind(user_id)
        .bind(step)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- WebAuthn passkeys (passwordless credentials) ----

    /// Register a new passkey credential. `cred` is the serialized webauthn-rs
    /// `Passkey` (stored as JSONB; a credential, never exposed by any API).
    pub async fn insert_passkey(
        &self,
        id: &str,
        user_id: &str,
        wa_uid: &str,
        name: Option<&str>,
        cred: serde_json::Value,
        created_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO passkeys (id, user_id, wa_uid, name, cred, created_at) \
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (id) DO UPDATE SET cred=$5, name=$4",
        )
        .bind(id)
        .bind(user_id)
        .bind(wa_uid)
        .bind(name)
        .bind(cred)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The stored WebAuthn user handle for a user (shared by all their passkeys),
    /// or `None` if they have none yet. Reused so a user keeps a stable handle.
    pub async fn wa_uid_for_user(&self, user_id: &str) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT wa_uid FROM passkeys WHERE user_id = $1 LIMIT 1")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("wa_uid")))
    }

    /// The serialized `Passkey` credentials for a user (for exclude-credentials on
    /// registration and for authentication).
    pub async fn passkeys_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<serde_json::Value>, sqlx::Error> {
        let rows = sqlx::query("SELECT cred FROM passkeys WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| r.get("cred")).collect())
    }

    /// `(user_id, serialized Passkey)` for every passkey under a WebAuthn user
    /// handle — used to resolve + verify a usernameless (discoverable) login.
    pub async fn passkeys_for_wa_uid(
        &self,
        wa_uid: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, sqlx::Error> {
        let rows = sqlx::query("SELECT user_id, cred FROM passkeys WHERE wa_uid = $1")
            .bind(wa_uid)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| (r.get("user_id"), r.get("cred"))).collect())
    }

    /// Passkey metadata for the management UI (no credential material).
    pub async fn list_passkeys_meta(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, Option<String>, String, Option<String>)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, created_at, last_used_at FROM passkeys WHERE user_id = $1 \
             ORDER BY created_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("id"), r.get("name"), r.get("created_at"), r.get("last_used_at")))
            .collect())
    }

    /// Update a passkey's stored credential (bumped signature counter) and
    /// `last_used_at` after a successful authentication.
    pub async fn touch_passkey(
        &self,
        id: &str,
        cred: serde_json::Value,
        last_used_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE passkeys SET cred=$2, last_used_at=$3 WHERE id=$1")
            .bind(id)
            .bind(cred)
            .bind(last_used_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete one of a user's passkeys (scoped to the owner). Idempotent.
    pub async fn delete_passkey(&self, user_id: &str, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM passkeys WHERE id=$1 AND user_id=$2")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Transient WebAuthn ceremony state (single-use, multi-instance safe) ----

    /// Store a begin-ceremony state under a random `id` (the handle returned to the
    /// client). `state` is the serialized PasskeyRegistration / DiscoverableAuthentication.
    pub async fn insert_webauthn_state(
        &self,
        id: &str,
        user_id: Option<&str>,
        kind: &str,
        state: serde_json::Value,
        created_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO webauthn_states (id, user_id, kind, state, created_at) \
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(user_id)
        .bind(kind)
        .bind(state)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically consume a ceremony state: delete and return `(user_id, kind,
    /// state)` if present. SINGLE-USE (`DELETE ... RETURNING`), defeating replay.
    pub async fn take_webauthn_state(
        &self,
        id: &str,
    ) -> Result<Option<(Option<String>, String, serde_json::Value)>, sqlx::Error> {
        let row = sqlx::query(
            "DELETE FROM webauthn_states WHERE id = $1 RETURNING user_id, kind, state",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get("user_id"), r.get("kind"), r.get("state"))))
    }

    /// Drop ceremony states created before `cutoff` (RFC 3339), reaping abandoned
    /// flows. Best-effort housekeeping.
    pub async fn cleanup_webauthn_states(&self, cutoff: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM webauthn_states WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// (token, user_id) for every live session — loaded into the in-memory cache
    /// on startup.
    pub async fn load_sessions(&self) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT token, user_id FROM sessions")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("token"), r.get("user_id")))
            .collect())
    }

    pub async fn upsert_vault(&self, user_id: &str, v: &Vault) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO vaults (user_id, pin_hash, salt, photo_ids) VALUES ($1,$2,$3,$4) \
             ON CONFLICT (user_id) DO UPDATE SET pin_hash=$2, salt=$3, photo_ids=$4",
        )
        .bind(user_id)
        .bind(&v.pin_hash)
        .bind(&v.salt)
        .bind(serde_json::to_value(&v.photo_ids).unwrap_or_default())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_vault(&self, user_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM vaults WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Face recognition (faces + people) ----

    /// Replace ALL of `owner`'s faces + people with the in-memory set: upsert the
    /// given rows, then delete any owner rows whose id is no longer present
    /// (clustering rebuilds person ids, so stale people must be pruned). Runs in a
    /// transaction so a reader never sees a half-applied re-cluster. The face
    /// `embedding` is stored as a portable `float8[]` (lossless `f32 -> f64`), the
    /// same approach as CLIP embeddings — never exposed by any API.
    pub async fn replace_owner_faces(
        &self,
        owner: &str,
        faces: &[&Face],
        people: &[&Person],
        keep_faces: &[String],
        keep_people: &[String],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for f in faces {
            let embedding: Vec<f64> = f.embedding.iter().map(|&x| x as f64).collect();
            sqlx::query(
                "INSERT INTO faces (id, photo_id, owner_id, bbox, score, person_id, embedding, ignored, assigned_label, confirmed) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
                 ON CONFLICT (id) DO UPDATE SET photo_id=$2, owner_id=$3, bbox=$4, score=$5, person_id=$6, embedding=$7, ignored=$8, assigned_label=$9, confirmed=$10",
            )
            .bind(&f.id)
            .bind(&f.photo_id)
            .bind(&f.owner_id)
            .bind(serde_json::to_value(f.bbox).unwrap_or_default())
            .bind(f.score as f64)
            .bind(&f.person_id)
            .bind(embedding)
            .bind(f.ignored)
            .bind(&f.assigned_label)
            .bind(f.confirmed)
            .execute(&mut *tx)
            .await?;
        }
        for pe in people {
            sqlx::query(
                "INSERT INTO people (id, owner_id, name, face_ids, cover_photo_id, cover_bbox, relationships, birthdate, hidden, cover_locked) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
                 ON CONFLICT (id) DO UPDATE SET owner_id=$2, name=$3, face_ids=$4, cover_photo_id=$5, cover_bbox=$6, relationships=$7, birthdate=$8, hidden=$9, cover_locked=$10",
            )
            .bind(&pe.id)
            .bind(&pe.owner_id)
            .bind(&pe.name)
            .bind(serde_json::to_value(&pe.face_ids).unwrap_or_default())
            .bind(&pe.cover_photo_id)
            .bind(pe.cover_bbox.map(|b| serde_json::to_value(b).unwrap_or_default()))
            .bind(serde_json::to_value(&pe.relationships).unwrap_or_default())
            .bind(&pe.birthdate)
            .bind(pe.hidden)
            .bind(pe.cover_locked)
            .execute(&mut *tx)
            .await?;
        }
        // Prune stale rows for this owner (ids no longer in memory).
        let keep_f = serde_json::to_value(keep_faces).unwrap_or_default();
        sqlx::query(
            "DELETE FROM faces WHERE owner_id = $1 AND NOT (id = ANY(SELECT jsonb_array_elements_text($2)))",
        )
        .bind(owner)
        .bind(&keep_f)
        .execute(&mut *tx)
        .await?;
        let keep_p = serde_json::to_value(keep_people).unwrap_or_default();
        sqlx::query(
            "DELETE FROM people WHERE owner_id = $1 AND NOT (id = ANY(SELECT jsonb_array_elements_text($2)))",
        )
        .bind(owner)
        .bind(&keep_p)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Faces detected in ONE photo (targeted read for the per-photo faces
    /// endpoint). The biometric `embedding` is NOT selected — it never leaves the
    /// server — so the returned `Face.embedding` is empty.
    pub async fn faces_for_photo(&self, photo_id: &str) -> Result<Vec<Face>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, photo_id, owner_id, bbox, score, person_id, ignored, assigned_label, confirmed FROM faces WHERE photo_id = $1 ORDER BY id",
        )
        .bind(photo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Face {
                id: r.get("id"),
                photo_id: r.get("photo_id"),
                owner_id: r.get("owner_id"),
                bbox: from_json(r.get("bbox")),
                embedding: Vec::new(),
                score: r.get::<f64, _>("score") as f32,
                person_id: r.get("person_id"),
                ignored: r.get("ignored"),
                assigned_label: r.get("assigned_label"),
                confirmed: r.get("confirmed"),
            })
            .collect())
    }

    pub async fn load_faces(&self) -> Result<Vec<Face>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, photo_id, owner_id, bbox, score, person_id, embedding, ignored, assigned_label, confirmed FROM faces",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let embedding: Option<Vec<f64>> = r.get("embedding");
                let embedding = embedding
                    .map(|v| v.into_iter().map(|x| x as f32).collect())
                    .unwrap_or_default();
                let bbox: [f32; 4] = from_json(r.get("bbox"));
                Face {
                    id: r.get("id"),
                    photo_id: r.get("photo_id"),
                    owner_id: r.get("owner_id"),
                    bbox,
                    embedding,
                    score: r.get::<f64, _>("score") as f32,
                    person_id: r.get("person_id"),
                    ignored: r.get("ignored"),
                    assigned_label: r.get("assigned_label"),
                confirmed: r.get("confirmed"),
                }
            })
            .collect())
    }

    pub async fn load_people(&self) -> Result<Vec<Person>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_id, name, face_ids, cover_photo_id, cover_bbox, relationships, birthdate, hidden, cover_locked FROM people",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let cover_bbox: Option<serde_json::Value> = r.get("cover_bbox");
                let cover_bbox =
                    cover_bbox.and_then(|v| serde_json::from_value::<[f32; 4]>(v).ok());
                Person {
                    id: r.get("id"),
                    owner_id: r.get("owner_id"),
                    name: r.get("name"),
                    face_ids: from_json(r.get("face_ids")),
                    cover_photo_id: r.get("cover_photo_id"),
                    cover_bbox,
                    relationships: from_json(r.get("relationships")),
                    birthdate: r.get("birthdate"),
                    hidden: r.get("hidden"),
                    cover_locked: r.get("cover_locked"),
                }
            })
            .collect())
    }

    // ---- Near-duplicate detection results ----

    /// Replace ALL duplicate-group rows with the freshly-computed set (one row per
    /// owner). Runs in a transaction: truncate-then-insert so a reader never sees a
    /// half-applied recompute, and owners that no longer have duplicates are pruned.
    pub async fn replace_duplicate_groups(
        &self,
        groups: &std::collections::HashMap<String, Vec<Vec<String>>>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM duplicate_groups").execute(&mut *tx).await?;
        for (owner, g) in groups {
            sqlx::query("INSERT INTO duplicate_groups (owner_id, groups) VALUES ($1,$2)")
                .bind(owner)
                .bind(serde_json::to_value(g).unwrap_or_default())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_duplicate_groups(
        &self,
    ) -> Result<Vec<(String, Vec<Vec<String>>)>, sqlx::Error> {
        let rows = sqlx::query("SELECT owner_id, groups FROM duplicate_groups")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("owner_id"), from_json(r.get("groups"))))
            .collect())
    }

    // ---- Bulk loaders (startup: DB -> in-memory) ----

    /// Single user by id — Postgres-first read (one statement = one transaction).
    pub async fn get_user(&self, id: &str) -> Result<Option<User>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, email, avatar_url, password_hash, salt, pepper, is_admin, disabled, quota_mb, partners, totp_secret FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| User {
            id: r.get("id"),
            name: r.get("name"),
            email: r.get("email"),
            avatar_url: r.get("avatar_url"),
            password_hash: r.get("password_hash"),
            salt: r.get("salt"),
            pepper: r.get("pepper"),
            is_admin: r.get("is_admin"),
            disabled: r.get("disabled"),
            quota_mb: r.get::<Option<i64>, _>("quota_mb").map(|q| q as u64),
            partners: from_json(r.get("partners")),
            totp_secret: r.get("totp_secret"),
        }))
    }

    pub async fn load_users(&self) -> Result<Vec<User>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, email, avatar_url, password_hash, salt, pepper, is_admin, disabled, quota_mb, partners, totp_secret FROM users",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| User {
                id: r.get("id"),
                name: r.get("name"),
                email: r.get("email"),
                avatar_url: r.get("avatar_url"),
                password_hash: r.get("password_hash"),
                salt: r.get("salt"),
                pepper: r.get("pepper"),
                is_admin: r.get("is_admin"),
                disabled: r.get("disabled"),
                quota_mb: r.get::<Option<i64>, _>("quota_mb").map(|q| q as u64),
                partners: from_json(r.get("partners")),
                totp_secret: r.get("totp_secret"),
            })
            .collect())
    }

    pub async fn load_groups(&self) -> Result<Vec<Group>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, name, owner_id, member_ids FROM groups")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Group {
                id: r.get("id"),
                name: r.get("name"),
                owner_id: r.get("owner_id"),
                member_ids: from_json(r.get("member_ids")),
            })
            .collect())
    }

    pub async fn load_photos(&self) -> Result<Vec<Photo>, sqlx::Error> {
        let rows = sqlx::query(&format!("SELECT {PHOTO_COLUMNS} FROM photos"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_photo).collect())
    }

    /// Begin a transaction for handlers that need several statements atomically
    /// (one HTTP request = one SQL transaction).
    pub async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
        self.pool.begin().await
    }

    /// Mint a cluster-unique id `<prefix>_<n>` from the shared Postgres sequence
    /// (safe across instances, unlike the in-memory counter).
    pub async fn next_id(&self, prefix: &str) -> Result<String, sqlx::Error> {
        let n: i64 = sqlx::query_scalar("SELECT nextval('photon_id_seq')")
            .fetch_one(&self.pool)
            .await?;
        Ok(format!("{prefix}_{n}"))
    }

    /// Reserve a contiguous BLOCK of ids for a request that mints several (e.g. an
    /// import: batch + N file ids + N photo ids). Returns the block base; the
    /// caller seeds a per-request counter with it. `nextval * 1000` gives each
    /// request a private 1000-id window (no overlap across instances/requests).
    pub async fn next_id_base(&self) -> Result<u64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar("SELECT nextval('photon_id_seq')")
            .fetch_one(&self.pool)
            .await?;
        Ok((n as u64) * 1000)
    }

    pub async fn upsert_import_batch(
        &self,
        b: &crate::models::ImportBatch,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO import_batches (id, owner_id, album_id, items, created_at) \
             VALUES ($1,$2,$3,$4,$5) \
             ON CONFLICT (id) DO UPDATE SET owner_id=$2, album_id=$3, items=$4, created_at=$5",
        )
        .bind(&b.id)
        .bind(&b.owner_id)
        .bind(&b.album_id)
        .bind(serde_json::to_value(&b.items).unwrap_or_default())
        .bind(&b.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append a completed job run to the history (admin "Run history").
    pub async fn insert_job_run(&self, r: &crate::models::JobRun) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO job_runs (name, outcome, items, started_at, duration_ms, trigger) \
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(&r.name)
        .bind(&r.outcome)
        .bind(r.items)
        .bind(&r.started_at)
        .bind(r.duration_ms)
        .bind(&r.trigger)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The most recent `limit` job runs, newest first.
    pub async fn load_job_runs(&self, limit: i64) -> Result<Vec<crate::models::JobRun>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT name, outcome, items, started_at, duration_ms, trigger FROM job_runs \
             ORDER BY started_at DESC, id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| crate::models::JobRun {
                name: r.get("name"),
                outcome: r.get("outcome"),
                items: r.get("items"),
                started_at: r.get("started_at"),
                duration_ms: r.get("duration_ms"),
                trigger: r.get("trigger"),
            })
            .collect())
    }

    pub async fn get_import_batch(
        &self,
        id: &str,
    ) -> Result<Option<crate::models::ImportBatch>, sqlx::Error> {
        let row =
            sqlx::query("SELECT id, owner_id, album_id, items, created_at FROM import_batches WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| crate::models::ImportBatch {
            id: r.get("id"),
            owner_id: r.get("owner_id"),
            album_id: r.get("album_id"),
            items: from_json(r.get("items")),
            created_at: r.get("created_at"),
        }))
    }

    /// Set a photo's `archived` flag in one statement, returning the updated row.
    pub async fn set_photo_archived(
        &self,
        id: &str,
        archived: bool,
    ) -> Result<Option<Photo>, sqlx::Error> {
        let cols = "id, owner_id, filename, seed, kind, exif, overrides, companions, archived, deleted_at, backed_up, thumb_url, size_mb, ocr_text, ai_tags, ai_people, analyzed, clip_embedding";
        let row = sqlx::query(&format!(
            "UPDATE photos SET archived = $2 WHERE id = $1 RETURNING {cols}"
        ))
        .bind(id)
        .bind(archived)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_photo))
    }

    /// Set a photo's `deleted_at` (trash = `Some(now)`, restore = `None`) in one
    /// statement, returning the updated row.
    pub async fn set_photo_deleted_at(
        &self,
        id: &str,
        deleted_at: Option<String>,
    ) -> Result<Option<Photo>, sqlx::Error> {
        let cols = "id, owner_id, filename, seed, kind, exif, overrides, companions, archived, deleted_at, backed_up, thumb_url, size_mb, ocr_text, ai_tags, ai_people, analyzed, clip_embedding";
        let row = sqlx::query(&format!(
            "UPDATE photos SET deleted_at = $2 WHERE id = $1 RETURNING {cols}"
        ))
        .bind(id)
        .bind(deleted_at)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_photo))
    }

    /// Single photo by id — Postgres-first read (one statement = one transaction).
    pub async fn get_photo(&self, id: &str) -> Result<Option<Photo>, sqlx::Error> {
        let row = sqlx::query(&format!("SELECT {PHOTO_COLUMNS} FROM photos WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_photo))
    }

    /// Single album by id — Postgres-first read.
    pub async fn get_album(&self, id: &str) -> Result<Option<Album>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, owner_id, cover_seed, photo_ids, shares FROM albums WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let cover_seed: i64 = r.get("cover_seed");
            Album {
                id: r.get("id"),
                name: r.get("name"),
                owner_id: r.get("owner_id"),
                cover_seed: cover_seed as u32,
                photo_ids: from_json(r.get("photo_ids")),
                shares: from_json(r.get("shares")),
            }
        }))
    }

    /// Single group by id — Postgres-first read.
    pub async fn get_group(&self, id: &str) -> Result<Option<Group>, sqlx::Error> {
        let row = sqlx::query("SELECT id, name, owner_id, member_ids FROM groups WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| Group {
            id: r.get("id"),
            name: r.get("name"),
            owner_id: r.get("owner_id"),
            member_ids: from_json(r.get("member_ids")),
        }))
    }

    /// Single person (face cluster) by id — Postgres-first targeted read. Used by
    /// the auth middleware's per-resource ownership check so it can authorize a
    /// `/api/people/{id}/…` request without loading the whole `people` table.
    pub async fn get_person(&self, id: &str) -> Result<Option<Person>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, owner_id, name, face_ids, cover_photo_id, cover_bbox, relationships, birthdate, hidden, cover_locked FROM people WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let cover_bbox: Option<serde_json::Value> = r.get("cover_bbox");
            let cover_bbox = cover_bbox.and_then(|v| serde_json::from_value::<[f32; 4]>(v).ok());
            Person {
                id: r.get("id"),
                owner_id: r.get("owner_id"),
                name: r.get("name"),
                face_ids: from_json(r.get("face_ids")),
                cover_photo_id: r.get("cover_photo_id"),
                cover_bbox,
                relationships: from_json(r.get("relationships")),
                birthdate: r.get("birthdate"),
                hidden: r.get("hidden"),
                cover_locked: r.get("cover_locked"),
            }
        }))
    }

    pub async fn load_albums(&self) -> Result<Vec<Album>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, name, owner_id, cover_seed, photo_ids, shares FROM albums")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let cover_seed: i64 = r.get("cover_seed");
                Album {
                    id: r.get("id"),
                    name: r.get("name"),
                    owner_id: r.get("owner_id"),
                    cover_seed: cover_seed as u32,
                    photo_ids: from_json(r.get("photo_ids")),
                    shares: from_json(r.get("shares")),
                }
            })
            .collect())
    }

    pub async fn load_prefs(&self) -> Result<Vec<(String, TimelinePrefs)>, sqlx::Error> {
        let rows = sqlx::query("SELECT user_id, prefs FROM timeline_prefs")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("user_id"), from_json(r.get("prefs"))))
            .collect())
    }

    pub async fn load_storage(&self) -> Result<Option<StorageSettings>, sqlx::Error> {
        let row = sqlx::query("SELECT settings FROM storage_settings WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| from_json(r.get("settings"))))
    }

    pub async fn load_smtp(&self) -> Result<Option<SmtpConfig>, sqlx::Error> {
        let row = sqlx::query("SELECT config FROM smtp_config WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            let v: Option<serde_json::Value> = r.get("config");
            v.and_then(|v| serde_json::from_value(v).ok())
        }))
    }

    pub async fn load_invites(&self) -> Result<Vec<Invite>, sqlx::Error> {
        let rows = sqlx::query("SELECT token, email, inviter_id, created_at, accepted FROM invites")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Invite {
                token: r.get("token"),
                email: r.get("email"),
                inviter_id: r.get("inviter_id"),
                created_at: r.get("created_at"),
                accepted: r.get("accepted"),
            })
            .collect())
    }

    pub async fn load_reset_tokens(&self) -> Result<Vec<ResetToken>, sqlx::Error> {
        let rows = sqlx::query("SELECT token, user_id, created_at, used FROM reset_tokens")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| ResetToken {
                token: r.get("token"),
                user_id: r.get("user_id"),
                created_at: r.get("created_at"),
                used: r.get("used"),
            })
            .collect())
    }

    /// True if `photo_id` is a member of ANY user's PIN vault. Vaulted photos are
    /// hidden from everyone — including unauthenticated public album links.
    pub async fn is_photo_vaulted(&self, photo_id: &str) -> Result<bool, sqlx::Error> {
        let needle = serde_json::json!([photo_id]);
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM vaults WHERE photo_ids @> $1)",
        )
        .bind(needle)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn load_vaults(&self) -> Result<Vec<(String, Vault)>, sqlx::Error> {
        let rows = sqlx::query("SELECT user_id, pin_hash, salt, photo_ids FROM vaults")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("user_id"),
                    Vault {
                        pin_hash: r.get("pin_hash"),
                        salt: r.get("salt"),
                        photo_ids: from_json(r.get("photo_ids")),
                    },
                )
            })
            .collect())
    }
}

/// Decode a jsonb column value into `T`, falling back to `Default` on any error.
fn from_json<T: serde::de::DeserializeOwned + Default>(v: serde_json::Value) -> T {
    serde_json::from_value(v).unwrap_or_default()
}

/// Map a `photos` row to a `Photo` (shared by `load_photos` + `get_photo`).
/// The full stored-column list for the `photos` table, in one place so the
/// SELECTs in [`Persistence::load_photos`] / [`Persistence::get_photo`] can't
/// drift apart. `row_to_photo` reads by NAME (not position), so column order is
/// irrelevant — but the SET of columns must stay in sync with the `r.get(...)`
/// reads below. A `&'static str` interpolated into a query string (no user
/// input) carries no injection risk.
const PHOTO_COLUMNS: &str = "id, owner_id, filename, seed, kind, exif, overrides, companions, archived, deleted_at, backed_up, thumb_url, size_mb, ocr_text, ai_tags, ai_people, analyzed, clip_embedding";

fn row_to_photo(r: &sqlx::postgres::PgRow) -> Photo {
    let seed: i64 = r.get("seed");
    let embedding: Option<Vec<f64>> = r.get("clip_embedding");
    let clip_embedding = embedding.map(|v| v.into_iter().map(|x| x as f32).collect());
    let id: String = r.get("id");
    let thumb_url: Option<String> = r.get("thumb_url");
    // `full_url` is a COMPUTED field, not a stored column: a photo that has a
    // thumbnail also has a stored original (both are produced together by the
    // import pipeline), so it can be served full-size via the render endpoint.
    // Seed/placeholder photos have neither. Without this, a Postgres round-trip
    // would drop `full_url` and the lightbox would fall back to the thumbnail.
    let full_url = thumb_url.as_ref().map(|_| format!("/api/photos/{id}/render"));
    Photo {
        id,
        owner_id: r.get("owner_id"),
        filename: r.get("filename"),
        seed: seed as u32,
        kind: r.get("kind"),
        exif: from_json(r.get("exif")),
        overrides: from_json(r.get("overrides")),
        companions: from_json(r.get("companions")),
        archived: r.get("archived"),
        deleted_at: r.get("deleted_at"),
        backed_up: r.get("backed_up"),
        thumb_url,
        size_mb: r.get("size_mb"),
        ocr_text: r.get("ocr_text"),
        ai_tags: from_json(r.get("ai_tags")),
        ai_people: from_json(r.get("ai_people")),
        analyzed: r.get("analyzed"),
        clip_embedding,
        full_url,
    }
}
