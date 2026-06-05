import type {
  CapabilityToken,
  SessionResponse,
  HealthResponse,
  ReadyResponse,
  FolderChildrenResponse,
  EntryMetaResponse,
  ShareEntry,
  SharesResponse,
  CreateShareBody,
  TokenEntry,
  TokensResponse,
  RevokeTokenResponse,
  CreateTokenBody,
  GrantEntry,
  GrantsResponse,
  CreateGrantBody,
  AuditResponse,
  PeersResponse,
  PinsResponse,
  PinEntry,
  CreatePinBody,
  PoliciesResponse,
  PolicyEntry,
  CreatePolicyBody,
  BackendsResponse,
  CacheWarmBody,
  CacheActionResponse,
  ConfigPushBody,
  PaginationParams,
  ApiErrorResponse,
  ApiErrorDetail,
  ErrorCode,
} from './types';
import { ApiError } from './types';
import { createClient as createBridgeClient, type ApiClient as BridgeApiClient, RuntimeMode } from '@/wasm';

export const API_BASE_KEY = 'cascade-api-base';
export const TOKEN_KEY = 'cascade-token';

function getBase(): string {
  return localStorage.getItem(API_BASE_KEY) ?? '';
}

function getStoredToken(): CapabilityToken | null {
  try {
    const raw = localStorage.getItem(TOKEN_KEY);
    if (raw === null) return null;
    const parsed: unknown = JSON.parse(raw);
    if (!isCapabilityToken(parsed)) return null;
    return parsed;
  } catch {
    return null;
  }
}

function isCapabilityToken(value: unknown): value is CapabilityToken {
  if (typeof value !== 'object' || value === null) return false;
  if (!('token_id' in value) || typeof value.token_id !== 'string') return false;
  if (!('issuer' in value) || typeof value.issuer !== 'string') return false;
  if (!('bearer' in value) || typeof value.bearer !== 'string') return false;
  if (!('capability' in value) || typeof value.capability !== 'string') return false;
  if (!('scope' in value) || typeof value.scope !== 'object') return false;
  if (!('expires' in value) || typeof value.expires !== 'string') return false;
  if (!('issued_at' in value) || typeof value.issued_at !== 'string') return false;
  return true;
}

function isBridgeMethod(m: string): m is 'GET' | 'POST' | 'PUT' | 'DELETE' {
  return m === 'GET' || m === 'POST' || m === 'PUT' || m === 'DELETE';
}

function isApiErrorResponse(body: unknown): body is ApiErrorResponse {
  if (typeof body !== 'object' || body === null) return false;
  if (!('error' in body) || typeof body.error !== 'object' || body.error === null) return false;
  const e = body.error;
  return 'code' in e && 'message' in e && 'request_id' in e;
}

function isErrorCode(code: unknown): code is ErrorCode {
  const codes: ReadonlyArray<string> = [
    'unauthorised', 'forbidden', 'not_found', 'conflict', 'gone',
    'payload_too_large', 'unprocessable', 'rate_limited', 'internal',
    'unavailable', 'timeout', 'bearer_mismatch', 'token_too_large',
    'chain_too_deep', 'data_verb_node_wide_forbidden',
    'delegation_exceeds_parent', 'data_plane_not_ready', 'precondition_failed',
  ];
  return typeof code === 'string' && codes.includes(code);
}

function parseApiError(body: unknown): ApiErrorDetail {
  if (isApiErrorResponse(body)) {
    const e = body.error;
    const code = isErrorCode(e.code) ? e.code : ('internal' as ErrorCode);
    const result: ApiErrorDetail = {
      code,
      message: String(e.message),
      request_id: String(e.request_id),
    };
    if (typeof e.details === 'object' && e.details !== null && !Array.isArray(e.details)) {
      result.details = e.details as Record<string, unknown>;
    }
    return result;
  }
  return { code: 'internal', message: 'Unknown error', request_id: '' };
}

export class ApiClient {
  private token: CapabilityToken | null = null;
  private on401: (() => void) | null = null;
  private bridgeClient: BridgeApiClient | null = null;
  private currentMode: RuntimeMode = RuntimeMode.Connected;

  setMode(mode: RuntimeMode): void {
    if (mode === this.currentMode) return;
    this.currentMode = mode;
    if (mode === RuntimeMode.Connected) {
      this.bridgeClient = null;
    } else {
      this.bridgeClient = createBridgeClient(mode);
    }
  }

  getMode(): RuntimeMode {
    return this.currentMode;
  }

  // Resolves to true once the WASM worker has initialised. Always false in Connected mode.
  wasmReady(): Promise<boolean> {
    if (this.bridgeClient === null) return Promise.resolve(false);
    return this.bridgeClient.ready();
  }

  setToken(token: CapabilityToken | null): void {
    this.token = token;
  }

  getToken(): CapabilityToken | null {
    return this.token ?? getStoredToken();
  }

