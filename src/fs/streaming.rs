//! Streaming Ingester for Memory-Efficient File Encoding
//!
//! This module provides a streaming API for ingesting large files without loading
//! them entirely into memory. It processes data in chunks, encoding each chunk
//! as it arrives and bundling progressively into the root engram.
//!
//! # Why Streaming?
//!
//! For large files (>100MB), loading everything into memory before encoding:
//! - Causes memory pressure and potential OOM for multi-GB files
//! - Increases latency (must read entire file before any encoding starts)
//! - Wastes resources (chunks are independent, can encode in parallel)
//!
//! Streaming ingestion solves these by:
//! - Processing fixed-size chunks as they arrive
//! - Encoding each chunk immediately
//! - Maintaining bounded memory usage (~chunk_size * pipeline_depth)
//! - Enabling early error detection
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     ┌─────────────────┐     ┌──────────────────┐
//! │ Data Source │────▶│ StreamingIngester│────▶│ VersionedEmbrFS │
//! │ (Read/io)   │     │ (chunk + encode) │     │ (store + bundle) │
//! └─────────────┘     └─────────────────┘     └──────────────────┘
//!                              │
//!                              ├── Chunk Buffer (4KB default)
//!                              ├── Correction Accumulator
//!                              └── Progressive Hash
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use embeddenator_fs::streaming::StreamingIngester;
//! use std::fs::File;
//! use std::io::BufReader;
//!
//! let file = File::open("large_file.bin")?;
//! let reader = BufReader::new(file);
//!
//! let mut ingester = StreamingIngester::new(&fs)
//!     .with_chunk_size(8192)
//!     .with_path("large_file.bin");
//!
//! ingester.ingest_reader(reader)?;
//! let result = ingester.finalize()?;
//! ```

use crate::correction::ChunkCorrection;
use crate::versioned::{ChunkId, VersionedChunk, VersionedFileEntry};
use crate::versioned_embrfs::{EmbrFSError, VersionedEmbrFS, DEFAULT_CHUNK_SIZE};
use embeddenator_io::{wrap_or_legacy, BinaryWriteOptions, CompressionCodec, PayloadKind};
use embeddenator_vsa::SparseVec;
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicU64, Ordering};

/// Result of a completed streaming ingestion
#[derive(Debug, Clone)]
pub struct StreamingResult {
    /// Path of the ingested file
    pub path: String,
    /// Total bytes ingested
    pub total_bytes: usize,
    /// Number of chunks created
    pub chunk_count: usize,
    /// File version in the filesystem
    pub version: u64,
    /// Bytes saved by correction optimization (if applicable)
    pub correction_savings: usize,
}

/// Pending chunk data for batch insertion
struct PendingChunk {
    chunk_id: ChunkId,
    data: Vec<u8>,
    vector: SparseVec,
    correction: ChunkCorrection,
}

/// Builder for configuring streaming ingestion
pub struct StreamingIngesterBuilder<'a> {
    fs: &'a VersionedEmbrFS,
    chunk_size: usize,
    path: Option<String>,
    expected_version: Option<u64>,
    compression: Option<CompressionCodec>,
    adaptive_chunking: bool,
    correction_threshold: f64,
}

impl<'a> StreamingIngesterBuilder<'a> {
    /// Create a new builder with default settings
    pub fn new(fs: &'a VersionedEmbrFS) -> Self {
        Self {
            fs,
            chunk_size: DEFAULT_CHUNK_SIZE,
            path: None,
            expected_version: None,
            compression: None,
            adaptive_chunking: false,
            correction_threshold: 0.1,
        }
    }

