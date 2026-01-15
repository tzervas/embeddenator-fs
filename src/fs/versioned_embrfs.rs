//! Mutable EmbrFS implementation using versioned structures
//!
//! This module provides a mutable, concurrent-safe version of EmbrFS that supports
//! read-write operations with optimistic locking. Unlike the original EmbrFS which is
//! immutable by design, VersionedEmbrFS allows in-place updates while maintaining:
//!
//! - Bit-perfect reconstruction guarantees
//! - Concurrent read access without blocking
//! - Optimistic locking for writes with conflict detection
//! - VSA-native operations on compressed state
//!
//! ## Architecture
//!
//! ```text
//! VersionedEmbrFS
//!     ↓
//! VersionedEngram (coordinatesthree versioned components)
//!     ├── VersionedChunkStore (chunk_id → SparseVec)
//!     ├── VersionedManifest (file metadata)
//!     └── VersionedCorrectionStore (bit-perfect adjustments)
//! ```
//!
//! ## Usage Example
//!
//! ```rust,no_run
//! use embeddenator_fs::VersionedEmbrFS;
//! use std::path::Path;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create new mutable filesystem
//! let fs = VersionedEmbrFS::new();
//!
//! // Write a file
//! let data = b"Hello, EmbrFS!";
//! fs.write_file("hello.txt", data, None)?;
//!
//! // Read it back
//! let (content, version) = fs.read_file("hello.txt")?;
//! assert_eq!(&content, data);
//!
//! // Update the file
//! let updated = b"Hello, Mutable EmbrFS!";
//! fs.write_file("hello.txt", updated, Some(version))?;
//!
//! // Concurrent operations work with optimistic locking
//! # Ok(())
//! # }
//! ```

use crate::versioned::{
    VersionedChunk, VersionedChunkStore, VersionedCorrectionStore, VersionedFileEntry,
    VersionedManifest,
};
use crate::ReversibleVSAConfig;
use embeddenator_vsa::SparseVec;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use crate::versioned::types::{ChunkId, VersionMismatch, VersionedResult};
pub use crate::versioned::Operation;

/// Default chunk size for file encoding (4KB)
pub const DEFAULT_CHUNK_SIZE: usize = 4096;

/// Error types for VersionedEmbrFS operations
#[derive(Debug, Clone)]
pub enum EmbrFSError {
    /// File not found
    FileNotFound(String),
    /// Chunk not found
    ChunkNotFound(ChunkId),
    /// Version mismatch during optimistic locking
    VersionMismatch { expected: u64, actual: u64 },
    /// File already exists
    FileExists(String),
    /// Invalid operation
    InvalidOperation(String),
    /// IO error
    IoError(String),
}

impl std::fmt::Display for EmbrFSError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbrFSError::FileNotFound(path) => write!(f, "File not found: {}", path),
            EmbrFSError::ChunkNotFound(id) => write!(f, "Chunk not found: {}", id),
            EmbrFSError::VersionMismatch { expected, actual } => {
                write!(f, "Version mismatch: expected {}, got {}", expected, actual)
            }
            EmbrFSError::FileExists(path) => write!(f, "File already exists: {}", path),
            EmbrFSError::InvalidOperation(msg) => write!(f, "Invalid operation: {}", msg),
            EmbrFSError::IoError(msg) => write!(f, "IO error: {}", msg),
        }
    }
}

impl std::error::Error for EmbrFSError {}

impl From<VersionMismatch> for EmbrFSError {
    fn from(e: VersionMismatch) -> Self {
        EmbrFSError::VersionMismatch {
            expected: e.expected,
            actual: e.actual,
        }
    }
}

/// A mutable, versioned filesystem backed by holographic engrams
///
/// VersionedEmbrFS provides read-write operations with optimistic locking,
/// enabling concurrent access while maintaining consistency and bit-perfect
/// reconstruction guarantees.
pub struct VersionedEmbrFS {
    /// Root VSA vector (bundled superposition of all chunks)
    root: Arc<RwLock<Arc<SparseVec>>>,
    root_version: Arc<AtomicU64>,

    /// Versioned chunk store (chunk_id → encoded SparseVec)
    pub chunk_store: VersionedChunkStore,

    /// Versioned corrections for bit-perfect reconstruction
    pub corrections: VersionedCorrectionStore,

    /// Versioned manifest (file metadata)
    pub manifest: VersionedManifest,

