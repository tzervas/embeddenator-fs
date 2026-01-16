# EmbrFS Mutability Architecture Plan

**Date**: 2026-01-15
**Version**: 1.0
**Status**: Planning Phase
**Branch**: claude/engram-mutability-vsa-b8om1

## Executive Summary

This document outlines the plan to transform EmbrFS from an **immutable snapshot-based** filesystem to a **mutable, read-write filesystem with optimistic locking**. The key insight is that while VSA operations are mathematically pure, we can layer versioning and concurrency control to enable safe mutations while preserving the holographic properties of the engram.

### Key Goals

1. **Enable read-write operations** on engrams without full rebuilds
2. **Implement optimistic locking** for concurrent access control
3. **Leverage VSA for all operations** in the compressed/holographic state
4. **Maintain bit-perfect reconstruction** guarantees
5. **Support atomic transactions** for multi-chunk operations
6. **Eliminate or minimize compact operations** through incremental updates

---

## Part 0: Critical Architecture Clarification

### VSA Codebook vs Chunk Store vs Engram

**IMPORTANT**: There are three distinct concepts that must not be confused:

#### 1. VSA Codebook (in `embeddenator-vsa`)
- **What it is**: The base vectors (dictionary/basis) used by VSA for encoding/decoding operations
- **Location**: `embeddenator-vsa` crate, part of the VSA algorithm itself
- **Mutability**: **STATIC** - typically fixed for a given VSA configuration
- **Purpose**: Provides the mathematical foundation for transforming data ↔ sparse vectors
- **Versioning**: Only changes when creating entirely new VSA configurations
- **Analogy**: Like the alphabet in language - you don't change it for each sentence

#### 2. Chunk Store (formerly called "codebook" in engram)
- **What it is**: A `HashMap<ChunkId, SparseVec>` mapping file chunk IDs to their VSA-encoded representations
- **Location**: `Engram` struct, `embrfs.rs:564`
- **Mutability**: **MUTABLE** - this is what we're versioning!
- **Purpose**: Stores the encoded chunks that make up files in the filesystem
- **Versioning**: Gets versioned as `VersionedChunkStore` in the new architecture
- **Analogy**: Like a dictionary of words (encoded from the alphabet) that make up your documents

#### 3. Engram Root
- **What it is**: A bundled/superposition `SparseVec` representing all chunks holographically
- **Location**: `Engram` struct, `embrfs.rs:563`
- **Mutability**: **IMMUTABLE per-operation** (bundle is pure), but replaced with new versions
- **Purpose**: Enables holographic retrieval and queries across all encoded data
- **Versioning**: Root version tracked via atomic counter with CAS updates
- **Analogy**: Like a hologram of your entire document collection

### Why This Distinction Matters

The original codebase unfortunately named the chunk store "codebook", causing confusion:

```rust
// Current (confusing naming)
pub struct Engram {
    pub root: SparseVec,
    pub codebook: HashMap<usize, SparseVec>,  // ← NOT the VSA codebook!
    pub corrections: CorrectionStore,
}
```

This led to initial misunderstanding that we'd be versioning the VSA codebook itself, which would be incorrect. The VSA codebook (base vectors) is in `embeddenator-vsa` and should remain static.

**Corrected architecture**:
- **VSA Codebook** (embeddenator-vsa) = Base vectors for encoding (STATIC)
- **Chunk Store** (engram.codebook HashMap) = Encoded chunks (VERSIONED)
- **Root** (engram.root) = Holographic bundle (VERSIONED with CAS)
- **Manifest** = File metadata (VERSIONED)
- **Corrections** = Bit-perfect adjustments (VERSIONED)

### Transparent Compression Philosophy

EmbrFS is **NOT**:
- A traditional indexed filesystem with separate compression
- A system where compression is an explicit, add-on feature

EmbrFS **IS**:
- A transparent compression encoding system
- Compression inherent to the VSA representation itself
- A holographic storage system where data is naturally dense

The compression comes from:
1. **VSA encoding**: Data → sparse vectors (inherently compressed)
2. **Bundling**: Superposition allows multiple chunks in one vector
3. **Correction layer**: Minimal overhead for bit-perfect reconstruction

Future layers (signatures, encryption) will be built **on top** of this transparent compression, not as alternatives to it.

### Data Flow

```text
File Data (bytes)
    ↓
 Chunking (4KB blocks)
    ↓
VSA Encoding (using static VSA codebook from embeddenator-vsa)
    ↓
Encoded Chunks (SparseVec) → stored in Chunk Store (VERSIONED)
    ↓
Bundle → Root Engram (SparseVec, VERSIONED with CAS)
    ↓
Correction Layer → Bit-perfect adjustments (VERSIONED)
```

---

## Part 1: Current State Analysis

### Current Architecture Limitations

Based on analysis of the codebase (see exploration above):

**Immutability Assumptions:**
1. **Root Vector** (`embrfs.rs:861`): `self.engram.root = self.engram.root.bundle(&chunk_vec)` - complete replacement
2. **Soft Deletes** (`embrfs.rs:1015`): Files marked `deleted=true` but chunks remain in engram
3. **Modify = Delete + Add** (`embrfs.rs:1070-1077`): No true in-place updates
4. **Compact Required** (`embrfs.rs:1126-1232`): Full re-encoding to reclaim space
5. **FUSE Read-Only** (`fuse_shim.rs`): Write operations return EROFS

