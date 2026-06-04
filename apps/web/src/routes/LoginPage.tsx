import { useState, useEffect, useContext } from 'preact/hooks';
import { validateToken, isCapabilityToken } from '@/auth';
import { ApiError } from '@/api/types';
import { AppContext } from '@/context';
import { RuntimeMode } from '@/wasm';
import { initiateAuth, handleCallback, GDRIVE_SCOPES } from '@/wasm/oauth';
import { Spinner } from '@/components';

const OAUTH_CLIENT_ID_KEY = 'cascade-oauth-client-id';

// ─── Connected mode login (capability token paste) ────────────────────────────

function ConnectedLoginPage() {
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
        {error !== null && <p class="error">{error}</p>}
        <button type="submit" disabled={loading}>
          {loading ? 'Verifying…' : 'Connect'}
        </button>
      </form>
    </div>
  );
}

// ─── WASM mode login (Google OAuth PKCE redirect flow) ────────────────────────

function WasmLoginPage() {
  const [clientId, setClientId] = useState<string>(
    () => localStorage.getItem(OAUTH_CLIENT_ID_KEY) ?? '',
  );
  const [loading, setLoading] = useState(false);
  const [signedIn, setSignedIn] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Handle the OAuth redirect-back on mount: exchange the code for tokens.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    if (!params.has('code')) return;

    setLoading(true);
    handleCallback()
      .then(() => {
        window.history.replaceState({}, '', window.location.pathname);
        setSignedIn(true);
        setLoading(false);
      })
      .catch((err: unknown) => {
        setError(err instanceof Error ? err.message : 'Sign-in failed');
        setLoading(false);
      });
  }, []);

  async function handleSignIn() {
    if (clientId.trim() === '') return;
    setLoading(true);
    setError(null);
    try {
      localStorage.setItem(OAUTH_CLIENT_ID_KEY, clientId.trim());
      await initiateAuth({
        clientId: clientId.trim(),
        redirectUri: window.location.origin + window.location.pathname,
        scopes: [...GDRIVE_SCOPES],
      });
      // Execution continues only if the redirect did not happen (error path).
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Sign-in failed');
      setLoading(false);
    }
  }

  function handleClearClientId() {
    setClientId('');
    localStorage.removeItem(OAUTH_CLIENT_ID_KEY);
  }

  if (loading) return <Spinner />;

  if (signedIn) {
    return (
      <div class="login-page">
        <h1>Cascade</h1>
        <p class="success-msg">Signed in with Google.</p>
        <a href="/">Go to dashboard</a>
      </div>
    );
  }

  return (
    <div class="login-page">
      <h1>Cascade</h1>
      <p>Sign in with your Google account to access Google Drive.</p>

      {clientId === '' && (
        <div class="oauth-client-id-form">
          <p class="muted">
            Enter your Google OAuth client ID (Desktop application type).
          </p>
          <input
            type="text"
            value={clientId}
            placeholder="123456789.apps.googleusercontent.com"
            onInput={(e) => setClientId((e.target as HTMLInputElement).value)}
          />
        </div>
      )}

      {error !== null && <p class="error">{error}</p>}

      <button
        type="button"
        disabled={loading || clientId.trim() === ''}
        onClick={handleSignIn}
      >
        Sign in with Google
      </button>

      {clientId !== '' && (
        <button type="button" class="link" onClick={handleClearClientId}>
          Use a different client ID
        </button>
      )}
    </div>
  );
}

// ─── Public export ────────────────────────────────────────────────────────────

export function LoginPage() {
  const { mode } = useContext(AppContext);

  if (mode !== RuntimeMode.Connected) {
    return <WasmLoginPage />;
  }
  return <ConnectedLoginPage />;
}
