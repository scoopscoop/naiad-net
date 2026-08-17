import { thumbUrl } from './api';
import { thumbQueue, type CancelFn, type ThumbCallbacks } from './thumb-queue';

const HEADER_BYTES = 36;
const OPEN_TIMEOUT_MS = 1_500;
const RETRY_MS = [1_000, 2_000, 5_000, 10_000, 30_000] as const;
const HASH_RE = /^[0-9a-f]{64}$/;
const SOCKET_OPEN = 1;

export interface WebSocketLike {
  binaryType: BinaryType;
  readonly readyState: number;
  send(data: string | ArrayBufferLike | Blob | ArrayBufferView): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: string, listener: EventListener): void;
  removeEventListener(type: string, listener: EventListener): void;
}

export interface ThumbStreamOptions {
  socketFactory?: (url: string) => WebSocketLike;
  fallbackRequest?: (url: string, callbacks: ThumbCallbacks) => CancelFn;
  setTimer?: typeof setTimeout;
  clearTimer?: typeof clearTimeout;
}

interface Entry {
  subscribers: Map<number, ThumbCallbacks>;
  transport: 'pending' | 'stream' | 'http';
  cancelHttp?: CancelFn;
}

export interface DecodedFrame {
  hash: string;
  jpeg: Uint8Array<ArrayBuffer> | null;
}

