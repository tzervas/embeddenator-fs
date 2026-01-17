//! FUSE adapter for VersionedEmbrFS with write support
//!
//! This module provides FUSE integration for the mutable VersionedEmbrFS,
//! enabling full read-write filesystem operations through FUSE.
//!
//! Unlike the original read-only EngramFS, this supports:
//! - Writing files (create, update)
//! - Creating files
//! - Deleting files
//! - Truncating files
//! - All with optimistic locking and version tracking

use crate::versioned_embrfs::VersionedEmbrFS;
use crate::{DirEntry, FileAttr, FileKind, Ino, ROOT_INO};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

#[cfg(feature = "fuse")]
use fuser::{
    FileType, Filesystem, KernelConfig, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
#[cfg(feature = "fuse")]
use std::ffi::OsStr;

/// FUSE adapter for VersionedEmbrFS
///
/// Provides full read-write FUSE filesystem on top of VersionedEmbrFS.
/// Manages inode assignments, directory structures, and FUSE protocol translation.
#[allow(dead_code)]
pub struct VersionedFUSE {
    /// The underlying versioned filesystem
    fs: Arc<VersionedEmbrFS>,

    /// Inode to path mapping
    inode_paths: Arc<RwLock<HashMap<Ino, String>>>,

    /// Path to inode mapping
    path_inodes: Arc<RwLock<HashMap<String, Ino>>>,

    /// Directory structure (parent_ino -> child entries)
    directories: Arc<RwLock<HashMap<Ino, Vec<DirEntry>>>>,

    /// Next available inode number
    next_ino: Arc<RwLock<Ino>>,

    /// File handle counter
    next_fh: Arc<RwLock<u64>>,

    /// Open file handles (fh -> (path, version))
    open_files: Arc<RwLock<HashMap<u64, (String, u64)>>>,

    /// TTL for attributes
    attr_ttl: Duration,

    /// TTL for directory entries
    entry_ttl: Duration,
}

#[allow(dead_code)]
impl VersionedFUSE {
    /// Create a new FUSE adapter for a versioned filesystem
    pub fn new(fs: VersionedEmbrFS) -> Self {
        let mut adapter = Self {
            fs: Arc::new(fs),
            inode_paths: Arc::new(RwLock::new(HashMap::new())),
            path_inodes: Arc::new(RwLock::new(HashMap::new())),
            directories: Arc::new(RwLock::new(HashMap::new())),
            next_ino: Arc::new(RwLock::new(ROOT_INO + 1)),
            next_fh: Arc::new(RwLock::new(1)),
            open_files: Arc::new(RwLock::new(HashMap::new())),
            attr_ttl: Duration::from_secs(1),
            entry_ttl: Duration::from_secs(1),
        };

        // Initialize root directory
        adapter.init_root();

        // Scan filesystem and build inode structure
        adapter.rebuild_directory_structure();

        adapter
    }

    /// Initialize root directory inode
    fn init_root(&mut self) {
        let mut inode_paths = self.inode_paths.write().unwrap();
        let mut path_inodes = self.path_inodes.write().unwrap();
        let mut directories = self.directories.write().unwrap();

        inode_paths.insert(ROOT_INO, "/".to_string());
        path_inodes.insert("/".to_string(), ROOT_INO);
        directories.insert(ROOT_INO, Vec::new());
    }

    /// Rebuild the directory structure from filesystem contents
    fn rebuild_directory_structure(&mut self) {
        let files = self.fs.list_files();

        for path in files {
            self.register_file(&path);
        }
    }

    /// Register a file and all its parent directories
    fn register_file(&self, path: &str) {
        // Split path into components
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Register each directory level
        let mut current_path = String::new();
        for (i, part) in parts.iter().enumerate() {
            let parent_path = current_path.clone();
            current_path = if current_path.is_empty() {
                format!("/{}", part)
            } else {
                format!("{}/{}", current_path, part)
            };

            // Check if this is the file (last component) or a directory
            let is_file = i == parts.len() - 1;

            // Get or allocate inode
            let ino = self.get_or_create_inode(&current_path);

            // Add to parent directory
            let parent_ino = if parent_path.is_empty() {
                ROOT_INO
            } else {
                self.get_or_create_inode(&parent_path)
            };

            let mut directories = self.directories.write().unwrap();
            let parent_entries = directories.entry(parent_ino).or_default();

            // Check if entry already exists
            if !parent_entries.iter().any(|e| e.name == *part) {
                parent_entries.push(DirEntry {
                    ino,
                    name: part.to_string(),
                    kind: if is_file {
                        FileKind::RegularFile
                    } else {
                        FileKind::Directory
                    },
                });
            }

            // If it's a directory, ensure it has an entry in directories map
            if !is_file {
                directories.entry(ino).or_default();
            }
        }
    }

    /// Get existing inode or create new one for a path
    fn get_or_create_inode(&self, path: &str) -> Ino {
        let path_inodes = self.path_inodes.read().unwrap();
        if let Some(&ino) = path_inodes.get(path) {
            return ino;
        }
        drop(path_inodes);

        // Allocate new inode
        let mut next_ino = self.next_ino.write().unwrap();
        let ino = *next_ino;
        *next_ino += 1;
        drop(next_ino);

        // Register it
        let mut path_inodes = self.path_inodes.write().unwrap();
        let mut inode_paths = self.inode_paths.write().unwrap();

        path_inodes.insert(path.to_string(), ino);
        inode_paths.insert(ino, path.to_string());

        ino
    }

    /// Get path for an inode
    fn get_path(&self, ino: Ino) -> Option<String> {
        let inode_paths = self.inode_paths.read().unwrap();
        inode_paths.get(&ino).cloned()
    }

    /// Get inode for a path
    fn get_inode(&self, path: &str) -> Option<Ino> {
        let path_inodes = self.path_inodes.read().unwrap();
        path_inodes.get(path).copied()
    }

    /// Create file attributes from file info
    fn make_file_attr(&self, ino: Ino, path: &str) -> FileAttr {
        let now = SystemTime::now();

        // Check if it's a directory
        let directories = self.directories.read().unwrap();
        if directories.contains_key(&ino) {
            return FileAttr {
                ino,
                size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                kind: FileKind::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 1000,
                gid: 1000,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            };
        }

        // It's a file - get its size
        let size = if let Ok((data, _version)) = self.fs.read_file(path) {
            data.len() as u64
        } else {
            0
        };

        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(4096),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileKind::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Allocate a file handle
    fn allocate_fh(&self, path: String, version: u64) -> u64 {
        let mut next_fh = self.next_fh.write().unwrap();
        let fh = *next_fh;
        *next_fh += 1;
        drop(next_fh);

        let mut open_files = self.open_files.write().unwrap();
        open_files.insert(fh, (path, version));

        fh
    }

    /// Release a file handle
    fn release_fh(&self, fh: u64) -> Option<(String, u64)> {
        let mut open_files = self.open_files.write().unwrap();
        open_files.remove(&fh)
    }
}

#[cfg(feature = "fuse")]
impl Filesystem for VersionedFUSE {
    fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), libc::c_int> {
        Ok(())
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Get parent path
        let parent_path = match self.get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Construct child path
        let child_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        // Get or create inode
        let ino = match self.get_inode(&child_path) {
            Some(i) => i,
            None => {
                // File doesn't exist yet
                reply.error(libc::ENOENT);
                return;
            }
        };

        let attr = self.make_file_attr(ino, &child_path);
        reply.entry(&self.entry_ttl, &attr.into(), 0);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let attr = self.make_file_attr(ino, &path);
        reply.attr(&self.attr_ttl, &attr.into());
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Read file data
        let data = match self.fs.read_file(&path) {
            Ok((data, _version)) => data,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        // Return requested slice
        let offset = offset as usize;
        let size = size as usize;
        let end = (offset + size).min(data.len());

        if offset >= data.len() {
            reply.data(&[]);
        } else {
            reply.data(&data[offset..end]);
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let directories = self.directories.read().unwrap();
        let entries = match directories.get(&ino) {
            Some(e) => e,
            None => {
                reply.error(libc::ENOTDIR);
                return;
            }
        };

        // Always include . and ..
        let mut all_entries = vec![
            DirEntry {
                ino,
                name: ".".to_string(),
                kind: FileKind::Directory,
            },
            DirEntry {
                ino: if ino == ROOT_INO { ROOT_INO } else { ROOT_INO }, // TODO: track real parent
                name: "..".to_string(),
                kind: FileKind::Directory,
            },
        ];
        all_entries.extend_from_slice(entries);

        for (i, entry) in all_entries.iter().enumerate().skip(offset as usize) {
            let full = reply.add(entry.ino, (i + 1) as i64, entry.kind.into(), &entry.name);

            if full {
                break;
            }
        }

        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Get current file version
        let version = match self.fs.read_file(&path) {
            Ok((_data, version)) => version,
            Err(EmbrFSError::FileNotFound(_)) => 0, // New file
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        // Allocate file handle
        let fh = self.allocate_fh(path, version);
        reply.opened(fh, flags as u32);
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.release_fh(fh);
        reply.ok();
    }

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
        // Get path and version from file handle
        let (path, file_version) = match self.open_files.read().unwrap().get(&fh) {
            Some(info) => info.clone(),
            None => {
                reply.error(libc::EBADF);
                return;
            }
        };

        // Read current file data
        let mut file_data = match self.fs.read_file(&path) {
            Ok((data, _ver)) => data,
            Err(EmbrFSError::FileNotFound(_)) => Vec::new(), // New file
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        // Apply write at offset
        let offset = offset as usize;
        if offset + data.len() > file_data.len() {
            file_data.resize(offset + data.len(), 0);
        }
        file_data[offset..offset + data.len()].copy_from_slice(data);

        // Write back with optimistic locking
        let expected_version = if file_data.len() == data.len() {
            None // New file
        } else {
            Some(file_version)
        };

        match self.fs.write_file(&path, &file_data, expected_version) {
            Ok(new_version) => {
                // Update file handle version
                let mut open_files = self.open_files.write().unwrap();
                if let Some(info) = open_files.get_mut(&fh) {
                    info.1 = new_version;
                }
                reply.written(data.len() as u32);
            }
            Err(EmbrFSError::VersionMismatch { .. }) => {
                // Concurrent modification - retry needed
                reply.error(libc::EAGAIN);
            }
            Err(_) => {
                reply.error(libc::EIO);
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        // Get parent path
        let parent_path = match self.get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Construct file path
        let file_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        // Create empty file
        match self.fs.write_file(&file_path, &[], None) {
            Ok(version) => {
                // Register in inode structure
                self.register_file(&file_path);

                let ino = self.get_inode(&file_path).unwrap();
                let attr = self.make_file_attr(ino, &file_path);
                let fh = self.allocate_fh(file_path, version);

                reply.created(&self.entry_ttl, &attr.into(), 0, fh, flags as u32);
            }
            Err(_) => {
                reply.error(libc::EIO);
            }
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        // Get parent path
        let parent_path = match self.get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Construct file path
        let file_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        // Get current file version
        let file_version = match self.fs.read_file(&file_path) {
            Ok((_data, version)) => version,
            Err(_) => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Delete file
        match self.fs.delete_file(&file_path, file_version) {
            Ok(_) => {
                // Remove from directory structure
                let mut directories = self.directories.write().unwrap();
                if let Some(entries) = directories.get_mut(&parent) {
                    entries.retain(|e| e.name != name_str);
                }
                reply.ok();
            }
            Err(EmbrFSError::VersionMismatch { .. }) => {
                reply.error(libc::EAGAIN);
            }
            Err(_) => {
                reply.error(libc::EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Handle truncate (size change)
        if let Some(new_size) = size {
            let path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            // Read current file
            let (mut data, version) = match self.fs.read_file(&path) {
                Ok(result) => result,
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            // Resize
            data.resize(new_size as usize, 0);

            // Write back
            match self.fs.write_file(&path, &data, Some(version)) {
                Ok(_) => {
                    let attr = self.make_file_attr(ino, &path);
                    reply.attr(&self.attr_ttl, &attr.into());
                }
                Err(_) => {
                    reply.error(libc::EIO);
                }
            }
        } else {
            // No size change - just return current attributes
            let path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };
            let attr = self.make_file_attr(ino, &path);
            reply.attr(&self.attr_ttl, &attr.into());
        }
    }
}

/// Mount a VersionedEmbrFS as a FUSE filesystem
#[cfg(feature = "fuse")]
pub fn mount_versioned_fs(
    fs: VersionedEmbrFS,
    mountpoint: &std::path::Path,
    options: &[fuser::MountOption],
) -> std::io::Result<()> {
    let fuse_fs = VersionedFUSE::new(fs);
    fuser::mount2(fuse_fs, mountpoint, options)
}
