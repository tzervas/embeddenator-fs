//! # embeddenator-fs
//!
//! EmbrFS: FUSE-based holographic filesystem.
//!
//! Extracted from embeddenator core as part of Phase 2A component decomposition.

pub mod fs;
pub use fs::*;

#[cfg(test)]
mod tests {
    #[test]
    fn component_loads() {
        assert!(true);
    }
}
