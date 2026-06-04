import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api';
import type { FileEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

export function FilesPage() {
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [currentParent, setCurrentParent] = useState<string | null>(null);
  const [path, setPath] = useState<Array<{ id: string | null; name: string }>>([
    { id: null, name: 'Root' },
  ]);

  useEffect(() => {
    setLoading(true);
    api
      .listFolder(currentParent)
      .then((e) => {
        setEntries(e);
        setError(null);
      })
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, [currentParent]);

  function navigate(entry: FileEntry) {
    if (!entry.isDir) return;
    setPath((prev) => [...prev, { id: entry.id, name: entry.name }]);
    setCurrentParent(entry.id);
  }

  function navigateTo(index: number) {
    const target = path[index]!;
    setPath(path.slice(0, index + 1));
    setCurrentParent(target.id);
  }

  async function handlePin(entry: FileEntry, pin: boolean) {
    try {
      if (pin) {
        await api.pin(entry.id);
      } else {
        await api.unpin(entry.id);
      }
      // Refresh.
      const refreshed = await api.listFolder(currentParent);
      setEntries(refreshed);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (loading) return <Spinner />;

  return (
    <div class="files-page">
      <nav class="breadcrumb">
        {path.map((segment, i) => (
          <span key={i}>
            {i > 0 && ' / '}
            <button
              class="link"
              onClick={() => navigateTo(i)}
              disabled={i === path.length - 1}
            >
              {segment.name}
            </button>
          </span>
        ))}
      </nav>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {entries.length === 0 ? (
        <p class="muted">Empty directory.</p>
      ) : (
        <table class="file-table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Size</th>
              <th>Modified</th>
              <th>State</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {entries.map((entry) => (
              <tr key={entry.id}>
                <td>
                  <button
                    class="name-btn"
                    onClick={() => navigate(entry)}
                    disabled={!entry.isDir}
                  >
                    {entry.isDir ? (
                      <span class="icon">📁</span>
                    ) : (
                      <span class="icon">📄</span>
                    )}{' '}
                    {entry.name}
                  </button>
                </td>
                <td>{entry.size !== null ? formatBytes(entry.size) : '—'}</td>
                <td>{entry.modTime ? new Date(entry.modTime).toLocaleString() : '—'}</td>
                <td>
                  <span class={`cache-badge ${entry.cacheState}`}>
                    {entry.cacheState}
                  </span>
                </td>
                <td>
                  <button onClick={() => handlePin(entry, entry.cacheState !== 'pinned')}>
                    {entry.cacheState === 'pinned' ? 'Unpin' : 'Pin'}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i] ?? 'B'}`;
}
