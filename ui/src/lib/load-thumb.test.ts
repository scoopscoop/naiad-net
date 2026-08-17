import { afterEach, describe, expect, it, vi } from 'vitest';
import { loadThumb } from './load-thumb';
import { thumbStream } from './thumb-stream';

type LoadThumbHandle = {
  update?: (hash: string) => void;
  destroy: () => void;
};

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('loadThumb', () => {
  it('does not request a thumbnail for a tile destroyed before 125ms', () => {
    vi.useFakeTimers();
    const request = vi.spyOn(thumbStream, 'request');
    const img = document.createElement('img');
    const action = loadThumb(img, 'a'.repeat(64));
    vi.advanceTimersByTime(124);
    action.destroy();
    vi.advanceTimersByTime(1);
    expect(request).not.toHaveBeenCalled();
  });

  it('requests once after a tile remains mounted for 125ms', () => {
    vi.useFakeTimers();
    const cancel = vi.fn();
    const request = vi.spyOn(thumbStream, 'request').mockReturnValue(cancel);
    const img = document.createElement('img');
    const action = loadThumb(img, 'b'.repeat(64));
    vi.advanceTimersByTime(125);
    expect(request).toHaveBeenCalledOnce();
    expect(request).toHaveBeenCalledWith('b'.repeat(64), expect.anything());
    action.destroy();
    expect(cancel).toHaveBeenCalledOnce();
  });

  it('hash update cancels the old timer and only requests the replacement', () => {
    vi.useFakeTimers();
    const request = vi.spyOn(thumbStream, 'request').mockReturnValue(vi.fn());
    const img = document.createElement('img');
    const action = loadThumb(img, 'c'.repeat(64));
    vi.advanceTimersByTime(100);
    action.update?.('d'.repeat(64));
    vi.advanceTimersByTime(124);
    expect(request).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(request.mock.calls.map(([hash]) => hash)).toEqual(['d'.repeat(64)]);
    action.destroy();
  });

  it('cancels an active request before subscribing for an updated hash', () => {
    vi.useFakeTimers();
    const cancels: ReturnType<typeof vi.fn>[] = [];
    const request = vi.spyOn(thumbStream, 'request').mockImplementation(() => {
      const cancel = vi.fn();
      cancels.push(cancel);
      return cancel;
    });
    const img = document.createElement('img');
    const action = loadThumb(img, 'e'.repeat(64)) as LoadThumbHandle;

    vi.advanceTimersByTime(125);
    action.update?.('f'.repeat(64));
    expect(cancels[0]).toHaveBeenCalledOnce();
    expect(request).toHaveBeenCalledOnce();

    vi.advanceTimersByTime(125);
    expect(request.mock.calls.map(([hash]) => hash)).toEqual([
      'e'.repeat(64),
      'f'.repeat(64),
    ]);
    action.destroy();
    expect(cancels[1]).toHaveBeenCalledOnce();
  });

  it('ignores stale blobs and revokes the current blob after image load', () => {
    vi.useFakeTimers();
    const callbacks: Parameters<typeof thumbStream.request>[1][] = [];
    vi.spyOn(thumbStream, 'request').mockImplementation((_hash, nextCallbacks) => {
      callbacks.push(nextCallbacks);
      return vi.fn();
    });
    const createObjectURL = vi.fn()
      .mockReturnValueOnce('blob:stale')
      .mockReturnValueOnce('blob:current');
    const revokeObjectURL = vi.fn();
    vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL });
    const img = document.createElement('img');
    const action = loadThumb(img, '1'.repeat(64)) as LoadThumbHandle;

    vi.advanceTimersByTime(125);
    action.update?.('2'.repeat(64));
    vi.advanceTimersByTime(125);
    callbacks[0].onBlob?.(new Blob(['stale']));
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:stale');
    expect(img.hasAttribute('src')).toBe(false);

    callbacks[1].onBlob?.(new Blob(['current']));
    expect(img.src).toBe('blob:current');
    expect(img.classList.contains('loaded')).toBe(false);
    img.dispatchEvent(new Event('load'));
    expect(img.classList.contains('loaded')).toBe(true);
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:current');

    action.destroy();
    expect(img.classList.contains('loaded')).toBe(false);
    expect(img.hasAttribute('src')).toBe(false);
  });

  it('revokes a live blob when the tile is destroyed before image load', () => {
    vi.useFakeTimers();
    let onBlob: ((blob: Blob) => void) | undefined;
    vi.spyOn(thumbStream, 'request').mockImplementation((_hash, callbacks) => {
      onBlob = callbacks.onBlob;
      return vi.fn();
    });
    const revokeObjectURL = vi.fn();
    vi.stubGlobal('URL', {
      ...URL,
      createObjectURL: vi.fn(() => 'blob:live'),
      revokeObjectURL,
    });
    const img = document.createElement('img');
    const action = loadThumb(img, '3'.repeat(64));

    vi.advanceTimersByTime(125);
    onBlob?.(new Blob(['live']));
    action.destroy();

    expect(revokeObjectURL).toHaveBeenCalledWith('blob:live');
    expect(img.hasAttribute('src')).toBe(false);
  });

  it('destroy removes load/error listeners so a stale image event cannot invoke reveal or revoke', () => {
    vi.useFakeTimers();
    let capturedOnBlob: ((blob: Blob) => void) | undefined;
    vi.spyOn(thumbStream, 'request').mockImplementation((_hash, callbacks) => {
      capturedOnBlob = callbacks.onBlob;
      return vi.fn();
    });
    const createObjectURL = vi.fn(() => 'blob:destroy-test');
    const revokeObjectURL = vi.fn();
    vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL });
    const img = document.createElement('img');
    const removeListener = vi.spyOn(img, 'removeEventListener');
    const action = loadThumb(img, '6'.repeat(64));

    vi.advanceTimersByTime(125);
    capturedOnBlob?.(new Blob(['data']));
    // load and error listeners are now attached; blob URL is assigned to img.src.

    action.destroy();
    // destroy() → clear() must removeEventListener for 'load' and 'error', and
    // revoke the live blob.
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:destroy-test');
    expect(removeListener).toHaveBeenCalledWith('load', expect.any(Function));
    expect(removeListener).toHaveBeenCalledWith('error', expect.any(Function));

    // After destroy, a stale 'load' event must not re-trigger reveal or revoke.
    revokeObjectURL.mockClear();
    img.dispatchEvent(new Event('load'));
    expect(revokeObjectURL).not.toHaveBeenCalled();
    expect(img.classList.contains('loaded')).toBe(false);
  });

  it('reveals and revokes once when the image reports a terminal error', () => {
    vi.useFakeTimers();
    let onBlob: ((blob: Blob) => void) | undefined;
    vi.spyOn(thumbStream, 'request').mockImplementation((_hash, callbacks) => {
      onBlob = callbacks.onBlob;
      return vi.fn();
    });
    const revokeObjectURL = vi.fn();
    vi.stubGlobal('URL', {
      ...URL,
      createObjectURL: vi.fn(() => 'blob:error'),
      revokeObjectURL,
    });
    const img = document.createElement('img');
    const action = loadThumb(img, '4'.repeat(64));

    vi.advanceTimersByTime(125);
    onBlob?.(new Blob(['broken']));
    img.dispatchEvent(new Event('error'));

    expect(img.classList.contains('loaded')).toBe(true);
    expect(revokeObjectURL).toHaveBeenCalledOnce();
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:error');

    action.update?.('5'.repeat(64));
    expect(revokeObjectURL).toHaveBeenCalledOnce();
    action.destroy();
    expect(revokeObjectURL).toHaveBeenCalledOnce();
  });
});
