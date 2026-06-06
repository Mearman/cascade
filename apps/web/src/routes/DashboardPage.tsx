import { useEffect, useState, useContext, useRef } from 'preact/hooks';
import { api } from '@/api/client';
import type { HealthResponse, SessionResponse } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';
import { AppContext } from '@/context';
import { RuntimeMode } from '@/wasm';

// ─── Connected mode dashboard ─────────────────────────────────────────────────

function ConnectedDashboard() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [session, setSession] = useState<SessionResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    Promise.all([api.health(), api.session()])
      .then(([h, s]) => {
        setHealth(h);
        setSession(s);
      })
      .catch((err: unknown) => { setError(err instanceof Error ? err.message : String(err)); })
      .finally(() => { setLoading(false); });
  }, []);

  if (loading) return <Spinner />;
  if (error !== null) return <ErrorBanner message={error} />;
  if (health === null || session === null) return null;

  const { abilities } = session;

  return (
    <div class="dashboard">
      <h2>Dashboard</h2>

      <section class="dashboard-section">
        <h3>Node</h3>
        <dl class="info-list">
          <dt>Version</dt>
          <dd>{health.version}</dd>
          <dt>Device ID</dt>
          <dd><code>{health.node_device_id}</code></dd>
          <dt>Session class</dt>
          <dd>
            <span class={`badge badge-class badge-${session.session.class}`}>
              {session.session.class}
            </span>
          </dd>
        </dl>
      </section>

      <section class="dashboard-section">
        <h3>Abilities</h3>
        <ul class="abilities-list">
          {abilities.status_read && <li><span class="badge ok">status:read</span></li>}
          {abilities.pin_write && <li><span class="badge ok">pin:write</span></li>}
          {abilities.cache_manage && <li><span class="badge ok">cache:manage</span></li>}
          {abilities.config_push && <li><span class="badge ok">config:push</span></li>}
          {abilities.policy_set && <li><span class="badge ok">policy:set</span></li>}
          {abilities.backend_manage && <li><span class="badge ok">backend:manage</span></li>}
          {abilities.lifecycle_control && <li><span class="badge ok">lifecycle:control</span></li>}
          {abilities.grant_admin && <li><span class="badge ok">grant:admin</span></li>}
        </ul>

        {abilities.data_read.length > 0 && (
          <div class="data-abilities">
            <p><span class="badge ok">data:read</span> folders:</p>
            <ul>
              {abilities.data_read.map((f) => <li key={f}><code>{f}</code></li>)}
            </ul>
          </div>
        )}

        {abilities.data_write.length > 0 && (
          <div class="data-abilities">
            <p><span class="badge ok">data:write</span> folders:</p>
            <ul>
              {abilities.data_write.map((f) => <li key={f}><code>{f}</code></li>)}
            </ul>
          </div>
        )}

        {!abilities.status_read &&
          abilities.data_read.length === 0 &&
          abilities.data_write.length === 0 && (
          <p class="muted">No abilities granted.</p>
        )}
      </section>
    </div>
  );
}

// ─── WASM mode dashboard ──────────────────────────────────────────────────────

interface WasmHealth { version: string; status: string }
interface WasmBackend { id: string; type: string; display_name: string; hasHandle: boolean }
interface WasmCapabilities { backends: boolean; config: boolean; files: boolean; sync: boolean }

function isWasmHealth(value: unknown): value is WasmHealth {
  if (typeof value !== 'object' || value === null) return false;
  return 'version' in value && typeof value.version === 'string'
    && 'status' in value && typeof value.status === 'string';
}

function isWasmBackend(value: unknown): value is WasmBackend {
  if (typeof value !== 'object' || value === null) return false;
  return 'id' in value && typeof value.id === 'string'
    && 'type' in value && typeof value.type === 'string'
    && 'display_name' in value && typeof value.display_name === 'string'
    && 'hasHandle' in value && typeof value.hasHandle === 'boolean';
}

function isWasmCapabilities(value: unknown): value is WasmCapabilities {
  if (typeof value !== 'object' || value === null) return false;
  return 'backends' in value && typeof value.backends === 'boolean'
    && 'config' in value && typeof value.config === 'boolean'
    && 'files' in value && typeof value.files === 'boolean'
    && 'sync' in value && typeof value.sync === 'boolean';
}

