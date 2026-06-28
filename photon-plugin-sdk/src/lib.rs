//! Photon plugin SDK — author-facing API for writing subprocess plugins
//! (`go-plugin` style).
//!
//! DX is the whole point: a plugin author implements ONE trait
//! ([`JobPlugin`] / [`EditorPlugin`] / [`RoutePlugin`]), wraps it with the
//! matching constructor ([`job`] / [`editor`] / [`route`]), and calls
//! [`serve`]. Everything below — the magic-cookie handshake, binding the Unix
//! socket, printing the handshake line, running the tonic gRPC server, and
//! converting between the ergonomic types here and the prost wire types — is
//! hidden.
//!
//! ```no_run
//! use photon_plugin_sdk::*;
//!
//! struct Hello;
//! #[async_trait]
//! impl JobPlugin for Hello {
//!     fn jobs(&self) -> Vec<JobDecl> { vec![JobDecl::new("hello_sweep", "Hello Sweep", "demo")] }
//!     async fn run(&self, id: &str, _trigger: &str, report: &Reporter) -> JobOutcome {
//!         report.steps(["work"]);
//!         report.done(0);
//!         Ok(format!("ran {id}"))
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     serve(job(PluginMeta::new("hello", "Hello Job", "0.1.0"), Hello)).await
//! }
//! ```

pub use async_trait::async_trait;
/// Re-exported so plugin authors log with `use photon_plugin_sdk::tracing;` (or
/// just `tracing::info!`) against the exact version [`serve`] wires up — no need
/// to add `tracing` to their own `Cargo.toml`.
pub use tracing;

mod api;
pub use api::{ApiError, PhotonClient};

use photon_plugin_proto::pb;
use std::sync::Arc;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{transport::Server, Request, Response, Status};

// ---- Author-facing ergonomic types ---------------------------------------

/// Plugin identity, advertised to the host during `Info`: a stable `id`, a human
/// `name`, and a `version`.
#[derive(Clone, Debug)]
pub struct PluginMeta {
    /// Stable identifier (used as the registry key, route prefix, etc.).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    pub version: String,
}

impl PluginMeta {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
        }
    }
}

/// An error surfaced from a plugin trait method; mapped to a gRPC error on the
/// wire and to a "failed" outcome host-side.
#[derive(Debug, Clone)]
pub struct PluginError(pub String);

impl PluginError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PluginError {}

impl From<String> for PluginError {
    fn from(s: String) -> Self {
        PluginError(s)
    }
}

impl From<&str> for PluginError {
    fn from(s: &str) -> Self {
        PluginError(s.to_string())
    }
}

// ---- Job ------------------------------------------------------------------

/// One background job the plugin declares (mirrors the host's built-in jobs).
/// Like [`PluginMeta`], a job has a stable `id` (what you invoke) and a
/// human-readable `name` (what the admin console shows).
#[derive(Clone, Debug)]
pub struct JobDecl {
    /// Stable identifier, e.g. `"hello_sweep"` — the value passed to [`JobPlugin::run`].
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    pub description: String,
    /// Run automatically every N seconds (`0` = manual/admin-triggered only). The
    /// host schedules these with `trigger = "cron"`, recording each run like any job.
    pub schedule_secs: u32,
}

impl JobDecl {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            schedule_secs: 0,
        }
    }

    /// Schedule this job to run automatically every `secs` seconds (builder).
    pub fn every_secs(mut self, secs: u32) -> Self {
        self.schedule_secs = secs;
        self
    }
}

/// A job failure with an explanatory message; mapped to a "failed" `JobRun`
/// host-side. Construct from any string via `.into()` or [`JobError::new`].
#[derive(Debug, Clone)]
pub struct JobError(pub String);

impl JobError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JobError {}

impl From<String> for JobError {
    fn from(s: String) -> Self {
        JobError(s)
    }
}

impl From<&str> for JobError {
    fn from(s: &str) -> Self {
        JobError(s.to_string())
    }
}

