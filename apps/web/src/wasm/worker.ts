import type { WorkerRequest, WorkerResponse, WorkerEvent } from './messages';

// The WASM glue is served from public/wasm/ which Vite exposes at the base
// path.  In a module worker import.meta.env.BASE_URL gives "/cascade/" so the
// full URL resolves to /cascade/wasm/cascade_wasm.js.
const WASM_GLUE_URL = `${import.meta.env.BASE_URL}wasm/cascade_wasm.js`;

let wasmReady = false;

// Dynamically imported at runtime; the WASM glue is not bundled by Vite.
type WasmInit = () => Promise<unknown>;
let wasmInit: WasmInit | undefined;

async function initWasm(): Promise<void> {
  try {
    const mod: { default: WasmInit } = await import(/* @vite-ignore */ WASM_GLUE_URL);
    await mod.default();
    wasmInit = mod.default;
    wasmReady = true;
  } catch (err) {
    const event: WorkerEvent = { type: 'error', data: String(err) };
    postMessage(event);
    return;
  }
  const event: WorkerEvent = { type: 'ready' };
  postMessage(event);
}

function isWorkerRequest(value: unknown): value is WorkerRequest {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('method' in value) || typeof value.method !== 'string') return false;
  if (!('path' in value) || typeof value.path !== 'string') return false;
  return true;
}

function handleRequest(req: WorkerRequest): WorkerResponse {
  // Stub: only /health and /v1/health are handled until the WASM engine is wired up.
  if (req.path === '/health' || req.path === '/v1/health') {
    return { id: req.id, status: 200, body: { status: 'ok', mode: 'wasm' } };
  }
  return {
    id: req.id,
    status: 404,
    body: {
      error: {
        code: 'not_found',
        message: `No WASM handler for ${req.method} ${req.path}`,
        request_id: req.id,
      },
    },
  };
}

addEventListener('message', (event: MessageEvent<unknown>) => {
  if (!isWorkerRequest(event.data)) return;

  if (!wasmReady) {
    const response: WorkerResponse = {
      id: event.data.id,
      status: 503,
      body: null,
      error: 'WASM engine not yet ready',
    };
    postMessage(response);
    return;
  }

  postMessage(handleRequest(event.data));
});

initWasm().catch((err: unknown) => {
  const event: WorkerEvent = { type: 'error', data: String(err) };
  postMessage(event);
});
