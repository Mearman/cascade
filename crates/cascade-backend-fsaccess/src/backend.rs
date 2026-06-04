//! The File System Access backend surface.
//!
//! [`FsAccessBackend`] wraps a user-granted directory handle and exposes a small
//! contract shaped after the engine's backend trait — `id`, `display_name`,
//! `changes`, `download`, `upload` — without depending on `cascade-engine`,
//! which cannot compile to `wasm32-unknown-unknown` (see the crate-level docs).

use std::cell::RefCell;

use js_sys::{Array, Map, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};

use crate::js;

/// A failure crossing the File System Access bridge.
#[derive(Debug, thiserror::Error)]
pub enum FsAccessError {
    /// A JavaScript-side call rejected or threw. The message carries the JS
    /// error text where one was available.
    #[error("File System Access call failed: {0}")]
    Js(String),
    /// A value returned by the bridge did not have the expected shape.
    #[error("could not decode a value from the File System Access bridge: {0}")]
    Decode(String),
    /// No file with the requested name exists in the directory.
    #[error("file not found in directory: {0}")]
    NotFound(String),
}

/// The set of file-name changes detected since the previous [`FsAccessBackend::changes`]
/// call. Mirrors the `DirectoryChanges` shape produced by the JS bridge; only
/// immediate file children are tracked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirectoryChanges {
    /// Names that appeared since the last snapshot.
    pub created: Vec<String>,
    /// Names whose size or modification time changed since the last snapshot.
    pub modified: Vec<String>,
    /// Names that disappeared since the last snapshot.
    pub deleted: Vec<String>,
}

/// A backend backed by a user-granted browser directory.
///
/// The crate runs in the browser's single-threaded WASM context, so the handle
/// and the change-detection snapshot are held behind a [`RefCell`] rather than a
/// thread-safe lock — the engine's `Send + Sync` requirements do not apply here.
#[derive(Debug)]
pub struct FsAccessBackend {
    id: String,
    display_name: String,
    /// The granted `FileSystemDirectoryHandle`, opaque to Rust.
    dir: JsValue,
    /// The last snapshot returned by the JS change detector, passed back on the
    /// next [`Self::changes`] call. Starts as an empty `Map`.
    snapshot: RefCell<JsValue>,
}

/// Prompt the user to grant a directory and build a backend over it.
///
/// Resolves once the user has picked a directory in the browser's native
/// picker; rejects if the picker is dismissed or the API is unavailable.
pub async fn create_backend() -> Result<FsAccessBackend, FsAccessError> {
    let dir = js::request_directory()
        .await
        .map_err(|e| FsAccessError::Js(describe(&e)))?;
    Ok(FsAccessBackend {
        id: "fsaccess".to_string(),
        display_name: "Local directory (browser)".to_string(),
        dir,
        snapshot: RefCell::new(Map::new().into()),
    })
}

impl FsAccessBackend {
    /// The stable backend identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The human-readable mount name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Enumerate the directory and report file-name changes since the previous
    /// call, advancing the stored snapshot.
    pub async fn changes(&self) -> Result<DirectoryChanges, FsAccessError> {
        let previous = self.snapshot.borrow().clone();
        let result = js::detect_changes(&self.dir, &previous)
            .await
            .map_err(|e| FsAccessError::Js(describe(&e)))?;

        let changes = DirectoryChanges {
            created: string_array(&field(&result, "created")?)?,
            modified: string_array(&field(&result, "modified")?)?,
            deleted: string_array(&field(&result, "deleted")?)?,
        };

        *self.snapshot.borrow_mut() = field(&result, "snapshot")?;
        Ok(changes)
    }

    /// Read the full contents of an immediate file child by name.
    pub async fn download(&self, name: &str) -> Result<Vec<u8>, FsAccessError> {
        let entries = js::enumerate_directory(&self.dir)
            .await
            .map_err(|e| FsAccessError::Js(describe(&e)))?;
        let array: Array = entries
            .dyn_into()
            .map_err(|_| FsAccessError::Decode("directory listing was not an array".to_string()))?;

        for handle in array.iter() {
            if field(&handle, "kind")?.as_string().as_deref() != Some("file") {
                continue;
            }
            if field(&handle, "name")?.as_string().as_deref() != Some(name) {
                continue;
            }
            let buffer = js::read_file(&handle)
                .await
                .map_err(|e| FsAccessError::Js(describe(&e)))?;
            return Ok(Uint8Array::new(&buffer).to_vec());
        }

        Err(FsAccessError::NotFound(name.to_string()))
    }

    /// Write `data` to an immediate file child by name, creating or truncating
    /// it.
    pub async fn upload(&self, name: &str, data: &[u8]) -> Result<(), FsAccessError> {
        js::write_file(&self.dir, name, data)
            .await
            .map_err(|e| FsAccessError::Js(describe(&e)))
    }
}

/// Read a named property from a JS object, mapping a missing or non-object
/// target to a decode error.
fn field(value: &JsValue, key: &str) -> Result<JsValue, FsAccessError> {
    Reflect::get(value, &JsValue::from_str(key))
        .map_err(|e| FsAccessError::Decode(format!("missing property '{key}': {}", describe(&e))))
}

/// Convert a JS array of strings into a `Vec<String>`.
fn string_array(value: &JsValue) -> Result<Vec<String>, FsAccessError> {
    let array: Array = value
        .clone()
        .dyn_into()
        .map_err(|_| FsAccessError::Decode("expected an array of names".to_string()))?;
    let mut names = Vec::new();
    for item in array.iter() {
        let name = item
            .as_string()
            .ok_or_else(|| FsAccessError::Decode("array element was not a string".to_string()))?;
        names.push(name);
    }
    Ok(names)
}

/// Best-effort human-readable rendering of a JS error value.
fn describe(value: &JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