/// What a job run returns: `Ok(success_message)` on success, `Err(JobError)` on
/// failure. The `Ok` string is the human-facing result recorded on the `JobRun`.
pub type JobOutcome = Result<String, JobError>;

/// Live progress reporter handed to [`JobPlugin::run`]. Declare the named steps
/// once with [`Reporter::steps`], then drive them by index: [`start`] a step,
/// push [`percent`] updates, mark it [`done`]. Every call streams a fresh
/// snapshot to the host, which surfaces it in the admin console (idle until the
/// job calls `steps`). All methods are cheap and non-blocking; reporting is
/// entirely optional — a job that never calls these just has no step breakdown.
///
/// [`start`]: Reporter::start
/// [`percent`]: Reporter::percent
/// [`done`]: Reporter::done
#[derive(Clone)]
pub struct Reporter {
    inner: Arc<ReporterInner>,
}

struct ReporterInner {
    tx: tokio::sync::mpsc::UnboundedSender<Result<pb::RunJobUpdate, Status>>,
    steps: std::sync::Mutex<Vec<pb::JobStep>>,
}

impl Reporter {
    fn new(tx: tokio::sync::mpsc::UnboundedSender<Result<pb::RunJobUpdate, Status>>) -> Self {
        Self { inner: Arc::new(ReporterInner { tx, steps: std::sync::Mutex::new(vec![]) }) }
    }

    /// Declare the ordered steps of this job, all initially pending.
    pub fn steps<I, S>(&self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let steps: Vec<pb::JobStep> = names
            .into_iter()
            .map(|n| pb::JobStep {
                name: n.into(),
                state: pb::job_step::State::Pending as i32,
                percent: 0,
            })
            .collect();
        *self.inner.steps.lock().unwrap() = steps;
        self.emit(0);
    }

    /// Begin step `index`: it goes RUNNING at 0%, and any earlier still-pending
    /// step is auto-marked DONE (so "I'm on step 3" implies 1–2 finished).
    pub fn start(&self, index: usize) {
        {
            let mut steps = self.inner.steps.lock().unwrap();
            for (i, s) in steps.iter_mut().enumerate() {
                if i < index && s.state == pb::job_step::State::Pending as i32 {
                    s.state = pb::job_step::State::Done as i32;
                    s.percent = 100;
                }
            }
            if let Some(s) = steps.get_mut(index) {
                s.state = pb::job_step::State::Running as i32;
                s.percent = 0;
            }
        }
        self.emit(index);
    }

    /// Update the completion percentage (0..=100) of the currently running step.
    pub fn percent(&self, index: usize, percent: u32) {
        {
            let mut steps = self.inner.steps.lock().unwrap();
            if let Some(s) = steps.get_mut(index) {
                s.state = pb::job_step::State::Running as i32;
                s.percent = percent.min(100);
            }
        }
        self.emit(index);
    }

    /// Mark step `index` complete (DONE at 100%).
    pub fn done(&self, index: usize) {
        {
            let mut steps = self.inner.steps.lock().unwrap();
            if let Some(s) = steps.get_mut(index) {
                s.state = pb::job_step::State::Done as i32;
                s.percent = 100;
            }
        }
        self.emit(index);
    }

    /// Mark step `index` as failed (keeps the partial progress visible).
    pub fn fail(&self, index: usize) {
        {
            let mut steps = self.inner.steps.lock().unwrap();
            if let Some(s) = steps.get_mut(index) {
                s.state = pb::job_step::State::Failed as i32;
            }
        }
        self.emit(index);
    }

    /// Send a snapshot of all steps with `current` highlighted. Best-effort: if
    /// the host has hung up the stream the send is silently dropped.
    fn emit(&self, current: usize) {
        let steps = self.inner.steps.lock().unwrap().clone();
        let update = pb::RunJobUpdate {
            update: Some(pb::run_job_update::Update::Progress(pb::JobProgress {
                steps,
                current: current as u32,
            })),
        };
        let _ = self.inner.tx.send(Ok(update));
    }
}

