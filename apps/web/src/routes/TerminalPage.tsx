import { useEffect, useRef, useState } from 'preact/hooks';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { ErrorBanner, Spinner } from '@/components';
import { getApiBase, loadToken } from '@/auth';

interface ServerMessage {
  type: string;
  session?: number;
  stream?: string;
  bytes?: number[];
  code?: number | null;
  signal?: number | null;
  message?: string;
}

interface TicketResponse {
  ticket: string;
}

function isServerMessage(value: unknown): value is ServerMessage {
  if (typeof value !== 'object' || value === null) return false;
  return 'type' in value && typeof value.type === 'string';
}

function isTicketResponse(value: unknown): value is TicketResponse {
  if (typeof value !== 'object' || value === null) return false;
  return 'ticket' in value && typeof value.ticket === 'string';
}

export function TerminalPage() {
  const [folder, setFolder] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [exited, setExited] = useState(false);
  const [connecting, setConnecting] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  // Cleanup thunks for the current session's disposables + listeners.
  const cleanupRef = useRef<(() => void)[]>([]);

  // Tear down any live session on unmount.
  useEffect(() => () => {
    for (const cleanup of cleanupRef.current) cleanup();
    cleanupRef.current = [];
  }, []);

  function teardown() {
    for (const cleanup of cleanupRef.current) cleanup();
    cleanupRef.current = [];
    if (wsRef.current !== null && wsRef.current.readyState === WebSocket.OPEN) {
      wsRef.current.close();
    }
    if (termRef.current !== null) {
      termRef.current.dispose();
      termRef.current = null;
    }
    wsRef.current = null;
    setConnected(false);
  }

  async function connect() {
    if (containerRef.current === null) return;
    const trimmed = folder.trim();
    // A terminal runs code, so exec:pty is authorised over a specific folder
    // (never node-wide); the root folds to node-wide and is refused.
    if (trimmed === '' || trimmed === '/') {
      setError('Enter a specific folder to open the terminal in.');
      return;
    }
    const token = loadToken();
    if (token === null) {
      setError('No capability token found. Log in first.');
      return;
    }

    setConnecting(true);
    setError(null);

    // Exchange the long-lived token (which grants remote code execution) for a
    // short-lived, single-use ticket over a normal fetch — the browser can set
    // headers on fetch, unlike on a WebSocket. The ticket is the only thing
    // that ever appears in the websocket URL, so the real credential never
    // reaches access/proxy logs or history.
    let ticket: string;
    try {
      const base = getApiBase();
      const res = await fetch(`${base}/v1/exec/ticket`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${btoa(JSON.stringify(token))}`,
          'X-Cascade-Bearer-Device': token.bearer,
        },
        body: JSON.stringify({ folder: trimmed }),
      });
      if (!res.ok) {
        setError(`The daemon refused to authorise the terminal (${String(res.status)}).`);
        setConnecting(false);
        return;
      }
      const body: unknown = await res.json();
      if (!isTicketResponse(body) || body.ticket === '') {
        setError('The daemon did not issue a terminal ticket.');
        setConnecting(false);
        return;
      }
      ticket = body.ticket;
    } catch {
      setError('Could not reach the daemon to authorise the terminal.');
      setConnecting(false);
      return;
    }

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 14,
      fontFamily: 'Menlo, Monaco, "Courier New", monospace',
      cols: 80,
      rows: 24,
    });
    termRef.current = term;

    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(containerRef.current);
    fitAddon.fit();

    const wsBase = getApiBase().replace(/^http/, 'ws');
    const params = new URLSearchParams({ ticket, folder: trimmed });
    const ws = new WebSocket(`${wsBase}/v1/exec/ws?${params}`);
    wsRef.current = ws;
    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
      ws.send(JSON.stringify({ type: 'spawn', shell: null, cols: term.cols, rows: term.rows }));
    };

    ws.onmessage = (event) => {
      if (typeof event.data !== 'string') return;
      let msg: unknown;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      if (!isServerMessage(msg)) return;

      switch (msg.type) {
        case 'ready':
          setConnected(true);
          break;
        case 'output': {
          const bytes = msg.bytes;
          if (bytes !== undefined) {
            term.write(new Uint8Array(bytes));
          }
          break;
        }
        case 'exited':
          setExited(true);
          term.write(`\r\n[process exited${msg.code !== null && msg.code !== undefined ? ` code ${String(msg.code)}` : ''}]\r\n`);
          break;
        case 'error':
          setError(msg.message ?? 'Unknown error');
          break;
        default:
          break;
      }
    };

    ws.onerror = () => {
      setError('WebSocket connection failed. The daemon may not have exec enabled, or the ticket was rejected.');
    };

    ws.onclose = () => {
      setConnected(false);
      setConnecting(false);
    };

    const disposable = term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'input', bytes: Array.from(new TextEncoder().encode(data)) }));
      }
    });

    const resizeDisposable = term.onResize(({ cols, rows }) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'resize', cols, rows }));
      }
    });

    const handleResize = () => {
      fitAddon.fit();
    };
    window.addEventListener('resize', handleResize);

    cleanupRef.current = [
      () => { disposable.dispose(); },
      () => { resizeDisposable.dispose(); },
      () => { window.removeEventListener('resize', handleResize); },
      () => {
        if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) {
          ws.close();
        }
      },
      () => { term.dispose(); },
    ];
  }

  function handleDisconnect() {
    if (wsRef.current !== null && wsRef.current.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify({ type: 'signal', signal: 15 }));
    }
    teardown();
  }

  const showTerminal = connecting || connected;

  return (
    <div class="terminal-page">
      <h2>Terminal</h2>
      {error !== null && (
        <ErrorBanner message={error} onDismiss={() => { setError(null); }} />
      )}
      {!showTerminal && (
        <form
          class="terminal-toolbar"
          onSubmit={(e) => {
            e.preventDefault();
            void connect();
          }}
        >
          <input
            type="text"
            placeholder="folder to open the terminal in, e.g. work"
            value={folder}
            onInput={(e) => { setFolder(e.currentTarget.value); }}
            style="flex:1;min-width:12rem"
          />
          <button type="submit" class="primary">Connect</button>
        </form>
      )}
      {showTerminal && !exited && !connected && <Spinner />}
      {exited && <p class="muted">Session ended.</p>}
      {showTerminal && (
        <>
          <div class="terminal-toolbar">
            <button class="secondary" onClick={() => { handleDisconnect(); }}>
              Disconnect
            </button>
          </div>
          <div ref={containerRef} class="terminal-container" />
        </>
      )}
    </div>
  );
}
