use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use serde::Serialize;

use crate::analyze::{Analyzer, HeuristicAnalyzer};
use crate::extract::{ExifExtractor, MetadataExtractor};
use crate::mailer::{Notification, SmtpNotification, StdoutNotification};
use crate::models::{
    Album, Companion, Exif, Group, Invite, MetadataOverride, Photo, PhotoView, ResetToken, Share,
    ShareRole, ShareTarget, SmtpConfig, StorageMode, StorageSettings, TimelinePrefs, UploadedFile,
    User, Vault,
};
use crate::storage::{LocalFsBackend, S3Backend, StorageBackend};
use crate::transcode::{MediaFormat, RealTranscoder, TranscodePlan, Transcoder};

/// Current time as an RFC3339 / ISO-8601 UTC string (e.g. "2026-06-23T10:00:00Z").
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// RFC3339 UTC timestamp `secs` seconds in the past — a cutoff for reaping
/// short-lived rows (e.g. transient WebAuthn / OIDC ceremony state).
pub fn rfc3339_secs_ago(secs: i64) -> String {
    (OffsetDateTime::now_utc() - time::Duration::seconds(secs))
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Classic Gravatar avatar URL for an email: MD5 of the trimmed, lowercased
/// address. `d=identicon` yields a deterministic fallback for unknown emails.
pub fn gravatar_url(email: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(email.trim().to_ascii_lowercase().as_bytes());
    let hex = h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("https://www.gravatar.com/avatar/{hex}?d=identicon&s=200")
}

/// An RFC3339 UTC string `secs` seconds in the future.
pub fn rfc3339_in(secs: i64) -> String {
    (OffsetDateTime::now_utc() + time::Duration::seconds(secs))
        .format(&Rfc3339)
        .unwrap_or_else(|_| now_rfc3339())
}

/// Brute-force lockout counter (per login email / per vault user).
#[derive(Default, Clone)]
pub struct Lockout {
    pub fails: u32,
    /// RFC3339 instant until which the key is locked, if any.
    pub until: Option<String>,
}

/// Failed attempts before a temporary lockout kicks in.
pub const RATE_MAX_FAILS: u32 = 5;
/// Lockout duration in seconds once the threshold is hit.
pub const RATE_LOCK_SECS: i64 = 60;

/// Parse an RFC3339 string into seconds-since-epoch, if valid.
fn rfc3339_to_unix(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// Extensions considered displayable raster images (highest display priority).
fn is_raster(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "heic" | "heif" | "png" | "tif" | "tiff"
    )
}

fn is_video(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4" | "mov" | "m4v" | "avi" | "mkv"
    )
}

fn is_raw(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "arw" | "raf" | "cr3" | "cr2" | "nef" | "dng" | "rw2" | "orf" | "raw"
    )
}

/// Classify an extension into a companion/photo kind label.
fn classify(ext: &str) -> &'static str {
    if is_raster(ext) {
        "jpeg"
    } else if is_video(ext) {
        "video"
    } else if is_raw(ext) {
        "raw"
    } else {
        "other"
    }
}

/// Display priority: lower number = preferred primary.
/// Displayable raster wins, then video, then raw, then other.
fn display_priority(ext: &str) -> u8 {
    if is_raster(ext) {
        0
    } else if is_video(ext) {
        1
    } else if is_raw(ext) {
        2
    } else {
        3
    }
}

/// The Photo.kind label for a primary of the given extension.
fn photo_kind(ext: &str) -> &'static str {
    if is_raster(ext) {
        "photo"
    } else if is_video(ext) {
        "video"
    } else if is_raw(ext) {
        "raw"
    } else {
        "photo"
    }
}

/// Case-insensitive substring match of `needle` over a photo's filename and its
/// effective text fields (title/caption/city/country/tags/people). `needle` is
/// assumed already lowercased.
fn photo_matches(p: &Photo, needle: &str) -> bool {
    let v = p.effective();
    let mut hay: Vec<String> = vec![v.filename.to_lowercase()];
    if let Some(s) = &v.title {
        hay.push(s.to_lowercase());
    }
    if let Some(s) = &v.caption {
        hay.push(s.to_lowercase());
    }
    if let Some(s) = &v.city {
        hay.push(s.to_lowercase());
    }
    if let Some(s) = &v.country {
        hay.push(s.to_lowercase());
    }
    for t in &v.tags {
        hay.push(t.to_lowercase());
    }
    for person in &v.people {
        hay.push(person.to_lowercase());
    }
    if let Some(cam) = &p.exif.camera {
        hay.push(cam.to_lowercase());
    }
    // AI-analysis (stage 4) derived metadata is searchable: OCR'd text, machine
    // context/scene tags, and detected people all join the free-text haystack.
    if let Some(ocr) = &p.ocr_text {
        hay.push(ocr.to_lowercase());
    }
    for t in &p.ai_tags {
        hay.push(t.to_lowercase());
    }
    for person in &p.ai_people {
        hay.push(person.to_lowercase());
    }
    hay.iter().any(|h| h.contains(needle))
}

/// Structured search filters (engine-agnostic). Free text is `q`; the rest are
/// optional facets. `near` is (lat, lng, radius_km) for geo-radius search.
#[derive(Default)]
pub struct SearchFilters {
    pub q: String,
    pub camera: Option<String>,
    pub from: Option<String>, // YYYY-MM-DD inclusive
    pub to: Option<String>,   // YYYY-MM-DD inclusive
    pub place: Option<String>,
    pub near: Option<(f64, f64, f64)>,
}

/// Parse an EXIF-style coordinate ("45.7640° N" / "9.1393° W") into signed
/// decimal degrees. Returns None when there is no parseable number.
pub fn parse_coord(s: Option<&str>) -> Option<f64> {
    let s = s?;
    let num: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    let mut v: f64 = num.parse().ok()?;
    if s.contains('S') || s.contains('W') {
        v = -v.abs();
    }
    Some(v)
}

/// Mandatory default quota (MB) when S3 is the primary store and the user has no
/// explicit quota — you can't read a "filesystem size" for an object store.
const DEFAULT_S3_QUOTA_MB: u64 = 100_000;
/// Fallback when the filesystem capacity can't be read (non-unix / statvfs fail).
const DEFAULT_FS_FALLBACK_MB: f64 = 256_000.0;

/// Total capacity (MB) of the filesystem backing `path`, via statvfs. None on
/// non-unix or error.
#[cfg(unix)]
fn fs_total_mb(path: &str) -> Option<f64> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: statvfs writes into a zeroed struct; we check the return code.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let total_bytes = st.f_frsize as f64 * st.f_blocks as f64;
        Some(total_bytes / (1024.0 * 1024.0))
    }
}
#[cfg(not(unix))]
fn fs_total_mb(_path: &str) -> Option<f64> {
    None
}

/// Great-circle distance in kilometres between two lat/lng points.
fn haversine_km(a_lat: f64, a_lng: f64, b_lat: f64, b_lng: f64) -> f64 {
    let r = 6371.0_f64;
    let dlat = (b_lat - a_lat).to_radians();
    let dlng = (b_lng - a_lng).to_radians();
    let h = (dlat / 2.0).sin().powi(2)
        + a_lat.to_radians().cos() * b_lat.to_radians().cos() * (dlng / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// Base name (filename without its final extension), lowercased.
pub fn base_name(filename: &str) -> String {
    match filename.rfind('.') {
        Some(i) => filename[..i].to_ascii_lowercase(),
        None => filename.to_ascii_lowercase(),
    }
}

#[derive(Default)]
pub struct AppState {
    pub users: HashMap<String, User>,
    pub groups: HashMap<String, Group>,
    pub photos: HashMap<String, Photo>,
    pub albums: HashMap<String, Album>,
    pub prefs: HashMap<String, TimelinePrefs>,
    /// Per-user PIN vaults (user_id -> Vault), created lazily on first use.
    pub vaults: HashMap<String, Vault>,
    pub storage: StorageSettings,
    /// Configurable SMTP server. `None` until configured (seed: None); the
    /// mailer falls back to [`StdoutNotification`] while unset.
    pub smtp: Option<SmtpConfig>,
    /// Pending/accepted invites, keyed by token.
    pub invites: HashMap<String, Invite>,
    /// Single-use password reset tokens, keyed by token string.
    pub reset_tokens: HashMap<String, ResetToken>,
    /// Background-job run state registry, keyed by job name
    /// ("backup" | "trash_purge" | "thumbnail" | "ai_analysis" | "duplicates").
    pub jobs: HashMap<String, JobStats>,
    /// In-memory thumbnail blobs keyed by photo id: `(bytes, content_type)`.
    /// Real uploads populate this; it backs `GET /api/photos/{id}/thumb`. The
    /// same bytes are also pushed to the configured `StorageBackend` under
    /// `thumbs/{id}.webp` (best-effort). The demo seed has no entries here.
    pub thumbs: HashMap<String, (Vec<u8>, String)>,
    /// In-memory ORIGINAL upload blobs keyed by photo id: `(bytes, content_type)`.
    /// Real uploads populate this with the primary file's UNMODIFIED bytes; it
    /// backs `GET /api/photos/{id}/original` and `GET /api/photos/{id}/render`.
    /// The SAME bytes are also pushed best-effort to the configured
    /// `StorageBackend` under `originals/{id}.{ext}` — that backend (filesystem /
    /// S3) is the REAL store for blobs; this map is a DEMO convenience so the
    /// in-process server can serve originals without a round-trip to storage. The
    /// demo seed has no entries here (seed photos have no original bytes).
    pub originals: HashMap<String, (Vec<u8>, String)>,
    /// Server-wide secret key for argon2id PASSWORD hashing, read once at startup
    /// from env `PHOTON_PASSWORD_SALT` (a documented dev default is used when
    /// unset). Mixed into every user password hash on top of the per-user pepper.
    /// Vault PINs use the SAME argon2id secret (per-vault salt).
    pub password_secret: Vec<u8>,
    /// Brute-force lockout counters keyed by e.g. "login:&lt;email&gt;" / "vault:&lt;user&gt;".
    pub lockouts: HashMap<String, Lockout>,
    /// Root directory for the `LocalFsBackend` object store, honoring env
    /// `PHOTON_DATA_DIR` (defaults to `"data"` when unset).
    pub data_dir: String,
    /// Optional Postgres persistence backend. `None` => in-memory mode (the
    /// default for demos/tests). Set at startup when `DATABASE_URL` is present.
    pub persistence: Option<crate::db::Persistence>,
    /// Opt-in bearer-token sessions: `token -> user_id`. Created by `POST
    /// /api/login`, read by `GET /api/me`, dropped by `POST /api/logout`. These
    /// are NOT yet enforced on the existing per-user/data routes (a documented
    /// follow-up); they are the primitive the UI will adopt to gate access.
    /// In-memory only — not persisted across restarts.
    pub sessions: HashMap<String, String>,
    /// Durable job-queue handle (graphile_worker). `Some` only in DB mode, set in
    /// `main` after the worker starts; lets handlers enqueue background jobs.
    /// `None` in-memory ⇒ inline tokio fallback.
    pub worker_utils: Option<graphile_worker::WorkerUtils>,
    /// CONTEXT RECOGNITION (CLIP): optional client to the ML embedding sidecar.
    /// `None` (the default for [`seed`], demos and the whole test suite) means ML
    /// is disabled — no network is used and behavior is exactly as before. Set in
    /// `main` from `PHOTON_ML_URL` via [`crate::ml::MlClient::from_env`].
    pub ml: Option<crate::ml::MlClient>,
    /// SUBPROCESS PLUGINS (go-plugin style): optional registry of plugin binaries
    /// launched + connected over gRPC Unix sockets. `None` (the default for
    /// [`seed`], demos and the whole test suite) means plugins are disabled — no
    /// child process is launched and behavior is exactly as before. Set in `main`
    /// from `PHOTON_PLUGINS_DIR` via [`crate::plugins::PluginHost::from_env`]. Has
    /// its OWN `RwLock`, so a slow plugin call never holds this `AppState` lock.
    pub plugins: Option<std::sync::Arc<crate::plugins::PluginHost>>,
    /// WEBAUTHN / PASSKEYS: the relying-party instance (RP id + allowed origins).
    /// `None` (the default for [`seed`], demos and the whole test suite) means
    /// passkeys are disabled — every passkey route degrades gracefully and behavior
    /// is exactly as before. Built in `main` from `PHOTON_RP_ID`/`PHOTON_RP_ORIGIN`
    /// (localhost defaults) via [`crate::webauthn::build_webauthn`]. Cheap `Arc`
    /// clone; the registered credentials themselves live in Postgres, not here.
    pub webauthn: Option<std::sync::Arc<webauthn_rs::Webauthn>>,
    /// OIDC WEB LOGIN (relying-party / authorization-code flow). `None` (the
    /// default for [`seed`], demos and the whole test suite) means the login
    /// feature is OFF — `/api/auth/oidc/*` is inert and no IdP is contacted. Set
    /// in `main` from `OIDC_*` via [`crate::oidc::OidcLogin::from_env`].
    pub oidc_login: Option<crate::oidc::OidcLogin>,
    /// Transient per-import-batch progress, keyed by batch id. NOT persisted —
    /// this is ephemeral progress the client polls via `GET /api/uploads/{id}`.
    pub imports: HashMap<String, crate::models::ImportBatch>,
    /// Decoded upload bytes awaiting processing, keyed by `(batch_id, file_id)`.
    /// Kept OUT of [`crate::models::ImportBatch`] so they are never serialized to
    /// clients; drained by the worker as each file is processed.
    pub pending_bytes: HashMap<(String, String), (String, String, Vec<u8>)>,
    /// COMPANION DOWNLOAD: kept bytes of each photo's companion files (e.g. the
    /// RAW/.ARW sidecar of a JPG), keyed by `(photo_id, ext_lowercased)` →
    /// `(bytes, original_filename, mime)`. Populated in the import CREATE phase
    /// for every companion attached to a primary, so `GET
    /// /api/photos/{id}/companions/{ext}/download` can serve the real bytes.
    /// The SAME bytes are also pushed best-effort to the configured
    /// `StorageBackend` under `companions/{photo_id}.{ext}` — that backend
    /// (filesystem / S3) is the AUTHORITATIVE store; this in-memory map is a DEMO
    /// convenience so the in-process server can serve companions without a
    /// round-trip to storage (mirrors [`Self::originals`]). The demo seed has no
    /// entries here.
    pub companion_bytes: HashMap<(String, String), (Vec<u8>, String, String)>,
    /// DUPLICATE DETECTION: per-owner near-duplicate groups (each inner Vec is a
    /// set of photo ids whose perceptual hashes are within the Hamming threshold,
    /// length >= 2). Recomputed by the daily `duplicates` job via
    /// [`Self::detect_duplicates`]; in-memory only (cheap to rebuild).
    pub duplicate_groups: HashMap<String, Vec<Vec<String>>>,
    /// DLNA/UPnP CASTING: last-discovered MediaRenderer devices, keyed by their
    /// stable id. Refreshed by `GET /api/cast/devices` (SSDP discovery at request
    /// time) so the subsequent `POST /api/cast/dlna` can resolve a `device_id`
    /// back to a [`crate::dlna::DlnaDevice`]. In-memory only (cheap to rebuild;
    /// devices come and go on the LAN).
    pub dlna_devices: HashMap<String, crate::dlna::DlnaDevice>,
    /// FACE RECOGNITION: detected faces keyed by face id. Populated during the
    /// analysis stage when ML is set (`PHOTON_ML_URL`); empty offline. The face
    /// `embedding` is sensitive (server-side only, never serialized).
    pub faces: HashMap<String, crate::models::Face>,
    /// FACE RECOGNITION: face clusters (People) keyed by person id, produced by
    /// [`Self::cluster_faces`]. In-memory + persisted alongside faces.
    pub people: HashMap<String, crate::models::Person>,
    counter: AtomicU64,
}

/// The minimal storage configuration a blob operation needs: where blobs live
/// (`data_dir` for the local FS backend) and the active [`StorageSettings`]
/// (which selects FS vs S3). Cloned cheaply out of [`AppState`] via
/// [`AppState::storage_ctx`] so the blob-serving handlers don't have to build a
/// throwaway config-only `AppState` under the global lock just to read a single
/// photo's bytes.
#[derive(Clone, Default)]
pub struct StorageCtx {
    pub data_dir: String,
    pub storage: StorageSettings,
}

/// Documented dev-default for the argon2id server secret, used ONLY when the
/// `PHOTON_PASSWORD_SALT` env var is unset (a warning is logged at startup).
/// Production deployments MUST set `PHOTON_PASSWORD_SALT`.
pub const DEV_PASSWORD_SECRET: &[u8] = b"photon-dev-insecure-password-secret-change-me";

/// Default `LocalFsBackend` root when env `PHOTON_DATA_DIR` is unset.
pub const DEFAULT_DATA_DIR: &str = "data";

/// Reserved companion `ext` for a plugin-EDITED version of a photo. The original
/// is never modified; the edit is stored alongside it as this companion
/// (`companions/{id}.edited`, always PNG) and PREFERRED for display. Re-editing
/// overwrites it. There is at most one per photo.
pub const EDITED_EXT: &str = "edited";

/// One step of a multi-step job's progress, surfaced live in the admin console.
#[derive(Debug, Clone, Serialize)]
pub struct JobStepStat {
    pub name: String,
    /// "pending" | "running" | "done" | "failed".
    pub state: String,
    /// 0..=100 within this step (meaningful while "running").
    pub percent: u32,
}

/// A live progress snapshot reported by a (plugin) job mid-run.
#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {
    pub steps: Vec<JobStepStat>,
    /// Index into `steps` of the step currently in progress.
    pub current: u32,
}

/// Run state of a background job, surfaced by `GET /api/admin/stats`.
#[derive(Debug, Clone, Serialize)]
pub struct JobStats {
    pub name: String,
    /// "idle" | "running".
    pub status: String,
    pub last_run_at: Option<String>,
    pub last_result: Option<String>,
    /// Live multi-step progress while running (plugin jobs that call `report`);
    /// `None` for jobs that don't report steps.
    pub progress: Option<JobProgress>,
}

impl JobStats {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            status: "idle".to_string(),
            last_run_at: None,
            last_result: None,
            progress: None,
        }
    }
}

/// A potential access leak found by [`AppState::audit_access`]: `user_id` could
/// reach `photo_id` via a read surface without a legitimate grant (or the photo
/// is vaulted/archived/trashed). An empty audit result means the system is sound.
#[derive(Debug, Clone, Serialize)]
pub struct AccessViolation {
    pub user_id: String,
    pub photo_id: String,
    pub reason: String,
}

/// Default job registry with all four tracked jobs in the idle state.
fn default_jobs() -> HashMap<String, JobStats> {
    // List exactly the runnable jobs (`JOB_NAMES`) so the admin console shows every
    // one — including the maintenance jobs (recluster_faces, rebuild_thumbnails,
    // reextract_metadata) — with a run button, before any of them has run once. The
    // old hardcoded list both omitted those AND used stale names ("thumbnail",
    // "backup") that didn't match a runnable job.
    let mut jobs = HashMap::new();
    for name in crate::jobs::JOB_NAMES {
        jobs.insert(name.to_string(), JobStats::new(name));
    }
    jobs
}

/// Lowercase hex encoding of bytes.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// CSPRNG-backed random token: fill `n_bytes` from the OS entropy source via the
/// `getrandom` crate and hex-encode them (so the returned string is `2*n_bytes`
/// chars). This replaces the old predictable `hex(sha256(next_id||now))`
/// derivations used for invite/reset tokens, vault salts and user peppers. If the
/// OS RNG ever fails (extremely unlikely), we panic rather than emit a weak token.
pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG (getrandom) must be available");
    hex(&buf)
}

/// PROCESS-WIDE test guard for any test that mutates `OIDC_*` environment vars.
/// `cargo test` runs every test in ONE process in parallel; the OIDC config in
/// both [`crate::mcp`] (resource-server) and [`crate::oidc`] (login) reads the
/// SAME `OIDC_ISSUER` env var, so without a single shared lock their env
/// mutations race. Both modules' tests acquire this guard.
#[cfg(test)]
pub(crate) fn oidc_env_guard() -> &'static tokio::sync::Mutex<()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &GUARD
}

impl StorageCtx {
    /// The configured object-storage backend: S3 when in replacement mode with a
    /// valid `primary_s3` config, else the local filesystem. All blob reads/writes
    /// go through this so the configured storage mode is honored consistently
    /// (previously the store helpers hard-coded the filesystem even in S3 mode).
    pub fn active_backend(&self) -> Box<dyn StorageBackend> {
        if matches!(self.storage.mode, crate::models::StorageMode::S3Replacement) {
            if let Some(cfg) = &self.storage.primary_s3 {
                match S3Backend::from_config(cfg) {
                    Ok(b) => return Box::new(b),
                    Err(e) => tracing::warn!("S3 backend unavailable ({e}); using filesystem"),
                }
            }
        }
        Box::new(LocalFsBackend::new(self.data_dir.clone()))
    }

    /// Load a photo's ORIGINAL bytes from the backend, deriving the key from the
    /// passed `&Photo` directly (filename → ext/mime). Used by the blob endpoints
    /// so they can serve a single photo WITHOUT loading the whole DB into a
    /// snapshot.
    pub async fn load_original_blob(&self, photo: &crate::models::Photo) -> Option<(Vec<u8>, String)> {
        let raw = photo.filename.rsplit('.').next()?.to_lowercase();
        let fmt = MediaFormat::from_ext(&raw);
        let ext = fmt.map(|f| f.ext()).unwrap_or("bin").to_string();
        let mime = fmt
            .map(|f| f.mime().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let key = format!("originals/{}.{ext}", photo.id);
        self.active_backend()
            .get_object(&key)
            .await
            .ok()
            .flatten()
            .map(|b| (b, mime))
    }

    /// Load a companion file's bytes from the backend, deriving the filename/mime
    /// from the passed `&Photo`'s companion list. Targeted to a single photo so the
    /// blob endpoint avoids a full snapshot. Returns `(bytes, filename, mime)`.
    pub async fn load_companion_blob(
        &self,
        photo: &crate::models::Photo,
        ext: &str,
    ) -> Option<(Vec<u8>, String, String)> {
        let ext = ext.to_lowercase();
        let filename = photo
            .companions
            .iter()
            .find(|c| c.ext.to_lowercase() == ext)
            .map(|c| c.filename.clone())?;
        // The reserved `edited` companion (a plugin-edited version) is always PNG;
        // other companions derive their mime from the real ext.
        let mime = if ext == EDITED_EXT {
            "image/png".to_string()
        } else {
            MediaFormat::from_ext(&ext)
                .map(|f| f.mime().to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string())
        };
        let key = format!("companions/{}.{ext}", photo.id);
        self.active_backend()
            .get_object(&key)
            .await
            .ok()
            .flatten()
            .map(|b| (b, filename, mime))
    }

    /// Load the bytes to DISPLAY for a photo: the edited version if one exists
    /// (preferred everywhere — render, timeline), else the untouched original. The
    /// original always remains available via [`Self::load_original_blob`].
    pub async fn load_display_blob(&self, photo: &crate::models::Photo) -> Option<(Vec<u8>, String)> {
        if AppState::has_edited_version(photo) {
            let key = format!("companions/{}.{EDITED_EXT}", photo.id);
            if let Ok(Some(b)) = self.active_backend().get_object(&key).await {
                return Some((b, "image/png".to_string()));
            }
        }
        self.load_original_blob(photo).await
    }

    /// Persist a plugin-EDITED version of `photo` (PNG bytes): write the edited
    /// blob as the reserved `edited` companion, regenerate the thumbnail from it
    /// (so the timeline shows the edit), and update the photo's companion list
    /// (overwriting any prior edit). The ORIGINAL blob is left untouched. Returns
    /// the mutated `Photo` for the caller to persist to Postgres; `None` if the
    /// backend write fails.
    pub async fn store_edited_version(
        &self,
        mut photo: crate::models::Photo,
        png: &[u8],
    ) -> Option<crate::models::Photo> {
        let backend = self.active_backend();
        let key = format!("companions/{}.{EDITED_EXT}", photo.id);
        backend.put_object(&key, png).await.ok()?;

        // Regenerate the thumbnail from the edited image so timeline/grid views
        // show the edit. Best-effort: a thumbnail failure doesn't fail the edit.
        if let Some(thumb) = AppState::render_thumbnail_bytes(png) {
            let _ = backend.put_object(&format!("thumbs/{}.webp", photo.id), &thumb).await;
        }

        // Replace any prior edited companion with the fresh one.
        let base = base_name(&photo.filename);
        photo.companions.retain(|c| c.ext != EDITED_EXT);
        photo.companions.push(crate::models::Companion {
            filename: format!("{base}-edited.png"),
            ext: EDITED_EXT.to_string(),
            kind: "edited".to_string(),
            size_mb: png.len() as f64 / 1_000_000.0,
            downloadable: true,
        });
        Some(photo)
    }

    /// Revert a photo to its ORIGINAL: drop the reserved `edited` companion from the
    /// photo (so `load_display_blob` serves the original again) and regenerate the
    /// thumbnail from the original bytes. Returns the mutated `Photo` to persist.
    pub async fn clear_edited_version(
        &self,
        mut photo: crate::models::Photo,
    ) -> Option<crate::models::Photo> {
        if let Some((bytes, _ct)) = self.load_original_blob(&photo).await {
            if let Some(thumb) = AppState::render_thumbnail_bytes(&bytes) {
                let _ = self
                    .active_backend()
                    .put_object(&format!("thumbs/{}.webp", photo.id), &thumb)
                    .await;
            }
        }
        photo.companions.retain(|c| c.ext != EDITED_EXT);
        Some(photo)
    }
}

impl AppState {
    /// Generate a new monotonic id with the given prefix, e.g. `alb_5`.
    pub fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{n}")
    }

