# embeddenator-fs Implementation Plan v2
**Date:** 2026-01-15
**Current Version:** v0.20.0-alpha.3
**Target Version:** v0.20.0-beta.1 (short-term), v1.0.0 (long-term)

---

## 🚨 CRITICAL ARCHITECTURAL CORRECTION

**Previous Assumption (INCORRECT):** Engrams are immutable, read-only snapshots.

**Correct Design:** Engrams MUST be read-write capable. Read-only was a testing workaround from early development in read-only root filesystem Docker containers.

**Core Goal:** **VSA operations directly on compressed engrams** - Enable read AND write while in the "compressed" engram state, performantly.

---

## Executive Summary

**Status**: Core library has immutability baked into documentation and design. **Critical pivot needed**: Transform from read-only snapshots to read-write compressed filesystem.

**Scope**: embeddenator-fs is a **library component** in a distributed architecture:
- **CLI**: Lives in `embeddenator-cli` repo (separate)
- **Testing**: Centralized in `embeddenator-testkit` repo
- **Dependencies**: embeddenator-vsa, embeddenator-retrieval, embeddenator-io, embeddenator-interop
- **Developer Tools**: embeddenator-workspace, embeddenator-obs

**Immediate Goal**: Remove read-only assumptions, implement read-write engram architecture.

---

## Current State Analysis

### ✅ What Works
- **Core VSA encoding/decoding**: Solid foundation
- **Correction layer**: 100% bit-perfect reconstruction
- **Hierarchical structures**: Sub-engrams for scalability
- **Testing**: 30 passing tests (but all assume read-only!)
- **FUSE integration**: Working but returns EROFS on writes

### 🔴 Critical Issues (Read-Only Assumptions)

#### Documentation (Pervasive)
1. **docs/ARCHITECTURE.md** (Lines 40-53)
   - "Holographic engrams are immutable snapshots"
   - "FUSE operations are read-only by design"
   - Presents immutability as a feature, not a limitation

2. **docs/FUSE.md** (Throughout)
   - "mount holographic engrams as read-only filesystems"
   - "write, create, mkdir, unlink, rmdir - Return EROFS"
   - "Design Rationale: Holographic engrams are immutable snapshots"

3. **README.md**
   - Likely has "read-only FUSE mounting"
   - Describes engrams as snapshots

4. **CHANGELOG.md**
   - "Holographic engrams are immutable snapshots"

5. **ROADMAP.md**
   - Mount options mention "read-only"

6. **QUICKSTART.md**
   - Examples show "Mount read-only"

#### Code (Enforcement)
1. **src/fs/fuse_shim.rs**
   - `read_only: bool` field (always true)
   - Returns EROFS for write operations
   - `open()` checks for write flags, returns EROFS

2. **src/fs/embrfs.rs**
   - Incremental operations (`add_file`, `modify_file`, `remove_file`) create NEW engrams
   - No in-place modification support
   - Compact rebuilds entire engram

#### Tests (All Read-Only)
- 30 tests assume read-only semantics
- No tests for write operations
- No tests for concurrent read-write
- No tests for VSA operations on compressed engrams

---

## Architectural Requirements for Read-Write Engrams

### Goal: VSA Operations on Compressed Engrams

**Principle**: Modify engram content **without full decompression/recompression**.

#### 1. In-Place Chunk Modification
```rust
pub trait CompressedWrite {
    /// Modify a chunk directly in the engram
    fn modify_chunk(&mut self, chunk_id: u64, new_data: &[u8]) -> Result<()>;

    /// Add a new chunk without rebuilding
    fn append_chunk(&mut self, data: &[u8]) -> Result<u64>;

    /// Remove a chunk (mark deleted, reclaim later)
    fn delete_chunk(&mut self, chunk_id: u64) -> Result<()>;
}
```

**VSA Challenge**: Bundled chunks are entangled. Modifying one affects unbundling.

**Solution Approaches**:
- **Differential Updates**: Store deltas, apply on read
- **Lazy Rebundling**: Update only affected sub-engrams
- **Shadow Structures**: Maintain overlay for modifications

#### 2. Write-Through FUSE Operations
```rust
// FUSE operations that MUST work:
- write()      // Modify file content
- create()     // Create new file
- mkdir()      // Create directory
- unlink()     // Delete file
- rmdir()      // Delete directory
- truncate()   // Change file size
- chmod()      // Change permissions
- chown()      // Change ownership
- utimens()    // Update timestamps
- rename()     // Move/rename file
```

