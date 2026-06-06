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
const BASE = import.meta.env.BASE_URL;
const WASM_GLUE_URL = `${BASE}wasm/cascade_wasm.js`;
const WASM_BINARY_URL = `${BASE}wasm/cascade_wasm_bg.wasm`;

let wasmReady = false;

// Dynamically imported WASM glue exports. Available after initWasm().
// The wasm-bindgen init function accepts a string/URL/Request or an object
// { module_or_path: string | URL | Request }.
type WasmInitInput = string | URL | Request | { module_or_path: string | URL | Request };
type WasmInit = (input?: WasmInitInput) => Promise<unknown>;

interface WasmGlue {
  default: WasmInit;
  handle_request(request_json: string): unknown;
  register_backend(id: string, backend_type: string, handle?: unknown): void;
  deregister_backend(id: string): boolean;
  store_auth_token(provider: string, token_json: string): void;
  clear_auth_token(provider: string): boolean;
  upsert_files(backend_id: string, files_json: string): void;
  set_peer_connection(session_id: string, connection: unknown): void;
  remove_peer_connection(session_id: string): boolean;
}

function assertString(value: unknown, name: string): string {
  if (typeof value !== 'string') throw new Error(`expected ${name} to be a string`);
  return value;
}

function hasFunction(value: unknown, key: string): boolean {
  if (typeof value !== 'object' || value === null) return false;
  if (!(key in value)) return false;
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  return typeof descriptor?.value === 'function';
}

function isWasmGlue(value: unknown): value is WasmGlue {
  if (typeof value !== 'object' || value === null) return false;
  return hasFunction(value, 'default')
    && hasFunction(value, 'handle_request')
    && hasFunction(value, 'register_backend')
    && hasFunction(value, 'deregister_backend')
    && hasFunction(value, 'store_auth_token')
    && hasFunction(value, 'clear_auth_token')
    && hasFunction(value, 'upsert_files')
    && hasFunction(value, 'set_peer_connection')
    && hasFunction(value, 'remove_peer_connection');
}

let glue: WasmGlue | undefined;

async function initWasm(): Promise<void> {
  try {
    // Files in public/ cannot be imported with dynamic import() in Vite dev
    // mode — Vite intercepts the import and refuses to serve raw files from
    // public/ through its transform pipeline.
    //
    // The wasm-bindgen glue uses ES module syntax (export), so we need an
    // import that resolves to an ES module. Approach: fetch the glue JS,
    // create a blob URL, and dynamic-import it. Blob URLs support ES module
    // imports in module workers in Chrome 80+.
    const jsText = await (await fetch(WASM_GLUE_URL)).text();
    const blob = new Blob([jsText], { type: 'text/javascript' });
    const blobUrl = URL.createObjectURL(blob);
    let loaded: unknown;
    try {
      loaded = await import(/* @vite-ignore */ blobUrl);
    } finally {
      URL.revokeObjectURL(blobUrl);
    }

    if (!isWasmGlue(loaded)) {
      throw new Error('Loaded WASM module does not match expected interface');
    }
    // Pass the explicit WASM binary URL so the init function does not rely
    // on import.meta.url (which was the blob URL).
    await loaded.default({ module_or_path: WASM_BINARY_URL });
    glue = loaded;
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
    glue?.register_backend(assertString(args[0], 'id'), assertString(args[1], 'backendType'), args[2]);
    return undefined;
  },
  deregister_backend(...args) {
    return glue?.deregister_backend(assertString(args[0], 'id')) ?? false;
  },
  store_auth_token(...args) {
    glue?.store_auth_token(assertString(args[0], 'provider'), assertString(args[1], 'tokenJson'));
    return undefined;
  },
  clear_auth_token(...args) {
    return glue?.clear_auth_token(assertString(args[0], 'provider')) ?? false;
  },
  upsert_files(...args) {
    glue?.upsert_files(assertString(args[0], 'backendId'), assertString(args[1], 'filesJson'));
    return undefined;
  },
  set_peer_connection(...args) {
    glue?.set_peer_connection(assertString(args[0], 'sessionId'), args[1]);
    return undefined;
  },
  remove_peer_connection(...args) {
    return glue?.remove_peer_connection(assertString(args[0], 'sessionId')) ?? false;
  },
};

function handleMutator(msg: WorkerMutator): MutatorAck {
  const handler = mutatorHandlers[msg.mutator];
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
