# embeddenator-fs Implementation Plan
**Date:** 2026-01-15
**Current Version:** v0.20.0-alpha.3
**Target Version:** v0.20.0-beta.1 (short-term), v1.0.0 (long-term)

---

## Executive Summary

**Status**: Main branch contains comprehensive documentation and integration tests. Core library is complete and stable (30 tests passing). **Critical gaps**: CLI tooling and examples.

**Immediate Goal**: Implement CLI and examples to reach beta (v0.20.0-beta.1) within 2-3 weeks.

---

## Current State Analysis

### ✅ Completed (Merged to main)
- **Core Library**: Fully functional with 100% bit-perfect reconstruction
- **Documentation Suite**: 4,000+ lines across 11 files
  - ARCHITECTURE.md, CORRECTION.md, FUSE.md, STATUS.md
  - GAP_ANALYSIS.md, ROADMAP.md, QUICKSTART.md
  - CONTRIBUTING.md, CHANGELOG.md, README.md
- **Testing**: 30 passing tests (20 unit + 10 integration + doc tests)
- **Project Files**: LICENSE (MIT), Cargo.toml with proper metadata
- **Version**: Bumped to 0.20.0-alpha.3

### 🔴 Critical Gaps (Blockers for beta)
1. **No CLI** - Library only, no executable
2. **No examples** - Zero usage examples
3. **1 clippy warning** - `level_bundle` unused assignment (line 1765)

### 🟡 Non-Critical Issues
- 6 clippy style suggestions (non-blocking)
- Some doc comments could be enhanced
- No benchmarks yet