#### 3. Transactional Consistency
```rust
pub struct Transaction {
    operations: Vec<Operation>,
    snapshot: EngramSnapshot,
}

impl Transaction {
    fn commit(&self) -> Result<()>;
    fn rollback(&self) -> Result<()>;
}
```

**Requirements**:
- Atomic writes (all or nothing)
- Crash recovery (journal/log)
- Concurrent access (readers during write)

#### 4. Lock Granularity
```rust
// Current: Single RwLock<EmbrFS> (coarse)
// Needed: Fine-grained locking

pub struct LockTable {
    chunk_locks: HashMap<u64, RwLock<()>>,
    file_locks: HashMap<PathBuf, RwLock<()>>,
}
```

**Strategy**:
- Read locks: Multiple readers per chunk/file
- Write locks: Exclusive per chunk/file
- Lock-free reads for immutable chunks

---

## Implementation Plan: Read-Write Transformation

### Phase 0: Assessment & Planning (Week 1)

**Goal**: Understand current architecture, plan transformation

**Tasks**:

1. **Audit all read-only assumptions**
   - Grep for "immutable", "read-only", "read_only", "EROFS"
   - List all occurrences in code and docs
   - Categorize by severity (blocker vs. documentation)

2. **Review other repos** (light touch)
   - embeddenator-cli: What API does it expect?
   - embeddenator-vsa: Does it support in-place operations?
   - embeddenator-retrieval: Hierarchical write implications?
   - embeddenator-testkit: Test patterns we should follow
   - embeddenator-workspace: Development patterns/standards

3. **Design write architecture**
   - Choose approach: Differential updates vs. lazy rebundling vs. shadow structures
   - Define transaction model
   - Specify locking strategy
   - Plan backward compatibility

4. **Update documentation** (high-level)
   - Create ARCHITECTURE_V2.md describing read-write design
   - Document VSA operations on compressed engrams
   - Update README to remove "read-only" claims

**Deliverable**: Detailed design document, updated high-level docs

---

### Phase 1: Foundation for Writes (Weeks 2-3)

**Goal**: Core infrastructure for write operations

#### 1.1 Data Model Changes

**Modify `EmbrFS` structure**:
```rust
pub struct EmbrFS {
    // Existing fields
    pub engram: Engram,
    pub manifest: Manifest,
    pub codebook: Codebook,

    // NEW: Write support
    pub write_log: WriteLog,           // Pending writes
    pub lock_table: LockTable,          // Fine-grained locks
    pub transaction: Option<Transaction>, // Active transaction
    pub dirty_chunks: HashSet<u64>,     // Modified chunks
}
```

**Add `WriteLog`**:
```rust
pub struct WriteLog {
    operations: Vec<WriteOp>,
    created_at: SystemTime,
}

pub enum WriteOp {
    ModifyChunk { chunk_id: u64, data: Vec<u8> },
    AppendChunk { data: Vec<u8> },
    DeleteChunk { chunk_id: u64 },
    UpdateManifest { path: PathBuf, metadata: FileMetadata },
}
```

#### 1.2 Transaction System

**Implement basic transactions**:
```rust
impl EmbrFS {
    pub fn begin_transaction(&mut self) -> Result<TransactionGuard>;
    pub fn commit(&mut self) -> Result<()>;
    pub fn rollback(&mut self) -> Result<()>;
}
```

**Journal for durability**:
- Append-only log of operations
- Replay on startup for crash recovery

#### 1.3 Locking Infrastructure

**Add fine-grained locks**:
```rust
impl EmbrFS {
    pub fn lock_chunk_read(&self, chunk_id: u64) -> ReadGuard;
    pub fn lock_chunk_write(&mut self, chunk_id: u64) -> WriteGuard;
    pub fn lock_file_write(&mut self, path: &Path) -> FileWriteGuard;
}
```

**Lock ordering** to prevent deadlocks:
- Always acquire locks in ascending chunk_id order
- File locks before chunk locks

---

### Phase 2: Write Operations (Weeks 4-5)

**Goal**: Implement core write operations

#### 2.1 Chunk-Level Writes

