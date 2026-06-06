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
import {
  isCapabilityToken,
  isSessionResponse,
  isHealthResponse,
  isReadyResponse,
  isFolderChildrenResponse,
  isEntryMetaResponse,
  isSharesResponse,
  isShareEntry,
  isTokensResponse,
  isRevokeTokenResponse,
  isGrantsResponse,
  isGrantEntry,
  isAuditResponse,
  isPeersResponse,
  isPinsResponse,
  isPinEntry,
  isPoliciesResponse,
  isPolicyEntry,
  isBackendsResponse,
  isCacheActionResponse,
  isDeviceCodeResponse,
  isDevicePollResponse,
} from './guards';
import { createClient as createBridgeClient, type ApiClient as BridgeApiClient, type FileInput, RuntimeMode } from '@/wasm';
import { downloadDriveFile, uploadDriveFile, deleteDriveFile } from '@/wasm/gdrive';

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

function isBridgeMethod(m: string): m is 'GET' | 'POST' | 'PUT' | 'DELETE' {
  return m === 'GET' || m === 'POST' || m === 'PUT' || m === 'DELETE';
}

function isApiErrorResponse(body: unknown): body is ApiErrorResponse {
  if (typeof body !== 'object' || body === null) return false;
  if (!('error' in body) || typeof body.error !== 'object' || body.error === null) return false;
  const e = body.error;
  return 'code' in e && 'message' in e && 'request_id' in e;
}

const INTERNAL_ERROR: ErrorCode = 'internal';

function isErrorCode(code: unknown): code is ErrorCode {
  const codes: readonly string[] = [
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
    const code = isErrorCode(e.code) ? e.code : INTERNAL_ERROR;
    const result: ApiErrorDetail = {
      code,
      message: e.message,
      request_id: e.request_id,
    };
    if (e.details !== undefined) {
      result.details = e.details;
    }
    return result;
  }
  return { code: INTERNAL_ERROR, message: 'Unknown error', request_id: '' };
}

