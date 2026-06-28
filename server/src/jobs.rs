//! Durable background jobs via **graphile_worker** (Postgres-backed).
//!
//! In DB mode every background task — import enrichment, trash purge, S3 backup,
//! AI-analysis sweep, duplicate detection — is a graphile_worker job: durable
//! (survives restarts), retried on failure, and processed by ANY instance (cron
//! schedules are claimed once across the cluster). When `DATABASE_URL` is unset
//! there is no queue, so the caller falls back to the inline `tokio` path (see
//! `main.rs` interval jobs + `handlers::run_import_enrich`), keeping offline/test
//! behavior unchanged.
//!
//! The job BODIES live here as `run_*` free functions so both execution paths
//! (graphile task handler and the inline interval) call exactly the same code.

use graphile_worker::{
    Cron, IntoTaskHandlerResult, TaskHandler, WorkerContext, WorkerOptions, WorkerUtils,
};
use serde::{Deserialize, Serialize};

use crate::handlers::Shared;

/// Dedicated Postgres schema for the worker's own tables.
const SCHEMA: &str = "photon_worker";

/// Wrapper so the shared app state can be a worker extension: `add_extension`
/// requires `Debug`, which `AppState` doesn't (and shouldn't) implement.
#[derive(Clone)]
pub struct AppExt(pub Shared);
impl std::fmt::Debug for AppExt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppExt")
    }
}

/// Pull the shared app state out of the worker context (registered via
/// `add_extension`). Robust whether `get_ext` yields an `Arc<AppExt>` or `&AppExt`.
fn shared(ctx: &WorkerContext) -> Shared {
    (*ctx.get_ext::<AppExt>().expect("AppExt extension registered")).0.clone()
}

// ---- Job bodies (shared by the durable task handlers AND the inline intervals) ----
//
// POSTGRES-FIRST: each job loads a FRESH snapshot of domain state from the DB,
// does its work, and writes the results back to Postgres (the single source of
// truth). No job relies on a long-lived in-memory cache. Telemetry (`job_*`) is
// per-instance runtime state, so it's recorded on the live `Shared`.

use crate::state::AppState;

/// The set of jobs that can be triggered by name (cron, inline, or on-demand from
/// the admin console). The first four are the scheduled jobs; the last three are
/// "maintenance" re-processing passes.
pub const JOB_NAMES: &[&str] = &[
    "trash_purge",
    "s3_backup",
    "ai_analysis",
    "duplicates",
    "rebuild_thumbnails",
    "recluster_faces",
    "redetect_faces",
    "reset_faces",
    "reextract_metadata",
];

pub fn is_job(name: &str) -> bool {
    JOB_NAMES.contains(&name)
}