**Implement `modify_chunk()`**:
```rust
impl EmbrFS {
    pub fn modify_chunk(&mut self, chunk_id: u64, new_data: &[u8]) -> Result<()> {
        // 1. Acquire write lock
        let _guard = self.lock_chunk_write(chunk_id);

        // 2. Decode existing chunk from engram
        let old_data = self.decode_chunk(chunk_id)?;

        // 3. Compute VSA update (differential)
        let delta = compute_delta(&old_data, new_data);

        // 4. Apply delta to engram (in-place!)
        self.engram.apply_delta(chunk_id, &delta)?;

        // 5. Update correction layer
        let new_correction = ChunkCorrection::new(chunk_id, new_data, &approximation);
        self.correction_store.update(chunk_id, new_correction);

        // 6. Mark dirty
        self.dirty_chunks.insert(chunk_id);

        Ok(())
    }
}
```

**Key Challenge**: `compute_delta()` and `apply_delta()` for VSA.
- May need support from embeddenator-vsa
- Differential bundling/unbundling

#### 2.2 File-Level Operations

**Implement `write_file()`**:
```rust
impl EmbrFS {
    pub fn write_file(&mut self, path: &Path, offset: u64, data: &[u8]) -> Result<usize> {
        // 1. Lock file
        let _guard = self.lock_file_write(path);

        // 2. Determine affected chunks
        let start_chunk = offset / CHUNK_SIZE;
        let end_chunk = (offset + data.len() as u64) / CHUNK_SIZE;

        // 3. Modify each chunk
        for chunk_id in start_chunk..=end_chunk {
            // Partial chunk update logic
            self.modify_chunk(chunk_id, chunk_data)?;
        }

        // 4. Update manifest (file size, mtime)
        self.manifest.update_metadata(path, ...)?;

        Ok(data.len())
    }
}
```

#### 2.3 Directory Operations

**Implement `create_file()`, `mkdir()`, `unlink()`, `rmdir()`**:
```rust
impl EmbrFS {
    pub fn create_file(&mut self, path: &Path, mode: u32) -> Result<()> {
        // 1. Check parent exists
        // 2. Add to manifest
        // 3. Allocate chunk IDs
        // 4. Update engram structure
    }

    pub fn mkdir(&mut self, path: &Path, mode: u32) -> Result<()> {
        // Similar to create_file but directory type
    }

    pub fn unlink(&mut self, path: &Path) -> Result<()> {
        // 1. Remove from manifest
        // 2. Mark chunks deleted (don't reclaim immediately)
        // 3. Compact later to reclaim space
    }
}
```

---

### Phase 3: FUSE Write Support (Week 6)

**Goal**: Expose write operations through FUSE

#### 3.1 Remove EROFS Enforcement

**src/fs/fuse_shim.rs changes**:
```rust
// OLD:
if self.read_only && (flags & write_flags != 0) {
    reply.error(libc::EROFS);
    return;
}

// NEW:
// Allow write operations, check permissions instead
if !has_permission(ino, flags) {
    reply.error(libc::EACCES);
    return;
}
```

#### 3.2 Implement FUSE Write Operations

**Add implementations**:
```rust
impl Filesystem for EngramFS {
    fn write(&mut self, req: &Request, ino: u64, fh: u64, offset: i64,
             data: &[u8], write_flags: i32, flags: i32,
             lock_owner: Option<u64>, reply: ReplyWrite) {
        // Call EmbrFS::write_file()
    }

    fn create(&mut self, req: &Request, parent: u64, name: &OsStr,
              mode: u32, umask: u32, flags: i32, reply: ReplyCreate) {
        // Call EmbrFS::create_file()
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr,
             mode: u32, umask: u32, reply: ReplyEntry) {
        // Call EmbrFS::mkdir()
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr,
              reply: ReplyEmpty) {
        // Call EmbrFS::unlink()
    }

    // ... rmdir, truncate, chmod, chown, utimens, rename
}
```

#### 3.3 Update FUSE Attributes

**Change filesystem flags**:
```rust
impl Filesystem for EngramFS {
    fn init(&mut self, req: &Request, config: &mut KernelConfig) -> Result<(), c_int> {
        // OLD: Report as read-only
        // NEW: Report as writable
        config.add_capabilities(FUSE_CAP_WRITEBACK_CACHE)?;
        Ok(())
    }

    fn statfs(&mut self, req: &Request, ino: u64, reply: ReplyStatfs) {
        // OLD: bfree = 0, bavail = 0 (read-only)
        // NEW: Report available space for writes
    }
}
```

---

### Phase 4: Testing & Validation (Week 7)

**Goal**: Comprehensive testing of write operations

