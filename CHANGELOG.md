# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.20.0-alpha.2] - 2026-01-10

### Added
- Comprehensive documentation suite (LICENSE, CHANGELOG, CONTRIBUTING, docs/)
- Integration test suite with 10 end-to-end tests (all passing)
- Professional documentation with accurate limitations and capabilities
- Architecture documentation (ARCHITECTURE.md, FUSE.md, CORRECTION.md)
- Project status tracking (STATUS.md)

### Changed
- Enhanced README with detailed feature descriptions and realistic scope
- Updated inline documentation for accuracy and professionalism
- Code formatted with rustfmt for consistency

### Fixed
- Added missing MIT LICENSE file (previously only declared in Cargo.toml)
- Integration tests properly handle output directory creation
- Correction statistics test accounts for VSA variability

## [0.20.0-alpha.1] - 2026-01-09

### Changed
- **BREAKING**: Version bump from 0.2.0 to 0.20.0-alpha.1 for alignment with embeddenator ecosystem
- Converted path/git dependencies to published crates.io versions
  - `embeddenator-vsa = "0.20.0-alpha.1"`
  - `embeddenator-retrieval = "0.20.0-alpha.1"`
- Published to crates.io as alpha release

### Migration Guide
This is a significant version jump (0.2.0 → 0.20.0-alpha.1) to align versioning across the Embeddenator ecosystem. If you were using git dependencies, update to:

```toml
[dependencies]
embeddenator-fs = "0.20.0-alpha.1"
```

## [0.2.0] - 2024-12-XX (Monorepo Era)

### Added
- Complete holographic filesystem implementation (EmbrFS)
- Bit-perfect reconstruction with algebraic correction layer
- FUSE integration for kernel-level filesystem mounting (behind `fuse` feature flag)
- Hierarchical sub-engram architecture for scalability
- Incremental operations: add, modify, remove, compact
- Multiple correction strategies (BitFlips, TritFlips, BlockReplace, Verbatim)
- Comprehensive test suite (20 tests covering core functionality)
- Lock poisoning recovery for read operations

### Architecture
- `embrfs.rs`: Core holographic filesystem logic (1884 lines)
- `fuse_shim.rs`: FUSE kernel integration layer (1263 lines)
- `correction.rs`: Bit-perfect reconstruction system (531 lines)

### Limitations (Acknowledged)
- Read-only FUSE filesystem (write operations return EROFS by design)
- Holographic engrams are immutable snapshots; modifications require re-encoding
- No symbolic link support (returns ENOSYS)
- No extended attributes support

## [0.1.0] - 2024-XX-XX

### Added
- Initial extraction from [Embeddenator monorepo](https://github.com/tzervas/embeddenator)
- Phase 2A component decomposition (see ADR-016)
- Separated filesystem concerns from core VSA library
- Independent crate structure with modular dependencies

---

## Version Numbering Strategy

- **0.x.x-alpha.x**: Early alpha releases, API may change significantly
- **0.x.x-beta.x**: Feature-complete beta releases, API stabilizing
- **0.x.x**: Pre-1.0 releases with stable API within minor version
- **1.0.0**: First stable release with long-term API stability guarantees

## Semantic Versioning Notes

Given the innovative nature of holographic filesystems, we anticipate:
- **Minor version bumps** (0.x.0) for new encoding strategies or significant performance improvements
- **Patch version bumps** (0.x.y) for bug fixes and documentation improvements
- **Alpha/beta suffixes** during pre-release stabilization phases

---

[Unreleased]: https://github.com/tzervas/embeddenator-fs/compare/v0.20.0-alpha.2...HEAD
[0.20.0-alpha.2]: https://github.com/tzervas/embeddenator-fs/compare/v0.20.0-alpha.1...v0.20.0-alpha.2
[0.20.0-alpha.1]: https://github.com/tzervas/embeddenator-fs/compare/v0.2.0...v0.20.0-alpha.1
[0.2.0]: https://github.com/tzervas/embeddenator-fs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tzervas/embeddenator-fs/releases/tag/v0.1.0
