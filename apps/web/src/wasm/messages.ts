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

/** Mutator command sent from the main thread to the worker. */
export type MutatorMethod =
  | 'register_backend'
  | 'deregister_backend'
  | 'store_auth_token'
  | 'clear_auth_token'
  | 'upsert_files'
  | 'delete_files'
  | 'set_peer_connection'
  | 'remove_peer_connection';

export interface WorkerMutator {
  id: string;
  mutator: MutatorMethod;
  args: unknown[];
}

/** Sent from the worker to the main thread as a lifecycle notification. */
export interface WorkerEvent {
  type: 'ready' | 'error' | 'state-change';
  data?: unknown;
}

/** Sent from the worker to acknowledge a mutator command. */
export interface MutatorAck {
  id: string;
  result: unknown;
  error?: string;
}
