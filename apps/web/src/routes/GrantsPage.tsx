import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api/client';
import type { GrantEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

function scopeLabel(entry: GrantEntry): string {
  if (entry.scope.kind === 'node') return '*';
  return entry.scope.path;
}

export function GrantsPage() {
  const [grants, setGrants] = useState<GrantEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api.grants()
      .then((r) => setGrants(r.grants))
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  async function revoke(id: number) {
    try {
      await api.deleteGrant(id);
      setGrants((prev) => prev.filter((g) => g.id !== id));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (loading) return <Spinner />;

  return (
    <div class="grants-page">
      <h2>Grants</h2>
      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
      {grants.length === 0 ? (
        <p class="muted">No grants configured.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Grantee</th>
              <th>Capability</th>
              <th>Scope</th>
              <th>Expires</th>
              <th>Granted by</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {grants.map((g) => (
              <tr key={g.id}>
                <td><code>{g.grantee}</code></td>
                <td>{g.capability}</td>
                <td><code>{scopeLabel(g)}</code></td>
                <td>{g.expires ? new Date(g.expires).toLocaleString() : 'never'}</td>
                <td><code>{g.granted_by}</code></td>
                <td>
                  <button class="danger" onClick={() => void revoke(g.id)}>
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
