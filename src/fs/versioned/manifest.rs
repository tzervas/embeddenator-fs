//! Versioned Manifest with File-Level Versioning
//! ==============================================
//!
//! The manifest is the "table of contents" for an engram - it stores file metadata
//! with per-file version tracking for concurrent access control.
//!
//! # Why a Manifest?
//!
//! An engram stores file data as chunks in a content-addressed store. The manifest
//! provides the mapping from file paths to chunks, along with all the metadata
//! needed to reconstruct the original filesystem:
//!
//! - **Path → Chunks**: Which chunks make up each file
//! - **Metadata**: Permissions, timestamps, ownership
//! - **File Types**: Regular, symlink, device, etc.
//! - **Versioning**: Concurrent modification detection
//!
//! Without the manifest, chunks would be meaningless binary blobs.
//!
//! # Why Per-File Versioning?
//!
//! Traditional locking (mutexes, reader-writer locks) has problems:
//!
//! - **Coarse locking**: Locking the whole manifest blocks all operations
//! - **Fine locking**: Locking per-file requires many locks (memory/complexity)
//! - **Deadlocks**: Multiple locks risk deadlock situations
//!
//! We use **optimistic concurrency control** instead:
//!
//! 1. Read file entry with its current version
//! 2. Make modifications to your local copy
//! 3. Write back with expected version
//! 4. If version changed, someone else modified it - retry
//!
//! This approach:
//! - **No locks held during computation**: Only brief atomic operations
//! - **Scales with concurrency**: No contention on different files
//! - **Simple recovery**: No lock state to recover after crash
//! - **Good for reads**: Reads never block, always see consistent snapshot
//!
//! The cost is occasional retries when conflicts occur, but conflicts are rare
//! in most workloads (different files accessed concurrently).
//!
//! # Why These File Types?
//!
//! Unix filesystems have 7 file types, and we support them all:
//!
//! | Type | Why It Exists | Our Handling |
//! |------|--------------|--------------|
//! | Regular | Normal files | Chunks contain file data |
//! | Directory | Organize files | Metadata only (listing is a query) |
//! | Symlink | Flexible references | Store target path, no data |
//! | Hardlink | Multiple names for same file | Store target path, shares chunks |
//! | CharDevice | Unbuffered I/O (terminal) | Usually metadata-only |
//! | BlockDevice | Buffered I/O (disk) | Option C: can encode data |
//! | FIFO | Inter-process communication | Metadata only |
//! | Socket | Network-style local IPC | Metadata only |
//!
//! # Option C: Device Data Encoding
//!
//! Block devices are special. Most devices in `/dev` are hardware interfaces
//! with no persistent data. But some block devices ARE persistent data:
//!
//! - `/dev/loop0` - Loop device with disk image
//! - `/dev/nbd0` - Network block device
//! - `/dev/mapper/encrypted` - LUKS encrypted volume
//!
//! "Option C" encoding captures this data:
//!
//! ```text
//! Option A: Metadata only (default for most devices)
//!   /dev/sda → {type: block, major: 8, minor: 0, chunks: []}
//!
//! Option B: Full device encoding (explicit request)
//!   /dev/loop0 → {type: block, major: 7, minor: 0, chunks: [c1, c2, ...]}
//!
//! Option C: Compressed device encoding (for large devices)
//!   /dev/loop0 → {type: block, compressed: zstd, chunks: [c1, c2, ...]}
//! ```
//!
//! # Why Compression Tracking in Manifest?
//!
//! Compression is stored per-file rather than per-chunk because:
//!
//! 1. **Different files compress differently**: Text vs binary vs already-compressed
//! 2. **User preferences**: Some files shouldn't be compressed (already compressed)
//! 3. **Streaming decode**: Know decompressor before reading first chunk
//! 4. **Size validation**: Track uncompressed size for progress/validation
//!
//! # Manifest Structure
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │ Manifest                                                    │
//! ├─────────────────────────────────────────────────────────────┤
//! │ files: HashMap<String, VersionedFileEntry>                  │
//! │   └─ Path → Entry with version tracking                    │
//! │ global_version: AtomicU64                                   │
//! │   └─ Monotonic counter for version assignment              │
//! │ path_index: HashMap<String, Vec<String>>                    │
//! │   └─ Directory → Children for fast listing                 │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! The path index exists because directory listing is O(n) without it.
//! With the index, listing a directory is O(children) regardless of total files.

