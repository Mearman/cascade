// Test environment setup.
//
// jsdom does not implement the Worker constructor; every test that exercises
// the WASM bridge layer needs a stub. We provide a no-op Worker class so the
// bridge can construct one without crashing. Individual tests should stub the
// worker's `postMessage` and `addEventListener` for behaviour-specific
// assertions via `vi.spyOn` or per-test mocks.
class StubWorker {
  postMessage(_message: unknown): void {
    // No-op: tests that assert on posted messages install their own MockWorker.
  }
  addEventListener(_type: string, _listener: EventListener): void {
    // No-op: see postMessage.
  }
  removeEventListener(_type: string, _listener: EventListener): void {
    // No-op: see postMessage.
  }
  terminate(): void {
    // No-op: see postMessage.
  }
}

if (typeof globalThis.Worker === 'undefined') {
  Object.defineProperty(globalThis, 'Worker', {
    value: StubWorker,
    configurable: true,
    writable: true,
  });
}

// jsdom lacks matchMedia, which some Preact/Testing Library helpers probe.
if (typeof window !== 'undefined' && typeof window.matchMedia !== 'function') {
  const noop = (): void => {
    // No-op: jsdom's MediaQueryList stub never emits change events.
  };
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: noop,
      removeListener: noop,
      addEventListener: noop,
      removeEventListener: noop,
      dispatchEvent: () => false,
    }),
  });
}
