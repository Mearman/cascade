import { useEffect, useState } from 'preact/hooks';
import { api } from '@/api';
import type { StatusResponse } from '@/api/types';
import { ErrorBanner, Spinner } from '@/components';

export function DashboardPage() {
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api
      .status()
      .then((s) => setStatus(s))
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  if (loading) return <Spinner />;
  if (error) return <ErrorBanner message={error} />;
  if (!status) return null;

  const { version, uptimeSeconds, backends, cache, peers } = status;

  const hours = Math.floor(uptimeSeconds / 3600);
  const minutes = Math.floor((uptimeSeconds % 3600) / 60);

  return (
    <div class="dashboard">
      <h2>Status</h2>
      <p>Version {version} — up {hours}h {minutes}m</p>

      <section>
        <h3>Backends</h3>
        {backends.length === 0 ? (
          <p class="muted">No backends configured.</p>
        ) : (
          <ul>
            {backends.map((b) => (
              <li key={b.id}>
                <strong>{b.displayName}</strong> ({b.mountPath}){' '}
                {b.healthy ? (
                  <span class="badge ok">healthy</span>
                ) : (
                  <span class="badge danger">unhealthy</span>
                )}
                {b.quota && (
                  <span class="quota">
                    {' '}
                    {formatBytes(b.quota.usedBytes ?? 0)} /{' '}
                    {b.quota.totalBytes ? formatBytes(b.quota.totalBytes) : '?'}
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>

      <section>
        <h3>Cache</h3>
        <p>
          {formatBytes(cache.usedBytes)} used of {formatBytes(cache.totalBytes)}
        </p>
        <p>
          {cache.pinnedFiles} pinned, {cache.cachedFiles} cached
        </p>
      </section>

      <section>
        <h3>Peers</h3>
        {peers.length === 0 ? (
          <p class="muted">No known peers.</p>
        ) : (
          <ul>
            {peers.map((p) => (
              <li key={p.deviceId}>
                {p.name} ({p.deviceId}){' '}
                {p.online ? (
                  <span class="badge ok">online</span>
                ) : (
                  <span class="badge muted">offline</span>
                )}
                {p.folders.length > 0 && (
                  <span class="folders"> {p.folders.join(', ')}</span>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i] ?? 'B'}`;
}
