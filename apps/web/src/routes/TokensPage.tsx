import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api';
import type { TokenEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

export function TokensPage() {
  const [tokens, setTokens] = useState<TokenEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api
      .tokens()
      .then(setTokens)
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  async function revoke(id: string) {
    try {
      await api.revokeToken(id);
      setTokens((prev) => prev.filter((t) => t.id !== id));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (loading) return <Spinner />;

  return (
    <div class="tokens-page">
      <h2>Tokens</h2>
      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
      {tokens.length === 0 ? (
        <p class="muted">No tokens issued.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Bearer</th>
              <th>Capability</th>
              <th>Scope</th>
              <th>Expires</th>
              <th>Status</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {tokens.map((t) => (
              <tr key={t.id}>
                <td>
                  <code>{t.bearerId}</code>
                </td>
                <td>{t.capability}</td>
                <td>
                  {t.scopeKind === 'node' ? '*' : t.scopePath}
                </td>
                <td>{new Date(t.expiresAt).toLocaleString()}</td>
                <td>
                  {t.revoked ? (
                    <span class="badge danger">revoked</span>
                  ) : (
                    <span class="badge ok">active</span>
                  )}
                </td>
                <td>
                  {!t.revoked && (
                    <button class="danger" onClick={() => revoke(t.id)}>
                      Revoke
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
