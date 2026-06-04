import { useState } from 'preact/hooks';
import { validateToken, isCapabilityToken } from '@/auth';
import { ApiError } from '@/api/types';

export function LoginPage() {
  const [tokenJson, setTokenJson] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  async function handleSubmit(ev: Event) {
    ev.preventDefault();
    setError(null);
    setLoading(true);

    try {
      const parsed: unknown = JSON.parse(tokenJson);
      if (!isCapabilityToken(parsed)) {
        throw new Error(
          'Token must be a CapabilityToken JSON object with token_id, issuer, bearer, capability, scope, expires, and issued_at fields.',
        );
      }
      await validateToken(parsed);
      window.location.reload();
    } catch (err) {
      if (err instanceof ApiError) {
        setError(`${err.code}: ${err.message}`);
      } else {
        setError(err instanceof Error ? err.message : 'Invalid token');
      }
    } finally {
      setLoading(false);
    }
  }

  return (
    <div class="login-page">
      <h1>Cascade</h1>
      <p>Paste your CapabilityToken JSON to continue.</p>
      <form onSubmit={handleSubmit}>
        <textarea
          rows={10}
          placeholder={'{\n  "token_id": "…",\n  "bearer": "…",\n  "capability": "status:read",\n  …\n}'}
          value={tokenJson}
          onInput={(e) => setTokenJson((e.target as HTMLTextAreaElement).value)}
          disabled={loading}
        />
        {error && <p class="error">{error}</p>}
        <button type="submit" disabled={loading}>
          {loading ? 'Verifying…' : 'Connect'}
        </button>
      </form>
    </div>
  );
}