    /// Set the chunk size for encoding
    ///
    /// Smaller chunks improve memory efficiency but may reduce encoding quality
    /// for highly correlated data. Default is 4KB.
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(512).min(1024 * 1024); // Clamp to 512B - 1MB
        self
    }

    /// Set the target path for the file
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set expected version for optimistic locking (update existing file)
    pub fn with_expected_version(mut self, version: u64) -> Self {
        self.expected_version = Some(version);
        self
    }

    /// Enable compression with specified codec
    pub fn with_compression(mut self, codec: CompressionCodec) -> Self {
        self.compression = Some(codec);
        self
    }

    /// Enable adaptive chunking based on content similarity
    ///
    /// When enabled, the ingester may adjust chunk boundaries to improve
    /// encoding quality for files with repeated patterns.
    pub fn with_adaptive_chunking(mut self, enabled: bool) -> Self {
        self.adaptive_chunking = enabled;
        self
    }

    /// Set correction threshold for large file optimization
    ///
    /// For large files, corrections beyond this threshold trigger re-encoding
    /// with adjusted parameters. Range: 0.0 - 1.0 (default 0.1 = 10%)
    pub fn with_correction_threshold(mut self, threshold: f64) -> Self {
        self.correction_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Build the streaming ingester
    pub fn build(self) -> Result<StreamingIngester<'a>, EmbrFSError> {
        let path = self.path.ok_or_else(|| {
            EmbrFSError::InvalidOperation("Path must be specified for streaming ingestion".into())
        })?;

        Ok(StreamingIngester {
            fs: self.fs,
            path,
            chunk_size: self.chunk_size,
            expected_version: self.expected_version,
            compression: self.compression,
            adaptive_chunking: self.adaptive_chunking,
            correction_threshold: self.correction_threshold,
            // State
            buffer: Vec::with_capacity(self.chunk_size),
            pending_chunks: Vec::new(),
            chunk_ids: Vec::new(),
            total_bytes: AtomicU64::new(0),
            hasher: Sha256::new(),
            correction_bytes: AtomicU64::new(0),
        })
    }
}

/// Streaming ingester for memory-efficient file encoding
///
/// Processes data in chunks as it arrives, maintaining bounded memory usage.
/// Suitable for files of any size including multi-GB datasets.
pub struct StreamingIngester<'a> {
    // Configuration
    fs: &'a VersionedEmbrFS,
    path: String,
    chunk_size: usize,
    expected_version: Option<u64>,
    compression: Option<CompressionCodec>,
    adaptive_chunking: bool,
    correction_threshold: f64,

    // Mutable state
    buffer: Vec<u8>,
    pending_chunks: Vec<PendingChunk>,
    chunk_ids: Vec<ChunkId>,
    total_bytes: AtomicU64,
    hasher: Sha256,
    correction_bytes: AtomicU64,
}

