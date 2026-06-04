import { useState } from 'preact/hooks';
import { api } from '@/api/client';
import { saveApiBase, clearToken, getApiBase } from '@/auth';
import { ErrorBanner } from '@/components';

interface Props {
  onBaseChanged?: (base: string) => void;
}

export function SettingsPage({ onBaseChanged }: Props) {
  const [url, setUrl] = useState(getApiBase);
  const [testing, setTesting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  async function handleSubmit(ev: Event) {
    ev.preventDefault();
    setError(null);
    setSaved(false);
    setTesting(true);

    const trimmed = url.trim().replace(/\/$/, '');
    try {
      const res = await fetch(`${trimmed}/v1/health`);
      if (!res.ok) throw new Error(`Daemon returned ${res.status}`);
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

  function handleDisconnect() {
    clearToken();
    window.location.reload();
  }

  const currentBase = getApiBase();

  return (
    <div class="settings-page">
      <h2>Settings</h2>

      <section>
        <h3>Daemon connection</h3>
        <p class="muted">
          {currentBase === ''
            ? 'Using same-origin (daemon serves the PWA directly).'
            : `Connected to ${currentBase}`}
        </p>

        <form class="settings-form" onSubmit={handleSubmit}>
          <label>
            Daemon address
            <input
              type="url"
              value={url}
              placeholder="http://192.168.1.100:7842"
              onInput={(e) => {
                setUrl((e.target as HTMLInputElement).value);
                setSaved(false);
              }}
            />
          </label>
          {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
          {saved && <p class="success-msg">Saved — connection verified.</p>}
          <div class="settings-actions">
            <button type="submit" disabled={testing}>
              {testing ? 'Testing…' : 'Save and test'}
            </button>
            <button type="button" class="secondary" onClick={handleDisconnect}>
              Sign out
            </button>
          </div>
        </form>
      </section>
    </div>
  );
}
