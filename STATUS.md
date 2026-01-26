# embeddenator-fs Status

**Version:** 0.23.0
**crates.io:** [embeddenator-fs](https://crates.io/crates/embeddenator-fs)
**Last Updated:** 2026-01-26

---

## Overview

EmbrFS: FUSE-based holographic filesystem for encoding directory trees into high-dimensional sparse vectors with bit-perfect reconstruction.

---

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `fuse` | No | FUSE filesystem support |
| `async-streaming` | No | Async streaming with tokio |
| `disk-image` | No | QCOW2/raw disk image support |
| `disk-image-portable` | No | Portable disk image (no io_uring) |

---

## Test Coverage

- **Unit Tests:** 74 (highest in workspace)
- **Benchmarks:** 4 (filesystem, ingest, query, incremental)

---

## Key Components

| Component | Status | Description |
|-----------|--------|-------------|
| EmbrFS | Alpha | Core filesystem abstraction |
| Engram | Production | Holographic encoding container |
| Manifest | Production | File metadata and chunk mapping |
| Hierarchical | Production | Multi-level chunking |
| FUSE Mount | Alpha | Read-only FUSE integration |

---

## Known Limitations

- Large file reconstruction (>10MB) has degraded quality
- Deep hierarchy paths (>20 levels) may produce incorrect output
- FUSE mount is read-only

---

## Remaining Tasks

- [ ] FUSE write support
- [ ] Production hardening for FUSE
- [ ] Disk image support testing
- [ ] Large-scale (TB) validation

---

## Links

- **Documentation:** https://docs.rs/embeddenator-fs
- **Repository:** https://github.com/tzervas/embeddenator-fs
