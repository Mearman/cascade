import { useState } from 'preact/hooks';
import { saveToken } from '@/auth';
import type { AuthToken, Role } from '@/api/types';

export function LoginPage() {
  const [tokenJson, setTokenJson] = useState('');
  const [error, setError] = useState<string | null>(null);

  function parseRole(raw: string): Role | null {
    const trimmed = raw.trim().toLowerCase();
    if (trimmed === 'owner' || trimmed === 'named-user' || trimmed === 'bearer') {
      return trimmed;
    }
    return null;
  }

  async function handleSubmit(ev: Event) {
    ev.preventDefault();
    setError(null);

    try {
      const parsed = JSON.parse(tokenJson);
      if (typeof parsed !== 'object' || parsed === null) {
        throw new Error('Token must be a JSON object.');
      }

      const role = parseRole(parsed['role'] ?? '');
      if (!role) {
        throw new Error('Token must have a valid role: owner, named-user, or bearer.');
      }

      const token: AuthToken = {
        role,
        deviceId: parsed['deviceId'] ?? undefined,
        issuerId: parsed['issuerId'] ?? undefined,
        scope: parsed['scope'] ?? undefined,
        expiresAt: parsed['expiresAt'] ?? undefined,
      };

      saveToken(token);
      window.location.reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Invalid token');
    }
  }

  return (
    <div class="login-page">
      <h1>Cascade</h1>
      <p>Enter your authentication token to continue.</p>
      <form onSubmit={handleSubmit}>
        <textarea
          rows={8}
          placeholder={'{\n  "role": "owner",\n  "deviceId": "…"\n}'}
          value={tokenJson}
          onInput={(e) => setTokenJson((e.target as HTMLTextAreaElement).value)}
        />
        {error && <p class="error">{error}</p>}
        <button type="submit">Connect</button>
      </form>
    </div>
  );
}
