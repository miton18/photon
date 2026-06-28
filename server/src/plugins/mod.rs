//! HOST side of the subprocess plugin system (go-plugin style).
//!
//! OFFLINE-FIRST, exactly like the ML sidecar (`ml::MlClient::from_env`): the
//! whole feature is gated on `PHOTON_PLUGINS_DIR`. When it is unset (the default
//! for demos and the entire test suite) [`PluginHost::from_env`] returns `None`,
//! no child process is ever launched, and behavior is identical to before. When
//! set, the host scans the directory for executables, launches + handshakes +
//! connects each over a gRPC Unix socket, and registers the jobs it declares.
//!
//! ROBUSTNESS: every host→plugin call is wrapped in a `tokio::time::timeout` and
//! maps any transport/timeout/rpc error to a graceful "failed" outcome (never a
//! panic, never a fatal 5xx). A crashed plugin degrades the next call; the server
//! stays up.

mod handshake;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use photon_plugin_proto::pb;
use tokio::process::Child;
use tokio::sync::RwLock;
use tonic::transport::Channel;

/// Env var pointing at a directory of plugin binaries. Unset ⇒ feature inert.
pub const PLUGINS_DIR_ENV: &str = "PHOTON_PLUGINS_DIR";

/// How often the lifecycle supervisor health-checks each plugin.
const HEALTH_INTERVAL: Duration = Duration::from_secs(15);
/// Per-call timeout for the lightweight `Health` RPC (short — it does no work).
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on the exponential restart backoff for a crash-looping plugin.
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(600);

/// Backoff before the next restart attempt after `fails` consecutive failures:
/// `15s · 2^(fails-1)`, capped at [`MAX_RESTART_BACKOFF`]. So 1st retry waits 15s,
/// then 30s, 60s, … up to 10 min — a flaky plugin self-heals without thrashing.
fn restart_backoff(fails: u32) -> Duration {
    let shift = fails.saturating_sub(1).min(6); // cap the shift so we never overflow
    Duration::from_secs(15u64.saturating_mul(1u64 << shift)).min(MAX_RESTART_BACKOFF)
}

/// The per-plugin API identity: a base URL + bearer token (injected as env vars
/// at launch). Each plugin gets its OWN service account + token so it acts as an
/// auditable, individually-revocable identity (`u_plugin_<binary>`). The plugin's
/// SDK `PhotonClient::from_env` reads these to call back into the Photon HTTP API.
#[derive(Clone)]
pub struct PluginApi {
    pub base_url: String,
    pub token: String,
    /// The service-account user id this token authenticates as (for revocation).
    pub user_id: String,
}

/// Sanitize a binary file name into a stable service-account suffix
/// (`[a-z0-9_]`), so the identity is `u_plugin_<binary>`.
fn identity_from_path(path: &std::path::Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    if cleaned.is_empty() { "unknown".to_string() } else { cleaned }
}

/// An admin service account a plugin authenticates as. No password (cannot log in
/// interactively) — reachable ONLY via its injected session token. Operator-
/// installed plugins are trusted (same OS privileges as the server).
fn service_user(user_id: &str, identity: &str) -> crate::models::User {
    crate::models::User {
        id: user_id.to_string(),
        name: format!("Plugin: {identity}"),
        email: format!("{identity}.plugin@photon.local"),
        avatar_url: String::new(),
        password_hash: None,
        salt: String::new(),
        pepper: String::new(),
        is_admin: true,
        disabled: false,
        quota_mb: Some(0),
        partners: vec![],
        totp_secret: None,
    }
}

/// Provision a per-plugin service account + fresh callback token, persisting both
/// to Postgres so the normal auth middleware honors the token. When there's no DB
/// (tests/offline) the token is still returned but won't authenticate — harmless,
/// since plugins are disabled without `PHOTON_PLUGINS_DIR`.
async fn provision_api(
    pool: Option<&crate::db::Persistence>,
    base_url: &str,
    identity: &str,
) -> PluginApi {
    let user_id = format!("u_plugin_{identity}");
    let token = crate::state::random_hex(32);
    if let Some(p) = pool {
        let user = service_user(&user_id, identity);
        if let Err(e) = p.upsert_user(&user).await {
            tracing::warn!("plugins: provisioning service user {user_id} failed: {e}");
        }
        if let Err(e) = p.upsert_session(&token, &user_id, &crate::state::now_rfc3339()).await {
            tracing::warn!("plugins: minting token for {user_id} failed: {e}");
        }
    }
    PluginApi { base_url: base_url.to_string(), token, user_id }
}