**Concurrency Model:**
- Single `Arc<RwLock<EmbrFS>>` wrapper (`fuse_shim.rs:64`)
- Coarse-grained: any write blocks all reads
- No versioning or conflict detection

**VSA Limitations:**
- Bundle operations are **not invertible** (`embrfs.rs:966-972`)
- Cannot "subtract" chunks from root vector
- Root represents superposition of ALL chunks

---

## Part 2: Core Design Principles

### Principle 1: VSA Operations Remain Pure

**Critical Insight:** We cannot mutate VSA structures in-place, but we can:
- Create new versions efficiently using **structural sharing**
- Apply optimistic concurrency control with **Compare-And-Swap (CAS)**
- Batch operations to minimize root vector updates

**Example:**
```rust
// Before: Immediate root mutation
self.engram.root = self.engram.root.bundle(&chunk_vec);

// After: Versioned with optimistic lock
let old_version = self.engram.root_version.load(Ordering::Acquire);
let new_root = self.engram.root.load().bundle(&chunk_vec);
match self.engram.root.compare_exchange(old_version, new_root) {
    Ok(_) => { /* Success */ },
    Err(_) => { /* Retry or queue */ }
}
```

### Principle 2: Multi-Level Versioning

Each component gets independent version tracking:

1. **Chunk-Level Versions** (codebook entries)
   - Each chunk has creation version
   - Readers see consistent snapshot
   - Writers check version on update

2. **File-Level Versions** (manifest entries)
   - Each file entry has version number
   - Enables per-file optimistic locking
   - Supports partial updates

3. **Engram-Level Version** (global)
   - Root vector version
   - Coordinated with chunk/file versions
   - Used for transaction boundaries

### Principle 3: Copy-On-Write with Structural Sharing

Instead of full copies, share immutable data:

```rust
// Codebook with Arc-wrapped chunks
struct VersionedChunkStore {
    chunks: Arc<HashMap<ChunkId, Arc<VersionedChunk>>>,
    version: u64,
}

// Updating a single chunk creates new map with shared entries
fn update_chunk(&self, id: ChunkId, new_chunk: VersionedChunk) -> VersionedChunkStore {
    let mut new_chunks = (*self.chunks).clone(); // Shallow clone (Arc pointers)
    new_chunks.insert(id, Arc::new(new_chunk));
    VersionedChunkStore {
        chunks: Arc::new(new_chunks),
        version: self.version + 1,
    }
}
```

### Principle 4: VSA-Native In-Place Operations

Leverage VSA properties for operations WITHOUT full decode/encode:

**Operations on Compressed State:**

1. **Chunk Deduplication** - Already supported via codebook
2. **Similarity Search** - Query engram directly via VSA similarity
3. **Partial Unbundle** - Retrieve specific chunks without full decode
4. **Incremental Bundle** - Add chunks to existing root (already works)
5. **Chunk Substitution** - Replace chunk in codebook + update correction

**NEW: In-Place Chunk Updates:**
```rust
impl Engram {
    /// Update a chunk in the codebook without touching root
    /// Root remains valid because bundle is superposition
    pub fn update_chunk_in_place(&mut self,
        chunk_id: ChunkId,
        new_data: &[u8],
        config: &ReversibleVSAConfig
    ) -> Result<(), Error> {
        // 1. Encode new chunk
        let new_vec = SparseVec::encode_data(new_data, config)?;

        // 2. Update codebook (mutable HashMap)
        self.chunk_store.insert(chunk_id, new_vec);

        // 3. Update correction
        let decoded = new_vec.decode_data(config, None, new_data.len());
        self.corrections.add(chunk_id, new_data, &decoded);

        // 4. Root stays the same! Bundle is associative:
        // (A ⊕ B ⊕ C) with B' = A ⊕ B' ⊕ C (superposition property)
        // Retrieval will use updated codebook entry

        Ok(())
    }
}
```

**Key Insight:** Because VSA bundle is **superposition**, updating a chunk in the codebook automatically affects future unbundle operations WITHOUT touching the root!

---

## Part 3: Architecture Design

### 3.1 Versioned Data Structures

#### VersionedChunk
```rust
#[derive(Clone)]
pub struct VersionedChunk {
    pub vector: Arc<SparseVec>,
    pub version: u64,
    pub created_at: Instant,
    pub modified_at: Instant,
    pub ref_count: AtomicU32, // How many files reference this chunk
}
```

