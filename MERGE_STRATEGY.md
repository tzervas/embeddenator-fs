# Merge Strategy: release/alpha-publish → main

## Current State

### Branch Status

| Branch | Version | Key Content |
|--------|---------|-------------|
| **main** | 0.20.0-alpha.1 | - Basic codebase<br>- Formatting fixes (PR#2)<br>- Re-exports in lib.rs |
| **release/alpha-publish** | 0.20.0-alpha.2 | - 📚 Full docs/ directory (4 files)<br>- ✅ Integration tests<br>- 📝 CHANGELOG.md<br>- 🤝 CONTRIBUTING.md (552 lines)<br>- ⚖️ LICENSE<br>- ❌ Removed re-exports from lib.rs |
| **dev** | 0.20.0-alpha.1 | - All of main<br>- GAP_ANALYSIS.md<br>- ROADMAP.md<br>- QUICKSTART.md<br>- Enhanced CI |

## Strategic Analysis

### ✅ What to Accept from release/alpha-publish

1. **Documentation** (ALL - high value, no conflicts):
   - `docs/ARCHITECTURE.md` (541 lines) - System design
   - `docs/CORRECTION.md` (536 lines) - Correction layer details
   - `docs/FUSE.md` (612 lines) - FUSE implementation
   - `docs/STATUS.md` (270 lines) - Project status

2. **Testing** (ACCEPT):
   - `tests/integration_tests.rs` (412 lines) - 10 integration tests

3. **Project Files** (ACCEPT with modification):
   - `CHANGELOG.md` (99 lines) - Version history
   - `CONTRIBUTING.md` (552 lines) - Full guidelines (replaces dev's 30-line version)
   - `LICENSE` (21 lines) - MIT license
   - `README.md` (enhanced) - Better feature descriptions

4. **Version Bump** (⚠️ DISCUSS):
   - Cargo.toml: alpha.1 → alpha.2

### ❌ What to REJECT from release/alpha-publish

1. **src/lib.rs changes** (REGRESSION):
   - **Current (main)**: Has re-exports for convenience
     ```rust
     pub use embeddenator_retrieval::resonator::Resonator;
     pub use embeddenator_vsa::{ReversibleVSAConfig, SparseVec};
     ```
   - **release/alpha-publish**: REMOVES these re-exports
   - **Decision**: KEEP main's version (re-exports are good for library users)

2. **Test simplification** (REGRESSION):
   - **Current (main)**: Meaningful test
     ```rust
     let fs = EmbrFS::new();
     assert!(fs.engram.codebook.is_empty());
     ```
   - **release/alpha-publish**: Trivial `assert!(true)`
   - **Decision**: KEEP main's version

### 🔄 What to MERGE from dev

After merging release/alpha-publish, we need to add from dev:
1. `GAP_ANALYSIS.md` - My comprehensive gap analysis
2. `ROADMAP.md` - Implementation roadmap
3. `QUICKSTART.md` - Future-looking user guide
4. Enhanced `.github/workflows/ci.yml` - Better CI config

## Merge Strategy

### Step 1: Selective Merge of release/alpha-publish

```bash
# Start from main
git checkout main

# Merge release/alpha-publish but DON'T commit yet
git merge --no-commit --no-ff origin/release/alpha-publish

# REVERT the unwanted changes to lib.rs
git checkout HEAD -- src/lib.rs

# Check if there are doc comment issues in embrfs.rs
# (The diff shows `use embeddenator::` instead of `use embeddenator_fs::`)
# This is CORRECT since users will `use embeddenator_fs::...`
# But let's verify...
```

### Step 2: Investigate Doc Comment Changes

The release branch has doc comments like:
```rust
/// use embeddenator::EmbrFS;
```

But the crate name is `embeddenator-fs`, so users would actually do:
```rust
use embeddenator_fs::EmbrFS;
```

**Decision**: Need to verify which is correct. If crate publishes as `embeddenator` (without `-fs`), then release branch is correct. Otherwise, need to fix doc comments.

### Step 3: Version Number Decision

**Options:**
- **A) Keep alpha.2** (from release branch)
  - Pro: Acknowledges the work done
  - Con: Gap analysis shows we're not ready for alpha.2

- **B) Revert to alpha.1**
  - Pro: Honest about current state
  - Con: Loses the version bump signal

