//! Immich → Photon import plugin (Web/Route plugin).
//!
//! A wizard that: (1) connects to a real Immich server (URL + API key), (2) lists
//! the Immich photos NOT already in Photon, (3) asks the user to confirm and to map
//! each Immich album to a Photon album, then (4) imports the chosen photos into the
//! user's Photon library — AS the calling user (the host forwards the user's bearer
//! token), so imported photos are owned by them and land in the mapped albums.
//!
//! INTENTIONAL Immich references: this is an interop/import tool for a real Immich,
//! authorized as an explicit exception to the product-wide "no Immich" rule.
//!
//! Immich API assumptions (recent Immich, ~v1.106+): API-key header `x-api-key`;
//! `GET /api/users/me`; `POST /api/search/metadata` (paginated `assets.items` +
//! `assets.nextPage`); `GET /api/albums` + `GET /api/albums/{id}`;
//! `GET /api/assets/{id}/original` for the original bytes.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use photon_plugin_sdk::*;
use serde::Deserialize;
use serde_json::json;

struct ImmichImport;

// ---------- helpers ----------

fn json_resp(status: u16, value: serde_json::Value) -> PluginHttpResponse {
    let mut headers = HashMap::new();
    headers.insert("content-type".into(), "application/json".into());
    PluginHttpResponse { status, headers, body: serde_json::to_vec(&value).unwrap_or_default() }
}
fn err(status: u16, msg: impl Into<String>) -> PluginHttpResponse {
    json_resp(status, json!({ "error": msg.into() }))
}
fn html_resp(body: &str) -> PluginHttpResponse {
    let mut headers = HashMap::new();
    headers.insert("content-type".into(), "text/html; charset=utf-8".into());
    PluginHttpResponse { status: 200, headers, body: body.as_bytes().to_vec() }
}

/// The calling user's Photon bearer token, forwarded by the host in the request
/// headers — lets this plugin act AS that user (uploads owned by them).
fn user_token(req: &PluginHttpRequest) -> Option<String> {
    let raw = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.clone())
        .or_else(|| {
            // Fallback: `?token=` (the page is opened with the user's token in its URL).
            req.query.split('&').find_map(|p| p.strip_prefix("token=").map(str::to_string))
        })?;
    Some(raw.trim_start_matches("Bearer ").trim_start_matches("bearer ").trim().to_string())
}

/// Send `rb`, failing on a transport error or a non-2xx status (the status error
/// is prefixed with `ctx`). Returns the successful response — the shared core of
/// the reqwest send/error_for_status ladder repeated across both API clients.
async fn send_checked(rb: reqwest::RequestBuilder, ctx: &str) -> Result<reqwest::Response, String> {
    rb.send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| format!("{ctx}: {e}"))
}

/// Send `rb` and deserialize the JSON body into `T`.
async fn get_json<T: serde::de::DeserializeOwned>(
    rb: reqwest::RequestBuilder,
    ctx: &str,
) -> Result<T, String> {
    send_checked(rb, ctx).await?.json().await.map_err(|e| e.to_string())
}

/// Send `rb`, discarding the response body — for POSTs whose result we don't read.
async fn send_ok(rb: reqwest::RequestBuilder, ctx: &str) -> Result<(), String> {
    send_checked(rb, ctx).await.map(|_| ())
}

/// The `(immich_url, immich_token)` pair every wizard step requires, or a ready
/// 400 response when either is missing.
fn immich_creds(body: &serde_json::Value) -> Result<(&str, &str), PluginHttpResponse> {
    match (
        body.get("immich_url").and_then(|v| v.as_str()),
        body.get("immich_token").and_then(|v| v.as_str()),
    ) {
        (Some(url), Some(key)) => Ok((url, key)),
        _ => Err(err(400, "immich_url and immich_token required")),
    }
}

// ---------- Immich API client ----------