/// Per-call timeout for plugin RPCs (jobs can do real work, so be generous —
/// matches the ML sidecar's 30s).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// One registered, running plugin: its tonic channel (clone-cheap, channel-backed)
/// plus the child process kept alive via `kill_on_drop`.
struct PluginConn {
    id: String,
    channel: Channel,
    /// Kept so the child is killed when the host drops (`kill_on_drop(true)`).
    #[allow(dead_code)]
    child: Child,
    #[allow(dead_code)]
    socket_path: PathBuf,
    /// The binary this plugin was launched from — kept so the supervisor can
    /// relaunch it after a crash.
    binary: PathBuf,
    /// The per-plugin API identity (re-injected verbatim on restart).
    api: PluginApi,
    /// Capability codes the plugin advertised (`pb::Capability` as i32).
    #[allow(dead_code)]
    capabilities: Vec<i32>,
    /// True if the plugin advertised `Capability::Route` (serves proxied HTTP).
    route_capable: bool,
    /// True if the plugin advertised `Capability::Editor` (photo-edit ops).
    editor_capable: bool,
    /// Human-friendly display name (from `Info.name`), for the UI tools list.
    label: String,
    /// Declared routes as `"METHOD path"` (for introspection / finding a UI entry).
    routes: Vec<String>,
}

/// A route plugin surfaced to the UI's tools list: its id, label, and the GET
/// route to open as its UI (preferring `/ui`), if it serves one.
#[derive(serde::Serialize, Clone)]
pub struct RoutePluginInfo {
    pub id: String,
    pub label: String,
    pub ui_path: Option<String>,
}

/// The host registry. Owns its OWN `RwLock` so a slow plugin call never holds the
/// big `AppState` lock across an `.await`.
pub struct PluginHost {
    /// name → connection.
    plugins: RwLock<HashMap<String, PluginConn>>,
    /// job name → owning plugin name.
    job_owner: RwLock<HashMap<String, String>>,
    /// job name → schedule interval in seconds (only jobs with `schedule_secs > 0`).
    job_schedules: RwLock<HashMap<String, u32>>,
    /// DB handle for restart-time re-provisioning (None in tests/offline).
    pool: Option<crate::db::Persistence>,
}

impl PluginHost {
    /// Build the host from `PHOTON_PLUGINS_DIR`. Returns `None` when the var is
    /// unset/empty (feature disabled — no process launched). Otherwise scans the
    /// dir, launches every executable, and registers the ones that handshake.
    pub async fn from_env(state: &crate::state::AppState, base_url: String) -> Option<Arc<PluginHost>> {
        let dir = std::env::var(PLUGINS_DIR_ENV).ok().filter(|s| !s.is_empty())?;
        let pool = state.persistence.clone();
        let host = Arc::new(Self::scan_dir(&dir, pool, base_url).await);
        // Start the lifecycle supervisor: health-checks each plugin and relaunches
        // any that have crashed/exited.
        host.clone().spawn_supervisor();
        Some(host)
    }

    /// Scan `dir` for executable plugin binaries, provisioning a per-plugin token,
    /// then launching + handshaking + registering each (injecting its API identity
    /// into the child's environment). Failures are logged and the plugin skipped.
    /// Factored out of [`from_env`] so integration tests can point at a specific
    /// directory without mutating the process-global env.
    async fn scan_dir(dir: &str, pool: Option<crate::db::Persistence>, base_url: String) -> PluginHost {
        let host = PluginHost {
            plugins: RwLock::new(HashMap::new()),
            job_owner: RwLock::new(HashMap::new()),
            job_schedules: RwLock::new(HashMap::new()),
            pool: pool.clone(),
        };

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("plugins: cannot read {PLUGINS_DIR_ENV}={dir}: {e}");
                return host;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !is_executable_file(&path) {
                continue;
            }
            let identity = identity_from_path(&path);
            let api = provision_api(pool.as_ref(), &base_url, &identity).await;
            match handshake::launch(&path, &api).await {
                Ok(launched) => {
                    host.register(launched, path, api).await;
                }
                Err(e) => {
                    tracing::warn!("plugins: failed to launch {}: {e}", path.display());
                }
            }
        }

