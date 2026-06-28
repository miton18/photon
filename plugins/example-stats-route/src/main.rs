use std::collections::HashMap;

use photon_plugin_sdk::*;

struct Stats;

/// Build a response with a single `Content-Type` header.
fn with_content_type(status: u16, content_type: &str, body: Vec<u8>) -> PluginHttpResponse {
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), content_type.to_string());
    PluginHttpResponse { status, headers, body }
}

#[async_trait]
impl RoutePlugin for Stats {
    fn routes(&self) -> Vec<RouteDecl> {
        vec![
            RouteDecl::new("GET", "/summary"),
            RouteDecl::new("GET", "/ping"),
        ]
    }

    async fn handle(&self, req: PluginHttpRequest) -> Result<PluginHttpResponse, PluginError> {
        // Surfaces in the SERVER log, tagged `plugin=…`, at INFO level.
        tracing::info!(method = %req.method, path = %req.path, actor = %req.actor, "stats plugin: handling request");
        match req.path.as_str() {
            "/ping" => Ok(with_content_type(200, "text/plain", b"pong".to_vec())),
            "/summary" => {
                let body = serde_json::json!({
                    "plugin": "stats",
                    "path": req.path,
                    "method": req.method,
                    "actor": req.actor,
                    "is_admin": req.is_admin,
                    "uptime_hint": "served by the stats route plugin",
                });
                Ok(with_content_type(
                    200,
                    "application/json",
                    serde_json::to_vec(&body).unwrap_or_default(),
                ))
            }
            _ => {
                let body = serde_json::json!({ "error": "not found" });
                Ok(with_content_type(
                    404,
                    "application/json",
                    serde_json::to_vec(&body).unwrap_or_default(),
                ))
            }
        }
    }
}

#[tokio::main]
async fn main() {
    serve(route(
        PluginMeta::new("stats", "Stats Routes", env!("CARGO_PKG_VERSION")),
        Stats,
    ))
    .await
}
