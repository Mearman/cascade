import { useState } from 'preact/hooks';
import { Route, Router, Switch } from 'wouter-preact';

// Vite sets BASE_URL to the configured `base` (e.g. "/cascade/").
// Strip the trailing slash for wouter's `base` prop.
const ROUTER_BASE = import.meta.env.BASE_URL.replace(/\/$/, '');
import { AppShell } from '@/components';
import {
  DashboardPage,
  FilesPage,
  GrantsPage,
  LoginPage,
  SharesPage,
  SettingsPage,
  TokensPage,
} from '@/routes';
import { loadToken, hasApiBase, saveApiBase } from '@/auth';
import { ErrorBanner } from '@/components';

function ConnectionSetup({ onConnected }: { onConnected: () => void }) {
  const [url, setUrl] = useState('');
  const [testing, setTesting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(ev: Event) {
    ev.preventDefault();
    setError(null);
    setTesting(true);
    const trimmed = url.trim().replace(/\/$/, '');
    try {
      const res = await fetch(`${trimmed}/v1/health`);
      if (!res.ok) throw new Error(`Daemon returned ${res.status}`);
      saveApiBase(trimmed);
      onConnected();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not reach the daemon at that address');
    } finally {
      setTesting(false);
    }
  }

  function useSameOrigin() {
    saveApiBase('');
    onConnected();
  }

  return (
    <div class="setup-page">
      <h1>Connect to Cascade</h1>
      <p>Enter the address of your running Cascade daemon.</p>
      <form onSubmit={handleSubmit}>
        <input
          type="url"
          value={url}
          placeholder="http://192.168.1.100:7842"
          onInput={(e) => setUrl((e.target as HTMLInputElement).value)}
          disabled={testing}
        />
        {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
        <div class="setup-actions">
          <button type="submit" disabled={testing || url.trim() === ''}>
            {testing ? 'Connecting…' : 'Connect'}
          </button>
          <button type="button" class="secondary" onClick={useSameOrigin}>
            Same origin
          </button>
        </div>
      </form>
      <p class="muted">
        Use <em>Same origin</em> when the daemon is serving this PWA directly.
      </p>
    </div>
  );
}

function RequireAuth({ children }: { children: preact.ComponentChildren }) {
  const token = loadToken();
  if (!token) return <LoginPage />;
  return <>{children}</>;
}

export function App() {
  const [connected, setConnected] = useState(hasApiBase);

  if (!connected) {
    return <ConnectionSetup onConnected={() => setConnected(true)} />;
  }

  return (
    <AppShell>
      <Router base={ROUTER_BASE}>
        <Switch>
          <Route path="/login" component={LoginPage} />
          <Route path="/settings" component={() => <SettingsPage />} />
          <Route path="/" component={() => (
            <RequireAuth><DashboardPage /></RequireAuth>
          )} />
          <Route path="/files" component={() => (
            <RequireAuth><FilesPage /></RequireAuth>
          )} />
          <Route path="/grants" component={() => (
            <RequireAuth><GrantsPage /></RequireAuth>
          )} />
          <Route path="/shares" component={() => (
            <RequireAuth><SharesPage /></RequireAuth>
          )} />
          <Route path="/tokens" component={() => (
            <RequireAuth><TokensPage /></RequireAuth>
          )} />
          <Route>
            <p class="muted">Page not found.</p>
          </Route>
        </Switch>
      </Router>
    </AppShell>
  );
}