        host
    }

    /// Register a launched plugin: fetch `Info`, then (if it's a Job plugin) its
    /// declared jobs, and index them. Failures are logged and the plugin skipped.
    /// `binary`/`api` are stored so the supervisor can relaunch it after a crash.
    async fn register(&self, launched: handshake::LaunchedPlugin, binary: PathBuf, api: PluginApi) {
        let handshake::LaunchedPlugin { child, channel, socket_path } = launched;

        let info = match handshake::fetch_info(channel.clone()).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("plugins: Info failed for {}: {e}", socket_path.display());
                return;
            }
        };

        if info.protocol_version != photon_plugin_proto::PROTOCOL_VERSION {
            tracing::warn!(
                "plugins: {} declares protocol_version {} != host {}; skipping",
                info.id,
                info.protocol_version,
                photon_plugin_proto::PROTOCOL_VERSION
            );
            return;
        }

        let caps: Vec<&'static str> = info
            .capabilities
            .iter()
            .map(|c| match pb::Capability::try_from(*c) {
                Ok(pb::Capability::Editor) => "editor",
                Ok(pb::Capability::Route) => "route",
                Ok(pb::Capability::Job) => "job",
                _ => "unknown",
            })
            .collect();

        // For Job plugins, register every declared job → this plugin.
        let mut job_names: Vec<String> = vec![];
        if info.capabilities.contains(&(pb::Capability::Job as i32)) {
            match handshake::fetch_jobs(channel.clone()).await {
                Ok(jobs) => {
                    let mut owner = self.job_owner.write().await;
                    let mut sched = self.job_schedules.write().await;
                    for j in jobs {
                        owner.insert(j.id.clone(), info.id.clone());
                        if j.schedule_secs > 0 {
                            sched.insert(j.id.clone(), j.schedule_secs);
                        }
                        job_names.push(j.id);
                    }
                }
                Err(e) => tracing::warn!("plugins: ListJobs failed for {}: {e}", info.id),
            }
        }

        // For Route plugins, mark route-capable and cache declared routes (for
        // logging/introspection only — proxying works regardless).
        let route_capable = info.capabilities.contains(&(pb::Capability::Route as i32));
        let editor_capable = info.capabilities.contains(&(pb::Capability::Editor as i32));
        let mut route_decls: Vec<String> = vec![];
        if route_capable {
            match handshake::fetch_routes(channel.clone()).await {
                Ok(routes) => {
                    for r in routes {
                        route_decls.push(format!("{} {}", r.method, r.path));
                    }
                }
                Err(e) => tracing::warn!("plugins: ListRoutes failed for {}: {e}", info.id),
            }
        }

        tracing::info!(
            "plugins: discovered {} [{}] v{} (capabilities: {}; jobs: [{}]; routes: [{}])",
            info.name,
            info.id,
            info.version,
            caps.join(", "),
            job_names.join(", "),
            route_decls.join(", ")
        );

        self.plugins.write().await.insert(
            info.id.clone(),
            PluginConn {
                id: info.id.clone(),
                channel,
                child,
                socket_path,
                binary,
                api,
                label: info.name.clone(),
                routes: route_decls,
                capabilities: info.capabilities,
                route_capable,
                editor_capable,
            },
        );
    }

    /// Remove a plugin from the registry (dropping its `Child` → killed) and drop
    /// any jobs it owned. Used before re-registering on restart.
    async fn deregister(&self, id: &str) {
        self.plugins.write().await.remove(id);
        let dropped: Vec<String> = {
            let mut owner = self.job_owner.write().await;
            let dropped = owner
                .iter()
                .filter(|(_, o)| o.as_str() == id)
                .map(|(j, _)| j.clone())
                .collect::<Vec<_>>();
            owner.retain(|_job, o| o != id);
            dropped
        };
        let mut sched = self.job_schedules.write().await;
        for j in dropped {
            sched.remove(&j);
        }
    }

    /// Every plugin job with a declared schedule, as `(job_id, interval_secs)`.
    /// The caller (main) spawns a periodic task per entry that runs the job via
    /// `run_named(.., "cron")`, reusing the normal JobRun recording.
    pub async fn scheduled_jobs(&self) -> Vec<(String, u32)> {
        self.job_schedules.read().await.iter().map(|(j, s)| (j.clone(), *s)).collect()
    }

    /// Spawn the lifecycle supervisor: every [`HEALTH_INTERVAL`], `Health`-check
    /// each registered plugin and relaunch any that has crashed/exited. A plugin
    /// that keeps failing is restarted with EXPONENTIAL BACKOFF (so a crash-looping
    /// plugin isn't relaunched every tick forever) — the delay doubles per
    /// consecutive failure up to [`MAX_RESTART_BACKOFF`], and resets the moment the
    /// plugin reports healthy again. Never panics.
    fn spawn_supervisor(self: Arc<Self>) {
        tokio::spawn(async move {
            // id → (consecutive failures, earliest next restart attempt).
            let mut backoff: HashMap<String, (u32, std::time::Instant)> = HashMap::new();
            let mut tick = tokio::time::interval(HEALTH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let now = std::time::Instant::now();
                // Snapshot (id, channel) so we don't hold the lock across the RPC.
                let probes: Vec<(String, Channel)> = {
                    self.plugins
                        .read()
                        .await
                        .values()
                        .map(|c| (c.id.clone(), c.channel.clone()))
                        .collect()
                };
                // Forget backoff for plugins no longer registered.
                backoff.retain(|id, _| probes.iter().any(|(pid, _)| pid == id));

                for (id, channel) in probes {
                    let healthy = matches!(
                        tokio::time::timeout(HEALTH_TIMEOUT, handshake::fetch_health(channel)).await,
                        Ok(Ok(true))
                    );
                    if healthy {
                        backoff.remove(&id); // recovered → reset the backoff clock
                        continue;
                    }
                    let entry = backoff.entry(id.clone()).or_insert((0, now));
                    if now < entry.1 {
                        continue; // still within the backoff window — don't thrash
                    }
                    entry.0 = entry.0.saturating_add(1);
                    let delay = restart_backoff(entry.0);
                    entry.1 = now + delay;
                    tracing::warn!(
                        "plugins: {id} failed health check; restarting (attempt {}, next retry in {}s if it fails again)",
                        entry.0,
                        delay.as_secs()
                    );
                    self.restart(&id).await;
                }
            }
        });
    }

    /// Relaunch a (presumed dead) plugin from its stored binary + API identity and
    /// re-register it. On a DB-backed host the token is re-minted so the restarted
    /// child gets a live session. Best-effort: a failure is logged, the old (dead)
    /// entry left in place to be retried next tick.
    async fn restart(&self, id: &str) {
        let (binary, mut api) = match self.plugins.read().await.get(id) {
            Some(c) => (c.binary.clone(), c.api.clone()),
            None => return,
        };
        // Re-mint the session token for this plugin's service account (the old one
        // stays valid too, but a fresh process gets a guaranteed-live token).
        if let Some(p) = &self.pool {
            let token = crate::state::random_hex(32);
            if p.upsert_session(&token, &api.user_id, &crate::state::now_rfc3339()).await.is_ok() {
                api.token = token;
            }
        }
        match handshake::launch(&binary, &api).await {
            Ok(launched) => {
                self.deregister(id).await;
                self.register(launched, binary, api).await;
                tracing::info!("plugins: restarted {id}");
            }
            Err(e) => tracing::warn!("plugins: restart of {id} failed: {e}"),
        }
    }

    /// Names of all registered plugins (for a startup log / introspection).
    pub async fn plugin_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.plugins.read().await.values().map(|p| p.id.clone()).collect();
        names.sort();
        names
    }

    /// Does any registered plugin own a job called `name`?
    pub async fn has_job(&self, name: &str) -> bool {
        self.job_owner.read().await.contains_key(name)
    }

    /// Run the plugin job `name` (resolving its owning plugin) and map the result
    /// to `(outcome, items, result)` exactly like the built-in job bodies. The
    /// RPC is server-streaming: each `JobProgress` snapshot is handed to
    /// `on_progress` (so the caller can surface live progress) and the terminal
    /// `JobResult` becomes the return triple. Any transport/timeout/rpc error
    /// degrades gracefully to a "failed" outcome so the server stays up. The
    /// per-message timeout means a job that goes silent for [`CALL_TIMEOUT`] is
    /// treated as hung, but a job that keeps reporting may run arbitrarily long.
    /// Returns `None` only if no plugin owns `name`.
    pub async fn run_job<F>(
        &self,
        name: &str,
        trigger: &str,
        mut on_progress: F,
    ) -> Option<(&'static str, i64, String)>
    where
        F: FnMut(crate::state::JobProgress) + Send,
    {
        let owner = { self.job_owner.read().await.get(name).cloned()? };
        let channel = {
            let plugins = self.plugins.read().await;
            match plugins.get(&owner) {
                Some(p) => p.channel.clone(),
                None => {
                    return Some(("failed", 0, format!("plugin {owner} not connected")));
                }
            }
        };

        let mut client = pb::job_client::JobClient::new(channel);
        let req = pb::RunJobRequest { name: name.to_string(), trigger: trigger.to_string() };
        let mut stream = match tokio::time::timeout(CALL_TIMEOUT, client.run_job(req)).await {
            Ok(Ok(resp)) => resp.into_inner(),
            Ok(Err(status)) => {
                tracing::warn!("plugins: RunJob {name} on {owner} failed: {status}");
                return Some(("failed", 0, format!("plugin error: {}", status.message())));
            }
            Err(_) => {
                tracing::warn!("plugins: RunJob {name} on {owner} timed out");
                return Some(("failed", 0, "plugin timed out".to_string()));
            }
        };

        let mut final_result: Option<(&'static str, i64, String)> = None;
        loop {
            match tokio::time::timeout(CALL_TIMEOUT, stream.message()).await {
                Ok(Ok(Some(update))) => match update.update {
                    Some(pb::run_job_update::Update::Progress(p)) => on_progress(convert_progress(p)),
                    Some(pb::run_job_update::Update::Result(r)) => {
                        let outcome = if r.success { "success" } else { "failed" };
                        final_result = Some((outcome, r.items, r.result));
                    }
                    None => {}
                },
                Ok(Ok(None)) => break, // stream closed
                Ok(Err(status)) => {
                    tracing::warn!("plugins: RunJob {name} on {owner} stream failed: {status}");
                    return Some(("failed", 0, format!("plugin error: {}", status.message())));
                }
                Err(_) => {
                    tracing::warn!("plugins: RunJob {name} on {owner} stalled");
                    return Some(("failed", 0, "plugin timed out".to_string()));
                }
            }
        }
        Some(final_result.unwrap_or(("failed", 0, "plugin ended without a result".to_string())))
    }

    /// Route-capable plugins, for the UI tools list. Each carries its id, label and
    /// the GET route to open as its UI (preferring `/ui`, else the first GET route).
    pub async fn route_plugins(&self) -> Vec<RoutePluginInfo> {
        let g = self.plugins.read().await;
        let mut out: Vec<RoutePluginInfo> = g
            .values()
            .filter(|c| c.route_capable)
            .map(|c| {
                let gets: Vec<&str> = c.routes.iter().filter_map(|r| r.strip_prefix("GET ")).collect();
                let ui_path = gets
                    .iter()
                    .find(|p| **p == "/ui")
                    .or_else(|| gets.first())
                    .map(|s| s.to_string());
                RoutePluginInfo { id: c.id.clone(), label: c.label.clone(), ui_path }
            })
            .collect();
        out.sort_by(|a, b| a.label.cmp(&b.label));
        out
    }

    /// Is a plugin with id `name` connected AND route-capable (advertised
    /// `Capability::Route`)? Used by the catch-all proxy to 404 unknown/non-route
    /// plugins before attempting an RPC.
    pub async fn has_route(&self, name: &str) -> bool {
        self.plugins
            .read()
            .await
            .get(name)
            .map(|p| p.route_capable)
            .unwrap_or(false)
    }

    /// Proxy an HTTP request to the route plugin `name` over gRPC, wrapped in a
    /// timeout. Returns `None` (after a `warn!`) on any transport/timeout/rpc
    /// error or if the plugin isn't connected/route-capable — the caller maps
    /// that to a 502. Mirrors [`run_job`]'s degradation discipline (never panics).
    pub async fn route_handle(
        &self,
        name: &str,
        req: pb::HttpRequest,
    ) -> Option<pb::HttpResponse> {
        let channel = {
            let plugins = self.plugins.read().await;
            match plugins.get(name) {
                Some(p) if p.route_capable => p.channel.clone(),
                _ => return None,
            }
        };

        let mut client = pb::route_client::RouteClient::new(channel);
        match tokio::time::timeout(CALL_TIMEOUT, client.handle(req)).await {
            Ok(Ok(resp)) => Some(resp.into_inner()),
            Ok(Err(status)) => {
                tracing::warn!("plugins: Route.Handle on {name} failed: {status}");
                None
            }
            Err(_) => {
                tracing::warn!("plugins: Route.Handle on {name} timed out");
                None
            }
        }
    }

    /// The combined catalog of editor operations across all Editor-capable
    /// plugins, each tagged with its owning plugin id. Used by
    /// `GET /api/plugins/editor/ops` to populate the editor UI. Degrades to
    /// whatever plugins answer (a failing plugin is skipped, never fatal).
    pub async fn editor_ops(&self) -> Vec<EditorOpInfo> {
        // Snapshot (id, channel) of editor-capable plugins without holding the lock.
        let editors: Vec<(String, Channel)> = {
            self.plugins
                .read()
                .await
                .values()
                .filter(|p| p.editor_capable)
                .map(|p| (p.id.clone(), p.channel.clone()))
                .collect()
        };
        let mut out = vec![];
        for (id, channel) in editors {
            let mut client = pb::editor_client::EditorClient::new(channel);
            match tokio::time::timeout(CALL_TIMEOUT, client.list_ops(pb::ListOpsRequest {})).await {
                Ok(Ok(resp)) => {
                    for op in resp.into_inner().ops {
                        out.push(EditorOpInfo {
                            plugin: id.clone(),
                            id: op.id,
                            label: op.label,
                            description: op.description,
                            params: op
                                .params
                                .into_iter()
                                .map(|p| EditorParamInfo {
                                    name: p.name,
                                    label: p.label,
                                    default: p.default,
                                })
                                .collect(),
                        });
                    }
                }
                Ok(Err(status)) => tracing::warn!("plugins: ListOps on {id} failed: {status}"),
                Err(_) => tracing::warn!("plugins: ListOps on {id} timed out"),
            }
        }
        out
    }

    /// Apply editor `op_id` of `plugin` to `image` bytes. Returns the edited
    /// `(bytes, content_type)`, or `None` (after a `warn!`) on any
    /// transport/timeout/rpc error or unknown/non-editor plugin — the caller maps
    /// that to a 502. Never panics (mirrors [`route_handle`]).
    pub async fn editor_apply(
        &self,
        plugin: &str,
        op_id: &str,
        image: Vec<u8>,
        content_type: &str,
        params: HashMap<String, String>,
    ) -> Option<(Vec<u8>, String)> {
        let channel = {
            let plugins = self.plugins.read().await;
            match plugins.get(plugin) {
                Some(p) if p.editor_capable => p.channel.clone(),
                _ => return None,
            }
        };
        let mut client = pb::editor_client::EditorClient::new(channel);
        let req = pb::ApplyOpRequest {
            op_id: op_id.to_string(),
            image,
            content_type: content_type.to_string(),
            params,
        };
        match tokio::time::timeout(CALL_TIMEOUT, client.apply_op(req)).await {
            Ok(Ok(resp)) => {
                let r = resp.into_inner();
                Some((r.image, r.content_type))
            }
            Ok(Err(status)) => {
                tracing::warn!("plugins: Editor.ApplyOp {op_id} on {plugin} failed: {status}");
                None
            }
            Err(_) => {
                tracing::warn!("plugins: Editor.ApplyOp {op_id} on {plugin} timed out");
                None
            }
        }
    }
}

/// One editor operation in the cross-plugin catalog (`GET /api/plugins/editor/ops`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditorOpInfo {
    pub plugin: String,
    pub id: String,
    pub label: String,
    pub description: String,
    pub params: Vec<EditorParamInfo>,
}

