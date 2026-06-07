// The worker module runs `initWasm()` and calls `addEventListener` /
// `postMessage` at import time. We need to stub those globals *before* the
// import is evaluated, so the stubs are installed via `vi.hoisted` (which
// vitest runs before any import).
vi.hoisted(() => {
  const noop = (): void => {
    // Stub: the worker module wires up listeners and posts at import time;
    // these globals just need to exist so that import does not throw.
  };
  const define = (key: string, value: unknown): void => {
    Object.defineProperty(globalThis, key, { value, configurable: true, writable: true });
  };
  define('addEventListener', noop);
  define('postMessage', noop);
  define('fetch', () => Promise.resolve(new Response('')));
});

import { describe, expect, it, vi } from 'vitest';
import type { Mock } from 'vitest';
import { handleMutator, isWasmGlue, mutatorHandlers } from './worker';
import type { WasmGlue } from './worker';
import type { MutatorMethod, WorkerMutator } from './messages';

// Each glue member is a mock that carries the real WasmGlue signature, so the
// object is both assignable to WasmGlue (for the handlers under test) and
// retains the mock matchers (`toHaveBeenCalledWith`).
type WasmGlueMock = { [K in keyof WasmGlue]: Mock<WasmGlue[K]> };

// ── isWasmGlue guard ──────────────────────────────────────────────────────

describe('isWasmGlue', () => {
  function makeGlue(overrides: Partial<WasmGlueMock> = {}): WasmGlueMock {
    return {
      default: vi.fn<WasmGlue['default']>(() => Promise.resolve(undefined)),
      handle_request: vi.fn<WasmGlue['handle_request']>(),
      register_backend: vi.fn<WasmGlue['register_backend']>(),
      deregister_backend: vi.fn<WasmGlue['deregister_backend']>(() => true),
      store_auth_token: vi.fn<WasmGlue['store_auth_token']>(),
      clear_auth_token: vi.fn<WasmGlue['clear_auth_token']>(() => true),
      upsert_files: vi.fn<WasmGlue['upsert_files']>(),
      delete_files: vi.fn<WasmGlue['delete_files']>(),
      set_peer_connection: vi.fn<WasmGlue['set_peer_connection']>(),
      remove_peer_connection: vi.fn<WasmGlue['remove_peer_connection']>(() => true),
      ...overrides,
    };
  }

  it('accepts a complete WASM glue module', () => {
    expect(isWasmGlue(makeGlue())).toBe(true);
  });

  it.each([
    ['null', null],
    ['undefined', undefined],
    ['a primitive', 42],
    ['a string', 'wasm'],
  ])('rejects %s', (_label, value) => {
    expect(isWasmGlue(value)).toBe(false);
  });

  it('rejects a glue missing one of the required methods', () => {
    const glue = makeGlue();
    const { delete_files: _omit, ...rest } = glue;
    expect(isWasmGlue(rest)).toBe(false);
  });

  it('rejects when an "export" is a data property holding a non-function', () => {
    expect(isWasmGlue({ default: 'not a function' })).toBe(false);
  });
});

// ── mutatorHandlers dispatch ───────────────────────────────────────────────

function makeGlue(): WasmGlueMock {
  return {
    default: vi.fn<WasmGlue['default']>(() => Promise.resolve(undefined)),
    handle_request: vi.fn<WasmGlue['handle_request']>(() => ({ id: 'x', status: 200, body: null })),
    register_backend: vi.fn<WasmGlue['register_backend']>(),
    deregister_backend: vi.fn<WasmGlue['deregister_backend']>(() => true),
    store_auth_token: vi.fn<WasmGlue['store_auth_token']>(),
    clear_auth_token: vi.fn<WasmGlue['clear_auth_token']>(() => true),
    upsert_files: vi.fn<WasmGlue['upsert_files']>(),
    delete_files: vi.fn<WasmGlue['delete_files']>(),
    set_peer_connection: vi.fn<WasmGlue['set_peer_connection']>(),
    remove_peer_connection: vi.fn<WasmGlue['remove_peer_connection']>(() => true),
  };
}