#[async_trait]
pub trait JobPlugin: Send + Sync + 'static {
    /// Declare the jobs this plugin provides (shown in the admin console).
    fn jobs(&self) -> Vec<JobDecl>;
    /// Run the job with identifier `id` (one of the declared jobs); `trigger` is
    /// `"cron"` or `"manual"`. Report multi-step progress through `report` (see
    /// [`Reporter`]); it is optional. Return `Ok(message)` or `Err(JobError)`.
    async fn run(&self, id: &str, trigger: &str, report: &Reporter) -> JobOutcome;
}

// ---- Editor ---------------------------------------------------------------

/// A single editor-operation parameter declaration (UI hint for the host).
#[derive(Clone, Debug, Default)]
pub struct OpParam {
    pub name: String,
    pub label: String,
    pub default: String,
}

impl OpParam {
    pub fn new(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self { name: name.into(), label: label.into(), default: String::new() }
    }
}

/// An editor operation the plugin exposes (bytes in → bytes out).
#[derive(Clone, Debug)]
pub struct EditorOp {
    pub id: String,
    pub label: String,
    pub description: String,
    pub params: Vec<OpParam>,
}

impl EditorOp {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self { id: id.into(), label: label.into(), description: description.into(), params: vec![] }
    }
}

/// The edited image returned by an editor operation.
#[derive(Clone, Debug)]
pub struct EditedImage {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

impl EditedImage {
    pub fn new(bytes: Vec<u8>, content_type: impl Into<String>) -> Self {
        Self { bytes, content_type: content_type.into() }
    }
}

#[async_trait]
pub trait EditorPlugin: Send + Sync + 'static {
    fn ops(&self) -> Vec<EditorOp>;
    async fn apply(
        &self,
        op_id: &str,
        image: Vec<u8>,
        content_type: &str,
        params: &std::collections::HashMap<String, String>,
    ) -> Result<EditedImage, PluginError>;
}

// ---- Route ----------------------------------------------------------------

/// A complementary route the plugin serves.
#[derive(Clone, Debug)]
pub struct RouteDecl {
    pub method: String,
    pub path: String,
}

impl RouteDecl {
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self { method: method.into(), path: path.into() }
    }
}

/// An inbound HTTP request proxied from the host (`/api/plugins/{name}/…`).
#[derive(Clone, Debug)]
pub struct PluginHttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
    pub actor: String,
    pub is_admin: bool,
}

/// The HTTP response the plugin returns for a proxied request.
#[derive(Clone, Debug)]
pub struct PluginHttpResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

impl PluginHttpResponse {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, headers: Default::default(), body: body.into() }
    }
}

#[async_trait]
pub trait RoutePlugin: Send + Sync + 'static {
    fn routes(&self) -> Vec<RouteDecl> {
        vec![]
    }
    async fn handle(
        &self,
        req: PluginHttpRequest,
    ) -> Result<PluginHttpResponse, PluginError>;
}

// ---- Plugin wrapper + constructors ----------------------------------------

/// Which capability an assembled [`Plugin`] carries.
enum Capability {
    Job(Arc<dyn JobPlugin>),
    Editor(Arc<dyn EditorPlugin>),
    Route(Arc<dyn RoutePlugin>),
}

/// An assembled plugin: identity + one capability implementation. Build with
/// [`job`] / [`editor`] / [`route`], then hand to [`serve`].
pub struct Plugin {
    meta: PluginMeta,
    cap: Capability,
}

/// Wrap a [`JobPlugin`] into a servable [`Plugin`].
pub fn job<P: JobPlugin>(meta: PluginMeta, p: P) -> Plugin {
    Plugin { meta, cap: Capability::Job(Arc::new(p)) }
}

