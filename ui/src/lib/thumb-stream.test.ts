import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  createThumbStreamClient,
  decodeFrame,
  streamUrl,
  type WebSocketLike,
} from './thumb-stream';
import type { CancelFn, ThumbCallbacks } from './thumb-queue';

class FakeWebSocket implements WebSocketLike {
  binaryType: BinaryType = 'blob';
  readyState: number = WebSocket.CONNECTING;
  sent: (string | ArrayBufferLike | Blob | ArrayBufferView)[] = [];
  closes: { code?: number; reason?: string }[] = [];
  private listeners = new Map<string, Set<EventListener>>();

  send(data: string | ArrayBufferLike | Blob | ArrayBufferView) {
    this.sent.push(data);
  }

  close(code?: number, reason?: string) {
    this.closes.push({ code, reason });
    this.readyState = WebSocket.CLOSING;
  }

  addEventListener(type: string, listener: EventListener) {
    const listeners = this.listeners.get(type) ?? new Set<EventListener>();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: EventListener) {
    this.listeners.get(type)?.delete(listener);
  }

  open() {
    this.readyState = WebSocket.OPEN;
    this.emit('open', new Event('open'));
  }

  message(data: unknown) {
    this.emit('message', new MessageEvent('message', { data }));
  }

  error() {
    this.emit('error', new Event('error'));
  }

  closed(code = 1006, reason = '') {
    this.readyState = WebSocket.CLOSED;
    this.emit('close', new CloseEvent('close', { code, reason }));
  }