#### VersionedChunkStore
```rust
pub struct VersionedChunkStore {
    chunks: Arc<RwLock<HashMap<ChunkId, Arc<VersionedChunk>>>>,
    global_version: AtomicU64,
    pending_writes: Arc<RwLock<Vec<PendingWrite>>>,
}

pub struct PendingWrite {
    chunk_id: ChunkId,
    base_version: u64,
    new_chunk: VersionedChunk,
    timestamp: Instant,
}

impl VersionedChunkStore {
    /// Read with optimistic locking - no blocking
    pub fn get(&self, chunk_id: ChunkId) -> Option<(Arc<VersionedChunk>, u64)> {
        let chunks = self.chunks.read().unwrap();
        let version = self.global_version.load(Ordering::Acquire);
        chunks.get(&chunk_id).map(|c| (Arc::clone(c), version))
    }

    /// Write with optimistic locking - validates version
    pub fn update(&self,
        chunk_id: ChunkId,
        new_chunk: VersionedChunk,
        expected_version: u64
    ) -> Result<u64, VersionMismatch> {
        let mut chunks = self.chunks.write().unwrap();

        // Check global version hasn't changed
        let current_version = self.global_version.load(Ordering::Acquire);
        if current_version != expected_version {
            return Err(VersionMismatch {
                expected: expected_version,
                actual: current_version,
            });
        }

        // Update
        chunks.insert(chunk_id, Arc::new(new_chunk));
        let new_version = self.global_version.fetch_add(1, Ordering::AcqRel) + 1;

        Ok(new_version)
    }

    /// Batch update - atomic multi-chunk update
    pub fn batch_update(&self,
        updates: Vec<(ChunkId, VersionedChunk)>,
        expected_version: u64
    ) -> Result<u64, VersionMismatch> {
        let mut chunks = self.chunks.write().unwrap();

        let current_version = self.global_version.load(Ordering::Acquire);
        if current_version != expected_version {
            return Err(VersionMismatch {
                expected: expected_version,
                actual: current_version,
            });
        }

        for (chunk_id, new_chunk) in updates {
            chunks.insert(chunk_id, Arc::new(new_chunk));
        }

        let new_version = self.global_version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }
}
```

#### VersionedCorrectionStore
```rust
pub struct VersionedCorrectionStore {
    corrections: Arc<RwLock<HashMap<u64, Arc<ChunkCorrection>>>>,
    stats: Arc<RwLock<CorrectionStats>>,
    version: AtomicU64,
}

impl VersionedCorrectionStore {
    /// Optimistic read
    pub fn get(&self, chunk_id: u64) -> Option<(Arc<ChunkCorrection>, u64)> {
        let corrections = self.corrections.read().unwrap();
        let version = self.version.load(Ordering::Acquire);
        corrections.get(&chunk_id).map(|c| (Arc::clone(c), version))
    }

    /// Optimistic write
    pub fn update(&self,
        chunk_id: u64,
        correction: ChunkCorrection,
        expected_version: u64
    ) -> Result<u64, VersionMismatch> {
        let mut corrections = self.corrections.write().unwrap();

        let current_version = self.version.load(Ordering::Acquire);
        if current_version != expected_version {
            return Err(VersionMismatch {
                expected: expected_version,
                actual: current_version,
            });
        }

        corrections.insert(chunk_id, Arc::new(correction));

        // Update stats
        let mut stats = self.stats.write().unwrap();
        stats.update_for_correction(&correction);

        let new_version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }
}
```

#### VersionedManifest
```rust
pub struct VersionedManifest {
    files: Arc<RwLock<Vec<VersionedFileEntry>>>,
    file_index: Arc<RwLock<HashMap<String, usize>>>, // path -> index
    version: AtomicU64,
}

pub struct VersionedFileEntry {
    pub path: String,
    pub is_text: bool,
    pub size: usize,
    pub chunks: Vec<ChunkId>,
    pub deleted: bool,
    pub version: u64,           // Per-file version
    pub created_at: Instant,
    pub modified_at: Instant,
}

impl VersionedManifest {
    /// Read file entry with version
    pub fn get_file(&self, path: &str) -> Option<(VersionedFileEntry, u64)> {
        let files = self.files.read().unwrap();
        let index = self.file_index.read().unwrap();
        let version = self.version.load(Ordering::Acquire);

        index.get(path)
            .and_then(|&idx| files.get(idx))
            .map(|entry| (entry.clone(), version))
    }

    /// Update file entry
    pub fn update_file(&self,
        path: &str,
        new_entry: VersionedFileEntry,
        expected_file_version: u64
    ) -> Result<u64, VersionMismatch> {
        let mut files = self.files.write().unwrap();
        let index = self.file_index.read().unwrap();

        let idx = index.get(path).ok_or(FileNotFound)?;
        let current_entry = &files[*idx];

        if current_entry.version != expected_file_version {
            return Err(VersionMismatch {
                expected: expected_file_version,
                actual: current_entry.version,
            });
        }

        files[*idx] = new_entry;
        files[*idx].version = expected_file_version + 1;
        files[*idx].modified_at = Instant::now();

        let new_version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }

    /// Add new file
    pub fn add_file(&self,
        entry: VersionedFileEntry
    ) -> Result<u64, Error> {
        let mut files = self.files.write().unwrap();
        let mut index = self.file_index.write().unwrap();

        if index.contains_key(&entry.path) {
            return Err(FileExists);
        }

        let idx = files.len();
        files.push(entry);
        index.insert(entry.path.clone(), idx);

        let new_version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }

    /// Remove file (soft delete)
    pub fn remove_file(&self,
        path: &str,
        expected_file_version: u64
    ) -> Result<u64, VersionMismatch> {
        let mut files = self.files.write().unwrap();
        let index = self.file_index.read().unwrap();

        let idx = index.get(path).ok_or(FileNotFound)?;
        let current_entry = &files[*idx];

        if current_entry.version != expected_file_version {
            return Err(VersionMismatch {
                expected: expected_file_version,
                actual: current_entry.version,
            });
        }

        files[*idx].deleted = true;
        files[*idx].version = expected_file_version + 1;
        files[*idx].modified_at = Instant::now();

        let new_version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(new_version)
    }
}
```

