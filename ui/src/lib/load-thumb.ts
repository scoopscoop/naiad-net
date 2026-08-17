import { thumbUrl } from './api';
import { thumbStream } from './thumb-stream';
import { thumbQueue, type CancelFn, type ThumbCallbacks } from './thumb-queue';

const VIEWPORT_STABLE_MS = 125;

/**
 * Shared thumbnail action lifecycle. Transport and admission timing are bound
 * below so consumers cannot configure the grid's viewport-stability policy.
 *
 * Blob URLs are revoked after either terminal image event, or immediately when
 * an update/destroy clears an image that has not settled yet.
 */
function createLoadThumbAction(
  request: (hash: string, callbacks: ThumbCallbacks) => CancelFn,
  stableMs: number | null,
) {
  return function loadThumbAction(img: HTMLImageElement, hash: string) {
    let objectUrl: string | null = null;
    let cancel: CancelFn = () => {};
    let currentHash = '';
    let seq = 0;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let loadHandler: EventListener | null = null;
    let errorHandler: EventListener | null = null;

    const reveal = () => img.classList.add('loaded');

    function releaseObjectUrl() {
      const liveObjectUrl = objectUrl;
      objectUrl = null;
      if (liveObjectUrl) URL.revokeObjectURL(liveObjectUrl);
    }

    function clear() {
      if (timer !== null) clearTimeout(timer);
      timer = null;
      cancel();
      cancel = () => {};
      if (loadHandler !== null) {
        img.removeEventListener('load', loadHandler);
        loadHandler = null;
      }
      if (errorHandler !== null) {
        img.removeEventListener('error', errorHandler);
        errorHandler = null;
      }
      releaseObjectUrl();
      img.classList.remove('loaded');
      img.removeAttribute('src');
    }

    function start(nextHash: string) {
      currentHash = nextHash;
      const requestSeq = ++seq;
      const subscribe = () => {
        timer = null;
        cancel = request(nextHash, {
          onBlob: (blob) => {
            const nextObjectUrl = URL.createObjectURL(blob);
            if (requestSeq !== seq) {
              URL.revokeObjectURL(nextObjectUrl);
              return;
            }
            objectUrl = nextObjectUrl;
            // Revoke only after the browser has decoded the bytes; revoking before
            // `load` would blank the tile.
            loadHandler = () => {
              if (requestSeq !== seq) return;
              reveal();
              releaseObjectUrl();
            };
            errorHandler = () => {
              if (requestSeq !== seq) return;
              reveal();
              releaseObjectUrl();
            };
            img.addEventListener('load', loadHandler, { once: true });
            img.addEventListener('error', errorHandler, { once: true });
            img.src = objectUrl;
          },
          // A failed request settles the image on its placeholder rather than
          // hanging at opacity 0. No retry; a remount or hash update requests it again.
          onError: () => {
            if (requestSeq === seq) reveal();
          },
        });
      };
      if (stableMs === null) subscribe();
      else timer = setTimeout(subscribe, stableMs);
    }

    start(hash);

    return {
      update(nextHash: string) {
        if (nextHash === currentHash) return;
        clear();
        start(nextHash);
      },
      destroy() {
        seq += 1;
        clear();
      },
    };
  };
}

/** Grid action: subscribe through the stream only after 125 ms in the viewport. */
export const loadThumb = createLoadThumbAction(
  (hash, callbacks) => thumbStream.request(hash, callbacks),
  VIEWPORT_STABLE_MS,
);

/** Inspector action: preserve the previous immediate GET through the HTTP queue. */
export const loadThumbHttp = createLoadThumbAction(
  (hash, callbacks) => thumbQueue.request(thumbUrl(hash), callbacks),
  null,
);
