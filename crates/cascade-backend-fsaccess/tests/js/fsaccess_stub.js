// Stub module for wasm-bindgen-test --node.
//
// Mirrors the contract of apps/web/src/wasm/fsaccess.ts exactly: the Rust
// extern block (under feature js-test-stub) imports this module instead of the
// production .ts file. Node can load plain .js without a transpiler; it cannot
// load .ts at all.
//
// Only the four functions called by the Rust bridge are exported. The stub
// synthesises the exact JS-object shapes the Rust decode paths expect:
//   - requestDirectory  → a fake dir handle object  (opaque to Rust)
//   - enumerateDirectory → an Array of {kind, name} handle objects
//   - readFile          → an ArrayBuffer with known bytes
//   - writeFile         → records the call in LAST_WRITE and resolves
//   - detectChanges     → returns {created, modified, deleted, snapshot: Map}
//
// The inspector API (getLastWrite, setNextEntries, setNextChanges,
// setNextReject) is also exported so Rust tests can drive the stub state via
// wasm-bindgen imports of this same module.

// ── Module-level state driven by tests ───────────────────────────────────────

// The entries that the next enumerateDirectory call will return.
// Each entry is { kind: 'file'|'directory', name: string, bytes?: Uint8Array }.
let NEXT_ENTRIES = [];

// The result that the next detectChanges call will return.
// If set to a string, detectChanges rejects with that message.
let NEXT_CHANGES = null;

// The last writeFile call recorded as { name, bytes }.
let LAST_WRITE = null;

// ── Inspector exports (called from Rust test via stub imports) ────────────────

export function setNextEntries(entriesJson) {
  NEXT_ENTRIES = JSON.parse(entriesJson);
}

export function setNextChanges(changesJson) {
  NEXT_CHANGES = JSON.parse(changesJson);
}

export function setNextReject(message) {
  // Sentinel: a string in NEXT_CHANGES means "reject with this message".
  NEXT_CHANGES = { __reject: message };
}

export function getLastWriteName() {
  return LAST_WRITE ? LAST_WRITE.name : null;
}

export function getLastWriteBytes() {
  if (!LAST_WRITE) return null;
  return LAST_WRITE.bytes.buffer;
}

export function resetState() {
  NEXT_ENTRIES = [];
  NEXT_CHANGES = null;
  LAST_WRITE = null;
}

// ── Bridge exports (called from wasm-generated glue) ─────────────────────────

// Returns an opaque "directory handle" object. The Rust side never inspects it;
// it only passes it back to enumerateDirectory / detectChanges.
export async function requestDirectory() {
  return { __stub: 'dir-handle' };
}

// Returns an Array of handle objects shaped like FileSystemFileHandle /
// FileSystemDirectoryHandle: each has a `kind` and `name` property.
// readFile is available on file handles.
export async function enumerateDirectory(_handle) {
  return NEXT_ENTRIES.map((entry) => {
    const handle = { kind: entry.kind, name: entry.name };
    if (entry.kind === 'file') {
      // readFile will be called on this handle object.
      handle.__bytes = entry.bytes ? new Uint8Array(entry.bytes) : new Uint8Array(0);
    }
    return handle;
  });
}

// Returns the ArrayBuffer for the given file handle.
export async function readFile(handle) {
  // handle is the object returned by enumerateDirectory.
  const bytes = handle.__bytes ?? new Uint8Array(0);
  // Return the underlying ArrayBuffer (Uint8Array::new(&buffer) in Rust needs
  // the raw ArrayBuffer, not a TypedArray view).
  return bytes.buffer;
}

// Records the write and resolves without error.
export async function writeFile(_dir, name, data) {
  // data arrives as a Uint8Array (wasm-bindgen maps &[u8] to Uint8Array).
  LAST_WRITE = { name, bytes: new Uint8Array(data) };
}

// Returns a DirectoryChanges-shaped object:
//   { created: string[], modified: string[], deleted: string[], snapshot: Map }
// If NEXT_CHANGES has __reject, rejects with that message.
export async function detectChanges(_handle, _snapshot) {
  if (NEXT_CHANGES && typeof NEXT_CHANGES.__reject === 'string') {
    throw new Error(NEXT_CHANGES.__reject);
  }
  if (NEXT_CHANGES) {
    const c = NEXT_CHANGES;
    // Build the Map from snapshot entries if provided.
    const snapshot = new Map();
    if (c.snapshotEntries) {
      for (const [k, v] of Object.entries(c.snapshotEntries)) {
        snapshot.set(k, v);
      }
    }
    return {
      created: c.created ?? [],
      modified: c.modified ?? [],
      deleted: c.deleted ?? [],
      snapshot,
    };
  }
  return {
    created: [],
    modified: [],
    deleted: [],
    snapshot: new Map(),
  };
}
