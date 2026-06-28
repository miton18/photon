//! Pluggable object storage backends.
//!
//! Two real backends are provided:
//!   * [`LocalFsBackend`] — writes objects under a root directory (source of
//!     truth in `Filesystem` mode).
//!   * [`S3Backend`] — pushes objects to an S3-compatible bucket via the
//!     `rust-s3` crate (used for the hourly BACKUP job and for
//!     `S3Replacement` mode).
//!
//! The upload/backup paths depend on the [`StorageBackend`] trait, not on a
//! concrete type, so additional backends can be slotted in later.

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use s3::creds::Credentials;
use s3::{Bucket, Region};

use crate::models::S3Config;

#[derive(Debug)]
pub enum StorageError {
    Io(String),
    Backend(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "io error: {e}"),
            StorageError::Backend(e) => write!(f, "backend error: {e}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        StorageError::Io(e.to_string())
    }
}

/// An async object store: write a blob at `key`, and check whether it exists.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn put_object(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError>;
    async fn exists(&self, key: &str) -> Result<bool, StorageError>;
    /// Read an object's bytes, or `None` if it doesn't exist.
    async fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
}

/// Filesystem-backed store rooted at a directory. Keys map to relative paths
/// under `root`; parent directories are created on demand.
pub struct LocalFsBackend {
    pub root: PathBuf,
}

impl LocalFsBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        // Strip any leading slash so the key stays under `root`.
        self.root.join(key.trim_start_matches('/'))
    }
}

#[async_trait]
impl StorageBackend for LocalFsBackend {
    async fn put_object(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        Ok(Path::new(&self.path_for(key)).exists())
    }

    async fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        match tokio::fs::read(self.path_for(key)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// S3-compatible backend backed by a `rust-s3` [`Bucket`].
pub struct S3Backend {
    bucket: Box<Bucket>,
    prefix: Option<String>,
}

impl S3Backend {
    /// Build an `S3Backend` from an [`S3Config`]. Supports custom endpoints
    /// (MinIO, Scaleway, Clever Cloud Cellar, ...) via `endpoint`.
    pub fn from_config(cfg: &S3Config) -> Result<Self, StorageError> {
        let credentials = Credentials::new(
            Some(&cfg.access_key_id),
            Some(&cfg.secret_access_key),
            None,
            None,
            None,
        )
        .map_err(|e| StorageError::Backend(e.to_string()))?;

        let region = match &cfg.endpoint {
            Some(endpoint) => Region::Custom {
                region: cfg.region.clone(),
                endpoint: endpoint.clone(),
            },
            None => cfg
                .region
                .parse()
                .map_err(|e: std::str::Utf8Error| StorageError::Backend(e.to_string()))?,
        };

        // Path-style addressing is the safe default for custom endpoints.
        let bucket = Bucket::new(&cfg.bucket, region, credentials)
            .map_err(|e| StorageError::Backend(e.to_string()))?
            .with_path_style();

        Ok(Self {
            bucket,
            prefix: cfg.prefix.clone(),
        })
    }

    fn full_key(&self, key: &str) -> String {
        match &self.prefix {
            Some(p) if !p.is_empty() => {
                format!("{}/{}", p.trim_end_matches('/'), key.trim_start_matches('/'))
            }
            _ => key.to_string(),
        }
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    async fn put_object(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let key = self.full_key(key);
        self.bucket
            .put_object(&key, bytes)
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let key = self.full_key(key);
        match self.bucket.head_object(&key).await {
            Ok(_) => Ok(true),
            // rust-s3 returns an error for a 404; treat that as "not found".
            Err(_) => Ok(false),
        }
    }

    async fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let key = self.full_key(key);
        match self.bucket.get_object(&key).await {
            Ok(resp) if resp.status_code() == 200 => Ok(Some(resp.bytes().to_vec())),
            // 404 (or any non-200) → treat as missing rather than a hard error.
            Ok(_) => Ok(None),
            Err(_) => Ok(None),
        }
    }
}