  setOn401(handler: () => void): void {
    this.on401 = handler;
  }

  private async request<T>(
    method: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'HEAD',
    path: string,
    body?: unknown,
  ): Promise<T> {
    // WASM modes route through the bridge worker instead of direct fetch.
    if (this.bridgeClient !== null) {
      if (!isBridgeMethod(method)) {
        throw new Error(`WASM bridge does not support HTTP method: ${method}`);
      }
      const result = await this.bridgeClient.request(method, path, body);
      if (result.status === 204) return undefined as T;
      if (result.status === 401) {
        this.token = null;
        this.on401?.();
        throw new ApiError(parseApiError(result.body));
      }
      if (result.status < 200 || result.status >= 300) {
        throw new ApiError(parseApiError(result.body));
      }
      return result.body as T;
    }

    const token = this.getToken();
    const headers: Record<string, string> = {};

    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
    }

    if (token !== null) {
      headers['Authorization'] = `Bearer ${btoa(JSON.stringify(token))}`;
      headers['X-Cascade-Bearer-Device'] = token.bearer;
    }

    // Generate a request id: 26-char base32 (Crockford) approximation using random bytes.
    const reqBytes = crypto.getRandomValues(new Uint8Array(16));
    headers['X-Cascade-Request-Id'] = Array.from(reqBytes)
      .map((b) => '0123456789ABCDEFGHJKMNPQRSTVWXYZ'[b % 32] ?? '0')
      .join('')
      .slice(0, 26);

    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      init.body = JSON.stringify(body);
    }

    const url = `${getBase()}/v1${path}`;
    const res = await fetch(url, init);

    if (res.status === 401) {
      this.token = null;
      this.on401?.();
      const raw: unknown = await res.json().catch(() => null);
      throw new ApiError(parseApiError(raw));
    }

    if (res.status === 204) {
      return undefined as T;
    }

    const raw: unknown = await res.json().catch(() => null);

    if (!res.ok) {
      throw new ApiError(parseApiError(raw));
    }

    return raw as T;
  }

  // ─── Health / Readiness ────────────────────────────────────────────────────

  async health(): Promise<HealthResponse> {
    if (this.bridgeClient !== null) {
      const result = await this.bridgeClient.request('GET', '/health');
      return result.body as HealthResponse;
    }
    return fetch(`${getBase()}/v1/health`).then((r) => r.json() as Promise<HealthResponse>);
  }

  // ─── Auth (no session required) ──────────────────────────────────────────────

  /** Pairing code: submit a code from `cascade auth pair`. */
  authPair(code: string): Promise<CapabilityToken> {
    const base = getBase();
    return fetch(`${base}/v1/auth/pair`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code }),
    }).then(async (r) => {
      const raw: unknown = await r.json().catch(() => null);
      if (!r.ok) throw new ApiError(parseApiError(raw));
      if (!isCapabilityToken(raw)) throw new Error('invalid token response');
      return raw;
    });
  }

  /** Shared secret: submit the daemon secret. */
  authSecret(secret: string): Promise<CapabilityToken> {
    const base = getBase();
    return fetch(`${base}/v1/auth/secret`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ secret }),
    }).then(async (r) => {
      const raw: unknown = await r.json().catch(() => null);
      if (!r.ok) throw new ApiError(parseApiError(raw));
      if (!isCapabilityToken(raw)) throw new Error('invalid token response');
      return raw;
    });
  }

  /** Device code: request a new code. */
  authDeviceRequest(): Promise<{ code: string; expires_in: number }> {
    const base = getBase();
    return fetch(`${base}/v1/auth/device`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
    }).then(async (r) => {
      const raw: unknown = await r.json().catch(() => null);
      if (!r.ok) throw new ApiError(parseApiError(raw));
      return raw as { code: string; expires_in: number };
    });
  }

  /** Device code: poll for authorisation. */
  authDevicePoll(code: string): Promise<{ status: string; token?: CapabilityToken }> {
    const base = getBase();
    return fetch(`${base}/v1/auth/device/${encodeURIComponent(code)}`).then(async (r) => {
      const raw: unknown = await r.json().catch(() => null);
      if (!r.ok) throw new ApiError(parseApiError(raw));
      return raw as { status: string; token?: CapabilityToken };
    });
  }

  ready(): Promise<ReadyResponse> {
    return this.request('GET', '/ready');
  }

  // ─── Session ──────────────────────────────────────────────────────────────

  session(): Promise<SessionResponse> {
    return this.request('GET', '/session');
  }

  revokeSession(): Promise<SessionResponse> {
    return this.request('POST', '/session/revoke');
  }

  // ─── Folders ──────────────────────────────────────────────────────────────

  folderChildren(
    folder: string,
    path: string,
    pagination?: PaginationParams,
  ): Promise<FolderChildrenResponse> {
    const params = new URLSearchParams({ path });
    if (pagination?.limit !== undefined) params.set('limit', String(pagination.limit));
    if (pagination?.cursor !== undefined) params.set('cursor', pagination.cursor);
    return this.request('GET', `/folders/${encodeURIComponent(folder)}/children?${params}`);
  }

  entryMeta(folder: string, path: string): Promise<EntryMetaResponse> {
    return this.request('GET', `/folders/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`);
  }

  // ─── Files ─────────────────────────────────────────────────────────────────

  downloadFile(folder: string, path: string): Promise<Response> {
    const token = this.getToken();
    const headers: Record<string, string> = {};
    if (token !== null) {
      headers['Authorization'] = `Bearer ${btoa(JSON.stringify(token))}`;
      headers['X-Cascade-Bearer-Device'] = token.bearer;
    }
    return fetch(`${getBase()}/v1/files/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`, { headers });
  }

  uploadFile(folder: string, path: string, content: Blob, etag?: string): Promise<EntryMetaResponse> {
    const token = this.getToken();
    const headers: Record<string, string> = {};
    if (token !== null) {
      headers['Authorization'] = `Bearer ${btoa(JSON.stringify(token))}`;
      headers['X-Cascade-Bearer-Device'] = token.bearer;
    }
    if (etag !== undefined) headers['If-Match'] = etag;
    return fetch(
      `${getBase()}/v1/files/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`,
      { method: 'PUT', headers, body: content },
    ).then((r) => r.json() as Promise<EntryMetaResponse>);
  }

  deleteFile(folder: string, path: string): Promise<void> {
    return this.request('DELETE', `/files/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`);
  }

  // ─── Shares ───────────────────────────────────────────────────────────────

  shares(): Promise<SharesResponse> {
    return this.request('GET', '/shares');
  }

  createShare(body: CreateShareBody): Promise<ShareEntry> {
    return this.request('POST', '/shares', body);
  }

  deleteShare(id: number): Promise<void> {
    return this.request('DELETE', `/shares/${id}`);
  }

  // ─── Tokens ───────────────────────────────────────────────────────────────

  tokens(): Promise<TokensResponse> {
    return this.request('GET', '/tokens');
  }

  createToken(body: CreateTokenBody): Promise<CapabilityToken> {
    return this.request('POST', '/tokens', body);
  }

  revokeToken(id: string): Promise<RevokeTokenResponse> {
    return this.request('POST', `/tokens/${encodeURIComponent(id)}/revoke`);
  }

  // ─── Grants ───────────────────────────────────────────────────────────────

  grants(): Promise<GrantsResponse> {
    return this.request('GET', '/grants');
  }

  createGrant(body: CreateGrantBody): Promise<GrantEntry> {
    return this.request('POST', '/grants', body);
  }

  deleteGrant(id: number): Promise<void> {
    return this.request('DELETE', `/grants/${id}`);
  }

  // ─── Audit ────────────────────────────────────────────────────────────────

  audit(params?: { since?: string } & PaginationParams): Promise<AuditResponse> {
    const qs = new URLSearchParams();
    if (params?.since !== undefined) qs.set('since', params.since);
    if (params?.limit !== undefined) qs.set('limit', String(params.limit));
    if (params?.cursor !== undefined) qs.set('cursor', params.cursor);
    const suffix = qs.toString() ? `?${qs}` : '';
    return this.request('GET', `/audit${suffix}`);
  }

  // ─── Peers ────────────────────────────────────────────────────────────────

  peers(): Promise<PeersResponse> {
    return this.request('GET', '/peers');
  }

  // ─── Pins ─────────────────────────────────────────────────────────────────

  pins(): Promise<PinsResponse> {
    return this.request('GET', '/pins');
  }

  createPin(body: CreatePinBody): Promise<PinEntry> {
    return this.request('POST', '/pins', body);
  }

  deletePin(id: number): Promise<void> {
    return this.request('DELETE', `/pins/${id}`);
  }

  // ─── Policies ─────────────────────────────────────────────────────────────

  policies(): Promise<PoliciesResponse> {
    return this.request('GET', '/policies');
  }

  createPolicy(body: CreatePolicyBody): Promise<PolicyEntry> {
    return this.request('POST', '/policies', body);
  }

  deletePolicy(id: number): Promise<void> {
    return this.request('DELETE', `/policies/${id}`);
  }

  // ─── Backends ─────────────────────────────────────────────────────────────

  backends(): Promise<BackendsResponse> {
    return this.request('GET', '/backends');
  }

  // ─── Cache ────────────────────────────────────────────────────────────────

  cacheEvict(): Promise<CacheActionResponse> {
    return this.request('POST', '/cache/evict');
  }

  cacheWarm(body: CacheWarmBody): Promise<CacheActionResponse> {
    return this.request('POST', '/cache/warm', body);
  }

  // ─── Config ───────────────────────────────────────────────────────────────

  configPush(body: ConfigPushBody): Promise<void> {
    return this.request('POST', '/config/push', body);
  }
}

export const api = new ApiClient();