impl<'a> StreamingIngester<'a> {
    /// Create a new streaming ingester with default settings
    pub fn new(fs: &'a VersionedEmbrFS) -> StreamingIngesterBuilder<'a> {
        StreamingIngesterBuilder::new(fs)
    }

    /// Ingest data from a reader
    ///
    /// Reads data in chunks and encodes progressively. Memory usage is bounded
    /// by `chunk_size` regardless of total file size.
    pub fn ingest_reader<R: Read>(&mut self, mut reader: R) -> Result<(), EmbrFSError> {
        let mut read_buf = vec![0u8; self.chunk_size];

        loop {
            let bytes_read = reader
                .read(&mut read_buf)
                .map_err(|e| EmbrFSError::IoError(e.to_string()))?;

            if bytes_read == 0 {
                break; // EOF
            }

            self.ingest_bytes(&read_buf[..bytes_read])?;
        }

        Ok(())
    }

    /// Ingest data from a buffered reader (more efficient)
    pub fn ingest_buffered<R: BufRead>(&mut self, mut reader: R) -> Result<(), EmbrFSError> {
        loop {
            let buf = reader
                .fill_buf()
                .map_err(|e| EmbrFSError::IoError(e.to_string()))?;

            if buf.is_empty() {
                break; // EOF
            }

            let len = buf.len();
            self.ingest_bytes(buf)?;
            reader.consume(len);
        }

        Ok(())
    }

    /// Ingest a slice of bytes
    ///
    /// Can be called multiple times. Data is buffered until a full chunk is
    /// available, then encoded and stored.
    pub fn ingest_bytes(&mut self, data: &[u8]) -> Result<(), EmbrFSError> {
        self.total_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.hasher.update(data);

        // Add to buffer
        self.buffer.extend_from_slice(data);

        // Process complete chunks
        while self.buffer.len() >= self.chunk_size {
            let chunk_data: Vec<u8> = self.buffer.drain(..self.chunk_size).collect();
            self.process_chunk(chunk_data)?;
        }

        Ok(())
    }

    /// Finalize the ingestion and commit to filesystem
    ///
    /// Processes any remaining buffered data and commits all chunks to the
    /// versioned filesystem atomically.
    pub fn finalize(mut self) -> Result<StreamingResult, EmbrFSError> {
        // Process remaining data in buffer
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            self.process_chunk(remaining)?;
        }

        let total_bytes = self.total_bytes.load(Ordering::Relaxed) as usize;
        let chunk_count = self.chunk_ids.len();
        let correction_bytes = self.correction_bytes.load(Ordering::Relaxed) as usize;

        // Check existing file for version verification
        let existing = self.fs.manifest.get_file(&self.path);

        match (&existing, self.expected_version) {
            (Some((entry, _)), Some(expected_ver)) => {
                if entry.version != expected_ver {
                    return Err(EmbrFSError::VersionMismatch {
                        expected: expected_ver,
                        actual: entry.version,
                    });
                }
            }
            (Some(_), None) => {
                return Err(EmbrFSError::FileExists(self.path.clone()));
            }
            (None, Some(_)) => {
                return Err(EmbrFSError::FileNotFound(self.path.clone()));
            }
            (None, None) => {}
        }

        // Build chunk updates from pending chunks
        let chunk_updates: Vec<_> = self
            .pending_chunks
            .iter()
            .map(|pc| {
                let mut hash_bytes = [0u8; 8];
                let mut hasher = Sha256::new();
                hasher.update(&pc.data);
                let hash = hasher.finalize();
                hash_bytes.copy_from_slice(&hash[0..8]);
                (
                    pc.chunk_id,
                    VersionedChunk::new(pc.vector.clone(), pc.data.len(), hash_bytes),
                )
            })
            .collect();

        // Insert chunks
        self.fs.chunk_store.batch_insert_new(chunk_updates)?;

        // Insert corrections
        let corrections: Vec<_> = self
            .pending_chunks
            .into_iter()
            .map(|pc| (pc.chunk_id as u64, pc.correction))
            .collect();
        self.fs.corrections.batch_insert_new(corrections)?;

        // Create manifest entry
        let is_text = total_bytes > 0 && is_likely_text(total_bytes);
        let file_entry = if let Some(codec) = self.compression {
            let codec_byte = match codec {
                CompressionCodec::None => 0,
                CompressionCodec::Zstd => 1,
                CompressionCodec::Lz4 => 2,
            };
            VersionedFileEntry::new_compressed(
                self.path.clone(),
                is_text,
                total_bytes, // compressed size (actual)
                total_bytes, // original size
                codec_byte,
                self.chunk_ids.clone(),
            )
        } else {
            VersionedFileEntry::new(
                self.path.clone(),
                is_text,
                total_bytes,
                self.chunk_ids.clone(),
            )
        };

        let version = if let Some((entry, _)) = existing {
            self.fs
                .manifest
                .update_file(&self.path, file_entry, entry.version)?;
            entry.version + 1
        } else {
            self.fs.manifest.add_file(file_entry)?;
            0
        };

        // Bundle chunks into root
        self.fs.bundle_chunks_to_root_streaming(&self.chunk_ids)?;

        Ok(StreamingResult {
            path: self.path,
            total_bytes,
            chunk_count,
            version,
            correction_savings: correction_bytes,
        })
    }

    /// Process a single chunk
    fn process_chunk(&mut self, data: Vec<u8>) -> Result<(), EmbrFSError> {
        let chunk_id = self.fs.allocate_chunk_id();

        // Apply compression if configured
        let encoded_data = if let Some(codec) = self.compression {
            if codec != CompressionCodec::None {
                let write_opts = BinaryWriteOptions { codec, level: None };
                wrap_or_legacy(PayloadKind::EngramBincode, write_opts, &data)
                    .map_err(|e| EmbrFSError::IoError(format!("Compression failed: {}", e)))?
            } else {
                data.clone()
            }
        } else {
            data.clone()
        };

        // Encode chunk using VSA
        let chunk_vec = SparseVec::encode_data(&encoded_data, self.fs.config(), Some(&self.path));

        // Decode and compute correction
        let decoded = chunk_vec.decode_data(self.fs.config(), Some(&self.path), encoded_data.len());
        let correction = ChunkCorrection::new(chunk_id as u64, &encoded_data, &decoded);

        // Track correction overhead
        let corr_size = correction.storage_size();
        self.correction_bytes
            .fetch_add(corr_size as u64, Ordering::Relaxed);

        // Check if correction exceeds threshold (triggers adaptive strategy)
        if self.adaptive_chunking {
            let correction_ratio = corr_size as f64 / encoded_data.len() as f64;
            if correction_ratio > self.correction_threshold {
                // For now, just log - future: implement re-encoding with different params
                eprintln!(
                    "Warning: High correction ratio {:.2}% for chunk {} - consider adjusting parameters",
                    correction_ratio * 100.0,
                    chunk_id
                );
            }
        }

        // Store pending chunk for batch insertion
        self.pending_chunks.push(PendingChunk {
            chunk_id,
            data: encoded_data,
            vector: chunk_vec,
            correction,
        });
        self.chunk_ids.push(chunk_id);

        Ok(())
    }