use super::types::{ChunkId, VersionMismatch, VersionedResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// File type for special file handling
///
/// # Option C: Device Encoding
///
/// Block devices can optionally have their content encoded (Option C).
/// This is useful for:
/// - Loop devices with actual disk images
/// - VM disk images mounted as block devices
/// - Partition backups
///
/// For most devices in /dev, the chunks will be empty (metadata-only).
/// Use `new_device_with_data` or `new_device_compressed` for Option C encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum FileType {
    /// Regular file (default)
    #[default]
    Regular = 0,
    /// Directory
    Directory = 1,
    /// Symbolic link (target stored in symlink_target field)
    Symlink = 2,
    /// Hard link (target stored in hardlink_target field)
    Hardlink = 3,
    /// Character device (typically metadata-only, but can have data with Option C)
    CharDevice = 4,
    /// Block device (typically metadata-only, but can have data with Option C)
    BlockDevice = 5,
    /// FIFO/named pipe (metadata only)
    Fifo = 6,
    /// Unix socket (metadata only)
    Socket = 7,
}

impl FileType {
    /// Returns true if this file type has actual data content
    pub fn has_content(&self) -> bool {
        matches!(self, FileType::Regular)
    }

    /// Returns true if this file type CAN have data content (Option C devices)
    pub fn can_have_content(&self) -> bool {
        matches!(
            self,
            FileType::Regular | FileType::CharDevice | FileType::BlockDevice
        )
    }

    /// Returns true if this file type is always metadata-only (no data possible)
    pub fn is_metadata_only(&self) -> bool {
        matches!(self, FileType::Fifo | FileType::Socket)
    }

    /// Returns true if this is a link type
    pub fn is_link(&self) -> bool {
        matches!(self, FileType::Symlink | FileType::Hardlink)
    }

    /// Returns true if this is a device node
    pub fn is_device(&self) -> bool {
        matches!(self, FileType::CharDevice | FileType::BlockDevice)
    }
}

/// Unix file permissions and ownership
#[derive(Debug, Clone, Copy, Default)]
pub struct FilePermissions {
    /// User ID (owner)
    pub uid: u32,
    /// Group ID
    pub gid: u32,
    /// File mode (permission bits + special bits like setuid/setgid/sticky)
    pub mode: u32,
}

impl FilePermissions {
    /// Create new permissions with specified uid, gid, and mode
    pub fn new(uid: u32, gid: u32, mode: u32) -> Self {
        Self { uid, gid, mode }
    }

    /// Default permissions for a regular file (0644, root:root)
    pub fn default_file() -> Self {
        Self {
            uid: 0,
            gid: 0,
            mode: 0o644,
        }
    }

    /// Default permissions for a directory (0755, root:root)
    pub fn default_dir() -> Self {
        Self {
            uid: 0,
            gid: 0,
            mode: 0o755,
        }
    }

    /// Check if setuid bit is set
    pub fn is_setuid(&self) -> bool {
        self.mode & 0o4000 != 0
    }

    /// Check if setgid bit is set
    pub fn is_setgid(&self) -> bool {
        self.mode & 0o2000 != 0
    }

    /// Check if sticky bit is set
    pub fn is_sticky(&self) -> bool {
        self.mode & 0o1000 != 0
    }
}

/// A versioned file entry in the manifest
#[derive(Debug, Clone)]
pub struct VersionedFileEntry {
    /// File path (relative to engram root)
    pub path: String,

    /// Type of file (regular, symlink, device, etc.)
    pub file_type: FileType,

