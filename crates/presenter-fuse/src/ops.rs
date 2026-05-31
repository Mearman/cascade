//! FUSE filesystem operations.
//!
//! On Linux, translates FUSE callbacks into VFS queries via the engine protocol.
//! On other platforms, provides compile-time stubs.

use cascade_engine::types::{ItemId, VfsItem};
use cascade_engine::vfs::VfsTree;

use crate::inode::InodeMap;

use std::sync::{Arc, RwLock};

/// Internal file attribute representation, independent of platform-specific FUSE types.
#[derive(Debug, Clone, Copy)]
pub struct FileAttr {
    pub inode: u64,
    pub size: u64,
    pub is_dir: bool,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

impl FileAttr {
    /// Build attributes for a directory.
    // On non-unix the body is entirely const (uid/gid are const stubs);
    // clippy would suggest `const fn`, but the unix body is not const so
    // we'd need duplicate cfg-gated impls. Easier to allow.
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn directory(inode: u64) -> Self {
        Self {
            inode,
            size: 0,
            is_dir: true,
            mode: 0o755,
            nlink: 2,
            uid: current_uid(),
            gid: current_gid(),
        }
    }

    /// Build attributes for a regular file.
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn file(inode: u64, size: u64) -> Self {
        Self {
            inode,
            size,
            is_dir: false,
            mode: 0o644,
            nlink: 1,
            uid: current_uid(),
            gid: current_gid(),
        }
    }
}

/// Resolve the calling process's effective UID. On Windows there is no
/// equivalent concept so the crate (which is a no-op outside Linux)
/// reports 0.
#[cfg(unix)]
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
const fn current_uid() -> u32 {
    0
}

#[cfg(not(unix))]
const fn current_gid() -> u32 {
    0
}

/// Convert a `VfsItem` to `FileAttr` using the inode from the map.
#[must_use]
pub fn vfs_item_to_attr(item: &VfsItem, inode: u64) -> FileAttr {
    if item.is_dir {
        FileAttr::directory(inode)
    } else {
        FileAttr::file(inode, item.size.unwrap_or(0))
    }
}

/// State shared between FUSE operation handlers.
pub struct FuseOps {
    /// Inode ↔ `ItemId` mapping.
    pub inode_map: std::sync::Mutex<InodeMap>,
    /// VFS tree for resolving paths to backends.
    pub vfs: Arc<RwLock<VfsTree>>,
}

impl std::fmt::Debug for FuseOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuseOps").finish_non_exhaustive()
    }
}

impl FuseOps {
    /// Create a new `FuseOps` with the given root `ItemId` (no VFS tree).
    #[must_use]
    pub fn new(root_id: ItemId) -> Self {
        Self {
            inode_map: std::sync::Mutex::new(InodeMap::new(root_id)),
            vfs: Arc::new(RwLock::new(VfsTree::new(Arc::new(
                cascade_engine::backend::NullBackend::new("null"),
            )))),
        }
    }

    /// Create a new `FuseOps` with the given root `ItemId` and VFS tree.
    pub fn new_with_vfs(root_id: ItemId, vfs: Arc<RwLock<VfsTree>>) -> Self {
        Self {
            inode_map: std::sync::Mutex::new(InodeMap::new(root_id)),
            vfs,
        }
    }

    /// Synchronously resolve a path through the VFS tree and get metadata.
    #[allow(dead_code)] // Used in #[cfg(target_os = "linux")] Filesystem impl
    fn metadata_sync(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<cascade_engine::types::FileEntry> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            let (backend, relative) = {
                let vfs = self
                    .vfs
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (backend, relative) = vfs.resolve(path);
                (Arc::clone(backend), relative)
            };
            backend.metadata(&relative).await
        })
    }

    /// Synchronously list a directory through the VFS tree.
    #[allow(dead_code)] // Used in #[cfg(target_os = "linux")] Filesystem impl
    #[allow(clippy::await_holding_lock)]
    fn readdir_sync(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<Vec<cascade_engine::types::DirEntry>> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            let vfs = self
                .vfs
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = vfs.read_dir(path).await;
            drop(vfs);
            result
        })
    }

    /// Synchronously read file data from the backend.
    #[allow(dead_code)] // Used in #[cfg(target_os = "linux")] Filesystem impl
    fn read_sync(&self, path: &std::path::Path, offset: u64, size: u32) -> anyhow::Result<Vec<u8>> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            let (backend, relative) = {
                let vfs = self
                    .vfs
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (backend, relative) = vfs.resolve(path);
                (Arc::clone(backend), relative)
            };
            let entry = backend.metadata(&relative).await?;
            let mut buf = Vec::new();
            backend.download(&entry, &mut buf).await?;

            let off = usize::try_from(offset).unwrap_or(usize::MAX);
            if off >= buf.len() {
                return Ok(Vec::new());
            }
            let remaining = buf.get(off..).unwrap_or_default();
            let end = usize::try_from(size)
                .unwrap_or(usize::MAX)
                .min(remaining.len());
            Ok(remaining.get(..end).unwrap_or_default().to_vec())
        })
    }
}

