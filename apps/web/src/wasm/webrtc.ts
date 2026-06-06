// WebRTC data channel transport for browser-side P2P communication.
// Handles signalling via the Cascade relay WebSocket and exchanges
// SDP offers/answers and ICE candidates to establish a direct data channel.
//
// Role determination:
//   - config.sessionId === undefined → initiator: generates a session ID,
//     connects to the relay, and sends an SDP offer immediately.
//   - config.sessionId !== undefined → responder: connects to the relay
//     using the provided session ID and waits for an incoming SDP offer.
//
// The relay is an opaque byte-pipe; no relay-level framing is added.
// All messages sent over the relay WebSocket are JSON-encoded RelayMessages.

export interface WebRtcConfig {
  relayUrl: string;
  stunServers: string[];
  sessionId?: string;
}

export interface FrameTransport {
  send(data: ArrayBuffer): void;
  onFrame(handler: (data: ArrayBuffer) => void): void;
  onClose(handler: () => void): void;
  close(): void;
  readonly connected: boolean;
}

// ─── Signalling message types ─────────────────────────────────────────────────

type RelayMessage =
  | { type: 'sdp-offer'; sdp: string }
  | { type: 'sdp-answer'; sdp: string }
  | { type: 'ice-candidate'; candidate: RTCIceCandidateInit };

function isRelayMessage(value: unknown): value is RelayMessage {
  if (typeof value !== 'object' || value === null) return false;
  if (!('type' in value) || typeof value.type !== 'string') return false;
  const t = value.type;
  if (t === 'sdp-offer' || t === 'sdp-answer') {
    return 'sdp' in value && typeof value.sdp === 'string';
  }
  if (t === 'ice-candidate') {
    return 'candidate' in value && typeof value.candidate === 'object' && value.candidate !== null;
  }
  return false;
}

// ─── Transport implementation ─────────────────────────────────────────────────

class WebRtcTransportImpl implements FrameTransport {
  private readonly pc: RTCPeerConnection;
  private readonly ws: WebSocket;
  private channel: RTCDataChannel | null = null;
  private frameHandler: ((data: ArrayBuffer) => void) | null = null;
  private closeHandler: (() => void) | null = null;
  private _connected = false;
  private readonly isInitiator: boolean;

  constructor(config: WebRtcConfig) {
    const iceServers = config.stunServers.map((url) => ({ urls: url }));
    this.pc = new RTCPeerConnection({ iceServers });
    this.isInitiator = config.sessionId === undefined;

    // Append the session ID as a query parameter so the relay can pair peers.
    const sessionId = config.sessionId ?? crypto.randomUUID();
    const separator = config.relayUrl.includes('?') ? '&' : '?';
    const relayUrl = `${config.relayUrl}${separator}session=${encodeURIComponent(sessionId)}`;

    this.ws = new WebSocket(relayUrl);

    this.setupPeerConnection();
    this.setupWebSocket();
  }

  private setupPeerConnection(): void {
    this.pc.onicecandidate = ({ candidate }) => {
      if (candidate === null || this.ws.readyState !== WebSocket.OPEN) return;
      this.sendRelay({ type: 'ice-candidate', candidate: candidate.toJSON() });
    };

    if (this.isInitiator) {
      // Initiator creates the data channel before generating the offer,
      // so the channel description is included in the SDP.
      const channel = this.pc.createDataChannel('cascade', { ordered: true });
      this.attachChannel(channel);
    } else {
      // Responder receives the data channel via the ondatachannel event.
      this.pc.ondatachannel = (event) => {
        this.attachChannel(event.channel);
      };
    }
  }

  private attachChannel(channel: RTCDataChannel): void {
    channel.binaryType = 'arraybuffer';
    this.channel = channel;

    channel.onopen = () => {
      this._connected = true;
    };

    channel.onclose = () => {
      this._connected = false;
      this.closeHandler?.();
    };

    channel.onmessage = (event) => {
      const raw: unknown = event.data;
      if (raw instanceof ArrayBuffer) {
        this.frameHandler?.(raw);
      }
    };
  }

  private setupWebSocket(): void {
    this.ws.onopen = () => {
      if (!this.isInitiator) return;

      void this.pc
        .createOffer()
        .then((offer) => this.pc.setLocalDescription(offer).then(() => offer))
        .then((offer) => {
          if (offer.sdp === undefined) throw new Error('SDP offer has no sdp field');
          this.sendRelay({ type: 'sdp-offer', sdp: offer.sdp });
        })
        .catch((err: unknown) => {
          console.error('WebRTC: failed to create offer', err);
          this.teardown();
        });
    };

    this.ws.onmessage = (event) => {
      const raw: unknown = event.data;
      let text: string;
      if (typeof raw === 'string') {
        text = raw;
      } else if (raw instanceof ArrayBuffer) {
        text = new TextDecoder().decode(raw);
      } else {
        return;
      }

      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch {
        console.warn('WebRTC: received non-JSON relay message');
        return;
      }

      if (!isRelayMessage(parsed)) {
        console.warn('WebRTC: received unrecognised relay message', parsed);
        return;
      }

      void this.handleRelayMessage(parsed).catch((err: unknown) => {
        console.error('WebRTC: error handling relay message', err);
        this.teardown();
      });
    };

    this.ws.onclose = () => {
      if (!this._connected) {
        this.teardown();
      }
    };

    this.ws.onerror = () => {
      console.error('WebRTC: relay WebSocket error');
      this.teardown();
    };
  }

  private async handleRelayMessage(msg: RelayMessage): Promise<void> {
    if (msg.type === 'sdp-offer') {
      await this.pc.setRemoteDescription({ type: 'offer', sdp: msg.sdp });
      const answer = await this.pc.createAnswer();
      await this.pc.setLocalDescription(answer);
      if (answer.sdp === undefined) throw new Error('SDP answer has no sdp field');
      this.sendRelay({ type: 'sdp-answer', sdp: answer.sdp });
    } else if (msg.type === 'sdp-answer') {
      await this.pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp });
    } else {
      await this.pc.addIceCandidate(new RTCIceCandidate(msg.candidate));
    }
  }

  private sendRelay(msg: RelayMessage): void {
    if (this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  private teardown(): void {
    this._connected = false;
    this.channel?.close();
    this.pc.close();
    if (
      this.ws.readyState !== WebSocket.CLOSING &&
      this.ws.readyState !== WebSocket.CLOSED
    ) {
      this.ws.close();
    }
    this.closeHandler?.();
  }

  // FrameTransport implementation

  get connected(): boolean {
    return this._connected;
  }

  send(data: ArrayBuffer): void {
    if (this.channel?.readyState === 'open') {
      this.channel.send(data);
    }
  }

  onFrame(handler: (data: ArrayBuffer) => void): void {
    this.frameHandler = handler;
  }

  onClose(handler: () => void): void {
    this.closeHandler = handler;
  }

  close(): void {
    this.teardown();
  }
}

// ─── Public API ───────────────────────────────────────────────────────────────

// Returns true if RTCPeerConnection and RTCDataChannel are available.
export function isWebRtcSupported(): boolean {
  return (
    typeof RTCPeerConnection !== 'undefined' &&
    typeof RTCDataChannel !== 'undefined'
  );
}

// Creates an RTCPeerConnection backed by the given relay for signalling.
// The returned FrameTransport is immediately usable; the underlying
// connection is established asynchronously. Register onFrame and onClose
// handlers before sending any data.
export function createPeerConnection(config: WebRtcConfig): FrameTransport {
  return new WebRtcTransportImpl(config);
}
