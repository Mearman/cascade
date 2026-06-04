import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api/client';
import type { BackendEntry, FileEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'] as const;
  const k = 1024;
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), units.length - 1);
  const unit = units[i];
  if (unit === undefined) return `${bytes} B`;
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${unit}`;
}

export function FilesPage() {
  const [backends, setBackends] = useState<BackendEntry[]>([]);
  const [selectedFolder, setSelectedFolder] = useState<string | null>(null);
  const [currentPath, setCurrentPath] = useState('');
  const [breadcrumb, setBreadcrumb] = useState<string[]>(['']);
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadingBackends, setLoadingBackends] = useState(true);
  const [loadingEntries, setLoadingEntries] = useState(false);

  useEffect(() => {
    api.backends()
      .then((r) => {
        const p2p = r.backends.filter((b) => b.folder_id !== null);
        setBackends(p2p);
        const first = p2p[0];
        if (first?.folder_id !== null && first !== undefined) {
          setSelectedFolder(first.folder_id ?? null);
        }
      })
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoadingBackends(false));
  }, []);

  useEffect(() => {
    if (selectedFolder === null) return;
    setLoadingEntries(true);
    setEntries([]);
    setNextCursor(null);
    api.folderChildren(selectedFolder, currentPath)
      .then((r) => {
        setEntries(r.entries);
        setNextCursor(r.next_cursor);
        setError(null);
      })
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoadingEntries(false));
  }, [selectedFolder, currentPath]);

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
    const value = (e.target as HTMLSelectElement).value;
    setSelectedFolder(value);
    setCurrentPath('');
    setBreadcrumb(['']);
  }

  async function loadMore() {
    if (!selectedFolder || nextCursor === null) return;
    try {
      const r = await api.folderChildren(selectedFolder, currentPath, { cursor: nextCursor });
      setEntries((prev) => [...prev, ...r.entries]);
      setNextCursor(r.next_cursor);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (loadingBackends) return <Spinner />;

  if (backends.length === 0) {
    return (
      <div class="files-page">
        <h2>Files</h2>
        <p class="muted">No P2P backends configured.</p>
      </div>
    );
  }

  const folderLabel = backends.find((b) => b.folder_id === selectedFolder)?.name ?? selectedFolder ?? '';

  return (
    <div class="files-page">
      <h2>Files</h2>

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

      <nav class="breadcrumb">
        {breadcrumb.map((seg, i) => (
          <span key={i}>
            {i > 0 && ' / '}
            <button
              class="link"
              onClick={() => navigateToBreadcrumb(i)}
              disabled={i === breadcrumb.length - 1}
            >
              {i === 0 ? folderLabel : seg.split('/').pop() ?? seg}
            </button>
          </span>
        ))}
      </nav>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {loadingEntries ? (
        <Spinner />
      ) : entries.length === 0 ? (
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
                      onClick={() => navigate(entry)}
                      disabled={entry.kind !== 'directory'}
                    >
                      {entry.kind === 'directory' ? '📁 ' : '📄 '}
                      {entry.name}
                    </button>
                  </td>
                  <td>{entry.size !== null ? formatBytes(entry.size) : '—'}</td>
                  <td>{entry.mtime ? new Date(entry.mtime).toLocaleString() : '—'}</td>
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
    </div>
  );
}
