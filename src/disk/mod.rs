//! Disk Image Support for VM Encoding
//!
//! This module provides native support for encoding virtual machine disk images
//! directly into engrams without requiring external mounting tools.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                        Disk Image Pipeline                          │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │                                                                     │
//! │  ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐  │
//! │  │  QCOW2/Raw   │───▶│  Partition   │───▶│  Filesystem          │  │
//! │  │  Image       │    │  Table       │    │  Traversal           │  │
//! │  │  (qcow2.rs)  │    │  (part.rs)   │    │  (filesystem.rs)     │  │
//! │  └──────────────┘    └──────────────┘    └──────────────────────┘  │
//! │         │                   │                      │               │
//! │         ▼                   ▼                      ▼               │
//! │  ┌──────────────────────────────────────────────────────────────┐  │
//! │  │                    Async Block Device API                    │  │
//! │  │              (tokio + tokio-uring when available)            │  │
//! │  └──────────────────────────────────────────────────────────────┘  │
//! │                              │                                     │
//! │                              ▼                                     │
//! │  ┌──────────────────────────────────────────────────────────────┐  │
//! │  │                      EmbrFS Encoder                          │  │
//! │  │              (compression profiles per path)                 │  │
//! │  └──────────────────────────────────────────────────────────────┘  │
//! │                                                                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Why Native Disk Image Support?
//!
//! 1. **No External Dependencies**: Avoids requiring `qemu-nbd`, `losetup`, or
//!    root privileges for mounting disk images.
//!
//! 2. **Async I/O Performance**: Uses `tokio-uring` on Linux for io_uring-based
//!    I/O which is 2-3x faster than traditional read/write syscalls for large
//!    sequential operations.
//!
//! 3. **Cross-Platform**: Works on Linux, macOS, and Windows without needing
//!    platform-specific block device utilities.
//!
//! 4. **VM Workflow Integration**: Enables encoding QCOW2 images directly from
//!    QEMU/KVM workflows without intermediate conversion steps.
//!
//! # Supported Formats
//!
//! | Format | Read | Write | Backing Files | Compression |
//! |--------|------|-------|---------------|-------------|
//! | QCOW2  | ✅   | ✅    | ✅            | ✅          |
//! | Raw    | ✅   | ✅    | N/A           | N/A         |
//!
//! # Supported Partition Tables
//!
//! | Type | Read | Write | Notes |
//! |------|------|-------|-------|
//! | GPT  | ✅   | ✅    | UEFI systems, disks >2TB |
//! | MBR  | ✅   | ✅    | Legacy systems, <2TB limit |
//!
//! # Supported Filesystems
//!
//! | Filesystem | Read | Write | Notes |
//! |------------|------|-------|-------|
//! | ext4       | ✅   | ❌    | Most common Linux FS |
//! | ext2/ext3  | ✅   | ❌    | Via ext4 compat |
//!
//! # Example Usage
//!
//! ```rust,ignore
//! use embeddenator_fs::disk::{DiskImage, ImageFormat};
//!
//! // Open a QCOW2 image
//! let image = DiskImage::open("vm.qcow2", ImageFormat::Qcow2).await?;
//!
//! // List partitions
//! for partition in image.partitions()? {
//!     println!("Partition {}: {} bytes", partition.name, partition.size);
//! }
//!
//! // Traverse filesystem and encode to engram
//! let encoder = image.encoder_for_partition(0)?;
//! encoder.encode_to_engram(&mut embrfs, |path, progress| {
//!     println!("{}: {:.1}%", path, progress * 100.0);
//! }).await?;
//! ```
//!
//! # Feature Flags
//!
//! - `disk-image`: Full support with `tokio-uring` (Linux only, fastest)
//! - `disk-image-portable`: Support without `tokio-uring` (cross-platform)

#[cfg(feature = "disk-image")]
pub mod qcow2;

#[cfg(feature = "disk-image")]
pub mod raw;

#[cfg(feature = "disk-image")]
pub mod partition;

#[cfg(feature = "disk-image")]
pub mod filesystem;

#[cfg(feature = "disk-image")]
mod error;

#[cfg(feature = "disk-image")]
pub use error::{DiskError, DiskResult};

#[cfg(feature = "disk-image")]
pub use qcow2::Qcow2Image;

#[cfg(feature = "disk-image")]
pub use raw::RawImage;

#[cfg(feature = "disk-image")]
pub use partition::{PartitionInfo, PartitionTable, PartitionType};

#[cfg(feature = "disk-image")]
pub use filesystem::FilesystemTraverser;

/// Disk image format detection and handling
#[cfg(feature = "disk-image")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// QCOW2 format (QEMU Copy-On-Write version 2)
    /// Most common format for KVM/QEMU virtual machines
    Qcow2,
    /// Raw disk image (direct byte-for-byte copy)
    /// Simplest format, no metadata overhead
    Raw,
    /// Auto-detect format from file header
    Auto,
}

#[cfg(feature = "disk-image")]
impl ImageFormat {
    /// Detect image format from file magic bytes
    ///
    /// # Why Magic Detection?
    ///
    /// File extensions are unreliable - users may name files incorrectly or
    /// convert between formats without renaming. Magic byte detection is
    /// foolproof and matches how `file(1)` and `qemu-img` identify formats.
    pub async fn detect(path: &std::path::Path) -> DiskResult<Self> {
        use tokio::io::AsyncReadExt;

        let mut file = tokio::fs::File::open(path).await?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).await?;

        // QCOW2 magic: "QFI\xfb" (0x514649fb in big-endian)
        if magic == [0x51, 0x46, 0x49, 0xfb] {
            return Ok(Self::Qcow2);
        }

        // Raw images have no magic - assume raw if not recognized
        // This is safe because we'll fail later if it's actually garbage
        Ok(Self::Raw)
    }
}

/// Unified disk image interface
///
/// # Why a Unified Interface?
///
/// Different image formats (QCOW2, raw) have different internal structures
/// but present the same logical view: a sequence of bytes representing a
/// block device. This trait abstracts over format differences so the
/// partition and filesystem layers don't need format-specific code.
#[cfg(feature = "disk-image")]
#[allow(async_fn_in_trait)]
pub trait BlockDevice: Send + Sync {
    /// Read bytes at the given offset
    ///
    /// # Why Offset-Based Reads?
    ///
    /// Block devices are random-access. Unlike files which are often read
    /// sequentially, disk images need random access to:
    /// - Jump to partition starts
    /// - Read filesystem metadata scattered across the disk
    /// - Follow inode pointers to data blocks
    async fn read_at(&self, buf: &mut [u8], offset: u64) -> DiskResult<usize>;

    /// Get total size in bytes
    fn size(&self) -> u64;

    /// Get logical block size (typically 512 or 4096)
    fn block_size(&self) -> u32 {
        512 // Default sector size
    }
}

/// Open a disk image with automatic format detection
///
/// # Example
///
/// ```rust,ignore
/// let image = open_image("vm.qcow2").await?;
/// println!("Image size: {} bytes", image.size());
/// ```
#[cfg(feature = "disk-image")]
pub async fn open_image(path: impl AsRef<std::path::Path>) -> DiskResult<Box<dyn BlockDevice>> {
    let path = path.as_ref();
    let format = ImageFormat::detect(path).await?;

    match format {
        ImageFormat::Qcow2 => {
            let img = Qcow2Image::open(path).await?;
            Ok(Box::new(img))
        }
        ImageFormat::Raw => {
            let img = RawImage::open(path).await?;
            Ok(Box::new(img))
        }
        ImageFormat::Auto => {
            // Auto already resolved in detect()
            unreachable!()
        }
    }
}
