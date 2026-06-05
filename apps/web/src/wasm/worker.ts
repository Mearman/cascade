import type {
  WorkerRequest,
  WorkerResponse,
  WorkerEvent,
  WorkerMutator,
  MutatorAck,
  MutatorMethod,
} from './messages';

// The WASM glue is served from public/wasm/ which Vite exposes at the base
// path.  In a module worker import.meta.env.BASE_URL gives "/cascade/" so the
// full URL resolves to /cascade/wasm/cascade_wasm.js.
const WASM_GLUE_URL = `${import.meta.env.BASE_URL}wasm/cascade_wasm.js`;

let wasmReady = false;

// Dynamically imported WASM glue exports. Available after initWasm().
type WasmInit = (input?: RequestInfo | URL) => Promise<unknown>;

interface WasmGlue {
  default: WasmInit;
  handle_request(request_json: string): unknown;
  register_backend(id: string, backend_type: string, handle?: unknown): void;
  deregister_backend(id: string): boolean;
  store_auth_token(provider: string, token_json: string): void;
  clear_auth_token(provider: string): boolean;
  set_peer_connection(session_id: string, connection: unknown): void;
  remove_peer_connection(session_id: string): boolean;
}

let glue: WasmGlue | undefined;

async function initWasm(): Promise<void> {
  try {
    const mod: WasmGlue = await import(/* @vite-ignore */ WASM_GLUE_URL);
    await mod.default();
    glue = mod;
    wasmReady = true;
  } catch (err) {
    const event: WorkerEvent = { type: 'error', data: String(err) };
    postMessage(event);
    return;
  }
  const event: WorkerEvent = { type: 'ready' };
  postMessage(event);
}

// ─── Message type guards ────────────────────────────────────────────────────

function isWorkerRequest(value: unknown): value is WorkerRequest {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('method' in value) || typeof value.method !== 'string') return false;
  if (!('path' in value) || typeof value.path !== 'string') return false;
  return true;
}

function isWorkerMutator(value: unknown): value is WorkerMutator {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('mutator' in value) || typeof value.mutator !== 'string') return false;
  if (!('args' in value) || !Array.isArray(value.args)) return false;
  return true;
}

// ─── Request handling ───────────────────────────────────────────────────────

function handleRequest(req: WorkerRequest): WorkerResponse {
  // The glue's handle_request accepts a JSON string and returns a JS object
  // matching the WorkerResponse shape (id, status, body, optional error).
  const requestJson = JSON.stringify(req);
  const result = glue?.handle_request(requestJson);
  // The WASM glue returns a plain JS object via externref — no JSON.parse needed.
  if (isWorkerResponse(result)) {
    return result;
  }
  // Fallback for malformed responses (should not occur with a valid engine).
  return {
    id: req.id,
    status: 500,
    body: null,
    error: 'WASM engine returned an unexpected response shape',
  };
}

function isWorkerResponse(value: unknown): value is WorkerResponse {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('status' in value) || typeof value.status !== 'number') return false;
  return 'body' in value;
}

// ─── Mutator dispatch ───────────────────────────────────────────────────────

const mutatorHandlers: Record<MutatorMethod, (...args: unknown[]) => unknown> = {
  register_backend(...args) {
    const [id, backendType, handle] = args as [string, string, unknown?];
    glue?.register_backend(id, backendType, handle);
    return undefined;
  },
  deregister_backend(...args) {
    const [id] = args as [string];
    return glue?.deregister_backend(id) ?? false;
  },
  store_auth_token(...args) {
    const [provider, tokenJson] = args as [string, string];
    glue?.store_auth_token(provider, tokenJson);
    return undefined;
  },
  clear_auth_token(...args) {
    const [provider] = args as [string];
    return glue?.clear_auth_token(provider) ?? false;
  },
  set_peer_connection(...args) {
    const [sessionId, connection] = args as [string, unknown];
    glue?.set_peer_connection(sessionId, connection);
    return undefined;
  },
  remove_peer_connection(...args) {
    const [sessionId] = args as [string];
    return glue?.remove_peer_connection(sessionId) ?? false;
  },
};

function handleMutator(msg: WorkerMutator): MutatorAck {
  const handler = mutatorHandlers[msg.mutator];
  if (handler === undefined) {
    return { id: msg.id, result: undefined, error: `unknown mutator: ${msg.mutator}` };
  }
  try {
    const result = handler(...msg.args);
    return { id: msg.id, result };
  } catch (err: unknown) {
    return { id: msg.id, result: undefined, error: String(err) };
  }
}

// ─── Message listener ───────────────────────────────────────────────────────

addEventListener('message', (event: MessageEvent<unknown>) => {
  const { data } = event;

  if (isWorkerMutator(data)) {
    postMessage(handleMutator(data));
    return;
  }

  if (!isWorkerRequest(data)) return;

  if (!wasmReady) {
    const response: WorkerResponse = {
      id: data.id,
      status: 503,
      body: null,
      error: 'WASM engine not yet ready',
    };
    postMessage(response);
    return;
  }

  postMessage(handleRequest(data));
});

initWasm().catch((err: unknown) => {
  const event: WorkerEvent = { type: 'error', data: String(err) };
  postMessage(event);
});
