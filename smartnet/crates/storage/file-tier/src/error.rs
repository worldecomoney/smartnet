//! Error types for file-tier operations.

/// Result alias for file-tier operations.
pub type FileTierResult<T> = Result<T, FileTierError>;

/// Errors that can occur while writing, sealing, or recovering file-tier shards.
#[derive(Debug, thiserror::Error)]
pub enum FileTierError {
    /// A filesystem operation failed, with the path that was involved.
    #[error(transparent)]
    Fs(#[from] smartnet_fs_util::FsPathError),

    /// A JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A record field could not be converted back to its consensus type (or a read was
    /// attempted without a configured file-tier base).
    #[error("record error: {0}")]
    RecordField(String),
}