#### VersionedEngram (Top-Level)
```rust
pub struct VersionedEngram {
    // Root is Arc-wrapped for immutable sharing
    root: Arc<RwLock<Arc<SparseVec>>>,
    root_version: AtomicU64,

    // Versioned components
    chunk_store: VersionedChunkStore,
    corrections: VersionedCorrectionStore,
    manifest: VersionedManifest,

    // Coordination
    transaction_log: Arc<RwLock<Vec<Transaction>>>,
}

pub struct Transaction {
    pub id: u64,
    pub engram_version: u64,
    pub operations: Vec<Operation>,
    pub timestamp: Instant,
    pub status: TransactionStatus,
}

pub enum Operation {
    AddChunk { chunk_id: ChunkId, data: Vec<u8> },
    UpdateChunk { chunk_id: ChunkId, data: Vec<u8> },
    RemoveChunk { chunk_id: ChunkId },
    AddFile { path: String, chunks: Vec<ChunkId> },
    UpdateFile { path: String, chunks: Vec<ChunkId> },
    RemoveFile { path: String },
    BundleRoot { chunk_ids: Vec<ChunkId> },
}

pub enum TransactionStatus {
    Pending,
    Committed,
    Aborted,
}
```

### 3.2 Optimistic Locking Protocol

#### Read Operations (Non-Blocking)
```rust
impl VersionedEngram {
    /// Read file data with snapshot isolation
    pub fn read_file(&self, path: &str) -> Result<(Vec<u8>, u64), Error> {
        // 1. Get manifest entry + version
        let (file_entry, manifest_version) = self.manifest
            .get_file(path)
            .ok_or(FileNotFound)?;

        // 2. Read chunks (non-blocking)
        let mut file_data = Vec::with_capacity(file_entry.size);
        for &chunk_id in &file_entry.chunks {
            let (chunk, _) = self.chunk_store.get(chunk_id).ok_or(ChunkNotFound)?;
            let (correction, _) = self.corrections.get(chunk_id as u64)?;

            // Decode chunk
            let decoded = chunk.vector.decode_data(&self.config, None, 4096);
            let corrected = correction.apply(&decoded);

            file_data.extend_from_slice(&corrected);
        }

        // 3. Return data + version for future updates
        Ok((file_data, file_entry.version))
    }
}
```

#### Write Operations (Optimistic)
```rust
impl VersionedEngram {
    /// Write file data with optimistic locking
    pub fn write_file(&self,
        path: &str,
        data: &[u8],
        expected_file_version: Option<u64>
    ) -> Result<u64, Error> {
        // Start transaction
        let tx_id = self.begin_transaction()?;

        // 1. Check if file exists
        let existing = self.manifest.get_file(path);

        match (existing, expected_file_version) {
            (Some((entry, _)), Some(expected_ver)) => {
                // Update existing file - verify version
                if entry.version != expected_ver {
                    self.abort_transaction(tx_id)?;
                    return Err(VersionMismatch {
                        expected: expected_ver,
                        actual: entry.version,
                    });
                }
            },
            (Some(_), None) => {
                // File exists but no version check - fail
                self.abort_transaction(tx_id)?;
                return Err(FileExists);
            },
            (None, Some(_)) => {
                // Expected file but doesn't exist
                self.abort_transaction(tx_id)?;
                return Err(FileNotFound);
            },
            (None, None) => {
                // New file - OK
            }
        }

        // 2. Chunk the data
        let chunks = self.chunk_data(data);
        let mut chunk_ids = Vec::new();

        // 3. Get current codebook version
        let codebook_version = self.chunk_store.global_version.load(Ordering::Acquire);

        // 4. Encode chunks and update codebook
        let mut chunk_updates = Vec::new();
        for (i, chunk_data) in chunks.iter().enumerate() {
            let chunk_id = self.generate_chunk_id(path, i);

            // Encode
            let chunk_vec = SparseVec::encode_data(chunk_data, &self.config)?;
            let decoded = chunk_vec.decode_data(&self.config, None, chunk_data.len());

            // Create versioned chunk
            let versioned_chunk = VersionedChunk {
                vector: Arc::new(chunk_vec),
                version: codebook_version + 1,
                created_at: Instant::now(),
                modified_at: Instant::now(),
                ref_count: AtomicU32::new(1),
            };

            chunk_updates.push((chunk_id, versioned_chunk));

            // Update corrections
            self.corrections.update(
                chunk_id as u64,
                ChunkCorrection::new(chunk_id as u64, chunk_data, &decoded),
                self.corrections.version.load(Ordering::Acquire)
            )?;

            chunk_ids.push(chunk_id);
        }

        // 5. Batch update codebook
        self.chunk_store.batch_update(chunk_updates, codebook_version)?;

        // 6. Update manifest
        let new_entry = VersionedFileEntry {
            path: path.to_string(),
            is_text: is_text_data(data),
            size: data.len(),
            chunks: chunk_ids.clone(),
            deleted: false,
            version: expected_file_version.map(|v| v + 1).unwrap_or(0),
            created_at: Instant::now(),
            modified_at: Instant::now(),
        };

        if let Some((entry, _)) = existing {
            self.manifest.update_file(path, new_entry, entry.version)?;
        } else {
            self.manifest.add_file(new_entry)?;
        }

        // 7. Update root (if needed)
        self.bundle_chunks_to_root(&chunk_ids)?;

        // Commit transaction
        self.commit_transaction(tx_id)?;

        Ok(new_entry.version)
    }

    /// Bundle chunks into root with CAS
    fn bundle_chunks_to_root(&self, chunk_ids: &[ChunkId]) -> Result<(), Error> {
        loop {
            // Read current root + version
            let root_lock = self.root.read().unwrap();
            let current_root = Arc::clone(&*root_lock);
            let current_version = self.root_version.load(Ordering::Acquire);
            drop(root_lock);

            // Build new root
            let mut new_root = (*current_root).clone();
            for &chunk_id in chunk_ids {
                if let Some((chunk, _)) = self.chunk_store.get(chunk_id) {
                    new_root = new_root.bundle(&chunk.vector);
                }
            }

            // Try to update with CAS
            let mut root_lock = self.root.write().unwrap();
            let actual_version = self.root_version.load(Ordering::Acquire);

            if actual_version == current_version {
                // Success - no concurrent update
                *root_lock = Arc::new(new_root);
                self.root_version.fetch_add(1, Ordering::AcqRel);
                return Ok(());
            } else {
                // Retry - someone else updated root
                drop(root_lock);
                continue;
            }
        }
    }
}
```