/// Wrap an [`EditorPlugin`] into a servable [`Plugin`].
pub fn editor<P: EditorPlugin>(meta: PluginMeta, p: P) -> Plugin {
    Plugin { meta, cap: Capability::Editor(Arc::new(p)) }
}

/// Wrap a [`RoutePlugin`] into a servable [`Plugin`].
pub fn route<P: RoutePlugin>(meta: PluginMeta, p: P) -> Plugin {
    Plugin { meta, cap: Capability::Route(Arc::new(p)) }
}

impl Plugin {
    fn capability_code(&self) -> pb::Capability {
        match self.cap {
            Capability::Job(_) => pb::Capability::Job,
            Capability::Editor(_) => pb::Capability::Editor,
            Capability::Route(_) => pb::Capability::Route,
        }
    }

    fn info(&self) -> pb::PluginInfo {
        pb::PluginInfo {
            id: self.meta.id.clone(),
            name: self.meta.name.clone(),
            version: self.meta.version.clone(),
            protocol_version: photon_plugin_proto::PROTOCOL_VERSION,
            capabilities: vec![self.capability_code() as i32],
        }
    }
}

// ---- gRPC server adapters -------------------------------------------------

/// Base service every plugin exposes: `Info` (built from the meta) + `Health`.
struct BaseSvc {
    info: pb::PluginInfo,
}

#[async_trait]
impl pb::plugin_server::Plugin for BaseSvc {
    async fn info(
        &self,
        _request: Request<pb::InfoRequest>,
    ) -> Result<Response<pb::PluginInfo>, Status> {
        Ok(Response::new(self.info.clone()))
    }
    async fn health(
        &self,
        _request: Request<pb::HealthRequest>,
    ) -> Result<Response<pb::HealthStatus>, Status> {
        Ok(Response::new(pb::HealthStatus { ok: true, detail: String::new() }))
    }
}

/// Adapter turning the prost `Job` service into trait calls.
struct JobSvc {
    inner: Arc<dyn JobPlugin>,
}

#[async_trait]
impl pb::job_server::Job for JobSvc {
    async fn list_jobs(
        &self,
        _request: Request<pb::ListJobsRequest>,
    ) -> Result<Response<pb::ListJobsResponse>, Status> {
        let jobs = self.inner.jobs().into_iter().map(jobdecl_to_pb).collect();
        Ok(Response::new(pb::ListJobsResponse { jobs }))
    }

    type RunJobStream =
        tokio_stream::wrappers::UnboundedReceiverStream<Result<pb::RunJobUpdate, Status>>;