    /// VSA configuration
    config: ReversibleVSAConfig,

    /// Global filesystem version
    global_version: Arc<AtomicU64>,

    /// Next chunk ID to allocate
    next_chunk_id: Arc<AtomicU64>,
}

impl VersionedEmbrFS {
    /// Create a new empty mutable filesystem
    pub fn new() -> Self {
        Self::with_config(ReversibleVSAConfig::default())
    }

    /// Create a new mutable filesystem with custom VSA configuration
    pub fn with_config(config: ReversibleVSAConfig) -> Self {
        Self {
            root: Arc::new(RwLock::new(Arc::new(SparseVec::new()))),
            root_version: Arc::new(AtomicU64::new(0)),
            chunk_store: VersionedChunkStore::new(),
            corrections: VersionedCorrectionStore::new(),
            manifest: VersionedManifest::new(),
            config,
            global_version: Arc::new(AtomicU64::new(0)),
            next_chunk_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Get the current global version
    pub fn version(&self) -> u64 {
        self.global_version.load(Ordering::Acquire)
    }

    /// Read a file's contents
    ///
    /// Returns the file data and the file entry version at read time.
    /// The version can be used for optimistic locking on subsequent writes.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use embeddenator_fs::VersionedEmbrFS;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let fs = VersionedEmbrFS::new();
    /// let (data, version) = fs.read_file("example.txt")?;
    /// println!("Read {} bytes at version {}", data.len(), version);
    /// # Ok(())
    /// # }
    /// ```
    pub fn read_file(&self, path: &str) -> Result<(Vec<u8>, u64), EmbrFSError> {
        // 1. Get file entry with version
        let (file_entry, _manifest_version) = self
            .manifest
            .get_file(path)
            .ok_or_else(|| EmbrFSError::FileNotFound(path.to_string()))?;

        if file_entry.deleted {
            return Err(EmbrFSError::FileNotFound(path.to_string()));
        }

        // 2. Read and decode chunks
        let mut file_data = Vec::with_capacity(file_entry.size);

        for &chunk_id in &file_entry.chunks {
            // Get chunk from store
            let (chunk, _chunk_version) = self
                .chunk_store
                .get(chunk_id)
                .ok_or(EmbrFSError::ChunkNotFound(chunk_id))?;

            // Decode chunk
            let decoded = chunk
                .vector
                .decode_data(&self.config, Some(&file_entry.path), DEFAULT_CHUNK_SIZE);

            // Apply correction
            let corrected = self
                .corrections
                .get(chunk_id as u64)
                .map(|(corr, _)| corr.apply(&decoded))
                .unwrap_or(decoded);

            file_data.extend_from_slice(&corrected);
        }

        // Truncate to exact file size
        file_data.truncate(file_entry.size);

        Ok((file_data, file_entry.version))
    }

    /// Write a file's contents
    ///
    /// If `expected_version` is provided, performs optimistic locking - the write
    /// will fail with VersionMismatch if the file has been modified since the version
    /// was read.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use embeddenator_fs::VersionedEmbrFS;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let fs = VersionedEmbrFS::new();
    ///
    /// // Create new file
    /// let version = fs.write_file("new.txt", b"content", None)?;
    ///
    /// // Update with version check
    /// let new_version = fs.write_file("new.txt", b"updated", Some(version))?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn write_file(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<u64>,
    ) -> Result<u64, EmbrFSError> {
        // 1. Check existing file
        let existing = self.manifest.get_file(path);

        match (&existing, expected_version) {
            (Some((entry, _)), Some(expected_ver)) => {
                // Update existing - verify version
                if entry.version != expected_ver {
                    return Err(EmbrFSError::VersionMismatch {
                        expected: expected_ver,
                        actual: entry.version,
                    });
                }
            }
            (Some(_), None) => {
                // File exists but no version check - fail
                return Err(EmbrFSError::FileExists(path.to_string()));
            }
            (None, Some(_)) => {
                // Expected file but doesn't exist
                return Err(EmbrFSError::FileNotFound(path.to_string()));
            }
            (None, None) => {
                // New file - OK
            }
        }

        // 2. Chunk the data
        let chunks = self.chunk_data(data);
        let mut chunk_ids = Vec::new();

        // 3. Get current chunk store version
        let store_version = self.chunk_store.version();

        // 4. Encode chunks and build updates
        let mut chunk_updates = Vec::new();
        let mut corrections_to_add = Vec::new();

        for chunk_data in chunks {
            let chunk_id = self.allocate_chunk_id();

            // Encode chunk
            let chunk_vec = SparseVec::encode_data(chunk_data, &self.config, Some(path));

            // Immediately verify
            let decoded = chunk_vec.decode_data(&self.config, Some(path), chunk_data.len());

            // Compute content hash
            let mut hasher = Sha256::new();
            hasher.update(chunk_data);
            let hash = hasher.finalize();
            let mut hash_bytes = [0u8; 8];
            hash_bytes.copy_from_slice(&hash[0..8]);

            // Create versioned chunk
            let versioned_chunk = VersionedChunk::new(chunk_vec, chunk_data.len(), hash_bytes);

            chunk_updates.push((chunk_id, versioned_chunk));

            // Prepare correction
            let correction = crate::correction::ChunkCorrection::new(chunk_id as u64, chunk_data, &decoded);
            corrections_to_add.push((chunk_id, correction));

            chunk_ids.push(chunk_id);
        }

        // 5. Batch insert chunks into store
        self.chunk_store
            .batch_insert(chunk_updates, store_version)?;

        // 6. Add corrections (after chunk store update)
        let mut corrections_version = self.corrections.current_version();
        for (chunk_id, correction) in corrections_to_add {
            corrections_version = self.corrections
                .update(chunk_id as u64, correction, corrections_version)?;
        }

        // 6. Update manifest
        let is_text = is_text_data(data);
        let new_entry = VersionedFileEntry::new(path.to_string(), is_text, data.len(), chunk_ids.clone());

        let file_version = if let Some((entry, _)) = existing {
            self.manifest
                .update_file(path, new_entry, entry.version)?;
            entry.version + 1
        } else {
            self.manifest.add_file(new_entry)?;
            0
        };

        // 7. Bundle chunks into root
        self.bundle_chunks_to_root(&chunk_ids)?;

        // 8. Increment global version
        self.global_version.fetch_add(1, Ordering::AcqRel);

        Ok(file_version)
    }

    /// Delete a file (soft delete)
    ///
    /// The file is marked as deleted but its chunks remain in the engram until
    /// compaction. Requires the current file version for optimistic locking.
    pub fn delete_file(&self, path: &str, expected_version: u64) -> Result<(), EmbrFSError> {
        self.manifest.remove_file(path, expected_version)?;
        self.global_version.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// List all non-deleted files
    pub fn list_files(&self) -> Vec<String> {
        self.manifest.list_files()
    }

    /// Check if a file exists
    pub fn exists(&self, path: &str) -> bool {
        self.manifest
            .get_file(path)
            .map(|(entry, _)| !entry.deleted)
            .unwrap_or(false)
    }

    /// Get filesystem statistics
    pub fn stats(&self) -> FilesystemStats {
        let manifest_stats = self.manifest.stats();
        let chunk_stats = self.chunk_store.stats();
        let correction_stats = self.corrections.stats();

        FilesystemStats {
            total_files: manifest_stats.total_files,
            active_files: manifest_stats.active_files,
            deleted_files: manifest_stats.deleted_files,
            total_chunks: chunk_stats.total_chunks as u64,
            total_size_bytes: manifest_stats.total_size_bytes,
            correction_overhead_bytes: correction_stats.total_correction_bytes,
            version: self.version(),
        }
    }

    // === Private helper methods ===

    /// Chunk data into DEFAULT_CHUNK_SIZE blocks
    fn chunk_data<'a>(&self, data: &'a [u8]) -> Vec<&'a [u8]> {
        data.chunks(DEFAULT_CHUNK_SIZE).collect()
    }

