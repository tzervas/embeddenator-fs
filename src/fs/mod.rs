pub mod correction;
pub mod embrfs;
pub mod fuse_shim;
pub mod versioned;
pub mod versioned_embrfs;
pub mod versioned_fuse;

pub use correction::*;
pub use embrfs::*;
pub use fuse_shim::*;
pub use versioned::*;

// Re-export main types from versioned_embrfs (not all to avoid name conflicts)
pub use versioned_embrfs::{EmbrFSError, FilesystemStats, VersionedEmbrFS};

// Re-export compression types from embeddenator-io for convenience
pub use embeddenator_io::{CompressionCodec, CompressionProfile, CompressionProfiler};

#[cfg(feature = "fuse")]
pub use versioned_fuse::{mount_versioned_fs, VersionedFUSE};