- **C) Bump to alpha.3**
  - Pro: Clean slate, acknowledges both additions
  - Con: Another version bump without much code change

**Recommendation**: Keep alpha.2 BUT update STATUS.md and CHANGELOG.md to reflect actual current state (including gap analysis findings).

### Step 4: Add Gap Analysis Docs

```bash
# Cherry-pick or copy from dev
git show origin/dev:GAP_ANALYSIS.md > GAP_ANALYSIS.md
git show origin/dev:ROADMAP.md > ROADMAP.md
git show origin/dev:QUICKSTART.md > QUICKSTART.md
git add GAP_ANALYSIS.md ROADMAP.md QUICKSTART.md
```

### Step 5: Update STATUS.md

Modify `docs/STATUS.md` to incorporate findings from GAP_ANALYSIS.md:
- Add "Critical Gaps" section
- Update statistics
- Add link to GAP_ANALYSIS.md and ROADMAP.md

### Step 6: Update CHANGELOG.md

Add entry for the gap analysis work:
```markdown
### Added
- Comprehensive gap analysis documentation (GAP_ANALYSIS.md)
- Implementation roadmap (ROADMAP.md)
- Future-looking quick start guide (QUICKSTART.md)
```

## Execution Plan

### Phase 1: Merge release/alpha-publish (with fixes)

1. `git checkout main`
2. `git merge --no-commit --no-ff origin/release/alpha-publish`
3. `git checkout HEAD -- src/lib.rs` (keep re-exports)
4. Verify doc comments in embrfs.rs (check crate name)
5. `git commit -m "Merge release/alpha-publish: add docs, tests, and project files"`

### Phase 2: Add gap analysis

1. Copy GAP_ANALYSIS.md, ROADMAP.md, QUICKSTART.md from dev
2. Update docs/STATUS.md to reference gap analysis
3. Update CHANGELOG.md
4. `git commit -m "docs: add gap analysis, roadmap, and quickstart"`

### Phase 3: Sync with dev

1. `git checkout dev`
2. `git merge main` (fast-forward or merge)
3. `git push origin dev`

### Phase 4: Final push

1. `git checkout main`
2. `git push origin main`

## Questions to Resolve

1. **Crate name**: Is it published as `embeddenator` or `embeddenator-fs`?
   - Check Cargo.toml `name` field
   - Affects doc comments throughout

2. **Version strategy**: Confirm alpha.2 is appropriate
   - Does the added documentation justify a version bump?
   - Should we wait for CLI implementation before alpha.2?

3. **CONTRIBUTING.md**: Keep the comprehensive 552-line version from release branch?
   - Replaces the basic 30-line version on dev
   - Decision: YES, more complete is better

## Risk Assessment

### Low Risk ✅
- Merging documentation files (no code impact)
- Adding integration tests (improves quality)
- Adding LICENSE (required for crates.io)

### Medium Risk ⚠️
- Version bump (signal to users)
- CHANGELOG updates (must be accurate)
- Doc comment corrections (affects user experience)

### High Risk ❌
- Accidentally removing re-exports (would break user code)
- Merging broken tests (CI would fail)

**Mitigation**: Keep main's src/lib.rs, run tests before pushing.

---

## Final Recommendation

**Proceed with selective merge:**
1. ✅ Accept all documentation
2. ✅ Accept tests
3. ✅ Accept CHANGELOG, CONTRIBUTING, LICENSE
4. ❌ Reject src/lib.rs changes (keep main's re-exports)
5. ✅ Keep version alpha.2
6. ✅ Add gap analysis docs afterward
7. ✅ Update STATUS.md to acknowledge gaps

**Timeline**: Execute immediately, no blockers identified.