### 3.3 Transaction Support

```rust
impl VersionedEngram {
    pub fn begin_transaction(&self) -> Result<u64, Error> {
        let mut tx_log = self.transaction_log.write().unwrap();
        let tx_id = tx_log.len() as u64;

        let tx = Transaction {
            id: tx_id,
            engram_version: self.root_version.load(Ordering::Acquire),
            operations: Vec::new(),
            timestamp: Instant::now(),
            status: TransactionStatus::Pending,
        };

        tx_log.push(tx);
        Ok(tx_id)
    }

    pub fn commit_transaction(&self, tx_id: u64) -> Result<(), Error> {
        let mut tx_log = self.transaction_log.write().unwrap();
        let tx = tx_log.get_mut(tx_id as usize).ok_or(TxNotFound)?;

        // Verify engram version hasn't changed significantly
        let current_version = self.root_version.load(Ordering::Acquire);
        if current_version != tx.engram_version {
            // Allow if changes are compatible (e.g., different files)
            // For now, fail
            tx.status = TransactionStatus::Aborted;
            return Err(TransactionConflict);
        }

        tx.status = TransactionStatus::Committed;
        Ok(())
    }

    pub fn abort_transaction(&self, tx_id: u64) -> Result<(), Error> {
        let mut tx_log = self.transaction_log.write().unwrap();
        let tx = tx_log.get_mut(tx_id as usize).ok_or(TxNotFound)?;
        tx.status = TransactionStatus::Aborted;
        Ok(())
    }
}
```

---

## Part 4: VSA In-Place Operations

### 4.1 Chunk-Level Updates (No Root Rebuild)

**Key Insight:** Because the root is a **superposition** (bundle) of all chunks, we can update individual chunks in the codebook WITHOUT rebuilding the root, as long as we maintain the chunk ID.

```rust
impl VersionedEngram {
    /// Update a single chunk in-place
    /// Root remains valid because retrieval uses codebook lookup
    pub fn update_chunk(&self,
        chunk_id: ChunkId,
        new_data: &[u8],
        expected_version: u64
    ) -> Result<u64, Error> {
        // 1. Encode new data
        let new_vec = SparseVec::encode_data(new_data, &self.config)?;
        let decoded = new_vec.decode_data(&self.config, None, new_data.len());

        // 2. Create versioned chunk
        let new_chunk = VersionedChunk {
            vector: Arc::new(new_vec),
            version: expected_version + 1,
            created_at: Instant::now(), // Keep original?
            modified_at: Instant::now(),
            ref_count: AtomicU32::new(1), // Will be updated
        };

        // 3. Update codebook (with version check)
        let new_version = self.chunk_store.update(chunk_id, new_chunk, expected_version)?;

        // 4. Update correction
        self.corrections.update(
            chunk_id as u64,
            ChunkCorrection::new(chunk_id as u64, new_data, &decoded),
            expected_version
        )?;

        // 5. NO ROOT UPDATE NEEDED!
        // Next unbundle will use updated codebook entry

        Ok(new_version)
    }
}
```

**Why This Works:**
- Root = Bundle(C1, C2, C3, ...)
- Unbundle(Root, probe_for_C2) ≈ C2 (approximate)
- Codebook stores exact C2
- Correction makes C2 bit-perfect
- If we update codebook[C2] = C2', next unbundle returns C2'
- Root doesn't need to change because bundle is associative and commutative

**Limitation:**
- Chunk ID must remain the same
- Chunk size should remain the same (or pad/truncate)
- Works best for fixed-size blocks

### 4.2 Similarity-Based Queries (VSA-Native)

