import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api/client';
import type { ShareEntry, SharePosture } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

function postureBadgeClass(posture: SharePosture): string {
  if (posture === 'read-only') return 'badge badge-read';
  if (posture === 'write-only') return 'badge badge-write';
  return 'badge badge-readwrite';
}

export function SharesPage() {
  const [shares, setShares] = useState<ShareEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api.shares()
      .then((r) => { setShares(r.shares); })
      .catch((err: unknown) => { setError(err instanceof Error ? err.message : String(err)); })
      .finally(() => { setLoading(false); });
  }, []);

  async function revoke(id: number) {
    try {
      await api.deleteShare(id);
      setShares((prev) => prev.filter((s) => !s.grant_ids.includes(id)));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (loading) return <Spinner />;

  return (
    <div class="shares-page">
      <h2>Shares</h2>
      {error !== null && error !== '' && <ErrorBanner message={error} onDismiss={() => { setError(null); }} />}
      {shares.length === 0 ? (
        <p class="muted">No shares configured.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Peer</th>
              <th>Folder</th>
              <th>Posture</th>
              <th>Expires</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {shares.map((s) => (
              <tr key={`${s.peer_device_id}/${s.folder_id}`}>
                <td><code>{s.peer_device_id}</code></td>
                <td>
                  <span title={s.folder_id}>{s.folder}</span>
                </td>
                <td>
                  <span class={postureBadgeClass(s.posture)}>{s.posture}</span>
                </td>
                <td>{s.expires !== null ? new Date(s.expires).toLocaleString() : 'never'}</td>
                <td>
                  {s.grant_ids.map((id) => (
                    <button
                      key={id}
                      class="danger"
                      onClick={() => void revoke(id)}
                    >
                      Revoke
                    </button>
                  ))}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
