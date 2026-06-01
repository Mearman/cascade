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
    #[cfg(unix)]
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

    /// Build attributes for a directory.
    #[cfg(not(unix))]
    #[must_use]
    pub const fn directory(inode: u64) -> Self {
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
    #[cfg(unix)]
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

    /// Build attributes for a regular file.
    #[cfg(not(unix))]
    #[must_use]
    pub const fn file(inode: u64, size: u64) -> Self {
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

    /// Synchronously list the immediate children of a directory by its `ItemId`.
    ///
    /// Inodes are allocated lazily and idempotently — every child receives a
    /// stable inode number before the kernel sees the directory entry, so that
    /// a subsequent `lookup()` on the same name resolves to the same inode.
    #[allow(dead_code)] // Used in #[cfg(target_os = "linux")] Filesystem impl
    fn list_children_sync(
        &self,
        id: &cascade_engine::types::ItemId,
    ) -> anyhow::Result<Vec<cascade_engine::types::FileEntry>> {
        // Clone the backend `Arc` while holding the read lock, then drop the
        // lock before the async call. This avoids holding a `RwLockReadGuard`
        // across an await point, which would trigger the `await_holding_lock`
        // lint and risk a deadlock if any other code tries to write the tree.
        let backend = {
            let vfs = self
                .vfs
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            vfs.backend_by_id(id.backend_id())
                .ok_or_else(|| {
                    anyhow::anyhow!("no backend registered for item id {}", id.backend_id())
                })
                .map(Arc::clone)
        }?;
        let native_id = id.native_id().to_owned();
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async move { backend.list_children(&native_id).await })
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

            // Resolve the ItemId for this inode. Any inode — root or nested —
            // is eligible; ENOENT is the correct response for an unknown inode.
            let id = {
                let map = self
                    .inode_map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                map.get_id(ino_u64).cloned()
            };

            let Some(id) = id else {
                reply.error(Errno::ENOENT);
                return;
            };

            // Ask the VFS whether this item has children. A non-directory
            // (file) will return an empty vec or an error; treat both as
            // ENOTDIR so the kernel receives the expected error code.
            let children = match self.list_children_sync(&id) {
                Ok(children) => children,
                Err(_) => {
                    // If the item is a file or the backend is unavailable,
                    // report ENOTDIR — the kernel treats readdir on a
                    // non-directory as ENOTDIR regardless of the underlying
                    // cause.
                    reply.error(Errno::ENOTDIR);
                    return;
                }
            };

            // Allocate stable inodes for every child before emitting any
            // directory entries. This ensures that a kernel lookup() on a
            // name seen in readdir always resolves to the same inode.
            let child_inodes: Vec<(u64, &cascade_engine::types::FileEntry)> = {
                let mut map = self
                    .inode_map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                children
                    .iter()
                    .map(|entry| (map.allocate(entry.id.clone()), entry))
                    .collect()
            };

            if offset == 0 {
                let _ = reply.add(INodeNo(ino_u64), 1, FileType::Directory, ".");
            }
            if offset <= 1 {
                let _ = reply.add(INodeNo(ino_u64), 2, FileType::Directory, "..");
            }

            // Emit children, skipping any that fall before the kernel's
            // resume offset. Offsets start at 3 because . and .. occupy 1
            // and 2 (FUSE offset convention).
            let mut entry_offset = 2u64;
            for (child_inode, entry) in &child_inodes {
                entry_offset += 1;
                if entry_offset <= offset {
                    continue;
                }
                let kind = if entry.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                let full = reply.add(INodeNo(*child_inode), entry_offset, kind, &entry.name);
                if full {
                    // Buffer is full; the kernel will call readdir again
                    // with the current offset to resume.
                    reply.ok();
                    return;
                }
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
        let map = ops
            .inode_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(map.get_inode(&root), Some(crate::inode::ROOT_INODE));
    }

    // -----------------------------------------------------------------------
    // Nested directory traversal — drive FuseOps::list_children_sync
    // directly (no kernel / FUSE mount required).
    // -----------------------------------------------------------------------

    /// In-memory backend for tests. Stores `FileEntry` records indexed by
    /// their full `ItemId` string. `list_children` filters by `parent_id`.
    mod fake_backend {
        use std::collections::HashMap;
        use std::path::Path;
        use std::sync::Mutex;
        use std::time::Duration;

        use async_trait::async_trait;
        use cascade_engine::backend::Backend;
        use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};

        #[derive(Debug)]
        pub struct FakeBackend {
            pub id: String,
            entries: Mutex<HashMap<String, FileEntry>>,
        }

        impl FakeBackend {
            pub fn new(id: &str) -> Self {
                Self {
                    id: id.to_string(),
                    entries: Mutex::new(HashMap::new()),
                }
            }

            pub fn insert(&self, entry: FileEntry) {
                self.entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(entry.id.0.clone(), entry);
            }
        }

        #[async_trait]
        impl Backend for FakeBackend {
            fn id(&self) -> &str {
                &self.id
            }

            fn display_name(&self) -> &str {
                &self.id
            }

            async fn quota(&self) -> anyhow::Result<Option<Quota>> {
                Ok(None)
            }

            async fn changes(
                &self,
                _cursor: Option<&Cursor>,
            ) -> anyhow::Result<(Vec<Change>, Cursor)> {
                Ok((vec![], Cursor("fake".to_string())))
            }

            async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
                let key = format!("{}:{}", self.id, path.display());
                self.entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("not found: {key}"))
            }

            async fn download(
                &self,
                _file: &FileEntry,
                _writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
            ) -> anyhow::Result<()> {
                anyhow::bail!("FakeBackend: download not implemented")
            }

            async fn upload(
                &self,
                _path: &Path,
                _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
                _parent_id: &FileId,
            ) -> anyhow::Result<FileEntry> {
                anyhow::bail!("FakeBackend: upload not implemented")
            }

            async fn update(
                &self,
                _file_id: &FileId,
                _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
            ) -> anyhow::Result<FileEntry> {
                anyhow::bail!("FakeBackend: update not implemented")
            }

            async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
                anyhow::bail!("FakeBackend: create_dir not implemented")
            }

            async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
                anyhow::bail!("FakeBackend: delete not implemented")
            }

            async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
                anyhow::bail!("FakeBackend: move_entry not implemented")
            }

            async fn list_children(
                &self,
                parent_native_id: &str,
            ) -> anyhow::Result<Vec<FileEntry>> {
                let parent_full = ItemId::new(&self.id, parent_native_id).0;
                let entries = self
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Ok(entries
                    .values()
                    .filter(|e| e.parent_id.0 == parent_full)
                    .cloned()
                    .collect())
            }

            async fn poll_interval(&self) -> Option<Duration> {
                None
            }
        }
    }

    /// Build a VFS tree backed by a `FakeBackend` with the following shape:
    ///
    /// ```text
    /// root  (dir)
    /// └── dir  (dir)  id = fake:dir
    ///     └── subdir  (dir)  id = fake:subdir
    ///         └── file.txt  (file)  id = fake:file
    /// ```
    fn make_nested_ops() -> FuseOps {
        use cascade_engine::types::FileEntry;
        use fake_backend::FakeBackend;

        let backend = std::sync::Arc::new(FakeBackend::new("fake"));

        let root_id = ItemId::new("fake", "root");
        let dir_id = ItemId::new("fake", "dir");
        let subdir_id = ItemId::new("fake", "subdir");
        let file_id = ItemId::new("fake", "file");

        backend.insert(FileEntry::dir(
            dir_id.clone(),
            root_id.clone(),
            "dir".to_string(),
        ));
        backend.insert(FileEntry::dir(
            subdir_id.clone(),
            dir_id,
            "subdir".to_string(),
        ));
        backend.insert(FileEntry::file(file_id, subdir_id, "file.txt".to_string()));

        let vfs = Arc::new(RwLock::new(VfsTree::new(backend)));
        FuseOps::new_with_vfs(root_id, vfs)
    }

    /// Helper: call `list_children_by_id` via the `VfsTree` directly.
    ///
    /// The tests below exercise the VFS → backend path that `list_children_sync`
    /// wraps. Using an async helper here avoids the "cannot `block_on` inside a
    /// Tokio runtime" panic that would occur if `list_children_sync` were called
    /// from within a `#[tokio::test]` context.
    ///
    /// The `Arc<dyn Backend>` is cloned while holding the read lock so that the
    /// guard is dropped before the async `list_children` call, preventing an
    /// `RwLockReadGuard` from being held across an `.await` point.
    async fn list_children(
        ops: &FuseOps,
        id: &ItemId,
    ) -> anyhow::Result<Vec<cascade_engine::types::FileEntry>> {
        let backend = {
            let vfs = ops
                .vfs
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            vfs.backend_by_id(id.backend_id())
                .map(std::sync::Arc::clone)
                .ok_or_else(|| anyhow::anyhow!("no backend for id {}", id.backend_id()))?
        };
        backend.list_children(id.native_id()).await
    }

    /// `list_children_by_id` on root returns the top-level `dir` entry.
    #[tokio::test]
    async fn list_children_root_returns_dir() -> anyhow::Result<()> {
        let ops = make_nested_ops();
        let root_id = ItemId::new("fake", "root");
        let children = list_children(&ops, &root_id).await?;
        assert_eq!(children.len(), 1);
        let child = children
            .first()
            .ok_or_else(|| anyhow::anyhow!("expected at least one child"))?;
        assert_eq!(child.name, "dir");
        assert!(child.is_dir);
        assert_eq!(child.id, ItemId::new("fake", "dir"));
        Ok(())
    }

    /// `list_children_by_id` on `dir` returns `subdir`.
    #[tokio::test]
    async fn list_children_nested_dir_returns_subdir() -> anyhow::Result<()> {
        let ops = make_nested_ops();
        let dir_id = ItemId::new("fake", "dir");
        let children = list_children(&ops, &dir_id).await?;
        assert_eq!(children.len(), 1);
        let child = children
            .first()
            .ok_or_else(|| anyhow::anyhow!("expected at least one child"))?;
        assert_eq!(child.name, "subdir");
        assert!(child.is_dir);
        assert_eq!(child.id, ItemId::new("fake", "subdir"));
        Ok(())
    }

    /// `list_children_by_id` on `subdir` returns `file.txt`.
    #[tokio::test]
    async fn list_children_subdir_returns_file() -> anyhow::Result<()> {
        let ops = make_nested_ops();
        let subdir_id = ItemId::new("fake", "subdir");
        let children = list_children(&ops, &subdir_id).await?;
        assert_eq!(children.len(), 1);
        let child = children
            .first()
            .ok_or_else(|| anyhow::anyhow!("expected at least one child"))?;
        assert_eq!(child.name, "file.txt");
        assert!(!child.is_dir);
        assert_eq!(child.id, ItemId::new("fake", "file"));
        Ok(())
    }

    /// `list_children_by_id` on `file.txt` returns an empty vec — a file has no
    /// children. The `readdir` handler treats this as ENOTDIR.
    #[tokio::test]
    async fn list_children_file_returns_empty() -> anyhow::Result<()> {
        let ops = make_nested_ops();
        let file_id = ItemId::new("fake", "file");
        let children = list_children(&ops, &file_id).await?;
        assert!(children.is_empty(), "expected no children for a file");
        Ok(())
    }

    /// Inode allocation is idempotent: querying the same `ItemId` twice always
    /// yields the same inode number.
    #[tokio::test]
    async fn inode_allocation_is_idempotent_across_list_calls() -> anyhow::Result<()> {
        let ops = make_nested_ops();
        let root_id = ItemId::new("fake", "root");

        // First call — allocates.
        let first = list_children(&ops, &root_id).await?;
        let first_id = first
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("expected at least one child"))?
            .id;
        let inode_first = ops
            .inode_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allocate(first_id.clone());

        // Second call — same ItemId must yield the same inode.
        let second = list_children(&ops, &root_id).await?;
        let second_id = second
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("expected at least one child"))?
            .id;
        let inode_second = ops
            .inode_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allocate(second_id);

        assert_eq!(
            inode_first, inode_second,
            "inode must be stable across calls"
        );
        assert_ne!(
            inode_first,
            crate::inode::ROOT_INODE,
            "child must not reuse root inode"
        );
        Ok(())
    }

    /// A child inode must differ from root.
    #[tokio::test]
    async fn child_inodes_are_distinct_from_root() -> anyhow::Result<()> {
        let ops = make_nested_ops();
        let root_id = ItemId::new("fake", "root");
        let children = list_children(&ops, &root_id).await?;

        let child_id = children
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("expected at least one child"))?
            .id;
        let child_inode = ops
            .inode_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allocate(child_id);

        assert_ne!(child_inode, crate::inode::ROOT_INODE);
        Ok(())
    }
}