### 📊 Branch Status
- **main**: Up to date with all merged work (✅ canonical)
- **dev**: Has different CI config (CODEOWNERS, dependabot, workflows)
- **release/alpha-publish**: Superseded by main
- **claude/* branches**: Work completed, can be cleaned up

---

## Implementation Plan

### Phase 1: Beta Release (Priority 0 - Next 2-3 Weeks)

#### Week 1: CLI Foundation

**Goal**: Working `embrfs` CLI with core commands

**Tasks**:

1. **Create CLI structure**
   ```bash
   mkdir -p src/bin src/cli
   ```
   - `src/bin/embrfs.rs` - Main entry point
   - `src/cli/mod.rs` - CLI module
   - `src/cli/args.rs` - Argument parsing with clap
   - `src/cli/commands.rs` - Command trait/interface

2. **Add dependencies**
   ```toml
   [dependencies]
   clap = { version = "4", features = ["derive", "cargo"] }
   anyhow = "1.0"  # Better error messages
   indicatif = "0.17"  # Progress bars
   ```

3. **Implement `ingest` command**
   ```rust
   embrfs ingest <input_dir> --output <engram> --manifest <json>
   ```
   - Directory traversal
   - Progress indication
   - Error handling
   - Stats output

4. **Implement `extract` command**
   ```rust
   embrfs extract <engram> --manifest <json> --output <dir>
   ```
   - Engram loading
   - File extraction
   - Verification
   - Progress bars

5. **Implement `stats` command**
   ```rust
   embrfs stats <engram> --manifest <json>
   ```
   - File count
   - Total size
   - Correction overhead
   - Chunk statistics

6. **Implement `compact` command**
   ```rust
   embrfs compact <engram> --manifest <json>
   ```
   - Remove deleted files
   - Reclaim space
   - Update manifest

7. **Add CLI documentation**
   - Update README.md with CLI examples
   - Add `--help` text for all commands
   - Create CLI section in QUICKSTART.md

**Deliverable**: Working `embrfs` binary installable via `cargo install --path .`

**Testing**: Manual testing with sample datasets, add CLI integration test

---

#### Week 2: Examples & Quality

**Goal**: 3+ working examples, fix warnings, polish documentation

**Tasks**:

1. **Create examples directory**
   ```bash
   mkdir -p examples/data
   ```

2. **Example 1: `basic_usage.rs`**
   ```rust
   // Simple ingest → extract workflow
   // - Create test files
   // - Ingest to engram
   // - Extract to new dir
   // - Verify contents
   ```

3. **Example 2: `incremental_updates.rs`**
   ```rust
   // Demonstrate add/modify/remove/compact
   // - Initial ingest
   // - Add new files
   // - Modify existing
   // - Remove old files
   // - Compact
   ```

4. **Example 3: `hierarchical.rs`**
   ```rust
   // Hierarchical bundling and query
   // - Ingest large dataset
   // - Create hierarchical structure
   // - Query with bounds
   // - Extract subset
   ```

5. **Example 4: `fuse_mount.rs` (with fuse feature)**
   ```rust
   // Mount engram as filesystem
   // - Load engram
   // - Mount at /mnt/embrfs
   // - Keep running until Ctrl-C
   // - Auto-unmount on exit
   ```

6. **Fix clippy warning**
   - Fix `level_bundle` unused assignment at line 1765
   - Verify build is clean

7. **Update documentation**
   - Add examples to README.md
   - Update STATUS.md with progress
   - Ensure all docs reference alpha.3

**Deliverable**: 4 working examples, clean build, updated docs

**Testing**: Verify each example compiles and runs successfully

---

#### Week 3: Testing & Release Prep

**Goal**: Comprehensive testing, beta release

**Tasks**:

1. **Add CLI integration tests**
   ```rust
   // tests/cli_tests.rs
   - Test ingest command
   - Test extract command
   - Test stats command
   - Test error handling
   ```

2. **Add example tests**
   ```bash
   # Verify examples compile
   cargo build --examples
   ```

3. **Documentation review**
   - Proofread all markdown files
   - Verify all links work
   - Check code examples compile

4. **Performance baseline**
   - Run ingest on 1GB dataset
   - Document throughput
   - Add to STATUS.md

5. **Release preparation**
   - Update CHANGELOG.md for beta.1
   - Bump version to 0.20.0-beta.1
   - Create git tag
   - Test `cargo publish --dry-run`

6. **Branch cleanup**
   - Archive/delete old claude/* branches
   - Sync dev with main if needed
   - Update PR templates

**Deliverable**: v0.20.0-beta.1 ready for release

**Success Criteria**:
- [ ] CLI works for all core workflows
- [ ] 4+ examples compile and run
- [ ] Zero clippy warnings
- [ ] All 30+ tests passing
- [ ] Documentation complete and accurate

---

### Phase 2: Post-Beta Enhancements (Weeks 4-6)

**Goal**: Observability, benchmarks, advanced features

#### Observability
- Add `tracing` for structured logging
- Instrument key operations
- Add `--verbose` flag to CLI

#### Benchmarks
- Create `benches/` directory
- Benchmark encoding (throughput)
- Benchmark query (latency)
- Benchmark hierarchical operations
- Document baselines

#### Advanced CLI Features
- `query` command for content search
- `mount` command for FUSE (wrapping existing)
- `verify` command for integrity checks
- Shell completion generation

#### Nice-to-Have
- Compression support (zstd feature)
- Streaming ingestion for large files
- Async variants (tokio feature)

---

### Phase 3: Stable Release (Weeks 7-8)

**Goal**: Production hardening, v1.0.0 preparation

#### Code Quality
- Comprehensive error handling audit
- Add retry logic where appropriate
- Security audit (no unsafe issues)
- Performance profiling

#### Documentation
- Add more examples
- Record screencasts for README
- Write migration guide (alpha → 1.0)
- Create comparison with traditional filesystems

#### Ecosystem Integration
- Verify compatibility with embeddenator-vsa
- Test with embeddenator-retrieval
- Document version matrix
- Coordinate release with other repos

#### Release Decision
- **Option A**: v0.20.0 (conservative, allows breaking changes)
- **Option B**: v1.0.0 (signals stability, commits to API)
- **Recommendation**: v0.20.0 first, v1.0.0 after production use

---

## Dependencies Analysis

### Current Dependencies (Production)
```toml
embeddenator-vsa = "0.20.0-alpha.1"        # Core VSA operations
embeddenator-retrieval = "0.20.0-alpha.1"  # Hierarchical queries
serde = "1.0"                              # Serialization
serde_json = "1.0"                         # JSON manifest
bincode = "1.3"                            # Engram serialization
walkdir = "2.3"                            # Directory traversal
arc-swap = "1.6"                           # Lock-free updates
rustc-hash = "2.0"                         # Fast hashing
sha2 = "0.10"                              # Correction hashes
fuser = "0.16" (optional)                  # FUSE integration
```

### Proposed Additions
```toml
# CLI (required for Phase 1)
clap = { version = "4", features = ["derive", "cargo"] }
anyhow = "1.0"          # Better error messages
indicatif = "0.17"      # Progress bars

# Future (Phase 2+)
tracing = "0.1"         # Structured logging
tracing-subscriber = "0.3"
criterion = "0.5"       # Benchmarking
zstd = { version = "0.13", optional = true }  # Compression
tokio = { version = "1", optional = true }     # Async
```

---

## Testing Strategy

### Current Coverage
- **Unit Tests**: 20 (correction, FUSE, filesystem core)
- **Integration Tests**: 10 (end-to-end workflows)
- **Doc Tests**: 14 (examples in rustdoc)
- **Total**: 44 tests, all passing

### Additional Testing Needed

#### CLI Tests
```rust
#[test]
fn test_ingest_command() {
    // Run: embrfs ingest test_data/
    // Verify: engram created, manifest valid
}

#[test]
fn test_extract_command() {
    // Run: embrfs extract test.engram
    // Verify: files extracted correctly
}

#[test]
fn test_error_handling() {
    // Run: embrfs extract nonexistent.engram
    // Verify: proper error message
}
```

#### Example Compilation Tests
```bash
# In CI
cargo build --examples --all-features
```

#### Property-Based Tests (Future)
```rust
proptest! {
    #[test]
    fn test_roundtrip(data in arbitrary_bytes()) {
        // Any data should roundtrip perfectly
        let engram = ingest(data);
        let recovered = extract(engram);
        assert_eq!(data, recovered);
    }
}
```

---

## Known Issues & Fixes

### Issue 1: Unused `level_bundle` Assignment
**Location**: `src/fs/embrfs.rs:1765`
**Severity**: Warning (non-blocking)
**Fix**:
```rust
// Current (warning):
level_bundle = level_bundle.thin(max_level_sparsity);

// Fix option 1 (use the result):
if sparsity_needed {
    level_bundle = level_bundle.thin(max_level_sparsity);
}

// Fix option 2 (remove if unused):
let _thinned = level_bundle.thin(max_level_sparsity);
// ... or remove entirely if not needed
```

### Issue 2: Hierarchical Query Integration
**Location**: `src/fs/embrfs.rs:1818-1890` (extract_hierarchically)
**Status**: Partial implementation
**Recommendation**: Defer to Phase 2 (not blocking beta)

### Issue 3: Doc Comment Correctness
**Status**: Minor issues in some examples
**Fix**: Audit during Week 2 documentation review

---

## Risk Assessment

### High Risk ⚠️
- **None identified** - Core library is stable, plan is conservative

### Medium Risk 🟡
1. **CLI API design**: First time exposing public CLI
   - Mitigation: Review against `ripgrep`, `fd`, `bat` for inspiration
   - Mitigation: Accept feedback in beta period

2. **Example complexity**: Need to be educational but not overwhelming
   - Mitigation: Start simple, add complexity gradually
   - Mitigation: Test with fresh eyes

3. **Timeline pressure**: 2-3 weeks is tight
   - Mitigation: Focus on P0 tasks only
   - Mitigation: Defer nice-to-haves to Phase 2

### Low Risk ✅
- Code quality (tests all pass)
- Documentation (comprehensive)
- Architecture (well-designed)

---

## Success Metrics

### Beta Release (v0.20.0-beta.1)
- [ ] CLI installed and works: `cargo install --path .`
- [ ] Can ingest 1GB directory in <5 minutes
- [ ] Can extract and verify bit-perfect reconstruction
- [ ] 4+ examples compile and run
- [ ] Zero clippy warnings
- [ ] All tests pass (30+)
- [ ] Documentation covers 90% of use cases

### Stable Release (v1.0.0)
- [ ] Used in at least 1 production deployment
- [ ] Zero critical bugs in tracker
- [ ] Performance baselines documented
- [ ] API stability guaranteed (semver)
- [ ] Comprehensive benchmarks

---

## Coordination with Other Repos

Based on your mention of 12 embeddenator repos, this plan should coordinate with:

### Core Dependencies
1. **embeddenator-vsa** (v0.20.0-alpha.1)
   - Verify API stability
   - Coordinate version bumps
   - Test integration

2. **embeddenator-retrieval** (v0.20.0-alpha.1)
   - Hierarchical query interface
   - Test with large datasets
   - Performance validation

### Testing
3. **embeddenator-testkit**
   - Centralized testing mentioned by user
   - Coordinate test data
   - Share test harness

### Developer Tools
4. **embeddenator-workspace**
   - Should have thorough documentation (per user)
   - Coordinate development setup
   - Sync CI/CD patterns

5. **embeddenator-obs** (observability?)
   - May provide telemetry integration
   - Future Phase 2 enhancement

### Other Components (mentioned)
6. **embeddenator-core** (the main repo)
7. **embeddenator-io**
8. **embeddenator-interop**
9. **+3 more** (specifics unknown)

**Recommendation**: Check embeddenator-core and embeddenator-workspace for:
- Architecture decisions that affect fs
- Shared patterns (error handling, logging, testing)
- Version coordination strategy
- CI/CD templates

---

## Open Questions

1. **CLI Installation**: Should we publish to crates.io with binary, or provide install script?
   - Recommendation: `cargo install embeddenator-fs` for now

2. **Async vs Sync**: Should we add async variants now or defer?
   - Recommendation: Defer to Phase 2, sync is simpler

3. **FUSE Default**: Should FUSE be default feature or optional?
   - Current: Optional (good - not all users need it)
   - Recommendation: Keep optional

4. **Version Coordination**: How do we coordinate versions across 12 repos?
   - Need to understand strategy from embeddenator-core
   - Should all be 0.20.0-alpha.3?

5. **Testing Strategy**: Is embeddenator-testkit ready for integration?
   - Need to examine testkit repo
   - Coordinate test data and fixtures

---

## Implementation Approach

### Development Process
1. **Feature Branch**: Create `feature/cli-and-examples` from main
2. **Incremental Commits**: Small, focused commits for each command
3. **CI Validation**: Tests must pass before merge
4. **Documentation Updates**: Update docs with each feature
5. **Review Points**:
   - After CLI foundation (Week 1)
   - After examples (Week 2)
   - Before beta release (Week 3)

### Git Strategy
```bash
# Week 1: CLI
git checkout -b feature/cli-foundation
# Implement CLI commands
git commit -m "feat: add CLI with ingest/extract/stats/compact commands"

# Week 2: Examples
git checkout -b feature/examples
# Create examples
git commit -m "docs: add 4 usage examples"

# Week 3: Polish
git checkout -b release/beta-1
# Final touches
git commit -m "chore: prepare v0.20.0-beta.1 release"
```

### CI/CD
- Run tests on every commit
- Build examples in CI
- Generate documentation
- Run clippy with `-D warnings`

---

## Next Steps (Pending Approval)

**Option 1: Start Immediately (Recommended)**
- Begin Phase 1, Week 1: CLI Foundation
- Create `src/bin/embrfs.rs` and CLI structure
- Implement `ingest` command first (most critical)

**Option 2: Review with embeddenator-core First**
- Examine embeddenator-core documentation
- Align CLI design with ecosystem patterns
- Then start implementation

**Option 3: Adjust Plan Based on Feedback**
- User provides additional context
- Modify priorities or approach
- Then proceed

---

## Summary

embeddenator-fs is **70% complete** with solid foundations. To reach beta:
- **2-3 weeks** focused work on CLI and examples
- **Conservative approach** - no risky refactors
- **Well-defined scope** - clear deliverables

The path forward is clear and achievable. Pending your approval, I'm ready to execute Phase 1, Week 1.

**Awaiting your approval to proceed.**
