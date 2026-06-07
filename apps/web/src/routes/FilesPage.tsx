import { useContext, useEffect, useRef, useState } from 'preact/hooks';
import { api } from '@/api/client';
import type { BackendEntry, FileEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';
import { RuntimeMode } from '@/wasm';
import { AppContext } from '@/context';
import { fetchFolderChildren } from '@/wasm/gdrive';

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'] as const;
  const k = 1024;
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), units.length - 1);
  const unit = units[i];
  if (unit === undefined) return `${String(bytes)} B`;
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${unit}`;
}

// WASM engine backend shape (differs from daemon BackendEntry).
interface WasmBackend {
  id: string;
  type: string;
  display_name: string;
  hasHandle: boolean;
}

function isWasmBackend(value: unknown): value is WasmBackend {
  if (typeof value !== 'object' || value === null) return false;
  return 'id' in value && typeof value.id === 'string'
    && 'type' in value && typeof value.type === 'string'
    && 'display_name' in value && typeof value.display_name === 'string'
    && 'hasHandle' in value && typeof value.hasHandle === 'boolean';
}

// WASM engine folder child shape (differs from daemon FileEntry).
interface WasmChild {
  id: string;
  name: string;
  is_dir: boolean;
  size: number;
  mime_type: string | null;
}

function isWasmChild(value: unknown): value is WasmChild {
  if (typeof value !== 'object' || value === null) return false;
  return 'id' in value && typeof value.id === 'string'
    && 'name' in value && typeof value.name === 'string'
    && 'is_dir' in value && typeof value.is_dir === 'boolean'
    && 'size' in value && typeof value.size === 'number';
}

function isWasmBackendsBody(value: unknown): value is { backends: unknown[] } {
  if (typeof value !== 'object' || value === null) return false;
  return 'backends' in value && Array.isArray(value.backends);
}

function isWasmChildrenBody(value: unknown): value is { children: unknown[] } {
  if (typeof value !== 'object' || value === null) return false;
  return 'children' in value && Array.isArray(value.children);
}

// Extract the native Drive ID from a full engine ItemId ("backendId:nativeId").
function nativeIdFromItemId(itemId: string): string {
  const idx = itemId.indexOf(':');
  return idx >= 0 ? itemId.substring(idx + 1) : itemId;
}

export function FilesPage() {
  const { mode } = useContext(AppContext);
  const isWasm = mode === RuntimeMode.BrowseOnly || mode === RuntimeMode.Standalone;

  // Shared state
  const [error, setError] = useState<string | null>(null);
  const [loadingBackends, setLoadingBackends] = useState(true);
  const [loadingEntries, setLoadingEntries] = useState(false);

  // Connected-mode state
  const [backends, setBackends] = useState<BackendEntry[]>([]);
  const [selectedFolder, setSelectedFolder] = useState<string | null>(null);
  const [currentPath, setCurrentPath] = useState('');
  const [breadcrumb, setBreadcrumb] = useState<string[]>(['']);
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);

  // WASM-mode state
  const [wasmBackends, setWasmBackends] = useState<WasmBackend[]>([]);
  const [selectedBackendId, setSelectedBackendId] = useState<string | null>(null);
  const [currentDriveFolderId, setCurrentDriveFolderId] = useState<string | null>(null);
  const [wasmEntries, setWasmEntries] = useState<WasmChild[]>([]);
  const [wasmBreadcrumb, setWasmBreadcrumb] = useState<{ name: string; folderId: string }[]>([]);
  const [fetchingDrive, setFetchingDrive] = useState(false);

  // ── Load backends ────────────────────────────────────────────────────────

  useEffect(() => {
    if (isWasm) {
      api.rawRequest('GET', '/v1/backends')
        .then((result) => {
          if (result.status !== 200) {
            setError(`Failed to load backends: status ${String(result.status)}`);
            return;
          }
          if (!isWasmBackendsBody(result.body)) {
            setError('Invalid backends response from engine');
            return;
          }
          const valid = result.body.backends.filter(isWasmBackend);
          setWasmBackends(valid);
          const first = valid[0];
          if (first !== undefined) {
            setSelectedBackendId(first.id);
            const rootId = `${first.id}:root`;
            setCurrentDriveFolderId(rootId);
            setWasmBreadcrumb([{ name: first.display_name, folderId: rootId }]);
          }
        })
        .catch((err: unknown) => { setError(err instanceof Error ? err.message : String(err)); })
        .finally(() => { setLoadingBackends(false); });
    } else {
      api.backends()
        .then((r) => {
          const p2p = r.backends.filter((b) => b.folder_id !== null);
          setBackends(p2p);
          const first = p2p[0];
          if (first !== undefined && first.folder_id !== null) {
            setSelectedFolder(first.folder_id);
          }
        })
        .catch((err: unknown) => { setError(err instanceof Error ? err.message : String(err)); })
        .finally(() => { setLoadingBackends(false); });
    }
  }, []);

  // ── Load entries (Connected mode) ───────────────────────────────────────

  useEffect(() => {
    if (isWasm || selectedFolder === null) return;
    setLoadingEntries(true);
    setEntries([]);
    setNextCursor(null);
    api.folderChildren(selectedFolder, currentPath)
      .then((r) => {
        setEntries(r.entries);
        setNextCursor(r.next_cursor);
        setError(null);
      })
      .catch((err: unknown) => { setError(err instanceof Error ? err.message : String(err)); })
      .finally(() => { setLoadingEntries(false); });
  }, [selectedFolder, currentPath]);

  // ── Load entries (WASM mode) ────────────────────────────────────────────

  const wasmEffectGen = useRef(0);

  useEffect(() => {
    if (!isWasm || selectedBackendId === null || currentDriveFolderId === null) return;
    const gen = ++wasmEffectGen.current;
    const stale = (): boolean => wasmEffectGen.current !== gen;

    setLoadingEntries(true);
    setWasmEntries([]);
    setFetchingDrive(false);

    const folderId = currentDriveFolderId;
    const backendId = selectedBackendId;

    api.rawRequest('GET', `/v1/folders/${folderId}/children`)
      .then(async (result) => {
        if (stale()) return;

        if (result.status !== 200) {
          setError(`Failed to load folder: status ${String(result.status)}`);
          return;
        }

        if (!isWasmChildrenBody(result.body)) {
          setError('Invalid folder children response from engine');
          return;
        }

        const children = result.body.children.filter(isWasmChild);

        if (children.length === 0) {
          // Engine has no entries for this folder — fetch from Drive API.
          setFetchingDrive(true);
          try {
            const driveParentId = nativeIdFromItemId(folderId);
            const fetched = await fetchFolderChildren(backendId, driveParentId);
            if (stale()) return;
            if (fetched > 0) {
              const retry = await api.rawRequest('GET', `/v1/folders/${folderId}/children`);
              if (stale()) return;
              if (retry.status === 200 && isWasmChildrenBody(retry.body)) {
                setWasmEntries(retry.body.children.filter(isWasmChild));
              }
            }
          } catch (driveErr: unknown) {
            if (!stale()) {
              setError(driveErr instanceof Error ? driveErr.message : String(driveErr));
            }
          } finally {
            if (!stale()) setFetchingDrive(false);
          }
        } else {
          setWasmEntries(children);
        }

        if (!stale()) setError(null);
      })
      .catch((err: unknown) => {
        if (!stale()) setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!stale()) setLoadingEntries(false);
      });
  }, [selectedBackendId, currentDriveFolderId]);

  // ── Navigation (Connected mode) ─────────────────────────────────────────

  function navigate(entry: FileEntry) {
    if (entry.kind !== 'directory') return;
    const next = currentPath === '' ? entry.name : `${currentPath}/${entry.name}`;
    setBreadcrumb((prev) => [...prev, next]);
    setCurrentPath(next);
  }

  function navigateToBreadcrumb(index: number) {
    const target = breadcrumb[index];
    if (target === undefined) return;
    setBreadcrumb(breadcrumb.slice(0, index + 1));
    setCurrentPath(target);
  }

  function handleFolderChange(e: Event) {
    const target = e.target;
    if (!(target instanceof HTMLSelectElement)) return;
    setSelectedFolder(target.value);
    setCurrentPath('');
    setBreadcrumb(['']);
  }

  async function loadMore() {
    if (selectedFolder === null || nextCursor === null) return;
    try {
      const r = await api.folderChildren(selectedFolder, currentPath, { cursor: nextCursor });
      setEntries((prev) => [...prev, ...r.entries]);
      setNextCursor(r.next_cursor);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  // ── Navigation (WASM mode) ──────────────────────────────────────────────

  function navigateWasm(child: WasmChild) {
    if (!child.is_dir) return;
    setCurrentDriveFolderId(child.id);
    setWasmBreadcrumb((prev) => [...prev, { name: child.name, folderId: child.id }]);
  }

  function navigateWasmToBreadcrumb(index: number) {
    const target = wasmBreadcrumb[index];
    if (target === undefined) return;
    setWasmBreadcrumb(wasmBreadcrumb.slice(0, index + 1));
    setCurrentDriveFolderId(target.folderId);
  }

  function handleWasmBackendChange(e: Event) {
    const target = e.target;
    if (!(target instanceof HTMLSelectElement)) return;
    const value = target.value;
    const backend = wasmBackends.find((b) => b.id === value);
    const displayName = backend !== undefined ? backend.display_name : value;
    setSelectedBackendId(value);
    const rootId = `${value}:root`;
    setCurrentDriveFolderId(rootId);
    setWasmBreadcrumb([{ name: displayName, folderId: rootId }]);
  }

  // ── Rendering ───────────────────────────────────────────────────────────

  if (loadingBackends) return <Spinner />;

  const hasNoBackends = isWasm
    ? wasmBackends.length === 0
    : backends.length === 0;

  if (hasNoBackends) {
    return (
      <div class="files-page">
        <h2>Files</h2>
        {error !== null && <ErrorBanner message={error} onDismiss={() => { setError(null); }} />}
        {isWasm ? (
          <p class="muted">No backends registered. <a href="/login">Log in</a> to connect Google Drive.</p>
        ) : (
          <p class="muted">No P2P backends configured.</p>
        )}
      </div>
    );
  }

  const folderLabel = isWasm
    ? (wasmBreadcrumb[0]?.name ?? '')
    : (backends.find((b) => b.folder_id === selectedFolder)?.name ?? selectedFolder ?? '');

  return (
    <div class="files-page">
      <h2>Files</h2>

      {isWasm ? (
        <div class="folder-picker">
          <label>
            Backend
            <select value={selectedBackendId ?? ''} onChange={handleWasmBackendChange}>
              {wasmBackends.map((b) => (
                <option key={b.id} value={b.id}>
                  {b.display_name}
                </option>
              ))}
            </select>
          </label>
        </div>
      ) : (
        <div class="folder-picker">
          <label>
            Folder
            <select value={selectedFolder ?? ''} onChange={handleFolderChange}>
              {backends.map((b) => (
                <option key={b.folder_id} value={b.folder_id ?? ''}>
                  {b.name} ({b.folder_id})
                </option>
              ))}
            </select>
          </label>
        </div>
      )}

      <nav class="breadcrumb">
        {isWasm
          ? wasmBreadcrumb.map((seg, i) => (
              <span key={seg.folderId}>
                {i > 0 && ' / '}
                <button
                  class="link"
                  onClick={() => { navigateWasmToBreadcrumb(i); }}
                  disabled={i === wasmBreadcrumb.length - 1}
                >
                  {i === 0 ? folderLabel : seg.name}
                </button>
              </span>
            ))
          : breadcrumb.map((seg, i) => (
              <span key={i}>
                {i > 0 && ' / '}
                <button
                  class="link"
                  onClick={() => { navigateToBreadcrumb(i); }}
                  disabled={i === breadcrumb.length - 1}
                >
                  {i === 0 ? folderLabel : seg.split('/').pop() ?? seg}
                </button>
              </span>
            ))
        }
      </nav>

      {error !== null && <ErrorBanner message={error} onDismiss={() => { setError(null); }} />}

      {fetchingDrive && <p class="muted">Fetching folder contents from Google Drive...</p>}

      {loadingEntries ? (
        <Spinner />
      ) : isWasm ? (
        wasmEntries.length === 0 ? (
          <p class="muted">Empty directory.</p>
        ) : (
          <table class="file-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Size</th>
                <th>Modified</th>
              </tr>
            </thead>
            <tbody>
              {wasmEntries.map((entry) => (
                <tr key={entry.id}>
                  <td>
                    <button
                      class="name-btn"
                      onClick={() => { navigateWasm(entry); }}
                      disabled={!entry.is_dir}
                    >
                      {entry.is_dir ? '📁 ' : '📄 '}
                      {entry.name}
                    </button>
                  </td>
                  <td>{!entry.is_dir ? formatBytes(entry.size) : '—'}</td>
                  <td>—</td>
                </tr>
              ))}
            </tbody>
          </table>
        )
      ) : (
        <>
          {entries.length === 0 ? (
            <p class="muted">Empty directory.</p>
          ) : (
            <>
              <table class="file-table">
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Size</th>
                    <th>Modified</th>
                  </tr>
                </thead>
                <tbody>
                  {entries.map((entry) => (
                    <tr key={entry.name}>
                      <td>
                        <button
                          class="name-btn"
                          onClick={() => { navigate(entry); }}
                          disabled={entry.kind !== 'directory'}
                        >
                          {entry.kind === 'directory' ? '📁 ' : '📄 '}
                          {entry.name}
                        </button>
                      </td>
                      <td>{entry.size !== null ? formatBytes(entry.size) : '—'}</td>
                      <td>{entry.mtime !== null ? new Date(entry.mtime).toLocaleString() : '—'}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {nextCursor !== null && (
                <button class="secondary" onClick={() => void loadMore()}>
                  Load more
                </button>
              )}
            </>
          )}
        </>
      )}
    </div>
  );
}
