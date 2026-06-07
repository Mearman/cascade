import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Mock } from 'vitest';
import { RuntimeMode } from './capabilities';
import { createClient } from './bridge';
import type { FileInput } from './bridge';
import type { MutatorAck, WorkerMutator, WorkerRequest, WorkerResponse } from './messages';

// ── Mock Worker ────────────────────────────────────────────────────────────

// The bridge only ever posts a WorkerRequest or a WorkerMutator, so the mock
// records that union directly — tests narrow on the discriminant rather than
// casting an `unknown`.
type PostedPayload = WorkerRequest | WorkerMutator;

interface PostedMessage {
  raw: PostedPayload;
  reply: (response: WorkerResponse | MutatorAck) => void;
}

function isWorkerMutator(payload: PostedPayload): payload is WorkerMutator {
  return 'mutator' in payload;
}

function expectMutator(sent: PostedMessage | undefined): WorkerMutator {
  expect(sent).toBeDefined();
  if (sent === undefined || !isWorkerMutator(sent.raw)) {
    throw new Error('expected the posted payload to be a WorkerMutator');
  }
  return sent.raw;
}

function expectRequest(sent: PostedMessage | undefined): WorkerRequest {
  expect(sent).toBeDefined();
  if (sent === undefined || isWorkerMutator(sent.raw)) {
    throw new Error('expected the posted payload to be a WorkerRequest');
  }
  return sent.raw;
}

// A mutator's string argument, asserted at the untyped `args` boundary.
function stringArg(mutator: WorkerMutator, index: number): string {
  const value = mutator.args[index];
  if (typeof value !== 'string') throw new Error(`arg ${String(index)} is not a string`);
  return value;
}

// Parse a JSON string the bridge produced and assert it decodes to an array.
function parseJsonArray(json: string): unknown[] {
  const parsed: unknown = JSON.parse(json);
  if (!Array.isArray(parsed)) throw new Error('expected a JSON array');
  return parsed;
}

// Read a recorded fetch call's URL and init, asserting both are present.
function fetchCall(mock: Mock<typeof fetch>, index: number): { url: string; init: RequestInit } {
  const call = mock.mock.calls[index];
  if (call === undefined) throw new Error(`no fetch call at index ${String(index)}`);
  const [input, init] = call;
  if (typeof input !== 'string') throw new Error('expected a string request URL');
  if (init === undefined) throw new Error('expected fetch init options');
  return { url: input, init };
}

// Narrow RequestInit.headers to the plain-object form the client uses.
function plainHeaders(init: RequestInit): Record<string, string> {
  const headers = init.headers;
  if (headers === undefined || headers instanceof Headers || Array.isArray(headers)) {
    throw new Error('expected a plain headers record');
  }
  return headers;
}

// Narrow RequestInit.body to the JSON string the client serialises.
function stringBody(init: RequestInit): string {
  const { body } = init;
  if (typeof body !== 'string') throw new Error('expected a string request body');
  return body;
}

class MockWorker {
  static lastInstance: MockWorker | undefined;
  static instances: MockWorker[] = [];

  readonly posted: PostedMessage[] = [];
  readonly listeners = new Set<EventListener>();
  readonly errorListeners = new Set<EventListener>();

  constructor(_url: URL, _options?: WorkerOptions) {
    MockWorker.instances.push(this);
    MockWorker.lastInstance = this;
  }

  postMessage(message: PostedPayload): void {
    const entry: PostedMessage = {
      raw: message,
      reply: (response) => {
        for (const listener of this.listeners) {
          listener(new MessageEvent('message', { data: response }));
        }
      },
    };
    this.posted.push(entry);
  }

  addEventListener(type: string, listener: EventListener): void {
    if (type === 'message') {
      this.listeners.add(listener);
    } else if (type === 'error') {
      this.errorListeners.add(listener);
    }
  }

  removeEventListener(type: string, listener: EventListener): void {
    if (type === 'message') this.listeners.delete(listener);
    else if (type === 'error') this.errorListeners.delete(listener);
  }

  terminate(): void {
    // No-op: nothing to release in the mock.
  }

  /** Simulate the worker posting a 'ready' event. */
  emitReady(): void {
    for (const listener of this.listeners) {
      listener(new MessageEvent('message', { data: { type: 'ready' } }));
    }
  }
}

const OriginalWorker = globalThis.Worker;

