// Messages exchanged over the worker's postMessage channel.

/** Sent from the main thread to the worker to invoke an API endpoint. */
export interface WorkerRequest {
  id: string;
  method: 'GET' | 'POST' | 'PUT' | 'DELETE';
  path: string;
  body?: unknown;
  headers?: Record<string, string>;
}

/** Sent from the worker to the main thread in response to a WorkerRequest. */
export interface WorkerResponse {
  id: string;
  status: number;
  body: unknown;
  error?: string;
}

/** Sent from the worker to the main thread as a lifecycle notification. */
export interface WorkerEvent {
  type: 'ready' | 'error' | 'state-change';
  data?: unknown;
}
