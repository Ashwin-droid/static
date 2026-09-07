import { parseMessage, reconnectDelay, type ServerMessage } from './protocol';

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected' | 'unavailable';
export interface SocketLike {
  readyState: number;
  bufferedAmount: number;
  onopen: (() => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  send(data: string): void;
  close(): void;
}
export interface ConnectionOptions {
  url: string | null;
  bands: () => number;
  onMessage: (message: ServerMessage) => void;
  onStatus: (status: ConnectionStatus) => void;
  socket?: (url: string) => SocketLike;
  schedule?: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  cancel?: (timer: ReturnType<typeof setTimeout>) => void;
}

/** One socket and one retry timer; commands are never queued across disconnects. */
export function connectStatic(options: ConnectionOptions) {
  const schedule = options.schedule ?? setTimeout;
  const cancel = options.cancel ?? clearTimeout;
  const makeSocket = options.socket ?? (url => new WebSocket(url) as unknown as SocketLike);
  let socket: SocketLike | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let timeout: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let attempt = 0;

  const send = (message: unknown) => {
    if (disposed || !socket || socket.readyState !== 1 || socket.bufferedAmount > 256_000) return false;
    try { socket.send(JSON.stringify(message)); return true; } catch { return false; }
  };
  const detach = () => {
    if (timeout !== undefined) cancel(timeout);
    timeout = undefined;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      socket.close();
      socket = null;
    }
  };
  const retry = () => {
    if (disposed) return;
    detach();
    options.onStatus('disconnected');
    if (attempt >= 8) { options.onStatus('unavailable'); return; }
    timer = schedule(open, reconnectDelay(attempt++));
  };
  const open = () => {
    timer = undefined;
    if (disposed || !options.url) return;
    options.onStatus('connecting');
    try { socket = makeSocket(options.url); } catch { retry(); return; }
    timeout = schedule(retry, 10_000);
    socket.onopen = () => {
      if (timeout !== undefined) cancel(timeout);
      timeout = undefined;
      attempt = 0;
      options.onStatus('connected');
      send({ type: 'configure', bands: options.bands() });
    };
    socket.onmessage = event => {
      const message = parseMessage(event.data);
      if (message) options.onMessage(message);
    };
    socket.onclose = retry;
    socket.onerror = retry;
  };
  if (options.url) open();
  else options.onStatus('unavailable');

  return {
    send,
    dispose() {
      disposed = true;
      if (timer !== undefined) cancel(timer);
      detach();
    },
  };
}