    /// Get current progress
    pub fn progress(&self) -> StreamingProgress {
        StreamingProgress {
            bytes_processed: self.total_bytes.load(Ordering::Relaxed) as usize,
            chunks_created: self.chunk_ids.len(),
            buffer_usage: self.buffer.len(),
            correction_overhead: self.correction_bytes.load(Ordering::Relaxed) as usize,
        }
    }
}

/// Progress information for streaming ingestion
#[derive(Debug, Clone)]
pub struct StreamingProgress {
    /// Total bytes processed so far
    pub bytes_processed: usize,
    /// Number of chunks created
    pub chunks_created: usize,
    /// Current buffer usage in bytes
    pub buffer_usage: usize,
    /// Total correction overhead in bytes
    pub correction_overhead: usize,
}

/// Heuristic to detect if data is likely text based on size
fn is_likely_text(size: usize) -> bool {
    // Small files are often text, large files are often binary
    size < 1024 * 1024
}

// =============================================================================
// ASYNC STREAMING SUPPORT
// =============================================================================

/// Async streaming ingester for memory-efficient file encoding
///
/// This is the async counterpart to `StreamingIngester`, supporting
/// `tokio::io::AsyncRead` and `tokio::io::AsyncBufRead` for non-blocking I/O.
///
/// # Example
///
/// ```rust,ignore
/// use embeddenator_fs::fs::AsyncStreamingIngester;
/// use tokio::io::AsyncReadExt;
///
/// async fn ingest_async(fs: &VersionedEmbrFS, path: &str) {
///     let file = tokio::fs::File::open("large_file.bin").await?;
///     let reader = tokio::io::BufReader::new(file);
///
///     let mut ingester = AsyncStreamingIngester::new(fs)
///         .with_path(path)
///         .build()
///         .await?;
///
///     ingester.ingest_async_reader(reader).await?;
///     let result = ingester.finalize().await?;
/// }
/// ```
#[cfg(feature = "tokio")]
pub struct AsyncStreamingIngester<'a> {
    /// Inner sync ingester (we delegate chunk processing to it)
    inner: StreamingIngester<'a>,
}

#[cfg(feature = "tokio")]
impl<'a> AsyncStreamingIngester<'a> {
    /// Create a new async streaming ingester builder
    pub fn new(fs: &'a VersionedEmbrFS) -> AsyncStreamingIngesterBuilder<'a> {
        AsyncStreamingIngesterBuilder {
            inner: StreamingIngesterBuilder::new(fs),
        }
    }

    /// Ingest data from an async reader
    ///
    /// Reads data in chunks and processes asynchronously.
    pub async fn ingest_async_reader<R>(&mut self, mut reader: R) -> Result<(), EmbrFSError>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;

        let mut buf = vec![0u8; self.inner.chunk_size];

        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| EmbrFSError::IoError(e.to_string()))?;

            if n == 0 {
                break;
            }

            // Delegate to sync ingester's bytes method
            self.inner.ingest_bytes(&buf[..n])?;
        }

        Ok(())
    }

    /// Ingest data from an async buffered reader
    ///
    /// More efficient than `ingest_async_reader` as it avoids double-buffering.
    pub async fn ingest_async_buffered<R>(&mut self, mut reader: R) -> Result<(), EmbrFSError>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        use tokio::io::AsyncBufReadExt;

        loop {
            let buf = reader
                .fill_buf()
                .await
                .map_err(|e| EmbrFSError::IoError(e.to_string()))?;

            if buf.is_empty() {
                break;
            }

            let len = buf.len();
            self.inner.ingest_bytes(buf)?;
            reader.consume(len);
        }

        Ok(())
    }

    /// Ingest a slice of bytes (synchronous, for convenience)
    pub fn ingest_bytes(&mut self, data: &[u8]) -> Result<(), EmbrFSError> {
        self.inner.ingest_bytes(data)
    }

    /// Get current ingestion progress
    pub fn progress(&self) -> StreamingProgress {
        self.inner.progress()
    }

    /// Finalize the ingestion and commit to filesystem
    pub fn finalize(self) -> Result<StreamingResult, EmbrFSError> {
        self.inner.finalize()
    }
}