/// A declared parameter of an editor op (UI hint).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditorParamInfo {
    pub name: String,
    pub label: String,
    pub default: String,
}

/// Convert a wire `pb::JobProgress` into the host-native [`crate::state::JobProgress`]
/// surfaced in AdminStats.
fn convert_progress(p: pb::JobProgress) -> crate::state::JobProgress {
    let steps = p
        .steps
        .into_iter()
        .map(|s| crate::state::JobStepStat {
            state: match pb::job_step::State::try_from(s.state) {
                Ok(pb::job_step::State::Pending) => "pending",
                Ok(pb::job_step::State::Running) => "running",
                Ok(pb::job_step::State::Done) => "done",
                Ok(pb::job_step::State::Failed) => "failed",
                _ => "pending",
            }
            .to_string(),
            name: s.name,
            percent: s.percent,
        })
        .collect();
    crate::state::JobProgress { steps, current: p.current }
}

/// Is `path` a regular file with an executable bit set? (Unix; the host only runs
/// on Linux/macOS.)
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(test)]
impl PluginHost {
    /// Build an EMPTY host (no plugins registered) for unit tests of the
    /// name-resolution + degradation logic.
    fn empty() -> Self {
        PluginHost {
            plugins: RwLock::new(HashMap::new()),
            job_owner: RwLock::new(HashMap::new()),
            job_schedules: RwLock::new(HashMap::new()),
            pool: None,
        }
    }

