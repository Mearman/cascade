export enum RuntimeMode {
  Standalone = 'standalone',
  Connected = 'connected',
  BrowseOnly = 'browse-only',
}

export interface Capabilities {
  fileSystemAccess: boolean;
  webRtc: boolean;
  serviceWorker: boolean;
  wasm: boolean;
  indexedDb: boolean;
}

export function detectCapabilities(): Capabilities {
  return {
    fileSystemAccess: 'showDirectoryPicker' in window,
    webRtc: 'RTCPeerConnection' in window,
    serviceWorker: 'serviceWorker' in navigator,
    wasm: typeof WebAssembly !== 'undefined',
    indexedDb: 'indexedDB' in window,
  };
}

// Connected takes precedence when a daemon URL is known; otherwise fall back to
// Standalone (WASM + File System Access API) or BrowseOnly (WASM + OPFS only).
export function recommendMode(capabilities: Capabilities, hasDaemonUrl: boolean): RuntimeMode {
  if (hasDaemonUrl) return RuntimeMode.Connected;
  if (capabilities.wasm && capabilities.fileSystemAccess) return RuntimeMode.Standalone;
  return RuntimeMode.BrowseOnly;
}
