import { RuntimeMode } from './capabilities';
import type { WorkerRequest, WorkerResponse, WorkerEvent, WorkerMutator, MutatorAck } from './messages';

export interface ApiClient {
  request(method: string, path: string, body?: unknown): Promise<{ status: number; body: unknown }>;
  ready(): Promise<boolean>;
  mode: RuntimeMode;
  registerBackend(id: string, type: string, handle?: unknown): Promise<void>;
  deregisterBackend(id: string): Promise<boolean>;
  storeAuthToken(provider: string, token: { scope: string; expiry: number }): Promise<void>;
  clearAuthToken(provider: string): Promise<boolean>;
  upsertFiles(backendId: string, files: FileInput[]): Promise<void>;
  deleteFiles(backendId: string, fileIds: string[]): Promise<void>;
}

/** A file entry to upsert into engine storage. */
export interface FileInput {
  id: string;
  parent_id: string;
  name: string;
  is_dir: boolean;
  size?: number | null;
  mime_type?: string | null;
}

function isHttpMethod(m: string): m is WorkerRequest['method'] {
  return m === 'GET' || m === 'POST' || m === 'PUT' || m === 'DELETE';
}

function isWorkerResponse(value: unknown): value is WorkerResponse {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('status' in value) || typeof value.status !== 'number') return false;
  return 'body' in value;
}

function isWorkerEvent(value: unknown): value is WorkerEvent {
  if (typeof value !== 'object' || value === null) return false;
  return 'type' in value && typeof value.type === 'string';
}

function isMutatorAck(value: unknown): value is MutatorAck {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('result' in value)) return false;
  return true;
}

class WasmApiClient implements ApiClient {
  readonly mode: RuntimeMode;
  private readonly worker: Worker;
  private readonly pending = new Map<string, (response: { status: number; body: unknown }) => void>();
  private readonly mutatorPending = new Map<string, { resolve: (result: unknown) => void; reject: (err: Error) => void }>();
  private readonly readyPromise: Promise<boolean>;
  // Assigned synchronously by the Promise executor; declared as definite assignment
  // because TypeScript cannot see that the executor runs before the constructor returns.
  private resolveReady!: (value: boolean) => void;

  constructor(mode: RuntimeMode) {
    this.mode = mode;
    this.readyPromise = new Promise<boolean>((resolve) => {
      this.resolveReady = resolve;
    });
    // Vite resolves this import.meta.url reference at build time and emits a
    // separate worker chunk automatically.
    this.worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });

    this.worker.addEventListener('message', (event: MessageEvent<unknown>) => {
      const { data } = event;
      if (isWorkerResponse(data)) {
        const handler = this.pending.get(data.id);
        if (handler !== undefined) {
          this.pending.delete(data.id);
          handler({ status: data.status, body: data.body });
        }
      } else if (isMutatorAck(data)) {
        const entry = this.mutatorPending.get(data.id);
        if (entry !== undefined) {
          this.mutatorPending.delete(data.id);
          if (data.error !== undefined) {
            entry.reject(new Error(data.error));
          } else {
            entry.resolve(data.result);
          }
        }
      } else if (isWorkerEvent(data)) {
        if (data.type === 'ready') {
          this.resolveReady(true);
        } else if (data.type === 'error') {
          this.resolveReady(false);
        }
      }
    });

    this.worker.addEventListener('error', () => {
      this.resolveReady(false);
    });
  }

  ready(): Promise<boolean> {
    return this.readyPromise;
  }

  request(method: string, path: string, body?: unknown): Promise<{ status: number; body: unknown }> {
    if (!isHttpMethod(method)) {
      return Promise.reject(new Error(`Unsupported HTTP method: ${method}`));
    }
    const id = crypto.randomUUID();
    const msg: WorkerRequest = body !== undefined
      ? { id, method, path, body }
      : { id, method, path };

    return new Promise<{ status: number; body: unknown }>((resolve) => {
      this.pending.set(id, resolve);
      this.worker.postMessage(msg);
    });
  }

  private sendMutator(mutator: WorkerMutator['mutator'], args: unknown[]): Promise<unknown> {
    const id = crypto.randomUUID();
    const msg: WorkerMutator = { id, mutator, args };
    return new Promise<unknown>((resolve, reject) => {
      this.mutatorPending.set(id, { resolve, reject });
      this.worker.postMessage(msg);
    });
  }

  registerBackend(id: string, type: string, handle?: unknown): Promise<void> {
    return this.sendMutator('register_backend', [id, type, handle]).then(() => undefined);
  }

  deregisterBackend(id: string): Promise<boolean> {
    return this.sendMutator('deregister_backend', [id]).then((result) => result === true);
  }

  storeAuthToken(provider: string, token: { scope: string; expiry: number }): Promise<void> {
    return this.sendMutator('store_auth_token', [provider, JSON.stringify(token)]).then(() => undefined);
  }

  clearAuthToken(provider: string): Promise<boolean> {
    return this.sendMutator('clear_auth_token', [provider]).then((result) => result === true);
  }

  upsertFiles(backendId: string, files: FileInput[]): Promise<void> {
    // Filter out null/undefined values that JSON.stringify would drop but
    // the Rust side expects to be absent (serde default).
    const cleaned = files.map((f) => ({
      id: f.id,
      parent_id: f.parent_id,
      name: f.name,
      is_dir: f.is_dir,
      size: f.size ?? undefined,
      mime_type: f.mime_type ?? undefined,
    }));
    return this.sendMutator('upsert_files', [backendId, JSON.stringify(cleaned)]).then(() => undefined);
  }

  deleteFiles(backendId: string, fileIds: string[]): Promise<void> {
    return this.sendMutator('delete_files', [backendId, JSON.stringify(fileIds)]).then(() => undefined);
  }
}