    /// Allocate a new unique chunk ID
    fn allocate_chunk_id(&self) -> ChunkId {
        self.next_chunk_id.fetch_add(1, Ordering::AcqRel) as ChunkId
    }

    /// Bundle chunks into root with CAS retry loop
    fn bundle_chunks_to_root(&self, chunk_ids: &[ChunkId]) -> Result<(), EmbrFSError> {
        // Retry loop for CAS
        loop {
            // Read current root
            let root_lock = self.root.read().unwrap();
            let current_root = Arc::clone(&*root_lock);
            let current_version = self.root_version.load(Ordering::Acquire);
            drop(root_lock);

            // Build new root by bundling chunks
            let mut new_root = (*current_root).clone();
            for &chunk_id in chunk_ids {
                if let Some((chunk, _)) = self.chunk_store.get(chunk_id) {
                    new_root = new_root.bundle(&chunk.vector);
                }
            }

            // Try CAS update
            let mut root_lock = self.root.write().unwrap();
            let actual_version = self.root_version.load(Ordering::Acquire);

            if actual_version == current_version {
                // Success - no concurrent update
                *root_lock = Arc::new(new_root);
                self.root_version.fetch_add(1, Ordering::AcqRel);
                return Ok(());
            }

            // Retry - someone else updated root
            drop(root_lock);
            // Small backoff to reduce contention
            std::thread::yield_now();
        }
    }
}

