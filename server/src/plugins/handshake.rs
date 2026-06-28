//! Launch a plugin binary, perform the go-plugin handshake, and dial its gRPC
//! Unix socket.
//!
//! The host spawns the child with the magic cookie + a unique socket path in its
//! environment, reads exactly ONE handshake line from the child's stdout (with a
//! timeout), validates it, then connects the tonic clients over the advertised
//! Unix socket. All remaining child output is drained into `tracing`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use photon_plugin_proto::pb;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tonic::transport::{Channel, Endpoint};

/// How long we wait for a plugin to print its handshake line before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A launched-and-connected plugin: the child process (kept alive,
/// `kill_on_drop`) plus a channel-backed tonic [`Channel`] over its Unix socket.
pub struct LaunchedPlugin {
    pub child: Child,
    pub channel: Channel,
    pub socket_path: PathBuf,
}

/// Launch `binary`, handshake, and connect. Returns `Err(reason)` on any failure
/// (bad cookie env, no handshake within the timeout, malformed line, dial error);
/// the caller logs it and skips this plugin — it never panics.
///
/// `api` is injected into the child's environment (API base URL + bearer token)
/// so the plugin can call back into the Photon HTTP API as a service account.
pub async fn launch(binary: &Path, api: &super::PluginApi) -> Result<LaunchedPlugin, String> {
    // Unique socket path under the OS temp dir, keyed on pid + a nanosecond stamp.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let socket_path = std::env::temp_dir().join(format!("photon-plugin-{}-{stamp}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);

    let mut child = Command::new(binary)
        .env(photon_plugin_proto::MAGIC_COOKIE_KEY, photon_plugin_proto::MAGIC_COOKIE_VALUE)
        .env(photon_plugin_proto::SOCKET_ENV, &socket_path)
        .env(photon_plugin_proto::API_URL_ENV, &api.base_url)
        .env(photon_plugin_proto::API_TOKEN_ENV, &api.token)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", binary.display()))?;

    let stdout = child.stdout.take().ok_or_else(|| "child stdout missing".to_string())?;
    let stderr = child.stderr.take();
    let mut lines = BufReader::new(stdout).lines();

    // Read exactly one handshake line, bounded by a timeout.
    let first = match tokio::time::timeout(HANDSHAKE_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(l))) => l,
        Ok(Ok(None)) => return Err("plugin exited before handshake".to_string()),
        Ok(Err(e)) => return Err(format!("reading handshake: {e}")),
        Err(_) => return Err("handshake timed out".to_string()),
    };

    let advertised = photon_plugin_proto::parse_handshake(&first)
        .ok_or_else(|| format!("bad handshake line: {first:?}"))?;

    // Drain the rest of the plugin's stdout/stderr into tracing (don't block on
    // it). Tag with the binary's file name (not the full temp path) for readable
    // `plugin=…` log fields.
    let who = binary
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.display().to_string());
    drain_to_tracing(lines, who.clone());
    if let Some(err) = stderr {
        drain_stderr_to_tracing(err, who);
    }

    let channel = connect_uds(&advertised).await.map_err(|e| format!("dial {advertised}: {e}"))?;

    Ok(LaunchedPlugin { child, channel, socket_path })
}

/// Connect a tonic [`Channel`] to a Unix socket. tonic 0.12 has no native
/// `unix://` scheme, so we hand it a dummy authority + a UDS connector
/// (`connect_with_connector` + `service_fn`).
pub async fn connect_uds(path: &str) -> Result<Channel, tonic::transport::Error> {
    let path = path.to_string();
    Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(tower::service_fn(move |_| {
            let p = path.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(p).await?,
                ))
            }
        }))
        .await
}

/// Fetch the plugin's `Info` over a connected channel.
pub async fn fetch_info(channel: Channel) -> Result<pb::PluginInfo, String> {
    let mut client = pb::plugin_client::PluginClient::new(channel);
    let resp = client
        .info(pb::InfoRequest {})
        .await
        .map_err(|e| format!("Info rpc failed: {e}"))?;
    Ok(resp.into_inner())
}

/// Probe a plugin's liveness over a connected channel. `Ok(true)` only when the
/// `Health` RPC returns `ok = true`.
pub async fn fetch_health(channel: Channel) -> Result<bool, String> {
    let mut client = pb::plugin_client::PluginClient::new(channel);
    let resp = client
        .health(pb::HealthRequest {})
        .await
        .map_err(|e| format!("Health rpc failed: {e}"))?;
    Ok(resp.into_inner().ok)
}

/// List the jobs a plugin declares over a connected channel.
pub async fn fetch_jobs(channel: Channel) -> Result<Vec<pb::JobDecl>, String> {
    let mut client = pb::job_client::JobClient::new(channel);
    let resp = client
        .list_jobs(pb::ListJobsRequest {})
        .await
        .map_err(|e| format!("ListJobs rpc failed: {e}"))?;
    Ok(resp.into_inner().jobs)
}

