/** The page the desktop shell opens *immediately* at launch, before the daemon
 *  exists (#48). Once the daemon binds an address the shell navigates this same
 *  window to it, discarding this page — which is why nothing here is shared with
 *  the app: no Svelte, no app.css, no network fetches.
 *
 *  Ordering matters. The daemon can die before this module ever executes, so we
 *  read the shell's authoritative state first and only then subscribe. An event
 *  emitted into the void would otherwise leave the page spinning forever. */

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';

type DaemonState =
  | { kind: 'starting'; lines: string[]; seq: number }
  | { kind: 'ready'; addr: string }
  | { kind: 'failed'; message: string; lines: string[] };

const root = document.getElementById('root') as HTMLDivElement;
const status = document.getElementById('status') as HTMLParagraphElement;
const quit = document.getElementById('quit') as HTMLButtonElement;
const bar = document.querySelector('.bar') as HTMLDivElement;

function close() {
  try {
    // The rejection matters as much as the throw: the IPC may be gone, or the
    // window already destroyed, by the time close() settles.
    void getCurrentWindow()
      .close()
      .catch(() => {});
  } catch {
    // not running inside a Tauri window
  }
}

quit.addEventListener('click', close);

/** The `seq` of the newest line drawn. The shell numbers every line it buffers,
 *  so a buffered read that resolves after a fresher `daemon://line` is simply
 *  older and gets dropped, rather than rewinding the status text by one line. */
let shownSeq = 0;

function showLine(text: string, seq: number) {
  if (seq <= shownSeq) return;
  shownSeq = seq;
  status.textContent = text;
}

/** Draw a determinate progress fraction. Shares `shownSeq` with showLine so the
 *  raw `naiad-startup ...` line for the same seq (emitted right after this event)
 *  is suppressed, and a stale progress event that lost a race is dropped. */
function showProgress(step: number, total: number, label: string, seq: number) {
  if (seq <= shownSeq) return;
  shownSeq = seq;
  status.textContent = label;
  if (total > 0) {
    const pct = Math.max(0, Math.min(1, step / total)) * 100;
    bar.classList.add('determinate');
    bar.style.setProperty('--fill', `${pct}%`);
    bar.setAttribute('aria-valuenow', String(Math.round(pct)));
  }
}

function showError(message: string, lines: string[]) {
  const details = lines.join('\n');
  root.replaceChildren();
  root.insertAdjacentHTML(
    'beforeend',
    `<div class="error">
       <h1></h1>
       <pre class="lines"></pre>
       <div class="actions">
         <button type="button" id="copy">Copy details</button>
         <button type="button" id="close">Quit</button>
       </div>
     </div>`,
  );
  // textContent, not innerHTML: daemon output is untrusted text.
  root.querySelector('h1')!.textContent = message;
  root.querySelector('.lines')!.textContent = details || '(no daemon output)';

  const copy = root.querySelector<HTMLButtonElement>('#copy')!;
  copy.addEventListener('click', () => {
    // tauri://localhost is a secure context, so the async clipboard API is
    // available without the Tauri clipboard plugin. It can still reject on lost
    // focus or a denied permission, and this button is the only way the daemon's
    // output leaves a failed startup — so say which of the two happened.
    const done = (label: string) => {
      copy.textContent = label;
    };
    try {
      void navigator.clipboard
        .writeText(`${message}\n\n${details}`)
        .then(() => done('Copied'))
        .catch(() => done('Copy failed'));
    } catch {
      done('Copy failed');
    }
  });
  root.querySelector<HTMLButtonElement>('#close')!.addEventListener('click', close);
}

/** Mirror of the Rust `parse_startup_progress` in ui/src-tauri/src/lib.rs.
 *  Parses `"naiad-startup <step>/<total> <label>"` lines. Returns `null` for
 *  any line without the prefix, non-positive-integer step/total, step > total,
 *  or an empty trimmed label. */
function parseStartupProgress(
  text: string,
): { step: number; total: number; label: string } | null {
  const rest = text.trim();
  if (!rest.startsWith('naiad-startup ')) return null;
  const body = rest.slice('naiad-startup '.length);
  const spaceIdx = body.indexOf(' ');
  if (spaceIdx === -1) return null;
  const count = body.slice(0, spaceIdx);
  const label = body.slice(spaceIdx + 1).trim();
  if (label.length === 0) return null;
  const slashIdx = count.indexOf('/');
  if (slashIdx === -1) return null;
  const stepStr = count.slice(0, slashIdx);
  const totalStr = count.slice(slashIdx + 1);
  const step = Number(stepStr);
  const total = Number(totalStr);
  if (
    !Number.isInteger(step) ||
    !Number.isInteger(total) ||
    step <= 0 ||
    total <= 0 ||
    step > total
  )
    return null;
  return { step, total, label };
}

let settled = false;

function fail(message: string, lines: string[]) {
  if (settled) return;
  settled = true;
  showError(message, lines);
}

/** Returns true when the state was terminal and has been rendered. */
async function readState(): Promise<boolean> {
  let state: DaemonState;
  try {
    state = await invoke<DaemonState>('daemon_state');
  } catch {
    // No shell (a plain `vite dev` visit) or the command is not registered yet.
    // Staying in the loading state is the right degraded behaviour.
    return false;
  }
  if (state.kind === 'failed') {
    fail(state.message, state.lines);
    return true;
  }
  if (state.kind === 'starting') {
    // The shell buffered whatever the daemon printed before this page could
    // subscribe. `seq` counts every line ever buffered, so the tail's own seq
    // is `seq`, and showLine drops it if a live event already drew something
    // newer while this invoke was in flight.
    const last = state.lines.at(-1);
    if (last !== undefined) {
      const p = parseStartupProgress(last);
      if (p) showProgress(p.step, p.total, p.label, state.seq);
      else showLine(last, state.seq);
    }
  }
  // `ready` needs no handling: the shell has already called navigate() and this
  // document is about to be replaced.
  return state.kind === 'ready';
}

async function main() {
  if (await readState()) return;

  try {
    await listen<{ step: number; total: number; label: string; seq: number }>(
      'daemon://progress',
      (e) => {
        if (!settled) showProgress(e.payload.step, e.payload.total, e.payload.label, e.payload.seq);
      },
    );
    await listen<{ stream: string; text: string; seq: number }>('daemon://line', (e) => {
      if (!settled) {
        const p = parseStartupProgress(e.payload.text);
        if (p) showProgress(p.step, p.total, p.label, e.payload.seq);
        else showLine(e.payload.text, e.payload.seq);
      }
    });
    await listen<{ message: string; lines: string[] }>('daemon://fatal', (e) => {
      fail(e.payload.message, e.payload.lines);
    });
  } catch {
    // No Tauri IPC (a plain `vite dev` visit). Staying in the loading state is
    // the right degraded behaviour; the page just won't receive daemon events.
  }

  // Close the gap: a fatal emitted between the first read and the listen above
  // reached no listener. The state is authoritative, so ask again. `settled`
  // makes the two paths idempotent. Unconditional: readState() has its own
  // catch and degrades quietly when IPC is unavailable.
  await readState();
}

void main();
