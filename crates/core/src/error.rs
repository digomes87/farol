use std::path::PathBuf;

/// Errors returned by every fallible operation in `farol-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("index file `{path}` is corrupt or was written by another version: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: bincode::Error,
    },

    #[error("failed to encode index: {0}")]
    Encode(#[source] bincode::Error),

    #[error("index format version {found} is not supported (expected {expected})")]
    Version { found: u32, expected: u32 },

    #[error("invalid query: {0}")]
    Query(String),
}

pub type Result<T> = std::result::Result<T, Error>;