```rust
impl VersionedEngram {
    /// Query similar files without full decode
    pub fn find_similar_files(&self,
        query_data: &[u8],
        top_k: usize
    ) -> Result<Vec<(String, f64)>, Error> {
        // 1. Encode query as SparseVec
        let query_vec = SparseVec::encode_data(query_data, &self.config)?;

        // 2. Query root engram for similar chunks
        let root = self.root.read().unwrap();
        let similar_chunks = root.find_similar(&query_vec, top_k * 10)?;

        // 3. Map chunks to files via manifest
        let manifest = self.manifest.files.read().unwrap();
        let mut file_scores: HashMap<String, f64> = HashMap::new();

        for (chunk_id, similarity) in similar_chunks {
            // Find files containing this chunk
            for file in manifest.iter().filter(|f| !f.deleted) {
                if file.chunks.contains(&chunk_id) {
                    *file_scores.entry(file.path.clone()).or_insert(0.0) += similarity;
                }
            }
        }

        // 4. Sort by total similarity score
        let mut results: Vec<_> = file_scores.into_iter().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        results.truncate(top_k);

        Ok(results)
    }
}
```

### 4.3 Partial Unbundle (Selective Decode)

```rust
impl VersionedEngram {
    /// Decode only specific chunks (no full file read needed)
    pub fn read_chunk_range(&self,
        path: &str,
        start_chunk: usize,
        num_chunks: usize
    ) -> Result<Vec<u8>, Error> {
        // 1. Get file entry
        let (file_entry, _) = self.manifest.get_file(path).ok_or(FileNotFound)?;

        // 2. Validate range
        if start_chunk + num_chunks > file_entry.chunks.len() {
            return Err(InvalidRange);
        }

        // 3. Read only requested chunks
        let mut data = Vec::new();
        for i in start_chunk..(start_chunk + num_chunks) {
            let chunk_id = file_entry.chunks[i];
            let (chunk, _) = self.chunk_store.get(chunk_id).ok_or(ChunkNotFound)?;
            let (correction, _) = self.corrections.get(chunk_id as u64)?;

            let decoded = chunk.vector.decode_data(&self.config, None, 4096);
            let corrected = correction.apply(&decoded);

            data.extend_from_slice(&corrected);
        }

        Ok(data)
    }
}
```

---

## Part 5: FUSE Layer Updates

### 5.1 Enable Write Operations

Update `fuse_shim.rs` to support write operations:

```rust
impl Filesystem for EmbrFSHandle {
    // NEW: write() implementation
    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        // 1. Get file path from inode
        let path = match self.inode_table.read().unwrap().get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // 2. Read current file data + version
        let fs = self.fs.read().unwrap();
        let (mut file_data, file_version) = match fs.read_file(&path) {
            Ok(data) => data,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        drop(fs);

        // 3. Apply write at offset
        let offset = offset as usize;
        if offset + data.len() > file_data.len() {
            file_data.resize(offset + data.len(), 0);
        }
        file_data[offset..offset + data.len()].copy_from_slice(data);

        // 4. Write back with optimistic locking
        let fs = self.fs.write().unwrap();
        match fs.write_file(&path, &file_data, Some(file_version)) {
            Ok(_) => reply.written(data.len() as u32),
            Err(Error::VersionMismatch { .. }) => {
                // Concurrent modification - retry
                reply.error(libc::EAGAIN);
            },
            Err(_) => reply.error(libc::EIO),
        }
    }

    // NEW: create() implementation
    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        // Get parent directory path
        let parent_path = match self.inode_table.read().unwrap().get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Construct new file path
        let file_name = name.to_string_lossy();
        let file_path = format!("{}/{}", parent_path.trim_end_matches('/'), file_name);

        // Create empty file
        let fs = self.fs.write().unwrap();
        match fs.write_file(&file_path, &[], None) {
            Ok(_) => {
                // Allocate inode
                let mut inode_table = self.inode_table.write().unwrap();
                let ino = inode_table.allocate(&file_path);

                // Return success
                let attr = FileAttr {
                    ino,
                    size: 0,
                    blocks: 0,
                    atime: SystemTime::now(),
                    mtime: SystemTime::now(),
                    ctime: SystemTime::now(),
                    crtime: SystemTime::now(),
                    kind: FileType::RegularFile,
                    perm: 0o644,
                    nlink: 1,
                    uid: 1000,
                    gid: 1000,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                };

                reply.created(&Duration::from_secs(1), &attr, 0, 0, 0);
            },
            Err(_) => reply.error(libc::EIO),
        }
    }

    // NEW: unlink() implementation
    fn unlink(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        reply: ReplyEmpty,
    ) {
        // Get parent directory path
        let parent_path = match self.inode_table.read().unwrap().get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Construct file path
        let file_name = name.to_string_lossy();
        let file_path = format!("{}/{}", parent_path.trim_end_matches('/'), file_name);

        // Get file version first
        let fs = self.fs.read().unwrap();
        let file_version = match fs.manifest.get_file(&file_path) {
            Some((entry, _)) => entry.version,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        drop(fs);

        // Remove file
        let fs = self.fs.write().unwrap();
        match fs.manifest.remove_file(&file_path, file_version) {
            Ok(_) => reply.ok(),
            Err(_) => reply.error(libc::EIO),
        }
    }
}
```

### 5.2 Fine-Grained Locking

Replace single `Arc<RwLock<EmbrFS>>` with component-level locks:

