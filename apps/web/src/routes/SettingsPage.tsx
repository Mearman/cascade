import { useState, useContext } from 'preact/hooks';
import { saveApiBase, clearToken, getApiBase, clearMode } from '@/auth';
import { ErrorBanner } from '@/components';
import { AppContext } from '@/context';
import { RuntimeMode } from '@/wasm';
import { requestDirectory, persistHandle } from '@/wasm/fsaccess';

interface Props {
  onBaseChanged?: (base: string) => void;
}

function modeLabel(mode: RuntimeMode): string {
  switch (mode) {
    case RuntimeMode.Standalone: return 'Standalone (WASM + local filesystem)';
    case RuntimeMode.BrowseOnly: return 'Browse-only (WASM)';
    case RuntimeMode.Connected: return 'Connected (daemon)';
  }
}

export function SettingsPage({ onBaseChanged }: Props) {
  const ctx = useContext(AppContext);
  const [url, setUrl] = useState(getApiBase);
  const [testing, setTesting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [changingDir, setChangingDir] = useState(false);

  async function handleSubmit(ev: Event) {
    ev.preventDefault();
    setError(null);
    setSaved(false);
    setTesting(true);

    const trimmed = url.trim().replace(/\/$/, '');
    try {
      const res = await fetch(`${trimmed}/v1/health`);
      if (!res.ok) throw new Error(`Daemon returned ${String(res.status)}`);
      saveApiBase(trimmed);
      setUrl(trimmed);
      setSaved(true);
      onBaseChanged?.(trimmed);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not reach the daemon at that address');
    } finally {
      setTesting(false);
    }
  }

  function handleSignOut() {
    clearToken();
    clearMode();
    window.location.reload();
  }

  async function handleChangeDir() {
    setChangingDir(true);
    try {
      const handle = await requestDirectory();
      await persistHandle('cascade-root', handle);
      ctx.setDirectoryName(handle.name);
    } catch {
      // user dismissed the picker; no action needed
    } finally {
      setChangingDir(false);
    }
  }

  return (
    <div class="settings-page">
      <h2>Settings</h2>

      <section>
        <h3>Runtime mode</h3>
        <dl class="info-list">
          <dt>Mode</dt>
          <dd>{modeLabel(ctx.mode)}</dd>
          {ctx.mode === RuntimeMode.Standalone && (
            <>
              <dt>Local folder</dt>
              <dd class="settings-dir-row">
                <span>{ctx.directoryName ?? 'None'}</span>
                <button
                  type="button"
                  class="link"
                  onClick={() => { void handleChangeDir(); }}
                  disabled={changingDir}
                >
                  {changingDir ? 'Choosing…' : 'Change'}
                </button>
              </dd>
            </>
          )}
          {ctx.mode === RuntimeMode.Connected && (
            <>
              <dt>Daemon URL</dt>
              <dd>{getApiBase() === '' ? 'Same origin' : getApiBase()}</dd>
            </>
          )}
        </dl>
      </section>

      <section>
        <h3>Browser capabilities</h3>
        <dl class="info-list">
          <dt>WebAssembly</dt>
          <dd>
            <span class={`badge ${ctx.capabilities.wasm ? 'ok' : 'muted'}`}>
              {ctx.capabilities.wasm ? 'Yes' : 'No'}
            </span>
          </dd>
          <dt>File System Access</dt>
          <dd>
            <span class={`badge ${ctx.capabilities.fileSystemAccess ? 'ok' : 'muted'}`}>
              {ctx.capabilities.fileSystemAccess ? 'Yes' : 'No'}
            </span>
          </dd>
          <dt>WebRTC</dt>
          <dd>
            <span class={`badge ${ctx.capabilities.webRtc ? 'ok' : 'muted'}`}>
              {ctx.capabilities.webRtc ? 'Yes' : 'No'}
            </span>
          </dd>
          <dt>Service Worker</dt>
          <dd>
            <span class={`badge ${ctx.capabilities.serviceWorker ? 'ok' : 'muted'}`}>
              {ctx.capabilities.serviceWorker ? 'Yes' : 'No'}
            </span>
          </dd>
          <dt>IndexedDB</dt>
          <dd>
            <span class={`badge ${ctx.capabilities.indexedDb ? 'ok' : 'muted'}`}>
              {ctx.capabilities.indexedDb ? 'Yes' : 'No'}
            </span>
          </dd>
        </dl>
      </section>

      {ctx.mode === RuntimeMode.Connected && (
        <section>
          <h3>Daemon connection</h3>
          <p class="muted">
            {getApiBase() === ''
              ? 'Using same-origin (daemon serves the PWA directly).'
              : `Connected to ${getApiBase()}`}
          </p>

          <form class="settings-form" onSubmit={(ev) => { void handleSubmit(ev); }}>
            <label>
              Daemon address
              <input
                type="url"
                value={url}
                placeholder="http://192.168.1.100:7842"
                onInput={(e) => {
                  const target = e.target;
                  if (target instanceof HTMLInputElement) {
                    setUrl(target.value);
                  }
                  setSaved(false);
                }}
              />
            </label>
            {error !== null && <ErrorBanner message={error} onDismiss={() => { setError(null); }} />}
            {saved && <p class="success-msg">Saved — connection verified.</p>}
            <div class="settings-actions">
              <button type="submit" disabled={testing}>
                {testing ? 'Testing…' : 'Save and test'}
              </button>
            </div>
          </form>
        </section>
      )}

      <section>
        <h3>Account</h3>
        <p class="muted">
          {ctx.mode === RuntimeMode.Connected
            ? 'Sign out to clear your capability token and return to setup.'
            : 'Reset to return to mode selection.'}
        </p>
        <div class="settings-actions">
          <button type="button" class="secondary" onClick={handleSignOut}>
            {ctx.mode === RuntimeMode.Connected ? 'Sign out' : 'Reset mode'}
          </button>
        </div>
      </section>
    </div>
  );
}
