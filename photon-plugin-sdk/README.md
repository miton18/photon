# photon-plugin-sdk

Write Photon plugins as **standalone binaries** that the server launches beside
itself and talks to over gRPC on a Unix socket (HashiCorp `go-plugin` model).
You implement **one trait**; the SDK hides the handshake, the socket, the gRPC
server, logging, and the API client.

Three plugin types:

| Type       | Trait          | What it adds                                         |
| ---------- | -------------- | ---------------------------------------------------- |
| **Job**    | `JobPlugin`    | New background jobs (run from the admin console/cron)|
| **Route**  | `RoutePlugin`  | Complementary HTTP endpoints under `/api/plugins/{id}/…` |
| **Editor** | `EditorPlugin` | Photo-editing operations (bytes in → bytes out)      |

The whole feature is **off by default**: the server only launches plugins when
`PHOTON_PLUGINS_DIR` points at a directory of plugin binaries (mirrors the ML
sidecar's `PHOTON_ML_URL` gating). A crashing plugin never takes the server down.

## Quick start — a Job plugin

`Cargo.toml`:

```toml
[dependencies]
photon-plugin-sdk = { path = "../../photon-plugin-sdk" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
serde_json = "1"   # only if you call the Photon API
```

`src/main.rs`:

```rust
use photon_plugin_sdk::*;

struct Hello;

#[async_trait]
impl JobPlugin for Hello {
    fn jobs(&self) -> Vec<JobDecl> {
        // (id, display name, description)
        vec![JobDecl::new("hello_sweep", "Hello Sweep", "A demo job")]
    }

    async fn run(&self, id: &str, _trigger: &str, report: &Reporter) -> JobOutcome {
        // `tracing` logs flow back into the SERVER's log, tagged with this plugin.
        tracing::info!(job = id, "starting");

        // Declare named steps; the admin console renders them as staged progress.
        report.steps(["Scan", "Process", "Finalize"]);
        report.start(0);
        report.done(0);
        report.start(1);
        for pct in [25, 50, 75, 100] {
            report.percent(1, pct);
        }
        report.done(1);
        report.start(2);
        report.done(2);

        Ok(format!("ran {id}"))            // Err(JobError::new("…")) on failure
    }
}

#[tokio::main]
async fn main() {
    serve(job(PluginMeta::new("hello", "Hello Job", env!("CARGO_PKG_VERSION")), Hello)).await
}
```

Build it and drop the binary in your plugins dir:

```bash
cargo build --release                       # in your plugin's crate dir
mkdir -p /opt/photon/plugins && cp target/release/my-plugin /opt/photon/plugins/
(cd path/to/photon/server && PHOTON_PLUGINS_DIR=/opt/photon/plugins cargo run)
# log: plugins: discovered Hello Job [hello] v0.1.0 (capabilities: job; jobs: [hello_sweep]; …)
```

Trigger it from the admin console, or `POST /api/admin/jobs/hello_sweep/run`.

## Identity: `id` vs `name`

`PluginMeta` and `JobDecl` both carry a **stable `id`** (used to invoke things —
the registry key, the job identifier, the route prefix) and a human **`name`**
(display only). `PluginMeta::new(id, name, version)`,
`JobDecl::new(id, name, description)`.

## Reporting progress (jobs)

`run` receives a `&Reporter`. It's cheap, non-blocking, and entirely optional —
a job that never calls it just has no step breakdown.

```rust
report.steps(["Download", "Index"]);  // declare ordered steps (all pending)
report.start(0);                       // step 0 → running 0% (earlier steps auto-done)
report.percent(0, 40);                 // step 0 → running 40%
report.done(0);                        // step 0 → done 100%
report.fail(1);                        // step 1 → failed (keeps partial progress)
```

Snapshots stream to the host and surface live under `GET /api/admin/stats`
(`jobs[].progress = { steps: [{name, state, percent}], current }`).

## Scheduling jobs

A job can run automatically on an interval, in addition to manual/admin runs.
Declare it with the `every_secs` builder; the host runs it with `trigger="cron"`,
recording each run in the job history like any other. With Postgres configured the
schedule becomes a **durable cron** claimed once across the cluster (multi-instance
safe); without a DB it falls back to a per-instance interval.

```rust
fn jobs(&self) -> Vec<JobDecl> {
    vec![JobDecl::new("nightly_sweep", "Nightly Sweep", "…").every_secs(24 * 3600)]
}
```

## Logging

Just use `tracing` (re-exported as `photon_plugin_sdk::tracing`, so you don't
add it to your own `Cargo.toml`):

```rust
tracing::info!(count = 12, "processed batch");
tracing::warn!("nothing to do");
```

`serve()` installs a JSON subscriber writing to **stderr** (stdout is reserved
for the handshake). The host parses those lines and re-emits them through the
**server's own logger** at the matching level, tagged `plugin=<binary-name>`.
Tune verbosity with `RUST_LOG` (defaults to `info`).

## Calling back into Photon — `PhotonClient`

At launch the host injects the API base URL + a bearer token for a trusted
service account into the plugin's environment. `PhotonClient::from_env()` reads
them (returns `None` when run outside the host, so you can degrade gracefully):

```rust
if let Some(api) = PhotonClient::from_env() {
    let me: serde_json::Value = api.get_json("/api/me").await?;        // who am I
    let _: serde_json::Value = api.post_json("/api/albums", &body).await?;
    api.patch_json::<_, serde_json::Value>("/api/photos/ph_1", &json_patch_ops).await?;  // RFC-6902
    api.delete("/api/albums/al_1").await?;
}
```

Every request carries the bearer token automatically. For requests the typed
helpers don't cover, reach the raw client via `api.http()` + `api.url(path)` +
`api.token()`. Errors are an `ApiError` (`Transport` / `Status{status,body}` /
`Decode`). **Each plugin gets its OWN admin service account** (`u_plugin_<binary>`)
and token, so calls are attributable per plugin and a single plugin's token can be
revoked independently. Operator-installed plugins are trusted (they already run
with the server's privileges).

## Route plugin

```rust
use std::collections::HashMap;
use photon_plugin_sdk::*;

struct Stats;

#[async_trait]
impl RoutePlugin for Stats {
    fn routes(&self) -> Vec<RouteDecl> {
        vec![RouteDecl::new("GET", "/ping")]      // for introspection; handling works regardless
    }
    async fn handle(&self, req: PluginHttpRequest) -> Result<PluginHttpResponse, PluginError> {
        // req has: method, path (after /api/plugins/{id}), query, headers, body,
        // actor (authenticated user id), is_admin.
        let mut headers = HashMap::new();
        headers.insert("content-type".into(), "text/plain".into());
        Ok(PluginHttpResponse { status: 200, headers, body: b"pong".to_vec() })
    }
}

#[tokio::main]
async fn main() {
    serve(route(PluginMeta::new("stats", "Stats Routes", env!("CARGO_PKG_VERSION")), Stats)).await
}
```

Reachable at `/api/plugins/stats/ping` (any signed-in user; the host fills
`actor`/`is_admin`). Unknown plugin → 404, plugin error/timeout → 502.

## Editor plugin

```rust
#[async_trait]
impl EditorPlugin for Watermark {
    fn ops(&self) -> Vec<EditorOp> {
        vec![EditorOp::new("watermark", "Watermark", "overlay text")]
    }
    async fn apply(
        &self,
        op_id: &str,
        image: Vec<u8>,
        content_type: &str,
        params: &std::collections::HashMap<String, String>,
    ) -> Result<EditedImage, PluginError> {
        // … transform bytes …
        Ok(EditedImage::new(image, content_type))
    }
}
// serve(editor(PluginMeta::new("watermark", "Watermark", "0.1.0"), Watermark)).await
```

## How it runs (under the hood)

`serve(plugin)` checks the magic-cookie env (refuses to run if not launched by
the host), installs the stderr JSON logger, binds the host-chosen Unix socket,
prints the one-line handshake to stdout, then serves the base `Plugin` service
(Info/Health) plus your capability service over gRPC until SIGTERM. The host
discovers binaries in `PHOTON_PLUGINS_DIR`, handshakes, registers each plugin's
jobs/routes/ops, and keeps the child alive (killed on shutdown). A **lifecycle
supervisor** health-checks each plugin periodically and **relaunches** any that
crashed or exited, so a flaky plugin self-heals without restarting the server.

See the complete, building examples:
- `plugins/example-hello-job` — job + progress + scheduling + logging + `PhotonClient`
- `plugins/example-stats-route` — routes + logging
- `plugins/example-watermark-editor` — editor ops (grayscale + watermark) with the `image` crate