    /// Unix permissions (uid, gid, mode)
    pub permissions: FilePermissions,

    /// Symlink target path (only for FileType::Symlink)
    pub symlink_target: Option<String>,

    /// Hardlink target path (only for FileType::Hardlink)
    pub hardlink_target: Option<String>,

    /// Device major/minor numbers (only for char/block devices)
    pub device_id: Option<(u32, u32)>,

    /// Is this a text file? (affects encoding strategy)
    pub is_text: bool,

    /// Size of the file in bytes
    pub size: usize,

    /// List of chunk IDs that make up this file
    pub chunks: Vec<ChunkId>,

    /// Is this file marked as deleted? (soft delete)
    pub deleted: bool,

    /// Compression codec used (0=None, 1=Zstd, 2=Lz4)
    /// None means no compression (backward compatible)
    pub compression_codec: Option<u8>,

    /// Original uncompressed size (for compressed files)
    pub uncompressed_size: Option<usize>,

    /// Encoding format version for holographic storage
    /// 0 = Legacy (Codebook.project() - ~0% accuracy)
    /// 1 = ReversibleVSA (ReversibleVSAEncoder - ~94% accuracy)
    /// None = standard VSA encoding (not holographic)
    pub encoding_format: Option<u8>,

    /// Version number of this file entry
    pub version: u64,

    /// When this file was created
    pub created_at: Instant,

    /// When this file was last modified
    pub modified_at: Instant,
}

