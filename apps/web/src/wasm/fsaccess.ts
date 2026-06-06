// File System Access API interop for the WASM engine.
// Provides typed access to user-granted local directories in Chromium browsers.
// Handles are persisted to IndexedDB so the user only needs to grant access once
// per origin (they must re-grant permission after the browser is restarted).

// showDirectoryPicker is not yet declared as a global in TypeScript's DOM lib;
// declare it here so callers get proper type-checking without an npm dependency.
declare function showDirectoryPicker(options?: {
  mode?: 'read' | 'readwrite';
  id?: string;
}): Promise<FileSystemDirectoryHandle>;

const FS_DB_NAME = 'cascade-fs-handles';
const FS_DB_VERSION = 1;
const FS_STORE_NAME = 'handles';

// ─── Public types ─────────────────────────────────────────────────────────────

export interface FsAccessHandle {
  name: string;
  kind: 'file' | 'directory';
}

export interface FsAccessFile extends FsAccessHandle {
  kind: 'file';
  lastModified: number;
  size: number;
}

export interface FsAccessDirectory extends FsAccessHandle {
  kind: 'directory';
}

export interface FileMetadata {
  name: string;
  lastModified: number;
  size: number;
}

export interface FileSnapshot {
  lastModified: number;
  size: number;
}

export interface DirectoryChanges {
  created: string[];
  modified: string[];
  deleted: string[];
  snapshot: Map<string, FileSnapshot>;
}

// ─── IndexedDB helpers ────────────────────────────────────────────────────────

function openFsDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(FS_DB_NAME, FS_DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(FS_STORE_NAME)) {
        db.createObjectStore(FS_STORE_NAME, { keyPath: 'key' });
      }
    };
    req.onsuccess = () => { resolve(req.result); };
    req.onerror = () => { reject(new Error(String(req.error))); };
  });
}

interface StoredHandleRecord {
  key: string;
  handle: FileSystemDirectoryHandle;
}

function isStoredHandleRecord(value: unknown): value is StoredHandleRecord {
  if (typeof value !== 'object' || value === null) return false;
  if (!('key' in value) || typeof value.key !== 'string') return false;
  if (!('handle' in value)) return false;
  return true;
}

// ─── Type guards ─────────────────────────────────────────────────────────────
// FileSystemDirectoryHandle.values() yields the base FileSystemHandle type.
// These guards narrow to the concrete subtypes so we can call their methods.

function isFileHandle(h: FileSystemHandle): h is FileSystemFileHandle {
  return h.kind === 'file';
}

function isDirHandle(h: FileSystemHandle): h is FileSystemDirectoryHandle {
  return h.kind === 'directory';
}

// ─── Capability detection ─────────────────────────────────────────────────────

// Returns true if the File System Access API is available in this browser.
// The API is supported in Chromium 86+ but not in Firefox or Safari.
export function isFileSystemAccessSupported(): boolean {
  return typeof showDirectoryPicker === 'function';
}

// ─── Directory access ─────────────────────────────────────────────────────────

// Prompts the user to select a directory with read-write access.
// Throws if the user dismisses the picker or if the API is unavailable.
export async function requestDirectory(): Promise<FileSystemDirectoryHandle> {
  if (!isFileSystemAccessSupported()) {
    throw new Error('File System Access API is not supported in this browser');
  }
  return showDirectoryPicker({ mode: 'readwrite' });
}

// Returns all immediate children of the given directory handle.
export async function enumerateDirectory(
  handle: FileSystemDirectoryHandle,
): Promise<(FileSystemDirectoryHandle | FileSystemFileHandle)[]> {
  const entries: (FileSystemDirectoryHandle | FileSystemFileHandle)[] = [];
  for await (const child of handle.values()) {
    if (isFileHandle(child) || isDirHandle(child)) {
      entries.push(child);
    }
  }
  return entries;
}

// ─── File I/O ─────────────────────────────────────────────────────────────────

// Read the full contents of a file as an ArrayBuffer.
export async function readFile(handle: FileSystemFileHandle): Promise<ArrayBuffer> {
  const file = await handle.getFile();
  return file.arrayBuffer();
}

// Write data to a named file within a directory, creating it if necessary.
// BufferSource covers both ArrayBuffer and ArrayBufferView<ArrayBuffer>.
export async function writeFile(
  handle: FileSystemDirectoryHandle,
  name: string,
  data: BufferSource,
): Promise<void> {
  const fileHandle = await handle.getFileHandle(name, { create: true });
  const writable = await fileHandle.createWritable();
  await writable.write(data);
  await writable.close();
}

// Return size and modification time for a file handle.
export async function getFileMetadata(handle: FileSystemFileHandle): Promise<FileMetadata> {
  const file = await handle.getFile();
  return {
    name: file.name,
    lastModified: file.lastModified,
    size: file.size,
  };
}

// ─── Change detection ─────────────────────────────────────────────────────────

// Compare the current state of a directory against a previous snapshot.
// Only tracks immediate file children; subdirectories appear in the
// snapshot as entries with size 0.
export async function detectChanges(
  handle: FileSystemDirectoryHandle,
  previousSnapshot: Map<string, FileSnapshot>,
): Promise<DirectoryChanges> {
  const snapshot = new Map<string, FileSnapshot>();
  const created: string[] = [];
  const modified: string[] = [];

  for await (const child of handle.values()) {
    if (isFileHandle(child)) {
      const file = await child.getFile();
      const current: FileSnapshot = {
        lastModified: file.lastModified,
        size: file.size,
      };
      snapshot.set(child.name, current);

      const prev = previousSnapshot.get(child.name);
      if (prev === undefined) {
        created.push(child.name);
      } else if (
        prev.lastModified !== current.lastModified ||
        prev.size !== current.size
      ) {
        modified.push(child.name);
      }
    }
  }

  const deleted: string[] = [];
  for (const name of previousSnapshot.keys()) {
    if (!snapshot.has(name)) {
      deleted.push(name);
    }
  }

  return { created, modified, deleted, snapshot };
}

// ─── Handle persistence ───────────────────────────────────────────────────────

// Store a directory handle in IndexedDB under the given key.
// The handle can be retrieved across page loads, but the user must
// re-grant read-write permission via requestPermission before use.
export async function persistHandle(
  key: string,
  handle: FileSystemDirectoryHandle,
): Promise<void> {
  const record: StoredHandleRecord = { key, handle };
  const db = await openFsDb();
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(FS_STORE_NAME, 'readwrite');
    const req = tx.objectStore(FS_STORE_NAME).put(record);
    req.onsuccess = () => { resolve(); };
    req.onerror = () => { reject(new Error(String(req.error))); };
  });
}

// Retrieve a persisted directory handle. Returns null if no handle is stored
// for the given key. The caller is responsible for requesting permission
// before performing any I/O on the returned handle.
export async function restoreHandle(
  key: string,
): Promise<FileSystemDirectoryHandle | null> {
  const db = await openFsDb();
  const record: unknown = await new Promise((resolve, reject) => {
    const tx = db.transaction(FS_STORE_NAME, 'readonly');
    const req = tx.objectStore(FS_STORE_NAME).get(key);
    req.onsuccess = () => { resolve(req.result); };
    req.onerror = () => { reject(new Error(String(req.error))); };
  });
  if (!isStoredHandleRecord(record)) return null;
  return record.handle;
}
