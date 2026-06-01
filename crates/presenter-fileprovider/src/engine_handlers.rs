//! Engine-backed implementation of [`FileProviderHandlers`].
//!
//! Routes every inbound RPC through the Cascade engine: the `VfsTree` to
//! locate the owning backend, the `StateDb` to look up cached metadata, and
//! the [`Backend`] trait for the actual operation. The seven handlers below
//! map one-to-one onto the seven [`FileProviderHandlers`] methods.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::db::StateDb;
use cascade_engine::types::{CacheState, FileEntry, FileId, ItemId, SyncCursor, VfsItem};
use cascade_engine::vfs::{VfsTree, derive_sync_cursor};
use data_encoding::BASE64URL_NOPAD;
use tokio::io::AsyncWriteExt;

use crate::handlers::{EnumerateOutput, FileProviderHandlers, HandlerError, HandlerResult};
use crate::items::FileProviderItem;

/// Subdirectory inside the cache directory where File Provider materialised
/// contents live. Each item gets its own folder keyed by sanitised ID.
const CACHE_SUBDIR: &str = "file-provider";

/// Number of children returned per `enumerateItems` request before the
/// engine emits a `next_page` cursor.
///
/// The handler asks the backend for the full child list (the current
/// `Backend::list_children` contract has no native page parameter) and
/// slices it locally. A future round will push pagination down into the
/// backend trait so the slice happens at the source. The size is chosen
/// to match the default `NSFileProviderEnumerator` batch — 256 — which
/// keeps each round-trip response well under macOS's XPC payload limit.
const ENUMERATE_PAGE_SIZE: usize = 256;

/// Production handler implementation.
///
/// Construction takes:
/// - a shared `VfsTree` so the handler can route operations to the correct
///   backend by `backend_id`;
/// - a shared `StateDb` so the handler can answer `getItem` from cached
///   metadata without round-tripping the cloud;
/// - a cache directory under which `fetchContents` materialises file
///   content.
pub struct EngineHandlers {
    vfs: Arc<RwLock<VfsTree>>,
    db: Arc<StateDb>,
    cache_dir: PathBuf,
}

impl std::fmt::Debug for EngineHandlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandlers")
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

impl EngineHandlers {
    /// Create a new engine-backed handler.
    ///
    /// `cache_dir` is the directory under which fetched file contents are
    /// materialised. The handler creates `cache_dir/file-provider/` on
    /// demand.
    pub const fn new(vfs: Arc<RwLock<VfsTree>>, db: Arc<StateDb>, cache_dir: PathBuf) -> Self {
        Self { vfs, db, cache_dir }
    }

    /// Look up the backend that owns an `ItemId`.
    fn backend_for(&self, item_id: &ItemId) -> HandlerResult<Arc<dyn Backend>> {
        let vfs = self
            .vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vfs.backend_by_id(item_id.backend_id())
            .cloned()
            .ok_or_else(|| {
                HandlerError::not_found(format!(
                    "no backend registered for id {}",
                    item_id.backend_id()
                ))
            })
    }

    /// Compute the cache directory for a given item.
    ///
    /// Format: `<cache_dir>/file-provider/<sanitised-id>/`. The handler
    /// creates this on demand.
    fn cache_dir_for(&self, item_id: &ItemId) -> PathBuf {
        self.cache_dir
            .join(CACHE_SUBDIR)
            .join(safe_filename(&item_id.0))
    }

    /// Build the cache path for a specific version of an item's content.
    ///
    /// The version segment is derived from the engine's last-modified
    /// timestamp so a remote update invalidates the cached file. If no
    /// modification time is known, the version is `unknown` and the cached
    /// file is treated as if it could be stale on the next fetch.
    fn cache_path_for(&self, entry: &FileEntry) -> PathBuf {
        let version = entry
            .mod_time
            .map_or_else(|| "unknown".to_string(), |t| t.timestamp().to_string());
        self.cache_dir_for(&entry.id).join(version)
    }
}

