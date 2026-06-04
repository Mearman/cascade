import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api';
import type { ShareEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

export function SharesPage() {
  const [shares, setShares] = useState<ShareEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api
      .shares()
      .then(setShares)
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  async function revoke(peerId: string, folder: string, direction?: string) {
    try {
      await api.revokeShare(peerId, folder, direction);
      setShares((prev) =>
        prev.filter(
          (s) =>
            !(s.peerId === peerId && s.folder === folder) ||
            (direction !== undefined && s.direction !== direction),
        ),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (loading) return <Spinner />;

  return (
    <div class="shares-page">
      <h2>Shares</h2>
      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
      {shares.length === 0 ? (
        <p class="muted">No shares configured.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Peer</th>
              <th>Folder</th>
              <th>Direction</th>
              <th>Expires</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {shares.map((s) => (
              <tr key={`${s.peerId}/${s.folder}/${s.direction}`}>
                <td>
                  <code>{s.peerId}</code>
                </td>
                <td>{s.folder}</td>
                <td>{s.direction}</td>
                <td>{s.expiresAt ? new Date(s.expiresAt).toLocaleString() : 'never'}</td>
                <td>
                  <button
                    class="danger"
                    onClick={() => revoke(s.peerId, s.folder, s.direction)}
                  >
                    Revoke
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
