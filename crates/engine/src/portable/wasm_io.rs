//! WASM adapters for the portable HTTP and filesystem contracts.
//!
//! - [`WasmHttpClient`] uses the browser's `fetch()` API via `web_sys`.
//! - [`WasmFileSystem`] is a no-op stub pending real File System Access API
//!   wiring (the browser API requires a user-granted directory handle that
//!   cannot be driven purely from Rust).
//!
//! The companion module [`super::wasm`] supplies [`super::RuntimeHandle`] and
//! [`super::StateStorage`] adapters for the wasm target. Both modules compile
//! only when `target_arch = "wasm32"` and the `portable` feature is active.

use std::path::Path;

use async_trait::async_trait;
use js_sys::{Promise, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, Response};

use super::{FileSystem, FsDirEntry, FsError, HeaderMap, HttpClient, HttpError, HttpResponse};

// ─────────────────────────── HTTP ───────────────────────────

/// [`HttpClient`] backed by the browser's `fetch()` API.
///
/// Every method goes through [`send`], which builds a `web_sys::Request`,
/// calls `window().fetch_with_request`, and buffers the response body into
/// an [`HttpResponse`].
#[derive(Debug, Clone)]
pub struct WasmHttpClient;

/// Issue a GET request.
#[async_trait]
impl HttpClient for WasmHttpClient {
    async fn get(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError> {
        send("GET", url, &headers, None).await
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        send("POST", url, &headers, Some(body)).await
    }

    async fn put(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        send("PUT", url, &headers, Some(body)).await
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError> {
        send("DELETE", url, &headers, None).await
    }

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError> {
        send("HEAD", url, &headers, None).await
    }

    async fn patch(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        send("PATCH", url, &headers, Some(body)).await
    }
}

/// Build a `web_sys::Request`, send it via the browser's `fetch()`, and
/// buffer the full response into the portable [`HttpResponse`].
///
/// Error mapping:
/// - A `Request::new()` rejection (malformed URL) becomes [`HttpError::InvalidUrl`].
/// - A `fetch()` failure (network) becomes [`HttpError::Connection`].
/// - An abort/timeout surfaces as [`HttpError::Timeout`].
/// - Everything else becomes [`HttpError::Request`].
async fn send(
    method: &str,
    url: &str,
    headers: &HeaderMap,
    body: Option<Vec<u8>>,
) -> Result<HttpResponse, HttpError> {
    let mut init = RequestInit::new();
    init.method(method);

    if let Some(data) = body {
        let array = Uint8Array::new_with_length(
            data.len()
                .try_into()
                .map_err(|_| HttpError::Request("body too large for Uint8Array".into()))?,
        );
        array.copy_from(&data);
        let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(
            &JsValue::from(array.buffer()),
            &web_sys::BlobPropertyBag::new().type_("application/octet-stream"),
        )
        .map_err(|e| HttpError::Request(format!("failed to create Blob: {e:?}")))?;
        init.body(Some(&blob));
    }

    let request = Request::new_with_str_and_init(url, &init).map_err(|e| {
        HttpError::InvalidUrl(format!("failed to build request for '{url}': {e:?}"))
    })?;

    let request_headers = request.headers();
    for (name, value) in headers.as_pairs() {
        request_headers
            .set(name, value)
            .map_err(|e| HttpError::Request(format!("invalid header '{name}': {e:?}")))?;
    }

    let window = web_sys::window()
        .ok_or_else(|| HttpError::Connection("no window object available".into()))?;

    let promise: Promise = window.fetch_with_request(&request);
    let response_value = JsFuture::from(promise)
        .await
        .map_err(|e| map_fetch_err(&e))?;

    let response: Response = response_value
        .dyn_into()
        .map_err(|e| HttpError::Request(format!("fetch did not return a Response: {e:?}")))?;

    let status = response.status();

    let mut out_headers = HeaderMap::new();
    let response_headers = response.headers();
    // The Headers JS object implements iteration. Use js_sys to grab key/value
    // pairs, since web_sys does not expose a direct iterator for it.
    let iterator = js_sys::try_iter(&response_headers)
        .map_err(|e| HttpError::Request(format!("failed to iterate response headers: {e:?}")))?;
    if let Some(iterator) = iterator {
        for entry in iterator {
            let entry = entry
                .map_err(|e| HttpError::Request(format!("error reading header entry: {e:?}")))?;
            // Each entry is [name, value].
            let array = js_sys::Array::from(&entry);
            let name = array.get(0).as_string();
            let value = array.get(1).as_string();
            if let (Some(n), Some(v)) = (name, value) {
                out_headers.insert(n, v);
            }
        }
    }

    let body = if method == "HEAD" {
        Vec::new()
    } else {
        let array_buffer_promise: Promise = response
            .array_buffer()
            .map_err(|e| HttpError::Request(format!("array_buffer() failed: {e:?}")))?;
        let array_buffer = JsFuture::from(array_buffer_promise)
            .await
            .map_err(|e| map_fetch_err(&e))?;
        let uint8 = Uint8Array::new(&array_buffer);
        let mut buf = vec![0u8; uint8.length() as usize];
        uint8.copy_to(&mut buf);
        buf
    };

    Ok(HttpResponse {
        status,
        headers: out_headers,
        body,
    })
}

/// Map a JS error from a rejected fetch/array_buffer promise into the portable
/// [`HttpError`] vocabulary. Aborts and timeouts surface as [`HttpError::Timeout`];
/// network failures as [`HttpError::Connection`]; everything else as
/// [`HttpError::Request`].
fn map_fetch_err(err: &JsValue) -> HttpError {
    let msg = format!("{err:?}");
    // DOMException names are the standard way to distinguish abort/timeout from
    // generic network errors.
    if let Some(obj) = err.dyn_ref::<js_sys::Object>() {
        let name_key = JsValue::from_str("name");
        if let Some(name) = js_sys::Reflect::get(obj, &name_key).ok() {
            if let Some(name_str) = name.as_string() {
                if name_str == "AbortError" {
                    return HttpError::Timeout;
                }
                if name_str == "TimeoutError" {
                    return HttpError::Timeout;
                }
                if name_str == "TypeError" {
                    // fetch() rejects with TypeError on network-level failures
                    // (DNS, refused connection, CORS block).
                    return HttpError::Connection(msg);
                }
            }
        }
    }
    HttpError::Request(msg)
}

// ─────────────────────────── Filesystem ───────────────────────────

/// [`FileSystem`] stub for the browser target.
///
/// The File System Access API requires a user-granted directory handle that
/// cannot be obtained purely from Rust; it must flow from the PWA's directory
/// picker. Until that wiring lands, every method returns
/// [`FsError::Other`] explaining the constraint. This stub lets the engine
/// compile with all four portable traits satisfied.
#[derive(Debug, Clone, Copy)]
pub struct WasmFileSystem;

const FS_ACCESS_REQUIRED: &str = "File System Access API requires a granted directory handle";

#[async_trait]
impl FileSystem for WasmFileSystem {
    async fn read_dir(&self, _path: &Path) -> Result<Vec<FsDirEntry>, FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }

    async fn read_file(&self, _path: &Path) -> Result<Vec<u8>, FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }

    async fn write_file(&self, _path: &Path, _data: &[u8]) -> Result<(), FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }

    async fn create_dir(&self, _path: &Path) -> Result<(), FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }

    async fn remove_file(&self, _path: &Path) -> Result<(), FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }

    async fn remove_dir(&self, _path: &Path) -> Result<(), FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }

    async fn exists(&self, _path: &Path) -> Result<bool, FsError> {
        Err(FsError::Other(FS_ACCESS_REQUIRED.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::{FS_ACCESS_REQUIRED, WasmFileSystem};
    use crate::portable::FileSystem;
    use std::path::Path;

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn wasm_filesystem_read_dir_returns_stub_error() {
        let fs = WasmFileSystem;
        let result = fs.read_dir(Path::new("/tmp")).await;
        match result {
            Err(crate::portable::FsError::Other(msg)) => {
                assert_eq!(msg, FS_ACCESS_REQUIRED);
            }
            other => panic!("expected Other error, got {other:?}"),
        }
    }
}