beforeEach(() => {
  MockWorker.instances = [];
  MockWorker.lastInstance = undefined;
  Object.defineProperty(globalThis, 'Worker', {
    value: MockWorker,
    configurable: true,
    writable: true,
  });
});

afterEach(() => {
  Object.defineProperty(globalThis, 'Worker', {
    value: OriginalWorker,
    configurable: true,
    writable: true,
  });
  vi.restoreAllMocks();
});

// ── WasmApiClient: deleteFiles ─────────────────────────────────────────────

describe('WasmApiClient.deleteFiles', () => {
  it('sends a delete_files mutator with the serialised file IDs', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;
    expect(worker).toBeDefined();

    const promise = client.deleteFiles('gdrive-personal', ['file-1', 'file-2']);
    const sent = worker?.posted[0];
    expect(sent).toBeDefined();

    const msg = expectMutator(sent);
    expect(msg.mutator).toBe('delete_files');
    expect(stringArg(msg, 0)).toBe('gdrive-personal');
    expect(stringArg(msg, 1)).toBe(JSON.stringify(['file-1', 'file-2']));

    // Acknowledge to resolve the promise.
    sent?.reply({ id: msg.id, result: undefined });
    await expect(promise).resolves.toBeUndefined();
  });

  it('resolves with undefined even when the worker returns a non-undefined result', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;
    const promise = client.deleteFiles('b1', ['f1']);
    const sent = worker?.posted[0];
    const msg = expectMutator(sent);
    sent?.reply({ id: msg.id, result: 'something-else' });
    await expect(promise).resolves.toBeUndefined();
  });
});

// ── WasmApiClient: upsertFiles ─────────────────────────────────────────────

describe('WasmApiClient.upsertFiles', () => {
  it('strips null/undefined optional fields and forwards serialised JSON', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;

    const files: FileInput[] = [
      {
        id: 'a',
        parent_id: 'root',
        name: 'A',
        is_dir: false,
        size: null,
        mime_type: null,
      },
      {
        id: 'b',
        parent_id: 'root',
        name: 'B',
        is_dir: true,
        size: 1024,
        mime_type: 'application/json',
      },
    ];

    const promise = client.upsertFiles('backend-1', files);
    const sent = worker?.posted[0];
    expect(sent).toBeDefined();
    const msg = expectMutator(sent);
    expect(msg.mutator).toBe('upsert_files');
    expect(stringArg(msg, 0)).toBe('backend-1');

    const payload = parseJsonArray(stringArg(msg, 1));
    // Nulls stripped; serde-default means the keys should be absent, not null.
    expect(payload[0]).toEqual({
      id: 'a',
      parent_id: 'root',
      name: 'A',
      is_dir: false,
    });
    expect(payload[0]).not.toHaveProperty('size');
    expect(payload[0]).not.toHaveProperty('mime_type');
    expect(payload[1]).toEqual({
      id: 'b',
      parent_id: 'root',
      name: 'B',
      is_dir: true,
      size: 1024,
      mime_type: 'application/json',
    });

    sent?.reply({ id: msg.id, result: undefined });
    await expect(promise).resolves.toBeUndefined();
  });

  it('drops null size/mime_type so they serialise as absent', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;
    // `upsertFiles` maps `null` to `undefined` so JSON.stringify omits the key
    // (the Rust side relies on serde defaults for absent fields).
    const files: FileInput[] = [{
      id: 'a',
      parent_id: 'p',
      name: 'A',
      is_dir: false,
      size: null,
      mime_type: null,
    }];
    const promise = client.upsertFiles('b1', files);
    const sent = worker?.posted[0];
    const msg = expectMutator(sent);
    const payload = parseJsonArray(stringArg(msg, 1));
    expect(payload[0]).not.toHaveProperty('size');
    expect(payload[0]).not.toHaveProperty('mime_type');
    sent?.reply({ id: msg.id, result: undefined });
    await expect(promise).resolves.toBeUndefined();
  });
});

// ── WasmApiClient: request() ───────────────────────────────────────────────