    /// Register a job → plugin-name mapping WITHOUT a real connection, to test the
    /// "owned job but plugin not connected" degradation path.
    async fn register_orphan_job(&self, job: &str, owner: &str) {
        self.job_owner.write().await.insert(job.to_string(), owner.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn has_job_false_when_empty() {
        let host = PluginHost::empty();
        assert!(!host.has_job("hello_sweep").await);
        // Unknown job → None (not owned by any plugin), so run_named falls through
        // to "unknown job".
        assert!(host.run_job("hello_sweep", "manual", |_| {}).await.is_none());
    }

    #[tokio::test]
    async fn run_job_degrades_when_plugin_disconnected() {
        let host = PluginHost::empty();
        // Job is owned, but no live PluginConn exists → graceful "failed", not panic.
        host.register_orphan_job("hello_sweep", "hello").await;
        assert!(host.has_job("hello_sweep").await);
        let (outcome, items, msg) = host.run_job("hello_sweep", "manual", |_| {}).await.unwrap();
        assert_eq!(outcome, "failed");
        assert_eq!(items, 0);
        assert!(msg.contains("not connected"));
    }

    #[test]
    fn restart_backoff_grows_then_caps() {
        assert_eq!(restart_backoff(1), Duration::from_secs(15));
        assert_eq!(restart_backoff(2), Duration::from_secs(30));
        assert_eq!(restart_backoff(3), Duration::from_secs(60));
        // Caps at MAX_RESTART_BACKOFF (10 min), never overflows.
        assert_eq!(restart_backoff(7), MAX_RESTART_BACKOFF);
        assert_eq!(restart_backoff(100), MAX_RESTART_BACKOFF);
    }

    #[tokio::test]
    async fn has_route_and_route_handle_false_when_empty() {
        let host = PluginHost::empty();
        // Unknown / not route-capable plugin → has_route false …
        assert!(!host.has_route("stats").await);
        // … and route_handle degrades to None (caller maps to 502), never panics.
        let req = pb::HttpRequest {
            method: "GET".into(),
            path: "/ping".into(),
            query: String::new(),
            headers: Default::default(),
            body: vec![],
            actor: "alice".into(),
            is_admin: false,
        };
        assert!(host.route_handle("stats", req).await.is_none());
    }

    /// END-TO-END (offline, no DB): launch the real `example-hello-job` binary,
    /// handshake, stream a RunJob, and assert the terminal outcome + that the
    /// reporter's multi-step progress arrived. Skips cleanly if the example
    /// binary hasn't been built yet (so it never fails CI that didn't build it).
    #[tokio::test]
    async fn e2e_hello_job_streams_progress() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../plugins/example-hello-job/target/debug/example-hello-job");
        if !bin.exists() {
            eprintln!("skip e2e_hello_job_streams_progress: {} not built", bin.display());
            return;
        }
        // Scan a temp dir containing ONLY the plugin binary (a real target/debug
        // dir is full of other executables).
        let tmp = std::env::temp_dir().join(format!("photon-plugin-e2e-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let dst = tmp.join("example-hello-job");
        std::fs::copy(&bin, &dst).expect("copy plugin binary");

        // Dummy API context: the plugin's best-effort callback to this URL just
        // fails fast (nothing is listening) and must not fail the job.
        let host = PluginHost::scan_dir(tmp.to_str().unwrap(), None, "http://127.0.0.1:1".to_string()).await;
        assert!(host.has_job("hello_sweep").await, "plugin should own hello_sweep");

        let mut snaps: Vec<crate::state::JobProgress> = vec![];
        let (outcome, _items, msg) = host
            .run_job("hello_sweep", "manual", |p| snaps.push(p))
            .await
            .expect("hello_sweep is owned");

        assert_eq!(outcome, "success");
        assert!(msg.contains("hello_sweep"), "result message: {msg}");

        // The job declared a 6h schedule → it shows up in scheduled_jobs().
        let scheduled = host.scheduled_jobs().await;
        assert!(
            scheduled.iter().any(|(j, s)| j == "hello_sweep" && *s == 6 * 3600),
            "scheduled_jobs: {scheduled:?}"
        );

        // The reporter declared 3 steps and finished them all.
        let last = snaps.last().expect("at least one progress snapshot");
        assert_eq!(last.steps.len(), 3, "three declared steps");
        assert!(
            last.steps.iter().all(|s| s.state == "done"),
            "all steps done at the end: {:?}",
            last.steps
        );

        // Lifecycle: restart() relaunches the binary and re-registers it — the job
        // is still owned afterwards and the fresh process is healthy (this is the
        // recovery path the supervisor drives on a failed health check).
        host.restart("hello").await;
        assert!(host.has_job("hello_sweep").await, "job re-registered after restart");
        let healthy = {
            let ch = host.plugins.read().await.get("hello").map(|c| c.channel.clone());
            match ch {
                Some(ch) => matches!(handshake::fetch_health(ch).await, Ok(true)),
                None => false,
            }
        };
        assert!(healthy, "restarted plugin is healthy");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// END-TO-END (offline, no DB): launch the real `example-watermark-editor`,
    /// list its ops, and apply one to a tiny in-memory PNG — proving the host↔
    /// Editor-plugin path (ListOps + ApplyOp). Skips if the binary isn't built.
    #[tokio::test]
    async fn e2e_watermark_editor_applies() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../plugins/example-watermark-editor/target/debug/example-watermark-editor");
        if !bin.exists() {
            eprintln!("skip e2e_watermark_editor_applies: {} not built", bin.display());
            return;
        }
        let tmp = std::env::temp_dir().join(format!("photon-editor-e2e-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::copy(&bin, tmp.join("example-watermark-editor")).expect("copy editor binary");

        let host = PluginHost::scan_dir(tmp.to_str().unwrap(), None, "http://127.0.0.1:1".to_string()).await;

        // Catalog: both declared ops are present, tagged with the plugin id.
        let ops = host.editor_ops().await;
        assert!(ops.iter().any(|o| o.plugin == "watermark" && o.id == "grayscale"), "ops: {ops:?}");
        assert!(ops.iter().any(|o| o.id == "watermark"), "ops: {ops:?}");

        // Encode a tiny PNG, apply `grayscale`, and confirm the result decodes.
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 100, 50, 255])))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let (out, ct) = host
            .editor_apply("watermark", "grayscale", png.into_inner(), "image/png", HashMap::new())
            .await
            .expect("editor_apply succeeds");
        assert_eq!(ct, "image/png");
        assert!(image::load_from_memory(&out).is_ok(), "edited bytes decode as an image");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
