//! Versioned manifest with file-level versioning
//!
//! The manifest stores file metadata with per-file version tracking.
//! Each file entry has its own version number for fine-grained optimistic locking.

use super::types::{ChunkId, VersionMismatch, VersionedResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// A versioned file entry in the manifest
#[derive(Debug, Clone)]
pub struct VersionedFileEntry {
    /// File path (relative to engram root)
    pub path: String,

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

    /// Version number of this file entry
    pub version: u64,

    /// When this file was created
    pub created_at: Instant,

    /// When this file was last modified
    pub modified_at: Instant,
}

impl VersionedFileEntry {
    /// Create a new file entry
    pub fn new(path: String, is_text: bool, size: usize, chunks: Vec<ChunkId>) -> Self {
        let now = Instant::now();
        Self {
            path,
            is_text,
            size,
            chunks,
            deleted: false,
            compression_codec: None,
            uncompressed_size: None,
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
            is_text,
            size: compressed_size,
            chunks,
            deleted: false,
            compression_codec: Some(compression_codec),
            uncompressed_size: Some(uncompressed_size),
            version: 0,
            created_at: now,
            modified_at: now,
        }
    }

    /// Create an updated version of this file entry
    pub fn update(&self, new_chunks: Vec<ChunkId>, new_size: usize) -> Self {
        Self {
            path: self.path.clone(),
            is_text: self.is_text,
            size: new_size,
            chunks: new_chunks,
            deleted: false,
            compression_codec: self.compression_codec,
            uncompressed_size: self.uncompressed_size,
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
            is_text: self.is_text,
            size: compressed_size,
            chunks: new_chunks,
            deleted: false,
            compression_codec: Some(compression_codec),
            uncompressed_size: Some(uncompressed_size),
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
