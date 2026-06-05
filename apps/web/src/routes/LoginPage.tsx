import { useState, useEffect, useRef, useContext } from 'preact/hooks';
import { validateToken, isCapabilityToken, saveToken } from '@/auth';
import { ApiError } from '@/api/types';
import { api } from '@/api/client';
import { AppContext } from '@/context';
import { RuntimeMode } from '@/wasm';
import { initiateAuth, handleCallback, GDRIVE_SCOPES } from '@/wasm/oauth';
import { Spinner } from '@/components';

const OAUTH_CLIENT_ID_KEY = 'cascade-oauth-client-id';

type AuthTab = 'pair' | 'secret' | 'device';

// ─── Connected mode login (tabbed auth) ───────────────────────────────────────

function ConnectedLoginPage() {
  const [tab, setTab] = useState<AuthTab>('pair');
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  // Pairing code state
  const [pairCode, setPairCode] = useState('');

  // Shared secret state
  const [secret, setSecret] = useState('');

  // Device code state
  const [deviceCode, setDeviceCode] = useState<string | null>(null);
  const [expiresIn, setExpiresIn] = useState(0);
  const [polling, setPolling] = useState(false);
  const pollTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const countdownTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const [countdown, setCountdown] = useState(0);

  // Clean up timers on unmount or tab switch.
  useEffect(() => {
    return () => {
      if (pollTimer.current !== null) clearInterval(pollTimer.current);
      if (countdownTimer.current !== null) clearInterval(countdownTimer.current);
    };
  }, []);

  // Stop polling when switching tabs.
  useEffect(() => {
    if (pollTimer.current !== null) clearInterval(pollTimer.current);
    if (countdownTimer.current !== null) clearInterval(countdownTimer.current);
    setDeviceCode(null);
    setPolling(false);
    setError(null);
  }, [tab]);

  async function handlePair(ev: Event) {
    ev.preventDefault();
    setError(null);
    setLoading(true);
    try {
      const token = await api.authPair(pairCode.trim());
      saveToken(token);
      window.location.reload();
    } catch (err) {
      setError(err instanceof ApiError ? `${err.code}: ${err.message}` : err instanceof Error ? err.message : 'Invalid code');
    } finally {
      setLoading(false);
    }
  }

  async function handleSecret(ev: Event) {
    ev.preventDefault();
    setError(null);
    setLoading(true);
    try {
      const token = await api.authSecret(secret.trim());
      saveToken(token);
      window.location.reload();
    } catch (err) {
      setError(err instanceof ApiError ? `${err.code}: ${err.message}` : err instanceof Error ? err.message : 'Invalid secret');
    } finally {
      setLoading(false);
    }
  }

  async function handleDeviceRequest() {
    setError(null);
    setLoading(true);
    try {
      const result = await api.authDeviceRequest();
      setDeviceCode(result.code);
      setExpiresIn(result.expires_in);
      setCountdown(result.expires_in);
      setPolling(true);

      // Start polling every 3 seconds.
      pollTimer.current = setInterval(async () => {
        try {
          const poll = await api.authDevicePoll(result.code);
          if (poll.status === 'authorised' && poll.token) {
            if (pollTimer.current !== null) clearInterval(pollTimer.current);
            if (countdownTimer.current !== null) clearInterval(countdownTimer.current);
            setPolling(false);
            saveToken(poll.token);
            window.location.reload();
          }
        } catch {
          // Poll errors are transient — keep trying.
        }
      }, 3000);

      // Countdown timer.
      countdownTimer.current = setInterval(() => {
        setCountdown((prev) => {
          if (prev <= 1) {
            if (pollTimer.current !== null) clearInterval(pollTimer.current);
            if (countdownTimer.current !== null) clearInterval(countdownTimer.current);
            setPolling(false);
            setError('Code expired — generate a new one.');
            return 0;
          }
          return prev - 1;
        });
      }, 1000);
    } catch (err) {
      setError(err instanceof ApiError ? `${err.code}: ${err.message}` : err instanceof Error ? err.message : 'Could not request device code');
    } finally {
      setLoading(false);
    }
  }

  const tabs: { id: AuthTab; label: string }[] = [
    { id: 'pair', label: 'Pairing code' },
    { id: 'secret', label: 'Shared secret' },
    { id: 'device', label: 'Device code' },
  ];

  return (
    <div class="login-page">
      <h1>Cascade</h1>
      <p>Authenticate with the daemon to continue.</p>

      <div class="auth-tabs">
        {tabs.map(({ id, label }) => (
          <button
            key={id}
            type="button"
            class={`auth-tab${tab === id ? ' active' : ''}`}
            onClick={() => setTab(id)}
          >
            {label}
          </button>
        ))}
      </div>

      {tab === 'pair' && (
        <form onSubmit={handlePair}>
          <p class="muted">Run <code>cascade auth pair</code> on the daemon host, then enter the code below.</p>
          <input
            type="text"
            value={pairCode}
            placeholder="ABC12345"
            onInput={(e) => setPairCode((e.target as HTMLInputElement).value)}
            disabled={loading}
            autocomplete="off"
          />
          {error !== null && <p class="error">{error}</p>}
          <button type="submit" disabled={loading || pairCode.trim() === ''}>
            {loading ? 'Connecting…' : 'Connect'}
          </button>
        </form>
      )}

      {tab === 'secret' && (
        <form onSubmit={handleSecret}>
          <p class="muted">Run <code>cascade auth secret</code> on the daemon host to get the secret.</p>
          <input
            type="password"
            value={secret}
            placeholder="Daemon secret"
            onInput={(e) => setSecret((e.target as HTMLInputElement).value)}
            disabled={loading}
            autocomplete="off"
          />
          {error !== null && <p class="error">{error}</p>}
          <button type="submit" disabled={loading || secret.trim() === ''}>
            {loading ? 'Connecting…' : 'Connect'}
          </button>
        </form>
      )}

      {tab === 'device' && (
        <div>
          {deviceCode === null ? (
            <>
              <p class="muted">Generate a code and enter it on the daemon host.</p>
              {error !== null && <p class="error">{error}</p>}
              <button type="button" onClick={handleDeviceRequest} disabled={loading}>
                {loading ? 'Generating…' : 'Generate code'}
              </button>
            </>
          ) : (
            <>
              <p class="muted">Run <code>{`cascade auth authorize ${deviceCode}`}</code> on the daemon host.</p>
              <div class="device-code-display">{deviceCode}</div>
              <p class="muted">
                {polling
                  ? `Waiting for authorisation… ${Math.floor(countdown / 60)}:${String(countdown % 60).padStart(2, '0')} remaining`
                  : error !== null ? error : 'Connecting…'}
              </p>
              {!polling && (
                <button type="button" onClick={handleDeviceRequest}>
                  Generate new code
                </button>
              )}
            </>
          )}
        </div>
      )}

      <AdvancedTokenPaste />
    </div>
  );
}

// ─── Advanced: raw JSON paste fallback ─────────────────────────────────────────

function AdvancedTokenPaste() {
  const [open, setOpen] = useState(false);
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
        throw new Error('Token must be a CapabilityToken JSON object.');
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
    <div class="advanced-section">
      <button type="button" class="link" onClick={() => setOpen(!open)}>
        {open ? 'Hide' : 'Paste capability token'}
      </button>
      {open && (
        <form onSubmit={handleSubmit}>
          <textarea
            rows={6}
            placeholder={'{\n  "token_id": "…",\n  "bearer": "…",\n  …\n}'}
            value={tokenJson}
            onInput={(e) => setTokenJson((e.target as HTMLTextAreaElement).value)}
            disabled={loading}
          />
          {error !== null && <p class="error">{error}</p>}
          <button type="submit" disabled={loading || tokenJson.trim() === ''}>
            {loading ? 'Verifying…' : 'Connect'}
          </button>
        </form>
      )}
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