struct Immich {
    base: String,
    key: String,
    http: reqwest::Client,
}
#[derive(Deserialize)]
struct ImAsset {
    id: String,
    #[serde(rename = "originalFileName", default)]
    original_file_name: String,
    #[serde(rename = "fileCreatedAt", default)]
    file_created_at: String,
    #[serde(rename = "type", default)]
    kind: String,
}
#[derive(Deserialize)]
struct SearchResp {
    assets: SearchAssets,
}
#[derive(Deserialize)]
struct SearchAssets {
    #[serde(default)]
    items: Vec<ImAsset>,
    #[serde(rename = "nextPage", default)]
    next_page: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct ImAlbumLite {
    id: String,
    #[serde(rename = "albumName", default)]
    album_name: String,
}
#[derive(Deserialize)]
struct ImAlbumDetail {
    #[serde(default)]
    assets: Vec<ImAsset>,
}

impl Immich {
    fn new(base: &str, key: &str) -> Self {
        Immich {
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: reqwest::Client::new(),
        }
    }
    fn url(&self, p: &str) -> String {
        format!("{}{}", self.base, p)
    }
    async fn me(&self) -> Result<serde_json::Value, String> {
        get_json(
            self.http.get(self.url("/api/users/me")).header("x-api-key", &self.key),
            "Immich auth/connection failed",
        )
        .await
    }
    /// Every asset on the server, paginated (cap to avoid runaway).
    async fn all_assets(&self) -> Result<Vec<ImAsset>, String> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let resp: SearchResp = get_json(
                self.http
                    .post(self.url("/api/search/metadata"))
                    .header("x-api-key", &self.key)
                    .json(&json!({ "page": page, "size": 1000 })),
                "Immich search failed",
            )
            .await?;
            let n = resp.assets.items.len();
            out.extend(resp.assets.items);
            let has_next = matches!(&resp.assets.next_page, Some(v) if !v.is_null());
            if n == 0 || !has_next || page >= 500 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }
    async fn albums(&self) -> Result<Vec<ImAlbumLite>, String> {
        get_json(
            self.http.get(self.url("/api/albums")).header("x-api-key", &self.key),
            "Immich albums",
        )
        .await
    }
    async fn album_asset_ids(&self, id: &str) -> Result<Vec<String>, String> {
        let d: ImAlbumDetail = get_json(
            self.http.get(self.url(&format!("/api/albums/{id}"))).header("x-api-key", &self.key),
            "Immich album",
        )
        .await?;
        Ok(d.assets.into_iter().map(|a| a.id).collect())
    }
    async fn download(&self, id: &str) -> Result<Vec<u8>, String> {
        let b = send_checked(
            self.http.get(self.url(&format!("/api/assets/{id}/original"))).header("x-api-key", &self.key),
            "Immich download failed",
        )
        .await?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
        Ok(b.to_vec())
    }
}

// ---------- Photon API (act as the calling user) ----------

struct Photon {
    base: String,
    token: String,
    http: reqwest::Client,
}
impl Photon {
    fn from_req(req: &PluginHttpRequest) -> Option<Self> {
        let base = std::env::var(photon_plugin_proto::API_URL_ENV).ok().filter(|s| !s.is_empty())?;
        let token = user_token(req)?;
        Some(Photon { base: base.trim_end_matches('/').to_string(), token, http: reqwest::Client::new() })
    }
    fn url(&self, p: &str) -> String {
        format!("{}{}", self.base, p)
    }
    async fn my_id(&self) -> Result<String, String> {
        let me: serde_json::Value =
            get_json(self.http.get(self.url("/api/me")).bearer_auth(&self.token), "Photon me").await?;
        me.get("id").and_then(|v| v.as_str()).map(str::to_string).ok_or("no user id".into())
    }
    /// Lowercased set of filenames already in the user's Photon library.
    async fn existing_filenames(&self) -> Result<HashSet<String>, String> {
        let photos: Vec<serde_json::Value> =
            get_json(self.http.get(self.url("/api/photos")).bearer_auth(&self.token), "Photon photos").await?;
        Ok(photos
            .iter()
            .filter_map(|p| p.get("filename").and_then(|v| v.as_str()))
            .map(|s| s.to_lowercase())
            .collect())
    }
    async fn albums(&self) -> Result<Vec<serde_json::Value>, String> {
        get_json(self.http.get(self.url("/api/albums")).bearer_auth(&self.token), "Photon albums").await
    }
    async fn create_album(&self, owner: &str, name: &str) -> Result<String, String> {
        let a: serde_json::Value = get_json(
            self.http
                .post(self.url("/api/albums"))
                .bearer_auth(&self.token)
                .json(&json!({ "name": name, "owner_id": owner, "photo_ids": [] })),
            "Photon album create",
        )
        .await?;
        a.get("id").and_then(|v| v.as_str()).map(str::to_string).ok_or("album create: no id".into())
    }
    async fn upload(&self, owner: &str, album: Option<&str>, filename: &str, bytes: &[u8]) -> Result<(), String> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let mut body = json!({ "owner_id": owner, "filename": filename, "bytes": b64 });
        if let Some(al) = album {
            body["album_id"] = json!(al);
        }
        send_ok(
            self.http.post(self.url("/api/uploads")).bearer_auth(&self.token).json(&body),
            "Photon upload failed",
        )
        .await
    }
}