#[async_trait]
impl FileProviderHandlers for EngineHandlers {
    async fn get_item(&self, id: &str) -> HandlerResult<FileProviderItem> {
        let item_id = ItemId(id.to_string());
        let entry = self
            .db
            .get_file(&item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {id}")))?;
        let cache_state = self
            .db
            .get_cache_state(&item_id)?
            .unwrap_or(CacheState::Online);

        let mut vfs_item: VfsItem = entry.into();
        vfs_item.cache_state = cache_state;
        Ok(FileProviderItem::from(vfs_item))
    }

    async fn enumerate_items(
        &self,
        parent_id: &str,
        page: Option<&str>,
    ) -> HandlerResult<EnumerateOutput> {
        let parent = ItemId(parent_id.to_string());
        let backend = self.backend_for(&parent)?;
        let mut entries = backend.list_children(parent.native_id()).await?;

        // Deterministic ordering — id is the only field guaranteed unique
        // and stable across calls.
        entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));

        let after_id = decode_enumerate_page(page)?;
        let start = after_id.as_deref().map_or(0, |last| {
            entries
                .iter()
                .position(|entry| entry.id.0.as_str() > last)
                .unwrap_or(entries.len())
        });
        let end = start.saturating_add(ENUMERATE_PAGE_SIZE).min(entries.len());

        let next_page = if end < entries.len() {
            entries
                .get(end.saturating_sub(1))
                .map(|entry| encode_enumerate_page(&entry.id.0))
        } else {
            None
        };

        let items = entries
            .drain(start..end)
            .map(|entry| FileProviderItem::from(VfsItem::from(entry)))
            .collect();

