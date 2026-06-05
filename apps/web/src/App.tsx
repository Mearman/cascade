import type { ComponentChildren } from 'preact';
import { useState, useEffect } from 'preact/hooks';
import { Route, Router, Switch } from 'wouter-preact';

const ROUTER_BASE = import.meta.env.BASE_URL.replace(/\/$/, '');

import { AppShell, ErrorBanner } from '@/components';
import {
  DashboardPage,
  FilesPage,
  GrantsPage,
  LoginPage,
  SharesPage,
  SettingsPage,
  TokensPage,
} from '@/routes';
import { loadToken, hasApiBase, saveApiBase, getStoredMode, saveMode } from '@/auth';
import { detectCapabilities, recommendMode, RuntimeMode, type Capabilities } from '@/wasm';
import { requestDirectory, persistHandle, restoreHandle } from '@/wasm/fsaccess';
import { AppContext, type AppContextValue } from '@/context';
import { api } from '@/api/client';

function ConnectionSetup({ onConnected }: {
  onConnected: (mode: RuntimeMode, dirName?: string) => void;
}) {
  const [capabilities] = useState<Capabilities>(() => detectCapabilities());
  const [selectedMode, setSelectedMode] = useState<RuntimeMode>(
    () => recommendMode(detectCapabilities(), false),
  );
  const [url, setUrl] = useState('');
  const [testing, setTesting] = useState(false);
  const [pickingDir, setPickingDir] = useState(false);
  const [dirName, setDirName] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  // Holds the FileSystemDirectoryHandle chosen by the user. Kept outside
  // dirName (which is just the display name) so we can pass it to
  // registerBackend on continue.
  const [directoryHandle, setDirectoryHandle] = useState<FileSystemDirectoryHandle | null>(null);

  const standaloneAvailable = capabilities.wasm && capabilities.fileSystemAccess;
  const browseOnlyAvailable = capabilities.wasm;

  async function handleChooseFolder() {
    setPickingDir(true);
    setError(null);
    try {
      const handle = await requestDirectory();
      await persistHandle('cascade-root', handle);
      setDirName(handle.name);
      setDirectoryHandle(handle);
    } catch (err) {
      if (err instanceof Error && err.name !== 'AbortError') {
        setError(err.message);
      }
    } finally {
      setPickingDir(false);
    }
  }

  async function handleConnect(ev: Event) {
    ev.preventDefault();
    setError(null);
    setTesting(true);
    try {
      const trimmed = url.trim().replace(/\/$/, '');
      const res = await fetch(`${trimmed}/v1/health`);
      if (!res.ok) throw new Error(`Daemon returned ${res.status}`);
      saveApiBase(trimmed);
      onConnected(RuntimeMode.Connected);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not reach the daemon at that address');
    } finally {
      setTesting(false);
    }
  }

  function handleSameOrigin() {
    saveApiBase('');
    onConnected(RuntimeMode.Connected);
  }

  async function handleContinue() {
    setError(null);
    setStarting(true);
    try {
      if (selectedMode === RuntimeMode.BrowseOnly) {
        onConnected(RuntimeMode.BrowseOnly);
      } else if (selectedMode === RuntimeMode.Standalone) {
        // Set mode first so the bridge client is created and the worker starts.
        api.setMode(RuntimeMode.Standalone);
        const ready = await api.wasmReady();
        if (!ready) {
          throw new Error('WASM engine failed to initialise');
        }
        if (directoryHandle !== null) {
          await api.registerBackend('local', 'local-fs', directoryHandle);
        }
        onConnected(RuntimeMode.Standalone, dirName ?? undefined);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setStarting(false);
    }
  }

  return (
    <div class="setup-page">
      <h1>Cascade</h1>
      <p class="muted">Choose how to use Cascade.</p>

      <div class="mode-cards">
        <button
          type="button"
          class={`mode-card${selectedMode === RuntimeMode.Standalone ? ' selected' : ''}`}
          onClick={() => standaloneAvailable && setSelectedMode(RuntimeMode.Standalone)}
          disabled={!standaloneAvailable}
        >
          <div class="mode-card-header">
            <span class="mode-card-title">Standalone</span>
          </div>
          <p class="mode-card-desc">Files sync to a local folder on your computer</p>
          <div class="mode-card-caps">
            <span class={`badge ${capabilities.wasm ? 'ok' : 'danger'}`}>WebAssembly</span>
            <span class={`badge ${capabilities.fileSystemAccess ? 'ok' : 'danger'}`}>
              File System Access
            </span>
          </div>
        </button>

        <button
          type="button"
          class={`mode-card${selectedMode === RuntimeMode.BrowseOnly ? ' selected' : ''}`}
          onClick={() => browseOnlyAvailable && setSelectedMode(RuntimeMode.BrowseOnly)}
          disabled={!browseOnlyAvailable}
        >
          <div class="mode-card-header">
            <span class="mode-card-title">Browse-only</span>
          </div>
          <p class="mode-card-desc">Browse and manage your cloud files in the browser</p>
          <div class="mode-card-caps">
            <span class={`badge ${capabilities.wasm ? 'ok' : 'danger'}`}>WebAssembly</span>
          </div>
        </button>

        <button
          type="button"
          class={`mode-card${selectedMode === RuntimeMode.Connected ? ' selected' : ''}`}
          onClick={() => setSelectedMode(RuntimeMode.Connected)}
        >
          <div class="mode-card-header">
            <span class="mode-card-title">Connected</span>
          </div>
          <p class="mode-card-desc">Connect to a running Cascade daemon</p>
          <div class="mode-card-caps">
            <span class="badge ok">Always available</span>
          </div>
        </button>
      </div>

      {selectedMode === RuntimeMode.Connected && (
        <form class="setup-url-form" onSubmit={handleConnect}>
          <input
            type="url"
            value={url}
            placeholder="http://192.168.1.100:7842"
            onInput={(e) => setUrl((e.target as HTMLInputElement).value)}
            disabled={testing}
          />
          {error !== null && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
          <div class="setup-actions">
            <button type="submit" disabled={testing || url.trim() === ''}>
              {testing ? 'Connecting…' : 'Connect'}
            </button>
            <button type="button" class="secondary" onClick={handleSameOrigin}>
              Same origin
            </button>
          </div>
        </form>
      )}

      {selectedMode === RuntimeMode.Standalone && (
        <div class="setup-dir">
          {dirName !== null
            ? <p>Selected folder: <strong>{dirName}</strong></p>
            : <p class="muted">Optionally choose a local folder to sync files to your computer.</p>
          }
          {error !== null && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
          <div class="setup-actions">
            <button type="button" class="secondary" onClick={handleChooseFolder} disabled={pickingDir}>
              {pickingDir ? 'Choosing…' : dirName !== null ? 'Choose a different folder' : 'Choose folder'}
            </button>
            <button type="button" onClick={handleContinue} disabled={starting}>
              {starting ? 'Starting…' : dirName !== null ? 'Continue' : 'Continue without folder'}
            </button>
          </div>
        </div>
      )}

      {selectedMode === RuntimeMode.BrowseOnly && (
        <div class="setup-actions setup-actions-top">
          {error !== null && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
          <button type="button" onClick={handleContinue} disabled={starting}>
            {starting ? 'Starting…' : 'Continue'}
          </button>
        </div>
      )}
    </div>
  );
}

function RequireAuth({ children }: { children: ComponentChildren }) {
  // WASM modes do not use capability tokens; access is gated by the WASM engine.
  // Render children directly and let the WASM dashboard handle auth state.
  const storedMode = getStoredMode();
  if (storedMode !== null && storedMode !== RuntimeMode.Connected) {
    return <>{children}</>;
  }
  const token = loadToken();
  if (!token) return <LoginPage />;
  return <>{children}</>;
}

function isSetup(): boolean {
  // Mode explicitly chosen → setup is complete.
  const { MODE_STORAGE_KEY } = { MODE_STORAGE_KEY: 'cascade-runtime-mode' };
  if (localStorage.getItem(MODE_STORAGE_KEY) !== null) return true;
  // Legacy: API base configured before mode selection was introduced.
  return hasApiBase();
}

export function App() {
  const [appMode, setAppMode] = useState<RuntimeMode>(() => {
    const stored = getStoredMode();
    const m = stored ?? RuntimeMode.Connected;
    // Wire the API singleton before any child component renders.
    api.setMode(m);
    return m;
  });
  const [capabilities] = useState<Capabilities>(() => detectCapabilities());
  const [directoryName, setDirectoryName] = useState<string | null>(null);
  const [connected, setConnected] = useState<boolean>(isSetup);

  // Restore persisted directory handle name for Standalone mode.
  useEffect(() => {
    if (appMode === RuntimeMode.Standalone) {
      restoreHandle('cascade-root')
        .then((handle) => {
          if (handle !== null) setDirectoryName(handle.name);
        })
        .catch(() => { /* no stored handle */ });
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function handleConnected(mode: RuntimeMode, dirName?: string) {
    saveMode(mode);
    api.setMode(mode);
    setAppMode(mode);
    setDirectoryName(dirName !== undefined ? dirName : null);
    setConnected(true);
  }

  if (!connected) {
    return <ConnectionSetup onConnected={handleConnected} />;
  }

  const ctxValue: AppContextValue = {
    mode: appMode,
    capabilities,
    directoryName,
    setDirectoryName,
  };

  return (
    <AppContext.Provider value={ctxValue}>
      <Router base={ROUTER_BASE}>
        <AppShell>
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
        </AppShell>
      </Router>
    </AppContext.Provider>
  );
}