class HttpApiClient implements ApiClient {
  readonly mode = RuntimeMode.Connected;
  private readonly baseUrl: string;

  constructor(baseUrl: string) {
    this.baseUrl = baseUrl;
  }

  ready(): Promise<boolean> {
    return fetch(`${this.baseUrl}/v1/health`)
      .then((r) => r.ok)
      .catch(() => false);
  }

  async request(method: string, path: string, body?: unknown): Promise<{ status: number; body: unknown }> {
    const init: RequestInit = { method };
    if (body !== undefined) {
      init.headers = { 'Content-Type': 'application/json' };
      init.body = JSON.stringify(body);
    }
    const res = await fetch(`${this.baseUrl}/v1${path}`, init);
    const responseBody: unknown = await res.json().catch(() => null);
    return { status: res.status, body: responseBody };
  }

  async registerBackend(id: string, type: string, _handle?: unknown): Promise<void> {
    const res = await fetch(`${this.baseUrl}/v1/backends`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id, type }),
    });
    if (!res.ok) {
      const body: unknown = await res.json().catch(() => null);
      throw new Error(`register_backend failed: ${String(res.status)} ${JSON.stringify(body)}`);
    }
  }

  async deregisterBackend(id: string): Promise<boolean> {
    const res = await fetch(`${this.baseUrl}/v1/backends/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    });
    return res.ok;
  }

  async storeAuthToken(provider: string, token: { scope: string; expiry: number }): Promise<void> {
    const res = await fetch(`${this.baseUrl}/v1/auth/tokens/${encodeURIComponent(provider)}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(token),
    });
    if (!res.ok) {
      const body: unknown = await res.json().catch(() => null);
      throw new Error(`store_auth_token failed: ${String(res.status)} ${JSON.stringify(body)}`);
    }
  }

  async clearAuthToken(provider: string): Promise<boolean> {
    const res = await fetch(`${this.baseUrl}/v1/auth/tokens/${encodeURIComponent(provider)}`, {
      method: 'DELETE',
    });
    return res.ok;
  }

  upsertFiles(_backendId: string, _files: FileInput[]): Promise<void> {
    // Connected mode does not use the mutator channel — file state comes from
    // the daemon's own backend polling.
    throw new Error('upsertFiles is not available in Connected mode');
  }

  deleteFiles(_backendId: string, _fileIds: string[]): Promise<void> {
    throw new Error('deleteFiles is not available in Connected mode');
  }
}

export function createClient(mode: RuntimeMode, daemonUrl?: string): ApiClient {
  if (mode === RuntimeMode.Connected) {
    return new HttpApiClient(daemonUrl ?? '');
  }
  return new WasmApiClient(mode);
}
