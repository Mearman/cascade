import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api/client';
import type { TokenEntry } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

function scopeLabel(entry: TokenEntry): string {
  if (entry.scope.kind === 'node') return '*';
  return entry.scope.path;
}

export function TokensPage() {
  const [tokens, setTokens] = useState<TokenEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api.tokens()
      .then((r) => setTokens(r.tokens))
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  async function revoke(tokenId: string) {
    try {
      await api.revokeToken(tokenId);
      setTokens((prev) =>
        prev.map((t) => (t.token_id === tokenId ? { ...t, revoked: true } : t)),
      );
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
              <th>Token ID</th>
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
              <tr key={t.token_id}>
                <td><code>{t.token_id.slice(0, 8)}…</code></td>
                <td><code>{t.bearer}</code></td>
                <td>{t.capability}</td>
                <td><code>{scopeLabel(t)}</code></td>
                <td>{new Date(t.expires).toLocaleString()}</td>
                <td>
                  {t.revoked ? (
                    <span class="badge danger">revoked</span>
                  ) : (
                    <span class="badge ok">active</span>
                  )}
                </td>
                <td>
                  {!t.revoked && (
                    <button class="danger" onClick={() => void revoke(t.token_id)}>
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