export function streamUrl(loc: Location = location): string {
  const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${loc.host}/thumb-stream`;
}

export function decodeFrame(buffer: ArrayBuffer): DecodedFrame {
  if (buffer.byteLength < HEADER_BYTES) {
    throw new Error(`thumbnail frame is shorter than ${HEADER_BYTES} bytes`);
  }

  const declaredLength = new DataView(buffer).getUint32(32, false);
  const actualLength = buffer.byteLength - HEADER_BYTES;
  if (declaredLength !== actualLength) {
    throw new Error(
      `thumbnail frame length mismatch: declared ${declaredLength}, received ${actualLength}`,
    );
  }

  const bytes = new Uint8Array(buffer);
  let hash = '';
  for (let index = 0; index < 32; index++) hash += bytes[index].toString(16).padStart(2, '0');
  return {
    hash,
    jpeg: declaredLength === 0 ? null : bytes.slice(HEADER_BYTES),
  };
}

export function createThumbStreamClient(options: ThumbStreamOptions = {}) {
  const socketFactory = options.socketFactory ?? ((url: string) => new WebSocket(url));
  const fallbackRequest = options.fallbackRequest
    ?? ((url: string, callbacks: ThumbCallbacks) => thumbQueue.request(url, callbacks));
  const setTimer = options.setTimer ?? setTimeout;
  const clearTimer = options.clearTimer ?? clearTimeout;

  const entries = new Map<string, Entry>();
  let nextSubscriberId = 0;
  let socket: WebSocketLike | undefined;
  let state: 'idle' | 'connecting' | 'open' | 'degraded' = 'idle';
  let outageActive = false;
  let retryIndex = 0;
  let openTimer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let detachSocketListeners: (() => void) | undefined;
  let lastUrl = '';

  function clearOpenTimer() {
    if (openTimer === undefined) return;
    clearTimer(openTimer);
    openTimer = undefined;
  }

  function clearRetryTimer() {
    if (retryTimer === undefined) return;
    clearTimer(retryTimer);
    retryTimer = undefined;
  }

  function finish(hash: string, entry: Entry, deliver: (callbacks: ThumbCallbacks) => void) {
    if (entries.get(hash) !== entry) return;
    const callbacks = [...entry.subscribers.values()];
    entries.delete(hash);
    for (const subscriber of callbacks) {
      try {
        deliver(subscriber);
      } catch (error) {
        console.error(`[thumb-stream] subscriber callback failed for ${hash}`, error);
      }
    }
  }

  function startHttp(hash: string, entry: Entry) {
    entry.transport = 'http';
    const cancel = fallbackRequest(thumbUrl(hash), {
      onBlob: (blob) => finish(hash, entry, (callbacks) => callbacks.onBlob?.(blob)),
      onError: (error) => finish(hash, entry, (callbacks) => callbacks.onError?.(error)),
    });
    if (entries.get(hash) === entry) entry.cancelHttp = cancel;
    else cancel();
  }

  function migrateOutstandingToHttp() {
    for (const [hash, entry] of entries) {
      if (entry.transport !== 'http') startHttp(hash, entry);
    }
  }

  function scheduleReconnect() {
    if (retryTimer !== undefined) return;
    const delay = RETRY_MS[Math.min(retryIndex, RETRY_MS.length - 1)];
    retryIndex = Math.min(retryIndex + 1, RETRY_MS.length - 1);
    retryTimer = setTimer(() => {
      retryTimer = undefined;
      connect();
    }, delay);
  }

  function activateFailure(failedSocket: WebSocketLike | undefined, detail: string, quiet = false) {
    if (failedSocket !== socket || state === 'degraded') return;
    const outstanding = [...entries.values()].filter((entry) => entry.transport !== 'http').length;
    detachSocketListeners?.();
    detachSocketListeners = undefined;
    socket = undefined;
    state = 'degraded';
    clearOpenTimer();

    if (!outageActive) {
      outageActive = true;
      const message = `[thumb-stream] ${lastUrl} failed (${detail}); outstanding=${outstanding}; HTTP fallback activated`;
      if (quiet) console.debug(message);
      else console.warn(message);
    }

    migrateOutstandingToHttp();
    scheduleReconnect();
  }

  function sendWant(hash: string, entry: Entry) {
    const current = socket;
    if (current === undefined || state !== 'open' || current.readyState !== SOCKET_OPEN) return;
    entry.transport = 'stream';
    try {
      current.send(`want ${hash}`);
    } catch (error) {
      activateFailure(current, `send error=${String(error)}`);
      current.close(1000, 'thumbnail stream send failed');
    }
  }

  function connect() {
    if (state === 'connecting' || state === 'open') return;
    state = 'connecting';
    clearRetryTimer();
    lastUrl = streamUrl();

    let candidate: WebSocketLike;
    try {
      candidate = socketFactory(lastUrl);
    } catch (error) {
      socket = undefined;
      activateFailure(undefined, `constructor error=${String(error)}`);
      return;
    }
    socket = candidate;
    candidate.binaryType = 'arraybuffer';

    const onOpen: EventListener = () => {
      if (socket !== candidate || state !== 'connecting') return;
      clearOpenTimer();
      state = 'open';
      retryIndex = 0;
      if (outageActive) {
        outageActive = false;
        console.info(`[thumb-stream] ${lastUrl} recovered; future thumbnails use the stream`);
      }
      for (const [hash, entry] of entries) {
        if (entry.transport === 'pending') sendWant(hash, entry);
      }
    };

    const onMessage: EventListener = (event) => {
      if (socket !== candidate || state !== 'open') return;
      const data = (event as MessageEvent<unknown>).data;
      if (!(data instanceof ArrayBuffer)) {
        activateFailure(candidate, `malformed frame kind=${Object.prototype.toString.call(data)}`);
        candidate.close(1002, 'malformed thumbnail frame');
        return;
      }

      let decoded: DecodedFrame;
      try {
        decoded = decodeFrame(data);
      } catch (error) {
        activateFailure(candidate, `malformed frame error=${String(error)}`);
        candidate.close(1002, 'malformed thumbnail frame');
        return;
      }

      const entry = entries.get(decoded.hash);
      if (entry === undefined || entry.transport !== 'stream') return;
      if (decoded.jpeg === null) {
        finish(decoded.hash, entry, (callbacks) => {
          callbacks.onError?.(new Error(`thumbnail generation failed for ${decoded.hash}`));
        });
        return;
      }
      const blob = new Blob([decoded.jpeg], { type: 'image/jpeg' });
      finish(decoded.hash, entry, (callbacks) => callbacks.onBlob?.(blob));
    };

    const onError: EventListener = () => {
      activateFailure(candidate, 'socket error');
      candidate.close(1000, 'thumbnail stream error');
    };

    const onClose: EventListener = (event) => {
      const close = event as CloseEvent;
      const reason = close.reason ? ` reason=${JSON.stringify(close.reason)}` : '';
      const detail = `close code=${close.code}${reason}`;
      activateFailure(candidate, detail, close.code === 1000);
    };

    candidate.addEventListener('open', onOpen);
    candidate.addEventListener('message', onMessage);
    candidate.addEventListener('error', onError);
    candidate.addEventListener('close', onClose);
    detachSocketListeners = () => {
      candidate.removeEventListener('open', onOpen);
      candidate.removeEventListener('message', onMessage);
      candidate.removeEventListener('error', onError);
      candidate.removeEventListener('close', onClose);
    };

    openTimer = setTimer(() => {
      if (socket !== candidate || state !== 'connecting') return;
      activateFailure(candidate, 'open timeout');
      candidate.close(1000, 'thumbnail stream open timeout');
    }, OPEN_TIMEOUT_MS);
  }

  function request(hash: string, callbacks: ThumbCallbacks): CancelFn {
    if (!HASH_RE.test(hash)) {
      callbacks.onError?.(new Error('thumbnail hash must be 64 lowercase hexadecimal characters'));
      return () => undefined;
    }

    const subscriberId = nextSubscriberId++;
    let entry = entries.get(hash);
    if (entry === undefined) {
      entry = {
        subscribers: new Map(),
        transport: state === 'degraded' || (outageActive && state === 'connecting')
          ? 'http'
          : state === 'open'
            ? 'stream'
            : 'pending',
      };
      entries.set(hash, entry);
      entry.subscribers.set(subscriberId, callbacks);

      if (entry.transport === 'http') startHttp(hash, entry);
      else if (entry.transport === 'stream') sendWant(hash, entry);
      else connect();
    } else {
      entry.subscribers.set(subscriberId, callbacks);
    }

    let cancelled = false;
    return () => {
      if (cancelled) return;
      cancelled = true;
      const current = entries.get(hash);
      if (current === undefined || !current.subscribers.delete(subscriberId)) return;
      if (current.subscribers.size > 0) return;
      entries.delete(hash);

      if (current.transport === 'http') {
        current.cancelHttp?.();
      } else if (current.transport === 'stream') {
        const active = socket;
        if (active !== undefined && state === 'open' && active.readyState === SOCKET_OPEN) {
          try {
            active.send(`cancel ${hash}`);
            console.debug(`[thumb-stream] cancelled ${hash}`);
          } catch (error) {
            activateFailure(active, `cancel send error=${String(error)}`);
            active.close(1000, 'thumbnail stream cancel failed');
          }
        }
      }
    };
  }

  return { request };
}

export const thumbStream = createThumbStreamClient();
