use photon_plugin_sdk::*;

struct Hello;

#[async_trait]
impl JobPlugin for Hello {
    fn jobs(&self) -> Vec<JobDecl> {
        // Demonstrates scheduling: the host runs this automatically every 6h
        // (trigger="cron"), in addition to manual admin-console runs.
        vec![
            JobDecl::new("hello_sweep", "Hello Sweep", "A demo job from the example plugin")
                .every_secs(6 * 3600),
        ]
    }
    async fn run(&self, id: &str, trigger: &str, report: &Reporter) -> JobOutcome {
        // Logs go to stderr as JSON and surface in the SERVER log, tagged with
        // this plugin — proving the plugin→host log bridge end to end.
        tracing::info!(job = id, trigger, "hello plugin: starting sweep");
        if id != "hello_sweep" {
            tracing::warn!(job = id, "hello plugin: unknown job id");
            return Err(JobError::new(format!("unknown job {id}")));
        }

        // Declare three named steps, then drive them — this is what the admin
        // console renders as staged progress.
        report.steps(["Scan", "Process", "Finalize"]);
        report.start(0);
        // Best-effort callback into the Photon API as the service account: prove
        // the injected endpoint+token work. Never fail the job on an API error.
        if let Some(api) = PhotonClient::from_env() {
            match api.get_json::<serde_json::Value>("/api/me").await {
                Ok(me) => tracing::info!(actor = ?me.get("id"), "hello plugin: called /api/me"),
                Err(e) => tracing::warn!("hello plugin: /api/me failed: {e}"),
            }
        }
        report.done(0);
        report.start(1);
        for pct in [25, 50, 75, 100] {
            report.percent(1, pct);
        }
        report.done(1);
        report.start(2);
        report.done(2);

        tracing::info!(job = id, "hello plugin: sweep done");
        Ok(format!("ran {id} from the plugin"))
    }
}

#[tokio::main]
async fn main() {
    serve(job(PluginMeta::new("hello", "Hello Job", env!("CARGO_PKG_VERSION")), Hello)).await
}