    /// Seed the per-request id counter from a DB-reserved block base, so a request
    /// snapshot mints cluster-unique ids (the in-memory counter alone would reset
    /// to 0 on a fresh snapshot and collide). See `Persistence::next_id_base`.
    pub fn seed_id_counter(&self, base: u64) {
        self.counter.store(base, Ordering::Relaxed);
    }

    /// The notification transport: a real [`SmtpNotification`] when SMTP is
    /// configured, otherwise the [`StdoutNotification`] dev fallback (which prints
    /// messages — incl. reset/invite links — to stdout, so the demo + tests never
    /// need a live SMTP server).
    pub fn mailer(&self) -> Box<dyn Notification> {
        match &self.smtp {
            Some(cfg) => Box::new(SmtpNotification::new(cfg.clone())),
            None => Box::new(StdoutNotification),
        }
    }

    /// Generate an invite token from the OS CSPRNG: 32 random bytes, hex-encoded
    /// (64 chars). Unpredictable, replacing the old `hex(sha256(next_id||now))`.
    pub fn new_invite_token(&self) -> String {
        random_hex(32)
    }

    /// Resolve the email addresses a [`ShareTarget`] expands to: the single
    /// user's email, or every group member's email. Unknown users are skipped.
    pub fn target_emails(&self, target: &ShareTarget) -> Vec<String> {
        match target {
            ShareTarget::User(uid) => self
                .users
                .get(uid)
                .map(|u| vec![u.email.clone()])
                .unwrap_or_default(),
            ShareTarget::Group(gid) => match self.groups.get(gid) {
                Some(g) => g
                    .member_ids
                    .iter()
                    .filter_map(|m| self.users.get(m).map(|u| u.email.clone()))
                    .collect(),
                None => Vec::new(),
            },
        }
    }

    /// Does `user_id` belong to group `group_id`?
    pub fn is_group_member(&self, group_id: &str, user_id: &str) -> bool {
        self.groups
            .get(group_id)
            .map(|g| g.member_ids.iter().any(|m| m == user_id))
            .unwrap_or(false)
    }

    /// Does the given share target match `user_id` (directly or via group)?
    fn target_matches(&self, target: &ShareTarget, user_id: &str) -> bool {
        match target {
            ShareTarget::User(u) => u == user_id,
            ShareTarget::Group(g) => self.is_group_member(g, user_id),
        }
    }

    /// Is `album` shared *to* `user_id`, directly or via a group membership?
    /// The role is irrelevant for visibility — any share grants access.
    pub fn album_shared_to(&self, album: &Album, user_id: &str) -> bool {
        album
            .shares
            .iter()
            .any(|s| self.target_matches(&s.target, user_id))
    }

    /// The effective role `user_id` has on `album_id`.
    ///
    /// The owner is treated as `Contributor` (they have full contribution
    /// rights). For shared users the strongest matching share role wins, so a
    /// Contributor share (direct or via group) beats a Viewer one. Returns
    /// `None` when the user is neither owner nor a share target, or the album
    /// does not exist.
    pub fn album_role_for(&self, user_id: &str, album_id: &str) -> Option<ShareRole> {
        let album = self.albums.get(album_id)?;
        if album.owner_id == user_id {
            return Some(ShareRole::Contributor);
        }
        let mut role: Option<ShareRole> = None;
        for s in &album.shares {
            if self.target_matches(&s.target, user_id) {
                match s.role {
                    ShareRole::Contributor => return Some(ShareRole::Contributor),
                    ShareRole::Viewer => role = Some(ShareRole::Viewer),
                }
            }
        }
        role
    }

    /// Whether `user_id` may add their own photos to `album_id` (owner or a
    /// Contributor share).
    pub fn can_contribute(&self, user_id: &str, album_id: &str) -> bool {
        matches!(
            self.album_role_for(user_id, album_id),
            Some(ShareRole::Contributor)
        )
    }

    /// Is `photo_id` stored in ANY user's vault? Vault photos are hidden from
    /// the timeline and search everywhere (only an authenticated unlock by the
    /// owner returns them).
    pub fn is_in_any_vault(&self, photo_id: &str) -> bool {
        self.vaults
            .values()
            .any(|v| v.photo_ids.iter().any(|p| p == photo_id))
    }

    /// PARTNER relationship: the ids of users who have granted `user_id` partner
    /// access (i.e. every user A whose `partners` list contains `user_id`). The
    /// grant is directed: A.partners=[B] means B can read A's live photos, not the
    /// reverse. Used to widen the timeline + search candidate sets.
    pub fn partner_grantors(&self, user_id: &str) -> Vec<&str> {
        self.users
            .values()
            .filter(|u| u.partners.iter().any(|p| p == user_id))
            .map(|u| u.id.as_str())
            .collect()
    }

    // ---- Vault PIN management ----

    /// Generate a per-vault random salt from the OS CSPRNG (16 bytes, hex-encoded
    /// to 32 chars), replacing the old predictable derivation. Used as the argon2id
    /// salt material for the vault PIN.
    fn new_salt(&self) -> String {
        random_hex(16)
    }

    /// Set (or update) the PIN of `user_id`'s vault. The vault is created lazily
    /// with a fresh CSPRNG salt on first use. The PIN is hashed with argon2id
    /// (server secret + per-vault salt); only the PHC string is stored.
    pub fn set_pin(&mut self, user_id: &str, pin: &str) {
        let salt = self.new_salt();
        let secret = self.password_secret.clone();
        let vault = self
            .vaults
            .entry(user_id.to_string())
            .or_insert_with(|| Vault {
                pin_hash: None,
                salt: salt.clone(),
                photo_ids: Vec::new(),
            });
        // Re-salt on every set so a changed PIN never reuses the old hash basis.
        vault.salt = salt;
        vault.set_pin(&secret, pin);
    }

    /// Verify a PIN against `user_id`'s vault. Returns false if no vault/PIN is
    /// configured or the PIN does not match.
    pub fn verify_pin(&self, user_id: &str, pin: &str) -> bool {
        match self.vaults.get(user_id) {
            Some(v) => v.verify_pin(&self.password_secret, pin),
            None => false,
        }
    }

    // ---- User password management (Feature 1) ----

    /// Generate a per-user random pepper for argon2id password hashing from the OS
    /// CSPRNG (32 random bytes, hex-encoded to 64 chars). A fresh pepper is
    /// produced on every password set so a changed password never reuses the old
    /// hash basis.
    pub fn new_pepper(&self) -> String {
        random_hex(32)
    }

    /// The server-wide argon2id secret key for PASSWORD hashing.
    pub fn password_secret(&self) -> &[u8] {
        &self.password_secret
    }

    // ---- Postgres write-through helpers ----
    //
    // Each looks up the in-memory entity and upserts/deletes it in Postgres.
    // When `self.persistence` is `None` (in-memory mode: demos + the 59 tests)
    // every helper is a cheap no-op, so the in-memory path is unchanged. Errors
    // are logged and swallowed — persistence must never fail a request.

    /// True in Postgres mode; false in the default in-memory mode.
    pub fn is_persistent(&self) -> bool {
        self.persistence.is_some()
    }

    pub async fn persist_user(&self, id: &str) {
        if let (Some(p), Some(u)) = (&self.persistence, self.users.get(id)) {
            if let Err(e) = p.upsert_user(u).await {
                tracing::warn!("persist_user({id}) failed: {e}");
            }
        }
    }

