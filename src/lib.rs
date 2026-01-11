//! # embeddenator-fs
//!
//! EmbrFS: FUSE-based holographic filesystem.
//!
//! Extracted from embeddenator core as part of Phase 2A component decomposition.

pub mod fs;
pub use fs::*;

// Re-export common types from vsa and retrieval for convenience
pub use embeddenator_retrieval::resonator::Resonator;
pub use embeddenator_vsa::{ReversibleVSAConfig, SparseVec};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_loads() {
        // Verify core types are accessible
        let fs = EmbrFS::new();
        assert!(fs.engram.codebook.is_empty());
    }
}