describe('WasmApiClient.request', () => {
  it('posts a WorkerRequest with the given method, path, and serialised body', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;
    const promise = client.request('POST', '/v1/backends', { id: 'x' });
    const sent = worker?.posted[0];
    const msg = expectRequest(sent);
    expect(msg.method).toBe('POST');
    expect(msg.path).toBe('/v1/backends');
    expect(msg.body).toEqual({ id: 'x' });
    sent?.reply({ id: msg.id, status: 200, body: { ok: true } });
    const result = await promise;
    expect(result.status).toBe(200);
    expect(result.body).toEqual({ ok: true });
  });

  it('omits the body key when no body is provided', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;
    const promise = client.request('GET', '/v1/health');
    const sent = worker?.posted[0];
    const msg = expectRequest(sent);
    expect(msg.method).toBe('GET');
    expect(msg.path).toBe('/v1/health');
    expect('body' in msg).toBe(false);
    sent?.reply({ id: msg.id, status: 204, body: null });
    await expect(promise).resolves.toEqual({ status: 204, body: null });
  });

  it('rejects unsupported HTTP methods', async () => {
    const client = createClient(RuntimeMode.Standalone);
    await expect(client.request('PATCH', '/v1/x')).rejects.toThrow(/Unsupported HTTP method/);
  });
});

// ── WasmApiClient: mutator rejection ───────────────────────────────────────

describe('WasmApiClient mutator error handling', () => {
  it('rejects the promise when the worker reports an error', async () => {
    const client = createClient(RuntimeMode.Standalone);
    const worker = MockWorker.lastInstance;
    const promise = client.deleteFiles('b1', ['f1']);
    const sent = worker?.posted[0];
    const msg = expectMutator(sent);
    sent?.reply({ id: msg.id, result: undefined, error: 'engine exploded' });
    await expect(promise).rejects.toThrow('engine exploded');
  });
});

// ── HttpApiClient ──────────────────────────────────────────────────────────

describe('HttpApiClient', () => {
  let fetchMock: Mock<typeof fetch>;

  beforeEach(() => {
    fetchMock = vi.fn<typeof fetch>();
    Object.defineProperty(globalThis, 'fetch', {
      value: fetchMock,
      configurable: true,
      writable: true,
    });
  });

  it('createClient returns an HttpApiClient in Connected mode', () => {
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    expect(client.mode).toBe(RuntimeMode.Connected);
  });

  it('request() hits the daemon /v1 path with the given method and JSON body', async () => {
    fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({ ok: true }), { status: 200 }));
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    const result = await client.request('POST', '/backends', { id: 'x' });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const { url, init } = fetchCall(fetchMock, 0);
    expect(url).toBe('http://localhost:7842/v1/backends');
    expect(init.method).toBe('POST');
    expect(plainHeaders(init)['Content-Type']).toBe('application/json');
    expect(init.body).toBe(JSON.stringify({ id: 'x' }));
    expect(result.status).toBe(200);
    expect(result.body).toEqual({ ok: true });
  });

  it('request() omits Content-Type and body when no body is given', async () => {
    fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({ entries: [] }), { status: 200 }));
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    await client.request('GET', '/shares');
    const { init } = fetchCall(fetchMock, 0);
    expect(init.method).toBe('GET');
    expect(init.headers).toBeUndefined();
    expect(init.body).toBeUndefined();
  });

  it('ready() returns true when the health endpoint responds ok', async () => {
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 200 }));
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    await expect(client.ready()).resolves.toBe(true);
  });

  it('ready() returns false when the health endpoint fails', async () => {
    fetchMock.mockRejectedValueOnce(new Error('network'));
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    await expect(client.ready()).resolves.toBe(false);
  });

  it('registerBackend() POSTs to /v1/backends with the id and type', async () => {
    fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({}), { status: 201 }));
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    await client.registerBackend('gdrive-1', 'gdrive');
    const { url, init } = fetchCall(fetchMock, 0);
    expect(url).toBe('http://localhost:7842/v1/backends');
    expect(init.method).toBe('POST');
    const parsed: unknown = JSON.parse(stringBody(init));
    expect(parsed).toEqual({ id: 'gdrive-1', type: 'gdrive' });
  });

  it('deregisterBackend() DELETEs /v1/backends/{id} and returns ok status', async () => {
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    await expect(client.deregisterBackend('gdrive 1')).resolves.toBe(true);
    const { url, init } = fetchCall(fetchMock, 0);
    expect(url).toBe('http://localhost:7842/v1/backends/gdrive%201');
    expect(init.method).toBe('DELETE');
  });

  it('upsertFiles() and deleteFiles() throw in Connected mode', () => {
    const client = createClient(RuntimeMode.Connected, 'http://localhost:7842');
    expect(() => client.upsertFiles('b', [])).toThrow(/not available in Connected mode/);
    expect(() => client.deleteFiles('b', [])).toThrow(/not available in Connected mode/);
  });
});