/** Validate that a raw JSON value matches the expected type, throwing on mismatch. */
function validated<T>(value: unknown, guard: (v: unknown) => v is T, label: string): T {
  if (!guard(value)) throw new Error(`Invalid ${label} response`);
  return value;
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

  /** Low-level request returning raw JSON (unknown) or null for 204. */
  private async request(
    method: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'HEAD',
    path: string,
    body?: unknown,
  ): Promise<unknown> {
    // WASM modes route through the bridge worker instead of direct fetch.
    if (this.bridgeClient !== null) {
      if (!isBridgeMethod(method)) {
        throw new Error(`WASM bridge does not support HTTP method: ${method}`);
      }
      const result = await this.bridgeClient.request(method, path, body);
      if (result.status === 204) return null;
      if (result.status === 401) {
        this.token = null;
        this.on401?.();
        throw new ApiError(parseApiError(result.body));
      }
      if (result.status < 200 || result.status >= 300) {
        throw new ApiError(parseApiError(result.body));
      }
      return result.body;
    }

    const token = this.getToken();
    const headers: Record<string, string> = {};

    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
    }

    if (token !== null) {
      headers.Authorization = `Bearer ${btoa(JSON.stringify(token))}`;
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
      return null;
    }

    const raw: unknown = await res.json().catch(() => null);

    if (!res.ok) {
      throw new ApiError(parseApiError(raw));
    }

    return raw;
  }

  // ─── Health / Readiness ────────────────────────────────────────────────────

  async health(): Promise<HealthResponse> {
    if (this.bridgeClient !== null) {
      const result = await this.bridgeClient.request('GET', '/health');
      return validated(result.body, isHealthResponse, 'health');
    }
    const raw: unknown = await fetch(`${getBase()}/v1/health`).then((r) => r.json());
    return validated(raw, isHealthResponse, 'health');
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
      return validated(raw, isCapabilityToken, 'auth pair');
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
      return validated(raw, isCapabilityToken, 'auth secret');
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
      return validated(raw, isDeviceCodeResponse, 'device code request');
    });
  }

  /** Device code: poll for authorisation. */
  authDevicePoll(code: string): Promise<{ status: string; token?: CapabilityToken }> {
    const base = getBase();
    return fetch(`${base}/v1/auth/device/${encodeURIComponent(code)}`).then(async (r) => {
      const raw: unknown = await r.json().catch(() => null);
      if (!r.ok) throw new ApiError(parseApiError(raw));
      return validated(raw, isDevicePollResponse, 'device poll');
    });
  }

  ready(): Promise<ReadyResponse> {
    return this.request('GET', '/ready').then((raw) => validated(raw, isReadyResponse, 'ready'));
  }

  // ─── Session ──────────────────────────────────────────────────────────────

  session(): Promise<SessionResponse> {
    return this.request('GET', '/session').then((raw) => validated(raw, isSessionResponse, 'session'));
  }

  revokeSession(): Promise<SessionResponse> {
    return this.request('POST', '/session/revoke').then((raw) => validated(raw, isSessionResponse, 'revoke session'));
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
    return this.request('GET', `/folders/${encodeURIComponent(folder)}/children?${params}`)
      .then((raw) => validated(raw, isFolderChildrenResponse, 'folder children'));
  }

  entryMeta(folder: string, path: string): Promise<EntryMetaResponse> {
    return this.request('GET', `/folders/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`)
      .then((raw) => validated(raw, isEntryMetaResponse, 'entry meta'));
  }

  // ─── Files ─────────────────────────────────────────────────────────────────

  /** Download a file's content. In WASM mode, `path` is the Drive file ID. */
  downloadFile(folder: string, path: string): Promise<Response> {
    if (this.bridgeClient !== null) {
      // WASM mode: fetch directly from Drive API using the stored access token.
      return downloadDriveFile(path).then((blob) => new Response(blob));
    }
    const token = this.getToken();
    const headers: Record<string, string> = {};
    if (token !== null) {
      headers.Authorization = `Bearer ${btoa(JSON.stringify(token))}`;
      headers['X-Cascade-Bearer-Device'] = token.bearer;
    }
    return fetch(`${getBase()}/v1/files/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`, { headers });
  }

  /** Upload a new file. In WASM mode, `folder` is the backend ID and `path` is
   *  interpreted as the parent folder ID. */
  uploadFile(folder: string, path: string, content: Blob, etag?: string): Promise<EntryMetaResponse> {
    if (this.bridgeClient !== null) {
      // WASM mode: upload directly to Drive API. `folder` is backend ID,
      // `path` is the parent Drive folder ID. The file name comes from the
      // Blob if available, or is generated.
      const name = content instanceof File ? content.name : `upload-${String(Date.now())}`;
      return uploadDriveFile(folder, path, name, content, content.type).then(() => ({
        name,
        kind: 'file' as const,
        size: content.size,
        mtime: null,
        etag: null,
      }));
    }
    const token = this.getToken();
    const headers: Record<string, string> = {};
    if (token !== null) {
      headers.Authorization = `Bearer ${btoa(JSON.stringify(token))}`;
      headers['X-Cascade-Bearer-Device'] = token.bearer;
    }
    if (etag !== undefined) headers['If-Match'] = etag;
    return fetch(
      `${getBase()}/v1/files/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`,
      { method: 'PUT', headers, body: content },
    ).then((r) => r.json()).then((raw: unknown) => validated(raw, isEntryMetaResponse, 'upload'));
  }

  /** Delete a file. In WASM mode, `path` is the Drive file ID. */
  deleteFile(folder: string, path: string): Promise<void> {
    if (this.bridgeClient !== null) {
      return deleteDriveFile(folder, path);
    }
    return this.request('DELETE', `/files/${encodeURIComponent(folder)}/entries/${encodeURIComponent(path)}`).then(() => undefined);
  }

  // ─── Shares ───────────────────────────────────────────────────────────────

  shares(): Promise<SharesResponse> {
    return this.request('GET', '/shares').then((raw) => validated(raw, isSharesResponse, 'shares'));
  }

  createShare(body: CreateShareBody): Promise<ShareEntry> {
    return this.request('POST', '/shares', body).then((raw) => validated(raw, isShareEntry, 'create share'));
  }

  deleteShare(id: number): Promise<void> {
    return this.request('DELETE', `/shares/${String(id)}`).then(() => undefined);
  }

  // ─── Tokens ───────────────────────────────────────────────────────────────

  tokens(): Promise<TokensResponse> {
    return this.request('GET', '/tokens').then((raw) => validated(raw, isTokensResponse, 'tokens'));
  }

  createToken(body: CreateTokenBody): Promise<CapabilityToken> {
    return this.request('POST', '/tokens', body).then((raw) => validated(raw, isCapabilityToken, 'create token'));
  }

  revokeToken(id: string): Promise<RevokeTokenResponse> {
    return this.request('POST', `/tokens/${encodeURIComponent(id)}/revoke`)
      .then((raw) => validated(raw, isRevokeTokenResponse, 'revoke token'));
  }

  // ─── Grants ───────────────────────────────────────────────────────────────

  grants(): Promise<GrantsResponse> {
    return this.request('GET', '/grants').then((raw) => validated(raw, isGrantsResponse, 'grants'));
  }

  createGrant(body: CreateGrantBody): Promise<GrantEntry> {
    return this.request('POST', '/grants', body).then((raw) => validated(raw, isGrantEntry, 'create grant'));
  }

  deleteGrant(id: number): Promise<void> {
    return this.request('DELETE', `/grants/${String(id)}`).then(() => undefined);
  }

  // ─── Audit ────────────────────────────────────────────────────────────────

  audit(params?: { since?: string } & PaginationParams): Promise<AuditResponse> {
    const qs = new URLSearchParams();
    if (params?.since !== undefined) qs.set('since', params.since);
    if (params?.limit !== undefined) qs.set('limit', String(params.limit));
    if (params?.cursor !== undefined) qs.set('cursor', params.cursor);
    const suffix = qs.toString() ? `?${qs}` : '';
    return this.request('GET', `/audit${suffix}`).then((raw) => validated(raw, isAuditResponse, 'audit'));
  }

  // ─── Peers ────────────────────────────────────────────────────────────────

  peers(): Promise<PeersResponse> {
    return this.request('GET', '/peers').then((raw) => validated(raw, isPeersResponse, 'peers'));
  }

  // ─── Pins ─────────────────────────────────────────────────────────────────

  pins(): Promise<PinsResponse> {
    return this.request('GET', '/pins').then((raw) => validated(raw, isPinsResponse, 'pins'));
  }

  createPin(body: CreatePinBody): Promise<PinEntry> {
    return this.request('POST', '/pins', body).then((raw) => validated(raw, isPinEntry, 'create pin'));
  }

  deletePin(id: number): Promise<void> {
    return this.request('DELETE', `/pins/${String(id)}`).then(() => undefined);
  }

  // ─── Policies ─────────────────────────────────────────────────────────────

  policies(): Promise<PoliciesResponse> {
    return this.request('GET', '/policies').then((raw) => validated(raw, isPoliciesResponse, 'policies'));
  }

  createPolicy(body: CreatePolicyBody): Promise<PolicyEntry> {
    return this.request('POST', '/policies', body).then((raw) => validated(raw, isPolicyEntry, 'create policy'));
  }

  deletePolicy(id: number): Promise<void> {
    return this.request('DELETE', `/policies/${String(id)}`).then(() => undefined);
  }

  // ─── Backends ─────────────────────────────────────────────────────────────

  backends(): Promise<BackendsResponse> {
    return this.request('GET', '/backends').then((raw) => validated(raw, isBackendsResponse, 'backends'));
  }

  /** Register a backend via the WASM mutator channel (or POST in Connected mode). */
  registerBackend(id: string, type: string, handle?: unknown): Promise<void> {
    if (this.bridgeClient !== null) {
      return this.bridgeClient.registerBackend(id, type, handle);
    }
    return this.request('POST', '/backends', { id, type }).then(() => undefined);
  }

  /** Deregister a backend via the WASM mutator channel (or DELETE in Connected mode). */
  deregisterBackend(id: string): Promise<boolean> {
    if (this.bridgeClient !== null) {
      return this.bridgeClient.deregisterBackend(id);
    }
    return this.request('DELETE', `/backends/${encodeURIComponent(id)}`).then(() => true).catch(() => false);
  }

  /** Store an auth token for a provider via the WASM mutator channel (or POST in Connected mode). */
  storeAuthToken(provider: string, token: { scope: string; expiry: number }): Promise<void> {
    if (this.bridgeClient !== null) {
      return this.bridgeClient.storeAuthToken(provider, token);
    }
    return this.request('POST', `/auth/tokens/${encodeURIComponent(provider)}`, token).then(() => undefined);
  }

  /** Clear an auth token for a provider via the WASM mutator channel (or DELETE in Connected mode). */
  clearAuthToken(provider: string): Promise<boolean> {
    if (this.bridgeClient !== null) {
      return this.bridgeClient.clearAuthToken(provider);
    }
    return this.request('DELETE', `/auth/tokens/${encodeURIComponent(provider)}`).then(() => true).catch(() => false);
  }

  /** Insert or replace file entries in engine storage (WASM modes only). */
  upsertFiles(backendId: string, files: FileInput[]): Promise<void> {
    if (this.bridgeClient !== null) {
      return this.bridgeClient.upsertFiles(backendId, files);
    }
    return Promise.reject(new Error('upsertFiles is only available in WASM mode'));
  }

  /** Make a raw request through the bridge (WASM modes) or fetch (Connected mode). */
  rawRequest(method: 'GET' | 'POST' | 'PUT' | 'DELETE', path: string, body?: unknown): Promise<{ status: number; body: unknown }> {
    if (this.bridgeClient !== null) {
      return this.bridgeClient.request(method, path, body);
    }
    return this.request(method, path, body)
      .then((result) => ({ status: 200, body: result }))
      .catch((err: unknown) => {
        if (err instanceof ApiError) {
          return { status: 500, body: { error: err } };
        }
        return { status: 500, body: { error: String(err) } };
      });
  }

  // ─── Cache ────────────────────────────────────────────────────────────────

  cacheEvict(): Promise<CacheActionResponse> {
    return this.request('POST', '/cache/evict').then((raw) => validated(raw, isCacheActionResponse, 'cache evict'));
  }

  cacheWarm(body: CacheWarmBody): Promise<CacheActionResponse> {
    return this.request('POST', '/cache/warm', body).then((raw) => validated(raw, isCacheActionResponse, 'cache warm'));
  }

  // ─── Config ───────────────────────────────────────────────────────────────

  configPush(body: ConfigPushBody): Promise<void> {
    return this.request('POST', '/config/push', body).then(() => undefined);
  }
}

export const api = new ApiClient();