    pub async fn delete_user_row(&self, id: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_user(id).await {
                tracing::warn!("delete_user({id}) failed: {e}");
            }
        }
    }

    pub async fn persist_group(&self, id: &str) {
        if let (Some(p), Some(g)) = (&self.persistence, self.groups.get(id)) {
            if let Err(e) = p.upsert_group(g).await {
                tracing::warn!("persist_group({id}) failed: {e}");
            }
        }
    }

    pub async fn delete_group_row(&self, id: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_group(id).await {
                tracing::warn!("delete_group({id}) failed: {e}");
            }
        }
    }

    pub async fn persist_photo(&self, id: &str) {
        if let (Some(p), Some(ph)) = (&self.persistence, self.photos.get(id)) {
            if let Err(e) = p.upsert_photo(ph).await {
                tracing::warn!("persist_photo({id}) failed: {e}");
            }
        }
    }

    pub async fn delete_photo_row(&self, id: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_photo(id).await {
                tracing::warn!("delete_photo({id}) failed: {e}");
            }
        }
    }

    pub async fn persist_album(&self, id: &str) {
        if let (Some(p), Some(a)) = (&self.persistence, self.albums.get(id)) {
            if let Err(e) = p.upsert_album(a).await {
                tracing::warn!("persist_album({id}) failed: {e}");
            }
        }
    }

    pub async fn delete_album_row(&self, id: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_album(id).await {
                tracing::warn!("delete_album({id}) failed: {e}");
            }
        }
    }

    pub async fn persist_prefs(&self, user_id: &str) {
        if let Some(p) = &self.persistence {
            let prefs = self.prefs.get(user_id).cloned().unwrap_or_default();
            if let Err(e) = p.upsert_prefs(user_id, &prefs).await {
                tracing::warn!("persist_prefs({user_id}) failed: {e}");
            }
        }
    }

    pub async fn delete_prefs_row(&self, user_id: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_prefs(user_id).await {
                tracing::warn!("delete_prefs({user_id}) failed: {e}");
            }
        }
    }

    pub async fn persist_storage(&self) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.upsert_storage(&self.storage).await {
                tracing::warn!("persist_storage failed: {e}");
            }
        }
    }

    pub async fn persist_smtp(&self) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.upsert_smtp(self.smtp.as_ref()).await {
                tracing::warn!("persist_smtp failed: {e}");
            }
        }
    }

    pub async fn persist_invite(&self, token: &str) {
        if let (Some(p), Some(inv)) = (&self.persistence, self.invites.get(token)) {
            if let Err(e) = p.upsert_invite(inv).await {
                tracing::warn!("persist_invite failed: {e}");
            }
        }
    }

    pub async fn persist_reset_token(&self, token: &str) {
        if let (Some(p), Some(rt)) = (&self.persistence, self.reset_tokens.get(token)) {
            if let Err(e) = p.upsert_reset_token(rt).await {
                tracing::warn!("persist_reset_token failed: {e}");
            }
        }
    }

    pub async fn persist_vault(&self, user_id: &str) {
        if let (Some(p), Some(v)) = (&self.persistence, self.vaults.get(user_id)) {
            if let Err(e) = p.upsert_vault(user_id, v).await {
                tracing::warn!("persist_vault({user_id}) failed: {e}");
            }
        }
    }

    /// FACE RECOGNITION write-through: upsert every face + person of `owner`, and
    /// delete any rows for that owner that no longer exist in memory (clustering
    /// rebuilds person ids). No-op in in-memory mode. Errors are logged + swallowed.
    pub async fn persist_faces(&self, owner: &str) {
        let Some(p) = &self.persistence else { return };
        let faces: Vec<&crate::models::Face> =
            self.faces.values().filter(|f| f.owner_id == owner).collect();
        let people: Vec<&crate::models::Person> =
            self.people.values().filter(|pe| pe.owner_id == owner).collect();
        let keep_faces: Vec<String> = faces.iter().map(|f| f.id.clone()).collect();
        let keep_people: Vec<String> = people.iter().map(|pe| pe.id.clone()).collect();
        if let Err(e) = p.replace_owner_faces(owner, &faces, &people, &keep_faces, &keep_people).await
        {
            tracing::warn!("persist_faces({owner}) failed: {e}");
        }
    }

    pub async fn delete_vault_row(&self, user_id: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_vault(user_id).await {
                tracing::warn!("delete_vault({user_id}) failed: {e}");
            }
        }
    }

    /// First-run path: persist the entire in-memory seed into an empty Postgres.
    pub async fn persist_seed(&self) {
        let p = match &self.persistence {
            Some(p) => p,
            None => return,
        };
        for u in self.users.values() {
            let _ = p.upsert_user(u).await;
        }
        for g in self.groups.values() {
            let _ = p.upsert_group(g).await;
        }
        for ph in self.photos.values() {
            let _ = p.upsert_photo(ph).await;
        }
        for a in self.albums.values() {
            let _ = p.upsert_album(a).await;
        }
        for (uid, prefs) in &self.prefs {
            let _ = p.upsert_prefs(uid, prefs).await;
        }
        for (uid, v) in &self.vaults {
            let _ = p.upsert_vault(uid, v).await;
        }
        for inv in self.invites.values() {
            let _ = p.upsert_invite(inv).await;
        }
        for rt in self.reset_tokens.values() {
            let _ = p.upsert_reset_token(rt).await;
        }
        let _ = p.upsert_storage(&self.storage).await;
        let _ = p.upsert_smtp(self.smtp.as_ref()).await;
    }

    /// Startup load path: replace the in-memory maps with the DB contents.
    pub async fn load_from_db(&mut self) -> Result<(), sqlx::Error> {
        let p = match self.persistence.clone() {
            Some(p) => p,
            None => return Ok(()),
        };
        self.users = p
            .load_users()
            .await?
            .into_iter()
            .map(|u| (u.id.clone(), u))
            .collect();
        self.groups = p
            .load_groups()
            .await?
            .into_iter()
            .map(|g| (g.id.clone(), g))
            .collect();
        self.photos = p
            .load_photos()
            .await?
            .into_iter()
            .map(|ph| (ph.id.clone(), ph))
            .collect();
        self.albums = p
            .load_albums()
            .await?
            .into_iter()
            .map(|a| (a.id.clone(), a))
            .collect();
        self.prefs = p.load_prefs().await?.into_iter().collect();
        self.vaults = p.load_vaults().await?.into_iter().collect();
        self.faces = p
            .load_faces()
            .await?
            .into_iter()
            .map(|f| (f.id.clone(), f))
            .collect();
        self.people = p
            .load_people()
            .await?
            .into_iter()
            .map(|pe| (pe.id.clone(), pe))
            .collect();
        self.invites = p
            .load_invites()
            .await?
            .into_iter()
            .map(|i| (i.token.clone(), i))
            .collect();
        self.reset_tokens = p
            .load_reset_tokens()
            .await?
            .into_iter()
            .map(|t| (t.token.clone(), t))
            .collect();
        if let Some(s) = p.load_storage().await? {
            self.storage = s;
        }
        self.smtp = p.load_smtp().await?;
        self.sessions = p.load_sessions().await?.into_iter().collect();
        self.duplicate_groups = p.load_duplicate_groups().await?.into_iter().collect();
        Ok(())
    }

    /// Write-through the freshly-computed near-duplicate groups to Postgres (the
    /// daily `duplicates` job calls this after [`Self::detect_duplicates`]). No-op
    /// in non-persistent contexts.
    pub async fn persist_duplicates(&self) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.replace_duplicate_groups(&self.duplicate_groups).await {
                tracing::warn!("persist_duplicates failed: {e}");
            }
        }
    }

    /// Generate a single-use reset token from the OS CSPRNG: 32 random bytes,
    /// hex-encoded (64 chars). Unpredictable, replacing the old derivation.
    pub fn new_reset_token(&self) -> String {
        random_hex(32)
    }

    // ---- Bearer-token sessions (opt-in auth primitive) ----

    /// Create a fresh session for `user_id` and return its CSPRNG bearer token
    /// (64 hex chars). The token maps to the user id in [`Self::sessions`].
    pub fn create_session(&mut self, user_id: &str) -> String {
        let token = random_hex(32);
        self.sessions.insert(token.clone(), user_id.to_string());
        token
    }

    /// Resolve a bearer token to its session's user id, if any. In-memory only —
    /// prefer [`Self::resolve_session`] on the request path so cross-instance
    /// sessions (minted on another node) are honored.
    pub fn session_user(&self, token: &str) -> Option<&str> {
        self.sessions.get(token).map(|s| s.as_str())
    }

    /// Resolve a token to its user id, falling back to the shared Postgres store
    /// when this instance's in-memory cache misses (so a token minted on another
    /// instance behind the load balancer still authenticates here).
    pub async fn resolve_session(&self, token: &str) -> Option<String> {
        if let Some(uid) = self.sessions.get(token) {
            return Some(uid.clone());
        }
        if let Some(p) = &self.persistence {
            if let Ok(Some(uid)) = p.get_session(token).await {
                return Some(uid);
            }
        }
        None
    }

    /// Write-through the current in-memory state of an import batch to Postgres so
    /// a polling `GET /api/uploads/{id}` (on ANY instance) sees live per-stage
    /// progress as the enrichment task advances it. No-op if the batch is gone.
    pub async fn persist_import_batch(&self, batch_id: &str) {
        if let (Some(p), Some(b)) = (&self.persistence, self.imports.get(batch_id)) {
            if let Err(e) = p.upsert_import_batch(b).await {
                tracing::warn!("persist_import_batch({batch_id}) failed: {e}");
            }
        }
    }

    /// Write-through a freshly-created session to the shared store (no-op when
    /// `DATABASE_URL` is unset).
    pub async fn persist_session(&self, token: &str) {
        if let (Some(p), Some(uid)) = (&self.persistence, self.sessions.get(token)) {
            if let Err(e) = p.upsert_session(token, uid, &now_rfc3339()).await {
                tracing::warn!("persist_session failed: {e}");
            }
        }
    }

    /// Drop a session by token (in-memory). Returns true if one was removed.
    pub fn end_session(&mut self, token: &str) -> bool {
        self.sessions.remove(token).is_some()
    }

    /// Delete a session from the shared store too (logout must invalidate it on
    /// every instance, not just this one's cache). The logout handler now clones
    /// the pool handle and calls [`crate::db::Persistence::delete_session`]
    /// directly (so it never holds the global write lock across the DB await); this
    /// remains as the parallel of [`Self::persist_session`].
    #[allow(dead_code)]
    pub async fn delete_session_persisted(&self, token: &str) {
        if let Some(p) = &self.persistence {
            if let Err(e) = p.delete_session(token).await {
                tracing::warn!("delete_session failed: {e}");
            }
        }
    }

    // ---- Brute-force lockout (login / vault PIN) ----

    /// True if `key` is currently locked out (too many recent failures).
    pub fn rate_locked(&self, key: &str) -> bool {
        match self.lockouts.get(key).and_then(|l| l.until.as_deref()) {
            Some(until) => now_rfc3339().as_str() < until,
            None => false,
        }
    }

    /// Record a failed attempt; locks the key for `RATE_LOCK_SECS` once the
    /// threshold is reached.
    pub fn rate_fail(&mut self, key: &str) {
        let l = self.lockouts.entry(key.to_string()).or_default();
        l.fails += 1;
        if l.fails >= RATE_MAX_FAILS {
            l.until = Some(rfc3339_in(RATE_LOCK_SECS));
            l.fails = 0;
        }
    }

    /// Clear the counter after a successful attempt.
    pub fn rate_reset(&mut self, key: &str) {
        self.lockouts.remove(key);
    }

    // ---- Background job state (Feature 3) ----

    /// Mark a job as running (called at the start of a job pass). Clears any stale
    /// progress from a previous run.
    pub fn job_running(&mut self, name: &str) {
        let j = self
            .jobs
            .entry(name.to_string())
            .or_insert_with(|| JobStats::new(name));
        j.status = "running".to_string();
        j.progress = None;
    }

    /// Record a live progress snapshot reported by a (plugin) job mid-run.
    pub fn job_progress(&mut self, name: &str, progress: JobProgress) {
        let j = self
            .jobs
            .entry(name.to_string())
            .or_insert_with(|| JobStats::new(name));
        j.progress = Some(progress);
    }

    /// Mark a job idle and record its last run time + short result string.
    pub fn job_done(&mut self, name: &str, result: impl Into<String>) {
        let j = self
            .jobs
            .entry(name.to_string())
            .or_insert_with(|| JobStats::new(name));
        j.status = "idle".to_string();
        j.last_run_at = Some(now_rfc3339());
        j.last_result = Some(result.into());
    }

    // ---- Gravatar ----

    /// A user with its EFFECTIVE avatar resolved for API responses. When the
    /// Gravatar feature is enabled app-wide, the email-derived Gravatar is used
    /// for everyone (there is no custom-avatar upload, so the stored value is just
    /// a placeholder). When disabled, the stored `avatar_url` is returned as-is.
    pub fn public_user(&self, u: &User) -> User {
        let mut out = u.clone();
        if self.storage.gravatar_enabled {
            out.avatar_url = gravatar_url(&u.email);
        }
        out
    }

    // ---- Authorization audit (Feature 4) ----

    /// Whether `user_id` legitimately may read `photo_id`: they own it, OR it is
    /// in an album they own, OR it is in an album shared to them (directly or via
    /// a group). This is the set of LEGITIMATE grants the read surfaces must not
    /// exceed.
    pub(crate) fn allowed(&self, user_id: &str, photo_id: &str) -> bool {
        if let Some(p) = self.photos.get(photo_id) {
            if p.owner_id == user_id {
                return true;
            }
            // PARTNER grant: the photo's owner declared `user_id` as a partner.
            if let Some(owner) = self.users.get(&p.owner_id) {
                if owner.partners.iter().any(|g| g == user_id) {
                    return true;
                }
            }
        }
        for album in self.albums.values() {
            if !album.photo_ids.iter().any(|pid| pid == photo_id) {
                continue;
            }
            if album.owner_id == user_id || self.album_shared_to(album, user_id) {
                return true;
            }
        }
        false
    }

    /// Self-audit: for every user and every photo exposed by a read surface
    /// (`timeline_photos` and `search(_, "")`), assert the photo is `allowed`
    /// AND is not archived/trashed/in-any-vault. Any breach is reported as an
    /// [`AccessViolation`]. An empty result means the system is sound.
    pub fn audit_access(&self) -> Vec<AccessViolation> {
        let mut violations = Vec::new();
        for uid in self.users.keys() {
            // Collect the photo ids each read surface exposes to this user.
            let mut surfaced: Vec<String> = Vec::new();
            surfaced.extend(self.timeline_photos(uid).into_iter().map(|p| p.id));
            surfaced.extend(self.search(uid, "").into_iter().map(|p| p.id));

            for pid in surfaced {
                if !self.allowed(uid, &pid) {
                    violations.push(AccessViolation {
                        user_id: uid.clone(),
                        photo_id: pid.clone(),
                        reason: "surfaced without a legitimate grant".to_string(),
                    });
                    continue;
                }
                // A legitimately-allowed photo must still never be exposed by a
                // timeline/search surface while archived/trashed/vaulted.
                if let Some(p) = self.photos.get(&pid) {
                    if p.archived {
                        violations.push(AccessViolation {
                            user_id: uid.clone(),
                            photo_id: pid.clone(),
                            reason: "archived photo leaked to a read surface".to_string(),
                        });
                    } else if p.deleted_at.is_some() {
                        violations.push(AccessViolation {
                            user_id: uid.clone(),
                            photo_id: pid.clone(),
                            reason: "trashed photo leaked to a read surface".to_string(),
                        });
                    } else if self.is_in_any_vault(&pid) {
                        violations.push(AccessViolation {
                            user_id: uid.clone(),
                            photo_id: pid.clone(),
                            reason: "vaulted photo leaked to a read surface".to_string(),
                        });
                    }
                }
            }
        }
        violations
    }

    /// Resolved views of a user's vault contents (no exclusion filtering here —
    /// this is the authenticated unlock path). Sorted newest-first.
    pub fn vault_views(&self, user_id: &str) -> Vec<PhotoView> {
        let mut photos: Vec<&Photo> = match self.vaults.get(user_id) {
            Some(v) => v
                .photo_ids
                .iter()
                .filter_map(|pid| self.photos.get(pid))
                .collect(),
            None => Vec::new(),
        };
        photos.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
        photos.into_iter().map(|p| p.effective()).collect()
    }

    /// Search photos reachable by `user_id` and matching `q` (case-insensitive
    /// substring; empty `q` ⇒ everything in scope). Scope is wider than the
    /// timeline: it includes EVERY photo of any album the user can access
    /// (shared to them directly/via group, or their own albums), plus the
    /// user's own photos. Archived, trashed and vault photos are excluded.
    /// Deduplicated by id; sorted newest-first.
    /// Free-text-only search (used by tests and the bare `?q=` path).
    pub fn search(&self, user_id: &str, q: &str) -> Vec<PhotoView> {
        self.search_filtered(user_id, &SearchFilters { q: q.to_string(), ..Default::default() })
    }

    /// Search across the user's accessible photos with structured filters +
    /// free text. Scope = own photos + every photo of any album the user can
    /// access (incl. non-owned photos in shared albums); excludes
    /// trashed/archived/vaulted photos. Engine-agnostic: the same filter set can
    /// later be pushed down to Postgres FTS / a dedicated index unchanged.
    pub fn search_filtered(&self, user_id: &str, f: &SearchFilters) -> Vec<PhotoView> {
        let needle = f.q.trim().to_lowercase();
        let camera = f.camera.as_ref().map(|s| s.to_lowercase());
        let place = f.place.as_ref().map(|s| s.to_lowercase());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<&Photo> = Vec::new();

        let include = |p: &Photo| -> bool {
            p.deleted_at.is_none() && !p.archived && !self.is_in_any_vault(&p.id)
        };

        for p in self.photos.values() {
            if p.owner_id == user_id && include(p) && seen.insert(p.id.clone()) {
                out.push(p);
            }
        }
        for album in self.albums.values() {
            let accessible = album.owner_id == user_id || self.album_shared_to(album, user_id);
            if !accessible {
                continue;
            }
            for pid in &album.photo_ids {
                if let Some(p) = self.photos.get(pid) {
                    if include(p) && seen.insert(p.id.clone()) {
                        out.push(p);
                    }
                }
            }
        }
        // PARTNER grants: include the live photos of every user who declared
        // `user_id` as a partner (same trash/archive/vault exclusions, deduped).
        let grantors = self.partner_grantors(user_id);
        if !grantors.is_empty() {
            for p in self.photos.values() {
                if grantors.contains(&p.owner_id.as_str())
                    && include(p)
                    && seen.insert(p.id.clone())
                {
                    out.push(p);
                }
            }
        }

        let mut matched: Vec<&Photo> = out
            .into_iter()
            .filter(|p| needle.is_empty() || photo_matches(p, &needle))
            .filter(|p| match &camera {
                Some(c) => p.exif.camera.as_ref().is_some_and(|cam| cam.to_lowercase().contains(c)),
                None => true,
            })
            .filter(|p| match &place {
                Some(pl) => {
                    let v = p.effective();
                    v.city.as_ref().is_some_and(|c| c.to_lowercase().contains(pl))
                        || v.country.as_ref().is_some_and(|c| c.to_lowercase().contains(pl))
                }
                None => true,
            })
            .filter(|p| {
                let date = p.effective_taken_at().get(0..10).unwrap_or("").to_string();
                f.from.as_ref().is_none_or(|from| date.as_str() >= from.as_str())
                    && f.to.as_ref().is_none_or(|to| date.as_str() <= to.as_str())
            })
            .filter(|p| match f.near {
                Some((lat, lng, radius_km)) => {
                    let v = p.effective();
                    match (parse_coord(v.lat.as_deref()), parse_coord(v.lng.as_deref())) {
                        (Some(plat), Some(plng)) => haversine_km(lat, lng, plat, plng) <= radius_km,
                        _ => false,
                    }
                }
                None => true,
            })
            .collect();
        matched.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
        matched.into_iter().map(|p| p.effective()).collect()
    }

    /// Per-user storage usage. `used_mb` = sum of the user's photos (primary +
    /// companions). `total_mb` = the user's explicit `quota_mb` if set; otherwise
    /// the filesystem capacity (Filesystem mode) or the mandatory S3 default quota
    /// (S3Replacement mode — no filesystem size exists for an object store).
    pub fn user_storage(&self, user_id: &str) -> (f64, f64) {
        let used_mb: f64 = self
            .photos
            .values()
            .filter(|p| p.owner_id == user_id)
            .map(|p| p.size_mb + p.companions.iter().map(|c| c.size_mb).sum::<f64>())
            .sum();
        let total_mb = match self.users.get(user_id).and_then(|u| u.quota_mb) {
            Some(q) => q as f64,
            None => match self.storage.mode {
                StorageMode::S3Replacement => DEFAULT_S3_QUOTA_MB as f64,
                StorageMode::Filesystem => {
                    fs_total_mb(&self.data_dir).unwrap_or(DEFAULT_FS_FALLBACK_MB)
                }
            },
        };
        (used_mb, total_mb)
    }

    /// Ingest a batch of uploaded files for `owner_id`, grouping companion files
    /// (same effective capture date + same base name, different extension) into a
    /// single logical photo. Returns the ids of the created photos.
    ///
    /// Within a group the primary is chosen by display priority (displayable
    /// raster > video > raw); the remaining files become downloadable companions.
    pub fn ingest_upload(&mut self, owner_id: &str, files: Vec<UploadedFile>) -> Vec<String> {
        // Group key = (date part of taken_at, lowercased base name).
        // Preserve first-seen order of groups for deterministic output.
        let mut order: Vec<(String, String)> = Vec::new();
        let mut groups: HashMap<(String, String), Vec<UploadedFile>> = HashMap::new();
        for f in files {
            let date = f.taken_at.get(0..10).unwrap_or("").to_string();
            let key = (date, base_name(&f.filename));
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(f);
        }

        let mut created = Vec::new();
        for key in order {
            let mut group = groups.remove(&key).unwrap();
            // Pick primary: lowest display priority, ties broken by filename.
            group.sort_by(|a, b| {
                display_priority(&a.ext)
                    .cmp(&display_priority(&b.ext))
                    .then_with(|| a.filename.cmp(&b.filename))
            });
            let primary = group.remove(0);

            let companions: Vec<Companion> = group
                .into_iter()
                .map(|c| {
                    let kind = classify(&c.ext);
                    Companion {
                        filename: c.filename,
                        ext: c.ext,
                        kind: kind.to_string(),
                        size_mb: c.size_mb,
                        downloadable: true,
                    }
                })
                .collect();

            let id = self.next_id("ph");
            let seed = primary.seed.unwrap_or_else(|| {
                // deterministic-ish placeholder seed from the filename bytes
                let s: u32 = primary.filename.bytes().map(|b| b as u32).sum();
                100 + (s % 900)
            });
            let exif = Exif {
                camera: primary.camera,
                lens: primary.lens,
                iso: primary.iso,
                shutter: primary.shutter,
                fnum: primary.fnum,
                focal: primary.focal,
                taken_at: primary.taken_at,
                width: primary.width,
                height: primary.height,
                city: primary.city,
                country: primary.country,
                lat: primary.lat,
                lng: primary.lng,
            };
            let photo = Photo {
                id: id.clone(),
                owner_id: owner_id.to_string(),
                filename: primary.filename,
                seed,
                kind: photo_kind(&primary.ext).to_string(),
                exif,
                overrides: MetadataOverride::default(),
                companions,
                archived: false,
                deleted_at: None,
                backed_up: false,
                // Synthetic ingest (seed/tests) has no real bytes to transcode,
                // so there is no thumbnail. ingest_upload_bytes sets thumb_url once
                // it has generated real thumbnail bytes.
                thumb_url: None,
                size_mb: primary.size_mb,
                // AI analysis (stage 4) runs later (sync in ingest_upload_bytes,
                // or via the background ai_analysis job for the seed/demo path).
                ocr_text: None,
                ai_tags: Vec::new(),
                ai_people: Vec::new(),
                analyzed: false,
                // CONTEXT RECOGNITION (CLIP): filled later by the ML sidecar when
                // PHOTON_ML_URL is set; stays None in offline mode.
                clip_embedding: None,
                full_url: None,
            };
            self.photos.insert(id.clone(), photo);
            created.push(id);
        }

        created
    }

    /// Ingest raw uploaded bytes, extracting EXIF/dimensions via the given
    /// [`MetadataExtractor`] (pure-Rust, never ImageMagick). Each file is a
    /// `(filename, ext, bytes)` tuple. Returns the created photo ids.
    ///
    /// Unlike [`ingest_upload`] (which takes pre-parsed `UploadedFile`s for the
    /// seed/demo path), this path is the one a real upload handler would use:
    /// it derives an `UploadedFile` from the extracted `Exif` and then reuses
    /// the same companion-grouping logic.
    /// Ingest ONE uploaded file, pairing it (owner-scoped) with an existing photo
    /// of the SAME base name when present — any arrival order. A RAW attaches to
    /// its primary as a companion; a displayable primary ADOPTS an earlier
    /// standalone RAW (the RAW becomes its companion). Returns
    /// `(photo_id, needs_face_detection)` — true only when a new primary image was
    /// created/adopted (a plain companion attach doesn't change the displayed
    /// image). The caller holds the per-base advisory lock and persists afterward.
    pub async fn ingest_single_file<E: MetadataExtractor>(
        &mut self,
        owner_id: &str,
        filename: &str,
        ext: &str,
        bytes: Vec<u8>,
        extractor: &E,
    ) -> (String, bool) {
        let base = base_name(filename);
        let incoming_raw = is_raw(ext);
        let existing = self
            .photos
            .values()
            .find(|p| p.owner_id == owner_id && p.deleted_at.is_none() && base_name(&p.filename) == base)
            .map(|p| (p.id.clone(), p.kind == "raw"));

        match existing {
            // A RAW for an existing photo → attach as a companion (no re-detect).
            Some((eid, _)) if incoming_raw => {
                self.attach_companion(&eid, filename, ext, bytes).await;
                (eid, false)
            }
            // A displayable primary arriving for a standalone RAW → adopt it.
            Some((eid, true)) => {
                self.adopt_into_primary(&eid, filename, ext, bytes, extractor).await;
                (eid, true)
            }
            // Duplicate of an existing primary → keep the existing photo as-is.
            Some((eid, false)) => (eid, false),
            // Brand new photo.
            None => {
                let ids = self.ingest_upload_bytes(
                    owner_id,
                    vec![(filename.to_string(), ext.to_string(), bytes)],
                    extractor,
                );
                let id = ids.into_iter().next().unwrap_or_default();
                self.store_originals(std::slice::from_ref(&id)).await;
                self.store_thumbnails(std::slice::from_ref(&id)).await;
                (id, true)
            }
        }
    }

    /// Attach a file to `photo_id` as a downloadable companion (bytes pushed to the
    /// backend under `companions/{id}.{ext}`). Replaces any companion of the same ext.
    async fn attach_companion(&mut self, photo_id: &str, filename: &str, ext: &str, bytes: Vec<u8>) {
        let kind = classify(ext);
        let size_mb = bytes.len() as f64 / (1024.0 * 1024.0);
        let ext_l = ext.to_lowercase();
        let mime = MediaFormat::from_ext(&ext_l)
            .map(|f| f.mime().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        match self.photos.get_mut(photo_id) {
            Some(p) => {
                p.companions.retain(|c| c.ext.to_lowercase() != ext_l);
                p.companions.push(crate::models::Companion {
                    filename: filename.to_string(),
                    ext: ext_l.clone(),
                    kind: kind.to_string(),
                    size_mb,
                    downloadable: true,
                });
            }
            None => return,
        }
        self.companion_bytes
            .insert((photo_id.to_string(), ext_l), (bytes, filename.to_string(), mime));
        self.store_companions(std::slice::from_ref(&photo_id.to_string())).await;
    }

    /// Promote a displayable file to be `photo_id`'s primary, demoting the existing
    /// (standalone RAW) primary to a companion. Used when a JPG arrives AFTER its RAW.
    async fn adopt_into_primary<E: MetadataExtractor>(
        &mut self,
        photo_id: &str,
        filename: &str,
        ext: &str,
        bytes: Vec<u8>,
        extractor: &E,
    ) {
        // The current primary (a RAW) becomes a companion: read its bytes first.
        let (raw_filename, raw_ext) = match self.photos.get(photo_id) {
            Some(p) => (
                p.filename.clone(),
                p.filename.rsplit('.').next().unwrap_or("").to_lowercase(),
            ),
            None => return,
        };
        let raw_bytes = self.load_original(photo_id).await.map(|(b, _)| b);
        let ex = extractor.extract(&bytes, filename);
        if let Some(p) = self.photos.get_mut(photo_id) {
            let raw_size = raw_bytes
                .as_ref()
                .map(|b| b.len() as f64 / (1024.0 * 1024.0))
                .unwrap_or(p.size_mb);
            p.companions.retain(|c| c.ext.to_lowercase() != raw_ext);
            p.companions.push(crate::models::Companion {
                filename: raw_filename.clone(),
                ext: raw_ext.clone(),
                kind: classify(&raw_ext).to_string(),
                size_mb: raw_size,
                downloadable: true,
            });
            p.filename = filename.to_string();
            p.kind = photo_kind(ext).to_string();
            p.size_mb = bytes.len() as f64 / (1024.0 * 1024.0);
            p.exif = Exif {
                camera: ex.camera,
                lens: ex.lens,
                iso: ex.iso,
                shutter: ex.shutter,
                fnum: ex.fnum,
                focal: ex.focal,
                taken_at: if ex.taken_at.is_empty() { now_rfc3339() } else { ex.taken_at },
                width: ex.width,
                height: ex.height,
                city: ex.city,
                country: ex.country,
                lat: ex.lat,
                lng: ex.lng,
            };
        } else {
            return;
        }
        if let Some(rb) = raw_bytes {
            let mime = MediaFormat::from_ext(&raw_ext)
                .map(|f| f.mime().to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            self.companion_bytes
                .insert((photo_id.to_string(), raw_ext), (rb, raw_filename, mime));
        }
        self.generate_thumbnail_step(photo_id, &bytes);
        self.store_original(photo_id, ext, &bytes);
        let id = photo_id.to_string();
        self.store_originals(std::slice::from_ref(&id)).await;
        self.store_thumbnails(std::slice::from_ref(&id)).await;
        self.store_companions(std::slice::from_ref(&id)).await;
    }

    pub fn ingest_upload_bytes<E: MetadataExtractor>(
        &mut self,
        owner_id: &str,
        files: Vec<(String, String, Vec<u8>)>,
        extractor: &E,
    ) -> Vec<String> {
        // Keep the raw bytes by filename so that, after the primary photo is
        // created, we can generate a thumbnail from its ORIGINAL source bytes
        // (the original is never modified).
        let mut bytes_by_name: HashMap<String, Vec<u8>> = HashMap::new();
        let parsed: Vec<UploadedFile> = files
            .into_iter()
            .map(|(filename, ext, bytes)| {
                let ex = extractor.extract(&bytes, &filename);
                let taken_at = if ex.taken_at.is_empty() {
                    now_rfc3339()
                } else {
                    ex.taken_at.clone()
                };
                bytes_by_name.insert(filename.clone(), bytes.clone());
                UploadedFile {
                    filename,
                    ext,
                    size_mb: bytes.len() as f64 / (1024.0 * 1024.0),
                    taken_at,
                    camera: ex.camera,
                    lens: ex.lens,
                    iso: ex.iso,
                    shutter: ex.shutter,
                    fnum: ex.fnum,
                    focal: ex.focal,
                    width: ex.width,
                    height: ex.height,
                    city: ex.city,
                    country: ex.country,
                    lat: ex.lat,
                    lng: ex.lng,
                    seed: None,
                }
            })
            .collect();
        let created = self.ingest_upload(owner_id, parsed);

        // Feature 2 (Thumbnail) + stage 4 (AI analysis) per created primary
        // photo. These per-photo steps are factored into shared helpers so the
        // async import worker reuses EXACTLY the same logic.
        for id in &created {
            let filename = match self.photos.get(id) {
                Some(p) => p.filename.clone(),
                None => continue,
            };
            if let Some(src) = bytes_by_name.get(&filename) {
                self.generate_thumbnail_step(id, src);
                // Keep the UNMODIFIED original bytes (in-memory demo store) so the
                // lightbox can request the full / a screen-adapted render.
                let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
                self.store_original(id, &ext, src);
            }
            // Import stage 4: AI analysis (heuristic; cheap). The background
            // `ai_analysis` job is a safety net for photos created elsewhere.
            self.analyze_photo(id);
        }

        created
    }

    /// THUMBNAIL STAGE (shared per-photo step): generate a SMALL, metadata-
    /// stripped webp thumbnail for `id` from its source `bytes`, store it in
    /// [`Self::thumbs`], and set the photo's `thumb_url`. Re-encoding through the
    /// `image` crate strips all EXIF (the original is never modified). Failures
    /// are non-fatal: the photo simply gets no thumbnail. Returns whether a
    /// thumbnail was produced. Used by both the sync ingest helper and the async
    /// import worker so the logic is defined once.
    pub fn generate_thumbnail_step(&mut self, id: &str, bytes: &[u8]) -> bool {
        // Max edge ~320px, encode to webp. We pass 320x320 so the LONG edge is
        // clamped to 320 via the aspect-preserving fit.
        let plan = TranscodePlan {
            format: MediaFormat::Webp,
            width: 320,
            height: 320,
            source_format: MediaFormat::Jpeg,
            needs_transcode: true,
        };
        match RealTranscoder.transcode_image(bytes, &plan) {
            Ok(thumb) => {
                self.thumbs
                    .insert(id.to_string(), (thumb, MediaFormat::Webp.mime().to_string()));
                if let Some(p) = self.photos.get_mut(id) {
                    p.thumb_url = Some(format!("/api/photos/{id}/thumb"));
                }
                true
            }
            Err(_) => false,
        }
    }

    /// Pure thumbnail render (decode → fit 320px → WebP) with NO `self`/lock — safe
    /// to run in `spawn_blocking` off the write lock. Returns the encoded bytes.
    pub fn render_thumbnail_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
        let plan = TranscodePlan {
            format: MediaFormat::Webp,
            width: 320,
            height: 320,
            source_format: MediaFormat::Jpeg,
            needs_transcode: true,
        };
        RealTranscoder.transcode_image(bytes, &plan).ok()
    }

    /// Take the THUMBNAIL phase inputs under a brief lock: for each primary at
    /// `Exif` with a `photo_id`, mark it `Thumbnail`/`Processing` and CONSUME its
    /// pending bytes, returning `(file_id, photo_id, bytes)`. Files whose bytes are
    /// missing are marked errored and omitted. The caller decodes OFF the lock and
    /// calls [`Self::import_thumbnail_apply`]. This keeps the heavy image decode
    /// out of the global write lock so polling GETs stay instant during import.
    pub fn import_thumbnail_take(&mut self, batch_id: &str) -> Vec<(String, String, Vec<u8>)> {
        let primaries: Vec<(String, String)> = self.import_primary_items(batch_id, |i| {
            i.stage == crate::models::ImportStage::Exif && i.photo_id.is_some()
        });
        let mut out = Vec::new();
        for (file_id, photo_id) in primaries {
            self.with_import_item(batch_id, &file_id, |it| {
                it.stage = crate::models::ImportStage::Thumbnail;
                it.status = crate::models::ImportStatus::Processing;
            });
            match self.pending_bytes.remove(&(batch_id.to_string(), file_id.clone())) {
                Some((_fname, _ext, bytes)) => out.push((file_id, photo_id, bytes)),
                None => self.with_import_item(batch_id, &file_id, |it| {
                    it.status = crate::models::ImportStatus::Error;
                    it.error = Some("missing upload bytes".to_string());
                }),
            }
        }
        out
    }

    /// Apply an off-lock-generated thumbnail (or mark the item errored).
    pub fn import_thumbnail_apply(
        &mut self,
        batch_id: &str,
        file_id: &str,
        photo_id: &str,
        thumb: Option<Vec<u8>>,
    ) {
        let ok = match thumb {
            Some(bytes) => {
                self.thumbs
                    .insert(photo_id.to_string(), (bytes, MediaFormat::Webp.mime().to_string()));
                if let Some(p) = self.photos.get_mut(photo_id) {
                    p.thumb_url = Some(format!("/api/photos/{photo_id}/thumb"));
                }
                true
            }
            None => false,
        };
        // A lone RAW (e.g. an .ARW with no paired JPG) becomes its own photo but
        // can't be decoded for a thumbnail — that's EXPECTED, not an import error.
        // It imports fine (the UI shows a placeholder). (Paired RAWs never reach
        // here: they collapse into the JPG as a companion.)
        let is_raw_photo = self.photos.get(photo_id).map(|p| p.kind == "raw").unwrap_or(false);
        self.with_import_item(batch_id, file_id, |it| {
            if ok || is_raw_photo {
                it.status = crate::models::ImportStatus::Ok;
            } else {
                it.status = crate::models::ImportStatus::Error;
                it.error = Some("thumbnail generation failed".to_string());
            }
        });
    }

    /// Best-effort push of all freshly-generated thumbnails to the configured
    /// `StorageBackend` under `thumbs/{id}.webp`. Kept separate from the sync
    /// ingest so the (async) storage write happens outside the ingest borrow;
    /// the upload handler calls this after ingest. Errors are swallowed.
    pub async fn store_thumbnails(&self, ids: &[String]) {
        let backend = self.active_backend();
        for id in ids {
            if let Some((bytes, _ct)) = self.thumbs.get(id) {
                let key = format!("thumbs/{id}.webp");
                let _ = backend.put_object(&key, bytes).await;
            }
        }
    }

    /// Store a photo's UNMODIFIED ORIGINAL bytes in the in-memory [`Self::originals`]
    /// map, with a MIME type guessed from `ext` (falling back to
    /// `application/octet-stream`). This is the demo convenience store that backs
    /// `GET /api/photos/{id}/original` and `GET /api/photos/{id}/render`; the
    /// configured `StorageBackend` (see [`Self::store_originals`]) is the REAL
    /// blob store. Synchronous (no I/O): the backend push happens separately.
    pub fn store_original(&mut self, id: &str, ext: &str, bytes: &[u8]) {
        let mime = MediaFormat::from_ext(ext)
            .map(|f| f.mime().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        self.originals
            .insert(id.to_string(), (bytes.to_vec(), mime));
        // Surface the render URL on the photo so every PhotoView gets `full_url`.
        if let Some(p) = self.photos.get_mut(id) {
            p.full_url = Some(format!("/api/photos/{id}/render"));
        }
    }

    /// Best-effort push of the freshly-stored ORIGINAL bytes to the configured
    /// `StorageBackend` under `originals/{id}.{ext}` (the real store for blobs —
    /// filesystem / S3 — since originals don't belong in Postgres or memory).
    /// Mirrors [`Self::store_thumbnails`]: kept separate from the sync ingest so
    /// the (async) storage write happens outside the ingest borrow; the import
    /// finalize phase calls it. Errors are swallowed (non-fatal).
    pub async fn store_originals(&self, ids: &[String]) {
        let backend = self.active_backend();
        for id in ids {
            if let Some((bytes, ct)) = self.originals.get(id) {
                let ext = MediaFormat::from_mime(ct).map(|f| f.ext()).unwrap_or("bin");
                let key = format!("originals/{id}.{ext}");
                let _ = backend.put_object(&key, bytes).await;
            }
        }
    }

    /// The minimal storage config ([`StorageCtx`]) needed to read/write blobs,
    /// cloned out of this state. Handlers that only serve a single photo's bytes
    /// grab this under a brief lock instead of building a throwaway `AppState`.
    pub fn storage_ctx(&self) -> StorageCtx {
        StorageCtx {
            data_dir: self.data_dir.clone(),
            storage: self.storage.clone(),
        }
    }

    /// The configured object-storage backend: S3 when in replacement mode with a
    /// valid `primary_s3` config, else the local filesystem. All blob reads/writes
    /// go through this so the configured storage mode is honored consistently
    /// (previously the store helpers hard-coded the filesystem even in S3 mode).
    pub fn active_backend(&self) -> Box<dyn StorageBackend> {
        self.storage_ctx().active_backend()
    }

    /// DURABLE blob write for the upload path: push the originals + companions of
    /// `ids` to the active backend, returning an error on the FIRST failure
    /// (unlike the best-effort `store_originals`/`store_companions`). The upload
    /// handler awaits this BEFORE acking, so a crash after the response can never
    /// lose an uploaded photo's bytes.
    #[allow(dead_code)] // durability primitive, exercised by its unit test
    pub async fn persist_blobs_durable(
        &self,
        ids: &[String],
    ) -> Result<(), crate::storage::StorageError> {
        let backend = self.active_backend();
        for id in ids {
            if let Some((bytes, ct)) = self.originals.get(id) {
                let ext = MediaFormat::from_mime(ct).map(|f| f.ext()).unwrap_or("bin");
                backend.put_object(&format!("originals/{id}.{ext}"), bytes).await?;
            }
        }
        for ((photo_id, ext), (bytes, _fname, _mime)) in &self.companion_bytes {
            if ids.iter().any(|id| id == photo_id) {
                backend.put_object(&format!("companions/{photo_id}.{ext}"), bytes).await?;
            }
        }
        Ok(())
    }

    /// COMPANION DOWNLOAD: best-effort push of every kept companion file (across
    /// the given primary photo ids) to the configured `StorageBackend` under
    /// `companions/{photo_id}.{ext}` — the AUTHORITATIVE blob store, mirroring
    /// [`Self::store_originals`]. The in-memory [`Self::companion_bytes`] map is
    /// only a demo convenience. Errors are swallowed (non-fatal).
    pub async fn store_companions(&self, ids: &[String]) {
        let backend = self.active_backend();
        for ((photo_id, ext), (bytes, _fname, _mime)) in &self.companion_bytes {
            if ids.iter().any(|id| id == photo_id) {
                let key = format!("companions/{photo_id}.{ext}");
                let _ = backend.put_object(&key, bytes).await;
            }
        }
    }

    /// The canonical storage ext + mime for a photo's ORIGINAL, derived from its
    /// filename — matches the key written by [`Self::persist_blobs_durable`].
    fn original_key_parts(&self, id: &str) -> Option<(String, String)> {
        let raw = self.photos.get(id)?.filename.rsplit('.').next()?.to_lowercase();
        let fmt = MediaFormat::from_ext(&raw);
        let ext = fmt.map(|f| f.ext()).unwrap_or("bin").to_string();
        let mime = fmt
            .map(|f| f.mime().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Some((ext, mime))
    }

    /// Load a photo's ORIGINAL bytes: in-RAM cache first, then the backend
    /// (FS/S3). Returns `(bytes, mime)`. After import the cache is evicted (see
    /// [`Self::evict_heavy_blobs`]) so this normally streams from the backend —
    /// the whole library's originals never sit resident in RAM.
    pub async fn load_original(&self, id: &str) -> Option<(Vec<u8>, String)> {
        if let Some((b, ct)) = self.originals.get(id) {
            return Some((b.clone(), ct.clone()));
        }
        let (ext, mime) = self.original_key_parts(id)?;
        let key = format!("originals/{id}.{ext}");
        self.active_backend()
            .get_object(&key)
            .await
            .ok()
            .flatten()
            .map(|b| (b, mime))
    }

    /// Load a photo's THUMBNAIL bytes (cache → backend). Thumbnails are pushed to
    /// the backend at `thumbs/{id}.webp` during import, so an instance whose RAM
    /// cache never held this thumb (e.g. a different node, or after restart) still
    /// serves it.
    pub async fn load_thumb(&self, id: &str) -> Option<(Vec<u8>, String)> {
        if let Some((b, ct)) = self.thumbs.get(id) {
            return Some((b.clone(), ct.clone()));
        }
        let key = format!("thumbs/{id}.webp");
        self.active_backend()
            .get_object(&key)
            .await
            .ok()
            .flatten()
            .map(|b| (b, MediaFormat::Webp.mime().to_string()))
    }

    /// True if this photo has a plugin-EDITED version stored as a companion.
    pub fn has_edited_version(photo: &crate::models::Photo) -> bool {
        photo.companions.iter().any(|c| c.ext == EDITED_EXT)
    }

    /// Drop the HEAVY blobs (full-res originals + RAW companions) of `ids` from the
    /// in-RAM caches once they're durably on the backend (called after import
    /// finalize). Keeps RAM from holding the whole library; thumbnails (small)
    /// stay cached. Reads then fall back to the backend via `load_*`.
    #[allow(dead_code)]
    pub fn evict_heavy_blobs(&mut self, ids: &[String]) {
        for id in ids {
            self.originals.remove(id);
        }
        self.companion_bytes
            .retain(|(pid, _), _| !ids.iter().any(|id| id == pid));
    }

    // ---- Context recognition (CLIP) ----

    /// CONTEXT RECOGNITION (CLIP): embed each photo's thumbnail bytes via the ML
    /// sidecar and store the resulting vector in `Photo::clip_embedding`. Runs as
    /// part of the AI-analysis stage of import (the upload handler calls this
    /// after thumbnail generation). It is a NO-OP when ML is disabled
    /// (`PHOTON_ML_URL` unset ⇒ `self.ml` is `None`) — then no network is used
    /// and offline behavior is exactly as before. Each embedding is best-effort:
    /// any failure (or a missing thumbnail) just leaves `clip_embedding = None`,
    /// never failing the upload. Kept async + outside the sync ingest because the
    /// embedding is an HTTP call.
    pub async fn embed_photos(&mut self, ids: &[String]) {
        let client = match &self.ml {
            Some(c) => c.clone(),
            None => return, // ML disabled (offline): nothing to do, no network.
        };
        for id in ids {
            let Some((bytes, _ct)) = self.thumbs.get(id) else {
                continue; // no thumbnail to embed
            };
            if let Some(embedding) = client.embed_image(bytes.clone()).await {
                if let Some(p) = self.photos.get_mut(id) {
                    p.clip_embedding = Some(embedding);
                }
            }
        }
    }

    /// OCR (text recognition): run the ML sidecar's OCR over each photo and set
    /// `Photo::ocr_text` from the recognized text. Like [`Self::embed_photos`],
    /// this is part of the AI-analysis stage of import (called in
    /// [`Self::import_phase_finalize`]) and is a NO-OP when ML is disabled
    /// (`PHOTON_ML_URL` unset ⇒ `self.ml` is `None`) — no network, offline
    /// behavior unchanged. OCR runs on the BEST available bytes for the photo:
    /// the original upload bytes still in `pending_bytes` if present, else the
    /// stored thumbnail bytes from [`Self::thumbs`]. The recognized text is only
    /// written when non-empty (a blank result leaves the existing `ocr_text`
    /// untouched), and any failure is best-effort — it never fails the upload.
    /// `ocr_text` is already searchable (see `photo_matches`).
    pub async fn ocr_photos(&mut self, batch_id: Option<&str>, ids: &[String]) {
        let client = match &self.ml {
            Some(c) => c.clone(),
            None => return, // ML disabled (offline): nothing to do, no network.
        };
        for id in ids {
            // RAW originals can't be decoded — skip (would 400).
            if self.photos.get(id).map(|p| p.kind == "raw").unwrap_or(false) {
                continue;
            }
            // Prefer the original upload bytes (best OCR quality) when the worker
            // still holds them; otherwise fall back to the stored thumbnail.
            let bytes = batch_id
                .and_then(|b| self.import_pending_bytes_for_photo(b, id))
                .or_else(|| self.thumbs.get(id).map(|(b, _ct)| b.clone()));
            let Some(bytes) = bytes else {
                continue; // no bytes to OCR
            };
            if let Some(text) = client.ocr(bytes).await {
                if let Some(p) = self.photos.get_mut(id) {
                    p.ocr_text = Some(text); // only set on non-empty (ocr() returns None when blank)
                }
            }
        }
    }

    // ---- Face recognition ----

    /// FACE DETECTION: run the ML sidecar's `/faces` over the best bytes for each
    /// photo (the original upload bytes still in `pending_bytes` if present, else
    /// the stored thumbnail) and store the detected faces in [`Self::faces`].
    /// Part of the AI-analysis stage of import (called in
    /// [`Self::import_phase_finalize`]); a NO-OP when ML is disabled
    /// (`PHOTON_ML_URL` unset ⇒ `self.ml` is `None`) — no network, offline
    /// behavior unchanged. Re-detecting a photo first drops its previous faces so
    /// repeated runs don't accumulate duplicates. Embeddings are stored
    /// server-side only and never serialized. Best-effort: any failure leaves the
    /// photo with no faces and never fails the upload.
    pub async fn detect_faces(&mut self, batch_id: Option<&str>, ids: &[String]) {
        let client = match &self.ml {
            Some(c) => c.clone(),
            None => return, // ML disabled (offline): nothing to do, no network.
        };
        for id in ids {
            // Skip RAW photos: the detector can't decode RAW (it would 400). A
            // RAW that's a JPG's companion is never its own photo anyway.
            if self.photos.get(id).map(|p| p.kind == "raw").unwrap_or(false) {
                continue;
            }
            // Detect on FULL-RES bytes (crucial: small/distant faces are lost in the
            // 320px thumbnail). Prefer the staged upload buffer, else the ORIGINAL
            // from the storage backend (written durably in import stage 2, before
            // this async finalize runs), else — last resort — the thumbnail. Boxes
            // are remapped to original-pixel coordinates below regardless.
            let bytes = if let Some(b) =
                batch_id.and_then(|bid| self.import_pending_bytes_for_photo(bid, id))
            {
                Some(b)
            } else if let Some((b, _ct)) = self.load_original(id).await {
                Some(b)
            } else {
                self.thumbs.get(id).map(|(b, _ct)| b.clone())
            };
            let Some(bytes) = bytes else {
                continue; // no bytes to scan
            };
            let (owner_id, ow, oh) = match self.photos.get(id) {
                Some(p) => (p.owner_id.clone(), p.exif.width, p.exif.height),
                None => continue,
            };
            // Dimensions of the bytes we actually scan (may be the thumbnail), to
            // map detector boxes back into ORIGINAL-pixel coordinates — which is
            // what the `/faces` endpoint reports as source_width/height, so the UI
            // scales boxes correctly. Reads just the image header (cheap).
            let (sw, sh) = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .ok()
                .and_then(|r| r.into_dimensions().ok())
                .unwrap_or((ow, oh));
            let (sx, sy) = if sw > 0 && sh > 0 {
                (ow as f32 / sw as f32, oh as f32 / sh as f32)
            } else {
                (1.0, 1.0)
            };
            if let Some(detected) = client.faces(bytes).await {
                // Drop any prior faces for this photo (idempotent re-detection).
                let stale: Vec<String> = self
                    .faces
                    .values()
                    .filter(|f| f.photo_id == *id)
                    .map(|f| f.id.clone())
                    .collect();
                for fid in stale {
                    self.faces.remove(&fid);
                }
                for d in detected {
                    let fid = self.next_id("face");
                    let bbox = [d.bbox[0] * sx, d.bbox[1] * sy, d.bbox[2] * sx, d.bbox[3] * sy];
                    self.faces.insert(
                        fid.clone(),
                        crate::models::Face {
                            id: fid,
                            photo_id: id.clone(),
                            owner_id: owner_id.clone(),
                            bbox,
                            embedding: d.embedding,
                            score: d.score,
                            person_id: None,
                            ignored: false,
                            assigned_label: None,
                            confirmed: false,
                        },
                    );
                }
            }
        }
    }

    /// FACE CLUSTERING: incrementally assign every face owned by `owner` to a
    /// Person cluster by cosine similarity. Each face joins the existing person
    /// whose centroid (mean embedding) is the most similar above
    /// [`Self::FACE_CLUSTER_THRESHOLD`]; otherwise it starts a new person. Faces
    /// are processed in id order for determinism. Existing person NAMES are
    /// preserved across re-clusters by matching the new clusters back to the old
    /// ones by membership overlap (so labeling survives re-runs). Finally, sets
    /// each photo's `ai_people` to the names of the NAMED people appearing in it
    /// (so name search works — `ai_people` is in `photo_matches`). Returns the
    /// number of person clusters for the owner.
    pub fn cluster_faces(&mut self, owner: &str) -> usize {
        // Ignored faces (user-marked intruders / non-faces) never participate in
        // clustering and never carry a person.
        for f in self.faces.values_mut() {
            if f.owner_id == owner && f.ignored {
                f.person_id = None;
            }
        }
        // Snapshot this owner's NON-ignored faces (id, embedding, manual label) in
        // id order. `assigned_label` is the authoritative manual identity tag.
        let mut owner_faces: Vec<(String, Vec<f32>, Option<String>)> = self
            .faces
            .values()
            .filter(|f| f.owner_id == owner && !f.ignored && !f.embedding.is_empty())
            .map(|f| (f.id.clone(), f.embedding.clone(), f.assigned_label.clone()))
            .collect();
        owner_faces.sort_by(|a, b| a.0.cmp(&b.0));

        // Remember prior clusters by the set of faces they covered, so we can carry
        // user curation (name, birthdate, hidden, kinship, a locked cover) onto
        // whichever new cluster inherits those faces. Person ids are regenerated
        // below, so we also build an old->new id map to remap relationship targets.
        struct Prior {
            id: String,
            name: Option<String>,
            relationships: Vec<crate::models::PersonRelation>,
            birthdate: Option<String>,
            hidden: bool,
            cover_locked: bool,
            cover_photo_id: Option<String>,
            cover_bbox: Option<[f32; 4]>,
            faces: std::collections::HashSet<String>,
        }
        let prior: Vec<Prior> = self
            .people
            .values()
            .filter(|p| p.owner_id == owner)
            .map(|p| Prior {
                id: p.id.clone(),
                name: p.name.clone(),
                relationships: p.relationships.clone(),
                birthdate: p.birthdate.clone(),
                hidden: p.hidden,
                cover_locked: p.cover_locked,
                cover_photo_id: p.cover_photo_id.clone(),
                cover_bbox: p.cover_bbox,
                faces: p.face_ids.iter().cloned().collect(),
            })
            .collect();

        struct Cluster {
            face_ids: Vec<String>,
            centroid: Vec<f32>,
        }
        let mut clusters: Vec<Cluster> = Vec::new();
        let mut label_idx: HashMap<String, usize> = HashMap::new();
        let join = |c: &mut Cluster, fid: &str, emb: &[f32]| {
            // Incremental mean: centroid = (centroid*n + emb) / (n+1).
            let n = c.face_ids.len() as f32;
            for (cv, ev) in c.centroid.iter_mut().zip(emb.iter()) {
                *cv = (*cv * n + *ev) / (n + 1.0);
            }
            c.face_ids.push(fid.to_string());
        };

        // PASS 1 — seed authoritative label clusters: every face sharing an
        // `assigned_label` is forced into the same cluster (move/merge are sticky).
        for (fid, emb, label) in &owner_faces {
            let Some(lab) = label else { continue };
            if let Some(&i) = label_idx.get(lab) {
                join(&mut clusters[i], fid, emb);
            } else {
                let i = clusters.len();
                clusters.push(Cluster { face_ids: vec![fid.clone()], centroid: emb.clone() });
                label_idx.insert(lab.clone(), i);
            }
        }
        // PASS 2 — greedy similarity for the rest. An unlabeled face joins the most
        // similar existing cluster above threshold (which may be a labeled one, so
        // new photos of a curated person attach automatically), else starts anew.
        for (fid, emb, label) in &owner_faces {
            if label.is_some() {
                continue;
            }
            let mut best: Option<(usize, f32)> = None;
            for (i, c) in clusters.iter().enumerate() {
                if let Some(sim) = crate::ml::cosine_similarity(&c.centroid, emb) {
                    if sim >= Self::FACE_CLUSTER_THRESHOLD
                        && best.map(|(_, s)| sim > s).unwrap_or(true)
                    {
                        best = Some((i, sim));
                    }
                }
            }
            match best {
                Some((i, _)) => join(&mut clusters[i], fid, emb),
                None => clusters.push(Cluster { face_ids: vec![fid.clone()], centroid: emb.clone() }),
            }
        }

        // Rebuild this owner's Person entries from the fresh clusters, carrying
        // over a prior name when the new cluster's membership best overlaps it.
        self.people.retain(|_, p| p.owner_id != owner);
        // Clear the per-face person_id for this owner before reassigning.
        for f in self.faces.values_mut() {
            if f.owner_id == owner && !f.ignored {
                f.person_id = None;
            }
        }
        let count = clusters.len();
        // old person id -> new person id, by best face-set overlap (a new cluster
        // claims at most one prior cluster's identity; used to remap kinship edges).
        let mut old_to_new: HashMap<String, String> = HashMap::new();
        // new person id -> the relationships carried over from its best-overlap prior.
        let mut carried: HashMap<String, Vec<crate::models::PersonRelation>> = HashMap::new();
        for cluster in clusters {
            // Best-overlap prior cluster (carries its name + kinship + id mapping).
            let best_prior = prior
                .iter()
                .map(|pr| {
                    let overlap = cluster.face_ids.iter().filter(|f| pr.faces.contains(*f)).count();
                    (overlap, pr)
                })
                .filter(|(overlap, _)| *overlap > 0)
                .max_by_key(|(overlap, _)| *overlap)
                .map(|(_, pr)| pr);
            let name = best_prior.and_then(|pr| pr.name.clone());
            let birthdate = best_prior.and_then(|pr| pr.birthdate.clone());
            let hidden = best_prior.map(|pr| pr.hidden).unwrap_or(false);
            let pid = self.next_id("person");
            if let Some(pr) = best_prior {
                old_to_new.insert(pr.id.clone(), pid.clone());
                carried.insert(pid.clone(), pr.relationships.clone());
            }
            // Cover: keep a user-locked cover from the best-overlap prior; otherwise
            // auto-pick the face from the person's FIRST photo — earliest by date
            // taken, then upload order (photo id), then face id as a tiebreak.
            let locked_cover = best_prior.filter(|pr| pr.cover_locked && pr.cover_photo_id.is_some());
            let (cover_photo_id, cover_bbox, cover_locked) = if let Some(pr) = locked_cover {
                (pr.cover_photo_id.clone(), pr.cover_bbox, true)
            } else {
                let cover_fid = cluster.face_ids.iter().min_by_key(|fid| {
                    self.faces.get(*fid).map(|f| {
                        let taken = self
                            .photos
                            .get(&f.photo_id)
                            .map(|p| p.exif.taken_at.clone())
                            .unwrap_or_default();
                        (taken, f.photo_id.clone(), (*fid).clone())
                    })
                });
                cover_fid
                    .and_then(|fid| self.faces.get(fid))
                    .map(|f| (Some(f.photo_id.clone()), Some(f.bbox), false))
                    .unwrap_or((None, None, false))
            };
            for fid in &cluster.face_ids {
                if let Some(f) = self.faces.get_mut(fid) {
                    f.person_id = Some(pid.clone());
                }
            }
            self.people.insert(
                pid.clone(),
                crate::models::Person {
                    id: pid,
                    owner_id: owner.to_string(),
                    name,
                    face_ids: cluster.face_ids,
                    cover_photo_id,
                    cover_bbox,
                    relationships: Vec::new(),
                    birthdate,
                    hidden,
                    cover_locked,
                },
            );
        }

        // Remap carried kinship edges through old_to_new; drop edges whose target
        // cluster no longer exists, and self-loops. Both directions were stored, so
        // remapping each side independently keeps them reciprocal.
        for (new_pid, rels) in carried {
            let remapped: Vec<crate::models::PersonRelation> = rels
                .into_iter()
                .filter_map(|r| {
                    let target = old_to_new.get(&r.person_id)?;
                    if *target == new_pid {
                        return None;
                    }
                    Some(crate::models::PersonRelation {
                        person_id: target.clone(),
                        relation: r.relation,
                    })
                })
                .collect();
            if let Some(p) = self.people.get_mut(&new_pid) {
                p.relationships = remapped;
            }
        }

        self.refresh_ai_people(owner);
        count
    }

    /// Cosine-similarity threshold above which a face joins an existing person
    /// cluster (else it seeds a new one). Tuned for the AuraFace embedder, whose
    /// same-identity cosines run lower than full InsightFace ArcFace: measured
    /// same-person pairs sit ~0.38+ while different people stay ≤ ~0.23, so 0.30
    /// sits cleanly in the gap (0.5 was too strict and split identities).
    pub const FACE_CLUSTER_THRESHOLD: f32 = 0.30;

    /// Recompute `Photo.ai_people` for `owner`'s photos from the NAMED people
    /// clusters: each photo lists the distinct names of named persons that have a
    /// face on it. Unnamed clusters contribute nothing (so `ai_people` only ever
    /// holds real names — keeping name search meaningful). Does not touch other
    /// owners' photos.
    fn refresh_ai_people(&mut self, owner: &str) {
        // photo_id -> set of names (named persons appearing on that photo).
        let mut by_photo: HashMap<String, Vec<String>> = HashMap::new();
        for person in self.people.values().filter(|p| p.owner_id == owner) {
            let Some(name) = &person.name else { continue };
            for fid in &person.face_ids {
                if let Some(face) = self.faces.get(fid) {
                    let names = by_photo.entry(face.photo_id.clone()).or_default();
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
            }
        }
        // Apply to every owned photo (clearing names from photos no longer named).
        let owned: Vec<String> = self
            .photos
            .values()
            .filter(|p| p.owner_id == owner)
            .map(|p| p.id.clone())
            .collect();
        for pid in owned {
            let names = by_photo.remove(&pid).unwrap_or_default();
            if let Some(p) = self.photos.get_mut(&pid) {
                if p.ai_people != names {
                    p.ai_people = names;
                }
            }
        }
    }

    /// Name (or rename) a Person cluster, then propagate the new name into the
    /// `ai_people` of every photo that person appears in (so name search works).
    /// Returns the owner id of the renamed person (for persistence scoping), or
    /// `None` if the person is unknown.
    pub fn name_person(&mut self, person_id: &str, name: &str) -> Option<String> {
        let owner = {
            let p = self.people.get_mut(person_id)?;
            let trimmed = name.trim();
            p.name = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            p.owner_id.clone()
        };
        self.refresh_ai_people(&owner);
        Some(owner)
    }

    /// Create (or replace) a reciprocal KINSHIP link between two People of the
    /// same owner: `a` gets the edge "`b` is a's `relation`", and `b` gets the
    /// inverse relation back. Returns the owner id (for persistence scoping), or
    /// `None` if invalid (unknown person, self-link, empty relation, or the two
    /// clusters belong to different owners). At most one edge exists per ordered
    /// pair, so re-linking just updates the relation.
    pub fn link_people(&mut self, a: &str, b: &str, relation: &str) -> Option<String> {
        if a == b {
            return None;
        }
        let relation = relation.trim().to_string();
        if relation.is_empty() {
            return None;
        }
        let owner = {
            let pa = self.people.get(a)?;
            let pb = self.people.get(b)?;
            if pa.owner_id != pb.owner_id {
                return None;
            }
            pa.owner_id.clone()
        };
        let inverse = crate::models::PersonRelation::inverse(&relation).to_string();
        if let Some(pa) = self.people.get_mut(a) {
            pa.relationships.retain(|r| r.person_id != b);
            pa.relationships.push(crate::models::PersonRelation {
                person_id: b.to_string(),
                relation,
            });
        }
        if let Some(pb) = self.people.get_mut(b) {
            pb.relationships.retain(|r| r.person_id != a);
            pb.relationships.push(crate::models::PersonRelation {
                person_id: a.to_string(),
                relation: inverse,
            });
        }
        Some(owner)
    }

    /// Remove the reciprocal kinship link between two People. Returns the owner id
    /// (for persistence), or `None` if either person is unknown.
    pub fn unlink_people(&mut self, a: &str, b: &str) -> Option<String> {
        let owner = self.people.get(a)?.owner_id.clone();
        if !self.people.contains_key(b) {
            return None;
        }
        if let Some(pa) = self.people.get_mut(a) {
            pa.relationships.retain(|r| r.person_id != b);
        }
        if let Some(pb) = self.people.get_mut(b) {
            pb.relationships.retain(|r| r.person_id != a);
        }
        Some(owner)
    }

    // ---- People Studio: manual face/person curation ----
    //
    // These mutate clusters IN PLACE so a person's id stays stable across the
    // edit (better UX — the client keeps its selection). The decisions also stick
    // across a FUTURE full re-cluster because they're recorded on the stable faces
    // (`ignored`, `assigned_label`) and carried by overlap (`birthdate`, `hidden`,
    // a locked cover). A label is the authoritative "same identity" tag.

    /// Recompute (or clear) a person's cover face after its membership changed,
    /// UNLESS the cover was user-locked. Picks the earliest-photo face like
    /// clustering does.
    fn refresh_cover(&mut self, person_id: &str) {
        let (locked, face_ids) = match self.people.get(person_id) {
            Some(p) => (p.cover_locked, p.face_ids.clone()),
            None => return,
        };
        if locked {
            return;
        }
        let cover_fid = face_ids.iter().min_by_key(|fid| {
            self.faces.get(*fid).map(|f| {
                let taken =
                    self.photos.get(&f.photo_id).map(|p| p.exif.taken_at.clone()).unwrap_or_default();
                (taken, f.photo_id.clone(), (*fid).clone())
            })
        });
        let cover = cover_fid.and_then(|fid| self.faces.get(fid)).map(|f| (f.photo_id.clone(), f.bbox));
        if let Some(p) = self.people.get_mut(person_id) {
            match cover {
                Some((pid, bbox)) => {
                    p.cover_photo_id = Some(pid);
                    p.cover_bbox = Some(bbox);
                }
                None => {
                    p.cover_photo_id = None;
                    p.cover_bbox = None;
                }
            }
        }
    }

    /// Drop a person if it has no faces left; returns true if removed.
    fn prune_if_empty(&mut self, person_id: &str) -> bool {
        if self.people.get(person_id).map(|p| p.face_ids.is_empty()).unwrap_or(false) {
            self.people.remove(person_id);
            true
        } else {
            false
        }
    }

    /// Ensure every face of a person carries a shared stable identity label,
    /// minting one if needed. Returns the label.
    fn ensure_label(&mut self, person_id: &str) -> Option<String> {
        let face_ids = self.people.get(person_id)?.face_ids.clone();
        let existing = face_ids
            .iter()
            .find_map(|fid| self.faces.get(fid).and_then(|f| f.assigned_label.clone()));
        let label = existing.unwrap_or_else(|| self.next_id("label"));
        for fid in &face_ids {
            if let Some(f) = self.faces.get_mut(fid) {
                f.assigned_label = Some(label.clone());
            }
        }
        Some(label)
    }

    /// Set (or clear) a person's birthdate. Returns the owner id.
    pub fn set_person_birthdate(&mut self, person_id: &str, dob: Option<String>) -> Option<String> {
        let p = self.people.get_mut(person_id)?;
        p.birthdate = dob.filter(|s| !s.trim().is_empty());
        Some(p.owner_id.clone())
    }

    /// Pin a specific face as the person's cover (locks it against auto-refresh).
    /// The face must belong to the person. Returns the owner id.
    pub fn set_person_cover(&mut self, person_id: &str, face_id: &str) -> Option<String> {
        let belongs = self.people.get(person_id)?.face_ids.iter().any(|f| f == face_id);
        if !belongs {
            return None;
        }
        let (photo_id, bbox) = self.faces.get(face_id).map(|f| (f.photo_id.clone(), f.bbox))?;
        let p = self.people.get_mut(person_id)?;
        p.cover_photo_id = Some(photo_id);
        p.cover_bbox = Some(bbox);
        p.cover_locked = true;
        Some(p.owner_id.clone())
    }

    /// APPROVE low-confidence faces: a human confirmed they belong to this person.
    /// They stop being flagged for review (`confirmed`) and are pinned to the
    /// person's identity label, so the confirmation survives a future re-cluster.
    /// Returns the owner id.
    pub fn approve_faces(&mut self, person_id: &str, face_ids: &[String]) -> Option<String> {
        let owner = self.people.get(person_id)?.owner_id.clone();
        let label = self.ensure_label(person_id)?;
        let belong: std::collections::HashSet<String> =
            self.people.get(person_id)?.face_ids.iter().cloned().collect();
        for fid in face_ids {
            if belong.contains(fid) {
                if let Some(f) = self.faces.get_mut(fid) {
                    f.confirmed = true;
                    f.assigned_label = Some(label.clone());
                }
            }
        }
        Some(owner)
    }

    /// Mark faces as intruders / non-faces: excluded from this (and any) person.
    /// Returns the owner id. The person is pruned if it becomes empty.
    pub fn ignore_faces(&mut self, person_id: &str, face_ids: &[String]) -> Option<String> {
        let owner = self.people.get(person_id)?.owner_id.clone();
        let set: std::collections::HashSet<&String> = face_ids.iter().collect();
        for fid in face_ids {
            if let Some(f) = self.faces.get_mut(fid) {
                if f.owner_id == owner {
                    f.ignored = true;
                    f.person_id = None;
                    f.assigned_label = None;
                }
            }
        }
        if let Some(p) = self.people.get_mut(person_id) {
            p.face_ids.retain(|f| !set.contains(f));
        }
        if !self.prune_if_empty(person_id) {
            self.refresh_cover(person_id);
        }
        self.refresh_ai_people(&owner);
        Some(owner)
    }

    /// Move faces from one person to another (same owner). The moved faces are
    /// pinned to the destination's identity label so the move survives re-cluster.
    /// Returns the owner id. The source person is pruned if it empties.
    pub fn move_faces(&mut self, from_id: &str, face_ids: &[String], to_id: &str) -> Option<String> {
        if from_id == to_id {
            return None;
        }
        let owner = self.people.get(from_id)?.owner_id.clone();
        if self.people.get(to_id)?.owner_id != owner {
            return None;
        }
        let label = self.ensure_label(to_id)?;
        let set: std::collections::HashSet<&String> = face_ids.iter().collect();
        // Only move faces that actually belong to the source.
        let moving: Vec<String> = self
            .people
            .get(from_id)?
            .face_ids
            .iter()
            .filter(|f| set.contains(*f))
            .cloned()
            .collect();
        if moving.is_empty() {
            return Some(owner);
        }
        for fid in &moving {
            if let Some(f) = self.faces.get_mut(fid) {
                f.assigned_label = Some(label.clone());
                f.person_id = Some(to_id.to_string());
                f.ignored = false;
            }
        }
        if let Some(from) = self.people.get_mut(from_id) {
            from.face_ids.retain(|f| !set.contains(f));
        }
        if let Some(to) = self.people.get_mut(to_id) {
            for fid in &moving {
                if !to.face_ids.contains(fid) {
                    to.face_ids.push(fid.clone());
                }
            }
        }
        if !self.prune_if_empty(from_id) {
            self.refresh_cover(from_id);
        }
        self.refresh_cover(to_id);
        self.refresh_ai_people(&owner);
        Some(owner)
    }

    /// Merge `src` into `dst` (same owner): all of src's faces are pinned to dst's
    /// identity and folded in, src's kinship edges are absorbed, references to src
    /// across other people are remapped to dst, and src is removed. dst keeps its
    /// name/birthdate/cover. Returns the owner id.
    pub fn merge_people(&mut self, src_id: &str, dst_id: &str) -> Option<String> {
        if src_id == dst_id {
            return None;
        }
        let owner = self.people.get(src_id)?.owner_id.clone();
        if self.people.get(dst_id)?.owner_id != owner {
            return None;
        }
        let label = self.ensure_label(dst_id)?;
        let src = self.people.get(src_id)?.clone();
        for fid in &src.face_ids {
            if let Some(f) = self.faces.get_mut(fid) {
                f.assigned_label = Some(label.clone());
                f.person_id = Some(dst_id.to_string());
                f.ignored = false;
            }
        }
        if let Some(dst) = self.people.get_mut(dst_id) {
            for fid in &src.face_ids {
                if !dst.face_ids.contains(fid) {
                    dst.face_ids.push(fid.clone());
                }
            }
            // Absorb src's kinship edges (skip self + dups).
            for r in &src.relationships {
                if r.person_id != dst_id && !dst.relationships.iter().any(|x| x.person_id == r.person_id) {
                    dst.relationships.push(r.clone());
                }
            }
            if dst.birthdate.is_none() {
                dst.birthdate = src.birthdate.clone();
            }
        }
        self.people.remove(src_id);
        // Remap any edges that pointed at src → dst (drop resulting self-loops/dups).
        for p in self.people.values_mut() {
            for r in p.relationships.iter_mut() {
                if r.person_id == src_id {
                    r.person_id = dst_id.to_string();
                }
            }
            p.relationships.retain(|r| r.person_id != p.id);
            p.relationships.dedup_by(|a, b| a.person_id == b.person_id);
        }
        self.refresh_cover(dst_id);
        self.refresh_ai_people(&owner);
        Some(owner)
    }

    /// Hide a person from the People surface (kept in storage; carried as hidden
    /// across re-cluster). Returns the owner id.
    pub fn hide_person(&mut self, person_id: &str) -> Option<String> {
        // Pin the cluster's identity so the hidden flag stays with these faces if
        // a future re-cluster reshuffles things.
        let _ = self.ensure_label(person_id);
        let p = self.people.get_mut(person_id)?;
        p.hidden = true;
        let owner = p.owner_id.clone();
        self.refresh_ai_people(&owner);
        Some(owner)
    }

    /// Faces of a person for the studio grid: id, photo, bbox + source dims, score.
    /// Owner-scoped by the caller. NEVER includes embeddings.
    pub fn person_faces(&self, person_id: &str) -> Option<crate::models::PersonFacesResponse> {
        let p = self.people.get(person_id)?;
        let faces = p
            .face_ids
            .iter()
            .filter_map(|fid| self.faces.get(fid))
            .filter(|f| !f.ignored)
            .map(|f| {
                let (sw, sh) = self
                    .photos
                    .get(&f.photo_id)
                    .map(|cp| (cp.exif.width, cp.exif.height))
                    .unwrap_or((0, 0));
                crate::models::StudioFace {
                    id: f.id.clone(),
                    photo_id: f.photo_id.clone(),
                    bbox: f.bbox,
                    score: f.score,
                    source_width: sw,
                    source_height: sh,
                    confirmed: f.confirmed,
                }
            })
            .collect();
        Some(crate::models::PersonFacesResponse { person_id: person_id.to_string(), faces })
    }

    /// Resolved People (face-cluster) summaries for `owner`, newest/largest first.
    /// NEVER includes embeddings — only counts, the cover crop, and sample photo
    /// ids. Clusters are sorted by descending face count (ties by person id).
    pub fn people_views(&self, owner: &str) -> Vec<crate::models::PersonView> {
        let mut out: Vec<crate::models::PersonView> = self
            .people
            .values()
            .filter(|p| p.owner_id == owner && !p.hidden)
            .map(|p| {
                // Distinct sample photo ids (up to 9) from this person's faces.
                let mut sample: Vec<String> = Vec::new();
                for fid in &p.face_ids {
                    if let Some(f) = self.faces.get(fid) {
                        if !sample.contains(&f.photo_id) {
                            sample.push(f.photo_id.clone());
                            if sample.len() >= 9 {
                                break;
                            }
                        }
                    }
                }
                let cover = match (&p.cover_photo_id, &p.cover_bbox) {
                    (Some(pid), Some(bbox)) => {
                        let (sw, sh) = self
                            .photos
                            .get(pid)
                            .map(|cp| (cp.exif.width, cp.exif.height))
                            .unwrap_or((0, 0));
                        Some(crate::models::PersonCover {
                            photo_id: pid.clone(),
                            bbox: *bbox,
                            source_width: sw,
                            source_height: sh,
                        })
                    }
                    _ => None,
                };
                // Resolve kinship edges with the other person's current name.
                let relationships = p
                    .relationships
                    .iter()
                    .filter_map(|r| {
                        let other = self.people.get(&r.person_id)?;
                        Some(crate::models::RelationView {
                            person_id: r.person_id.clone(),
                            name: other.name.clone(),
                            relation: r.relation.clone(),
                        })
                    })
                    .collect();
                crate::models::PersonView {
                    person_id: p.id.clone(),
                    name: p.name.clone(),
                    face_count: p.face_ids.len(),
                    cover,
                    sample_photo_ids: sample,
                    relationships,
                    birthdate: p.birthdate.clone(),
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.face_count
                .cmp(&a.face_count)
                .then_with(|| a.person_id.cmp(&b.person_id))
        });
        out
    }

    /// The [`PhotoView`]s of the photos a Person appears in, scoped to `owner`
    /// (the requester must equal the person's owner). LIVE photos only
    /// (trash/archive/vault excluded), deduped, newest-first. Returns `None` when
    /// the person is unknown or not owned by `owner`.
    pub fn person_photos(&self, owner: &str, person_id: &str) -> Option<Vec<PhotoView>> {
        let person = self.people.get(person_id)?;
        if person.owner_id != owner {
            return None;
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut photos: Vec<&Photo> = Vec::new();
        for fid in &person.face_ids {
            if let Some(face) = self.faces.get(fid) {
                if !seen.insert(face.photo_id.clone()) {
                    continue;
                }
                if let Some(p) = self.photos.get(&face.photo_id) {
                    if p.deleted_at.is_none() && !p.archived && !self.is_in_any_vault(&p.id) {
                        photos.push(p);
                    }
                }
            }
        }
        photos.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
        Some(photos.into_iter().map(|p| p.effective()).collect())
    }

    /// The original upload bytes still pending for the import item that produced
    /// `photo_id` in `batch_id`, if any (drained by the thumbnail phase, so this
    /// is usually `None` by finalize — OCR then falls back to the thumbnail).
    fn import_pending_bytes_for_photo(&self, batch_id: &str, photo_id: &str) -> Option<Vec<u8>> {
        let file_id = self.imports.get(batch_id)?.items.iter().find_map(|i| {
            if i.photo_id.as_deref() == Some(photo_id) {
                Some(i.file_id.clone())
            } else {
                None
            }
        })?;
        self.pending_bytes
            .get(&(batch_id.to_string(), file_id))
            .map(|(_fname, _ext, bytes)| bytes.clone())
    }

    // ---- Async multi-stage import pipeline ----
    //
    // The HTTP POST creates an `ImportBatch` (one `ImportItem` per file, stage
    // Upload/Ok) and stashes the decoded bytes in `pending_bytes`, then a worker
    // drives each item Exif → Thumbnail → Analysis → Done. Each step locks state,
    // mutates the batch item, and unlocks so polling sees progress between steps.

    /// Helper: mutate one `ImportItem` (by file_id) inside a batch, if present.
    fn with_import_item<F: FnOnce(&mut crate::models::ImportItem)>(
        &mut self,
        batch_id: &str,
        file_id: &str,
        f: F,
    ) {
        if let Some(batch) = self.imports.get_mut(batch_id) {
            if let Some(item) = batch.items.iter_mut().find(|i| i.file_id == file_id) {
                f(item);
            }
        }
    }

    /// IMPORT PHASE 1 — EXIF. For every item still pending, mark Exif/Processing,
    /// extract EXIF from its bytes, then either Reject it (non-image/undecodable)
    /// or mark Exif/Ok. Bytes stay in `pending_bytes` for the create phase.
    pub fn import_phase_exif(&mut self, batch_id: &str) {
        let file_ids: Vec<(String, String)> = match self.imports.get(batch_id) {
            Some(b) => b
                .items
                .iter()
                .map(|i| (i.file_id.clone(), i.filename.clone()))
                .collect(),
            None => return,
        };
        for (file_id, filename) in &file_ids {
            self.with_import_item(batch_id, file_id, |it| {
                it.stage = crate::models::ImportStage::Exif;
                it.status = crate::models::ImportStatus::Processing;
            });
            let (ext, bytes) = match self.pending_bytes.get(&(batch_id.to_string(), file_id.clone())) {
                Some((_fname, ext, bytes)) => (ext.clone(), bytes.clone()),
                None => {
                    self.with_import_item(batch_id, file_id, |it| {
                        it.stage = crate::models::ImportStage::Exif;
                        it.status = crate::models::ImportStatus::Error;
                        it.error = Some("missing upload bytes".to_string());
                    });
                    continue;
                }
            };
            let exif = ExifExtractor.extract(&bytes, filename);
            // A non-image / undecodable file has zero dimensions and is not a
            // RAW/video kind we accept — reject it at the EXIF stage.
            let kind = classify(&ext);
            let undecodable = exif.width == 0 && exif.height == 0 && kind == "other";
            if undecodable {
                self.pending_bytes.remove(&(batch_id.to_string(), file_id.clone()));
                self.with_import_item(batch_id, file_id, |it| {
                    it.status = crate::models::ImportStatus::Rejected;
                    it.error = Some("unsupported or undecodable file".to_string());
                });
                continue;
            }
            // EXIF stage succeeded; record the extracted dims so the create phase
            // need not re-extract (it re-reads bytes anyway, but we keep the item
            // at Exif/Ok). The bytes stay in `pending_bytes` for the next phase.
            let _ = (ext, exif);
            self.with_import_item(batch_id, file_id, |it| {
                it.status = crate::models::ImportStatus::Ok;
            });
        }
    }

    /// IMPORT PHASE 2 — CREATE (EXIF→Photo + companion grouping). Groups the
    /// batch's decodable files (those left at Exif/Ok with bytes still pending) by
    /// (capture date, base name); the display-priority primary of each group
    /// becomes a `Photo` (its item gets `photo_id`), and the rest collapse into
    /// that photo's `companions` — their items end `Done`/`Duplicate` referencing
    /// the primary's photo_id. Returns the created photo ids (in creation order).
    pub fn import_phase_create(&mut self, batch_id: &str) -> Vec<String> {
        // One decodable upload awaiting CREATE: its import file id, original
        // filename, lowercased extension, raw bytes, and extracted EXIF. (Named to
        // replace the `(String, String, String, Vec<u8>, Exif)` tuple soup.)
        struct PendingAsset {
            fid: String,
            filename: String,
            ext: String,
            bytes: Vec<u8>,
            exif: Exif,
        }

        let owner_id = match self.imports.get(batch_id) {
            Some(b) => b.owner_id.clone(),
            None => return Vec::new(),
        };
        // Collect decodable files in input order: items at Exif/Ok with no
        // photo_id yet and bytes still present.
        let decodable_ids: Vec<(String, String)> = match self.imports.get(batch_id) {
            Some(b) => b
                .items
                .iter()
                .filter(|i| {
                    i.stage == crate::models::ImportStage::Exif
                        && i.status == crate::models::ImportStatus::Ok
                        && i.photo_id.is_none()
                })
                .map(|i| (i.file_id.clone(), i.filename.clone()))
                .collect(),
            None => return Vec::new(),
        };
        let mut decodable: Vec<PendingAsset> = Vec::new();
        for (file_id, filename) in decodable_ids {
            if let Some((_fname, ext, bytes)) =
                self.pending_bytes.get(&(batch_id.to_string(), file_id.clone()))
            {
                let ext = ext.clone();
                let bytes = bytes.clone();
                let exif = ExifExtractor.extract(&bytes, &filename);
                decodable.push(PendingAsset { fid: file_id, filename, ext, bytes, exif });
            }
        }

        // ---- Companion grouping across the batch (date + base name). ----
        let mut order: Vec<(String, String)> = Vec::new();
        let mut groups: HashMap<(String, String), Vec<PendingAsset>> = HashMap::new();
        for entry in decodable {
            let date = entry.exif.taken_at.get(0..10).unwrap_or("").to_string();
            let key = (date, base_name(&entry.filename));
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(entry);
        }

        let mut created_ids: Vec<String> = Vec::new();
        for key in order {
            let mut group = groups.remove(&key).unwrap();
            // Pick primary: lowest display priority, ties broken by filename.
            group.sort_by(|a, b| {
                display_priority(&a.ext)
                    .cmp(&display_priority(&b.ext))
                    .then_with(|| a.filename.cmp(&b.filename))
            });
            let PendingAsset {
                fid: primary_fid,
                filename: primary_name,
                ext: primary_ext,
                bytes: primary_bytes,
                exif: primary_exif,
            } = group.remove(0);

            let companions: Vec<Companion> = group
                .iter()
                .map(|a| Companion {
                    filename: a.filename.clone(),
                    ext: a.ext.clone(),
                    kind: classify(&a.ext).to_string(),
                    size_mb: a.bytes.len() as f64 / (1024.0 * 1024.0),
                    downloadable: true,
                })
                .collect();

            // Create the Photo (EXIF -> Photo step). taken_at falls back to now.
            let id = self.next_id("ph");
            let taken_at = if primary_exif.taken_at.is_empty() {
                now_rfc3339()
            } else {
                primary_exif.taken_at.clone()
            };
            let seed: u32 = {
                let s: u32 = primary_name.bytes().map(|b| b as u32).sum();
                100 + (s % 900)
            };
            let mut exif = primary_exif.clone();
            exif.taken_at = taken_at;
            let photo = Photo {
                id: id.clone(),
                owner_id: owner_id.clone(),
                filename: primary_name.clone(),
                seed,
                kind: photo_kind(&primary_ext).to_string(),
                exif,
                overrides: MetadataOverride::default(),
                companions,
                archived: false,
                deleted_at: None,
                backed_up: false,
                thumb_url: None,
                size_mb: primary_bytes.len() as f64 / (1024.0 * 1024.0),
                ocr_text: None,
                ai_tags: Vec::new(),
                ai_people: Vec::new(),
                analyzed: false,
                clip_embedding: None,
                full_url: None,
            };
            self.photos.insert(id.clone(), photo);
            // Keep the primary's UNMODIFIED original bytes (in-memory demo store)
            // so the lightbox can request the full / a screen-adapted render. The
            // bytes are also pushed to the StorageBackend in the finalize phase.
            self.store_original(&id, &primary_ext, &primary_bytes);
            created_ids.push(id.clone());

            // COMPANION DOWNLOAD: keep each companion's bytes so the UI can
            // download the RAW/.ARW sidecar. Keyed by (photo_id, ext); the
            // backend push happens best-effort in the finalize phase.
            for a in &group {
                let mime = MediaFormat::from_ext(&a.ext)
                    .map(|f| f.mime().to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                self.companion_bytes.insert(
                    (id.clone(), a.ext.to_lowercase()),
                    (a.bytes.clone(), a.filename.clone(), mime),
                );
            }

            // Primary item: now has a photo_id; stays at Exif/Ok and advances
            // through the Thumbnail + Analysis phases. The primary's bytes are
            // intentionally LEFT in `pending_bytes` for the thumbnail phase.
            self.with_import_item(batch_id, &primary_fid, |it| {
                it.photo_id = Some(id.clone());
                it.status = crate::models::ImportStatus::Ok;
            });

            // Companion items collapse: Done/merge referencing the primary photo.
            for a in &group {
                let cfid = &a.fid;
                let primary = primary_name.clone();
                let pid = id.clone();
                self.with_import_item(batch_id, cfid, |it| {
                    it.stage = crate::models::ImportStage::Done;
                    it.status = crate::models::ImportStatus::Duplicate;
                    it.photo_id = Some(pid);
                    it.error = Some(format!("merged into {primary} as a companion"));
                });
                // Drain its bytes (already merged as a companion).
                self.pending_bytes
                    .remove(&(batch_id.to_string(), cfid.clone()));
            }
        }
        created_ids
    }

    /// The `(file_id, photo_id)` of every primary item that finished STAGE 3
    /// (Thumbnail) and is ready for STAGE 4 (Analysis). Lets the enrichment task
    /// drive analysis one photo at a time (persisting between) for live progress.
    pub fn import_analysis_primaries(&self, batch_id: &str) -> Vec<(String, String)> {
        self.import_primary_items(batch_id, |i| {
            i.stage == crate::models::ImportStage::Thumbnail && i.photo_id.is_some()
        })
    }

    /// Set one import item's stage + status (by file_id). Used by the enrichment
    /// task to advance an item through a stage and persist between steps.
    pub fn import_set_item_stage(
        &mut self,
        batch_id: &str,
        file_id: &str,
        stage: crate::models::ImportStage,
        status: crate::models::ImportStatus,
    ) {
        self.with_import_item(batch_id, file_id, |it| {
            it.stage = stage;
            it.status = status;
        });
    }

    /// IMPORT PHASE 5 — FINALIZE (async). Embed thumbnails (CLIP; no-op offline),
    /// push thumbnails to the storage backend, add created photos to the target
    /// album, and write-through-persist. Run once after the per-photo phases.
    pub async fn import_phase_finalize(&mut self, batch_id: &str, created_ids: &[String]) {
        let album_id = self.imports.get(batch_id).and_then(|b| b.album_id.clone());
        // ML enrichment is gated by the admin feature flags (Settings → Machine
        // learning). Disabling a flag actually SKIPS that step of the pipeline.
        let feat = self.storage.features.clone();
        // CONTEXT RECOGNITION (CLIP): embed the freshly-created photos' thumbs.
        if feat.clip {
            self.embed_photos(created_ids).await;
        }
        // OCR: extract text from the photos (original bytes if still pending,
        // else thumbnail) and fill `ocr_text`. No-op offline.
        if feat.ocr {
            self.ocr_photos(Some(batch_id), created_ids).await;
        }
        // FACE RECOGNITION: detect faces (original bytes if still pending, else
        // thumbnail) and incrementally cluster the owner's faces into People.
        // No-op offline. The full re-cluster also runs in the daily job.
        if feat.faces {
            self.detect_faces(Some(batch_id), created_ids).await;
            if self.ml.is_some() {
                let owner = self.imports.get(batch_id).map(|b| b.owner_id.clone());
                if let Some(owner) = owner {
                    self.cluster_faces(&owner);
                    self.persist_faces(&owner).await;
                }
            }
        }
        // Best-effort: push thumbnails to the configured storage backend.
        self.store_thumbnails(created_ids).await;
        // Best-effort: push ORIGINALS to the configured storage backend (the real
        // blob store; the in-memory `originals` map is only a demo convenience).
        self.store_originals(created_ids).await;
        // Best-effort: push companion files (RAW/.ARW sidecars) to the backend
        // under companions/{photo_id}.{ext} (the in-memory map is demo-only).
        self.store_companions(created_ids).await;
        // Add the created photos to the target album, if any — but only when the
        // batch owner may contribute to it (defense-in-depth; the request handler
        // already enforced this, but the attach must never run unauthorized).
        let owner = self.imports.get(batch_id).map(|b| b.owner_id.clone());
        if let Some(alb) = &album_id {
            let allowed = owner.as_deref().map(|o| self.can_contribute(o, alb)).unwrap_or(false);
            if allowed {
                if let Some(album) = self.albums.get_mut(alb) {
                    for pid in created_ids {
                        if !album.photo_ids.contains(pid) {
                            album.photo_ids.push(pid.clone());
                        }
                    }
                }
            }
        }
        // Write-through persistence (no-op in in-memory mode).
        for pid in created_ids {
            self.persist_photo(pid).await;
        }
        if let Some(alb) = &album_id {
            self.persist_album(alb).await;
        }
    }

    /// Collect `(file_id, photo_id)` for items matching `pred` that carry a
    /// photo_id. Helper for the per-photo phases.
    fn import_primary_items<F: Fn(&crate::models::ImportItem) -> bool>(
        &self,
        batch_id: &str,
        pred: F,
    ) -> Vec<(String, String)> {
        match self.imports.get(batch_id) {
            Some(b) => b
                .items
                .iter()
                .filter(|i| pred(i))
                .filter_map(|i| i.photo_id.clone().map(|pid| (i.file_id.clone(), pid)))
                .collect(),
            None => Vec::new(),
        }
    }

    /// CONTEXT RECOGNITION (CLIP): semantic ranking of `candidates` (already
    /// access-filtered [`PhotoView`]s, newest-first) against a free-text query.
    ///
    /// When ML is available, embeds `query` once, scores each candidate that has
    /// a `clip_embedding` by cosine similarity, drops those below `min_score`,
    /// and returns them sorted by descending similarity (best matches first).
    /// Candidates without an embedding are excluded from the semantic result.
    /// Returns `None` when ML is disabled or the query embedding fails, so the
    /// caller can fall back to the existing keyword/facet results unchanged.
    pub async fn semantic_rank(
        &self,
        candidates: Vec<PhotoView>,
        query: &str,
        min_score: f32,
    ) -> Option<Vec<PhotoView>> {
        let client = self.ml.as_ref()?;
        let qvec = client.embed_text(query).await?;
        let mut scored: Vec<(f32, PhotoView)> = candidates
            .into_iter()
            .filter_map(|view| {
                let p = self.photos.get(&view.id)?;
                let emb = p.clip_embedding.as_ref()?;
                let score = crate::ml::cosine_similarity(&qvec, emb)?;
                if score >= min_score { Some((score, view)) } else { None }
            })
            .collect();
        // Highest similarity first; ties keep the input (newest-first) order via a
        // stable sort.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Some(scored.into_iter().map(|(_, v)| v).collect())
    }

    // ---- AI analysis (import stage 4) ----

    /// Run the AI-analysis stage over a single photo with the default
    /// [`HeuristicAnalyzer`], using the stored thumbnail bytes (from
    /// [`Self::thumbs`]) when present, else `None`. Writes `ocr_text`,
    /// `ai_tags`, `ai_people`, sets `analyzed = true`, and persists (write-through
    /// is a no-op in in-memory mode). Returns `false` if the photo is unknown.
    ///
    /// To swap in a real OCR/face backend, construct a different [`Analyzer`]
    /// here (everything downstream is backend-agnostic).
    pub fn analyze_photo(&mut self, id: &str) -> bool {
        self.analyze_photo_with(id, &HeuristicAnalyzer)
    }

    /// As [`Self::analyze_photo`] but with an explicit analyzer (used by tests
    /// and to let a future ML backend be injected).
    pub fn analyze_photo_with<A: Analyzer>(&mut self, id: &str, analyzer: &A) -> bool {
        // Clone the photo + thumbnail bytes to avoid holding a borrow across the
        // analyzer call and the subsequent mutable write.
        let photo = match self.photos.get(id) {
            Some(p) => p.clone(),
            None => return false,
        };
        let bytes = self.thumbs.get(id).map(|(b, _ct)| b.clone());
        let result = analyzer.analyze(bytes.as_deref(), &photo);
        if let Some(p) = self.photos.get_mut(id) {
            p.ocr_text = result.ocr_text;
            p.ai_tags = result.tags;
            p.ai_people = result.people;
            p.analyzed = true;
        }
        true
    }

    /// Analyze every not-yet-`analyzed` photo in one pass. Returns the ids
    /// analyzed. NOTE: this holds `&mut self` for the whole walk, so the
    /// `ai_analysis` job loop in main.rs does NOT use it (it snapshots ids then
    /// analyzes one photo per lock acquisition — see LOCK HARDENING there). Kept
    /// as the single-shot helper used by tests and any synchronous caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn analyze_unanalyzed(&mut self) -> Vec<String> {
        let pending: Vec<String> = self
            .photos
            .values()
            .filter(|p| !p.analyzed)
            .map(|p| p.id.clone())
            .collect();
        for id in &pending {
            self.analyze_photo(id);
        }
        pending
    }

    // ---- Duplicate detection (perceptual hashing) ----

    /// Compute a 64-bit perceptual (gradient/dHash) hash for one photo from its
    /// THUMBNAIL bytes in [`Self::thumbs`], decoded via the `image` crate. Returns
    /// `None` when the photo has no thumbnail (e.g. the demo seed, which has no
    /// real bytes) or the thumbnail can't be decoded. Pure (no `&mut self`) so the
    /// job can call it per id while releasing the write lock between photos (see
    /// the `duplicates` job loop in main.rs).
    pub fn perceptual_hash(&self, photo_id: &str) -> Option<image_hasher::ImageHash> {
        let (bytes, _ct) = self.thumbs.get(photo_id)?;
        let img = image::load_from_memory(bytes).ok()?;
        // Gradient (dHash) — 8x8 => 64-bit, cheap and robust to scaling/recompress.
        let hasher = image_hasher::HasherConfig::new()
            .hash_alg(image_hasher::HashAlg::Gradient)
            .to_hasher();
        Some(hasher.hash_image(&img))
    }

    /// DUPLICATE DETECTION: for each owner, group their LIVE photos whose
    /// perceptual-hash Hamming distance is within the threshold (groups of >= 2),
    /// storing the result in [`Self::duplicate_groups`]. Photos without thumbnail
    /// bytes (e.g. the seed) have no hash and are skipped. Returns the total count
    /// of photos that landed in some duplicate group.
    ///
    /// Greedy single-link grouping: for each owner's hashable photos in id order,
    /// a photo joins the first existing cluster whose representative is within the
    /// threshold, else it starts a new cluster.
    pub fn detect_duplicates(&mut self) -> usize {
        /// Max Hamming distance (out of 64 bits) for two photos to be near-dupes.
        const HAMMING_THRESHOLD: u32 = 10;

        // Collect (owner, id, hash) for every live, hashable photo.
        let mut hashed: HashMap<String, Vec<(String, image_hasher::ImageHash)>> = HashMap::new();
        let live_ids: Vec<(String, String)> = self
            .photos
            .values()
            .filter(|p| p.deleted_at.is_none() && !p.archived && !self.is_in_any_vault(&p.id))
            .map(|p| (p.owner_id.clone(), p.id.clone()))
            .collect();
        for (owner, id) in live_ids {
            if let Some(h) = self.perceptual_hash(&id) {
                hashed.entry(owner).or_default().push((id, h));
            }
        }

        let mut groups: HashMap<String, Vec<Vec<String>>> = HashMap::new();
        let mut total = 0usize;
        for (owner, mut items) in hashed {
            // Deterministic order so grouping is stable across runs.
            items.sort_by(|a, b| a.0.cmp(&b.0));
            // Each cluster keeps a representative hash (its first member).
            let mut clusters: Vec<(image_hasher::ImageHash, Vec<String>)> = Vec::new();
            for (id, hash) in items {
                let mut placed = false;
                for (rep, ids) in clusters.iter_mut() {
                    if rep.dist(&hash) <= HAMMING_THRESHOLD {
                        ids.push(id.clone());
                        placed = true;
                        break;
                    }
                }
                if !placed {
                    clusters.push((hash, vec![id]));
                }
            }
            let dup_groups: Vec<Vec<String>> =
                clusters.into_iter().map(|(_, ids)| ids).filter(|g| g.len() >= 2).collect();
            for g in &dup_groups {
                total += g.len();
            }
            if !dup_groups.is_empty() {
                groups.insert(owner, dup_groups);
            }
        }
        self.duplicate_groups = groups;
        total
    }

    /// Resolved duplicate groups for `owner`: each inner Vec is a set of >= 2
    /// near-duplicate [`PhotoView`]s. Only LIVE photos are returned (a group
    /// member since trashed/archived/vaulted is dropped, and groups that fall
    /// below 2 members after filtering are omitted).
    pub fn duplicate_views(&self, owner: &str) -> Vec<Vec<PhotoView>> {
        let groups = match self.duplicate_groups.get(owner) {
            Some(g) => g,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for group in groups {
            let views: Vec<PhotoView> = group
                .iter()
                .filter_map(|pid| self.photos.get(pid))
                .filter(|p| p.deleted_at.is_none() && !p.archived && !self.is_in_any_vault(&p.id))
                .map(|p| p.effective())
                .collect();
            if views.len() >= 2 {
                out.push(views);
            }
        }
        out
    }

    /// Permanently remove photos whose `deleted_at` is older than
    /// `trash_retention_days`. Also drops them from any album. Returns the
    /// purged ids. Cheap: a single scan plus removals.
    pub fn purge_expired_trash(&mut self) -> Vec<String> {
        let retention_secs = self.storage.trash_retention_days as i64 * 86_400;
        let now = OffsetDateTime::now_utc().unix_timestamp();

        let expired: Vec<String> = self
            .photos
            .values()
            .filter_map(|p| {
                let deleted = p.deleted_at.as_deref()?;
                let ts = rfc3339_to_unix(deleted)?;
                if now - ts >= retention_secs {
                    Some(p.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &expired {
            self.photos.remove(id);
            for album in self.albums.values_mut() {
                album.photo_ids.retain(|pid| pid != id);
            }
        }
        expired
    }

    /// Run one backup pass: when backup is enabled and an S3 config is present,
    /// push every not-yet-backed-up, live photo to S3 and mark it backed up.
    /// Updates `last_backup_at` / `last_backup_count`. Returns the count pushed.
    ///
    /// The demo has no real file bytes, so a small synthetic payload (the
    /// filename bytes) is uploaded per photo — the point is the wiring + config.
    pub async fn run_backup(&mut self) -> Result<u64, String> {
        if !self.storage.backup.enabled {
            return Ok(0);
        }
        let cfg = match &self.storage.backup.s3 {
            Some(c) => c.clone(),
            None => return Ok(0),
        };
        let backend = S3Backend::from_config(&cfg).map_err(|e| e.to_string())?;
        // In BACKUP mode the filesystem stays the source of truth; we keep a
        // local copy under the data dir and push to S3. (Local root is best-effort.)
        let local = LocalFsBackend::new(self.data_dir.clone());

        // Collect targets first to avoid holding a borrow across await.
        let targets: Vec<(String, String)> = self
            .photos
            .values()
            .filter(|p| !p.backed_up && p.deleted_at.is_none())
            .map(|p| (p.id.clone(), p.filename.clone()))
            .collect();

        let mut count: u64 = 0;
        for (id, filename) in targets {
            // Synthetic payload: the filename bytes (no real file store here).
            let payload = filename.clone().into_bytes();
            let key = format!("photos/{id}/{filename}");

            // Keep the local source-of-truth copy.
            let _ = local.put_object(&key, &payload).await;

            // Skip if S3 already has this object (idempotent backup).
            if !backend.exists(&key).await.unwrap_or(false) {
                backend
                    .put_object(&key, &payload)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            if let Some(p) = self.photos.get_mut(&id) {
                p.backed_up = true;
            }
            // Persist the flipped `backed_up` flag (Postgres-first): the job runs
            // on a fresh DB snapshot, so the change must be written back per photo.
            self.persist_photo(&id).await;
            count += 1;
        }

        self.storage.backup.last_backup_at = Some(now_rfc3339());
        self.storage.backup.last_backup_count = count;
        self.persist_storage().await;
        Ok(count)
    }

    /// Compute the set of photos visible in `user_id`'s timeline, sorted by
    /// effective `taken_at` descending, deduplicated by photo id.
    pub fn timeline_photos(&self, user_id: &str) -> Vec<Photo> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<Photo> = Vec::new();
        let prefs = self.prefs.get(user_id).cloned().unwrap_or_default();

        // 1. All photos owned by the user. Trashed (soft-deleted) and archived
        //    photos are excluded from the timeline.
        for p in self.photos.values() {
            if p.deleted_at.is_some() || p.archived || self.is_in_any_vault(&p.id) {
                continue;
            }
            if p.owner_id == user_id && seen.insert(p.id.clone()) {
                out.push(p.clone());
            }
        }

        // 2. Photos from albums shared to the user, respecting prefs.
        for album in self.albums.values() {
            if album.owner_id == user_id {
                // Own album: photos already counted in step 1; skip.
                continue;
            }
            if !self.album_shared_to(album, user_id) {
                continue;
            }
            if !prefs.effective_visible(&album.id) {
                continue;
            }
            for pid in &album.photo_ids {
                if let Some(p) = self.photos.get(pid) {
                    if p.deleted_at.is_some() || p.archived || self.is_in_any_vault(&p.id) {
                        continue;
                    }
                    if seen.insert(p.id.clone()) {
                        out.push(p.clone());
                    }
                }
            }
        }

        // 3. PARTNER grants: photos owned by any user A who declared `user_id` as
        //    a partner. Same live-only exclusions (trash/archive/vault) as own
        //    photos; deduped by id so an album-shared photo isn't double-counted.
        //    These appear as "shared" in the UI (owner_id != viewer).
        let grantors = self.partner_grantors(user_id);
        if !grantors.is_empty() {
            for p in self.photos.values() {
                if p.deleted_at.is_some() || p.archived || self.is_in_any_vault(&p.id) {
                    continue;
                }
                if grantors.contains(&p.owner_id.as_str()) && seen.insert(p.id.clone()) {
                    out.push(p.clone());
                }
            }
        }

        // Sort by effective taken_at descending (RFC3339 strings sort
        // lexicographically); override-if-some-else-exif.
        out.sort_by(|a, b| b.effective_taken_at().cmp(a.effective_taken_at()));
        out
    }
}

/// Build the seeded demo state.
pub fn seed() -> AppState {
    let mut users = HashMap::new();
    let mut groups = HashMap::new();
    let mut photos = HashMap::new();
    let mut albums = HashMap::new();
    let mut prefs = HashMap::new();

    // ---- Users ----
    let user_data = [
        ("usr_alice", "Alice", "alice@photon.app", 12),
        ("usr_bob", "Bob", "bob@photon.app", 33),
        ("usr_carol", "Carol", "carol@photon.app", 45),
        ("usr_dave", "Dave", "dave@photon.app", 8),
    ];
    for (id, name, email, img) in user_data {
        // DEMO LOGINS for testing: each seed user's password is their first name
        // lowercased — alice/"alice", bob/"bob", carol/"carol", dave/"dave".
        // Hashed with argon2id (dev-default server secret + a per-user fixed
        // pepper so the seeded hash is reproducible); the plaintext is never
        // stored. Alice is the admin.
        let pepper = format!("photon-demo-pepper-{id}");
        let password = name.to_lowercase();
        let mut user = User {
            id: id.to_string(),
            name: name.to_string(),
            email: email.to_string(),
            avatar_url: format!("https://i.pravatar.cc/64?img={img}"),
            password_hash: None,
            salt: String::new(),
            pepper: String::new(),
            is_admin: id == "usr_alice",
            disabled: false,
            // Demo: give Bob an explicit quota to show a fixed total; others
            // derive their total from the backend (filesystem / S3 default).
            quota_mb: if id == "usr_bob" { Some(50_000) } else { None },
            partners: Vec::new(),
            totp_secret: None,
        };
        user.set_password(DEV_PASSWORD_SECRET, pepper, &password);
        users.insert(id.to_string(), user);
    }

    // PARTNER demo (directed grant): Bob declares Alice as a partner, so Alice
    // gains read access to all of Bob's LIVE photos in her timeline and search.
    if let Some(bob) = users.get_mut("usr_bob") {
        bob.partners.push("usr_alice".to_string());
    }

    // ---- Groups ----
    groups.insert(
        "grp_family".to_string(),
        Group {
            id: "grp_family".to_string(),
            name: "Family".to_string(),
            owner_id: "usr_alice".to_string(),
            member_ids: vec![
                "usr_alice".to_string(),
                "usr_bob".to_string(),
                "usr_carol".to_string(),
            ],
        },
    );
    groups.insert(
        "grp_work".to_string(),
        Group {
            id: "grp_work".to_string(),
            name: "Work".to_string(),
            owner_id: "usr_dave".to_string(),
            member_ids: vec!["usr_dave".to_string(), "usr_alice".to_string()],
        },
    );

    // ---- Photos (deterministic generator) ----
    // (city, country)
    let places = [
        ("Lyon", "FR"),
        ("Annecy", "FR"),
        ("Chamonix", "FR"),
        ("Lisboa", "PT"),
        ("Kyoto", "JP"),
    ];
    let kinds = ["photo", "photo", "photo", "raw", "video"];
    let seeds = [
        101, 202, 303, 404, 505, 606, 707, 808, 909, 110, 211, 312, 413, 514, 615, 716, 817, 918,
        119, 220, 321, 422, 523, 624, 725, 826, 927, 128, 229, 330, 431, 532, 633, 734, 835, 936,
    ];
    // owner rotation: mostly alice, some others.
    let owners = [
        "usr_alice",
        "usr_alice",
        "usr_alice",
        "usr_bob",
        "usr_alice",
        "usr_carol",
        "usr_alice",
        "usr_dave",
    ];

    // Build ~36 photos spread over the last ~45 days.
    let base_year = 2026;
    let base_month = 6; // June 2026 (relative to seed date 2026-06-23)
    for (i, &sd) in seeds.iter().enumerate() {
        let (city, country) = places[i % places.len()];
        let kind = kinds[i % kinds.len()];
        let owner = owners[i % owners.len()];
        let landscape = i % 3 != 0;
        let (width, height) = if landscape {
            (6240u32, 4160u32)
        } else {
            (4000u32, 6000u32)
        };
        // Spread days backwards across ~45 days; clamp to valid day-of-month.
        let days_ago = (i as u32) + (i as u32 % 9); // 0..~44
        let (month, day) = day_back(base_year, base_month, 23, days_ago);
        let hour = 8 + (i as u32 % 12);
        let minute = (i as u32 * 7) % 60;
        let taken_at = format!(
            "{base_year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z"
        );
        let id = format!("ph_{:04}", i + 1);
        let exif = Exif {
            camera: Some("Sony A7 IV".to_string()),
            lens: Some("FE 24-70mm F2.8 GM".to_string()),
            iso: Some(100 + (i as u32 % 8) * 200),
            shutter: Some("1/250".to_string()),
            fnum: Some("f/4.0".to_string()),
            focal: Some(format!("{}mm", 24 + (i as u32 % 5) * 10)),
            taken_at: taken_at.clone(),
            width,
            height,
            city: Some(city.to_string()),
            country: Some(country.to_string()),
            lat: None,
            lng: None,
        };

        // A few seed photos demonstrate override behavior.
        let mut overrides = MetadataOverride::default();
        if i % 5 == 0 {
            overrides.favorite = Some(true);
        }
        if i == 0 {
            overrides.title = Some("Golden hour over Lyon".to_string());
            overrides.caption = Some("Shot from the Fourvière hill.".to_string());
            overrides.rating = Some(5);
        }
        if i == 4 {
            // EXIF said Kyoto, librarian corrected the city.
            overrides.city = Some("Osaka".to_string());
            overrides.rating = Some(4);
        }
        if i == 7 {
            overrides.tags = Some(vec!["mountains".to_string(), "hiking".to_string()]);
            overrides.people = Some(vec!["Bob".to_string()]);
        }

        // AI-analysis (stage 4) demo data on a few seed photos so search over
        // AI-derived metadata is demonstrable. The background `ai_analysis` job
        // would also fill context tags for the rest from EXIF.
        let (ai_tags, ai_people, ocr_text, analyzed): (Vec<String>, Vec<String>, Option<String>, bool) =
            if i == 2 {
                (
                    vec!["night".to_string(), "mountains".to_string()],
                    Vec::new(),
                    None,
                    true,
                )
            } else if i == 4 {
                (
                    vec!["landscape".to_string(), "city".to_string()],
                    Vec::new(),
                    // A sign caption "read" from the photo (demo OCR result).
                    Some("Bienvenue à Lyon".to_string()),
                    true,
                )
            } else if i == 6 {
                (
                    vec!["portrait".to_string(), "beach".to_string()],
                    Vec::new(),
                    None,
                    true,
                )
            } else {
                (Vec::new(), Vec::new(), None, false)
            };

        // Demonstrate a JPG + RAW companion pair on one seed photo.
        let mut companions = Vec::new();
        let filename = if i == 1 {
            "IMG_4021.jpg".to_string()
        } else {
            format!("IMG_{:04}.jpg", sd)
        };
        if i == 1 {
            companions.push(Companion {
                filename: "IMG_4021.ARW".to_string(),
                ext: "ARW".to_string(),
                kind: "raw".to_string(),
                size_mb: 48.2,
                downloadable: true,
            });
        }

        // Seed photos have no real thumbnail bytes, so thumb_url stays None and
        // the UI falls back to its placeholder imagery. Real uploads get a
        // thumb_url once ingest_upload_bytes generates the thumbnail.
        let thumb_url = None;
        let size_mb = match kind {
            "raw" => 42.0 + (i % 12) as f64,
            "video" => 160.0 + (i % 80) as f64,
            _ => 7.0 + (i % 10) as f64,
        };
        photos.insert(
            id.clone(),
            Photo {
                id,
                owner_id: owner.to_string(),
                filename,
                seed: sd,
                kind: kind.to_string(),
                exif,
                overrides,
                companions,
                archived: false,
                deleted_at: None,
                backed_up: false,
                thumb_url,
                size_mb,
                ocr_text,
                ai_tags,
                ai_people,
                analyzed,
                clip_embedding: None,
                full_url: None,
            },
        );
    }

    // Helper to collect photo ids by owner.
    let ids_by_owner = |owner: &str, take: usize| -> Vec<String> {
        let mut v: Vec<String> = photos
            .values()
            .filter(|p| p.owner_id == owner)
            .map(|p| p.id.clone())
            .collect();
        v.sort();
        v.into_iter().take(take).collect()
    };

    // ---- Albums ----
    albums.insert(
        "alb_summer".to_string(),
        Album {
            id: "alb_summer".to_string(),
            name: "Summer 2026".to_string(),
            owner_id: "usr_alice".to_string(),
            cover_seed: 101,
            photo_ids: ids_by_owner("usr_alice", 8),
            // Family can view; Bob is a Family member and additionally gets a
            // direct Contributor share so he can add his own photos.
            shares: vec![
                Share {
                    target: ShareTarget::Group("grp_family".to_string()),
                    role: ShareRole::Viewer,
                },
                Share {
                    target: ShareTarget::User("usr_bob".to_string()),
                    role: ShareRole::Contributor,
                },
            ],
        },
    );
    albums.insert(
        "alb_chamonix".to_string(),
        Album {
            id: "alb_chamonix".to_string(),
            name: "Chamonix Trip".to_string(),
            owner_id: "usr_bob".to_string(),
            cover_seed: 404,
            photo_ids: ids_by_owner("usr_bob", 5),
            shares: vec![Share {
                target: ShareTarget::User("usr_alice".to_string()),
                role: ShareRole::Viewer,
            }],
        },
    );
    albums.insert(
        "alb_work".to_string(),
        Album {
            id: "alb_work".to_string(),
            name: "Work Offsite".to_string(),
            owner_id: "usr_dave".to_string(),
            cover_seed: 808,
            photo_ids: ids_by_owner("usr_dave", 5),
            shares: vec![Share {
                target: ShareTarget::Group("grp_work".to_string()),
                role: ShareRole::Viewer,
            }],
        },
    );

    // ---- TimelinePrefs ----
    let mut alice_prefs = TimelinePrefs::default();
    alice_prefs.show_shared = true;
    alice_prefs.per_album.insert("alb_work".to_string(), false); // hide work album
    prefs.insert("usr_alice".to_string(), alice_prefs);

    prefs.insert(
        "usr_bob".to_string(),
        TimelinePrefs {
            show_shared: false,
            per_album: HashMap::new(),
        },
    );
    // carol & dave: default (show_shared = true, no overrides)
    prefs.insert("usr_carol".to_string(), TimelinePrefs::default());
    prefs.insert("usr_dave".to_string(), TimelinePrefs::default());

    // ---- Vault (demo) ----
    // Give Alice a PIN-locked vault with 2 of her photos so the feature is
    // demonstrable. DEMO PIN: "1234". The plaintext is never stored — only the
    // argon2id PHC string (server secret + fixed demo salt) below. A fixed demo
    // salt is used so the seed is self-contained; runtime vaults use a CSPRNG salt
    // via `new_salt`.
    let mut vaults: HashMap<String, Vault> = HashMap::new();
    {
        let alice_photos = ids_by_owner("usr_alice", 2);
        let mut vault = Vault {
            pin_hash: None,
            salt: "photon-demo-salt".to_string(),
            photo_ids: alice_photos,
        };
        vault.set_pin(DEV_PASSWORD_SECRET, "1234");
        vaults.insert("usr_alice".to_string(), vault);
    }

    AppState {
        users,
        groups,
        photos,
        albums,
        prefs,
        vaults,
        storage: StorageSettings::default(),
        smtp: None,
        invites: HashMap::new(),
        reset_tokens: HashMap::new(),
        jobs: default_jobs(),
        thumbs: HashMap::new(),
        originals: HashMap::new(),
        password_secret: DEV_PASSWORD_SECRET.to_vec(),
        data_dir: DEFAULT_DATA_DIR.to_string(),
        persistence: None,
        sessions: HashMap::new(),
        worker_utils: None,
        lockouts: HashMap::new(),
        ml: None,
        plugins: None,
        webauthn: None,
        oidc_login: None,
        imports: HashMap::new(),
        pending_bytes: HashMap::new(),
        companion_bytes: HashMap::new(),
        duplicate_groups: HashMap::new(),
        dlna_devices: HashMap::new(),
        faces: HashMap::new(),
        people: HashMap::new(),
        counter: AtomicU64::new(1),
    }
}

/// Subtract `days_ago` from the given (year, month, day) and return (month, day),
/// assuming a window within the current and previous month only. Simplified
/// calendar math sufficient for seed data (no year boundary).
fn day_back(_year: u32, month: u32, day: u32, days_ago: u32) -> (u32, u32) {
    let d = day as i64 - days_ago as i64;
    if d >= 1 {
        (month, d as u32)
    } else {
        // roll into previous month
        let prev_month = if month == 1 { 12 } else { month - 1 };
        let prev_len = days_in_month(prev_month) as i64;
        let new_day = prev_len + d; // d is <= 0
        let new_day = new_day.clamp(1, prev_len) as u32;
        (prev_month, new_day)
    }
}

fn days_in_month(month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28,
        _ => 30,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Photo;

    fn photo(id: &str, owner: &str, taken_at: &str) -> Photo {
        Photo {
            id: id.to_string(),
            owner_id: owner.to_string(),
            filename: format!("{id}.jpg"),
            seed: 1,
            kind: "photo".to_string(),
            exif: Exif {
                taken_at: taken_at.to_string(),
                width: 6240,
                height: 4160,
                city: Some("Lyon".to_string()),
                country: Some("FR".to_string()),
                ..Default::default()
            },
            overrides: MetadataOverride::default(),
            companions: Vec::new(),
            archived: false,
            deleted_at: None,
            backed_up: false,
            thumb_url: None,
            size_mb: 10.0,
            ocr_text: None,
            ai_tags: Vec::new(),
            ai_people: Vec::new(),
            analyzed: false,
            clip_embedding: None,
            full_url: None,
        }
    }

    /// A plugin edit is kept alongside the untouched original as the reserved
    /// `edited` companion, preferred for display, and re-editing overwrites it.
    #[tokio::test]
    async fn edited_version_is_companion_and_preferred_for_display() {
        let dir = format!("{}/photon-edit-{}", std::env::temp_dir().display(), std::process::id());
        let mut st = AppState::default();
        st.data_dir = dir.clone();
        st.active_backend().put_object("originals/ph_x.jpg", b"ORIGINAL").await.unwrap();
        let p = photo("ph_x", "usr_alice", "2024:01:01 00:00:00");

        // A small valid PNG to stand in for the plugin's edited output.
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255])))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();

        let edited = st.storage_ctx().store_edited_version(p, &png).await.expect("store edited");
        assert!(AppState::has_edited_version(&edited));
        assert_eq!(edited.companions.iter().filter(|c| c.ext == EDITED_EXT).count(), 1);

        // Display prefers the edited bytes; the original is still served untouched.
        let (disp, ct) = st.storage_ctx().load_display_blob(&edited).await.unwrap();
        assert_eq!(disp, png);
        assert_eq!(ct, "image/png");
        let (orig, _) = st.storage_ctx().load_original_blob(&edited).await.unwrap();
        assert_eq!(orig, b"ORIGINAL");

        // Re-editing overwrites — still exactly one edited companion.
        let edited2 = st.storage_ctx().store_edited_version(edited, &png).await.unwrap();
        assert_eq!(edited2.companions.iter().filter(|c| c.ext == EDITED_EXT).count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn uf(filename: &str, ext: &str, taken_at: &str) -> UploadedFile {
        UploadedFile {
            filename: filename.to_string(),
            ext: ext.to_string(),
            size_mb: 10.0,
            taken_at: taken_at.to_string(),
            camera: None,
            lens: None,
            iso: None,
            shutter: None,
            fnum: None,
            focal: None,
            width: 6000,
            height: 4000,
            city: None,
            country: None,
            lat: None,
            lng: None,
            seed: None,
        }
    }

    /// Build a minimal hand-crafted state for visibility tests.
    fn test_state() -> AppState {
        let mut users = HashMap::new();
        for id in ["usr_alice", "usr_bob", "usr_carol", "usr_eve"] {
            users.insert(
                id.to_string(),
                User {
                    id: id.to_string(),
                    name: id.to_string(),
                    email: format!("{id}@x"),
                    avatar_url: String::new(),
                    password_hash: None,
                    salt: String::new(),
                    pepper: String::new(),
                    is_admin: false,
                    disabled: false,
                    quota_mb: None,
                    partners: Vec::new(),
                    totp_secret: None,
                },
            );
        }

        let mut groups = HashMap::new();
        groups.insert(
            "grp_family".to_string(),
            Group {
                id: "grp_family".to_string(),
                name: "Family".to_string(),
                owner_id: "usr_alice".to_string(),
                member_ids: vec!["usr_alice".to_string(), "usr_bob".to_string()],
            },
        );

        let mut photos = HashMap::new();
        // alice's own photos
        photos.insert(
            "ph_a1".to_string(),
            photo("ph_a1", "usr_alice", "2026-06-20T10:00:00Z"),
        );
        photos.insert(
            "ph_a2".to_string(),
            photo("ph_a2", "usr_alice", "2026-06-22T10:00:00Z"),
        );
        // shared album photos owned by alice
        photos.insert(
            "ph_s1".to_string(),
            photo("ph_s1", "usr_alice", "2026-06-21T10:00:00Z"),
        );
        photos.insert(
            "ph_s2".to_string(),
            photo("ph_s2", "usr_alice", "2026-06-19T10:00:00Z"),
        );

        let mut albums = HashMap::new();
        // album owned by alice, shared to grp_family (bob is a member)
        albums.insert(
            "alb_shared".to_string(),
            Album {
                id: "alb_shared".to_string(),
                name: "Shared".to_string(),
                owner_id: "usr_alice".to_string(),
                cover_seed: 1,
                photo_ids: vec!["ph_s1".to_string(), "ph_s2".to_string()],
                shares: vec![Share {
                    target: ShareTarget::Group("grp_family".to_string()),
                    role: ShareRole::Viewer,
                }],
            },
        );

        AppState {
            users,
            groups,
            photos,
            albums,
            prefs: HashMap::new(),
            vaults: HashMap::new(),
            storage: StorageSettings::default(),
            smtp: None,
            invites: HashMap::new(),
            reset_tokens: HashMap::new(),
            jobs: default_jobs(),
            thumbs: HashMap::new(),
            originals: HashMap::new(),
            password_secret: DEV_PASSWORD_SECRET.to_vec(),
            data_dir: DEFAULT_DATA_DIR.to_string(),
            persistence: None,
            sessions: HashMap::new(),
            worker_utils: None,
            lockouts: HashMap::new(),
            ml: None,
            plugins: None,
            webauthn: None,
            oidc_login: None,
            imports: HashMap::new(),
            pending_bytes: HashMap::new(),
            companion_bytes: HashMap::new(),
            duplicate_groups: HashMap::new(),
            dlna_devices: HashMap::new(),
            faces: HashMap::new(),
            people: HashMap::new(),
            counter: AtomicU64::new(1),
        }
    }

    #[test]
    fn owner_sees_own_photos() {
        let st = test_state();
        let tl = st.timeline_photos("usr_alice");
        let ids: Vec<&str> = tl.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"ph_a1"));
        assert!(ids.contains(&"ph_a2"));
        // sorted descending by taken_at
        assert_eq!(tl.first().unwrap().id, "ph_a2");
    }

    #[test]
    fn group_member_sees_group_shared_album() {
        let mut st = test_state();
        // bob defaults to show_shared = true
        st.prefs
            .insert("usr_bob".to_string(), TimelinePrefs::default());
        let tl = st.timeline_photos("usr_bob");
        let ids: Vec<&str> = tl.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"ph_s1"));
        assert!(ids.contains(&"ph_s2"));
    }

    #[test]
    fn per_album_override_false_hides_shared_album() {
        let mut st = test_state();
        let mut prefs = TimelinePrefs::default(); // global true
        prefs.per_album.insert("alb_shared".to_string(), false);
        st.prefs.insert("usr_bob".to_string(), prefs);
        let tl = st.timeline_photos("usr_bob");
        let ids: Vec<&str> = tl.iter().map(|p| p.id.as_str()).collect();
        assert!(!ids.contains(&"ph_s1"));
        assert!(!ids.contains(&"ph_s2"));
    }

    #[test]
    fn global_show_shared_false_hides_album() {
        let mut st = test_state();
        st.prefs.insert(
            "usr_bob".to_string(),
            TimelinePrefs {
                show_shared: false,
                per_album: HashMap::new(),
            },
        );
        let tl = st.timeline_photos("usr_bob");
        assert!(tl.is_empty());
    }

    #[test]
    fn unshared_user_does_not_see_album() {
        let st = test_state();
        // eve is not in grp_family and not a direct share target
        let tl = st.timeline_photos("usr_eve");
        assert!(tl.is_empty());
    }

    #[test]
    fn dedup_when_owner_also_shared() {
        let mut st = test_state();
        // Share the album directly to alice too (she is also the owner).
        st.albums
            .get_mut("alb_shared")
            .unwrap()
            .shares
            .push(Share {
                target: ShareTarget::User("usr_alice".to_string()),
                role: ShareRole::Viewer,
            });
        let tl = st.timeline_photos("usr_alice");
        // ph_s1 / ph_s2 are alice's own photos; must appear exactly once.
        let count_s1 = tl.iter().filter(|p| p.id == "ph_s1").count();
        assert_eq!(count_s1, 1);
        // total distinct photos = 4 (a1, a2, s1, s2)
        assert_eq!(tl.len(), 4);
    }

    fn empty_state() -> AppState {
        AppState {
            users: HashMap::new(),
            groups: HashMap::new(),
            photos: HashMap::new(),
            albums: HashMap::new(),
            prefs: HashMap::new(),
            vaults: HashMap::new(),
            storage: StorageSettings::default(),
            smtp: None,
            invites: HashMap::new(),
            reset_tokens: HashMap::new(),
            jobs: default_jobs(),
            thumbs: HashMap::new(),
            originals: HashMap::new(),
            password_secret: DEV_PASSWORD_SECRET.to_vec(),
            data_dir: DEFAULT_DATA_DIR.to_string(),
            persistence: None,
            sessions: HashMap::new(),
            worker_utils: None,
            lockouts: HashMap::new(),
            ml: None,
            plugins: None,
            webauthn: None,
            oidc_login: None,
            imports: HashMap::new(),
            pending_bytes: HashMap::new(),
            companion_bytes: HashMap::new(),
            duplicate_groups: HashMap::new(),
            dlna_devices: HashMap::new(),
            faces: HashMap::new(),
            people: HashMap::new(),
            counter: AtomicU64::new(1),
        }
    }

    #[test]
    fn ingest_jpg_raw_pair_collapses_to_one_photo() {
        let mut st = empty_state();
        let ids = st.ingest_upload(
            "usr_alice",
            vec![
                uf("IMG_4021.ARW", "ARW", "2026-06-20T10:00:00Z"),
                uf("IMG_4021.jpg", "jpg", "2026-06-20T10:00:00Z"),
            ],
        );
        assert_eq!(ids.len(), 1);
        let p = &st.photos[&ids[0]];
        assert_eq!(p.kind, "photo");
        assert_eq!(p.filename, "IMG_4021.jpg");
        assert_eq!(p.companions.len(), 1);
        assert_eq!(p.companions[0].kind, "raw");
        assert_eq!(p.companions[0].ext, "ARW");
        assert!(p.companions[0].downloadable);
    }

    #[test]
    fn ingest_same_basename_different_dates_stay_separate() {
        let mut st = empty_state();
        let ids = st.ingest_upload(
            "usr_alice",
            vec![
                uf("IMG_4021.jpg", "jpg", "2026-06-20T10:00:00Z"),
                uf("IMG_4021.jpg", "jpg", "2026-06-21T10:00:00Z"),
            ],
        );
        assert_eq!(ids.len(), 2);
        for id in &ids {
            assert!(st.photos[id].companions.is_empty());
        }
    }

    #[test]
    fn ingest_lone_raw_becomes_raw_photo() {
        let mut st = empty_state();
        let ids = st.ingest_upload(
            "usr_alice",
            vec![uf("DSC_0007.NEF", "NEF", "2026-06-20T10:00:00Z")],
        );
        assert_eq!(ids.len(), 1);
        let p = &st.photos[&ids[0]];
        assert_eq!(p.kind, "raw");
        assert!(p.companions.is_empty());
    }

    #[test]
    fn ingest_two_rasters_one_primary_one_companion() {
        let mut st = empty_state();
        let ids = st.ingest_upload(
            "usr_alice",
            vec![
                uf("vacation.png", "png", "2026-06-20T10:00:00Z"),
                uf("vacation.jpg", "jpg", "2026-06-20T10:00:00Z"),
            ],
        );
        assert_eq!(ids.len(), 1);
        let p = &st.photos[&ids[0]];
        assert_eq!(p.kind, "photo");
        assert_eq!(p.companions.len(), 1);
        // both are raster -> tie broken by filename; jpg < png, so jpg primary.
        assert_eq!(p.filename, "vacation.jpg");
        assert_eq!(p.companions[0].ext, "png");
    }

    // ---- Lifecycle: trash / archive / purge ----

    #[test]
    fn trashing_hides_from_timeline() {
        let mut st = test_state();
        st.photos.get_mut("ph_a1").unwrap().deleted_at = Some(now_rfc3339());
        let tl = st.timeline_photos("usr_alice");
        let ids: Vec<&str> = tl.iter().map(|p| p.id.as_str()).collect();
        assert!(!ids.contains(&"ph_a1"));
        assert!(ids.contains(&"ph_a2"));
    }

    #[test]
    fn restore_brings_back_into_timeline() {
        let mut st = test_state();
        st.photos.get_mut("ph_a1").unwrap().deleted_at = Some(now_rfc3339());
        // restore = clear deleted_at
        st.photos.get_mut("ph_a1").unwrap().deleted_at = None;
        let tl = st.timeline_photos("usr_alice");
        let ids: Vec<&str> = tl.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"ph_a1"));
    }

    #[test]
    fn archiving_hides_from_timeline() {
        let mut st = test_state();
        st.photos.get_mut("ph_a1").unwrap().archived = true;
        let tl = st.timeline_photos("usr_alice");
        let ids: Vec<&str> = tl.iter().map(|p| p.id.as_str()).collect();
        assert!(!ids.contains(&"ph_a1"));
        // still present in storage (not deleted)
        assert!(st.photos.contains_key("ph_a1"));
    }

    #[test]
    fn purge_removes_only_expired_trash() {
        let mut st = test_state();
        // retention default = 7 days
        assert_eq!(st.storage.trash_retention_days, 7);
        // ph_a1: deleted long ago -> should be purged
        st.photos.get_mut("ph_a1").unwrap().deleted_at =
            Some("2020-01-01T00:00:00Z".to_string());
        // ph_a2: deleted just now -> should remain
        st.photos.get_mut("ph_a2").unwrap().deleted_at = Some(now_rfc3339());

        let purged = st.purge_expired_trash();
        assert_eq!(purged, vec!["ph_a1".to_string()]);
        assert!(!st.photos.contains_key("ph_a1"));
        assert!(st.photos.contains_key("ph_a2"));
    }

    #[test]
    fn purge_respects_updated_retention() {
        let mut st = test_state();
        // deleted ~3 days ago
        let three_days_ago = OffsetDateTime::now_utc() - time::Duration::days(3);
        st.photos.get_mut("ph_a1").unwrap().deleted_at =
            Some(three_days_ago.format(&Rfc3339).unwrap());

        // default 7-day retention: not yet expired
        assert!(st.purge_expired_trash().is_empty());

        // shrink retention to 1 day: now it should purge
        st.storage.trash_retention_days = 1;
        let purged = st.purge_expired_trash();
        assert_eq!(purged, vec!["ph_a1".to_string()]);
    }

    #[tokio::test]
    async fn backup_run_marks_photos_and_sets_count_when_disabled() {
        // Backup disabled by default -> no-op, count 0, nothing marked.
        let mut st = test_state();
        let count = st.run_backup().await.unwrap();
        assert_eq!(count, 0);
        assert_eq!(st.storage.backup.last_backup_count, 0);
    }

    // ---- Feature 1: share roles + contribution + search ----

    /// Make `alb_shared` a Contributor share to `target` (replacing existing
    /// share with the same target). Also adds a couple of bob/carol photos.
    fn contrib_state() -> AppState {
        let mut st = test_state();
        // bob & carol each own a photo not in any shared album.
        st.photos.insert(
            "ph_bob1".to_string(),
            photo("ph_bob1", "usr_bob", "2026-06-18T10:00:00Z"),
        );
        st.photos.insert(
            "ph_carol1".to_string(),
            photo("ph_carol1", "usr_carol", "2026-06-17T10:00:00Z"),
        );
        st
    }

    fn set_share(st: &mut AppState, album: &str, target: ShareTarget, role: ShareRole) {
        let a = st.albums.get_mut(album).unwrap();
        a.shares.retain(|s| s.target != target);
        a.shares.push(Share { target, role });
    }

    #[test]
    fn owner_and_contributor_can_contribute_viewer_cannot() {
        let mut st = contrib_state();
        // alice owns alb_shared.
        assert!(st.can_contribute("usr_alice", "alb_shared"));
        assert_eq!(
            st.album_role_for("usr_alice", "alb_shared"),
            Some(ShareRole::Contributor)
        );

        // bob is in grp_family (viewer share) -> cannot contribute yet.
        assert!(!st.can_contribute("usr_bob", "alb_shared"));
        assert_eq!(
            st.album_role_for("usr_bob", "alb_shared"),
            Some(ShareRole::Viewer)
        );

        // Direct contributor share to bob -> can contribute.
        set_share(
            &mut st,
            "alb_shared",
            ShareTarget::User("usr_bob".to_string()),
            ShareRole::Contributor,
        );
        assert!(st.can_contribute("usr_bob", "alb_shared"));
    }

    #[test]
    fn contributor_via_group_share() {
        let mut st = contrib_state();
        // Upgrade the grp_family share (bob is a member) to Contributor.
        set_share(
            &mut st,
            "alb_shared",
            ShareTarget::Group("grp_family".to_string()),
            ShareRole::Contributor,
        );
        assert!(st.can_contribute("usr_bob", "alb_shared"));
        // carol is not in grp_family -> still no access.
        assert!(!st.can_contribute("usr_carol", "alb_shared"));
    }

    #[test]
    fn non_share_target_has_no_role() {
        let st = contrib_state();
        // eve is unrelated.
        assert_eq!(st.album_role_for("usr_eve", "alb_shared"), None);
        assert!(!st.can_contribute("usr_eve", "alb_shared"));
        // missing album -> None.
        assert_eq!(st.album_role_for("usr_alice", "nope"), None);
    }

    #[test]
    fn search_returns_all_album_photos_for_shared_user_incl_non_owned() {
        let mut st = contrib_state();
        // Add a carol-owned photo into alice's shared album: bob (a viewer of
        // that album) must see it via search even though he doesn't own it.
        st.albums
            .get_mut("alb_shared")
            .unwrap()
            .photo_ids
            .push("ph_carol1".to_string());

        let res = st.search("usr_bob", "");
        let ids: Vec<&str> = res.iter().map(|p| p.id.as_str()).collect();
        // shared album photos, including the non-owned carol photo.
        assert!(ids.contains(&"ph_s1"));
        assert!(ids.contains(&"ph_s2"));
        assert!(ids.contains(&"ph_carol1"));
        // bob's own standalone photo too.
        assert!(ids.contains(&"ph_bob1"));
    }

    #[test]
    fn search_query_filters_case_insensitive() {
        let mut st = contrib_state();
        // Give ph_s1 a distinctive title.
        st.photos.get_mut("ph_s1").unwrap().overrides.title =
            Some("Sunset Over Lyon".to_string());
        let res = st.search("usr_alice", "sunset");
        let ids: Vec<&str> = res.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["ph_s1"]);
    }

    #[test]
    fn search_excludes_archived_trashed_and_vault() {
        let mut st = contrib_state();
        st.photos.get_mut("ph_a1").unwrap().archived = true;
        st.photos.get_mut("ph_a2").unwrap().deleted_at = Some(now_rfc3339());
        // Put ph_s1 in alice's vault.
        st.set_pin("usr_alice", "1234");
        st.vaults
            .get_mut("usr_alice")
            .unwrap()
            .photo_ids
            .push("ph_s1".to_string());

        let res = st.search("usr_alice", "");
        let ids: Vec<&str> = res.iter().map(|p| p.id.as_str()).collect();
        assert!(!ids.contains(&"ph_a1")); // archived
        assert!(!ids.contains(&"ph_a2")); // trashed
        assert!(!ids.contains(&"ph_s1")); // vaulted
        assert!(ids.contains(&"ph_s2")); // still visible
    }

    #[test]
    fn search_filtered_by_camera_and_period() {
        let mut st = contrib_state();
        {
            let p = st.photos.get_mut("ph_s1").unwrap();
            p.exif.camera = Some("Leica Q3".to_string());
            p.exif.taken_at = "2026-03-10T10:00:00Z".to_string();
        }
        {
            let p = st.photos.get_mut("ph_s2").unwrap();
            p.exif.camera = Some("Sony A7 IV".to_string());
            p.exif.taken_at = "2026-06-20T10:00:00Z".to_string();
        }
        // camera facet
        let by_cam = st.search_filtered(
            "usr_alice",
            &SearchFilters { camera: Some("leica".into()), ..Default::default() },
        );
        let ids: Vec<&str> = by_cam.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"ph_s1") && !ids.contains(&"ph_s2"));

        // period facet (March only)
        let by_period = st.search_filtered(
            "usr_alice",
            &SearchFilters {
                from: Some("2026-03-01".into()),
                to: Some("2026-03-31".into()),
                ..Default::default()
            },
        );
        let ids: Vec<&str> = by_period.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"ph_s1") && !ids.contains(&"ph_s2"));
    }

    // ---- Feature 2: vault PIN ----

    #[test]
    fn rate_lockout_after_repeated_failures() {
        let mut st = test_state();
        let key = "vault:usr_x";
        assert!(!st.rate_locked(key));
        for _ in 0..super::RATE_MAX_FAILS {
            assert!(!st.rate_locked(key), "should not lock before threshold");
            st.rate_fail(key);
        }
        assert!(st.rate_locked(key), "locked after threshold");
        st.rate_reset(key);
        assert!(!st.rate_locked(key), "reset clears lockout");
    }

    #[test]
    fn set_pin_then_verify() {
        let mut st = test_state();
        assert!(!st.verify_pin("usr_alice", "1234")); // no pin yet
        st.set_pin("usr_alice", "1234");
        assert!(st.verify_pin("usr_alice", "1234"));
        assert!(!st.verify_pin("usr_alice", "0000")); // wrong pin
    }

    #[test]
    fn changing_pin_changes_verification() {
        let mut st = test_state();
        st.set_pin("usr_alice", "1234");
        st.set_pin("usr_alice", "9999");
        assert!(!st.verify_pin("usr_alice", "1234"));
        assert!(st.verify_pin("usr_alice", "9999"));
    }

    #[test]
    fn vault_photos_excluded_from_timeline() {
        let mut st = test_state();
        // bob can normally see ph_s1/ph_s2 via the shared album.
        st.prefs
            .insert("usr_bob".to_string(), TimelinePrefs::default());
        // Vault ph_s1 (in alice's vault).
        st.set_pin("usr_alice", "1234");
        st.vaults
            .get_mut("usr_alice")
            .unwrap()
            .photo_ids
            .push("ph_s1".to_string());

        // Alice's own timeline excludes the vaulted photo.
        let tl_alice: Vec<String> =
            st.timeline_photos("usr_alice").iter().map(|p| p.id.clone()).collect();
        assert!(!tl_alice.contains(&"ph_s1".to_string()));
        // Bob's timeline (shared album) also excludes it.
        let tl_bob: Vec<String> =
            st.timeline_photos("usr_bob").iter().map(|p| p.id.clone()).collect();
        assert!(!tl_bob.contains(&"ph_s1".to_string()));
        assert!(tl_bob.contains(&"ph_s2".to_string()));
    }

    #[test]
    fn vault_views_returns_contents() {
        let mut st = test_state();
        st.set_pin("usr_alice", "1234");
        st.vaults
            .get_mut("usr_alice")
            .unwrap()
            .photo_ids
            .push("ph_a1".to_string());
        let views = st.vault_views("usr_alice");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, "ph_a1");
    }

    #[test]
    fn seed_alice_vault_pin_is_1234() {
        // Demo PIN documented in the seed: "1234".
        let st = seed();
        assert!(st.verify_pin("usr_alice", "1234"));
        assert!(!st.verify_pin("usr_alice", "0000"));
        let v = st.vaults.get("usr_alice").unwrap();
        assert_eq!(v.photo_ids.len(), 2);
        // Never store plaintext.
        assert!(v.pin_hash.is_some());
        assert_ne!(v.pin_hash.as_deref(), Some("1234"));
    }

    // ---- Feature 1: user passwords ----

    #[test]
    fn set_password_then_verify() {
        let mut st = test_state();
        let secret = st.password_secret().to_vec();
        let pepper = st.new_pepper();
        let u = st.users.get_mut("usr_alice").unwrap();
        assert!(!u.verify_password(&secret, "hunter2")); // no password yet
        u.set_password(&secret, pepper, "hunter2");
        assert!(u.verify_password(&secret, "hunter2"));
        assert!(!u.verify_password(&secret, "wrong")); // wrong password fails
        // The argon2id PHC string is stored, never the plaintext.
        let phc = u.password_hash.clone().unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(!phc.contains("hunter2"));
    }

    #[test]
    fn serialized_user_never_contains_password_or_salt() {
        let st = seed();
        let user = st.users.get("usr_alice").unwrap();
        // Sanity: alice DOES have a hash + salt in memory.
        assert!(user.password_hash.is_some());
        assert!(!user.salt.is_empty());
        // But the serialized JSON must NOT expose either.
        let v = serde_json::to_value(user).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("password_hash"));
        assert!(!obj.contains_key("salt"));
        assert!(!obj.contains_key("pepper"));
        // And of course the secret values never appear anywhere in the JSON.
        let text = serde_json::to_string(user).unwrap();
        assert!(!text.contains(user.salt.as_str()));
        assert!(!user.pepper.is_empty() && !text.contains(user.pepper.as_str()));
        assert!(!text.contains(user.password_hash.as_deref().unwrap()));
    }

    #[test]
    fn seed_demo_password_is_first_name_and_alice_is_admin() {
        // DEMO LOGINS: each seed user's password is their first name lowercased.
        let st = seed();
        let secret = st.password_secret().to_vec();
        for (id, password) in [
            ("usr_alice", "alice"),
            ("usr_bob", "bob"),
            ("usr_carol", "carol"),
            ("usr_dave", "dave"),
        ] {
            assert!(st.users.get(id).unwrap().verify_password(&secret, password));
            assert!(!st.users.get(id).unwrap().verify_password(&secret, "nope"));
        }
        assert!(st.users.get("usr_alice").unwrap().is_admin);
        assert!(!st.users.get("usr_bob").unwrap().is_admin);
    }

    // ---- Feature 3: stats counts ----

    #[test]
    fn audit_access_is_clean_on_seed() {
        let st = seed();
        let violations = st.audit_access();
        assert!(violations.is_empty(), "seed audit not clean: {violations:?}");
    }

    // ---- AI analysis (import stage 4) ----

    #[test]
    fn analyze_photo_sets_flag_and_fills_tags() {
        let mut st = test_state();
        // ph_a1 is a landscape photo (6240x4160) with no AI data yet.
        assert!(!st.photos["ph_a1"].analyzed);
        assert!(st.analyze_photo("ph_a1"));
        let p = &st.photos["ph_a1"];
        assert!(p.analyzed);
        assert!(p.ai_tags.contains(&"landscape".to_string()));
        // Unknown id -> false.
        assert!(!st.analyze_photo("nope"));
    }

    #[test]
    fn analyze_unanalyzed_processes_pending_only() {
        let mut st = test_state();
        st.photos.get_mut("ph_a1").unwrap().analyzed = true;
        let done = st.analyze_unanalyzed();
        assert!(!done.contains(&"ph_a1".to_string()));
        assert!(done.contains(&"ph_a2".to_string()));
        // A second pass finds nothing left.
        assert!(st.analyze_unanalyzed().is_empty());
    }

    #[test]
    fn search_finds_photo_by_ai_tag() {
        let mut st = test_state();
        st.photos.get_mut("ph_a1").unwrap().ai_tags = vec!["sunset".to_string()];
        let res = st.search("usr_alice", "sunset");
        let ids: Vec<&str> = res.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["ph_a1"]);
    }

    #[test]
    fn search_finds_photo_by_ocr_text() {
        let mut st = test_state();
        st.photos.get_mut("ph_a2").unwrap().ocr_text =
            Some("Welcome to the Museum".to_string());
        let res = st.search("usr_alice", "museum");
        let ids: Vec<&str> = res.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["ph_a2"]);
    }

    #[test]
    fn seed_has_searchable_ai_data() {
        let st = seed();
        // OCR caption seeded on a photo.
        let by_ocr = st.search("usr_alice", "bienvenue");
        assert!(!by_ocr.is_empty(), "ocr_text not searchable in seed");
        // AI context tag seeded on a photo.
        let by_tag = st.search("usr_alice", "mountains");
        assert!(!by_tag.is_empty(), "ai_tag not searchable in seed");
    }

    #[test]
    fn audit_timeline_subset_of_allowed_no_hidden_leak() {
        // For every seed user, timeline_photos ⊆ allowed and contains no
        // vault/archived/trashed photo.
        let st = seed();
        for uid in st.users.keys() {
            for p in st.timeline_photos(uid) {
                assert!(
                    st.allowed(uid, &p.id),
                    "{uid} sees {} without a grant",
                    p.id
                );
                let ph = st.photos.get(&p.id).unwrap();
                assert!(!ph.archived);
                assert!(ph.deleted_at.is_none());
                assert!(!st.is_in_any_vault(&p.id));
            }
        }
    }

    // ---- CONTEXT RECOGNITION (CLIP) ----

    /// Synthetic CLIP-style ranking: with no live sidecar, rank candidate photos
    /// by cosine similarity of their stored embeddings to a query vector and
    /// assert the ordering is best-match-first. This exercises the exact scoring
    /// logic `semantic_rank` uses (cosine similarity + descending sort), offline.
    #[test]
    fn cosine_ranking_orders_by_similarity() {
        // 3-dim toy embedding space. Query points along +x.
        let query = vec![1.0f32, 0.0, 0.0];
        // ph_close ~ aligned with query, ph_mid ~ 45deg, ph_far ~ orthogonal.
        let embeds = [
            ("ph_far", vec![0.0f32, 1.0, 0.0]),   // cos 0.0
            ("ph_close", vec![0.9f32, 0.1, 0.0]), // cos ~0.994
            ("ph_mid", vec![0.7f32, 0.7, 0.0]),   // cos ~0.707
        ];
        let mut scored: Vec<(&str, f32)> = embeds
            .iter()
            .map(|(id, v)| (*id, crate::ml::cosine_similarity(&query, v).unwrap()))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let order: Vec<&str> = scored.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec!["ph_close", "ph_mid", "ph_far"]);
    }

    /// `semantic_rank` is inert when ML is disabled: it returns `None` (so the
    /// caller falls back to keyword search) and never touches the network — even
    /// when candidates carry embeddings.
    #[tokio::test]
    async fn semantic_rank_none_when_ml_disabled() {
        let mut st = test_state();
        assert!(st.ml.is_none(), "test_state must have ML disabled");
        st.photos.get_mut("ph_a1").unwrap().clip_embedding = Some(vec![1.0, 0.0, 0.0]);
        let candidates = st.search("usr_alice", "");
        let res = st.semantic_rank(candidates, "yellow car", 0.2).await;
        assert!(res.is_none(), "ML disabled must yield no semantic ranking");
    }

    /// OCR is inert when ML is disabled: `ocr_photos` makes no network call and
    /// leaves each photo's `ocr_text` exactly as it was (offline behavior
    /// unchanged). The async import finalize/upload paths call this, so the
    /// offline upload pipeline must not mutate `ocr_text`.
    #[tokio::test]
    async fn ocr_photos_noop_when_ml_disabled() {
        let mut st = test_state();
        assert!(st.ml.is_none(), "test_state must have ML disabled");
        // Pre-set a sentinel; a no-op OCR must leave it untouched.
        st.photos.get_mut("ph_a1").unwrap().ocr_text = Some("keep me".to_string());
        let ids = vec!["ph_a1".to_string(), "ph_a2".to_string()];
        st.ocr_photos(None, &ids).await;
        assert_eq!(
            st.photos["ph_a1"].ocr_text.as_deref(),
            Some("keep me"),
            "OCR must not overwrite existing ocr_text when ML is disabled"
        );
        assert!(
            st.photos["ph_a2"].ocr_text.is_none(),
            "OCR must not set ocr_text when ML is disabled"
        );
    }

    /// With ML disabled, keyword/facet search is completely unchanged by this
    /// feature: results match the pre-existing substring behavior and embeddings
    /// (even if present) are ignored.
    #[test]
    fn keyword_search_unchanged_when_ml_disabled() {
        let mut st = test_state();
        assert!(st.ml.is_none());
        // An embedding on an otherwise non-matching photo must NOT make it match a
        // keyword query — keyword search ignores embeddings entirely.
        st.photos.get_mut("ph_a1").unwrap().clip_embedding = Some(vec![0.5, 0.5, 0.7]);
        st.photos.get_mut("ph_a2").unwrap().ai_tags = vec!["sunset".to_string()];
        let res = st.search("usr_alice", "sunset");
        let ids: Vec<&str> = res.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["ph_a2"]);
    }

    /// The `clip_embedding` field stays server-side: `PhotoView` exposes only the
    /// `has_embedding` bool, and the raw vector never appears in serialized JSON.
    #[test]
    fn embedding_not_serialized_only_has_embedding_bool() {
        let mut st = test_state();
        st.photos.get_mut("ph_a1").unwrap().clip_embedding = Some(vec![1.0, 2.0, 3.0]);
        let view = st.photos.get("ph_a1").unwrap().effective();
        assert!(view.has_embedding);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("clip_embedding"), "embedding leaked into PhotoView JSON");
        // And a photo without one reports false.
        let v2 = st.photos.get("ph_a2").unwrap().effective();
        assert!(!v2.has_embedding);
    }

    // ---- FEATURE A: partner relationship (directed grant) ----

    /// State where bob owns a couple of live photos (none in a shared album with
    /// alice), so partner grants are the only path for alice to see them.
    fn partner_state() -> AppState {
        let mut st = test_state();
        st.photos.insert(
            "ph_bob_live".to_string(),
            photo("ph_bob_live", "usr_bob", "2026-06-15T10:00:00Z"),
        );
        st.photos.insert(
            "ph_bob_arch".to_string(),
            photo("ph_bob_arch", "usr_bob", "2026-06-14T10:00:00Z"),
        );
        st.photos.insert(
            "ph_bob_trash".to_string(),
            photo("ph_bob_trash", "usr_bob", "2026-06-13T10:00:00Z"),
        );
        st.photos.insert(
            "ph_bob_vault".to_string(),
            photo("ph_bob_vault", "usr_bob", "2026-06-12T10:00:00Z"),
        );
        st.photos.get_mut("ph_bob_arch").unwrap().archived = true;
        st.photos.get_mut("ph_bob_trash").unwrap().deleted_at = Some(now_rfc3339());
        st.set_pin("usr_bob", "1234");
        st.vaults
            .get_mut("usr_bob")
            .unwrap()
            .photo_ids
            .push("ph_bob_vault".to_string());
        st
    }

    #[test]
    fn partner_grant_shares_live_photos_in_timeline_and_search() {
        let mut st = partner_state();
        // Bob grants alice partner access.
        st.users.get_mut("usr_bob").unwrap().partners.push("usr_alice".to_string());

        let tl: Vec<String> =
            st.timeline_photos("usr_alice").iter().map(|p| p.id.clone()).collect();
        assert!(tl.contains(&"ph_bob_live".to_string()), "live partner photo in timeline");
        // Excluded states never surface via the grant.
        assert!(!tl.contains(&"ph_bob_arch".to_string()));
        assert!(!tl.contains(&"ph_bob_trash".to_string()));
        assert!(!tl.contains(&"ph_bob_vault".to_string()));

        let sr: Vec<String> = st.search("usr_alice", "").iter().map(|p| p.id.clone()).collect();
        assert!(sr.contains(&"ph_bob_live".to_string()), "live partner photo in search");
        assert!(!sr.contains(&"ph_bob_arch".to_string()));
        assert!(!sr.contains(&"ph_bob_trash".to_string()));
        assert!(!sr.contains(&"ph_bob_vault".to_string()));
    }

    #[test]
    fn partner_grant_is_directed_not_reverse() {
        let mut st = partner_state();
        // Bob grants alice; this must NOT let bob see alice's photos.
        st.users.get_mut("usr_bob").unwrap().partners.push("usr_alice".to_string());
        let bob_tl: Vec<String> =
            st.timeline_photos("usr_bob").iter().map(|p| p.id.clone()).collect();
        assert!(!bob_tl.contains(&"ph_a1".to_string()));
        assert!(!bob_tl.contains(&"ph_a2".to_string()));
        // partner_grantors is the inverse view: alice was granted BY bob.
        assert_eq!(st.partner_grantors("usr_alice"), vec!["usr_bob"]);
        assert!(st.partner_grantors("usr_bob").is_empty());
    }

    #[test]
    fn removing_partner_revokes_access() {
        let mut st = partner_state();
        st.users.get_mut("usr_bob").unwrap().partners.push("usr_alice".to_string());
        assert!(st.search("usr_alice", "").iter().any(|p| p.id == "ph_bob_live"));
        // Revoke.
        st.users.get_mut("usr_bob").unwrap().partners.retain(|p| p != "usr_alice");
        let sr: Vec<String> = st.search("usr_alice", "").iter().map(|p| p.id.clone()).collect();
        assert!(!sr.contains(&"ph_bob_live".to_string()), "access revoked");
    }

    #[test]
    fn partner_dedup_with_album_share() {
        // Bob shares an album of his to alice AND grants her partner access; the
        // album's photo must appear exactly once.
        let mut st = partner_state();
        st.albums.insert(
            "alb_bob".to_string(),
            Album {
                id: "alb_bob".to_string(),
                name: "Bob".to_string(),
                owner_id: "usr_bob".to_string(),
                cover_seed: 1,
                photo_ids: vec!["ph_bob_live".to_string()],
                shares: vec![Share {
                    target: ShareTarget::User("usr_alice".to_string()),
                    role: ShareRole::Viewer,
                }],
            },
        );
        st.users.get_mut("usr_bob").unwrap().partners.push("usr_alice".to_string());
        let sr = st.search("usr_alice", "");
        assert_eq!(sr.iter().filter(|p| p.id == "ph_bob_live").count(), 1);
        let tl = st.timeline_photos("usr_alice");
        assert_eq!(tl.iter().filter(|p| p.id == "ph_bob_live").count(), 1);
    }

    #[test]
    fn partner_audit_access_stays_clean() {
        // A partner grant must not register as an access violation.
        let mut st = partner_state();
        st.users.get_mut("usr_bob").unwrap().partners.push("usr_alice".to_string());
        let violations = st.audit_access();
        assert!(violations.is_empty(), "partner grant tripped audit: {violations:?}");
    }

    #[test]
    fn seed_bob_partners_alice() {
        let st = seed();
        assert!(st.users.get("usr_bob").unwrap().partners.contains(&"usr_alice".to_string()));
        // Alice therefore sees bob's live photos via search (bob owns some seed photos).
        let bob_live: Vec<String> = st
            .photos
            .values()
            .filter(|p| p.owner_id == "usr_bob" && p.deleted_at.is_none() && !p.archived && !st.is_in_any_vault(&p.id))
            .map(|p| p.id.clone())
            .collect();
        assert!(!bob_live.is_empty());
        let alice_search: Vec<String> =
            st.search("usr_alice", "").iter().map(|p| p.id.clone()).collect();
        assert!(bob_live.iter().all(|id| alice_search.contains(id)));
    }

    // ---- FEATURE B: duplicate detection (perceptual hashing) ----

    /// Encode a tiny solid-color RGB image to PNG bytes (so the `image` crate can
    /// decode it during perceptual hashing). Distinct `tint` => distinct image.
    fn png_solid(w: u32, h: u32, tint: u8) -> Vec<u8> {
        use std::io::Cursor;
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // A smooth gradient keyed by tint so two different tints hash apart.
            *px = image::Rgb([
                ((x + tint as u32) % 256) as u8,
                ((y + tint as u32) % 256) as u8,
                tint,
            ]);
        }
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn detect_duplicates_groups_identical_images() {
        let mut st = empty_state();
        // Two photos for alice with IDENTICAL thumbnail bytes => same hash.
        let dup = png_solid(64, 64, 10);
        for id in ["ph_dup_a", "ph_dup_b"] {
            st.photos.insert(id.to_string(), photo(id, "usr_alice", "2026-06-20T10:00:00Z"));
            st.thumbs.insert(id.to_string(), (dup.clone(), "image/png".to_string()));
        }
        // A visually DISTINCT third photo for alice.
        st.photos.insert(
            "ph_distinct".to_string(),
            photo("ph_distinct", "usr_alice", "2026-06-20T10:00:00Z"),
        );
        st.thumbs
            .insert("ph_distinct".to_string(), (png_solid(64, 64, 200), "image/png".to_string()));

        let count = st.detect_duplicates();
        assert_eq!(count, 2, "the two identical photos form a duplicate group");
        let groups = &st.duplicate_groups["usr_alice"];
        assert_eq!(groups.len(), 1);
        let mut g = groups[0].clone();
        g.sort();
        assert_eq!(g, vec!["ph_dup_a".to_string(), "ph_dup_b".to_string()]);
        // The distinct photo is NOT in any group.
        assert!(!groups.iter().any(|grp| grp.contains(&"ph_distinct".to_string())));
    }

    #[test]
    fn detect_duplicates_skips_photos_without_thumbnail() {
        let mut st = empty_state();
        // Seed-like photos with NO thumbnail bytes are skipped (no hash).
        st.photos.insert("ph_x".to_string(), photo("ph_x", "usr_alice", "2026-06-20T10:00:00Z"));
        st.photos.insert("ph_y".to_string(), photo("ph_y", "usr_alice", "2026-06-20T10:00:00Z"));
        let count = st.detect_duplicates();
        assert_eq!(count, 0);
        assert!(st.duplicate_groups.is_empty());
    }

    #[test]
    fn duplicate_views_resolves_groups_and_drops_trashed() {
        let mut st = empty_state();
        let dup = png_solid(64, 64, 42);
        for id in ["ph_d1", "ph_d2", "ph_d3"] {
            st.photos.insert(id.to_string(), photo(id, "usr_alice", "2026-06-20T10:00:00Z"));
            st.thumbs.insert(id.to_string(), (dup.clone(), "image/png".to_string()));
        }
        let count = st.detect_duplicates();
        assert_eq!(count, 3);
        let views = st.duplicate_views("usr_alice");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].len(), 3);
        // Trash one member: the resolved group shrinks but stays a duplicate (>=2).
        st.photos.get_mut("ph_d3").unwrap().deleted_at = Some(now_rfc3339());
        let views = st.duplicate_views("usr_alice");
        assert_eq!(views[0].len(), 2);
        // Other owner with no dupes returns nothing.
        assert!(st.duplicate_views("usr_bob").is_empty());
    }

    // ---- Face recognition: clustering + naming propagation ----

    /// Insert a face with the given embedding into state (under `owner`, on
    /// `photo_id`). The embedding is L2-normalized so cosine acts like a dot.
    fn add_face(st: &mut AppState, id: &str, owner: &str, photo_id: &str, emb: Vec<f32>) {
        let norm = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        let emb: Vec<f32> = if norm > 0.0 { emb.iter().map(|x| x / norm).collect() } else { emb };
        st.faces.insert(
            id.to_string(),
            crate::models::Face {
                id: id.to_string(),
                photo_id: photo_id.to_string(),
                owner_id: owner.to_string(),
                bbox: [0.0, 0.0, 10.0, 10.0],
                embedding: emb,
                score: 0.99,
                person_id: None,
                ignored: false,
                assigned_label: None,
                confirmed: false,
            },
        );
    }

    #[test]
    fn cluster_faces_groups_near_and_splits_distant() {
        let mut st = empty_state();
        st.photos.insert("ph_f1".into(), photo("ph_f1", "usr_alice", "2026-06-20T10:00:00Z"));
        st.photos.insert("ph_f2".into(), photo("ph_f2", "usr_alice", "2026-06-21T10:00:00Z"));
        st.photos.insert("ph_f3".into(), photo("ph_f3", "usr_alice", "2026-06-22T10:00:00Z"));
        // Two near-identical embeddings (same person) + one orthogonal (distinct).
        add_face(&mut st, "face_a", "usr_alice", "ph_f1", vec![1.0, 0.0, 0.0, 0.02]);
        add_face(&mut st, "face_b", "usr_alice", "ph_f2", vec![1.0, 0.0, 0.0, 0.0]);
        add_face(&mut st, "face_c", "usr_alice", "ph_f3", vec![0.0, 1.0, 0.0, 0.0]);

        let count = st.cluster_faces("usr_alice");
        assert_eq!(count, 2, "two near faces cluster, the distant one is separate");

        // The two near faces share a person; the distant face is in another.
        let pa = st.faces["face_a"].person_id.clone().unwrap();
        let pb = st.faces["face_b"].person_id.clone().unwrap();
        let pc = st.faces["face_c"].person_id.clone().unwrap();
        assert_eq!(pa, pb, "near faces -> same person");
        assert_ne!(pa, pc, "distant face -> new person");

        // people_views never exposes embeddings; counts add up.
        let views = st.people_views("usr_alice");
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].face_count, 2, "largest cluster first");
        assert!(views[0].cover.is_some());
    }

    #[test]
    fn people_studio_curation_persists_across_recluster() {
        let mut st = empty_state();
        for (i, p) in ["ph_g1", "ph_g2", "ph_g3"].iter().enumerate() {
            st.photos.insert(p.to_string(), photo(p, "usr_alice", &format!("2026-06-2{i}T10:00:00Z")));
        }
        // Three MUTUALLY DISTINCT embeddings → three separate clusters.
        add_face(&mut st, "f_a", "usr_alice", "ph_g1", vec![1.0, 0.0, 0.0]);
        add_face(&mut st, "f_b", "usr_alice", "ph_g2", vec![0.0, 1.0, 0.0]);
        add_face(&mut st, "f_c", "usr_alice", "ph_g3", vec![0.0, 0.0, 1.0]);
        assert_eq!(st.cluster_faces("usr_alice"), 3, "distinct faces → 3 clusters");

        let pa = st.faces["f_a"].person_id.clone().unwrap();
        let pb = st.faces["f_b"].person_id.clone().unwrap();

        // MERGE b into a: the recognizer split them but they're the same person.
        st.merge_people(&pb, &pa).unwrap();
        assert_eq!(st.people_views("usr_alice").len(), 2, "merge → 2 clusters");
        assert_eq!(st.faces["f_a"].person_id, st.faces["f_b"].person_id);
        // The merge STICKS across a full re-cluster (orthogonal embeddings would
        // otherwise re-split) because both faces now share an assigned_label.
        assert_eq!(st.cluster_faces("usr_alice"), 2, "merge survives re-cluster");
        assert_eq!(st.faces["f_a"].person_id, st.faces["f_b"].person_id, "still merged");
        assert!(st.faces["f_a"].assigned_label.is_some());

        // MOVE f_c into the merged person; it sticks across re-cluster too.
        let merged = st.faces["f_a"].person_id.clone().unwrap();
        let pc = st.faces["f_c"].person_id.clone().unwrap();
        st.move_faces(&pc, &["f_c".into()], &merged).unwrap();
        assert_eq!(st.people_views("usr_alice").len(), 1, "all three now one person");
        assert_eq!(st.cluster_faces("usr_alice"), 1, "move survives re-cluster");

        // IGNORE f_a: it leaves People and stays out across re-cluster.
        let merged = st.faces["f_a"].person_id.clone().unwrap();
        st.ignore_faces(&merged, &["f_a".into()]).unwrap();
        assert!(st.faces["f_a"].ignored);
        assert!(st.faces["f_a"].person_id.is_none());
        st.cluster_faces("usr_alice");
        assert!(st.faces["f_a"].person_id.is_none(), "ignored face never re-joins a person");
        // The remaining two faces are still one person.
        assert_eq!(st.faces["f_b"].person_id, st.faces["f_c"].person_id);

        // HIDE that person: gone from the People surface, kept in storage.
        let person = st.faces["f_b"].person_id.clone().unwrap();
        st.hide_person(&person).unwrap();
        assert!(st.people_views("usr_alice").is_empty(), "hidden person not surfaced");
        assert!(st.people.values().any(|p| p.hidden), "but still stored as hidden");
    }

    #[test]
    fn naming_a_person_propagates_to_ai_people_and_is_searchable() {
        let mut st = empty_state();
        st.users.insert(
            "usr_alice".into(),
            User {
                id: "usr_alice".into(),
                name: "Alice".into(),
                email: "a@x".into(),
                avatar_url: String::new(),
                password_hash: None,
                salt: String::new(),
                pepper: String::new(),
                is_admin: false,
                disabled: false,
                quota_mb: None,
                partners: Vec::new(),
                totp_secret: None,
            },
        );
        st.photos.insert("ph_f1".into(), photo("ph_f1", "usr_alice", "2026-06-20T10:00:00Z"));
        st.photos.insert("ph_f2".into(), photo("ph_f2", "usr_alice", "2026-06-21T10:00:00Z"));
        add_face(&mut st, "face_a", "usr_alice", "ph_f1", vec![1.0, 0.0, 0.0]);
        add_face(&mut st, "face_b", "usr_alice", "ph_f2", vec![1.0, 0.0, 0.01]);
        st.cluster_faces("usr_alice");

        let person_id = st.people.values().next().unwrap().id.clone();
        // Before naming, ai_people is empty (unnamed clusters contribute nothing).
        assert!(st.photos["ph_f1"].ai_people.is_empty());

        let owner = st.name_person(&person_id, "Charlie").unwrap();
        assert_eq!(owner, "usr_alice");
        assert!(st.photos["ph_f1"].ai_people.contains(&"Charlie".to_string()));
        assert!(st.photos["ph_f2"].ai_people.contains(&"Charlie".to_string()));

        // The name is now searchable (ai_people is in photo_matches).
        let hits = st.search("usr_alice", "charlie");
        assert_eq!(hits.len(), 2);

        // Renaming to empty clears it from ai_people.
        st.name_person(&person_id, "  ").unwrap();
        assert!(st.photos["ph_f1"].ai_people.is_empty());
        assert!(st.search("usr_alice", "charlie").is_empty());
    }

    #[test]
    fn clustering_preserves_name_across_recluster() {
        let mut st = empty_state();
        st.photos.insert("ph_f1".into(), photo("ph_f1", "usr_alice", "2026-06-20T10:00:00Z"));
        add_face(&mut st, "face_a", "usr_alice", "ph_f1", vec![1.0, 0.0, 0.0]);
        st.cluster_faces("usr_alice");
        let pid = st.people.values().next().unwrap().id.clone();
        st.name_person(&pid, "Dana").unwrap();

        // A second face of the same person arrives, then re-cluster.
        st.photos.insert("ph_f2".into(), photo("ph_f2", "usr_alice", "2026-06-21T10:00:00Z"));
        add_face(&mut st, "face_b", "usr_alice", "ph_f2", vec![1.0, 0.0, 0.02]);
        st.cluster_faces("usr_alice");

        // The name carries over to the re-built cluster (membership overlap).
        let named: Vec<&str> = st
            .people
            .values()
            .filter_map(|p| p.name.as_deref())
            .collect();
        assert_eq!(named, vec!["Dana"]);
    }

    #[test]
    fn person_photos_scoped_to_owner() {
        let mut st = empty_state();
        st.photos.insert("ph_f1".into(), photo("ph_f1", "usr_alice", "2026-06-20T10:00:00Z"));
        add_face(&mut st, "face_a", "usr_alice", "ph_f1", vec![1.0, 0.0, 0.0]);
        st.cluster_faces("usr_alice");
        let pid = st.people.values().next().unwrap().id.clone();
        // Owner sees the person's photos; a non-owner is denied (None).
        assert_eq!(st.person_photos("usr_alice", &pid).unwrap().len(), 1);
        assert!(st.person_photos("usr_bob", &pid).is_none());
    }

    /// Two distinct people clusters; build them, link them (mother), and assert the
    /// reciprocal edge, that the link survives a re-cluster (ids regenerate but are
    /// remapped by face overlap), and that unlinking removes both directions.
    #[test]
    fn kinship_links_are_reciprocal_and_survive_recluster() {
        let mut st = empty_state();
        for (i, day) in ["20", "21"].iter().enumerate() {
            let id = format!("ph_k{i}");
            st.photos.insert(id.clone(), photo(&id, "usr_alice", &format!("2026-06-{day}T10:00:00Z")));
        }
        // Two orthogonal embeddings → two separate clusters.
        add_face(&mut st, "face_a", "usr_alice", "ph_k0", vec![1.0, 0.0, 0.0, 0.0]);
        add_face(&mut st, "face_b", "usr_alice", "ph_k1", vec![0.0, 1.0, 0.0, 0.0]);
        st.cluster_faces("usr_alice");
        let a = st.faces["face_a"].person_id.clone().unwrap();
        let b = st.faces["face_b"].person_id.clone().unwrap();
        assert_ne!(a, b);

        // Link: b is a's mother → a is b's child (inverse).
        let owner = st.link_people(&a, &b, "mother").unwrap();
        assert_eq!(owner, "usr_alice");
        let rel_a = &st.people[&a].relationships;
        assert_eq!(rel_a.len(), 1);
        assert_eq!(rel_a[0].person_id, b);
        assert_eq!(rel_a[0].relation, "mother");
        assert_eq!(st.people[&b].relationships[0].relation, "child");

        // Invalid links are rejected.
        assert!(st.link_people(&a, &a, "self").is_none());
        assert!(st.link_people(&a, &b, "   ").is_none());

        // Re-cluster: person ids regenerate, but the kinship edge must be remapped
        // to the new ids via face-set overlap and stay reciprocal.
        st.cluster_faces("usr_alice");
        let a2 = st.faces["face_a"].person_id.clone().unwrap();
        let b2 = st.faces["face_b"].person_id.clone().unwrap();
        assert_eq!(st.people[&a2].relationships.len(), 1, "edge survived re-cluster");
        assert_eq!(st.people[&a2].relationships[0].person_id, b2);
        assert_eq!(st.people[&a2].relationships[0].relation, "mother");
        assert_eq!(st.people[&b2].relationships[0].person_id, a2);

        // The view resolves the other person's (absent) name + relation.
        let views = st.people_views("usr_alice");
        let total_edges: usize = views.iter().map(|v| v.relationships.len()).sum();
        assert_eq!(total_edges, 2, "both directions present in views");

        // Unlink removes both directions.
        st.unlink_people(&a2, &b2).unwrap();
        assert!(st.people[&a2].relationships.is_empty());
        assert!(st.people[&b2].relationships.is_empty());
    }

    // ---- Upload durability ----

    /// `persist_blobs_durable` must actually write the original to the active
    /// (filesystem) backend, returning Ok — this is the guarantee the upload path
    /// awaits BEFORE acking the client.
    #[tokio::test]
    async fn persist_blobs_durable_writes_original_to_backend() {
        let mut st = empty_state();
        let dir = std::env::temp_dir().join(format!("photon_durable_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        st.data_dir = dir.to_string_lossy().to_string();
        // An original byte blob, as the create phase would have stashed it.
        st.originals
            .insert("ph_dur".to_string(), (vec![1, 2, 3, 4], "image/jpeg".to_string()));

        st.persist_blobs_durable(&["ph_dur".to_string()])
            .await
            .expect("durable write should succeed on the filesystem backend");

        // The blob is on disk under originals/ph_dur.<ext> before we would ack.
        let written = std::fs::read_dir(dir.join("originals"))
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("ph_dur."))
            })
            .unwrap_or(false);
        assert!(written, "original blob was not persisted to the backend");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resolve_session` serves cache hits and, with no DB configured, misses
    /// resolve to `None`. (The cross-instance DB fallback is exercised in DB mode.)
    #[tokio::test]
    async fn resolve_session_hits_cache_and_misses_without_db() {
        let mut st = empty_state(); // persistence = None
        let tok = st.create_session("usr_x");
        assert_eq!(st.resolve_session(&tok).await.as_deref(), Some("usr_x"));
        assert_eq!(st.resolve_session("bogus").await, None);
        assert!(st.end_session(&tok));
        assert_eq!(st.resolve_session(&tok).await, None);
    }
}