impl VersionedFileEntry {
    /// Create a new file entry for a regular file
    pub fn new(path: String, is_text: bool, size: usize, chunks: Vec<ChunkId>) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: FileType::Regular,
            permissions: FilePermissions::default_file(),
            symlink_target: None,
            hardlink_target: None,
            device_id: None,
            is_text,
            size,
            chunks,
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a new file entry with full metadata
    pub fn new_with_metadata(
        path: String,
        file_type: FileType,
        permissions: FilePermissions,
        is_text: bool,
        size: usize,
        chunks: Vec<ChunkId>,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type,
            permissions,
            symlink_target: None,
            hardlink_target: None,
            device_id: None,
            is_text,
            size,
            chunks,
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a symlink entry
    pub fn new_symlink(path: String, target: String, permissions: FilePermissions) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: FileType::Symlink,
            permissions,
            symlink_target: Some(target),
            hardlink_target: None,
            device_id: None,
            is_text: false,
            size: 0,
            chunks: Vec::new(),
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a hardlink entry
    pub fn new_hardlink(path: String, target: String, permissions: FilePermissions) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: FileType::Hardlink,
            permissions,
            symlink_target: None,
            hardlink_target: Some(target),
            device_id: None,
            is_text: false,
            size: 0,
            chunks: Vec::new(),
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a device node entry (Option C: with encoded data)
    ///
    /// Option C encodes actual device data for block devices (e.g., loop devices).
    /// For most devices in /dev, this will be empty. For block devices with
    /// actual content (like a disk image mounted as a loop device), this allows
    /// encoding the full device content.
    ///
    /// # Arguments
    ///
    /// * `path` - Device path (e.g., "/dev/loop0")
    /// * `is_char` - true for character device, false for block device
    /// * `major` - Major device number
    /// * `minor` - Minor device number
    /// * `permissions` - Unix permissions
    /// * `size` - Size of device data (0 for metadata-only)
    /// * `chunks` - Chunk IDs for device data (empty for metadata-only)
    pub fn new_device(
        path: String,
        is_char: bool,
        major: u32,
        minor: u32,
        permissions: FilePermissions,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: if is_char {
                FileType::CharDevice
            } else {
                FileType::BlockDevice
            },
            permissions,
            symlink_target: None,
            hardlink_target: None,
            device_id: Some((major, minor)),
            is_text: false,
            size: 0,
            chunks: Vec::new(),
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a device node entry with data (Option C full encoding)
    ///
    /// Use this for block devices that have actual content to encode,
    /// such as loop devices or disk images.
    pub fn new_device_with_data(
        path: String,
        is_char: bool,
        major: u32,
        minor: u32,
        permissions: FilePermissions,
        size: usize,
        chunks: Vec<ChunkId>,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: if is_char {
                FileType::CharDevice
            } else {
                FileType::BlockDevice
            },
            permissions,
            symlink_target: None,
            hardlink_target: None,
            device_id: Some((major, minor)),
            is_text: false,
            size,
            chunks,
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a device node with compressed data (Option C)
    #[allow(clippy::too_many_arguments)]
    pub fn new_device_compressed(
        path: String,
        is_char: bool,
        major: u32,
        minor: u32,
        permissions: FilePermissions,
        compressed_size: usize,
        uncompressed_size: usize,
        compression_codec: u8,
        chunks: Vec<ChunkId>,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: if is_char {
                FileType::CharDevice
            } else {
                FileType::BlockDevice
            },
            permissions,
            symlink_target: None,
            hardlink_target: None,
            device_id: Some((major, minor)),
            is_text: false,
            size: compressed_size,
            chunks,
            deleted: false,
            compression_codec: Some(compression_codec),
            uncompressed_size: Some(uncompressed_size),
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a special file entry (FIFO or socket - metadata only)
    pub fn new_special(path: String, file_type: FileType, permissions: FilePermissions) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type,
            permissions,
            symlink_target: None,
            hardlink_target: None,
            device_id: None,
            is_text: false,
            size: 0,
            chunks: Vec::new(),
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a new file entry with compression metadata
    pub fn new_compressed(
        path: String,
        is_text: bool,
        compressed_size: usize,
        uncompressed_size: usize,
        compression_codec: u8,
        chunks: Vec<ChunkId>,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: FileType::Regular,
            permissions: FilePermissions::default_file(),
            symlink_target: None,
            hardlink_target: None,
            device_id: None,
            is_text,
            size: compressed_size,
            chunks,
            deleted: false,
            compression_codec: Some(compression_codec),
            uncompressed_size: Some(uncompressed_size),
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create a new file entry with compression and full metadata
    pub fn new_compressed_with_metadata(
        path: String,
        permissions: FilePermissions,
        is_text: bool,
        compressed_size: usize,
        uncompressed_size: usize,
        compression_codec: u8,
        chunks: Vec<ChunkId>,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: FileType::Regular,
            permissions,
            symlink_target: None,
            hardlink_target: None,
            device_id: None,
            is_text,
            size: compressed_size,
            chunks,
            deleted: false,
            compression_codec: Some(compression_codec),
            uncompressed_size: Some(uncompressed_size),
            encoding_format: None,
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create an updated version of this file entry
    pub fn update(&self, new_chunks: Vec<ChunkId>, new_size: usize) -> Self {
        Self {
            path: self.path.clone(),
            file_type: self.file_type,
            permissions: self.permissions,
            symlink_target: self.symlink_target.clone(),
            hardlink_target: self.hardlink_target.clone(),
            device_id: self.device_id,
            is_text: self.is_text,
            size: new_size,
            chunks: new_chunks,
            deleted: false,
            compression_codec: self.compression_codec,
            uncompressed_size: self.uncompressed_size,
            encoding_format: self.encoding_format,
            version: self.version + 1,
            created_at: self.created_at,
            modified_at: Instant::now(),
        }
    }

    /// Create an updated version with new compression settings
    pub fn update_compressed(
        &self,
        new_chunks: Vec<ChunkId>,
        compressed_size: usize,
        uncompressed_size: usize,
        compression_codec: u8,
    ) -> Self {
        Self {
            path: self.path.clone(),
            file_type: self.file_type,
            permissions: self.permissions,
            symlink_target: self.symlink_target.clone(),
            hardlink_target: self.hardlink_target.clone(),
            device_id: self.device_id,
            is_text: self.is_text,
            size: compressed_size,
            chunks: new_chunks,
            deleted: false,
            compression_codec: Some(compression_codec),
            uncompressed_size: Some(uncompressed_size),
            encoding_format: self.encoding_format,
            version: self.version + 1,
            created_at: self.created_at,
            modified_at: Instant::now(),
        }
    }

    /// Mark this file as deleted
    pub fn mark_deleted(&self) -> Self {
        let mut updated = self.clone();
        updated.deleted = true;
        updated.version += 1;
        updated.modified_at = Instant::now();
        updated
    }

    /// Create a new file entry for holographic storage with encoding format
    pub fn new_holographic(
        path: String,
        is_text: bool,
        size: usize,
        chunks: Vec<ChunkId>,
        encoding_format: u8,
    ) -> Self {
        let now = Instant::now();
        Self {
            path,
            file_type: FileType::Regular,
            permissions: FilePermissions::default_file(),
            symlink_target: None,
            hardlink_target: None,
            device_id: None,
            is_text,
            size,
            chunks,
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
            encoding_format: Some(encoding_format),
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Check if this entry represents a regular file with content
    pub fn is_regular_file(&self) -> bool {
        self.file_type == FileType::Regular
    }

    /// Check if this entry is metadata-only (no data to encode)
    pub fn is_metadata_only(&self) -> bool {
        self.file_type.is_metadata_only()
    }
}

/// A versioned manifest with per-file locking
///
/// The manifest maintains a list of file entries with a global version number.
/// Each file entry also has its own version for fine-grained optimistic locking.
pub struct VersionedManifest {
    /// List of file entries (protected by RwLock)
    files: Arc<RwLock<Vec<VersionedFileEntry>>>,

    /// Index mapping file path to index in files vector
    file_index: Arc<RwLock<HashMap<String, usize>>>,

    /// Global version for the entire manifest
    global_version: Arc<AtomicU64>,

    /// Total number of chunks across all files
    total_chunks: Arc<AtomicU64>,
}

impl VersionedManifest {
    /// Create a new empty manifest
    pub fn new() -> Self {
        Self {
            files: Arc::new(RwLock::new(Vec::new())),
            file_index: Arc::new(RwLock::new(HashMap::new())),
            global_version: Arc::new(AtomicU64::new(0)),
            total_chunks: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the current global version
    pub fn version(&self) -> u64 {
        self.global_version.load(Ordering::Acquire)
    }

    /// Get a file entry by path
    ///
    /// Returns the file entry and the global manifest version at read time.
    pub fn get_file(&self, path: &str) -> Option<(VersionedFileEntry, u64)> {
        let files = self.files.read().unwrap();
        let index = self.file_index.read().unwrap();
        let version = self.version();

        index
            .get(path)
            .and_then(|&idx| files.get(idx))
            .map(|entry| (entry.clone(), version))
    }

    /// Check if a file exists
    pub fn contains(&self, path: &str) -> bool {
        self.file_index.read().unwrap().contains_key(path)
    }

    /// Add a new file to the manifest
    ///
    /// Returns an error if the file already exists.
    pub fn add_file(&self, entry: VersionedFileEntry) -> VersionedResult<u64> {
        let mut files = self.files.write().unwrap();
        let mut index = self.file_index.write().unwrap();

        // Check if file already exists
        if index.contains_key(&entry.path) {
            return Err(VersionMismatch {
                expected: 0,
                actual: self.version(),
            });
        }

        // Add to files vector
        let idx = files.len();
        let chunk_count = entry.chunks.len() as u64;
        files.push(entry.clone());
        index.insert(entry.path.clone(), idx);

        // Update counters
        self.total_chunks.fetch_add(chunk_count, Ordering::AcqRel);
        let new_version = self.global_version.fetch_add(1, Ordering::AcqRel) + 1;

        Ok(new_version)
    }

    /// Update an existing file entry
    ///
    /// Verifies the expected file version before updating.
    pub fn update_file(
        &self,
        path: &str,
        new_entry: VersionedFileEntry,
        expected_file_version: u64,
    ) -> VersionedResult<u64> {
        let mut files = self.files.write().unwrap();
        let index = self.file_index.read().unwrap();

        // Find file index
        let &idx = index.get(path).ok_or(VersionMismatch {
            expected: expected_file_version,
            actual: 0,
        })?;

        // Check file version
        let current_entry = &files[idx];
        if current_entry.version != expected_file_version {
            return Err(VersionMismatch {
                expected: expected_file_version,
                actual: current_entry.version,
            });
        }

        // Update chunk count
        let old_chunks = current_entry.chunks.len() as u64;
        let new_chunks = new_entry.chunks.len() as u64;
        if new_chunks >= old_chunks {
            let delta = new_chunks - old_chunks;
            if delta > 0 {
                self.total_chunks.fetch_add(delta, Ordering::AcqRel);
            }
        } else {
            let delta = old_chunks - new_chunks;
            if delta > 0 {
                self.total_chunks.fetch_sub(delta, Ordering::AcqRel);
            }
        }

        // Update file entry
        files[idx] = new_entry;
        files[idx].version = expected_file_version + 1;
        files[idx].modified_at = Instant::now();

        // Increment global version
        let new_version = self.global_version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }

    /// Remove a file (soft delete)
    ///
    /// Marks the file as deleted without removing it from the manifest.
    pub fn remove_file(&self, path: &str, expected_file_version: u64) -> VersionedResult<u64> {
        let mut files = self.files.write().unwrap();
        let index = self.file_index.read().unwrap();

        // Find file index
        let &idx = index.get(path).ok_or(VersionMismatch {
            expected: expected_file_version,
            actual: 0,
        })?;

        // Check file version
        let current_entry = &files[idx];
        if current_entry.version != expected_file_version {
            return Err(VersionMismatch {
                expected: expected_file_version,
                actual: current_entry.version,
            });
        }

        // Mark as deleted
        files[idx].deleted = true;
        files[idx].version = expected_file_version + 1;
        files[idx].modified_at = Instant::now();

        // Increment global version
        let new_version = self.global_version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }

    /// Get all file paths (snapshot)
    pub fn list_files(&self) -> Vec<String> {
        self.files
            .read()
            .unwrap()
            .iter()
            .filter(|f| !f.deleted)
            .map(|f| f.path.clone())
            .collect()
    }

    /// Get all file entries (snapshot)
    pub fn iter(&self) -> Vec<VersionedFileEntry> {
        self.files.read().unwrap().clone()
    }

    /// Get the number of files (including deleted)
    pub fn len(&self) -> usize {
        self.files.read().unwrap().len()
    }

    /// Check if the manifest is empty
    pub fn is_empty(&self) -> bool {
        self.files.read().unwrap().is_empty()
    }

    /// Get the total number of chunks across all files
    pub fn total_chunks(&self) -> u64 {
        self.total_chunks.load(Ordering::Acquire)
    }

    /// Compact the manifest (remove deleted files)
    ///
    /// This rebuilds the internal structures without deleted entries.
    pub fn compact(&self, expected_version: u64) -> VersionedResult<usize> {
        let mut files = self.files.write().unwrap();
        let mut index = self.file_index.write().unwrap();

        // Check version
        let current_version = self.version();
        if current_version != expected_version {
            return Err(VersionMismatch {
                expected: expected_version,
                actual: current_version,
            });
        }

        // Filter out deleted files
        let old_len = files.len();
        let new_files: Vec<VersionedFileEntry> = files.drain(..).filter(|f| !f.deleted).collect();
        let removed = old_len - new_files.len();

        // Rebuild index
        index.clear();
        for (idx, file) in new_files.iter().enumerate() {
            index.insert(file.path.clone(), idx);
        }

        *files = new_files;

        // Recalculate total chunks
        let total: u64 = files.iter().map(|f| f.chunks.len() as u64).sum();
        self.total_chunks.store(total, Ordering::Release);

        // Increment version
        if removed > 0 {
            self.global_version.fetch_add(1, Ordering::AcqRel);
        }

        Ok(removed)
    }

    /// Get manifest statistics
    pub fn stats(&self) -> ManifestStats {
        let files = self.files.read().unwrap();

        let total_files = files.len();
        let deleted_files = files.iter().filter(|f| f.deleted).count();
        let active_files = total_files - deleted_files;
        let total_size = files.iter().map(|f| f.size).sum();

        ManifestStats {
            total_files,
            active_files,
            deleted_files,
            total_chunks: self.total_chunks(),
            total_size_bytes: total_size,
            version: self.version(),
        }
    }
}

impl Default for VersionedManifest {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for VersionedManifest {
    fn clone(&self) -> Self {
        Self {
            files: Arc::clone(&self.files),
            file_index: Arc::clone(&self.file_index),
            global_version: Arc::clone(&self.global_version),
            total_chunks: Arc::clone(&self.total_chunks),
        }
    }
}

/// Statistics about a manifest
#[derive(Debug, Clone)]
pub struct ManifestStats {
    pub total_files: usize,
    pub active_files: usize,
    pub deleted_files: usize,
    pub total_chunks: u64,
    pub total_size_bytes: usize,
    pub version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_creation() {
        let manifest = VersionedManifest::new();
        assert_eq!(manifest.version(), 0);
        assert!(manifest.is_empty());
    }

    #[test]
    fn test_add_file() {
        let manifest = VersionedManifest::new();
        let entry = VersionedFileEntry::new("test.txt".to_string(), true, 100, vec![1, 2, 3]);

        let version = manifest.add_file(entry).unwrap();
        assert_eq!(version, 1);
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest.total_chunks(), 3);
    }

    #[test]
    fn test_update_file() {
        let manifest = VersionedManifest::new();
        let entry = VersionedFileEntry::new("test.txt".to_string(), true, 100, vec![1, 2, 3]);

        manifest.add_file(entry.clone()).unwrap();

        let updated = entry.update(vec![4, 5], 200);
        let version = manifest.update_file("test.txt", updated, 0).unwrap();

        assert_eq!(version, 2);

        let (retrieved, _) = manifest.get_file("test.txt").unwrap();
        assert_eq!(retrieved.version, 1);
        assert_eq!(retrieved.size, 200);
        assert_eq!(retrieved.chunks, vec![4, 5]);
    }

    #[test]
    fn test_remove_file() {
        let manifest = VersionedManifest::new();
        let entry = VersionedFileEntry::new("test.txt".to_string(), true, 100, vec![1, 2, 3]);

        manifest.add_file(entry).unwrap();
        manifest.remove_file("test.txt", 0).unwrap();

        let (retrieved, _) = manifest.get_file("test.txt").unwrap();
        assert!(retrieved.deleted);
        assert_eq!(retrieved.version, 1);
    }

    #[test]
    fn test_compact() {
        let manifest = VersionedManifest::new();

        for i in 0..10 {
            let entry = VersionedFileEntry::new(format!("file{}.txt", i), true, 100, vec![i]);
            manifest.add_file(entry).unwrap();
        }

        // Remove half
        for i in 0..5 {
            manifest.remove_file(&format!("file{}.txt", i), 0).unwrap();
        }

        assert_eq!(manifest.len(), 10);

        // Compact
        let removed = manifest.compact(manifest.version()).unwrap();
        assert_eq!(removed, 5);
        assert_eq!(manifest.len(), 5);
    }

    #[test]
    fn test_stats() {
        let manifest = VersionedManifest::new();

        for i in 0..5 {
            let entry = VersionedFileEntry::new(format!("file{}.txt", i), true, 100, vec![i]);
            manifest.add_file(entry).unwrap();
        }

        let stats = manifest.stats();
        assert_eq!(stats.total_files, 5);
        assert_eq!(stats.active_files, 5);
        assert_eq!(stats.deleted_files, 0);
        assert_eq!(stats.total_chunks, 5);
    }
}