// --- Linux: implement fuser::Filesystem -----------------------------------
#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use fuser::{
        Errno, FileAttr as FuseFileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo,
        KernelConfig, LockOwner, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyWrite,
        Request,
    };

    use super::{FileAttr, FuseOps};

    impl From<FileAttr> for FuseFileAttr {
        fn from(attr: FileAttr) -> Self {
            let kind = if attr.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            Self {
                ino: INodeNo(attr.inode),
                size: attr.size,
                blocks: attr.size.div_ceil(512),
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind,
                perm: u16::try_from(attr.mode).unwrap_or(0o777),
                nlink: attr.nlink,
                uid: attr.uid,
                gid: attr.gid,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            }
        }
    }

    impl Filesystem for FuseOps {
        fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
            tracing::info!("FUSE filesystem initialised");
            Ok(())
        }

        fn destroy(&mut self) {
            tracing::info!("FUSE filesystem destroyed");
        }

        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            let name_str = name.to_string_lossy();
            let parent_u64 = u64::from(parent);
            tracing::debug!(parent = parent_u64, name = %name_str, "lookup");

            // Resolve parent ItemId from inode.
            let parent_id = {
                let map = self
                    .inode_map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                map.get_id(parent_u64).cloned()
            };

            let Some(parent_id) = parent_id else {
                reply.error(Errno::ENOENT);
                return;
            };

            // Build child path and try to resolve it.
            let child_path = format!("{}/{}", parent_id.0, name_str);
            match self.metadata_sync(std::path::Path::new(&child_path)) {
                Ok(entry) => {
                    let mut map = self
                        .inode_map
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let child_id = entry.id.clone();
                    let inode = map.allocate(child_id);
                    let attr = if entry.is_dir {
                        FileAttr::directory(inode)
                    } else {
                        FileAttr::file(inode, entry.size.unwrap_or(0))
                    };
                    let fuse_attr: FuseFileAttr = attr.into();
                    reply.entry(&Duration::from_secs(1), &fuse_attr, Generation(0));
                }
                Err(_) => {
                    reply.error(Errno::ENOENT);
                }
            }
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            let ino_u64 = u64::from(ino);
            let map = self
                .inode_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            if ino_u64 == crate::inode::ROOT_INODE {
                let attr = FileAttr::directory(ino_u64);
                let fuse_attr: FuseFileAttr = attr.into();
                reply.attr(&Duration::from_secs(1), &fuse_attr);
                return;
            }

            let Some(id) = map.get_id(ino_u64) else {
                drop(map);
                reply.error(Errno::ENOENT);
                return;
            };
            let id_str = id.0.clone();
            drop(map);

            match self.metadata_sync(std::path::Path::new(&id_str)) {
                Ok(entry) => {
                    let attr = if entry.is_dir {
                        FileAttr::directory(ino_u64)
                    } else {
                        FileAttr::file(ino_u64, entry.size.unwrap_or(0))
                    };
                    let fuse_attr: FuseFileAttr = attr.into();
                    reply.attr(&Duration::from_secs(1), &fuse_attr);
                }
                Err(_) => {
                    reply.error(Errno::ENOENT);
                }
            }
        }

        fn readdir(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            mut reply: ReplyDirectory,
        ) {
            let ino_u64 = u64::from(ino);
            let map = self
                .inode_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            if ino_u64 != crate::inode::ROOT_INODE {
                // Only root is a directory for now.
                // TODO: support nested directories.
                drop(map);
                reply.error(Errno::ENOTDIR);
                return;
            }

            if offset == 0 {
                let _ = reply.add(ino, 1, FileType::Directory, ".");
            }
            if offset <= 1 {
                let _ = reply.add(ino, 2, FileType::Directory, "..");
            }

            // List children from the VFS tree.
            let Some(id) = map.get_id(ino_u64) else {
                drop(map);
                reply.error(Errno::ENOENT);
                return;
            };
            let id_str = id.0.clone();
            drop(map);

            let Ok(entries) = self.readdir_sync(std::path::Path::new(&id_str)) else {
                reply.ok();
                return;
            };

            let mut entry_offset = 3u64; // . and .. take 1 and 2
            for entry in &entries {
                entry_offset += 1;
                if entry_offset <= offset {
                    continue;
                }
                let kind = if entry.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                let _ = reply.add(
                    INodeNo(ino_u64 + entry_offset),
                    entry_offset,
                    kind,
                    &entry.name,
                );
            }

            reply.ok();
        }

        fn read(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            size: u32,
            _flags: fuser::OpenFlags,
            _lock_owner: Option<LockOwner>,
            reply: ReplyData,
        ) {
            let ino_u64 = u64::from(ino);
            tracing::debug!(ino = ino_u64, offset, size, "read");

            let path = {
                let map = self
                    .inode_map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                map.get_id(ino_u64).map(|id| id.0.clone())
            };

            let Some(path) = path else {
                reply.error(Errno::ENOENT);
                return;
            };

            match self.read_sync(std::path::Path::new(&path), offset, size) {
                Ok(data) => reply.data(&data),
                Err(_) => reply.error(Errno::EIO),
            }
        }

        fn write(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            data: &[u8],
            _write_flags: fuser::WriteFlags,
            _flags: fuser::OpenFlags,
            _lock_owner: Option<LockOwner>,
            reply: ReplyWrite,
        ) {
            tracing::debug!(ino = u64::from(ino), offset, len = data.len(), "write");
            // Phase 1: read-only filesystem.
            reply.error(Errno::EROFS);
        }

        fn create(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            reply: fuser::ReplyCreate,
        ) {
            tracing::debug!(
                parent = u64::from(parent),
                name = %name.to_string_lossy(),
                "create"
            );
            reply.error(Errno::EROFS);
        }

        fn mkdir(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            tracing::debug!(
                parent = u64::from(parent),
                name = %name.to_string_lossy(),
                "mkdir"
            );
            reply.error(Errno::EROFS);
        }

        fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
            tracing::debug!(
                parent = u64::from(parent),
                name = %name.to_string_lossy(),
                "unlink"
            );
            reply.error(Errno::EROFS);
        }

        fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
            tracing::debug!(
                parent = u64::from(parent),
                name = %name.to_string_lossy(),
                "rmdir"
            );
            reply.error(Errno::EROFS);
        }

        fn rename(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            newparent: INodeNo,
            newname: &OsStr,
            _flags: fuser::RenameFlags,
            reply: fuser::ReplyEmpty,
        ) {
            tracing::debug!(
                parent = u64::from(parent),
                name = %name.to_string_lossy(),
                newparent = u64::from(newparent),
                newname = %newname.to_string_lossy(),
                "rename"
            );
            reply.error(Errno::EROFS);
        }

        fn setattr(
            &self,
            _req: &Request,
            ino: INodeNo,
            mode: Option<u32>,
            uid: Option<u32>,
            gid: Option<u32>,
            size: Option<u64>,
            _atime: Option<fuser::TimeOrNow>,
            _mtime: Option<fuser::TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<FileHandle>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<fuser::BsdFileFlags>,
            reply: ReplyAttr,
        ) {
            tracing::debug!(ino = u64::from(ino), ?mode, ?uid, ?gid, ?size, "setattr");
            reply.error(Errno::EROFS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::types::CacheState;

    #[test]
    fn file_attr_directory() {
        let attr = FileAttr::directory(1);
        assert_eq!(attr.inode, 1);
        assert!(attr.is_dir);
        assert_eq!(attr.mode, 0o755);
        assert_eq!(attr.nlink, 2);
    }

    #[test]
    fn file_attr_regular_file() {
        let attr = FileAttr::file(42, 1024);
        assert_eq!(attr.inode, 42);
        assert!(!attr.is_dir);
        assert_eq!(attr.size, 1024);
        assert_eq!(attr.mode, 0o644);
        assert_eq!(attr.nlink, 1);
    }

    #[test]
    fn vfs_item_to_attr_file() {
        let item = VfsItem {
            id: ItemId::new("gdrive", "file1"),
            parent_id: ItemId::new("gdrive", "root"),
            name: "test.txt".to_string(),
            is_dir: false,
            size: Some(2048),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        let attr = vfs_item_to_attr(&item, 5);
        assert!(!attr.is_dir);
        assert_eq!(attr.size, 2048);
        assert_eq!(attr.inode, 5);
    }

    #[test]
    fn vfs_item_to_attr_directory() {
        let item = VfsItem {
            id: ItemId::new("gdrive", "docs"),
            parent_id: ItemId::new("gdrive", "root"),
            name: "Documents".to_string(),
            is_dir: true,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        let attr = vfs_item_to_attr(&item, 3);
        assert!(attr.is_dir);
        assert_eq!(attr.size, 0);
    }

    #[test]
    fn fuse_ops_new_with_vfs() {
        let root = ItemId::new("gdrive", "root");
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
            cascade_engine::backend::NullBackend::new("test"),
        ))));
        let ops = FuseOps::new_with_vfs(root.clone(), vfs);
        let map = ops.inode_map.lock().unwrap();
        assert_eq!(map.get_inode(&root), Some(crate::inode::ROOT_INODE));
    }
}
