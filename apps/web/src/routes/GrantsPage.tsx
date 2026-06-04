import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api';
import type { GrantEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

export function GrantsPage() {
  const [grants, setGrants] = useState<GrantEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api
      .grants()
      .then(setGrants)
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  async function revoke(id: number) {
    try {
      await api.revokeGrant(id);
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
                <td>
                  <code>{g.grantee}</code>
                </td>
                <td>{g.capability}</td>
                <td>
                  {g.scopeKind === 'node' ? '*' : g.scopePath}
                </td>
                <td>{g.expiresAt ? new Date(g.expiresAt).toLocaleString() : 'never'}</td>
                <td>
                  <code>{g.grantedBy}</code>
                </td>
                <td>
                  <button class="danger" onClick={() => revoke(g.id)}>
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
