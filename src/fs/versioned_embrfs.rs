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
use embeddenator_io::{
    unwrap_auto, wrap_or_legacy, CompressionCodec, CompressionProfiler, PayloadKind,
};
use embeddenator_vsa::{Codebook, ProjectionResult, ReversibleVSAEncoder, SparseVec, DIM};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use crate::versioned::types::{ChunkId, VersionMismatch, VersionedResult};
pub use crate::versioned::Operation;

/// Default chunk size for file encoding (4KB)
pub const DEFAULT_CHUNK_SIZE: usize = 4096;

/// Holographic encoding format versions
/// Format 0: Legacy Codebook.project() - ~0% uncorrected accuracy
pub const ENCODING_FORMAT_LEGACY: u8 = 0;
/// Format 1: ReversibleVSAEncoder - ~94% uncorrected accuracy
pub const ENCODING_FORMAT_REVERSIBLE_VSA: u8 = 1;

/// Chunk size for ReversibleVSAEncoder (64 bytes for optimal accuracy)
pub const REVERSIBLE_CHUNK_SIZE: usize = 64;

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

    /// Compression profiler for path-based compression selection
    profiler: CompressionProfiler,

    /// Global filesystem version
    global_version: Arc<AtomicU64>,

    /// Next chunk ID to allocate
    next_chunk_id: Arc<AtomicU64>,

    /// Codebook for differential encoding (basis vectors)
    /// When holographic mode is enabled, data is projected onto these
    /// basis vectors and only residuals are stored in corrections.
    /// NOTE: Legacy mode only - new code should use reversible_encoder
    codebook: Arc<RwLock<Codebook>>,

    /// ReversibleVSAEncoder for true holographic encoding with ~94% accuracy
    /// This encoder uses position-aware binding to achieve reversible storage
    reversible_encoder: Arc<RwLock<ReversibleVSAEncoder>>,

    /// Whether holographic mode is enabled
    /// When true, uses ReversibleVSAEncoder for encoding (~94% uncorrected accuracy)
    /// When false, uses SparseVec::encode_data() (legacy mode)
    holographic_mode: bool,
}

impl VersionedEmbrFS {
    /// Create a new empty mutable filesystem (legacy mode)
    pub fn new() -> Self {
        Self::with_config(ReversibleVSAConfig::default())
    }

    /// Create a new mutable filesystem with holographic mode enabled
    ///
    /// In holographic mode, data is encoded using ReversibleVSAEncoder which
    /// achieves ~94% uncorrected accuracy through position-aware VSA binding.
    /// Only ~6% of bytes need correction, resulting in <10% correction overhead.
    pub fn new_holographic() -> Self {
        let mut fs = Self::with_config(ReversibleVSAConfig::default());
        fs.holographic_mode = true;
        // ReversibleVSAEncoder is already initialized in with_config_and_profiler
        fs
    }

    /// Create a new mutable filesystem with custom VSA configuration
    pub fn with_config(config: ReversibleVSAConfig) -> Self {
        Self::with_config_and_profiler(config, CompressionProfiler::default())
    }

    /// Create a new mutable filesystem with custom VSA configuration and compression profiler
    pub fn with_config_and_profiler(
        config: ReversibleVSAConfig,
        profiler: CompressionProfiler,
    ) -> Self {
        Self {
            root: Arc::new(RwLock::new(Arc::new(SparseVec::new()))),
            root_version: Arc::new(AtomicU64::new(0)),
            chunk_store: VersionedChunkStore::new(),
            corrections: VersionedCorrectionStore::new(),
            manifest: VersionedManifest::new(),
            config,
            profiler,
            global_version: Arc::new(AtomicU64::new(0)),
            next_chunk_id: Arc::new(AtomicU64::new(1)),
            codebook: Arc::new(RwLock::new(Codebook::new(DIM))),
            reversible_encoder: Arc::new(RwLock::new(ReversibleVSAEncoder::new())),
            holographic_mode: false,
        }
    }

    /// Enable holographic mode on an existing filesystem
    ///
    /// New writes will use ReversibleVSAEncoder for encoding with ~94% accuracy.
    pub fn enable_holographic_mode(&mut self) {
        self.holographic_mode = true;
        // ReversibleVSAEncoder is already initialized
    }

    /// Get a reference to the reversible encoder
    pub fn reversible_encoder(&self) -> &Arc<RwLock<ReversibleVSAEncoder>> {
        &self.reversible_encoder
    }

    /// Check if holographic mode is enabled
    pub fn is_holographic(&self) -> bool {
        self.holographic_mode
    }