        Ok(EnumerateOutput { items, next_page })
    }

    async fn fetch_contents(&self, id: &str) -> HandlerResult<PathBuf> {
        let item_id = ItemId(id.to_string());
        let entry = self
            .db
            .get_file(&item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {id}")))?;
        if entry.is_dir {
            return Err(HandlerError::permission_denied(format!(
                "cannot fetch contents of a directory: {id}"
            )));
        }

        let cache_path = self.cache_path_for(&entry);
        if cache_path.exists() {
            return Ok(cache_path);
        }

        let parent = cache_path
            .parent()
            .ok_or_else(|| HandlerError::internal("cache path has no parent".to_string()))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| HandlerError::internal(format!("create cache dir: {error}")))?;

        let backend = self.backend_for(&entry.id)?;
        let temp_path = parent.join(format!(
            "{}.tmp",
            cache_path.file_name().map_or_else(
                || std::borrow::Cow::Borrowed("download"),
                std::ffi::OsStr::to_string_lossy
            )
        ));

        let file = tokio::fs::File::create(&temp_path)
            .await
            .map_err(|error| HandlerError::internal(format!("create temp file: {error}")))?;
        let mut writer = WriterAdapter { inner: file };
        backend.download(&entry, &mut writer).await?;
        writer
            .inner
            .flush()
            .await
            .map_err(|error| HandlerError::internal(format!("flush temp file: {error}")))?;
        tokio::fs::rename(&temp_path, &cache_path)
            .await
            .map_err(|error| HandlerError::internal(format!("persist cache file: {error}")))?;

        Ok(cache_path)
    }

    async fn import_document(
        &self,
        source_url: &str,
        parent_id: &str,
        name: Option<&str>,
        existing_id: Option<&str>,
    ) -> HandlerResult<FileProviderItem> {
        let source_path = decode_source_url(source_url)?;
        let parent = ItemId(parent_id.to_string());
        let backend = self.backend_for(&parent)?;

        let mut reader = tokio::fs::File::open(&source_path).await.map_err(|error| {
            HandlerError::internal(format!("open source {}: {error}", source_path.display()))
        })?;

        let entry = if let Some(existing) = existing_id {
            let existing_item = ItemId(existing.to_string());
            let file_id = FileId(existing_item.0.clone());
            backend.update(&file_id, &mut reader).await?
        } else {
            let filename = match name {
                Some(value) => value.to_string(),
                None => source_path
                    .file_name()
                    .map(|os| os.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        HandlerError::internal(format!(
                            "cannot derive filename from source {}",
                            source_path.display()
                        ))
                    })?,
            };
            let parent_file_id = FileId(parent.0.clone());
            backend
                .upload(Path::new(&filename), &mut reader, &parent_file_id)
                .await?
        };

        self.db.upsert_file(&entry)?;
        Ok(FileProviderItem::from(VfsItem::from(entry)))
    }

    async fn create_directory(
        &self,
        name: &str,
        parent_id: &str,
    ) -> HandlerResult<FileProviderItem> {
        let parent = ItemId(parent_id.to_string());
        let backend = self.backend_for(&parent)?;
        let parent_file_id = FileId(parent.0.clone());
        let entry = backend
            .create_dir_with_parent(name, &parent_file_id)
            .await?;
        self.db.upsert_file(&entry)?;
        Ok(FileProviderItem::from(VfsItem::from(entry)))
    }

    async fn delete_item(&self, id: &str) -> HandlerResult<()> {
        let item_id = ItemId(id.to_string());
        let entry = self
            .db
            .get_file(&item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {id}")))?;
        let backend = self.backend_for(&item_id)?;
        backend.delete(&entry).await?;
        self.db.delete_subtree(&item_id)?;
        Ok(())
    }

    async fn move_item(
        &self,
        id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> HandlerResult<FileProviderItem> {
        let item_id = ItemId(id.to_string());
        let new_parent = ItemId(new_parent_id.to_string());
        if item_id.backend_id() != new_parent.backend_id() {
            return Err(HandlerError::not_supported(format!(
                "cross-backend move not yet supported; download/upload/delete dance is the planned follow-up ({} -> {})",
                item_id.backend_id(),
                new_parent.backend_id()
            )));
        }
        let backend = self.backend_for(&item_id)?;
        let src_file_id = FileId(item_id.0.clone());
        let dst_parent_file_id = FileId(new_parent.0.clone());
        let entry = backend
            .move_by_id(&src_file_id, &dst_parent_file_id, new_name)
            .await?;
        self.db.upsert_file(&entry)?;
        Ok(FileProviderItem::from(VfsItem::from(entry)))
    }

    async fn current_sync_cursor(&self, parent_id: &str) -> HandlerResult<SyncCursor> {
        let parent = ItemId(parent_id.to_string());
        // backend_for() releases the std::sync RwLock guard before
        // returning — it would not be Send across the await otherwise.
        let backend = self.backend_for(&parent)?;
        let cursor = derive_sync_cursor(backend.as_ref(), parent.native_id()).await?;
        Ok(cursor)
    }
}

/// Encode the last item ID of a returned page as the opaque wire cursor.
///
/// The wire cursor is base64url-no-pad of the raw ID bytes, which keeps
/// the cursor JSON-safe (no quoting, no padding) without exposing the
/// underlying ID format to the Swift side. Consumers must not interpret
/// the bytes.
fn encode_enumerate_page(last_id: &str) -> String {
    BASE64URL_NOPAD.encode(last_id.as_bytes())
}

/// Decode an opaque wire cursor back into the last item ID it carries.
///
/// `None` or an empty string mean "first page". Malformed cursors map to
/// `Internal` errors — the Swift side should always echo back what the
/// engine emitted, so a decode failure indicates a protocol bug.
fn decode_enumerate_page(page: Option<&str>) -> HandlerResult<Option<String>> {
    let Some(encoded) = page.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let bytes = BASE64URL_NOPAD
        .decode(encoded.as_bytes())
        .map_err(|error| HandlerError::internal(format!("invalid page cursor: {error}")))?;
    let id = String::from_utf8(bytes).map_err(|error| {
        HandlerError::internal(format!("page cursor is not valid UTF-8: {error}"))
    })?;
    Ok(Some(id))
}

/// Decode a `file://` URL or a bare path into a `PathBuf`.
///
/// The Swift side passes `URL.path` which is already a filesystem path with
/// no scheme prefix, but accept either form so the handler is robust to
/// implementation changes on either end.
fn decode_source_url(source: &str) -> HandlerResult<PathBuf> {
    let trimmed = source.strip_prefix("file://").unwrap_or(source);
    if trimmed.is_empty() {
        return Err(HandlerError::internal("empty source URL".to_string()));
    }
    Ok(PathBuf::from(trimmed))
}

/// Sanitise an `ItemId` string into a filesystem-safe filename segment.
fn safe_filename(id: &str) -> String {
    id.replace([':', '/', '\\'], "_")
}

/// Adapter that exposes a `tokio::fs::File` as the
/// `dyn AsyncWrite + Unpin + Send` the `Backend` trait expects.
struct WriterAdapter {
    inner: tokio::fs::File,
}

impl tokio::io::AsyncWrite for WriterAdapter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl Unpin for WriterAdapter {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::ErrorCode;
    use async_trait::async_trait;
    use cascade_engine::types::{Change, Cursor, Quota};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Minimal in-memory backend used to exercise `EngineHandlers` without
    /// the real cloud transports. Implements just enough of the trait
    /// surface to cover the seven RPC paths.
    #[derive(Debug, Default)]
    struct InMemoryBackend {
        id: String,
        files: Mutex<HashMap<String, FileEntry>>,
        content: Mutex<HashMap<String, Vec<u8>>>,
        next_id: Mutex<u64>,
    }

    impl InMemoryBackend {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                files: Mutex::new(HashMap::new()),
                content: Mutex::new(HashMap::new()),
                next_id: Mutex::new(0),
            }
        }

        fn allocate_id(&self) -> String {
            let mut next = self
                .next_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *next += 1;
            format!("n{next}")
        }

        fn insert(&self, entry: FileEntry) {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files.insert(entry.id.0.clone(), entry);
        }
    }

    #[async_trait]
    impl Backend for InMemoryBackend {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &str {
            &self.id
        }

        async fn quota(&self) -> anyhow::Result<Option<Quota>> {
            Ok(None)
        }

        async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
            Ok((vec![], Cursor("static".to_string())))
        }

        async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("metadata not implemented for stub")
        }

        async fn download(
            &self,
            file: &FileEntry,
            writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> anyhow::Result<()> {
            let bytes = {
                let content = self
                    .content
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                content
                    .get(&file.id.0)
                    .ok_or_else(|| anyhow::anyhow!("no content for {}", file.id))?
                    .clone()
            };
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            Ok(())
        }

        async fn upload(
            &self,
            path: &Path,
            reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            let mut bytes = Vec::new();
            tokio::io::copy(reader, &mut bytes).await?;

            let new_id = self.allocate_id();
            let item_id = ItemId::new(&self.id, &new_id);
            let parent_item = ItemId(parent_id.0.clone());
            let entry = FileEntry {
                id: item_id.clone(),
                parent_id: parent_item,
                name: path
                    .file_name()
                    .map(|os| os.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unnamed".to_string()),
                is_dir: false,
                size: Some(bytes.len() as u64),
                mod_time: Some(Utc::now()),
                mime_type: None,
                hash: None,
            };

            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(item_id.0.clone(), bytes);
            self.insert(entry.clone());
            Ok(entry)
        }

        async fn update(
            &self,
            file_id: &FileId,
            reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        ) -> anyhow::Result<FileEntry> {
            let mut bytes = Vec::new();
            tokio::io::copy(reader, &mut bytes).await?;

            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = files
                .get_mut(&file_id.0)
                .ok_or_else(|| anyhow::anyhow!("file not found"))?;
            entry.size = Some(bytes.len() as u64);
            entry.mod_time = Some(Utc::now());
            let updated = entry.clone();
            drop(files);

            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(file_id.0.clone(), bytes);
            Ok(updated)
        }

        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("create_dir is not used by the stub; use create_dir_with_parent")
        }

        async fn create_dir_with_parent(
            &self,
            name: &str,
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            let new_id = self.allocate_id();
            let item_id = ItemId::new(&self.id, &new_id);
            let parent_item = ItemId(parent_id.0.clone());
            let entry = FileEntry {
                id: item_id,
                parent_id: parent_item,
                name: name.to_string(),
                is_dir: true,
                size: None,
                mod_time: Some(Utc::now()),
                mime_type: None,
                hash: None,
            };
            self.insert(entry.clone());
            Ok(entry)
        }

        async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files.remove(&file.id.0);
            drop(files);
            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&file.id.0);
            Ok(())
        }

        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("move_entry is not used by the stub; use move_by_id")
        }

        async fn move_by_id(
            &self,
            src_id: &FileId,
            dst_parent_id: &FileId,
            new_name: &str,
        ) -> anyhow::Result<FileEntry> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = files
                .get_mut(&src_id.0)
                .ok_or_else(|| anyhow::anyhow!("file not found"))?;
            entry.parent_id = ItemId(dst_parent_id.0.clone());
            entry.name = new_name.to_string();
            entry.mod_time = Some(Utc::now());
            Ok(entry.clone())
        }

        async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prefix = format!("{}:", self.id);
            let parent_full = format!("{prefix}{parent_native_id}");
            let entries = files
                .values()
                .filter(|entry| entry.parent_id.0 == parent_full)
                .cloned()
                .collect();
            Ok(entries)
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn make_handlers() -> (EngineHandlers, Arc<InMemoryBackend>, tempfile::TempDir) {
        let backend = Arc::new(InMemoryBackend::new("stub"));
        let vfs = Arc::new(RwLock::new(VfsTree::new(backend.clone())));
        let cache_dir = tempfile::tempdir().unwrap();

        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("stub", "stub", "Stub", None, None)
            .unwrap();

        // Seed one parent directory and one child file.
        let parent_id = ItemId::new("stub", "root");
        let parent_entry = FileEntry {
            id: parent_id.clone(),
            parent_id: ItemId::new("stub", ""),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        backend.insert(parent_entry.clone());
        db.upsert_file(&parent_entry).unwrap();

        let child_id = ItemId::new("stub", "file1");
        let child_entry = FileEntry {
            id: child_id.clone(),
            parent_id: parent_id.clone(),
            name: "hello.txt".to_string(),
            is_dir: false,
            size: Some(11),
            mod_time: Some(Utc::now()),
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        backend.insert(child_entry.clone());
        db.upsert_file(&child_entry).unwrap();
        backend
            .content
            .lock()
            .unwrap()
            .insert(child_id.0.clone(), b"hello world".to_vec());

        let handlers = EngineHandlers::new(vfs, db, cache_dir.path().to_path_buf());
        (handlers, backend, cache_dir)
    }

    #[tokio::test]
    async fn get_item_returns_metadata_for_known_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let item = handlers.get_item("stub:file1").await.unwrap();
        assert_eq!(item.id, "stub:file1");
        assert_eq!(item.filename, "hello.txt");
        assert!(!item.is_directory);
    }

    #[tokio::test]
    async fn get_item_returns_not_found_for_missing_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers.get_item("stub:missing").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn enumerate_items_lists_children_of_directory() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let output = handlers.enumerate_items("stub:root", None).await.unwrap();
        assert_eq!(output.items.len(), 1);
        assert_eq!(output.items[0].filename, "hello.txt");
        assert!(output.next_page.is_none());
    }

    #[tokio::test]
    async fn enumerate_items_fails_for_unknown_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .enumerate_items("ghost:root", None)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn fetch_contents_materialises_file_to_cache_dir() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let path = handlers.fetch_contents("stub:file1").await.unwrap();
        assert!(path.exists());
        let bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn fetch_contents_is_idempotent() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let first = handlers.fetch_contents("stub:file1").await.unwrap();
        let second = handlers.fetch_contents("stub:file1").await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn fetch_contents_refuses_directories() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers.fetch_contents("stub:root").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn import_document_uploads_a_new_file() {
        let (handlers, _backend, tempdir) = make_handlers();
        let source = tempdir.path().join("upload.txt");
        tokio::fs::write(&source, b"new bytes").await.unwrap();

        let item = handlers
            .import_document(
                source.to_str().unwrap(),
                "stub:root",
                Some("upload.txt"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(item.filename, "upload.txt");
        assert_eq!(item.parent_id, "stub:root");
        assert_eq!(item.size, Some(9));
    }

    #[tokio::test]
    async fn import_document_decodes_file_url_prefix() {
        let (handlers, _backend, tempdir) = make_handlers();
        let source = tempdir.path().join("u.txt");
        tokio::fs::write(&source, b"ok").await.unwrap();
        let url = format!("file://{}", source.display());

        let item = handlers
            .import_document(&url, "stub:root", Some("u.txt"), None)
            .await
            .unwrap();
        assert_eq!(item.filename, "u.txt");
    }

    #[tokio::test]
    async fn import_document_overwrites_existing_when_existing_id_set() {
        let (handlers, _backend, tempdir) = make_handlers();
        let source = tempdir.path().join("replace.txt");
        tokio::fs::write(&source, b"new contents long enough")
            .await
            .unwrap();

        let item = handlers
            .import_document(
                source.to_str().unwrap(),
                "stub:root",
                None,
                Some("stub:file1"),
            )
            .await
            .unwrap();
        assert_eq!(item.id, "stub:file1");
        assert_eq!(item.size, Some(24));
    }

    #[tokio::test]
    async fn create_directory_creates_under_parent() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let item = handlers
            .create_directory("Photos", "stub:root")
            .await
            .unwrap();
        assert!(item.is_directory);
        assert_eq!(item.filename, "Photos");
        assert_eq!(item.parent_id, "stub:root");
    }

    #[tokio::test]
    async fn delete_item_removes_known_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        handlers.delete_item("stub:file1").await.unwrap();
        let err = handlers.get_item("stub:file1").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn delete_item_fails_for_missing_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers.delete_item("stub:missing").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn move_item_renames_and_reparents_within_one_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();

        // Create a second directory to move into.
        let new_dir = handlers
            .create_directory("Archive", "stub:root")
            .await
            .unwrap();

        let moved = handlers
            .move_item("stub:file1", &new_dir.id, "renamed.txt")
            .await
            .unwrap();
        assert_eq!(moved.filename, "renamed.txt");
        assert_eq!(moved.parent_id, new_dir.id);
    }

    #[tokio::test]
    async fn move_item_rejects_cross_backend_moves() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .move_item("stub:file1", "other:root", "renamed.txt")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotSupported);
    }

    #[tokio::test]
    async fn current_sync_cursor_returns_stable_value_when_no_changes() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let first = handlers.current_sync_cursor("stub:root").await.unwrap();
        let second = handlers.current_sync_cursor("stub:root").await.unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[tokio::test]
    async fn current_sync_cursor_changes_after_upsert() {
        let (handlers, backend, _tempdir) = make_handlers();
        let before = handlers.current_sync_cursor("stub:root").await.unwrap();

        backend.insert(FileEntry {
            id: ItemId::new("stub", "file2"),
            parent_id: ItemId::new("stub", "root"),
            name: "second.txt".to_string(),
            is_dir: false,
            size: Some(4),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        });

        let after = handlers.current_sync_cursor("stub:root").await.unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn current_sync_cursor_fails_for_unknown_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .current_sync_cursor("ghost:root")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    /// Populate the stub backend with `count` extra files under `stub:root`
    /// with deterministic names so the sort order is stable and easy to
    /// reason about across pages.
    fn seed_children(backend: &InMemoryBackend, count: usize) {
        let parent = ItemId::new("stub", "root");
        for index in 0..count {
            // Zero-pad to 4 digits so lexicographic sort matches numeric order.
            let native = format!("p{index:04}");
            let item_id = ItemId::new("stub", &native);
            let entry = FileEntry {
                id: item_id,
                parent_id: parent.clone(),
                name: format!("file-{index:04}.txt"),
                is_dir: false,
                size: Some(1),
                mod_time: Some(Utc::now()),
                mime_type: Some("text/plain".to_string()),
                hash: None,
            };
            backend.insert(entry);
        }
    }

    #[tokio::test]
    async fn enumerate_items_returns_null_next_page_when_done() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let output = handlers.enumerate_items("stub:root", None).await.unwrap();
        // Only one seeded child; page size 256, so we're done in one shot.
        assert_eq!(output.items.len(), 1);
        assert!(output.next_page.is_none());
    }

    #[tokio::test]
    async fn enumerate_items_paginates_when_more_than_one_page_of_children() {
        let (handlers, backend, _tempdir) = make_handlers();
        // Replace the seed child with a clean slate plus a known count.
        backend
            .files
            .lock()
            .unwrap()
            .retain(|_, entry| entry.is_dir);
        let total: usize = 600;
        seed_children(&backend, total);

        let mut seen: Vec<String> = Vec::new();
        let mut page_cursor: Option<String> = None;
        loop {
            let output = handlers
                .enumerate_items("stub:root", page_cursor.as_deref())
                .await
                .unwrap();
            for item in &output.items {
                seen.push(item.id.clone());
            }
            match output.next_page {
                Some(cursor) => page_cursor = Some(cursor),
                None => break,
            }
            assert!(
                seen.len() < total,
                "consumed all items but engine kept emitting next_page"
            );
        }

        assert_eq!(
            seen.len(),
            total,
            "all children must be returned exactly once"
        );
        let mut deduped = seen.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), total, "no duplicates across pages");
    }

    #[tokio::test]
    async fn enumerate_items_rejects_malformed_page_cursor() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .enumerate_items("stub:root", Some("!!!not-base64!!!"))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
    }
}