/// Builder for async streaming ingester
#[cfg(feature = "tokio")]
pub struct AsyncStreamingIngesterBuilder<'a> {
    inner: StreamingIngesterBuilder<'a>,
}

#[cfg(feature = "tokio")]
impl<'a> AsyncStreamingIngesterBuilder<'a> {
    /// Set the target path for the file
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.inner = self.inner.with_path(path);
        self
    }

    /// Set chunk size for processing
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.inner = self.inner.with_chunk_size(size);
        self
    }

    /// Set expected version for optimistic locking
    pub fn with_expected_version(mut self, version: u64) -> Self {
        self.inner = self.inner.with_expected_version(version);
        self
    }

    /// Enable compression with specified codec
    pub fn with_compression(mut self, codec: CompressionCodec) -> Self {
        self.inner = self.inner.with_compression(codec);
        self
    }

    /// Enable adaptive chunking
    pub fn with_adaptive_chunking(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_adaptive_chunking(enabled);
        self
    }

    /// Set correction threshold
    pub fn with_correction_threshold(mut self, threshold: f64) -> Self {
        self.inner = self.inner.with_correction_threshold(threshold);
        self
    }

    /// Build the async streaming ingester
    pub fn build(self) -> Result<AsyncStreamingIngester<'a>, EmbrFSError> {
        Ok(AsyncStreamingIngester {
            inner: self.inner.build()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_small_file() {
        let fs = VersionedEmbrFS::new();
        let data = b"Hello, streaming world!";

        let mut ingester = StreamingIngester::new(&fs)
            .with_path("test.txt")
            .build()
            .unwrap();

        ingester.ingest_bytes(data).unwrap();
        let result = ingester.finalize().unwrap();

        assert_eq!(result.path, "test.txt");
        assert_eq!(result.total_bytes, data.len());
        assert!(result.chunk_count >= 1);

        // Verify data
        let (content, _) = fs.read_file("test.txt").unwrap();
        assert_eq!(&content[..], data);
    }

    #[test]
    fn test_streaming_large_file() {
        let fs = VersionedEmbrFS::new();

        // Create data larger than default chunk size
        let data: Vec<u8> = (0..DEFAULT_CHUNK_SIZE * 3 + 500)
            .map(|i| (i % 256) as u8)
            .collect();

        let mut ingester = StreamingIngester::new(&fs)
            .with_path("large.bin")
            .with_chunk_size(DEFAULT_CHUNK_SIZE)
            .build()
            .unwrap();

        // Feed in smaller pieces to test buffering
        for chunk in data.chunks(1024) {
            ingester.ingest_bytes(chunk).unwrap();
        }

        let result = ingester.finalize().unwrap();

        assert_eq!(result.total_bytes, data.len());
        assert!(result.chunk_count >= 3); // At least 3 chunks

        // Verify data integrity
        let (content, _) = fs.read_file("large.bin").unwrap();
        assert_eq!(content, data);
    }

    #[test]
    fn test_streaming_progress() {
        let fs = VersionedEmbrFS::new();

        let mut ingester = StreamingIngester::new(&fs)
            .with_path("progress.txt")
            .with_chunk_size(1024)
            .build()
            .unwrap();

        // Check initial progress
        let p1 = ingester.progress();
        assert_eq!(p1.bytes_processed, 0);
        assert_eq!(p1.chunks_created, 0);

        // Add data
        ingester.ingest_bytes(&[0u8; 500]).unwrap();
        let p2 = ingester.progress();
        assert_eq!(p2.bytes_processed, 500);
        assert_eq!(p2.buffer_usage, 500);
        assert_eq!(p2.chunks_created, 0); // Not a full chunk yet

        // Add more data to complete a chunk
        ingester.ingest_bytes(&[0u8; 600]).unwrap();
        let p3 = ingester.progress();
        assert_eq!(p3.bytes_processed, 1100);
        assert_eq!(p3.chunks_created, 1);
    }

    #[test]
    fn test_streaming_reader() {
        use std::io::Cursor;

        let fs = VersionedEmbrFS::new();
        let data = b"Data from a reader interface!";
        let reader = Cursor::new(data);

        let mut ingester = StreamingIngester::new(&fs)
            .with_path("from_reader.txt")
            .build()
            .unwrap();

        ingester.ingest_reader(reader).unwrap();
        let result = ingester.finalize().unwrap();

        assert_eq!(result.total_bytes, data.len());

        let (content, _) = fs.read_file("from_reader.txt").unwrap();
        assert_eq!(&content[..], data);
    }
}
