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

function isServerMessage(value: unknown): value is ServerMessage {
  if (typeof value !== 'object' || value === null) return false;
  return 'type' in value && typeof (value as { type: unknown }).type === 'string';
}

export function TerminalPage() {
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [exited, setExited] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const wsRef = useRef<WebSocket | null>(null);

  useEffect(() => {
    if (containerRef.current === null) return;

    const token = loadToken();
    if (token === null) {
      setError('No capability token found. Log in first.');
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

    // Build the websocket URL with auth as query parameters. The browser
    // WebSocket API cannot send custom headers, so the token and bearer
    // device ride as query params.
    const base = getApiBase();
    const wsBase = base.replace(/^http/, 'ws');
    const tokenB64 = btoa(JSON.stringify(token));
    const params = new URLSearchParams({
      token: tokenB64,
      bearer: token.bearer,
    });
    const wsUrl = `${wsBase}/v1/exec/ws?${params}`;

    const ws = new WebSocket(wsUrl);
    wsRef.current = ws;

    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
      const spawn = {
        type: 'spawn',
        shell: null as string | null,
        cols: term.cols,
        rows: term.rows,
      };
      ws.send(JSON.stringify(spawn));
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
            const data = new Uint8Array(bytes);
            term.write(data);
          }
          break;
        }
        case 'exited':
          setExited(true);
          term.write(`\r\n[process exited${msg.code !== null && msg.code !== undefined ? ` code ${msg.code}` : ''}]\r\n`);
          break;
        case 'error':
          setError(msg.message ?? 'Unknown error');
          break;
        default:
          break;
      }
    };

    ws.onerror = () => {
      setError('WebSocket connection failed. The daemon may not have exec enabled.');
    };

    ws.onclose = () => {
      setConnected(false);
    };

    // Forward keystrokes as PTY writes.
    const disposable = term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) {
        const bytes = Array.from(new TextEncoder().encode(data));
        ws.send(JSON.stringify({ type: 'input', bytes }));
      }
    });

    // Forward resize events.
    const resizeDisposable = term.onResize(({ cols, rows }) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'resize', cols, rows }));
      }
    });

    // Handle window resize.
    const handleResize = () => {
      fitAddon.fit();
    };
    window.addEventListener('resize', handleResize);

    return () => {
      disposable.dispose();
      resizeDisposable.dispose();
      window.removeEventListener('resize', handleResize);
      ws.close();
      term.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function handleDisconnect() {
    if (wsRef.current !== null) {
      if (wsRef.current.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ type: 'signal', signal: 15 }));
      }
      wsRef.current.close();
    }
    if (termRef.current !== null) {
      termRef.current.dispose();
    }
  }

  if (error !== null) {
    return (
      <div class="terminal-page">
        <h2>Terminal</h2>
        <ErrorBanner message={error} onDismiss={() => { setError(null); }} />
      </div>
    );
  }

  return (
    <div class="terminal-page">
      <h2>Terminal</h2>
      {!connected && !exited && <Spinner />}
      {exited && <p class="muted">Session ended.</p>}
      <div class="terminal-toolbar">
        <button class="secondary" onClick={() => { handleDisconnect(); }}>
          Disconnect
        </button>
      </div>
      <div ref={containerRef} class="terminal-container" />
    </div>
  );
}