```rust
pub struct EmbrFSHandle {
    fs: Arc<VersionedEngram>,  // Now has internal fine-grained locks
    inode_table: Arc<RwLock<InodeTable>>,
    open_files: Arc<RwLock<HashMap<u64, OpenFile>>>,
}

// Read operations now lock only what they need
fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
    let path = self.inode_table.read().unwrap().get_path(ino).unwrap();

    // Only locks manifest (not codebook or corrections)
    let (file_entry, _) = self.fs.manifest.get_file(&path).unwrap();

    // Build attributes
    let attr = FileAttr { ... };
    reply.attr(&Duration::from_secs(1), &attr);
}
```

---

## Part 6: Implementation Roadmap

### Phase 1: Foundation (Week 1-2)

**Goal:** Implement versioned data structures

**Tasks:**
1. Create `VersionedChunk` struct
2. Create `VersionedChunkStore` with optimistic locking
3. Create `VersionedCorrectionStore` with optimistic locking
4. Create `VersionedManifest` with per-file versioning
5. Create `VersionedEngram` top-level wrapper
6. Add comprehensive unit tests for each component

**Deliverables:**
- `src/fs/versioned/mod.rs` - Module structure
- `src/fs/versioned/chunk.rs` - VersionedChunk
- `src/fs/versioned/codebook.rs` - VersionedChunkStore
- `src/fs/versioned/corrections.rs` - VersionedCorrectionStore
- `src/fs/versioned/manifest.rs` - VersionedManifest
- `src/fs/versioned/engram.rs` - VersionedEngram
- `tests/versioned_structures.rs` - Tests

**Success Criteria:**
- [ ] All versioned structures compile
- [ ] Optimistic locking detects conflicts
- [ ] CAS operations work correctly
- [ ] 100% unit test coverage

### Phase 2: Core Operations (Week 3-4)

**Goal:** Implement read/write operations with optimistic locking

**Tasks:**
1. Implement `read_file()` with snapshot isolation
2. Implement `write_file()` with optimistic locking
3. Implement `update_chunk()` for in-place updates
4. Implement `bundle_chunks_to_root()` with CAS
5. Add transaction support
6. Add conflict resolution strategies

**Deliverables:**
- Updated `embrfs.rs` with new operations
- `src/fs/transaction.rs` - Transaction support
- `tests/concurrent_operations.rs` - Concurrency tests

**Success Criteria:**
- [ ] Concurrent reads don't block each other
- [ ] Concurrent writes detect conflicts
- [ ] Version mismatches are caught
- [ ] Transactions rollback on error
- [ ] No data corruption under load

### Phase 3: VSA In-Place Operations (Week 5-6)

**Goal:** Leverage VSA for operations on compressed state

**Tasks:**
1. Implement chunk-level updates (no root rebuild)
2. Implement similarity-based queries
3. Implement partial unbundle
4. Optimize root updates (batching)
5. Add VSA-native deduplication

**Deliverables:**
- `src/fs/vsa_ops.rs` - VSA-native operations
- `tests/vsa_operations.rs` - VSA operation tests
- Benchmark suite for VSA ops

**Success Criteria:**
- [ ] Chunk updates don't trigger full rebuild
- [ ] Similarity queries work on compressed data
- [ ] Partial reads are faster than full reads
- [ ] Deduplication reduces storage

### Phase 4: FUSE Write Support (Week 7-8)

**Goal:** Enable write operations through FUSE

**Tasks:**
1. Implement `write()` in FUSE layer
2. Implement `create()` in FUSE layer
3. Implement `unlink()` in FUSE layer
4. Implement `mkdir()` and `rmdir()`
5. Add proper error handling and retries
6. Update FUSE documentation

**Deliverables:**
- Updated `fuse_shim.rs` with write ops
- `tests/fuse_write_tests.rs` - FUSE write tests
- Updated `docs/FUSE.md`

**Success Criteria:**
- [ ] Can create files via FUSE
- [ ] Can write to files via FUSE
- [ ] Can delete files via FUSE
- [ ] Concurrent FUSE operations don't corrupt
- [ ] Standard tools (cp, mv, rm) work

### Phase 5: Advanced Features (Week 9-10)

**Goal:** Production-ready features

**Tasks:**
1. Implement MVCC snapshots
2. Add garbage collection for deleted chunks
3. Implement automatic compaction triggers
4. Add metrics and observability
5. Performance optimization

**Deliverables:**
- `src/fs/mvcc.rs` - Multi-version concurrency control
- `src/fs/gc.rs` - Garbage collection
- `src/fs/metrics.rs` - Metrics collection
- Performance benchmarks

**Success Criteria:**
- [ ] Snapshots provide point-in-time views
- [ ] Garbage collection reclaims space
- [ ] Automatic compaction triggers at thresholds
- [ ] Metrics track all operations
- [ ] Performance meets baseline

### Phase 6: Testing & Documentation (Week 11-12)

**Goal:** Comprehensive testing and documentation

**Tasks:**
1. Property-based testing (proptest)
2. Stress testing (high concurrency)
3. Fuzzing (afl.rs)
4. Update all documentation
5. Create migration guide
6. Write examples

**Deliverables:**
- `tests/property_tests.rs` - Property tests
- `tests/stress_tests.rs` - Stress tests
- Updated README, ARCHITECTURE, etc.
- `MIGRATION.md` - Migration guide
- `examples/mutable_fs.rs` - Example