impl Default for VersionedEmbrFS {
    fn default() -> Self {
        Self::new()
    }
}

/// Filesystem statistics
#[derive(Debug, Clone)]
pub struct FilesystemStats {
    pub total_files: usize,
    pub active_files: usize,
    pub deleted_files: usize,
    pub total_chunks: u64,
    pub total_size_bytes: usize,
    pub correction_overhead_bytes: u64,
    pub version: u64,
}

/// Heuristic to detect if data is likely text
fn is_text_data(data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }

    let sample_size = data.len().min(8192);
    let sample = &data[0..sample_size];

    let non_printable = sample
        .iter()
        .filter(|&&b| b < 32 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();

    (non_printable as f64 / sample_size as f64) < 0.05
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_filesystem() {
        let fs = VersionedEmbrFS::new();
        assert_eq!(fs.version(), 0);
        assert_eq!(fs.list_files().len(), 0);
    }

    #[test]
    fn test_write_and_read_file() {
        let fs = VersionedEmbrFS::new();
        let data = b"Hello, EmbrFS!";

        // Write file
        let version = fs.write_file("test.txt", data, None).unwrap();
        assert_eq!(version, 0);

        // Read it back
        let (content, read_version) = fs.read_file("test.txt").unwrap();
        assert_eq!(&content[..], data);
        assert_eq!(read_version, 0);
    }

    #[test]
    fn test_update_file_with_version_check() {
        let fs = VersionedEmbrFS::new();

        // Create file
        let v1 = fs.write_file("test.txt", b"version 1", None).unwrap();

        // Update with correct version
        let v2 = fs
            .write_file("test.txt", b"version 2", Some(v1))
            .unwrap();
        assert_eq!(v2, v1 + 1);

        // Try to update with stale version (should fail)
        let result = fs.write_file("test.txt", b"version 3", Some(v1));
        assert!(matches!(result, Err(EmbrFSError::VersionMismatch { .. })));
    }

    #[test]
    fn test_delete_file() {
        let fs = VersionedEmbrFS::new();

        // Create and delete
        let version = fs.write_file("test.txt", b"data", None).unwrap();
        fs.delete_file("test.txt", version).unwrap();

        // Should not exist
        assert!(!fs.exists("test.txt"));

        // Read should fail
        let result = fs.read_file("test.txt");
        assert!(matches!(result, Err(EmbrFSError::FileNotFound(_))));
    }

    #[test]
    fn test_list_files() {
        let fs = VersionedEmbrFS::new();

        fs.write_file("file1.txt", b"a", None).unwrap();
        fs.write_file("file2.txt", b"b", None).unwrap();
        fs.write_file("file3.txt", b"c", None).unwrap();

        let files = fs.list_files();
        assert_eq!(files.len(), 3);
        assert!(files.contains(&"file1.txt".to_string()));
        assert!(files.contains(&"file2.txt".to_string()));
        assert!(files.contains(&"file3.txt".to_string()));
    }

    #[test]
    fn test_large_file() {
        let fs = VersionedEmbrFS::new();

        // Create file larger than one chunk
        let data = vec![42u8; DEFAULT_CHUNK_SIZE * 3 + 100];
        fs.write_file("large.bin", &data, None).unwrap();

        let (content, _) = fs.read_file("large.bin").unwrap();
        assert_eq!(content, data);
    }

    #[test]
    fn test_stats() {
        let fs = VersionedEmbrFS::new();

        fs.write_file("file1.txt", b"hello", None).unwrap();
        fs.write_file("file2.txt", b"world", None).unwrap();

        let stats = fs.stats();
        assert_eq!(stats.active_files, 2);
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.deleted_files, 0);
        assert_eq!(stats.total_size_bytes, 10);
    }
}