#### 4.1 Unit Tests for Writes
```rust
#[test]
fn test_modify_chunk() {
    let mut fs = EmbrFS::new();
    // ... ingest data

    // Modify a chunk
    let chunk_id = 0;
    let new_data = b"modified content";
    fs.modify_chunk(chunk_id, new_data).unwrap();

    // Verify reconstruction
    let recovered = fs.decode_chunk(chunk_id).unwrap();
    assert_eq!(recovered, new_data);
}

#[test]
fn test_write_file() {
    let mut fs = EmbrFS::new();
    fs.create_file("/test.txt", 0o644).unwrap();

    // Write data
    let written = fs.write_file("/test.txt", 0, b"hello").unwrap();
    assert_eq!(written, 5);

    // Read back
    let read_data = fs.read_file("/test.txt", 0, 5).unwrap();
    assert_eq!(read_data, b"hello");
}
```

#### 4.2 FUSE Write Tests
```rust
#[test]
#[cfg(feature = "fuse")]
fn test_fuse_write() {
    // Mount filesystem
    let fs = EmbrFS::new();
    let mount = spawn_mount(fs, "/mnt/test").unwrap();

    // Write through FUSE
    std::fs::write("/mnt/test/file.txt", b"test").unwrap();

    // Verify
    let content = std::fs::read("/mnt/test/file.txt").unwrap();
    assert_eq!(content, b"test");
}
```

#### 4.3 Concurrent Access Tests
```rust
#[test]
fn test_concurrent_read_write() {
    let fs = Arc::new(RwLock::new(EmbrFS::new()));

    // Spawn readers and writers
    let handles: Vec<_> = (0..10).map(|i| {
        let fs_clone = Arc::clone(&fs);
        thread::spawn(move || {
            if i % 2 == 0 {
                // Writer
                fs_clone.write().unwrap()
                    .write_file("/test.txt", i * 100, &data).unwrap();
            } else {
                // Reader
                fs_clone.read().unwrap()
                    .read_file("/test.txt", 0, 100).unwrap();
            }
        })
    }).collect();

    // Join all
    for handle in handles {
        handle.join().unwrap();
    }
}
```

#### 4.4 Crash Recovery Tests
```rust
#[test]
fn test_crash_recovery() {
    let mut fs = EmbrFS::new();

    // Begin transaction
    let tx = fs.begin_transaction().unwrap();
    fs.write_file("/test.txt", 0, b"data").unwrap();

    // Simulate crash (drop without commit)
    drop(fs);

    // Reload
    let mut fs2 = EmbrFS::load("test.engram").unwrap();

    // Verify transaction was rolled back
    assert!(!fs2.manifest.contains("/test.txt"));
}
```

---

### Phase 5: Documentation Updates (Week 8)

**Goal**: Update all documentation to reflect read-write design

#### 5.1 Architecture Documentation

**docs/ARCHITECTURE_V2.md** (new):
```markdown
# EmbrFS Read-Write Architecture

## Design Principle

Engrams are **read-write compressed filesystems**. VSA operations are performed directly on the compressed engram, enabling modifications without full decompression/recompression.

## Write Strategy

### Differential Updates
- Compute delta between old and new chunk content
- Apply delta to VSA bundle (differential bundling)
- Update correction layer incrementally

### Transaction Model
- Atomic operations (all-or-nothing)
- Journal-based crash recovery
- MVCC for concurrent readers during writes

### Performance
- In-place chunk modification: O(1) for small changes
- No full engram rebuild required
- Lazy rebundling of affected sub-engrams only
```

#### 5.2 Update Existing Docs

**docs/ARCHITECTURE.md**:
- Remove "immutable" sections (lines 40-53)
- Add reference to ARCHITECTURE_V2.md
- Update FUSE operations table (remove EROFS)

**docs/FUSE.md**:
- Change "read-only filesystems" to "read-write filesystems"
- Update operation table (write, create, etc. now supported)
- Remove "Design Rationale" about immutability
- Add write performance characteristics

**README.md**:
- Update "What EmbrFS IS" section
- Add write operations to feature list
- Update FUSE mounting examples (remove "read-only")
- Add performance notes for writes

**CHANGELOG.md**:
- Add v0.20.0-beta.1 entry
- Document transition from read-only to read-write

**ROADMAP.md**:
- Update to reflect read-write focus
- Adjust timelines for new work

#### 5.3 Code Documentation

