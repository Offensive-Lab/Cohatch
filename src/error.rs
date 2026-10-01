use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid manifest: {0}")]
    Manifest(String),
    #[error("unsafe HTTP response: {0}")]
    Protocol(String),
    #[error(
        "remote object changed: {0}; keep existing chunks and recreate the manifest in a new directory"
    )]
    SourceChanged(String),
    #[error("HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("network error: {0}")]
    Network(reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid local data: {0}")]
    Integrity(String),
    #[error("missing or corrupt chunks: {0:?}")]
    MissingChunks(Vec<u64>),
    #[error("download directory is in use by another Cohatch operation")]
    Locked,
    #[error("cancelled; completed chunks remain available for resume")]
    Cancelled,
    #[error("{0}")]
    InvalidInput(String),
}

pub type Result<T> = std::result::Result<T, Error>;