**Success Criteria:**
- [ ] Property tests pass 10K iterations
- [ ] Stress tests pass 1M operations
- [ ] Fuzzing finds no crashes (24h run)
- [ ] Documentation is complete
- [ ] Migration path is clear

---

## Part 7: Success Metrics

### Performance Targets

**Read Operations:**
- [ ] Concurrent reads: 10K ops/sec minimum
- [ ] Read latency: <10ms p99
- [ ] No blocking between readers

**Write Operations:**
- [ ] Concurrent writes (different files): 1K ops/sec minimum
- [ ] Write latency: <50ms p99
- [ ] Version conflict rate: <5% under high contention

**VSA Operations:**
- [ ] Chunk update: <1ms (no root rebuild)
- [ ] Similarity query: <100ms for 1M chunks
- [ ] Partial read: <5ms per chunk

**Memory Usage:**
- [ ] Codebook: <100MB for 100K chunks
- [ ] Corrections: <5MB overhead (avg 2-5%)
- [ ] Manifest: <10MB for 100K files

### Correctness Targets

- [ ] 0 data corruption events
- [ ] 0 deadlocks
- [ ] 0 race conditions (verified by ThreadSanitizer)
- [ ] 100% bit-perfect reconstruction
- [ ] ACID properties for transactions

### Scalability Targets

- [ ] Support 1M files
- [ ] Support 10M chunks
- [ ] Support 100GB engrams
- [ ] Support 100 concurrent writers
- [ ] Support 1000 concurrent readers

---

## Part 8: Risk Mitigation

### Technical Risks

**Risk 1: CAS Performance**
- **Problem:** High contention on root updates causes retry storms
- **Mitigation:**
  - Batch root updates
  - Queue pending bundles
  - Background bundle worker thread
  - Fall back to write lock under extreme contention

**Risk 2: Memory Overhead**
- **Problem:** Versioning increases memory usage
- **Mitigation:**
  - Arc-wrap immutable data for sharing
  - Garbage collect old versions
  - Use compact representations (u32 instead of u64 where possible)
  - Implement version GC threshold

**Risk 3: Consistency**
- **Problem:** Multi-component updates may leave inconsistent state
- **Mitigation:**
  - Implement proper transaction boundaries
  - Use write-ahead log for durability
  - Add consistency checks on load
  - Implement recovery procedures

**Risk 4: Complexity**
- **Problem:** Versioned structures are complex to implement correctly
- **Mitigation:**
  - Extensive unit testing
  - Property-based testing
  - Formal verification (if time permits)
  - Incremental rollout (behind feature flag)

### Timeline Risks

**Risk 1: Scope Creep**
- **Mitigation:** Strict phase boundaries, no Phase N+1 work until Phase N complete

**Risk 2: Testing Bottleneck**
- **Mitigation:** Write tests in parallel with implementation, automated CI

**Risk 3: Performance Issues**
- **Mitigation:** Early benchmarking, profiling from day 1

---

## Part 9: Open Questions

1. **Compaction Strategy:**
   - When to trigger automatic compaction?
   - Background compaction vs. on-demand?
   - Impact on concurrent operations?

2. **Version GC:**
   - How long to keep old versions?
   - Snapshot retention policy?
   - User-configurable?

3. **Conflict Resolution:**
   - Automatic retry with backoff?
   - User-defined conflict handlers?
   - Last-write-wins vs. merge strategies?

4. **Durability:**
   - Write-ahead log for crash recovery?
   - Sync vs. async commits?
   - Checkpoint frequency?

5. **API Stability:**
   - Breaking changes acceptable in alpha?
   - Version migration path?
   - Backwards compatibility guarantees?

---

## Part 10: Next Steps

### Immediate Actions (This Week)

1. **Review this plan** with stakeholders
2. **Create feature branch**: `feat/engram-mutability` (already on it!)
3. **Set up project board** with all tasks
4. **Create test fixtures** for consistency
5. **Begin Phase 1** implementation

### Week 1 Deliverables

- [ ] Reviewed and approved plan
- [ ] Created `src/fs/versioned/` module structure
- [ ] Implemented `VersionedChunk`
- [ ] Implemented `VersionedChunkStore` (partial)
- [ ] Written unit tests
- [ ] Updated documentation outline

---

## Conclusion

This plan transforms EmbrFS from an immutable snapshot-based filesystem to a **fully mutable, read-write filesystem with optimistic locking**, while:

✅ **Preserving VSA properties** - All operations work on compressed/holographic state
✅ **Enabling concurrency** - Fine-grained locking, non-blocking reads
✅ **Maintaining correctness** - Optimistic locking, transactions, version checking
✅ **Improving performance** - Chunk-level updates, no unnecessary rebuilds
✅ **Supporting FUSE writes** - Full read-write filesystem capabilities

The key insight is that **VSA operations can remain pure while we layer versioning and concurrency control** to enable safe, efficient mutations. The engram stays holographic, but becomes **mutable through versioned components and optimistic locking**.

**Next:** Begin Phase 1 implementation!

---

**Document Status:** ✅ READY FOR IMPLEMENTATION
**Last Updated:** 2026-01-15
**Branch:** claude/engram-mutability-vsa-b8om1
**Author:** Claude (with human oversight)