**Update rustdoc comments**:
- Remove "immutable" claims from function docs
- Add write operation examples
- Document transaction requirements
- Explain locking behavior

---

## Challenges & Open Questions

### 1. VSA Differential Updates

**Challenge**: VSA bundling creates entangled representations. How do we modify one chunk without affecting others?

**Potential Solutions**:
- **Lazy Rebundling**: Only rebundle affected sub-engrams when needed
- **Delta Encoding**: Store deltas, apply on read (like Git)
- **Shadow Bundle**: Maintain overlay for modifications, merge on compact

**Question for embeddenator-vsa**: Does it support differential bundling/unbundling?

### 2. Hierarchical Write Propagation

**Challenge**: Hierarchical sub-engrams have parent-child dependencies. Modifying a leaf affects parent.

**Approach**:
- Mark parents dirty
- Rebundle parent only when queried (lazy)
- Or: Maintain separate index for modifications

**Question for embeddenator-retrieval**: How do writes propagate up the hierarchy?

### 3. Concurrent Write Consistency

**Challenge**: Multiple writers could conflict (e.g., two processes modifying same file).

**Options**:
- **Pessimistic Locking**: Lock file/chunk for duration of write (simple, may block)
- **Optimistic Locking**: Check version, retry on conflict (complex, better perf)
- **MVCC**: Multiple versions, resolve on read (like Git merge)

**Decision Needed**: Which concurrency model?

### 4. Space Reclamation

**Challenge**: Deleted chunks and old versions accumulate. When to reclaim?

**Strategies**:
- **Lazy Deletion**: Mark deleted, reclaim on compact
- **Reference Counting**: Reclaim when ref count = 0
- **Garbage Collection**: Periodic scan and cleanup

### 5. Backward Compatibility

**Challenge**: Existing engrams are read-only. How to migrate?

**Options**:
- **Version Flag**: Engram format v1 (read-only) vs. v2 (read-write)
- **Auto-Upgrade**: Convert on first write
- **Explicit Migration**: Tool to upgrade format

---

## Dependencies & Coordination

### Critical Dependencies

1. **embeddenator-vsa** (v0.20.0-alpha.1)
   - **Need**: Differential bundling/unbundling API
   - **Question**: Can we modify a bundle in-place?
   - **Action**: Review VSA crate, coordinate changes

2. **embeddenator-retrieval** (v0.20.0-alpha.1)
   - **Need**: Hierarchical write propagation
   - **Question**: How do writes affect hierarchical queries?
   - **Action**: Understand hierarchical update semantics

3. **embeddenator-cli** (separate repo)
   - **Need**: CLI commands for write operations
   - **Provides**: `embrfs write`, `embrfs rm`, `embrfs mv`, etc.
   - **Action**: Coordinate API expectations

4. **embeddenator-testkit** (separate repo)
   - **Need**: Centralized tests for write operations
   - **Provides**: Test harness, fixtures
   - **Action**: Contribute write tests

5. **embeddenator-workspace** (separate repo)
   - **Provides**: Development standards, patterns
   - **Action**: Review for architecture guidance

### Repositories to Review

**Priority 1 (Critical)**:
- embeddenator-cli (main, dev): Understand CLI API expectations
- embeddenator-vsa (main, dev): Check for differential update support
- embeddenator-retrieval (main, dev): Hierarchical write implications

**Priority 2 (Important)**:
- embeddenator-testkit (main, dev): Test patterns and harness
- embeddenator-workspace (main, dev): Architecture docs and patterns
- embeddenator-core (main, dev): Overall project direction

**Priority 3 (Context)**:
- embeddenator-io (main, dev): I/O patterns
- embeddenator-interop (main, dev): Component interfaces
- embeddenator-obs (main, dev): Observability/telemetry

---

## Success Criteria

### Beta Release (v0.20.0-beta.1)

- [ ] **Architectural Foundation**
  - [ ] Write infrastructure in place (WriteLog, transactions, locks)
  - [ ] All "immutable" documentation removed/updated
  - [ ] ARCHITECTURE_V2.md complete

- [ ] **Core Write Operations**
  - [ ] `modify_chunk()` working with differential updates
  - [ ] `write_file()` working for file modifications
  - [ ] `create_file()`, `mkdir()`, `unlink()`, `rmdir()` implemented

- [ ] **FUSE Integration**
  - [ ] EROFS removed, write operations exposed
  - [ ] write(), create(), mkdir(), unlink() working through FUSE
  - [ ] statfs() reports writable filesystem