    /// Server-streaming: run the job on a background task, letting its [`Reporter`]
    /// push `JobProgress` updates down the stream, then send one terminal
    /// `JobResult`. The stream closes when the sender drops. Failures are reported
    /// IN-BAND (`success=false`), not as a gRPC error — the host records both the
    /// same way.
    async fn run_job(
        &self,
        request: Request<pb::RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<pb::RunJobUpdate, Status>>();
        let reporter = Reporter::new(tx.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = inner.run(&req.name, &req.trigger, &reporter).await;
            let result = outcome_to_result(outcome);
            let _ = tx.send(Ok(pb::RunJobUpdate {
                update: Some(pb::run_job_update::Update::Result(result)),
            }));
            // tx (and the reporter's clone, dropped with `reporter`) go out of
            // scope here → the receiver stream ends.
        });
        Ok(Response::new(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }
}

/// Adapter turning the prost `Editor` service into trait calls.
struct EditorSvc {
    inner: Arc<dyn EditorPlugin>,
}

#[async_trait]
impl pb::editor_server::Editor for EditorSvc {
    async fn list_ops(
        &self,
        _request: Request<pb::ListOpsRequest>,
    ) -> Result<Response<pb::ListOpsResponse>, Status> {
        let ops = self.inner.ops().into_iter().map(editorop_to_pb).collect();
        Ok(Response::new(pb::ListOpsResponse { ops }))
    }
    async fn apply_op(
        &self,
        request: Request<pb::ApplyOpRequest>,
    ) -> Result<Response<pb::ApplyOpResponse>, Status> {
        let req = request.into_inner();
        match self.inner.apply(&req.op_id, req.image, &req.content_type, &req.params).await {
            Ok(out) => Ok(Response::new(pb::ApplyOpResponse {
                image: out.bytes,
                content_type: out.content_type,
            })),
            Err(e) => Err(Status::internal(e.0)),
        }
    }
}

/// Adapter turning the prost `Route` service into trait calls.
struct RouteSvc {
    inner: Arc<dyn RoutePlugin>,
}

#[async_trait]
impl pb::route_server::Route for RouteSvc {
    async fn list_routes(
        &self,
        _request: Request<pb::ListRoutesRequest>,
    ) -> Result<Response<pb::ListRoutesResponse>, Status> {
        let routes = self
            .inner
            .routes()
            .into_iter()
            .map(|r| pb::RouteDecl { method: r.method, path: r.path })
            .collect();
        Ok(Response::new(pb::ListRoutesResponse { routes }))
    }
    async fn handle(
        &self,
        request: Request<pb::HttpRequest>,
    ) -> Result<Response<pb::HttpResponse>, Status> {
        let r = request.into_inner();
        let req = PluginHttpRequest {
            method: r.method,
            path: r.path,
            query: r.query,
            headers: r.headers,
            body: r.body,
            actor: r.actor,
            is_admin: r.is_admin,
        };
        match self.inner.handle(req).await {
            Ok(resp) => Ok(Response::new(pb::HttpResponse {
                status: resp.status as u32,
                headers: resp.headers,
                body: resp.body,
            })),
            Err(e) => Err(Status::internal(e.0)),
        }
    }
}

// ---- prost <-> ergonomic conversions --------------------------------------

fn jobdecl_to_pb(d: JobDecl) -> pb::JobDecl {
    pb::JobDecl {
        id: d.id,
        name: d.name,
        description: d.description,
        schedule_secs: d.schedule_secs,
    }
}

fn outcome_to_result(o: JobOutcome) -> pb::JobResult {
    // `items` is no longer part of the author API; a job that wants to convey a
    // count puts it in the success message. Always 0 on the wire for plugins.
    match o {
        Ok(msg) => pb::JobResult { success: true, items: 0, result: msg },
        Err(e) => pb::JobResult { success: false, items: 0, result: e.0 },
    }
}

fn editorop_to_pb(o: EditorOp) -> pb::EditorOp {
    pb::EditorOp {
        id: o.id,
        label: o.label,
        description: o.description,
        params: o
            .params
            .into_iter()
            .map(|p| pb::OpParam {
                name: p.name,
                label: p.label,
                kind: pb::op_param::Kind::String as i32,
                default: p.default,
                enum_values: vec![],
                min: None,
                max: None,
            })
            .collect(),
    }
}

// ---- serve() --------------------------------------------------------------

/// Run the plugin: perform the go-plugin handshake on stdout, bind the host's
/// Unix socket, and serve the base `Plugin` service plus the capability service
/// over gRPC until SIGTERM. Never returns.
pub async fn serve(plugin: Plugin) -> ! {
    // 1. Magic-cookie gate: refuse to run unless launched by the host.
    match std::env::var(photon_plugin_proto::MAGIC_COOKIE_KEY) {
        Ok(v) if v == photon_plugin_proto::MAGIC_COOKIE_VALUE => {}
        _ => {
            eprintln!(
                "this is a Photon plugin and is meant to be launched by the Photon host \
                 ({} not set correctly)",
                photon_plugin_proto::MAGIC_COOKIE_KEY
            );
            std::process::exit(2);
        }
    }

    // 1b. Install a JSON tracing subscriber writing to STDERR. stdout is reserved
    //     for the single handshake line, so all `tracing::*` from the author goes
    //     to stderr as one JSON object per line; the host parses those back into
    //     its own native logger at the matching level (see the host's stderr
    //     drain). `try_init` so a plugin that set up its own subscriber wins.
    let _ = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // 2. Socket path the host chose for us.
    let sock = match std::env::var(photon_plugin_proto::SOCKET_ENV) {
        Ok(s) if !s.is_empty() => s,
        _ => {
            eprintln!("{} not set; cannot bind", photon_plugin_proto::SOCKET_ENV);
            std::process::exit(2);
        }
    };
    // A stale socket file would make bind() fail; remove it best-effort.
    let _ = std::fs::remove_file(&sock);

    // 3. Bind the Unix socket.
    let listener = match tokio::net::UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {sock}: {e}");
            std::process::exit(2);
        }
    };

    let base = BaseSvc { info: plugin.info() };

    // 4. Print the handshake line and flush (then keep stdout open + running).
    {
        use std::io::Write as _;
        let line = photon_plugin_proto::handshake_line(&sock);
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    // 5. Build the tonic server: base service + the capability service.
    let mut builder = Server::builder().add_service(pb::plugin_server::PluginServer::new(base));
    builder = match plugin.cap {
        Capability::Job(inner) => {
            builder.add_service(pb::job_server::JobServer::new(JobSvc { inner }))
        }
        Capability::Editor(inner) => {
            builder.add_service(pb::editor_server::EditorServer::new(EditorSvc { inner }))
        }
        Capability::Route(inner) => {
            builder.add_service(pb::route_server::RouteServer::new(RouteSvc { inner }))
        }
    };

    let incoming = UnixListenerStream::new(listener);
    let shutdown = async {
        // Graceful stop on SIGTERM (host kills the child), or Ctrl-C for manual runs.
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };

    if let Err(e) = builder.serve_with_incoming_shutdown(incoming, shutdown).await {
        eprintln!("plugin server stopped with error: {e}");
        let _ = std::fs::remove_file(&sock);
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&sock);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobdecl_converts_to_pb() {
        let d = JobDecl::new("hello_sweep", "Hello Sweep", "demo job");
        let pb = jobdecl_to_pb(d);
        assert_eq!(pb.id, "hello_sweep");
        assert_eq!(pb.name, "Hello Sweep");
        assert_eq!(pb.description, "demo job");
    }

    #[test]
    fn outcome_success_and_failed_convert_to_result() {
        let ok = outcome_to_result(Ok("did 7".to_string()));
        assert!(ok.success);
        assert_eq!(ok.result, "did 7");

        let bad = outcome_to_result(Err(JobError::new("boom")));
        assert!(!bad.success);
        assert_eq!(bad.items, 0);
        assert_eq!(bad.result, "boom");
    }

    #[test]
    fn editorop_converts_to_pb_with_params() {
        let op = EditorOp {
            params: vec![OpParam::new("strength", "Strength")],
            ..EditorOp::new("blur", "Blur", "gaussian blur")
        };
        let pb = editorop_to_pb(op);
        assert_eq!(pb.id, "blur");
        assert_eq!(pb.params.len(), 1);
        assert_eq!(pb.params[0].name, "strength");
        assert_eq!(pb.params[0].kind, pb::op_param::Kind::String as i32);
    }

    #[test]
    fn plugin_info_carries_protocol_and_capability() {
        struct Dummy;
        #[async_trait]
        impl JobPlugin for Dummy {
            fn jobs(&self) -> Vec<JobDecl> {
                vec![]
            }
            async fn run(&self, _id: &str, _t: &str, _r: &Reporter) -> JobOutcome {
                Ok(String::new())
            }
        }
        let p = job(PluginMeta::new("hello", "Hello", "0.1.0"), Dummy);
        let info = p.info();
        assert_eq!(info.id, "hello");
        assert_eq!(info.name, "Hello");
        assert_eq!(info.protocol_version, photon_plugin_proto::PROTOCOL_VERSION);
        assert_eq!(info.capabilities, vec![pb::Capability::Job as i32]);
    }
}
