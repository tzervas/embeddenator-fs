//! Common types for versioned data structures

use std::fmt;

/// Unique identifier for a chunk
pub type ChunkId = usize;

/// Error returned when optimistic locking detects a version mismatch
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMismatch {
    pub expected: u64,
    pub actual: u64,
}

impl fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Version mismatch: expected {}, found {}",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for VersionMismatch {}

/// Result type for operations that may encounter version mismatches
pub type VersionedResult<T> = Result<T, VersionMismatch>;

/// Trait for versioned data structures
pub trait Versioned {
    /// Get the current version
    fn version(&self) -> u64;

    /// Increment the version atomically
    fn increment_version(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_mismatch_display() {
        let err = VersionMismatch {
            expected: 5,
            actual: 7,
        };
        assert_eq!(err.to_string(), "Version mismatch: expected 5, found 7");
    }
}