  private emit(type: string, event: Event) {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

function frame(hash: string, jpeg: Uint8Array): ArrayBuffer {
  const bytes = new Uint8Array(36 + jpeg.byteLength);
  for (let index = 0; index < 32; index++) {
    bytes[index] = Number.parseInt(hash.slice(index * 2, index * 2 + 2), 16);
  }
  new DataView(bytes.buffer).setUint32(32, jpeg.byteLength, false);
  bytes.set(jpeg, 36);
  return bytes.buffer;
}

function setup(sockets = [new FakeWebSocket()]) {
  const cancelHttp = vi.fn<CancelFn>();
  const fallback = {
    request: vi.fn((_url: string, _callbacks: ThumbCallbacks) => cancelHttp),
  };
  const socketFactory = vi.fn(() => {
    const socket = sockets[socketFactory.mock.calls.length - 1];
    if (socket === undefined) throw new Error('no fake socket prepared');
    return socket;
  });
  const client = createThumbStreamClient({
    socketFactory,
    fallbackRequest: fallback.request,
  });
  return { client, fallback, cancelHttp, socketFactory };
}

describe('thumbnail stream frame', () => {
  it('decodes the raw hash, big-endian length, and JPEG payload', () => {
    const hash = '0a'.repeat(32);
    const decoded = decodeFrame(frame(hash, new Uint8Array([0xff, 0xd8])));
    expect(decoded.hash).toBe(hash);
    expect(decoded.jpeg).toEqual(new Uint8Array([0xff, 0xd8]));
  });

  it('returns null for a declared zero-length result', () => {
    expect(decodeFrame(frame('0b'.repeat(32), new Uint8Array())).jpeg).toBeNull();
  });

  it.each([
    ['short header', new ArrayBuffer(35)],
    ['mismatched length', (() => {
      const result = frame('0c'.repeat(32), new Uint8Array([1]));
      new DataView(result).setUint32(32, 2, false);
      return result;
    })()],
  ])('rejects a %s', (_label, input) => {
    expect(() => decodeFrame(input)).toThrow();
  });
});

describe('createThumbStreamClient', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    vi.spyOn(console, 'info').mockImplementation(() => undefined);
    vi.spyOn(console, 'debug').mockImplementation(() => undefined);
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it('derives the same-origin stream URL from the page location', () => {
    expect(streamUrl({ protocol: 'https:', host: 'naiad.test:8443' } as Location)).toBe(
      'wss://naiad.test:8443/thumb-stream',
    );
    expect(streamUrl({ protocol: 'http:', host: 'localhost:8080' } as Location)).toBe(
      'ws://localhost:8080/thumb-stream',
    );
  });

  it('rejects a non-canonical hash synchronously without opening a socket', () => {
    const socket = new FakeWebSocket();
    const { client, socketFactory } = setup([socket]);
    const onError = vi.fn();
    const cancel = client.request('AB'.repeat(32), { onError });
    expect(onError).toHaveBeenCalledOnce();
    expect(socketFactory).not.toHaveBeenCalled();
    expect(() => cancel()).not.toThrow();
  });

  it('sends one want for duplicate subscribers and one cancel after the last leaves', () => {
    const socket = new FakeWebSocket();
    const { client } = setup([socket]);
    const h = '01'.repeat(32);
    const a = client.request(h, {});
    const b = client.request(h, {});
    socket.open();
    expect(socket.sent).toEqual([`want ${h}`]);
    a();
    expect(socket.sent).toEqual([`want ${h}`]);
    b();
    expect(socket.sent).toEqual([`want ${h}`, `cancel ${h}`]);
  });

  it('sends nothing when the final subscriber cancels before initial open', () => {
    const socket = new FakeWebSocket();
    const { client } = setup([socket]);
    const cancel = client.request('08'.repeat(32), {});
    cancel();
    socket.open();
    expect(socket.sent).toEqual([]);
  });

  it('fans a valid binary result to every current subscriber', () => {
    const socket = new FakeWebSocket();
    const { client } = setup([socket]);
    const h = '02'.repeat(32);
    const one = vi.fn();
    const two = vi.fn();
    client.request(h, { onBlob: one });
    client.request(h, { onBlob: two });
    socket.open();
    socket.message(frame(h, new Uint8Array([0xff, 0xd8])));
    expect(one).toHaveBeenCalledOnce();
    expect(two).toHaveBeenCalledOnce();
    const blob = one.mock.calls[0][0] as Blob;
    expect(blob.type).toBe('image/jpeg');
    expect(blob.size).toBe(2);
  });

  it('isolates a throwing stream onBlob subscriber and delivers to later subscribers', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const h = '12'.repeat(32);
    const later = vi.fn();
    client.request(h, { onBlob: () => { throw new Error('subscriber failed'); } });
    client.request(h, { onBlob: later });
    socket.open();
    expect(() => socket.message(frame(h, new Uint8Array([1])))).not.toThrow();
    expect(later).toHaveBeenCalledOnce();
    expect(console.error).toHaveBeenCalledOnce();
    expect(fallback.request).not.toHaveBeenCalled();
  });

  it('isolates a throwing fallback onError subscriber and delivers to later subscribers', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const h = '13'.repeat(32);
    const later = vi.fn();
    client.request(h, { onError: () => { throw new Error('subscriber failed'); } });
    client.request(h, { onError: later });
    socket.closed(1006, 'lost');
    const callbacks = fallback.request.mock.calls[0][1];
    const failure = new Error('HTTP failed');
    expect(() => callbacks.onError?.(failure)).not.toThrow();
    expect(later).toHaveBeenCalledWith(failure);
    expect(console.error).toHaveBeenCalledOnce();
    expect(fallback.request).toHaveBeenCalledOnce();
  });

