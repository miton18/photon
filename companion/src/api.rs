//! Minimal async client for the Photon REST API (login + timeline + blobs).
//! Mirrors the web UI's contract: bearer token from `POST /api/login`, media
//! URLs carry the token as `?token=` (so they work for plain image GETs).

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // name/email are part of the client surface, shown in future views
pub struct User {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub email: String,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    token: String,
    user: User,
}

/// A single timeline photo, reduced to what the grid needs.
#[derive(Debug, Clone)]
pub struct Photo {
    pub id: String,
    /// Absolute, token-bearing thumbnail URL, or `None` when the server has no
    /// thumbnail for this photo yet (seed/demo rows).
    pub thumb_url: Option<String>,
}

/// An authenticated session against one Photon server.
#[derive(Clone)]
pub struct Session {
    base: String,
    token: String,
    pub user: User,
    http: reqwest::Client,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("photon-companion")
        .build()
        .expect("reqwest client")
}

/// Log in by email-or-username + password; returns an authenticated [`Session`].
pub async fn login(base: &str, ident: &str, password: &str) -> Result<Session, String> {
    let base = base.trim_end_matches('/').to_string();
    let http = http();
    let resp = http
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({ "email": ident, "password": password }))
        .send()
        .await
        .map_err(|e| format!("connection failed: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("invalid email/username or password".into());
    }
    if !resp.status().is_success() {
        return Err(format!("login failed ({})", resp.status()));
    }
    let body: LoginResponse = resp.json().await.map_err(|e| format!("bad response: {e}"))?;
    Ok(Session {
        base,
        token: body.token,
        user: body.user,
        http,
    })
}

impl Session {
    /// Append the session token to a server-relative media path → absolute URL.
    fn media_url(&self, path: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{}{}{}token={}", self.base, path, sep, self.token)
    }

    /// Fetch the user's timeline, flattened to a list of photos (newest first).
    pub async fn timeline(&self) -> Result<Vec<Photo>, String> {
        let url = format!("{}/api/users/{}/timeline", self.base, self.user.id);
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("timeline request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("timeline failed ({})", resp.status()));
        }
        let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        // The endpoint returns either `{sections:[{items:[...]}]}` or a flat list.
        let sections = value
            .get("sections")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_else(|| value.as_array().cloned().unwrap_or_default());
        let mut out = Vec::new();
        for sec in &sections {
            let items = sec.get("items").and_then(|i| i.as_array());
            let items = items.cloned().unwrap_or_else(|| vec![sec.clone()]);
            for it in items {
                let Some(id) = it.get("id").and_then(|v| v.as_str()) else { continue };
                let thumb_url = it
                    .get("thumb_url")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|p| self.media_url(p));
                out.push(Photo { id: id.to_string(), thumb_url });
            }
        }
        Ok(out)
    }

    /// Upload images to the user's own library via the async import endpoint.
    /// Only filename + bytes are sent; the server extracts everything else.
    pub async fn upload(&self, files: Vec<(String, Vec<u8>)>) -> Result<(), String> {
        use base64::Engine as _;
        let items: Vec<serde_json::Value> = files
            .into_iter()
            .map(|(name, bytes)| {
                serde_json::json!({
                    "filename": name,
                    "bytes": base64::engine::general_purpose::STANDARD.encode(&bytes),
                })
            })
            .collect();
        let body = serde_json::json!({ "owner_id": self.user.id, "files": items });
        let resp = self
            .http
            .post(format!("{}/api/uploads/raw", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("upload failed ({})", resp.status()));
        }
        Ok(())
    }

    /// Download raw bytes for a thumbnail/media URL.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("fetch failed ({})", resp.status()));
        }
        Ok(resp.bytes().await.map_err(|e| e.to_string())?.to_vec())
    }
}