const MUTATORS: { method: MutatorMethod; args: unknown[]; assert: (g: ReturnType<typeof makeGlue>) => void }[] = [
  {
    method: 'register_backend',
    args: ['gdrive-1', 'gdrive', { handle: 'h' }],
    assert: (g) => {
      expect(g.register_backend).toHaveBeenCalledWith('gdrive-1', 'gdrive', { handle: 'h' });
    },
  },
  {
    method: 'deregister_backend',
    args: ['gdrive-1'],
    assert: (g) => {
      expect(g.deregister_backend).toHaveBeenCalledWith('gdrive-1');
    },
  },
  {
    method: 'store_auth_token',
    args: ['gdrive', '{"scope":"full","expiry":1234}'],
    assert: (g) => {
      expect(g.store_auth_token).toHaveBeenCalledWith('gdrive', '{"scope":"full","expiry":1234}');
    },
  },
  {
    method: 'clear_auth_token',
    args: ['gdrive'],
    assert: (g) => {
      expect(g.clear_auth_token).toHaveBeenCalledWith('gdrive');
    },
  },
  {
    method: 'upsert_files',
    args: ['b1', '[{"id":"f1","parent_id":"root","name":"a","is_dir":false}]'],
    assert: (g) => {
      expect(g.upsert_files).toHaveBeenCalledWith(
        'b1',
        '[{"id":"f1","parent_id":"root","name":"a","is_dir":false}]',
      );
    },
  },
  {
    method: 'delete_files',
    args: ['b1', '["f1","f2"]'],
    assert: (g) => {
      expect(g.delete_files).toHaveBeenCalledWith('b1', '["f1","f2"]');
    },
  },
  {
    method: 'set_peer_connection',
    args: ['session-1', { conn: 'rtc' }],
    assert: (g) => {
      expect(g.set_peer_connection).toHaveBeenCalledWith('session-1', { conn: 'rtc' });
    },
  },
  {
    method: 'remove_peer_connection',
    args: ['session-1'],
    assert: (g) => {
      expect(g.remove_peer_connection).toHaveBeenCalledWith('session-1');
    },
  },
];

describe('mutatorHandlers dispatch', () => {
  it.each(MUTATORS)('$method routes to the corresponding glue method', ({ method, args, assert }) => {
    const glue = makeGlue();
    const result = mutatorHandlers[method](glue, ...args);
    assert(glue);
    // The methods that return booleans should reflect the glue's return value;
    // void-returning methods should return undefined.
    if (method === 'deregister_backend' || method === 'clear_auth_token' || method === 'remove_peer_connection') {
      expect(result).toBe(true);
    } else {
      expect(result).toBeUndefined();
    }
  });

  it('returns false for the boolean mutators when glue is undefined', () => {
    expect(mutatorHandlers.deregister_backend(undefined, 'x')).toBe(false);
    expect(mutatorHandlers.clear_auth_token(undefined, 'x')).toBe(false);
    expect(mutatorHandlers.remove_peer_connection(undefined, 'x')).toBe(false);
  });

  it('returns undefined for the void mutators when glue is undefined', () => {
    expect(mutatorHandlers.register_backend(undefined, 'x', 'y')).toBeUndefined();
    expect(mutatorHandlers.store_auth_token(undefined, 'x', 'y')).toBeUndefined();
    expect(mutatorHandlers.upsert_files(undefined, 'x', 'y')).toBeUndefined();
    expect(mutatorHandlers.delete_files(undefined, 'x', 'y')).toBeUndefined();
    expect(mutatorHandlers.set_peer_connection(undefined, 'x', 'y')).toBeUndefined();
  });

  it('asserts that the first argument is a string', () => {
    const glue = makeGlue();
    // The handler accepts `unknown` args, so a numeric first arg exercises the
    // runtime string assertion without any type cast.
    expect(() => mutatorHandlers.delete_files(glue, 42, '[]'))
      .toThrow(/expected backendId to be a string/);
  });
});

// ── handleMutator ─────────────────────────────────────────────────────────

describe('handleMutator', () => {
  it("delete_files dispatches to glue.delete_files and returns a MutatorAck", () => {
    const glue = makeGlue();
    const msg: WorkerMutator = {
      id: 'm1',
      mutator: 'delete_files',
      args: ['b1', JSON.stringify(['file-1', 'file-2'])],
    };
    const ack = handleMutator(msg, glue);
    expect(glue.delete_files).toHaveBeenCalledWith('b1', JSON.stringify(['file-1', 'file-2']));
    expect(ack).toEqual({ id: 'm1', result: undefined });
  });

  it('captures handler errors and surfaces them in the ack', () => {
    const glue = makeGlue();
    glue.register_backend = vi.fn(() => {
      throw new Error('boom');
    });
    const msg: WorkerMutator = { id: 'm2', mutator: 'register_backend', args: ['b', 'gdrive'] };
    const ack = handleMutator(msg, glue);
    expect(ack).toEqual({ id: 'm2', result: undefined, error: 'Error: boom' });
  });

  it('propagates the boolean return value of deregister_backend', () => {
    const glue = makeGlue();
    glue.deregister_backend = vi.fn(() => false);
    const ack = handleMutator({ id: 'm3', mutator: 'deregister_backend', args: ['b1'] }, glue);
    expect(ack).toEqual({ id: 'm3', result: false });
  });

  it('falls back to the module-level glue when none is passed', () => {
    // The module-level `glue` is undefined in the test environment, so the
    // boolean mutators should report failure rather than throw.
    const ack = handleMutator({ id: 'm4', mutator: 'deregister_backend', args: ['b1'] });
    expect(ack).toEqual({ id: 'm4', result: false });
  });
});