  it('ignores stale HTTP callbacks after a reentrant same-hash replacement', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const h = '14'.repeat(32);
    const oldLater = vi.fn();
    const replacementBlob = vi.fn();
    const replacementError = vi.fn();
    client.request(h, {
      onBlob: () => {
        client.request(h, { onBlob: replacementBlob, onError: replacementError });
        throw new Error('old subscriber failed after replacing entry');
      },
    });
    client.request(h, { onBlob: oldLater });
    socket.closed(1006, 'lost');

    const oldCallbacks = fallback.request.mock.calls[0][1];
    const oldBlob = new Blob(['old'], { type: 'image/jpeg' });
    expect(() => oldCallbacks.onBlob?.(oldBlob)).not.toThrow();
    expect(oldLater).toHaveBeenCalledWith(oldBlob);
    expect(console.error).toHaveBeenCalledOnce();
    expect(fallback.request).toHaveBeenCalledTimes(2);

    oldCallbacks.onError?.(new Error('late old transport callback'));
    expect(replacementBlob).not.toHaveBeenCalled();
    expect(replacementError).not.toHaveBeenCalled();
    expect(fallback.request).toHaveBeenCalledTimes(2);

    const replacementCallbacks = fallback.request.mock.calls[1][1];
    const replacementResult = new Blob(['replacement'], { type: 'image/jpeg' });
    replacementCallbacks.onBlob?.(replacementResult);
    expect(replacementBlob).toHaveBeenCalledOnce();
    expect(replacementBlob).toHaveBeenCalledWith(replacementResult);
    expect(replacementError).not.toHaveBeenCalled();
  });

  it('treats a zero-length frame as a per-item error without failing the stream', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const onError = vi.fn();
    const h = '03'.repeat(32);
    client.request(h, { onError });
    socket.open();
    socket.message(frame(h, new Uint8Array()));
    expect(onError).toHaveBeenCalledOnce();
    expect(fallback.request).not.toHaveBeenCalled();
    expect(console.warn).not.toHaveBeenCalled();
  });

  it('ignores a completed hash that has no outstanding request', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const wanted = '09'.repeat(32);
    client.request(wanted, {});
    socket.open();
    socket.message(frame('0a'.repeat(32), new Uint8Array([1])));
    expect(fallback.request).not.toHaveBeenCalled();
    expect(console.warn).not.toHaveBeenCalled();
  });

  it('moves every outstanding stream request to HTTP exactly once on unexpected close', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const h1 = '04'.repeat(32);
    const h2 = '05'.repeat(32);
    client.request(h1, {});
    client.request(h2, {});
    socket.open();
    socket.closed(1006, 'lost');
    socket.closed(1006, 'duplicate event');
    expect(fallback.request.mock.calls.map(([url]) => url).sort()).toEqual(
      [`/thumb/${h1}`, `/thumb/${h2}`].sort(),
    );
    expect(console.warn).toHaveBeenCalledOnce();
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('/thumb-stream'));
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('code=1006'));
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('outstanding=2'));
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('HTTP fallback activated'));
  });

  it('coalesces an error followed by close into one outage transition', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    client.request('0d'.repeat(32), {});
    socket.open();
    socket.error();
    socket.closed(1006, 'lost');
    expect(fallback.request).toHaveBeenCalledOnce();
    expect(console.warn).toHaveBeenCalledOnce();
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('HTTP fallback activated'));
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining(streamUrl()));
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('outstanding=1'));
  });

  it.each([
    ['truncated header', new ArrayBuffer(35)],
    ['wrong declared length', (() => {
      const result = frame('0e'.repeat(32), new Uint8Array([1]));
      new DataView(result).setUint32(32, 99, false);
      return result;
    })()],
  ])('activates fallback once for a malformed %s frame', (_label, malformed) => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    client.request('0e'.repeat(32), {});
    socket.open();
    socket.message(malformed);
    socket.closed(1002, 'duplicate protocol close');
    expect(socket.closes).toEqual([{ code: 1002, reason: 'malformed thumbnail frame' }]);
    expect(fallback.request).toHaveBeenCalledOnce();
    expect(console.warn).toHaveBeenCalledOnce();
  });

  it('moves pending requests to HTTP when the opening timeout expires', () => {
    const socket = new FakeWebSocket();
    const { client, fallback } = setup([socket]);
    const hash = '0f'.repeat(32);
    client.request(hash, {});
    vi.advanceTimersByTime(1_500);
    expect(fallback.request).toHaveBeenCalledWith(`/thumb/${hash}`, expect.anything());
    expect(socket.closes).toEqual([{ code: 1000, reason: 'thumbnail stream open timeout' }]);
    expect(console.warn).toHaveBeenCalledOnce();
  });

  it('keeps fallback work on HTTP and uses a recovered socket only for future hashes', () => {
    const first = new FakeWebSocket();
    const second = new FakeWebSocket();
    const { client, fallback } = setup([first, second]);
    const oldHash = '06'.repeat(32);
    const newHash = '07'.repeat(32);
    const secondHealthyHash = '17'.repeat(32);
    client.request(oldHash, {});
    first.open();
    first.closed(1006, 'lost');
    vi.advanceTimersByTime(1_000);
    second.open();
    client.request(newHash, {});
    client.request(secondHealthyHash, {});
    expect(fallback.request).toHaveBeenCalledWith(`/thumb/${oldHash}`, expect.anything());
    expect(second.sent).toEqual([`want ${newHash}`, `want ${secondHealthyHash}`]);
    expect(console.info).toHaveBeenCalledOnce();
    expect(console.info).toHaveBeenCalledWith(expect.stringContaining(streamUrl()));
    expect(console.info).toHaveBeenCalledWith(expect.stringContaining('recovered'));
    expect(console.info).toHaveBeenCalledWith(
      expect.stringContaining('future thumbnails use the stream'),
    );
  });

  it('uses bounded retry delays capped at thirty seconds', () => {
    const sockets = Array.from({ length: 7 }, () => new FakeWebSocket());
    const { client, socketFactory } = setup(sockets);
    client.request('10'.repeat(32), {});
    sockets[0].closed(1006, 'lost');
    const delays = [1_000, 2_000, 5_000, 10_000, 30_000, 30_000];
    delays.forEach((delay, index) => {
      vi.advanceTimersByTime(delay - 1);
      expect(socketFactory).toHaveBeenCalledTimes(index + 1);
      vi.advanceTimersByTime(1);
      expect(socketFactory).toHaveBeenCalledTimes(index + 2);
      sockets[index + 1].closed(1006, 'still down');
      expect(console.warn).toHaveBeenCalledOnce();
    });
    expect(console.warn).toHaveBeenCalledOnce();
  });

  it('routes a new request to HTTP when outage is active and socket is reconnecting', () => {
    // Covers the (outageActive && state === 'connecting') transport branch in
    // request(): after an outage the retry timer fires and connect() is called
    // (state = 'connecting'), but before the new socket opens any fresh
    // request() must go straight to HTTP and must NOT send a want when the
    // socket later opens.
    const first = new FakeWebSocket();
    const second = new FakeWebSocket();
    const { client, fallback } = setup([first, second]);

    const backlogHash = '20'.repeat(32);
    const newHash = '21'.repeat(32);

    // Establish outage: request, open, then unexpected close.
    client.request(backlogHash, {});
    first.open();
    first.closed(1006, 'connection lost');
    // outageActive=true, state='degraded'; backlogHash migrated to HTTP.
    expect(fallback.request).toHaveBeenCalledTimes(1);

    // Advance past the first retry delay so a new socket is constructed
    // (state='connecting') but not yet opened.
    vi.advanceTimersByTime(1_000);
    // socketFactory has been called twice; second socket exists but is CONNECTING.

    // New request during the reconnect window: outageActive && state === 'connecting'
    client.request(newHash, {});
    expect(fallback.request).toHaveBeenCalledTimes(2);
    expect(fallback.request).toHaveBeenCalledWith(`/thumb/${newHash}`, expect.anything());

    // When the socket opens, no want must be sent for newHash (already on HTTP).
    second.open();
    expect(second.sent).not.toContain(`want ${newHash}`);
    expect(second.sent).toEqual([]);
  });

  it('cancelling the final fallback subscriber calls its HTTP cancel function', () => {
    const socket = new FakeWebSocket();
    const { client, cancelHttp } = setup([socket]);
    const hash = '11'.repeat(32);
    const first = client.request(hash, {});
    const second = client.request(hash, {});
    socket.closed(1006, 'lost');
    first();
    expect(cancelHttp).not.toHaveBeenCalled();
    second();
    expect(cancelHttp).toHaveBeenCalledOnce();
  });
});