    /// Get a reference to the codebook
    pub fn codebook(&self) -> &Arc<RwLock<Codebook>> {
        &self.codebook
    }

    /// Get the compression profiler
    pub fn profiler(&self) -> &CompressionProfiler {
        &self.profiler
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
            let decoded =
                chunk
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

        // 3. Decompress if file was stored compressed
        let final_data = if let Some(codec) = file_entry.compression_codec {
            if codec != 0 {
                // Use unwrap_auto which auto-detects envelope format
                unwrap_auto(PayloadKind::EngramBincode, &file_data)
                    .map_err(|e| EmbrFSError::IoError(format!("Decompression failed: {}", e)))?
            } else {
                file_data
            }
        } else {
            file_data
        };

        Ok((final_data, file_entry.version))
    }

    /// Read a specific byte range from a file
    ///
    /// This method enables efficient partial file reads by only decoding the chunks
    /// needed to satisfy the requested byte range. When the file has a chunk offset
    /// index, chunks are located in O(log n) time.
    ///
    /// # Arguments
    /// * `path` - The file path within the engram
    /// * `offset` - The starting byte offset to read from
    /// * `length` - The number of bytes to read
    ///
    /// # Returns
    /// A tuple of (data, version) where data is the requested byte range.
    /// If the range extends beyond the file, only available bytes are returned.
    ///
    /// # Performance
    /// - With offset index: O(log n + k) where k = number of chunks in range
    /// - Without offset index: O(n) where n = total chunks (must scan all)
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use embeddenator_fs::VersionedEmbrFS;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let fs = VersionedEmbrFS::new();
    /// // Read bytes 1000-1999 from a file
    /// let (data, version) = fs.read_range("large_file.bin", 1000, 1000)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn read_range(
        &self,
        path: &str,
        offset: usize,
        length: usize,
    ) -> Result<(Vec<u8>, u64), EmbrFSError> {
        // 1. Get file entry
        let (file_entry, _manifest_version) = self
            .manifest
            .get_file(path)
            .ok_or_else(|| EmbrFSError::FileNotFound(path.to_string()))?;

        if file_entry.deleted {
            return Err(EmbrFSError::FileNotFound(path.to_string()));
        }

        // Handle edge cases
        if offset >= file_entry.size || length == 0 {
            return Ok((Vec::new(), file_entry.version));
        }

        // Clamp the actual read length to file bounds
        let actual_length = length.min(file_entry.size - offset);

        // 2. Determine which chunks to read
        if file_entry.has_offset_index() {
            // Fast path: use offset index
            self.read_range_with_index(path, &file_entry, offset, actual_length)
        } else {
            // Slow path: must read sequentially and skip
            self.read_range_sequential(path, &file_entry, offset, actual_length)
        }
    }

    /// Read a byte range using the chunk offset index (fast path)
    fn read_range_with_index(
        &self,
        _path: &str,
        file_entry: &crate::fs::versioned::manifest::VersionedFileEntry,
        offset: usize,
        length: usize,
    ) -> Result<(Vec<u8>, u64), EmbrFSError> {
        let chunk_ranges = file_entry.chunks_for_range(offset, length);

        if chunk_ranges.is_empty() {
            return Ok((Vec::new(), file_entry.version));
        }

        let mut result = Vec::with_capacity(length);

        for range in chunk_ranges {
            // Get chunk from store
            let (chunk, _chunk_version) = self
                .chunk_store
                .get(range.chunk_id)
                .ok_or(EmbrFSError::ChunkNotFound(range.chunk_id))?;

            // Decode the chunk
            let decoded =
                chunk
                    .vector
                    .decode_data(&self.config, Some(&file_entry.path), chunk.original_size);

            // Apply correction
            let corrected = self
                .corrections
                .get(range.chunk_id as u64)
                .map(|(corr, _)| corr.apply(&decoded))
                .unwrap_or(decoded);

            // Extract the relevant portion
            let chunk_data = if range.start_within_chunk == 0 && range.length == corrected.len() {
                corrected
            } else {
                let end = (range.start_within_chunk + range.length).min(corrected.len());
                corrected[range.start_within_chunk..end].to_vec()
            };

            result.extend_from_slice(&chunk_data);
        }

        // Ensure we don't return more than requested
        result.truncate(length);

        Ok((result, file_entry.version))
    }

    /// Read a byte range without offset index (slow path)
    fn read_range_sequential(
        &self,
        _path: &str,
        file_entry: &crate::fs::versioned::manifest::VersionedFileEntry,
        offset: usize,
        length: usize,
    ) -> Result<(Vec<u8>, u64), EmbrFSError> {
        let mut result = Vec::with_capacity(length);
        let mut current_offset = 0usize;
        let end_offset = offset + length;

        for &chunk_id in &file_entry.chunks {
            // Get chunk from store
            let (chunk, _chunk_version) = self
                .chunk_store
                .get(chunk_id)
                .ok_or(EmbrFSError::ChunkNotFound(chunk_id))?;

            let chunk_size = chunk.original_size;
            let chunk_end = current_offset + chunk_size;

            // Skip chunks entirely before our range
            if chunk_end <= offset {
                current_offset = chunk_end;
                continue;
            }

            // Stop if we've passed our range
            if current_offset >= end_offset {
                break;
            }

            // Decode the chunk
            let decoded =
                chunk
                    .vector
                    .decode_data(&self.config, Some(&file_entry.path), chunk_size);

            // Apply correction
            let corrected = self
                .corrections
                .get(chunk_id as u64)
                .map(|(corr, _)| corr.apply(&decoded))
                .unwrap_or(decoded);

            // Calculate overlap with our range
            let start_in_chunk = offset.saturating_sub(current_offset);
            let end_in_chunk = (end_offset - current_offset).min(corrected.len());

            if start_in_chunk < end_in_chunk {
                result.extend_from_slice(&corrected[start_in_chunk..end_in_chunk]);
            }

            current_offset = chunk_end;

            // Stop if we've read enough
            if result.len() >= length {
                break;
            }
        }

        // Ensure we don't return more than requested
        result.truncate(length);

        Ok((result, file_entry.version))
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
            let correction =
                crate::correction::ChunkCorrection::new(chunk_id as u64, chunk_data, &decoded);
            corrections_to_add.push((chunk_id as u64, correction));

            chunk_ids.push(chunk_id);
        }

        // 5. Batch insert chunks into store
        if expected_version.is_none() {
            // New file - use lock-free insert (chunk IDs are unique)
            self.chunk_store.batch_insert_new(chunk_updates)?;
        } else {
            // Existing file - use versioned update with optimistic locking
            self.chunk_store
                .batch_insert(chunk_updates, store_version)?;
        }

        // 6. Add corrections (after chunk store update)
        if expected_version.is_none() {
            // New file - use lock-free insert (chunk IDs are unique)
            self.corrections.batch_insert_new(corrections_to_add)?;
        } else {
            // Existing file - use versioned batch update
            let corrections_version = self.corrections.current_version();
            self.corrections
                .batch_update(corrections_to_add, corrections_version)?;
        }

        // 6. Update manifest
        let is_text = is_text_data(data);
        let new_entry =
            VersionedFileEntry::new(path.to_string(), is_text, data.len(), chunk_ids.clone());

        let file_version = if let Some((entry, _)) = existing {
            self.manifest.update_file(path, new_entry, entry.version)?;
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

    /// Write a file's contents with path-based automatic compression
    ///
    /// Uses the compression profiler to automatically select the appropriate
    /// compression codec based on the file path. For example, config files
    /// in /etc get fast LZ4 compression, kernel images get maximum zstd
    /// compression, and pre-compressed media files skip compression entirely.
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
    /// // Config file gets fast LZ4 compression automatically
    /// let version = fs.write_file_compressed("/etc/nginx.conf", b"worker_processes 4;", None)?;
    ///
    /// // Kernel image gets maximum zstd compression
    /// let kernel_data = std::fs::read("/boot/vmlinuz")?;
    /// fs.write_file_compressed("/boot/vmlinuz", &kernel_data, None)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn write_file_compressed(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<u64>,
    ) -> Result<u64, EmbrFSError> {
        // 1. Get compression profile for this path
        let profile = self.profiler.for_path(path);
        let write_opts = profile.to_write_options();

        // 2. Compress data using the selected profile
        let (compressed_data, codec_byte) = if write_opts.codec == CompressionCodec::None {
            // No compression - store as-is
            (data.to_vec(), 0u8)
        } else {
            // Wrap with envelope format (includes compression)
            let wrapped = wrap_or_legacy(PayloadKind::EngramBincode, write_opts, data)
                .map_err(|e| EmbrFSError::IoError(format!("Compression failed: {}", e)))?;
            let codec = match write_opts.codec {
                CompressionCodec::None => 0,
                CompressionCodec::Zstd => 1,
                CompressionCodec::Lz4 => 2,
            };
            (wrapped, codec)
        };

        // 3. Check existing file
        let existing = self.manifest.get_file(path);

        match (&existing, expected_version) {
            (Some((entry, _)), Some(expected_ver)) => {
                if entry.version != expected_ver {
                    return Err(EmbrFSError::VersionMismatch {
                        expected: expected_ver,
                        actual: entry.version,
                    });
                }
            }
            (Some(_), None) => {
                return Err(EmbrFSError::FileExists(path.to_string()));
            }
            (None, Some(_)) => {
                return Err(EmbrFSError::FileNotFound(path.to_string()));
            }
            (None, None) => {}
        }

        // 4. Chunk the compressed data
        let chunks = self.chunk_data(&compressed_data);
        let mut chunk_ids = Vec::new();

        // 5. Get current chunk store version
        let store_version = self.chunk_store.version();

        // 6. Encode chunks and build updates
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
            let correction =
                crate::correction::ChunkCorrection::new(chunk_id as u64, chunk_data, &decoded);
            corrections_to_add.push((chunk_id as u64, correction));

            chunk_ids.push(chunk_id);
        }

        // 7. Batch insert chunks into store
        if expected_version.is_none() {
            self.chunk_store.batch_insert_new(chunk_updates)?;
        } else {
            self.chunk_store
                .batch_insert(chunk_updates, store_version)?;
        }

        // 8. Add corrections
        if expected_version.is_none() {
            self.corrections.batch_insert_new(corrections_to_add)?;
        } else {
            let corrections_version = self.corrections.current_version();
            self.corrections
                .batch_update(corrections_to_add, corrections_version)?;
        }

        // 9. Update manifest with compression metadata
        let is_text = is_text_data(data);
        let new_entry = if codec_byte == 0 {
            VersionedFileEntry::new(path.to_string(), is_text, data.len(), chunk_ids.clone())
        } else {
            VersionedFileEntry::new_compressed(
                path.to_string(),
                is_text,
                compressed_data.len(),
                data.len(),
                codec_byte,
                chunk_ids.clone(),
            )
        };

        let file_version = if let Some((entry, _)) = existing {
            self.manifest.update_file(path, new_entry, entry.version)?;
            entry.version + 1
        } else {
            self.manifest.add_file(new_entry)?;
            0
        };

        // 10. Bundle chunks into root
        self.bundle_chunks_to_root(&chunk_ids)?;

        // 11. Increment global version
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

    // === Holographic encoding methods ===

    /// Write a file using holographic encoding via ReversibleVSAEncoder
    ///
    /// This is the TRUE holographic storage method achieving ~94% uncorrected accuracy:
    /// 1. Encode data using ReversibleVSAEncoder.encode_chunked() (position-aware binding)
    /// 2. Store encoded SparseVecs in chunk_store (one per REVERSIBLE_CHUNK_SIZE bytes)
    /// 3. Decode to verify and compute corrections for bit-perfect reconstruction
    /// 4. Store only sparse corrections (~6% of bytes need correction)
    ///
    /// The position-aware binding ensures each byte at each position has a unique
    /// representation that can be retrieved via unbinding.
    pub fn write_file_holographic(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<u64>,
    ) -> Result<u64, EmbrFSError> {
        // 1. Check existing file
        let existing = self.manifest.get_file(path);

        match (&existing, expected_version) {
            (Some((entry, _)), Some(expected_ver)) => {
                if entry.version != expected_ver {
                    return Err(EmbrFSError::VersionMismatch {
                        expected: expected_ver,
                        actual: entry.version,
                    });
                }
            }
            (Some(_), None) => {
                return Err(EmbrFSError::FileExists(path.to_string()));
            }
            (None, Some(_)) => {
                return Err(EmbrFSError::FileNotFound(path.to_string()));
            }
            (None, None) => {}
        }

        // 2. Encode data using ReversibleVSAEncoder with chunking for optimal accuracy
        let mut encoder = self.reversible_encoder.write().unwrap();
        let encoded_chunks = encoder.encode_chunked(data, REVERSIBLE_CHUNK_SIZE);

        // Decode to verify and compute corrections
        let decoded = encoder.decode_chunked(&encoded_chunks, REVERSIBLE_CHUNK_SIZE, data.len());
        drop(encoder);

        // 3. Store each encoded chunk
        let store_version = self.chunk_store.version();
        let mut chunk_ids = Vec::with_capacity(encoded_chunks.len());
        let mut chunk_updates = Vec::with_capacity(encoded_chunks.len());
        let mut corrections_to_add = Vec::new();

        for (chunk_idx, chunk_vec) in encoded_chunks.into_iter().enumerate() {
            let chunk_id = self.allocate_chunk_id();
            chunk_ids.push(chunk_id);

            // Calculate the data range for this chunk
            let start = chunk_idx * REVERSIBLE_CHUNK_SIZE;
            let end = (start + REVERSIBLE_CHUNK_SIZE).min(data.len());
            let chunk_data = &data[start..end];
            let decoded_chunk = &decoded[start..end];

            // Compute content hash for verification
            let mut hasher = Sha256::new();
            hasher.update(chunk_data);
            let hash = hasher.finalize();
            let mut hash_bytes = [0u8; 8];
            hash_bytes.copy_from_slice(&hash[0..8]);

            let versioned_chunk = VersionedChunk::new(chunk_vec, chunk_data.len(), hash_bytes);
            chunk_updates.push((chunk_id, versioned_chunk));

            // Prepare correction (only stores differences, should be ~6% of bytes)
            let correction =
                crate::correction::ChunkCorrection::new(chunk_id as u64, chunk_data, decoded_chunk);
            corrections_to_add.push((chunk_id as u64, correction));
        }

        // 4. Batch insert chunks
        if expected_version.is_none() {
            self.chunk_store.batch_insert_new(chunk_updates)?;
        } else {
            self.chunk_store
                .batch_insert(chunk_updates, store_version)?;
        }

        // 5. Batch insert corrections
        if expected_version.is_none() {
            self.corrections.batch_insert_new(corrections_to_add)?;
        } else {
            let corrections_version = self.corrections.current_version();
            self.corrections
                .batch_update(corrections_to_add, corrections_version)?;
        }

        // 6. Update manifest with encoding format version
        let is_text = is_text_data(data);
        let new_entry = VersionedFileEntry::new_holographic(
            path.to_string(),
            is_text,
            data.len(),
            chunk_ids.clone(),
            ENCODING_FORMAT_REVERSIBLE_VSA,
        );

        let file_version = if let Some((entry, _)) = existing {
            self.manifest.update_file(path, new_entry, entry.version)?;
            entry.version + 1
        } else {
            self.manifest.add_file(new_entry)?;
            0
        };

        // 7. Bundle into root
        self.bundle_chunks_to_root(&chunk_ids)?;

        // 8. Increment global version
        self.global_version.fetch_add(1, Ordering::AcqRel);

        Ok(file_version)
    }

    /// Read a file using holographic decoding via ReversibleVSAEncoder
    ///
    /// This reverses the holographic encoding:
    /// 1. Load all SparseVecs from chunk_store
    /// 2. Use ReversibleVSAEncoder.decode_chunked() to reconstruct data
    /// 3. Apply per-chunk corrections for bit-perfect result
    ///
    /// Supports both legacy (format 0) and new (format 1) encoding formats.
    pub fn read_file_holographic(&self, path: &str) -> Result<(Vec<u8>, u64), EmbrFSError> {
        // 1. Get file entry
        let (file_entry, _) = self
            .manifest
            .get_file(path)
            .ok_or_else(|| EmbrFSError::FileNotFound(path.to_string()))?;

        if file_entry.deleted {
            return Err(EmbrFSError::FileNotFound(path.to_string()));
        }

        // 2. Handle empty files
        if file_entry.chunks.is_empty() {
            return Ok((Vec::new(), file_entry.version));
        }

        // 3. Check encoding format and dispatch to appropriate decoder
        let encoding_format = file_entry.encoding_format.unwrap_or(ENCODING_FORMAT_LEGACY);

        match encoding_format {
            ENCODING_FORMAT_REVERSIBLE_VSA => self.read_file_holographic_reversible(&file_entry),
            // Legacy format (0) and any unknown formats: use codebook-based reconstruction
            _ => self.read_file_holographic_legacy(&file_entry),
        }
    }

    /// Read a file encoded with ReversibleVSAEncoder (format 1)
    fn read_file_holographic_reversible(
        &self,
        file_entry: &VersionedFileEntry,
    ) -> Result<(Vec<u8>, u64), EmbrFSError> {
        // 1. Load all chunk vectors (clone from Arc to owned SparseVec)
        let mut chunk_vecs = Vec::with_capacity(file_entry.chunks.len());
        for &chunk_id in &file_entry.chunks {
            let (chunk, _) = self
                .chunk_store
                .get(chunk_id)
                .ok_or(EmbrFSError::ChunkNotFound(chunk_id))?;
            // Clone the SparseVec out of the Arc for decode_chunked
            chunk_vecs.push((*chunk.vector).clone());
        }

        // 2. Decode using ReversibleVSAEncoder
        let encoder = self.reversible_encoder.read().unwrap();
        let mut reconstructed =
            encoder.decode_chunked(&chunk_vecs, REVERSIBLE_CHUNK_SIZE, file_entry.size);
        drop(encoder);

        // 3. Apply per-chunk corrections for bit-perfect result
        for (chunk_idx, &chunk_id) in file_entry.chunks.iter().enumerate() {
            if let Some((correction, _)) = self.corrections.get(chunk_id as u64) {
                // Calculate the data range for this chunk
                let start = chunk_idx * REVERSIBLE_CHUNK_SIZE;
                let end = (start + REVERSIBLE_CHUNK_SIZE).min(file_entry.size);

                // Apply correction to this chunk's portion of the reconstructed data
                let chunk_data = &reconstructed[start..end];
                let corrected = correction.apply(chunk_data);

                // Copy corrected data back
                reconstructed[start..end].copy_from_slice(&corrected[..end - start]);
            }
        }

        // Truncate to exact size
        reconstructed.truncate(file_entry.size);

        Ok((reconstructed, file_entry.version))
    }

    /// Read a file encoded with legacy Codebook.project() (format 0)
    fn read_file_holographic_legacy(
        &self,
        file_entry: &VersionedFileEntry,
    ) -> Result<(Vec<u8>, u64), EmbrFSError> {
        // Legacy format expects a single chunk containing the projection
        let chunk_id = file_entry.chunks[0];
        let (chunk, _) = self
            .chunk_store
            .get(chunk_id)
            .ok_or(EmbrFSError::ChunkNotFound(chunk_id))?;

        // Convert SparseVec back to projection
        let projection = self.sparsevec_to_projection(&chunk.vector);

        // Reconstruct using codebook
        let codebook = self.codebook.read().unwrap();
        let mut reconstructed = codebook.reconstruct(&projection, file_entry.size);
        drop(codebook);

        // Apply correction for bit-perfect result
        if let Some((correction, _)) = self.corrections.get(chunk_id as u64) {
            reconstructed = correction.apply(&reconstructed);
        }

        // Truncate to exact size
        reconstructed.truncate(file_entry.size);

        Ok((reconstructed, file_entry.version))
    }

    /// Convert a ProjectionResult to a SparseVec for holographic storage
    ///
    /// The coefficients (basis_id -> weight) are encoded as:
    /// - pos indices: basis_id * 256 + encoded_weight for positive weights
    /// - neg indices: basis_id * 256 + encoded_weight for negative weights
    ///
    /// NOTE: This is legacy code kept for backward compatibility with format 0.
    /// New writes use ReversibleVSAEncoder (format 1) instead.
    #[allow(dead_code)]
    fn projection_to_sparsevec(&self, projection: &ProjectionResult) -> SparseVec {
        let mut pos = Vec::new();
        let mut neg = Vec::new();

        for (&key, word) in &projection.coefficients {
            let value = word.decode();
            let basis_id = (key / 1000) as usize; // coefficient_key_spacing = 1000
            let chunk_idx = (key % 1000) as usize;

            // Encode as: (basis_id * max_chunks + chunk_idx) * 2 + sign
            // This gives us a unique index for each coefficient
            let base_idx = (basis_id * 1000 + chunk_idx) % DIM;

            if value > 0 {
                pos.push(base_idx);
            } else if value < 0 {
                neg.push(base_idx);
            }
        }

        pos.sort_unstable();
        pos.dedup();
        neg.sort_unstable();
        neg.dedup();

        SparseVec { pos, neg }
    }

    /// Convert a SparseVec back to a ProjectionResult for reconstruction
    fn sparsevec_to_projection(&self, vec: &SparseVec) -> ProjectionResult {
        use embeddenator_vsa::{BalancedTernaryWord, WordMetadata};
        use std::collections::HashMap;

        let mut coefficients = HashMap::new();

        // Decode positive indices
        for &idx in &vec.pos {
            let chunk_idx = idx % 1000;
            let basis_id = (idx / 1000) % 100; // Assume max 100 basis vectors
            let key = (basis_id * 1000 + chunk_idx) as u32;

            // Positive weight (use default scale)
            if let Ok(word) = BalancedTernaryWord::new(500, WordMetadata::Data) {
                coefficients.insert(key, word);
            }
        }

        // Decode negative indices
        for &idx in &vec.neg {
            let chunk_idx = idx % 1000;
            let basis_id = (idx / 1000) % 100;
            let key = (basis_id * 1000 + chunk_idx) as u32;

            // Negative weight
            if let Ok(word) = BalancedTernaryWord::new(-500, WordMetadata::Data) {
                coefficients.insert(key, word);
            }
        }

        ProjectionResult {
            coefficients,
            residual: Vec::new(), // Residual is in corrections
            outliers: Vec::new(),
            quality_score: 0.5,
        }
    }

    /// Convert projection residual to a ChunkCorrection
    ///
    /// NOTE: This is legacy code kept for backward compatibility with format 0.
    /// New writes use ReversibleVSAEncoder (format 1) instead.
    #[allow(dead_code)]
    fn projection_to_correction(
        &self,
        chunk_id: u64,
        original: &[u8],
        projection: &ProjectionResult,
    ) -> crate::correction::ChunkCorrection {
        // Reconstruct from projection (without correction)
        let codebook = self.codebook.read().unwrap();
        let reconstructed = codebook.reconstruct(projection, original.len());
        drop(codebook);

        // Create correction from difference
        crate::correction::ChunkCorrection::new(chunk_id, original, &reconstructed)
    }

    // === Public helper methods for streaming API ===

    /// Get the VSA configuration
    pub fn config(&self) -> &ReversibleVSAConfig {
        &self.config
    }

    /// Allocate a new unique chunk ID (public for streaming API)
    pub fn allocate_chunk_id(&self) -> ChunkId {
        self.next_chunk_id.fetch_add(1, Ordering::AcqRel) as ChunkId
    }

    /// Bundle chunks into root - streaming variant that doesn't retry on mismatch
    ///
    /// For streaming ingestion, we bundle progressively without requiring
    /// atomicity since we're building up the root from scratch.
    pub fn bundle_chunks_to_root_streaming(
        &self,
        chunk_ids: &[ChunkId],
    ) -> Result<(), EmbrFSError> {
        self.bundle_chunks_to_root(chunk_ids)
    }

    // === Private helper methods ===

    /// Chunk data into DEFAULT_CHUNK_SIZE blocks
    fn chunk_data<'a>(&self, data: &'a [u8]) -> Vec<&'a [u8]> {
        data.chunks(DEFAULT_CHUNK_SIZE).collect()
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
        let v2 = fs.write_file("test.txt", b"version 2", Some(v1)).unwrap();
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

    #[test]
    fn test_write_and_read_compressed_file() {
        let fs = VersionedEmbrFS::new();

        // Config files get LZ4 compression
        let config_data = b"[server]\nport = 8080\nhost = localhost";
        let version = fs
            .write_file_compressed("/etc/app.conf", config_data, None)
            .unwrap();
        assert_eq!(version, 0);

        // Read it back - should auto-decompress
        let (content, read_version) = fs.read_file("/etc/app.conf").unwrap();
        assert_eq!(&content[..], config_data);
        assert_eq!(read_version, 0);
    }

    #[test]
    fn test_write_compressed_with_zstd_profile() {
        let fs = VersionedEmbrFS::new();

        // Binary files get zstd compression
        let binary_data: Vec<u8> = (0..1000).map(|i| [0xDE, 0xAD, 0xBE, 0xEF][i % 4]).collect();
        let version = fs
            .write_file_compressed("/usr/bin/myapp", &binary_data, None)
            .unwrap();
        assert_eq!(version, 0);

        // Read it back
        let (content, _) = fs.read_file("/usr/bin/myapp").unwrap();
        assert_eq!(content, binary_data);
    }

    #[test]
    fn test_write_compressed_no_compression_for_media() {
        let fs = VersionedEmbrFS::new();

        // Media files skip compression (already compressed)
        let media_data: Vec<u8> = (0..500).map(|i| [0xFF, 0xD8, 0xFF, 0xE0][i % 4]).collect();
        let version = fs
            .write_file_compressed("/photos/image.jpg", &media_data, None)
            .unwrap();
        assert_eq!(version, 0);

        // Read it back - no decompression needed
        let (content, _) = fs.read_file("/photos/image.jpg").unwrap();
        assert_eq!(content, media_data);
    }

    #[test]
    fn test_profiler_access() {
        let fs = VersionedEmbrFS::new();
        let profiler = fs.profiler();

        // Test profile selection
        let kernel_profile = profiler.for_path("/boot/vmlinuz");
        assert_eq!(kernel_profile.name, "Kernel");

        let config_profile = profiler.for_path("/etc/nginx.conf");
        assert_eq!(config_profile.name, "Config");
    }

    #[test]
    fn test_holographic_write_and_read() {
        let fs = VersionedEmbrFS::new_holographic();
        let data = b"Hello, Holographic EmbrFS!";

        // Write file using holographic encoding
        let version = fs.write_file_holographic("test.txt", data, None).unwrap();
        assert_eq!(version, 0);

        // Read it back
        let (content, read_version) = fs.read_file_holographic("test.txt").unwrap();
        assert_eq!(&content[..], data);
        assert_eq!(read_version, 0);
    }

    #[test]
    fn test_holographic_accuracy() {
        let fs = VersionedEmbrFS::new_holographic();

        // Test with various data patterns to verify >90% uncorrected accuracy
        let test_data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();

        // Write and read back
        fs.write_file_holographic("accuracy_test.bin", &test_data, None)
            .unwrap();
        let (content, _) = fs.read_file_holographic("accuracy_test.bin").unwrap();

        // Should be bit-perfect (with corrections applied)
        assert_eq!(content, test_data);

        // Check that correction overhead is reasonable
        let stats = fs.stats();
        let correction_ratio =
            stats.correction_overhead_bytes as f64 / stats.total_size_bytes as f64;

        // With ~94% accuracy, correction should be <10% of data
        // (allowing some margin for test data patterns)
        assert!(
            correction_ratio < 0.15,
            "Correction overhead too high: {:.1}%",
            correction_ratio * 100.0
        );
    }

    #[test]
    fn test_holographic_large_file() {
        let fs = VersionedEmbrFS::new_holographic();

        // Create a larger file that spans multiple chunks
        let data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();

        fs.write_file_holographic("large_holo.bin", &data, None)
            .unwrap();
        let (content, _) = fs.read_file_holographic("large_holo.bin").unwrap();

        // Should be bit-perfect
        assert_eq!(content, data);
    }

    #[test]
    fn test_holographic_encoding_format_in_manifest() {
        let fs = VersionedEmbrFS::new_holographic();
        let data = b"Test encoding format";

        fs.write_file_holographic("format_test.txt", data, None)
            .unwrap();

        // Check manifest has correct encoding format
        let (file_entry, _) = fs.manifest.get_file("format_test.txt").unwrap();
        assert_eq!(
            file_entry.encoding_format,
            Some(ENCODING_FORMAT_REVERSIBLE_VSA)
        );
    }

    #[test]
    fn test_holographic_update_file() {
        let fs = VersionedEmbrFS::new_holographic();

        // Create file
        let v1 = fs
            .write_file_holographic("update_test.txt", b"version 1", None)
            .unwrap();
        assert_eq!(v1, 0);

        // Update with correct version
        let v2 = fs
            .write_file_holographic("update_test.txt", b"version 2 is longer", Some(v1))
            .unwrap();
        assert_eq!(v2, 1);

        // Read back and verify
        let (content, version) = fs.read_file_holographic("update_test.txt").unwrap();
        assert_eq!(&content[..], b"version 2 is longer");
        assert_eq!(version, 1);
    }

    #[test]
    fn test_holographic_empty_file() {
        let fs = VersionedEmbrFS::new_holographic();

        fs.write_file_holographic("empty.txt", b"", None).unwrap();
        let (content, _) = fs.read_file_holographic("empty.txt").unwrap();

        assert!(content.is_empty());
    }

    #[test]
    fn test_enable_holographic_mode() {
        let mut fs = VersionedEmbrFS::new();
        assert!(!fs.is_holographic());

        fs.enable_holographic_mode();
        assert!(fs.is_holographic());
    }

    #[test]
    fn test_read_range_basic() {
        let fs = VersionedEmbrFS::new();

        // Create a file with known content
        let data = b"Hello, World! This is a test file for range queries.";
        fs.write_file("range_test.txt", data, None).unwrap();

        // Read specific ranges
        let (result, _) = fs.read_range("range_test.txt", 0, 5).unwrap();
        assert_eq!(&result[..], b"Hello");

        let (result, _) = fs.read_range("range_test.txt", 7, 6).unwrap();
        assert_eq!(&result[..], b"World!");

        // Read beyond file size (should return what's available)
        let (result, _) = fs.read_range("range_test.txt", 44, 100).unwrap();
        assert_eq!(&result[..], b"queries.");

        // Read at/beyond file end
        let (result, _) = fs.read_range("range_test.txt", 1000, 10).unwrap();
        assert!(result.is_empty());

        // Read zero length
        let (result, _) = fs.read_range("range_test.txt", 0, 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_read_range_not_found() {
        let fs = VersionedEmbrFS::new();
        let result = fs.read_range("nonexistent.txt", 0, 10);
        assert!(result.is_err());
    }
}