function WasmDashboard({ mode }: { mode: RuntimeMode }) {
  const [ready, setReady] = useState<boolean | null>(null);
  const [health, setHealth] = useState<WasmHealth | null>(null);
  const [backends, setBackends] = useState<WasmBackend[]>([]);
  const [capabilities, setCapabilities] = useState<WasmCapabilities | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const cancelled = useRef<boolean>(false);

    async function load() {
      try {
        const isReady = await api.wasmReady();
        if (cancelled.current) return;
        setReady(isReady);
        if (!isReady) return;

        // Query the WASM engine for health, backends, and capabilities.
        // The WASM engine returns different response shapes than the daemon
        // API, so we use rawRequest and validate locally.
        const [healthRes, backendsRes, capsRes] = await Promise.allSettled([
          api.rawRequest('GET', '/health'),
          api.rawRequest('GET', '/backends'),
          api.rawRequest('GET', '/capabilities'),
        ]);

        if (healthRes.status === 'fulfilled' && healthRes.value.status === 200) {
          if (isWasmHealth(healthRes.value.body)) {
            setHealth(healthRes.value.body);
          }
        }
        if (backendsRes.status === 'fulfilled' && backendsRes.value.status === 200) {
          const body = backendsRes.value.body;
          if (typeof body === 'object' && body !== null && 'backends' in body && Array.isArray(body.backends) && body.backends.every(isWasmBackend)) {
            setBackends(body.backends);
          }
        }
        if (capsRes.status === 'fulfilled' && capsRes.value.status === 200) {
          if (isWasmCapabilities(capsRes.value.body)) {
            setCapabilities(capsRes.value.body);
          }
        }
      } catch (err) {
        if (!cancelled.current) setError(err instanceof Error ? err.message : String(err));
      }
    }

    void load();
    return () => { cancelled.current = true; };
  }, []);

  return (
    <div class="dashboard">
      <h2>Dashboard</h2>

      {error !== null && <ErrorBanner message={error} onDismiss={() => { setError(null); }} />}

      <section class="dashboard-section">
        <h3>WASM Engine</h3>
        <dl class="info-list">
          <dt>Status</dt>
          <dd>
            {ready === null
              ? <Spinner />
              : <span class={`badge ${ready ? 'ok' : 'danger'}`}>{ready ? 'Ready' : 'Error'}</span>
            }
          </dd>
          <dt>Mode</dt>
          <dd>
            {mode === RuntimeMode.Standalone
              ? 'Standalone (local filesystem + cloud)'
              : 'Browse-only (cloud access)'}
          </dd>
          {health !== null && (
            <>
              <dt>Version</dt>
              <dd>{health.version}</dd>
              <dt>Engine status</dt>
              <dd><span class={`badge ${health.status === 'ok' ? 'ok' : 'danger'}`}>{health.status}</span></dd>
            </>
          )}
        </dl>
      </section>

      {capabilities !== null && (
        <section class="dashboard-section">
          <h3>Capabilities</h3>
          <ul class="abilities-list">
            {capabilities.backends && <li><span class="badge ok">Backends</span></li>}
            {capabilities.config && <li><span class="badge ok">Config parsing</span></li>}
            {capabilities.files && <li><span class="badge ok">File browsing</span></li>}
            {capabilities.sync && <li><span class="badge ok">Sync</span></li>}
          </ul>
          {!capabilities.files && (
            <p class="muted">File browsing returns 501 from the WASM engine in this build.</p>
          )}
        </section>
      )}

      <section class="dashboard-section">
        <h3>Backends</h3>
        {backends.length > 0
          ? (
            <ul class="abilities-list">
              {backends.map((b) => (
                <li key={b.id}>
                  <code>{b.id}</code> ({b.type})
                  {b.hasHandle && <span class="badge ok">connected</span>}
                </li>
              ))}
            </ul>
          )
          : (
            <p class="muted">
              No backends connected yet.{' '}
              {mode === RuntimeMode.Standalone
                ? 'Choose a local folder or sign in with a cloud provider to add a backend.'
                : 'Sign in with a cloud provider on the login page to add a backend.'}
            </p>
          )
        }
      </section>

      <section class="dashboard-section">
        <h3>Sync</h3>
        <p class="muted">Sync state will appear here once a backend is connected.</p>
      </section>
    </div>
  );
}

// ─── Public export ────────────────────────────────────────────────────────────

export function DashboardPage() {
  const { mode } = useContext(AppContext);

  if (mode !== RuntimeMode.Connected) {
    return <WasmDashboard mode={mode} />;
  }
  return <ConnectedDashboard />;
}