- [ ] **Testing**
  - [ ] Unit tests for all write operations (20+ new tests)
  - [ ] FUSE write tests (5+ tests)
  - [ ] Concurrent access tests (5+ tests)
  - [ ] Crash recovery tests (3+ tests)

- [ ] **Quality**
  - [ ] Zero clippy warnings
  - [ ] All tests pass (50+ total)
  - [ ] Documentation complete and accurate

### Stable Release (v1.0.0)

- [ ] **Performance**
  - [ ] Write throughput documented (MB/s)
  - [ ] Latency benchmarks (P50, P99)
  - [ ] Concurrent write scalability tested

- [ ] **Production Hardening**
  - [ ] Crash recovery validated
  - [ ] Long-running stability tests
  - [ ] Memory leak tests

- [ ] **Ecosystem Integration**
  - [ ] Used by embeddenator-cli
  - [ ] Integrated with embeddenator-testkit
  - [ ] Documented in embeddenator-workspace

---

## Timeline

### Revised Timeline (Read-Write Focus)

**Week 1**: Assessment & Planning
- Audit read-only assumptions
- Light review of other repos
- Design write architecture
- Update high-level docs

**Weeks 2-3**: Foundation
- Implement WriteLog, transactions, locks
- Add data model changes
- Create basic write infrastructure

**Weeks 4-5**: Write Operations
- Implement chunk-level writes
- Implement file-level operations
- Implement directory operations

**Week 6**: FUSE Integration
- Remove EROFS
- Implement FUSE write operations
- Update FUSE attributes

**Week 7**: Testing
- Unit tests for writes
- FUSE write tests
- Concurrent access tests
- Crash recovery tests

**Week 8**: Documentation
- Create ARCHITECTURE_V2.md
- Update existing docs
- Update rustdoc comments

**Total**: 8 weeks to beta (read-write capable)

---

## Risk Assessment

### High Risk ⚠️⚠️

1. **VSA Differential Updates Not Supported**
   - Risk: embeddenator-vsa may not support in-place modifications
   - Mitigation: Review VSA crate early, coordinate changes
   - Fallback: Implement shadow structures for writes

2. **Hierarchical Write Complexity**
   - Risk: Parent-child update propagation may be complex
   - Mitigation: Understand embeddenator-retrieval early
   - Fallback: Disable hierarchical queries during writes

3. **Concurrency Bugs**
   - Risk: Deadlocks, race conditions in locking
   - Mitigation: Extensive testing, formal verification tools
   - Fallback: Single-writer mode initially

### Medium Risk ⚠️

4. **Performance Regression**
   - Risk: Write overhead may slow reads
   - Mitigation: Benchmark early, optimize hot paths

5. **Backward Compatibility**
   - Risk: Old engrams won't work
   - Mitigation: Version flag, migration tool

6. **Testing Complexity**
   - Risk: Concurrent/transactional tests are hard
   - Mitigation: Use property-based testing, fuzzing

---

## Next Steps (Pending Approval)

**Option 1: Immediate Start (Week 1)**
- Begin assessment and planning phase
- Audit read-only assumptions in code/docs
- Light review of embeddenator-cli, embeddenator-vsa, embeddenator-retrieval repos

**Option 2: Deep Review First**
- Thoroughly examine embeddenator-core, embeddenator-workspace
- Understand full distributed architecture
- Coordinate with other component owners
- Then proceed with implementation

**Option 3: Incremental Approach**
- Start with documentation fixes (remove "immutable" claims)
- Add write infrastructure (WriteLog, transactions)
- Implement writes incrementally, test at each step

---

## Summary

The transformation from read-only to read-write is a **fundamental architectural change** requiring:
- **8 weeks** to beta (read-write capable)
- **Coordination** with embeddenator-vsa, embeddenator-retrieval, embeddenator-cli
- **Extensive testing** for concurrency and crash recovery
- **Complete documentation overhaul**

This is the **correct path** for embeddenator-fs. The read-only assumption was a testing artifact, not the intended design.

**Awaiting your approval and guidance on:**
1. Which repos to review first (priorities)?
2. Should we coordinate with embeddenator-vsa team for differential update support?
3. Preferred concurrency model (pessimistic vs. optimistic vs. MVCC)?
4. Timeline constraints or urgency factors?

**Ready to execute once you authorize.**
