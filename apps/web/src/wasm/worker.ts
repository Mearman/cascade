import type { WorkerRequest, WorkerResponse, WorkerEvent } from './messages';

// The WASM module will live at a path relative to the built worker chunk.
// Uncomment and adjust once the cascade_wasm crate is compiled to WASM:
// import init from '../wasm/cascade_wasm.js';

let wasmReady = false;

async function initWasm(): Promise<void> {
  try {
    // await init();
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
