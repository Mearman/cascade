//! FUSE filesystem operations.
//!
//! On Linux, translates FUSE callbacks into VFS queries via the engine protocol.
//! On other platforms, provides compile-time stubs.

use cascade_engine::types::{ItemId, VfsItem};

use crate::inode::InodeMap;

/// Internal file attribute representation, independent of platform-specific FUSE types.
#[derive(Debug, Clone)]
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
    pub fn directory(inode: u64) -> Self {
        Self {
            inode,
            size: 0,
            is_dir: true,
            mode: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Build attributes for a regular file.
    pub fn file(inode: u64, size: u64) -> Self {
        Self {
            inode,
            size,
            is_dir: false,
            mode: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }
}

/// Convert a VfsItem to FileAttr using the inode from the map.
pub fn vfs_item_to_attr(item: &VfsItem, inode: u64) -> FileAttr {
    if item.is_dir {
        FileAttr::directory(inode)
    } else {
        FileAttr::file(inode, item.size.unwrap_or(0))
    }
}

/// State shared between FUSE operation handlers.
pub struct FuseOps {
    /// Inode ↔ ItemId mapping.
    pub inode_map: std::sync::Mutex<InodeMap>,
}

impl FuseOps {
    /// Create a new FuseOps with the given root ItemId.
    pub fn new(root_id: ItemId) -> Self {
        Self {
            inode_map: std::sync::Mutex::new(InodeMap::new(root_id)),
        }
    }
}

// --- Linux: implement fuser::Filesystem -----------------------------------
#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use fuser::{
        FileAttr as FuseFileAttr, FileHandle, FileType, Filesystem, INodeNo, KernelConfig,
        LockOwner, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyWrite, Request,
    };

    use super::*;

    impl From<FileAttr> for FuseFileAttr {
        fn from(attr: FileAttr) -> Self {
            let kind = if attr.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            FuseFileAttr {
                ino: attr.inode,
                size: attr.size,
                blocks: (attr.size + 511) / 512,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind,
                perm: attr.mode as u16,
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
            let map = self.inode_map.lock().unwrap();
            let name_str = name.to_string_lossy();

            // TODO: Query the VFS tree for the child with this name under parent.
            // For now, return ENOENT until engine integration is wired up.
            tracing::debug!(parent, name = %name_str, "lookup");
            drop(map);
            reply.error(libc::ENOENT);
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            let ino_u64: u64 = ino.into();
            let map = self.inode_map.lock().unwrap();
            if ino_u64 == crate::inode::ROOT_INODE {
                let attr = FileAttr::directory(ino_u64);
                reply.attr(&Duration::from_secs(1), attr.into());
                return;
            }
            // TODO: Look up VfsItem by inode and convert to attr.
            tracing::debug!(ino = ino_u64, "getattr");
            drop(map);
            reply.error(libc::ENOENT);
        }

        fn readdir(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            mut reply: ReplyDirectory,
        ) {
            let map = self.inode_map.lock().unwrap();
            let ino_u64: u64 = ino.into();
            if ino_u64 != crate::inode::ROOT_INODE {
                drop(map);
                reply.error(libc::ENOTDIR);
                return;
            }

            // Standard "." and ".." entries for root.
            if offset == 0 {
                reply.add(ino, 1, FileType::Directory, ".");
            }
            if offset <= 1 {
                reply.add(ino, 2, FileType::Directory, "..");
            }

            // TODO: Iterate VFS children of this directory and add entries.
            tracing::debug!(ino = ino_u64, offset, "readdir");
            drop(map);
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
            // TODO: Fetch file contents from cache/backend and return the requested range.
            tracing::debug!(ino = u64::from(ino), offset, size, "read");
            reply.error(libc::ENOENT);
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
            // TODO: Write data to the file at the given offset.
            tracing::debug!(ino = u64::from(ino), offset, len = data.len(), "write");
            reply.error(libc::EROFS);
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
            // TODO: Create a new file in the VFS.
            tracing::debug!(parent = u64::from(parent), name = %name.to_string_lossy(), "create");
            reply.error(libc::EROFS);
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
            // TODO: Create a new directory in the VFS.
            tracing::debug!(parent = u64::from(parent), name = %name.to_string_lossy(), "mkdir");
            reply.error(libc::EROFS);
        }

        fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
            // TODO: Delete a file from the VFS.
            tracing::debug!(parent = u64::from(parent), name = %name.to_string_lossy(), "unlink");
            reply.error(libc::EROFS);
        }

        fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
            // TODO: Delete a directory from the VFS.
            tracing::debug!(parent = u64::from(parent), name = %name.to_string_lossy(), "rmdir");
            reply.error(libc::EROFS);
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
            // TODO: Move/rename in the VFS.
            tracing::debug!(
                parent = u64::from(parent),
                name = %name.to_string_lossy(),
                newparent = u64::from(newparent),
                newname = %newname.to_string_lossy(),
                "rename"
            );
            reply.error(libc::EROFS);
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
            // TODO: Update file metadata in the VFS.
            tracing::debug!(ino = u64::from(ino), ?mode, ?uid, ?gid, ?size, "setattr");
            reply.error(libc::EROFS);
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
}
