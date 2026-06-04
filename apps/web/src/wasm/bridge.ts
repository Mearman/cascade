import { RuntimeMode } from './capabilities';
import type { WorkerRequest, WorkerResponse, WorkerEvent } from './messages';

export interface ApiClient {
  request(method: string, path: string, body?: unknown): Promise<{ status: number; body: unknown }>;
  ready(): Promise<boolean>;
  mode: RuntimeMode;
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

class WasmApiClient implements ApiClient {
  readonly mode: RuntimeMode;
  private readonly worker: Worker;
  private readonly pending = new Map<string, (response: { status: number; body: unknown }) => void>();
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
}

export function createClient(mode: RuntimeMode, daemonUrl?: string): ApiClient {
  if (mode === RuntimeMode.Connected) {
    return new HttpApiClient(daemonUrl ?? '');
  }
  return new WasmApiClient(mode);
}