/// List the routes a plugin declares over a connected channel.
pub async fn fetch_routes(channel: Channel) -> Result<Vec<pb::RouteDecl>, String> {
    let mut client = pb::route_client::RouteClient::new(channel);
    let resp = client
        .list_routes(pb::ListRoutesRequest {})
        .await
        .map_err(|e| format!("ListRoutes rpc failed: {e}"))?;
    Ok(resp.into_inner().routes)
}

fn drain_to_tracing<R>(mut lines: tokio::io::Lines<BufReader<R>>, who: String)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(plugin = %who, "{line}");
        }
    });
}

/// Drain a plugin's STDERR into the host's native tracing logger.
///
/// The SDK installs a JSON tracing subscriber on the plugin side that writes one
/// JSON object per line to stderr (`{"timestamp":..,"level":"INFO","fields":
/// {"message":".."},"target":".."}`). We parse each line, pull out the level +
/// message (+ target), and RE-EMIT it through the server's own tracing at the
/// matching level, tagged with `plugin=<who>`. Lines that aren't the SDK's JSON
/// (a panic backtrace, a raw `eprintln!`, third-party C library noise) fall back
/// to `warn!` verbatim so nothing is ever lost.
fn drain_stderr_to_tracing(stderr: tokio::process::ChildStderr, who: String) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            re_emit_plugin_log(&who, &line);
        }
    });
}

/// Parse one stderr line as an SDK JSON log record and forward it to the host
/// logger at the matching level; fall back to `warn!` for anything unparseable.
fn re_emit_plugin_log(who: &str, line: &str) {
    let trimmed = line.trim();
    if let Some((level, message, target)) = parse_plugin_log(trimmed) {
        // The plugin's own target (module path) is forwarded as a field so it is
        // visible without polluting the host's static target.
        match level {
            tracing::Level::ERROR => {
                tracing::error!(plugin = %who, plugin_target = %target, "{message}")
            }
            tracing::Level::WARN => {
                tracing::warn!(plugin = %who, plugin_target = %target, "{message}")
            }
            tracing::Level::INFO => {
                tracing::info!(plugin = %who, plugin_target = %target, "{message}")
            }
            tracing::Level::DEBUG => {
                tracing::debug!(plugin = %who, plugin_target = %target, "{message}")
            }
            tracing::Level::TRACE => {
                tracing::trace!(plugin = %who, plugin_target = %target, "{message}")
            }
        }
    } else if !trimmed.is_empty() {
        tracing::warn!(plugin = %who, "{trimmed}");
    }
}

/// Extract `(level, message, target)` from one SDK JSON log line. Returns `None`
/// for non-JSON lines or JSON missing the expected shape.
fn parse_plugin_log(line: &str) -> Option<(tracing::Level, String, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    let level = match obj.get("level")?.as_str()? {
        "ERROR" => tracing::Level::ERROR,
        "WARN" => tracing::Level::WARN,
        "INFO" => tracing::Level::INFO,
        "DEBUG" => tracing::Level::DEBUG,
        "TRACE" => tracing::Level::TRACE,
        _ => return None,
    };
    // tracing-subscriber's json format nests the event message under
    // `fields.message`; fall back to a top-level `message` just in case.
    let message = obj
        .get("fields")
        .and_then(|f| f.get("message"))
        .or_else(|| obj.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let target = obj.get("target").and_then(|t| t.as_str()).unwrap_or("").to_string();
    Some((level, message, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tracing_json_line() {
        // The exact shape `tracing_subscriber::fmt().json()` emits.
        let line = r#"{"timestamp":"2026-06-25T10:00:00Z","level":"INFO","fields":{"message":"hello plugin"},"target":"example_hello_job"}"#;
        let (level, message, target) = parse_plugin_log(line).expect("should parse");
        assert_eq!(level, tracing::Level::INFO);
        assert_eq!(message, "hello plugin");
        assert_eq!(target, "example_hello_job");
    }

    #[test]
    fn maps_each_level() {
        for (s, want) in [
            ("ERROR", tracing::Level::ERROR),
            ("WARN", tracing::Level::WARN),
            ("INFO", tracing::Level::INFO),
            ("DEBUG", tracing::Level::DEBUG),
            ("TRACE", tracing::Level::TRACE),
        ] {
            let line = format!(r#"{{"level":"{s}","fields":{{"message":"m"}}}}"#);
            let (level, _, _) = parse_plugin_log(&line).expect("parse");
            assert_eq!(level, want);
        }
    }

    #[test]
    fn non_json_line_is_not_parsed() {
        // A raw panic line or eprintln! → None (caller falls back to warn!).
        assert!(parse_plugin_log("thread 'main' panicked at ...").is_none());
        assert!(parse_plugin_log("").is_none());
        // Valid JSON but unknown level → None.
        assert!(parse_plugin_log(r#"{"level":"WAT","fields":{"message":"m"}}"#).is_none());
    }
}
