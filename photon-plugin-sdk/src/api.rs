//! [`PhotonClient`] — the author-facing client a plugin uses to call BACK into
//! the Photon HTTP API.
//!
//! The host injects the API base URL + a bearer token for a trusted service
//! account into every plugin's environment at launch (see the proto crate's
//! `API_URL_ENV` / `API_TOKEN_ENV`). [`PhotonClient::from_env`] reads them, so a
//! plugin author writes:
//!
//! ```no_run
//! # async fn demo() {
//! use photon_plugin_sdk::PhotonClient;
//! if let Some(api) = PhotonClient::from_env() {
//!     let me: serde_json::Value = api.get_json("/api/me").await.unwrap();
//!     println!("acting as {me:?}");
//! }
//! # }
//! ```
//!
//! Every request carries the bearer token automatically. The methods are thin
//! typed wrappers over `reqwest`; an author who needs something bespoke can reach
//! the raw [`reqwest::Client`] via [`PhotonClient::http`] + [`PhotonClient::url`].

use serde::Serialize;
use serde::de::DeserializeOwned;

/// An error from a Photon API call: a transport failure, or a non-2xx response
/// (with the status code and body text captured for diagnostics).
#[derive(Debug)]
pub enum ApiError {
    /// The request never completed (DNS, connect, timeout, TLS, …).
    Transport(reqwest::Error),
    /// The server returned a non-2xx status.
    Status { status: u16, body: String },
    /// The 2xx body failed to deserialize into the requested type.
    Decode(reqwest::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Transport(e) => write!(f, "photon api transport error: {e}"),
            ApiError::Status { status, body } => {
                write!(f, "photon api returned {status}: {body}")
            }
            ApiError::Decode(e) => write!(f, "photon api decode error: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// A client for the Photon HTTP API, pre-authenticated as the plugin service
/// account. Cheap to clone (the inner `reqwest::Client` is reference-counted).
#[derive(Clone)]
pub struct PhotonClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl PhotonClient {
    /// Build a client from the env vars the host injects at launch
    /// (`PHOTON_API_URL` + `PHOTON_API_TOKEN`). Returns `None` when either is
    /// missing — e.g. when the plugin is run outside the host — so callers can
    /// gracefully skip API calls.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var(photon_plugin_proto::API_URL_ENV)
            .ok()
            .filter(|s| !s.is_empty())?;
        let token = std::env::var(photon_plugin_proto::API_TOKEN_ENV)
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Self::new(base_url, token))
    }

    /// Build a client explicitly (useful in tests).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    /// The underlying `reqwest::Client`, for requests the typed helpers don't
    /// cover. Remember to attach the token yourself with `.bearer_auth(...)` —
    /// see [`PhotonClient::token`].
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The bearer token (to attach to hand-rolled requests).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Join the base URL with an API `path` (leading slash optional).
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    /// `GET path` → deserialize the JSON body into `T`.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        Self::json(resp).await
    }

    /// `POST path` with a JSON `body` → deserialize the JSON response into `T`.
    /// Use `()` as the response type to ignore the body.
    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        Self::json(resp).await
    }

    /// `PATCH path` with a JSON `body` (Photon's PATCH endpoints take RFC-6902
    /// JSON Patch op arrays) → deserialize the JSON response into `T`.
    pub async fn patch_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        let resp = self
            .http
            .patch(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        Self::json(resp).await
    }

    /// `DELETE path`. Succeeds on any 2xx (the body is ignored).
    pub async fn delete(&self, path: &str) -> Result<(), ApiError> {
        let resp = self
            .http
            .delete(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        Self::ok(resp).await.map(|_| ())
    }

    /// Map a response into `T`, turning a non-2xx into [`ApiError::Status`].
    async fn json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, ApiError> {
        let resp = Self::ok(resp).await?;
        resp.json::<T>().await.map_err(ApiError::Decode)
    }

    /// Return the response if 2xx, else read the body and produce a status error.
    async fn ok(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(ApiError::Status { status, body })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joins_without_double_slash() {
        let c = PhotonClient::new("http://host:3000/", "tok");
        assert_eq!(c.url("/api/me"), "http://host:3000/api/me");
        assert_eq!(c.url("api/photos"), "http://host:3000/api/photos");
    }

    #[test]
    fn from_env_needs_both_vars() {
        // Missing vars → None (the safe, run-outside-host path). We don't set env
        // here to avoid racing other tests; just assert the unset case is None
        // unless the host happens to have injected them.
        if std::env::var(photon_plugin_proto::API_URL_ENV).is_err() {
            assert!(PhotonClient::from_env().is_none());
        }
    }
}