// ---------- request handlers ----------

async fn handle_connect(body: &serde_json::Value) -> PluginHttpResponse {
    let (url, key) = match immich_creds(body) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match Immich::new(url, key).me().await {
        Ok(me) => json_resp(200, json!({ "ok": true, "user": me })),
        Err(e) => err(502, e),
    }
}

async fn handle_scan(req: &PluginHttpRequest, body: &serde_json::Value) -> PluginHttpResponse {
    let (url, key) = match immich_creds(body) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(photon) = Photon::from_req(req) else {
        return err(401, "not authenticated to Photon (missing user token)");
    };
    let im = Immich::new(url, key);

    let existing = match photon.existing_filenames().await {
        Ok(s) => s,
        Err(e) => return err(502, format!("listing Photon photos: {e}")),
    };
    let assets = match im.all_assets().await {
        Ok(a) => a,
        Err(e) => return err(502, e),
    };
    // Photos in Immich but NOT in Photon (matched by original filename).
    let missing: Vec<&ImAsset> = assets
        .iter()
        .filter(|a| !a.original_file_name.is_empty() && !existing.contains(&a.original_file_name.to_lowercase()))
        .collect();
    let missing_ids: HashSet<&str> = missing.iter().map(|a| a.id.as_str()).collect();

    // Map each MISSING asset to the Immich albums it belongs to.
    let im_albums = im.albums().await.unwrap_or_default();
    let mut asset_albums: HashMap<String, Vec<String>> = HashMap::new(); // asset id -> [immich album ids]
    let mut album_missing_count: HashMap<String, usize> = HashMap::new();
    let mut album_names: HashMap<String, String> = HashMap::new();
    for al in &im_albums {
        let ids = im.album_asset_ids(&al.id).await.unwrap_or_default();
        let mut count = 0;
        for aid in ids {
            if missing_ids.contains(aid.as_str()) {
                asset_albums.entry(aid).or_default().push(al.id.clone());
                count += 1;
            }
        }
        if count > 0 {
            album_missing_count.insert(al.id.clone(), count);
            album_names.insert(al.id.clone(), al.album_name.clone());
        }
    }

    let missing_json: Vec<serde_json::Value> = missing
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "filename": a.original_file_name,
                "date": a.file_created_at,
                "kind": a.kind,
                "albums": asset_albums.get(&a.id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    let albums_json: Vec<serde_json::Value> = album_missing_count
        .iter()
        .map(|(id, n)| json!({ "id": id, "name": album_names.get(id).cloned().unwrap_or_default(), "missing": n }))
        .collect();

    // Existing Photon albums (mapping targets).
    let photon_albums: Vec<serde_json::Value> = photon
        .albums()
        .await
        .unwrap_or_default()
        .iter()
        .map(|a| json!({ "id": a.get("id"), "name": a.get("name") }))
        .collect();

    json_resp(
        200,
        json!({
            "total_immich": assets.len(),
            "missing": missing_json,
            "immich_albums": albums_json,
            "photon_albums": photon_albums,
        }),
    )
}

async fn handle_import(req: &PluginHttpRequest, body: &serde_json::Value) -> PluginHttpResponse {
    let (url, key) = match immich_creds(body) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(photon) = Photon::from_req(req) else {
        return err(401, "not authenticated to Photon");
    };
    let owner = match photon.my_id().await {
        Ok(o) => o,
        Err(e) => return err(401, e),
    };
    let im = Immich::new(url, key);

    // album_map: { immich_album_id -> "<photon_album_id>" | "new:<Name>" | "" }
    let album_map: HashMap<String, String> = body
        .get("album_map")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default();
    // Resolve each Immich album to a Photon album id, creating new ones once.
    let mut resolved: HashMap<String, String> = HashMap::new();
    for (im_id, target) in &album_map {
        if target.is_empty() {
            continue;
        }
        let pid = if let Some(name) = target.strip_prefix("new:") {
            match photon.create_album(&owner, name).await {
                Ok(id) => id,
                Err(e) => return err(502, format!("creating album '{name}': {e}")),
            }
        } else {
            target.clone()
        };
        resolved.insert(im_id.clone(), pid);
    }

    // items: [{ id, filename, albums:[immich_album_id] }]
    let items = body.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut imported = 0u32;
    let mut failed: Vec<serde_json::Value> = Vec::new();
    for it in &items {
        let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let filename = it.get("filename").and_then(|v| v.as_str()).unwrap_or("immich.jpg");
        if id.is_empty() {
            continue;
        }
        // First mapped album wins as the upload target (Photon pairs one album here).
        let album = it
            .get("albums")
            .and_then(|v| v.as_array())
            .and_then(|a| a.iter().filter_map(|x| x.as_str()).find_map(|im_id| resolved.get(im_id)))
            .map(|s| s.as_str());
        match im.download(id).await {
            Ok(bytes) => match photon.upload(&owner, album, filename, &bytes).await {
                Ok(()) => imported += 1,
                Err(e) => failed.push(json!({ "filename": filename, "error": e })),
            },
            Err(e) => failed.push(json!({ "filename": filename, "error": e })),
        }
    }
    json_resp(200, json!({ "imported": imported, "failed": failed }))
}

#[async_trait]
impl RoutePlugin for ImmichImport {
    fn routes(&self) -> Vec<RouteDecl> {
        vec![
            // Served at `/ui` (not `/`) because the host's catch-all proxy matches
            // only non-empty sub-paths. Open `…/immich-import/ui?token=<token>`.
            RouteDecl::new("GET", "/ui"),
            RouteDecl::new("POST", "/connect"),
            RouteDecl::new("POST", "/scan"),
            RouteDecl::new("POST", "/import"),
        ]
    }

    async fn handle(&self, req: PluginHttpRequest) -> Result<PluginHttpResponse, PluginError> {
        let body: serde_json::Value =
            if req.body.is_empty() { json!({}) } else { serde_json::from_slice(&req.body).unwrap_or(json!({})) };
        let path = req.path.trim_end_matches('/');
        Ok(match (req.method.as_str(), path) {
            ("GET", "/ui") | ("GET", "") => html_resp(UI_HTML),
            ("POST", "/connect") => handle_connect(&body).await,
            ("POST", "/scan") => handle_scan(&req, &body).await,
            ("POST", "/import") => handle_import(&req, &body).await,
            _ => err(404, "not found"),
        })
    }
}

#[tokio::main]
async fn main() {
    serve(route(
        PluginMeta::new("immich-import", "Immich Import", env!("CARGO_PKG_VERSION")),
        ImmichImport,
    ))
    .await
}

/// The wizard UI (self-contained). Reads the user's Photon token from `?token=` in
/// its own URL and sends it on every call to this plugin's endpoints.
const UI_HTML: &str = include_str!("ui.html");
