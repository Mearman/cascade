import type {
  AuthToken,
  StatusResponse,
  FileEntry,
  GrantEntry,
  ShareEntry,
  TokenEntry,
  CacheState,
} from './types';

const BASE = import.meta.env['VITE_API_BASE'] ?? '/api';

function isApiError(body: unknown): body is { error: string; detail?: string } {
  return typeof body === 'object' && body !== null && 'error' in body;
}

class ApiClient {
  private token: AuthToken | null = null;

  setToken(token: AuthToken | null): void {
    this.token = token;
  }

  getToken(): AuthToken | null {
    return this.token;
  }

  private async request<T>(
    method: 'GET' | 'POST' | 'PUT' | 'DELETE',
    path: string,
    body?: unknown,
  ): Promise<T> {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
    };

    if (this.token) {
      headers['Authorization'] = `Bearer ${btoa(JSON.stringify(this.token))}`;
    }

    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      init['body'] = JSON.stringify(body);
    }
    const res = await fetch(`${BASE}${path}`, init);

    if (res.status === 401) {
      // Token missing, invalid, or expired — clear and let the UI handle redirect.
      this.token = null;
      throw new Error('Unauthorised');
    }

    if (!res.ok) {
      const text = await res.text().catch(() => '');
      let message = `HTTP ${res.status}`;
      try {
        const parsed = JSON.parse(text);
        if (isApiError(parsed)) {
          message = parsed.detail ?? parsed.error;
        }
      } catch {
        // Use status text if JSON parsing fails.
      }
      throw new Error(message);
    }

    if (res.status === 204) {
      return undefined as T;
    }

    return res.json() as Promise<T>;
  }

  // ─── Status ────────────────────────────────────────────────────────────────

  status(): Promise<StatusResponse> {
    return this.request('GET', '/status');
  }

  // ─── Files ─────────────────────────────────────────────────────────────────

  listFolder(parentId: string | null): Promise<FileEntry[]> {
    const q = parentId !== null ? `?parent=${encodeURIComponent(parentId)}` : '';
    return this.request('GET', `/files${q}`);
  }

  file(id: string): Promise<FileEntry> {
    return this.request('GET', `/files/${encodeURIComponent(id)}`);
  }

  // ─── Pinning ───────────────────────────────────────────────────────────────

  pin(id: string): Promise<void> {
    return this.request('POST', `/files/${encodeURIComponent(id)}/pin`);
  }

  unpin(id: string): Promise<void> {
    return this.request('POST', `/files/${encodeURIComponent(id)}/unpin`);
  }

  // ─── Cache ─────────────────────────────────────────────────────────────────

  cacheWarm(path: string): Promise<void> {
    return this.request('POST', '/cache/warm', { path });
  }

  cacheEvict(path?: string): Promise<void> {
    const q = path !== undefined ? `?path=${encodeURIComponent(path)}` : '';
    return this.request('POST', `/cache/evict${q}`);
  }

  // ─── Grants / Sharing ─────────────────────────────────────────────────────

  grants(): Promise<GrantEntry[]> {
    return this.request('GET', '/grants');
  }

  addGrant(grantee: string, capability: string, scope: string, expiresAt?: string): Promise<GrantEntry> {
    return this.request('POST', '/grants', { grantee, capability, scope, expiresAt });
  }

  revokeGrant(id: number): Promise<void> {
    return this.request('DELETE', `/grants/${id}`);
  }

  shares(folder?: string): Promise<ShareEntry[]> {
    const q = folder !== undefined ? `?folder=${encodeURIComponent(folder)}` : '';
    return this.request('GET', `/shares${q}`);
  }

  addShare(peerId: string, folder: string, direction: string, expiresAt?: string): Promise<ShareEntry> {
    return this.request('POST', '/shares', { peerId, folder, direction, expiresAt });
  }

  revokeShare(peerId: string, folder: string, direction?: string): Promise<void> {
    const body: Record<string, string> = { peerId, folder };
    if (direction) body['direction'] = direction;
    return this.request('DELETE', '/shares', body);
  }

  // ─── Tokens ───────────────────────────────────────────────────────────────

  tokens(): Promise<TokenEntry[]> {
    return this.request('GET', '/tokens');
  }

  revokeToken(id: string): Promise<void> {
    return this.request('DELETE', `/tokens/${id}`);
  }
}

export const api = new ApiClient();