/// Run a named job end-to-end: mark telemetry running, load a fresh DB snapshot,
/// do the work (persisting results to Postgres), record a [`JobRun`] in the
/// history, and update telemetry. `trigger` is `"cron"` or `"manual"`. Returns the
/// recorded run (or `None` when there's no DB, i.e. tests/offline).
pub async fn run_named(st: &Shared, name: &str, trigger: &str) -> Option<crate::models::JobRun> {
    st.write().await.job_running(name);
    // Grab the plugin host up front (cheap Arc clone; it has its OWN RwLock so we
    // never hold the AppState lock across the plugin RPC). Used as the fallback for
    // any name that isn't a built-in job.
    let plugin_host = st.read().await.plugins.clone();
    let mut snap = crate::handlers::request_snapshot(st).await?;
    let started_at = crate::state::now_rfc3339();
    let t0 = std::time::Instant::now();

    let (outcome, items, result): (&str, i64, String) = match name {
        "trash_purge" => body_purge(&mut snap).await,
        "s3_backup" | "backup" => body_backup(&mut snap).await,
        "ai_analysis" => body_ai_analysis(&mut snap).await,
        "duplicates" => body_duplicates(&mut snap).await,
        "rebuild_thumbnails" => body_rebuild_thumbnails(&mut snap).await,
        "recluster_faces" => body_recluster_faces(&mut snap).await,
        "redetect_faces" => body_redetect_faces(&mut snap).await,
        "reset_faces" => body_reset_faces(&mut snap).await,
        "reextract_metadata" => body_reextract_metadata(&mut snap).await,
        // Plugin jobs: a subprocess plugin owns this name. Route it through the
        // SAME JobRun recording + telemetry wrapping as the built-ins below, so it
        // shows up in AdminStats history. Any transport/timeout error degrades to a
        // "failed" outcome (the host never panics); a name no plugin owns falls
        // through to "unknown job".
        _ => match &plugin_host {
            Some(h) => {
                // Stream the plugin's progress snapshots into the live job telemetry
                // so the admin console shows staged progress. The callback is sync,
                // so it forwards over an unbounded channel that a drain task pumps
                // into AppState; the channel closes (drain ends) when `run_job`
                // returns and drops the closure.
                let (ptx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<crate::state::JobProgress>();
                let st_progress = st.clone();
                let name_progress = name.to_string();
                let drain = tokio::spawn(async move {
                    while let Some(p) = prx.recv().await {
                        st_progress.write().await.job_progress(&name_progress, p);
                    }
                });
                let triple = h
                    .run_job(name, trigger, move |p| {
                        let _ = ptx.send(p);
                    })
                    .await
                    .unwrap_or_else(|| ("failed", 0, format!("unknown job {name}")));
                let _ = drain.await;
                triple
            }
            None => ("failed", 0, format!("unknown job {name}")),
        },
    };

    let run = crate::models::JobRun {
        name: name.to_string(),
        outcome: outcome.to_string(),
        items,
        started_at,
        duration_ms: t0.elapsed().as_millis() as i64,
        trigger: trigger.to_string(),
    };
    if let Some(p) = &snap.persistence {
        let _ = p.insert_job_run(&run).await;
    }
    st.write().await.job_done(name, result);
    Some(run)
}

// Thin wrappers kept for the cron task handlers + inline intervals (they pass
// `trigger = "cron"`). On-demand runs from the admin console pass `"manual"`.
pub async fn run_purge(st: &Shared) {
    run_named(st, "trash_purge", "cron").await;
}
pub async fn run_backup(st: &Shared) {
    run_named(st, "s3_backup", "cron").await;
}
pub async fn run_ai_analysis(st: &Shared) {
    run_named(st, "ai_analysis", "cron").await;
}
pub async fn run_duplicates(st: &Shared) {
    run_named(st, "duplicates", "cron").await;
}

// ---- Job bodies (operate on a fresh DB snapshot, persist results) ----

async fn body_purge(snap: &mut AppState) -> (&'static str, i64, String) {
    let purged = snap.purge_expired_trash();
    for id in &purged {
        snap.delete_photo_row(id).await;
    }
    // `purge_expired_trash` also dropped purged ids from albums in memory — write
    // those album rows back so the change is durable.
    let album_ids: Vec<String> = snap.albums.keys().cloned().collect();
    for aid in &album_ids {
        snap.persist_album(aid).await;
    }
    // Housekeeping: reap sessions past the absolute TTL (security hygiene; expired
    // tokens already fail to authenticate, this just bounds table growth).
    if let Some(p) = &snap.persistence {
        let _ = p.cleanup_sessions().await;
    }
    ("success", purged.len() as i64, format!("purged {}", purged.len()))
}

async fn body_backup(snap: &mut AppState) -> (&'static str, i64, String) {
    // `run_backup` persists each flipped `backed_up` flag itself (Postgres-first).
    match snap.run_backup().await {
        Ok(n) => ("success", n as i64, format!("backed up {n}")),
        Err(e) => ("failed", 0, format!("error: {e}")),
    }
}

async fn body_ai_analysis(snap: &mut AppState) -> (&'static str, i64, String) {
    let pending: Vec<String> =
        snap.photos.values().filter(|p| !p.analyzed).map(|p| p.id.clone()).collect();
    let mut analyzed = 0i64;
    for id in &pending {
        if snap.analyze_photo(id) {
            snap.persist_photo(id).await;
            analyzed += 1;
        }
    }
    ("success", analyzed, format!("analyzed {analyzed}"))
}

async fn body_duplicates(snap: &mut AppState) -> (&'static str, i64, String) {
    let found = snap.detect_duplicates();
    snap.persist_duplicates().await;
    let mut owners: Vec<String> = snap.faces.values().map(|f| f.owner_id.clone()).collect();
    owners.sort();
    owners.dedup();
    for owner in owners {
        snap.cluster_faces(&owner);
        snap.persist_faces(&owner).await;
    }
    ("success", found as i64, format!("found {found} duplicate photo(s)"))
}

// ---- Maintenance passes (re-process the whole library) ----

/// Re-render the thumbnail for every live photo from its stored original, push it
/// to the backend, and persist the updated `thumb_url`.
async fn body_rebuild_thumbnails(snap: &mut AppState) -> (&'static str, i64, String) {
    let ids: Vec<String> =
        snap.photos.values().filter(|p| p.deleted_at.is_none()).map(|p| p.id.clone()).collect();
    let mut rebuilt = 0i64;
    for id in &ids {
        let Some((bytes, _ct)) = snap.load_original(id).await else { continue };
        if let Some(thumb) = AppState::render_thumbnail_bytes(&bytes) {
            snap.thumbs.insert(id.clone(), (thumb, crate::transcode::MediaFormat::Webp.mime().to_string()));
            if let Some(p) = snap.photos.get_mut(id) {
                p.thumb_url = Some(format!("/api/photos/{id}/thumb"));
            }
            snap.persist_photo(id).await;
            rebuilt += 1;
        }
    }
    // Push the freshly-rendered thumbnails to the storage backend.
    snap.store_thumbnails(&ids).await;
    ("success", rebuilt, format!("rebuilt {rebuilt} thumbnail(s)"))
}

/// Re-cluster every owner's faces into People (no detection — uses existing face
/// embeddings) and persist the rebuilt clusters.
async fn body_recluster_faces(snap: &mut AppState) -> (&'static str, i64, String) {
    let mut owners: Vec<String> = snap.faces.values().map(|f| f.owner_id.clone()).collect();
    owners.sort();
    owners.dedup();
    let mut people = 0i64;
    for owner in &owners {
        snap.cluster_faces(owner);
        snap.persist_faces(owner).await;
        people += snap.people.values().filter(|p| &p.owner_id == owner).count() as i64;
    }
    ("success", people, format!("re-clustered {} owner(s) into {people} people", owners.len()))
}

/// Detect faces on live photos that have NONE yet — a catch-up for photos whose
/// detection failed/was skipped under heavy import load (the sidecar serializes
/// detection, so big batches can drop some). Skips RAW + non-live photos, then
/// re-clusters. Idempotent: photos that genuinely have no faces just stay empty.
async fn body_redetect_faces(snap: &mut AppState) -> (&'static str, i64, String) {
    use std::collections::HashSet;
    let have: HashSet<String> = snap.faces.values().map(|f| f.photo_id.clone()).collect();
    let todo: Vec<String> = snap
        .photos
        .values()
        .filter(|p| p.kind != "raw" && p.deleted_at.is_none() && !have.contains(&p.id))
        .map(|p| p.id.clone())
        .collect();
    let n = todo.len();
    if n == 0 {
        return ("success", 0, "no photos missing faces".to_string());
    }
    snap.detect_faces(None, &todo).await;
    let mut owners: Vec<String> = snap.faces.values().map(|f| f.owner_id.clone()).collect();
    owners.sort();
    owners.dedup();
    for owner in &owners {
        snap.cluster_faces(owner);
        snap.persist_faces(owner).await;
    }
    ("success", n as i64, format!("ran face detection on {n} photo(s) that had none"))
}

/// FULL face RESET: re-run the detector on EVERY live non-RAW photo (dropping &
/// replacing each photo's existing faces) and re-cluster every owner from scratch.
/// Unlike `redetect_faces` (which only touches photos with NO faces), this
/// recomputes the WHOLE library at the CURRENT detector threshold — the right
/// pass after tuning detection, so stale false positives (a chandelier read as a
/// face) disappear and previously-missed faces appear. People are rebuilt; a
/// user-assigned name survives wherever the new cluster's face-set overlaps the
/// old one. Clearing every photo owner (not just those with faces now) guarantees
/// an owner whose only "face" was a false positive ends up with zero people.
async fn body_reset_faces(snap: &mut AppState) -> (&'static str, i64, String) {
    let todo: Vec<String> = snap
        .photos
        .values()
        .filter(|p| p.kind != "raw" && p.deleted_at.is_none())
        .map(|p| p.id.clone())
        .collect();
    let n = todo.len();
    snap.detect_faces(None, &todo).await;
    // Re-cluster + persist EVERY photo owner: this rewrites their face/people rows
    // to the freshly-detected set, deleting anything no longer present (Postgres-
    // first replace), so a removed false positive is gone from the DB too.
    let mut owners: Vec<String> = snap.photos.values().map(|p| p.owner_id.clone()).collect();
    owners.sort();
    owners.dedup();
    let mut faces = 0i64;
    for owner in &owners {
        snap.cluster_faces(owner);
        snap.persist_faces(owner).await;
        faces += snap.faces.values().filter(|f| &f.owner_id == owner).count() as i64;
    }
    ("success", n as i64, format!("re-detected {n} photo(s) → {faces} face(s) after reset"))
}

/// Re-extract EXIF for every live photo from its stored original and persist the
/// refreshed immutable metadata (overrides are untouched).
async fn body_reextract_metadata(snap: &mut AppState) -> (&'static str, i64, String) {
    use crate::extract::MetadataExtractor as _;
    let entries: Vec<(String, String)> = snap
        .photos
        .values()
        .filter(|p| p.deleted_at.is_none())
        .map(|p| (p.id.clone(), p.filename.clone()))
        .collect();
    let mut updated = 0i64;
    for (id, filename) in &entries {
        let Some((bytes, _ct)) = snap.load_original(id).await else { continue };
        let exif = crate::extract::ExifExtractor.extract(&bytes, filename);
        if let Some(p) = snap.photos.get_mut(id) {
            p.exif = exif;
        }
        snap.persist_photo(id).await;
        updated += 1;
    }
    ("success", updated, format!("re-extracted {updated} photo(s)"))
}

// ---- graphile_worker task definitions ----

#[derive(Serialize, Deserialize)]
pub struct PurgeTrash {}
impl TaskHandler for PurgeTrash {
    const IDENTIFIER: &'static str = "purge_trash";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_purge(&shared(&ctx)).await;
        Ok::<(), String>(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct S3Backup {}
impl TaskHandler for S3Backup {
    const IDENTIFIER: &'static str = "s3_backup";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_backup(&shared(&ctx)).await;
        Ok::<(), String>(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct AiAnalysis {}
impl TaskHandler for AiAnalysis {
    const IDENTIFIER: &'static str = "ai_analysis";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_ai_analysis(&shared(&ctx)).await;
        Ok::<(), String>(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct DetectDuplicates {}
impl TaskHandler for DetectDuplicates {
    const IDENTIFIER: &'static str = "detect_duplicates";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_duplicates(&shared(&ctx)).await;
        Ok::<(), String>(())
    }
}

/// Generic durable task for SCHEDULED PLUGIN jobs. One cron entry per scheduled
/// plugin job enqueues this with `{ "job": "<id>" }`; the worker claims it once
/// across the cluster (so a scheduled plugin job runs on exactly ONE instance,
/// unlike the per-instance interval fallback). Routes through `run_named` so it
/// lands in the JobRun history like every other job.
#[derive(Serialize, Deserialize)]
pub struct PluginJob {
    pub job: String,
}
impl TaskHandler for PluginJob {
    const IDENTIFIER: &'static str = "plugin_job";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_named(&shared(&ctx), &self.job, "cron").await;
        Ok::<(), String>(())
    }
}

/// Durable PER-PHOTO face detection, enqueued right after a single-file upload
/// (`handlers::upload_file`). Runs the sidecar detector on exactly ONE freshly
/// imported photo, then re-clusters that owner's People. Durable (survives a
/// restart) + retried on failure, unlike the previous fire-and-forget
/// `tokio::spawn`. A no-op when ML is disabled or the photo vanished. NOT recorded
/// in the JobRun history — it's a high-frequency per-upload task, not a sweep.
#[derive(Serialize, Deserialize)]
pub struct DetectFaces {
    pub photo_id: String,
    pub owner_id: String,
}
impl TaskHandler for DetectFaces {
    const IDENTIFIER: &'static str = "detect_faces";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        detect_faces_for(&shared(&ctx), &self.photo_id, &self.owner_id).await;
        Ok::<(), String>(())
    }
}

/// Detect faces on a single photo + re-cluster its owner. Shared by the durable
/// [`DetectFaces`] task and the inline fallback (when there's no worker queue).
/// Loads a fresh DB snapshot, runs detection, and persists the rebuilt clusters.
pub async fn detect_faces_for(st: &Shared, photo_id: &str, owner_id: &str) {
    let Some(mut snap) = crate::handlers::request_snapshot(st).await else { return };
    if snap.ml.is_none() {
        return;
    }
    snap.detect_faces(None, std::slice::from_ref(&photo_id.to_string())).await;
    snap.cluster_faces(owner_id);
    snap.persist_faces(owner_id).await;
}

/// Generic durable runner for an ON-DEMAND named job triggered from the admin
/// console. Heavy maintenance passes (e.g. `reset_faces`, which re-detects the
/// whole library) must not run inline in the HTTP handler — they'd block the
/// request and get cancelled by a client timeout. `handlers::run_job` enqueues
/// this instead; the worker claims it once across the cluster and records the
/// real [`JobRun`] in history via `run_named` when it finishes.
#[derive(Serialize, Deserialize)]
pub struct MaintenanceJob {
    pub job: String,
}
impl TaskHandler for MaintenanceJob {
    const IDENTIFIER: &'static str = "maintenance_job";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_named(&shared(&ctx), &self.job, "manual").await;
        Ok::<(), String>(())
    }
}

/// Convert a plugin job's `schedule_secs` into a 5-field crontab expression.
/// graphile cron is minute-granular, so sub-minute schedules are rounded up to a
/// minute. Intervals that don't map cleanly to `*/N` are coarsened to the nearest
/// representable cadence (logged by the caller).
fn schedule_to_crontab(secs: u32) -> String {
    let mins = (secs / 60).max(1);
    if mins <= 59 {
        format!("*/{mins} * * * *")
    } else if mins % 60 == 0 && (mins / 60) <= 23 {
        format!("0 */{} * * *", mins / 60)
    } else if mins % (60 * 24) == 0 {
        "0 0 * * *".to_string() // daily
    } else {
        "0 * * * *".to_string() // coarsen to hourly
    }
}

// ---- Bootstrap + enqueue ----

type DynErr = Box<dyn std::error::Error + Send + Sync>;

/// Build + start the worker (sharing the app's Postgres pool), register every
/// task + its cron schedule, spawn its run loop, and return [`WorkerUtils`] for
/// enqueuing ad-hoc jobs (e.g. import enrichment). Cron schedules are claimed
/// once across the cluster, so background work runs exactly once regardless of
/// how many instances are up.
pub async fn start_worker(state: Shared, database_url: &str) -> Result<WorkerUtils, DynErr> {
    // Scheduled plugin jobs become durable cron entries (claimed once per cluster).
    let plugin_crons: Vec<(String, u32)> = match { state.read().await.plugins.clone() } {
        Some(host) => host.scheduled_jobs().await,
        None => vec![],
    };

    let mut opts = WorkerOptions::default()
        // Use the URL (graphile_worker is on sqlx 0.9; the app is on 0.8, so we
        // can't hand it our pool) — it opens its own small pool to the same DB.
        .database_url(database_url)
        .schema(SCHEMA)
        .concurrency(4)
        .add_extension(AppExt(state))
        .define_job::<PurgeTrash>()
        .define_job::<S3Backup>()
        .define_job::<AiAnalysis>()
        .define_job::<DetectDuplicates>()
        .define_job::<DetectFaces>()
        .define_job::<MaintenanceJob>()
        .define_job::<PluginJob>()
        // Set an explicit (empty) payload on each cron so jobs are enqueued with a
        // JSON `{}` body, not SQL NULL. The task structs are empty, but serde can't
        // deserialize `null` INTO a struct — without this the cron jobs fail with
        // "invalid type: null, expected struct …" and never run.
        .with_cron(Cron::hourly_at::<PurgeTrash>(0)?.payload(PurgeTrash {})?)
        .with_cron(Cron::hourly_at::<S3Backup>(30)?.payload(S3Backup {})?)
        .with_cron(Cron::every_n_minutes::<AiAnalysis>(5)?.payload(AiAnalysis {})?)
        .with_cron(Cron::daily_at::<DetectDuplicates>(3, 0)?.payload(DetectDuplicates {})?);

    // One cron line per scheduled plugin job: a distinct `?id=` disambiguates the
    // shared `plugin_job` task, and the JSON payload carries the job id.
    for (job, secs) in plugin_crons {
        let expr = schedule_to_crontab(secs);
        let line = format!("{expr} plugin_job ?id=plugin_{job} {{\"job\":\"{job}\"}}");
        opts = opts.with_cron(line.as_str())?;
        tracing::info!("plugins: durable cron for {job}: '{expr}'");
    }

    let worker = opts.init().await?;
    let utils = worker.create_utils();
    tokio::spawn(async move {
        if let Err(e) = worker.run().await {
            tracing::error!("graphile_worker run loop stopped: {e}");
        }
    });
    Ok(utils)
}

#[cfg(test)]
mod tests {
    use super::schedule_to_crontab;

    #[test]
    fn schedule_to_crontab_maps_common_intervals() {
        assert_eq!(schedule_to_crontab(60), "*/1 * * * *"); // 1 min
        assert_eq!(schedule_to_crontab(300), "*/5 * * * *"); // 5 min
        assert_eq!(schedule_to_crontab(30), "*/1 * * * *"); // sub-minute rounds up
        assert_eq!(schedule_to_crontab(3600), "0 */1 * * *"); // hourly
        assert_eq!(schedule_to_crontab(6 * 3600), "0 */6 * * *"); // every 6h
        assert_eq!(schedule_to_crontab(24 * 3600), "0 0 * * *"); // daily
    }
}
