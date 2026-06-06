import { createContext } from 'preact';
import { RuntimeMode, type Capabilities } from '@/wasm';

export interface AppContextValue {
  mode: RuntimeMode;
  capabilities: Capabilities;
  directoryName: string | null;
  setDirectoryName: (name: string | null) => void;
}

export const AppContext = createContext<AppContextValue>({
  mode: RuntimeMode.Connected,
  capabilities: {
    fileSystemAccess: false,
    webRtc: false,
    serviceWorker: false,
    wasm: false,
    indexedDb: false,
  },
  directoryName: null,
  setDirectoryName: (_name: string | null) => { /* no-op default */ },
});
