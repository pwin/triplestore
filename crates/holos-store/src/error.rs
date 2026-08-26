//! What a storage read or write can fail with.
//!
//! The in-memory tier never produces any of these. They exist because the RocksDB tier of
//! `DESIGN.md` §6.1 will: a scan is an LSM iterator that can hit I/O, a dictionary lookup
//! is a `get` against the `id2str` column family, and both can fail in ways a `BTreeSet`
//! cannot.
//!
//! Introducing the error channel before that substrate exists is deliberate. Adding it
//! afterwards would mean changing the signature every layer above already depends on.

/// A storage-layer failure.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The underlying store could not be read or written.
    #[error("storage i/o failed: {0}")]
    Io(#[from] std::io::Error),
    /// The store returned something structurally impossible — a dangling term id, a key
    /// that does not decode. Always a bug or on-disk damage, never user input.
    #[error("corrupt store: {0}")]
    Corruption(String),
    /// The backend cannot do what was asked, and no retry will change that.
    ///
    /// Distinct from [`StorageError::Io`], which means it tried and failed. This means it
    /// did not try, because the operation has no meaning for this backend — a checkpoint of
    /// an in-memory store, say. Callers turn it into an explanation rather than a retry.
    #[error("{0}")]
    Unsupported(String),
}

impl StorageError {
    /// Builds a corruption error.
    #[must_use]
    pub fn corruption(detail: impl Into<String>) -> Self {
        Self::Corruption(detail.into())
    }
}

/// Shorthand for a storage result.
pub type Result<T> = std::result::Result<T, StorageError>;
