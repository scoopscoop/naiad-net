use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{
    Emitter, LogicalSize, Manager, PhysicalPosition, RunEvent, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// How long the daemon sidecar may stay *silent* before we give up. Any output
/// (stdout or stderr) resets the clock: a schema migration can take minutes on
/// a large library, and the daemon heartbeats to stderr while it runs — killing
/// it mid-migration rolls the migration back, to be retried and killed again on
/// every launch. Silence for this long means hung or dead. Still generous
/// enough to survive a first-run Defender/SmartScreen scan of a freshly built,
/// unsigned `naiad.exe` (which stalls process start before any output).
const DAEMON_READY_TIMEOUT_SECS: u64 = 30;
const DEFAULT_WINDOW_WIDTH: f64 = 1200.0;
const DEFAULT_WINDOW_HEIGHT: f64 = 800.0;
const MIN_WINDOW_WIDTH: u32 = 700;
const MIN_WINDOW_HEIGHT: u32 = 500;
const MAX_WINDOW_WIDTH: u32 = 10000;
const MAX_WINDOW_HEIGHT: u32 = 10000;
const MAX_WINDOW_COORD_ABS: i32 = 100000;
/// The height of `TitleBar`'s `data-tauri-drag-region` header, in logical pixels.
const DRAG_STRIP_HEIGHT: u32 = 48;
/// How much of that strip must stay on one display for the window to be movable
/// by pointer. Roughly the brand mark plus a comfortable pointer target.
const MIN_GRAB_WIDTH: u32 = 120;
const MIN_GRAB_HEIGHT: u32 = 24;
/// Windows parks a minimized window at (-32000, -32000). A state file written by
/// an older build (or by hand) can still carry it, and restoring it would strand
/// the window offscreen — so it is rejected on read as well as never written.
const MINIMIZED_SENTINEL_COORD: i32 = -32000;
const WINDOW_STATE_FILE: &str = "window-state.json";
/// Bumped when the meaning of a persisted field changes. v2 moved `x`/`y` from
/// logical to physical pixels.
const WINDOW_STATE_VERSION: u32 = 2;
const VIEW_STATE_FILE: &str = "view-state.json";
/// How long window motion must stop before the coalesced write lands.
const WINDOW_STATE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Holds the daemon sidecar process so it can be killed on app exit.
struct DaemonChild(Mutex<Option<CommandChild>>);

/// How many lines of daemon output the error page gets to show.
const RING_CAP: usize = 20;

/// What the loading page receives from `daemon_state`. Built at the command
/// boundary: `Starting`'s output tail lives in the `LineBuffer`, not in the
/// stored `Status`, so there is no second copy for anyone to trust.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum DaemonState {
    Starting { lines: Vec<String>, seq: u64 },
    Ready { addr: String },
    Failed { message: String, lines: Vec<String> },
}

/// What the shell knows about the daemon, authoritatively.
///
/// The loading page cannot rely on events alone: the daemon can fail before the
/// webview finishes loading and attaches its listeners, and an event emitted
/// into the void would leave the page spinning forever. So the page reads this
/// through the `daemon_state` command, and events are only an optimisation.
#[derive(Clone, Debug)]
enum Status {
    Starting,
    Ready { addr: String },
    Failed { message: String, lines: Vec<String> },
}

/// The tail of the daemon's output and the count of everything it ever printed.
///
/// One struct behind one lock, not two: `seq` must name the last line *present*
/// in `lines`. Split across two mutexes, a line landing between the two reads is
/// counted but not returned, and the loading page then discards the live event
/// for it — leaving a stale line on screen for a daemon that goes quiet, which
/// is exactly what `seq` exists to prevent.
#[derive(Default)]
struct LineBuffer {
    lines: VecDeque<String>,
    /// Total lines ever buffered, never reset. The loading page renders a line
    /// only when its `seq` exceeds the last one it drew, so a buffered read
    /// that resolves after a fresher `daemon://line` cannot rewind the display.
    emitted: u64,
}

impl LineBuffer {
    /// Push a line, evicting the oldest once the buffer is full. Returns the
    /// `seq` of the line just pushed.
    fn push(&mut self, line: String) -> u64 {
        if self.lines.len() == RING_CAP {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
        self.emitted += 1;
        self.emitted
    }

    fn tail(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

/// Managed state: the daemon's status plus the tail of its output.
struct DaemonStatus {
    state: Mutex<Status>,
    buffer: Mutex<LineBuffer>,
}

/// Saved window bounds. The size is in **logical** pixels: that keeps the size
/// floor meaningful on a scaled display, and restores the size the user left
/// across a DPI change. The position is in **physical** pixels, because it is a
/// global desktop coordinate shared by every monitor — on a mixed-DPI desktop no
/// single scale factor converts it, and it is the space monitor geometry is
/// reported in.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct PersistedWindowState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// The on-disk shape. `version` gates the units of `x`/`y`, which changed from
/// logical to physical pixels: a v1 file read as v2 would misplace the window on
/// a scaled display, so an absent or unknown version is discarded rather than
/// reinterpreted.
#[derive(Deserialize, Serialize)]
struct WindowStateFile {
    version: u32,
    #[serde(flatten)]
    bounds: PersistedWindowState,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct PersistedViewState {
    #[serde(default)]
    inspector_collapsed: Option<bool>,
    /// Gallery zoom. Since #171 this holds the thumbs-per-row level (2..16);
    /// older installs left a pixel tile size (80..1024) here, which the
    /// frontend converts on load. The shell just round-trips the number.
    #[serde(default)]
    tile: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
struct ViewStatePayload {
    inspector_collapsed: Option<bool>,
    tile: Option<u32>,
}

/// Serializes the view-state file's read-modify-write. Tauri runs synchronous
/// commands on a thread pool, and the frontend fires its saves without ordering
/// them, so two rapid toggles could otherwise both read the pre-toggle file and
/// let the slower write win.
#[derive(Default)]
struct ViewStateLock(Mutex<()>);

impl PersistedWindowState {
    fn is_sane(&self) -> bool {
        (MIN_WINDOW_WIDTH..=MAX_WINDOW_WIDTH).contains(&self.width)
            && (MIN_WINDOW_HEIGHT..=MAX_WINDOW_HEIGHT).contains(&self.height)
            && (-MAX_WINDOW_COORD_ABS..=MAX_WINDOW_COORD_ABS).contains(&self.x)
            && (-MAX_WINDOW_COORD_ABS..=MAX_WINDOW_COORD_ABS).contains(&self.y)
            && !(self.x == MINIMIZED_SENTINEL_COORD && self.y == MINIMIZED_SENTINEL_COORD)
    }
}

/// Whether enough of the window's drag strip lands on one of `monitors` for the
/// user to grab it, all as `(x, y, width, height)` in physical pixels. A sliver
/// of window hanging onto a display is visible but not recoverable: with
/// `decorations(false)` the only way to move the window is the custom strip
/// along its top edge, so the strip is what must be reachable — on a *single*
/// monitor, since a pointer cannot press two halves of a seam at once.
///
/// Empty `monitors` counts as "cannot tell", so: yes.
fn drag_strip_is_grabbable(rect: (i32, i32, u32, u32), monitors: &[(i32, i32, u32, u32)]) -> bool {
    if monitors.is_empty() {
        return true;
    }
    let (x, y, width, height) = rect;
    let strip = (x, y, width, height.min(DRAG_STRIP_HEIGHT));
    let need_width = MIN_GRAB_WIDTH.min(strip.2);
    let need_height = MIN_GRAB_HEIGHT.min(strip.3);
    let overlap = |a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)| {
        let span = |a0: i32, a_len: u32, b0: i32, b_len: u32| {
            let (a0, b0) = (a0 as i64, b0 as i64);
            let lo = a0.max(b0);
            let hi = (a0 + a_len as i64).min(b0 + b_len as i64);
            (hi - lo).max(0) as u64
        };
        (span(a.0, a.2, b.0, b.2), span(a.1, a.3, b.1, b.3))
    };
    monitors.iter().any(|m| {
        let (w, h) = overlap(strip, *m);
        w >= need_width as u64 && h >= need_height as u64
    })
}

fn window_state_path(handle: &tauri::AppHandle) -> Option<PathBuf> {
    handle
        .path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(WINDOW_STATE_FILE))
}

fn load_window_state(path: &Path) -> Option<PersistedWindowState> {
    let text = std::fs::read_to_string(path).ok()?;
    let file = serde_json::from_str::<WindowStateFile>(&text).ok()?;
    if file.version != WINDOW_STATE_VERSION {
        return None;
    }
    file.bounds.is_sane().then_some(file.bounds)
}

fn write_window_state(path: &Path, state: PersistedWindowState) -> std::io::Result<()> {
    if !state.is_sane() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to persist implausible window bounds",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = WindowStateFile {
        version: WINDOW_STATE_VERSION,
        bounds: state,
    };
    let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// The window's current bounds, or `None` when they must not be persisted.
///
/// A minimized window reports the OS's parking position rather than where the
/// user left it, so it is skipped: saving it would strand the window offscreen
/// on the next launch.
fn current_window_state(window: &WebviewWindow) -> Option<PersistedWindowState> {
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    let position = window.outer_position().ok()?;
    if position.x == MINIMIZED_SENTINEL_COORD && position.y == MINIMIZED_SENTINEL_COORD {
        return None;
    }
    let scale = window.scale_factor().ok()?;
    let size = window.inner_size().ok()?.to_logical::<f64>(scale);
    let state = PersistedWindowState {
        x: position.x,
        y: position.y,
        width: size.width.round() as u32,
        height: size.height.round() as u32,
    };
    state.is_sane().then_some(state)
}

/// Coalesces window-state writes onto a background thread.
///
/// `Resized` and `Moved` fire continuously throughout a drag, so writing on each
/// would put hundreds of synchronous `fs::write` calls on the UI thread. Motion
/// only records the latest bounds and nudges the writer, which waits for the
/// gesture to stop before touching the disk. Teardown writes inline instead: the
/// process may be gone before a background thread gets its turn.
struct WindowStateSaver {
    path: PathBuf,
    pending: Arc<Mutex<Option<PersistedWindowState>>>,
    wake: std::sync::mpsc::Sender<()>,
    closing: Arc<AtomicBool>,
}

impl WindowStateSaver {
    fn new(path: PathBuf) -> Self {
        let (wake, rx) = std::sync::mpsc::channel::<()>();
        let pending: Arc<Mutex<Option<PersistedWindowState>>> = Arc::new(Mutex::new(None));
        let closing = Arc::new(AtomicBool::new(false));

        let writer_path = path.clone();
        let writer_pending = Arc::clone(&pending);
        let writer_closing = Arc::clone(&closing);
        std::thread::spawn(move || {
            while rx.recv().is_ok() {
                // Every further nudge restarts the wait, so the disk is touched
                // once the gesture has been still for the debounce interval.
                while rx.recv_timeout(WINDOW_STATE_DEBOUNCE).is_ok() {}
                // The slot is held across the write: a teardown flush must not
                // interleave and then be overwritten by this staler value.
                let mut slot = writer_pending.lock().unwrap();
                if writer_closing.load(Ordering::SeqCst) {
                    continue;
                }
                if let Some(state) = slot.take() {
                    if let Err(e) = write_window_state(&writer_path, state) {
                        eprintln!("naiad: could not save window state: {e}");
                    }
                }
            }
        });

        Self {
            path,
            pending,
            wake,
            closing,
        }
    }

    /// Record the bounds a motion event just produced. Never touches the disk.
    fn record(&self, window: &WebviewWindow) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        let Some(state) = current_window_state(window) else {
            return;
        };
        *self.pending.lock().unwrap() = Some(state);
        let _ = self.wake.send(());
    }

    /// Write the final bounds inline and retire the background writer. Falls back
    /// to the last recorded bounds when the window is closed while minimized.
    ///
    /// Close raises both `CloseRequested` and `Destroyed`; only the first call
    /// writes. The second would read a window mid-teardown and overwrite good
    /// bounds with whatever it reported.
    fn flush(&self, window: &WebviewWindow) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        let live = current_window_state(window);
        let mut slot = self.pending.lock().unwrap();
        let state = live.or_else(|| slot.take());
        *slot = None;
        let Some(state) = state else {
            return;
        };
        if let Err(e) = write_window_state(&self.path, state) {
            eprintln!("naiad: could not save window state: {e}");
        }
    }
}

/// Whether the saved rect reopens somewhere the user can still reach it. A
/// window left on a since-disconnected monitor would come back offscreen, or
/// with a sliver on a display too thin to grab, and with `decorations(false)`
/// there is no titlebar to drag it back.
///
/// The rect and the monitors share the physical coordinate space, so no scaling
/// is involved. The saved size is logical, and a logical extent never exceeds
/// the physical one, so using it here only makes the check stricter.
fn window_state_is_reachable(window: &WebviewWindow, state: PersistedWindowState) -> bool {
    let Ok(monitors) = window.available_monitors() else {
        return true;
    };
    let rect = (state.x, state.y, state.width, state.height);
    let screens: Vec<_> = monitors
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width, s.height)
        })
        .collect();
    drag_strip_is_grabbable(rect, &screens)
}

fn apply_window_state(window: &WebviewWindow, state: PersistedWindowState) {
    if let Err(e) = window.set_size(LogicalSize::new(state.width, state.height)) {
        eprintln!("naiad: could not restore window size: {e}");
    }
    if !window_state_is_reachable(window, state) {
        if let Err(e) = window.center() {
            eprintln!("naiad: could not center the window: {e}");
        }
        return;
    }
    if let Err(e) = window.set_position(PhysicalPosition::new(state.x, state.y)) {
        eprintln!("naiad: could not restore window position: {e}");
    }
}

fn view_state_path(handle: &tauri::AppHandle) -> Option<PathBuf> {
    handle
        .path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(VIEW_STATE_FILE))
}

fn load_view_state_from_path(path: &Path) -> PersistedViewState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<PersistedViewState>(&text).ok())
        .unwrap_or_default()
}

fn write_view_state_to_path(path: &Path, state: &PersistedViewState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Read, mutate, write — as one critical section, so the last toggle is the one
/// left on disk no matter how the pool interleaves two concurrent commands.
fn set_inspector_collapsed(
    lock: &ViewStateLock,
    path: &Path,
    collapsed: bool,
) -> std::io::Result<()> {
    let _guard = lock.0.lock().unwrap();
    let mut state = load_view_state_from_path(path);
    state.inspector_collapsed = Some(collapsed);
    write_view_state_to_path(path, &state)
}

/// Same read-modify-write discipline as [`set_inspector_collapsed`], so the
/// gallery zoom level persists across restarts alongside the inspector state.
fn set_tile(lock: &ViewStateLock, path: &Path, tile: u32) -> std::io::Result<()> {
    let _guard = lock.0.lock().unwrap();
    let mut state = load_view_state_from_path(path);
    state.tile = Some(tile);
    write_view_state_to_path(path, &state)
}

/// `Starting -> Ready`. Returns false if the state was already terminal.
fn mark_ready(state: &Mutex<Status>, addr: &str) -> bool {
    let mut s = state.lock().unwrap();
    if matches!(*s, Status::Starting) {
        *s = Status::Ready {
            addr: addr.to_string(),
        };
        true
    } else {
        false
    }
}

/// `Starting -> Failed`. Returns false if the state was already terminal — a
/// daemon that exits after the window navigated is not a startup failure, and a
/// second failure is less informative than the first.
fn mark_failed(state: &Mutex<Status>, message: &str, lines: Vec<String>) -> bool {
    let mut s = state.lock().unwrap();
    if matches!(*s, Status::Starting) {
        *s = Status::Failed {
            message: message.to_string(),
            lines,
        };
        true
    } else {
        false
    }
}

/// The loading page calls this on mount, before subscribing to events, and once
/// again after — closing the window in which a fatal could be emitted to nobody.
///
/// `Starting` carries the tail of the output the daemon has already produced.
/// The shell starts buffering at spawn, long before the webview attaches its
/// `daemon://line` listener, so without this the page would show nothing for a
/// daemon that printed its progress and then went quiet.
#[tauri::command]
fn daemon_state(status: tauri::State<'_, DaemonStatus>) -> DaemonState {
    let (buffered, seq) = {
        let buf = status.buffer.lock().unwrap();
        (buf.tail(), buf.emitted)
    };
    let state = status.state.lock().unwrap().clone();
    match state {
        Status::Starting => DaemonState::Starting {
            lines: buffered,
            seq,
        },
        Status::Ready { addr } => DaemonState::Ready { addr },
        Status::Failed { message, lines } => DaemonState::Failed { message, lines },
    }
}

#[tauri::command]
fn load_view_state(
    handle: tauri::AppHandle,
    lock: tauri::State<'_, ViewStateLock>,
) -> ViewStatePayload {
    let _guard = lock.0.lock().unwrap();
    let state = view_state_path(&handle)
        .as_deref()
        .map(load_view_state_from_path)
        .unwrap_or_default();
    ViewStatePayload {
        inspector_collapsed: state.inspector_collapsed,
        tile: state.tile,
    }
}

#[tauri::command]
fn save_inspector_collapsed(
    handle: tauri::AppHandle,
    lock: tauri::State<'_, ViewStateLock>,
    collapsed: bool,
) -> Result<(), String> {
    let Some(path) = view_state_path(&handle) else {
        return Ok(());
    };
    set_inspector_collapsed(&lock, &path, collapsed)
        .map_err(|e| format!("could not save view state: {e}"))
}

#[tauri::command]
fn save_tile(
    handle: tauri::AppHandle,
    lock: tauri::State<'_, ViewStateLock>,
    tile: u32,
) -> Result<(), String> {
    let Some(path) = view_state_path(&handle) else {
        return Ok(());
    };
    set_tile(&lock, &path, tile).map_err(|e| format!("could not save view state: {e}"))
}

/// Extract the `host:port` authority from the daemon's startup line.
///
/// `"naiad daemon on http://127.0.0.1:54321"` -> `Some("127.0.0.1:54321")`.
/// Returns `None` for any line without the marker, or whose authority is empty,
/// has no port colon, or contains whitespace.
fn parse_bound_addr(line: &str) -> Option<String> {
    let authority = line.trim().strip_prefix("naiad daemon on http://")?.trim();
    if authority.is_empty()
        || !authority.contains(':')
        || authority.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(authority.to_string())
}

/// One parsed startup-progress line: `naiad-startup <step>/<total> <label>`.
struct StartupProgress {
    step: u32,
    total: u32,
    label: String,
}

/// Parse a daemon startup-progress line into `{ step, total, label }`.
///
/// `"naiad-startup 3/5 opening read pool"` -> `Some { step: 3, total: 5, label:
/// "opening read pool" }`. Returns `None` for any line without the
/// `naiad-startup ` prefix, a non-positive-integer `step`/`total`, a `step`
/// that exceeds `total`, or an empty label. The counterpart of
/// `parse_bound_addr`, kept format-locked with the daemon's
/// `startup_progress_line` via the daemon's format-pinning test.
fn parse_startup_progress(line: &str) -> Option<StartupProgress> {
    let rest = line.trim().strip_prefix("naiad-startup ")?;
    let (count, label) = rest.split_once(' ')?;
    let (step_s, total_s) = count.split_once('/')?;
    let step: u32 = step_s.parse().ok()?;
    let total: u32 = total_s.parse().ok()?;
    if step == 0 || total == 0 || step > total {
        return None;
    }
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    Some(StartupProgress {
        step,
        total,
        label: label.to_string(),
    })
}

/// Print one line of daemon output with the `[daemon]` prefix, mirroring the
/// stream it came from (our stdout for the daemon's stdout, stderr for stderr).
fn relay_daemon_line(bytes: &[u8], is_stderr: bool) {
    let line = String::from_utf8_lossy(bytes);
    let line = line.trim_end();
    if is_stderr {
        eprintln!("[daemon] {line}");
    } else {
        println!("[daemon] {line}");
    }
}

/// One line of daemon output, mirrored to the loading page verbatim. The raw
/// text is deliberate: the daemon's own words ("still opening database...")
/// tell the user more than any rephrasing we could invent.
#[derive(Clone, Serialize)]
struct LinePayload {
    stream: &'static str,
    text: String,
    /// Monotonic, matching `DaemonState::Starting.seq`. Lets the page discard a
    /// buffered read that lost a race with a newer event.
    seq: u64,
}

/// A parsed startup-progress milestone, mirrored to the loading page so it can
/// draw a determinate bar. Emitted on `daemon://progress` in addition to the
/// raw `daemon://line`. `seq` reuses `LinePayload`'s monotonic counter so the
/// page can discard a stale progress event that lost a race.
#[derive(Clone, Serialize)]
struct ProgressPayload {
    step: u32,
    total: u32,
    label: String,
    seq: u64,
}

#[derive(Clone, Serialize)]
struct FatalPayload {
    message: String,
    lines: Vec<String>,
}

/// Record a startup failure and tell the loading page. Does **not** exit: the
/// window is already open, and an in-window error state beats a native dialog
/// that leaves the user with nothing behind it.
fn fail_startup(handle: &tauri::AppHandle, message: &str) {
    let status = handle.state::<DaemonStatus>();
    let lines = status.buffer.lock().unwrap().tail();
    if mark_failed(&status.state, message, lines.clone()) {
        let _ = handle.emit(
            "daemon://fatal",
            FatalPayload {
                message: message.to_string(),
                lines,
            },
        );
    }
}

/// Relay a daemon line to the console, the ring buffer, and the loading page.
fn record_daemon_line(handle: &tauri::AppHandle, bytes: &[u8], is_stderr: bool) {
    relay_daemon_line(bytes, is_stderr);
    let text = String::from_utf8_lossy(bytes).trim_end().to_string();
    record_daemon_text(handle, text, is_stderr);
}

/// The half of `record_daemon_line` that works on an already-decoded string, so
/// a `CommandEvent::Error` — which arrives as one — reaches the page too.
fn record_daemon_text(handle: &tauri::AppHandle, text: String, is_stderr: bool) {
    if text.is_empty() {
        return;
    }
    let status = handle.state::<DaemonStatus>();
    let seq = status.buffer.lock().unwrap().push(text.clone());
    // Emit the parsed progress event BEFORE the raw line: both carry this `seq`,
    // the page gates both on one `shownSeq`, and same-window events deliver in
    // emission order — so the phase label supersedes the raw `naiad-startup`
    // line for that seq instead of the raw text flashing on screen.
    if let Some(p) = parse_startup_progress(&text) {
        let _ = handle.emit(
            "daemon://progress",
            ProgressPayload {
                step: p.step,
                total: p.total,
                label: p.label,
                seq,
            },
        );
    }
    let _ = handle.emit(
        "daemon://line",
        LinePayload {
            stream: if is_stderr { "stderr" } else { "stdout" },
            text,
            seq,
        },
    );
}

/// Resolve the console switch from argv, env, and TOML using the
/// first-set-wins ladder. Returns the resolution so callers can log overrides.
///
/// Ladder: `--console`/`--no-console` → `NAIAD_CONSOLE` → `[log].console` → `false`.
fn resolve_console_from_args(
    mut args: impl Iterator<Item = String>,
    env: Option<&str>,
    toml_console: Option<bool>,
) -> naiad_bootstrap::ConsoleResolution {
    // Scan argv for --console / --no-console; first match wins.
    let flag: Option<bool> = args.find_map(|a| {
        if a == "--console" {
            Some(true)
        } else if a == "--no-console" {
            Some(false)
        } else {
            None
        }
    });
    naiad_bootstrap::resolve_console(flag, env, toml_console)
}

/// The database path the shell hands the daemon, as a `PathBuf`. Derived from
/// `resolve_db_arg` rather than re-deriving the rule: two copies could drift,
/// and the console check would then read a `naiad.toml` beside a different
/// database than the one the daemon opens.
fn resolve_db_path() -> Option<PathBuf> {
    resolve_db_arg().ok().map(|(path, _)| PathBuf::from(path))
}

/// The database path as the string the daemon's `--db` takes (plus the
/// resolution struct for override warnings), with the reason for a failure
/// preserved so the error page can show it.
///
/// Portable by default: `naiad.db` beside the executable, in the self-contained
/// app folder. `NAIAD_DB` overrides it — handy for pointing the app at an
/// existing library (e.g. during development). Error strings are unchanged from
/// before so the error-page copy stays the same.
fn resolve_db_arg() -> Result<(String, naiad_bootstrap::DbPathResolution), String> {
    let resolution = naiad_bootstrap::resolve_db_path_from_process(None)?;
    let path = resolution.path.clone();
    Ok((path, resolution))
}

/// Read `[log].console` from the `naiad.toml` beside the database. Returns
/// `Some(value)` when the key is explicitly present, `None` when absent or
/// when the file cannot be read — a debug switch must never block startup.
fn toml_console_setting() -> Option<bool> {
    #[derive(serde::Deserialize, Default)]
    struct Root {
        #[serde(default)]
        log: LogSection,
    }
    #[derive(serde::Deserialize, Default)]
    struct LogSection {
        /// `None` when the key is absent from the file.
        #[serde(default)]
        console: Option<bool>,
    }
    let db = resolve_db_path()?;
    let toml_path = db.with_file_name("naiad.toml");
    let text = std::fs::read_to_string(&toml_path).ok()?;
    toml::from_str::<Root>(&text).ok()?.log.console
}

/// If requested, give this GUI-subsystem process a real console window so the
/// user can watch daemon output. Must run before anything prints. Best-effort:
/// a failed `AllocConsole` changes nothing, and non-Windows terminal launches
/// already show output. Override warnings are emitted via `eprintln!` (the log
/// crate is not yet set up at this point — best effort is acceptable).
pub fn init_debug_console() {
    let env = std::env::var("NAIAD_CONSOLE").ok();
    let toml_val = toml_console_setting();
    let resolution = resolve_console_from_args(std::env::args(), env.as_deref(), toml_val);

    if !resolution.on {
        return;
    }

    #[cfg(windows)]
    // SAFETY: AllocConsole takes no pointers and has no Rust-side invariants.
    // It returns 0 on failure, which is intentionally ignored.
    unsafe {
        windows_sys::Win32::System::Console::AllocConsole();
    }

    // Emit cross-tier override warnings (best effort — pre-log-subscriber).
    for (loser_source, loser_val) in &resolution.overridden {
        eprintln!(
            "naiad: console: {} ({}) overrides {} ({})",
            resolution.source.name(),
            resolution.on,
            loser_source.name(),
            loser_val
        );
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            daemon_state,
            load_view_state,
            save_inspector_collapsed,
            save_tile
        ])
        .setup(|app| {
            app.manage(ViewStateLock::default());
            app.manage(DaemonStatus {
                state: Mutex::new(Status::Starting),
                buffer: Mutex::new(LineBuffer {
                    lines: VecDeque::with_capacity(RING_CAP),
                    emitted: 0,
                }),
            });

            // Window first. The daemon serves the UI, so we cannot wait for it
            // and still show anything (#48): open the real window at a bundled
            // local page, then navigate it to the daemon origin once that
            // exists. One window, one hand-off. We create it hidden just long
            // enough to restore saved native bounds before the first paint.
            let window_state_path = window_state_path(app.handle());
            let saved_window_state = window_state_path.as_deref().and_then(load_window_state);
            let mut window_builder = WebviewWindowBuilder::new(
                app.handle(),
                "main",
                WebviewUrl::App("loading.html".into()),
            )
            .title("Naiad")
            .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
            .min_inner_size(MIN_WINDOW_WIDTH as f64, MIN_WINDOW_HEIGHT as f64)
            .decorations(false)
            .visible(false);
            if saved_window_state.is_none() {
                window_builder = window_builder.center();
            }
            let window = match window_builder.build() {
                Ok(window) => window,
                Err(e) => {
                    fatal(&format!("Failed to open the Naiad window: {e}"));
                }
            };
            if let Some(state) = saved_window_state {
                apply_window_state(&window, state);
            }
            let window_for_state = window.clone();
            let saver = window_state_path.clone().map(WindowStateSaver::new);
            window.on_window_event(move |event| {
                let Some(saver) = saver.as_ref() else {
                    return;
                };
                match event {
                    WindowEvent::Resized(_)
                    | WindowEvent::Moved(_)
                    | WindowEvent::ScaleFactorChanged { .. } => saver.record(&window_for_state),
                    WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed => {
                        saver.flush(&window_for_state)
                    }
                    _ => {}
                }
            });
            if let Err(e) = window.show() {
                fatal(&format!("Failed to open the Naiad window: {e}"));
            }

            // From here the window is on screen, so every failure belongs on the
            // error page. A `?` would return Err from setup and panic the
            // builder, flashing the window open and closed with the reason on a
            // stderr nobody reads — and a missing or unspawnable sidecar is the
            // likeliest startup failure of all.
            let db_arg = match resolve_db_arg() {
                Ok((path, resolution)) => {
                    // Log any cross-tier override warnings via the log crate
                    // (best effort; if no subscriber is wired up, they are
                    // silently dropped — never block startup).
                    for (loser_source, loser_val) in &resolution.overridden {
                        log::warn!(
                            "db path: {} ({}) overrides {} ({})",
                            resolution.source.name(),
                            &resolution.path,
                            loser_source.name(),
                            loser_val
                        );
                    }
                    path
                }
                Err(e) => {
                    fail_startup(app.handle(), &e);
                    return Ok(());
                }
            };

            // Spawn the daemon sidecar on an OS-chosen free port.
            let spawned = app.shell().sidecar("naiad").and_then(|cmd| {
                cmd.args(["daemon", "--db", &db_arg, "--addr", "127.0.0.1:0"])
                    .spawn()
            });
            let (mut rx, child) = match spawned {
                Ok(pair) => pair,
                Err(e) => {
                    fail_startup(
                        app.handle(),
                        &format!("Could not start the Naiad daemon: {e}"),
                    );
                    return Ok(());
                }
            };
            app.manage(DaemonChild(Mutex::new(Some(child))));

            // Wait for the daemon to announce its bound address, then navigate
            // the already-open window there. The deadline bounds silence, not
            // total startup: any output resets it, so a long migration
            // heartbeating to stderr can take as long as it needs. Failures
            // set the in-window error state rather than showing a dialog.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Every arm that keeps waiting resets the clock through this, so
                // no future arm can forget to and silently turn the silence
                // budget back into a total-startup budget.
                let bump =
                    || tokio::time::Instant::now() + Duration::from_secs(DAEMON_READY_TIMEOUT_SECS);
                let mut deadline = bump();
                let timeout_msg = format!(
                    "The Naiad daemon produced no output for {DAEMON_READY_TIMEOUT_SECS} seconds."
                );
                // Phase 1: wait for the bound-address line. The deadline bounds
                // silence, not total startup: any daemon output resets it.
                // Relay everything so a debug console shows startup progress.
                loop {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        fail_startup(&handle, &timeout_msg);
                        return;
                    }
                    match tokio::time::timeout(remaining, rx.recv()).await {
                        Ok(Some(CommandEvent::Stdout(bytes))) => {
                            record_daemon_line(&handle, &bytes, false);
                            let line = String::from_utf8_lossy(&bytes);
                            if let Some(addr) = parse_bound_addr(&line) {
                                navigate_to_daemon(&handle, &addr);
                                break;
                            }
                        }
                        Ok(Some(CommandEvent::Stderr(bytes))) => {
                            record_daemon_line(&handle, &bytes, true);
                        }
                        Ok(Some(CommandEvent::Terminated(_))) => {
                            fail_startup(&handle, "The Naiad daemon exited before it was ready.");
                            return;
                        }
                        Ok(Some(CommandEvent::Error(msg))) => {
                            // A sidecar that fails to spawn, or whose reader
                            // breaks, says so only here. Swallowing it left the
                            // error page with "(no daemon output)" and a Copy
                            // details button that yielded nothing.
                            eprintln!("[daemon] {msg}");
                            record_daemon_text(&handle, msg, true);
                        }
                        // Other events: the daemon is alive, so keep waiting.
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            fail_startup(&handle, "The Naiad daemon closed unexpectedly.");
                            return;
                        }
                        Err(_) => {
                            fail_startup(&handle, &timeout_msg);
                            return;
                        }
                    }
                    // Reached only by the arms that saw the daemon alive and did
                    // not hand off: any sign of life buys another full budget.
                    deadline = bump();
                }

                // Phase 2: the window is on the daemon origin. Keep relaying
                // daemon output for the app's lifetime. No timeout: a quiet
                // daemon is healthy once startup has completed.
                //
                // Console only, deliberately. The loading page is gone, so there
                // is nobody left to receive `daemon://line`, and a daemon that
                // dies now is not a *startup* failure — the app surfaces it as
                // failing requests against the origin, which is what the
                // title-bar activity indicator reports (#34).
                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(bytes) => relay_daemon_line(&bytes, false),
                        CommandEvent::Stderr(bytes) => relay_daemon_line(&bytes, true),
                        // A reader that breaks after the hand-off reports it only
                        // here. Nobody is left to render it, but the console must
                        // not go silent about why the daemon stopped talking.
                        CommandEvent::Error(msg) => eprintln!("[daemon] {msg}"),
                        _ => {}
                    }
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Naiad desktop app");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { .. } = event {
            if let Some(state) = handle.try_state::<DaemonChild>() {
                if let Some(child) = state.0.lock().unwrap().take() {
                    let _ = child.kill();
                }
            }
        }
    });
}

/// Point the already-open main window at the daemon. The UI uses relative API
/// paths, so loading the daemon-served page keeps API/thumb/file on one origin —
/// this navigation is what preserves that property, not a fresh window.
fn navigate_to_daemon(handle: &tauri::AppHandle, addr: &str) {
    let url: tauri::Url = match format!("http://{addr}/").parse() {
        Ok(u) => u,
        Err(e) => {
            fail_startup(handle, &format!("Bad daemon address '{addr}': {e}"));
            return;
        }
    };
    let Some(window) = handle.get_webview_window("main") else {
        fail_startup(
            handle,
            "The Naiad window disappeared before the daemon was ready.",
        );
        return;
    };
    // Navigate first, mark Ready only once it succeeded. Marking Ready up front
    // would make `mark_failed` a no-op on the error path below — the loading
    // page would never be told, and would spin forever behind a dead webview.
    // The window this races with is harmless: a `daemon_state` landing mid-flight
    // reads `Starting`, subscribes to events, and is then discarded by the
    // navigation itself.
    if let Err(e) = window.navigate(url) {
        fail_startup(handle, &format!("Failed to open the Naiad window: {e}"));
        return;
    }
    mark_ready(&handle.state::<DaemonStatus>().state, addr);
}

/// Report a failure and exit non-zero. Only for a webview that failed to build:
/// once the window exists, `fail_startup` renders the error inside it, where the
/// user can copy the daemon's output.
///
/// No dialog. This runs on the main thread inside `setup()`, before `app.run()`
/// services the event loop, and a native dialog dispatches onto that loop —
/// blocking on one here would hang the process with nothing on screen.
fn fatal(message: &str) -> ! {
    eprintln!("naiad: {message}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::{
        drag_strip_is_grabbable, load_view_state_from_path, load_window_state, parse_bound_addr,
        parse_startup_progress, resolve_console_from_args, write_view_state_to_path,
        write_window_state, PersistedViewState, PersistedWindowState,
    };

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    // Helper: resolve and return just the bool result.
    fn console(argv: &[&str], env: Option<&str>, toml: Option<bool>) -> bool {
        resolve_console_from_args(args(argv), env, toml).on
    }

    #[test]
    fn console_flag_enables_the_console() {
        assert!(console(&["naiad-desktop", "--console"], None, None));
    }

    #[test]
    fn env_var_enables_the_console() {
        assert!(console(&["naiad-desktop"], Some("1"), None));
    }

    #[test]
    fn toml_console_enables_the_console() {
        assert!(console(&["naiad-desktop"], None, Some(true)));
    }

    #[test]
    fn default_is_off() {
        assert!(!console(&["naiad-desktop"], None, None));
        assert!(!console(&["naiad-desktop"], Some(""), None));
        assert!(!console(&["naiad-desktop"], Some("0"), None));
    }

    #[test]
    fn no_console_flag_beats_env_on() {
        // --no-console wins over NAIAD_CONSOLE=1
        assert!(!console(
            &["naiad-desktop", "--no-console"],
            Some("1"),
            None
        ));
    }

    #[test]
    fn naiad_console_zero_beats_toml_true() {
        // NAIAD_CONSOLE=0 overrides console=true in naiad.toml
        assert!(!console(&["naiad-desktop"], Some("0"), Some(true)));
    }

    #[test]
    fn empty_env_is_absent_so_toml_wins() {
        // Empty string env var = absent; toml true should still enable console
        assert!(console(&["naiad-desktop"], Some(""), Some(true)));
    }

    #[test]
    fn parses_the_authority_from_the_startup_line() {
        assert_eq!(
            parse_bound_addr("naiad daemon on http://127.0.0.1:54321"),
            Some("127.0.0.1:54321".to_string())
        );
    }

    #[test]
    fn tolerates_surrounding_whitespace_and_newlines() {
        assert_eq!(
            parse_bound_addr("  naiad daemon on http://127.0.0.1:8080\n"),
            Some("127.0.0.1:8080".to_string())
        );
    }

    #[test]
    fn rejects_unrelated_lines() {
        assert_eq!(parse_bound_addr("file watching disabled: nope"), None);
        assert_eq!(parse_bound_addr(""), None);
    }

    #[test]
    fn rejects_a_marker_with_no_authority() {
        assert_eq!(parse_bound_addr("naiad daemon on http://"), None);
        assert_eq!(parse_bound_addr("naiad daemon on http://garbage"), None);
    }

    #[test]
    fn parse_startup_progress_accepts_wellformed() {
        let p = parse_startup_progress("naiad-startup 3/5 opening read pool")
            .expect("well-formed line parses");
        assert_eq!(p.step, 3);
        assert_eq!(p.total, 5);
        assert_eq!(p.label, "opening read pool");
    }

    #[test]
    fn parse_startup_progress_tolerates_surrounding_whitespace() {
        let p = parse_startup_progress("  naiad-startup 1/5 opening database (migrations)\n")
            .expect("leading/trailing whitespace tolerated");
        assert_eq!(p.step, 1);
        assert_eq!(p.total, 5);
        assert_eq!(p.label, "opening database (migrations)");
    }

    #[test]
    fn parse_startup_progress_rejects_missing_prefix() {
        assert!(parse_startup_progress("naiad daemon on http://127.0.0.1:9000").is_none());
        assert!(parse_startup_progress("").is_none());
    }

    #[test]
    fn parse_startup_progress_rejects_non_integer_step() {
        assert!(parse_startup_progress("naiad-startup x/5 opening").is_none());
    }

    #[test]
    fn parse_startup_progress_rejects_non_integer_total() {
        assert!(parse_startup_progress("naiad-startup 3/y opening").is_none());
    }

    #[test]
    fn parse_startup_progress_rejects_empty_label() {
        assert!(parse_startup_progress("naiad-startup 3/5 ").is_none());
        assert!(parse_startup_progress("naiad-startup 3/5").is_none());
    }

    #[test]
    fn parse_startup_progress_rejects_malformed_count() {
        // Trailing garbage in the count field: "5x" is not an integer.
        assert!(parse_startup_progress("naiad-startup 3/5x opening read pool").is_none());
    }

    #[test]
    fn parse_startup_progress_rejects_step_over_total() {
        assert!(parse_startup_progress("naiad-startup 6/5 x").is_none());
    }

    #[test]
    fn persisted_window_state_rejects_implausible_bounds() {
        assert!(PersistedWindowState {
            x: -1920,
            y: 120,
            width: 1200,
            height: 800,
        }
        .is_sane());
        assert!(!PersistedWindowState {
            x: 0,
            y: 0,
            width: 300,
            height: 800,
        }
        .is_sane());
        assert!(!PersistedWindowState {
            x: 0,
            y: 0,
            width: 1200,
            height: 200,
        }
        .is_sane());
        assert!(!PersistedWindowState {
            x: 100001,
            y: 0,
            width: 1200,
            height: 800,
        }
        .is_sane());
    }

    /// `i32::MIN.abs()` overflows, so the bound must be a range check.
    #[test]
    fn persisted_window_state_rejects_extreme_negative_coordinates() {
        assert!(!PersistedWindowState {
            x: i32::MIN,
            y: 0,
            width: 1200,
            height: 800,
        }
        .is_sane());
        assert!(!PersistedWindowState {
            x: 0,
            y: i32::MIN,
            width: 1200,
            height: 800,
        }
        .is_sane());
        assert_eq!(
            load_window_state_from_json(
                "extreme",
                r#"{"version":2,"x":-2147483648,"y":0,"width":1200,"height":800}"#
            ),
            None
        );
    }

    /// v1 stored the position in logical pixels. Reading it as physical would
    /// misplace the window on a scaled display, so the file is discarded.
    #[test]
    fn window_state_from_an_older_schema_is_ignored() {
        assert_eq!(
            load_window_state_from_json("v1", r#"{"x":50,"y":75,"width":1400,"height":900}"#),
            None
        );
        assert_eq!(
            load_window_state_from_json(
                "v99",
                r#"{"version":99,"x":50,"y":75,"width":1400,"height":900}"#
            ),
            None
        );
    }

    fn load_window_state_from_json(tag: &str, json: &str) -> Option<PersistedWindowState> {
        let path = std::env::temp_dir().join(format!(
            "naiad-window-state-{}-{}.json",
            std::process::id(),
            tag
        ));
        std::fs::write(&path, json).unwrap();
        let state = load_window_state(&path);
        let _ = std::fs::remove_file(path);
        state
    }

    #[test]
    fn a_rect_off_every_attached_monitor_is_not_reachable() {
        let monitors = [(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];

        assert!(drag_strip_is_grabbable((100, 100, 1200, 800), &monitors));
        // Straddling the seam still counts: the strip's right half is grabbable
        // on the secondary.
        assert!(drag_strip_is_grabbable((1900, 0, 1200, 800), &monitors));
        // The secondary monitor is gone; its window is now nowhere.
        assert!(!drag_strip_is_grabbable((-3000, 0, 1200, 800), &monitors));
        assert!(!drag_strip_is_grabbable((0, 1440, 1200, 800), &monitors));
        // Touching an edge is not overlapping.
        assert!(!drag_strip_is_grabbable((4480, 0, 1200, 800), &monitors));
        // No monitors reported: cannot tell, so do not move the window.
        assert!(drag_strip_is_grabbable((-3000, 0, 1200, 800), &[]));
    }

    /// Overlapping a display is not enough — the top strip is the only way to
    /// move an undecorated window, so it must be wide and tall enough to grab.
    #[test]
    fn a_rect_with_only_a_sliver_onscreen_is_not_reachable() {
        let monitors = [(0, 0, 1920, 1080)];

        // One visible pixel column at the right edge.
        assert!(!drag_strip_is_grabbable((1919, 100, 1200, 800), &monitors));
        // 119 columns: still under the pointer target.
        assert!(!drag_strip_is_grabbable((1801, 100, 1200, 800), &monitors));
        assert!(drag_strip_is_grabbable((1800, 100, 1200, 800), &monitors));
        // Pushed left: the visible slice is the window's right edge.
        assert!(!drag_strip_is_grabbable((-1101, 100, 1200, 800), &monitors));
        assert!(drag_strip_is_grabbable((-1080, 100, 1200, 800), &monitors));
        // The strip itself is below the monitor's bottom edge; the body may show
        // above nothing, but there is nothing left to grab.
        assert!(!drag_strip_is_grabbable((100, 1057, 1200, 800), &monitors));
        assert!(drag_strip_is_grabbable((100, 1056, 1200, 800), &monitors));
        // Only the window's lower body is onscreen: the strip is off the top.
        assert!(!drag_strip_is_grabbable((100, -25, 1200, 800), &monitors));
        assert!(drag_strip_is_grabbable((100, -24, 1200, 800), &monitors));
    }

    /// Windows parks a minimized window at (-32000, -32000). Persisting it would
    /// strand the window offscreen on the next launch.
    #[test]
    fn persisted_window_state_rejects_the_minimized_sentinel() {
        let sentinel = PersistedWindowState {
            x: -32000,
            y: -32000,
            width: 1200,
            height: 800,
        };
        assert!(!sentinel.is_sane());

        let path = std::env::temp_dir().join(format!(
            "naiad-window-state-{}-{}.json",
            std::process::id(),
            "sentinel"
        ));
        assert!(write_window_state(&path, sentinel).is_err());
        assert!(!path.exists());

        // A file written by an older build still carries it; ignore it on read.
        std::fs::write(
            &path,
            r#"{"version":2,"x":-32000,"y":-32000,"width":1200,"height":800}"#,
        )
        .unwrap();
        assert_eq!(load_window_state(&path), None);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persisted_window_state_round_trips_through_json() {
        let path = std::env::temp_dir().join(format!(
            "naiad-window-state-{}-{}.json",
            std::process::id(),
            "round-trip"
        ));
        let state = PersistedWindowState {
            x: 50,
            y: 75,
            width: 1400,
            height: 900,
        };

        write_window_state(&path, state).unwrap();
        assert_eq!(load_window_state(&path), Some(state));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_window_state_file_is_ignored() {
        let path = std::env::temp_dir().join(format!(
            "naiad-window-state-{}-{}.json",
            std::process::id(),
            "invalid"
        ));
        std::fs::write(&path, r#"{"version":2,"x":0,"y":0,"width":10,"height":10}"#).unwrap();

        assert_eq!(load_window_state(&path), None);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persisted_view_state_round_trips_inspector_preference() {
        let path = std::env::temp_dir().join(format!(
            "naiad-view-state-{}-{}.json",
            std::process::id(),
            "round-trip"
        ));
        let state = PersistedViewState {
            inspector_collapsed: Some(true),
            tile: Some(240),
        };

        write_view_state_to_path(&path, &state).unwrap();
        assert_eq!(load_view_state_from_path(&path), state);

        let _ = std::fs::remove_file(path);
    }

    /// The tile size and the inspector state share the file; writing one must
    /// preserve the other (read-modify-write), not blank it.
    #[test]
    fn tile_and_inspector_persist_independently() {
        let path = std::env::temp_dir().join(format!(
            "naiad-view-state-{}-{}.json",
            std::process::id(),
            "tile-inspector"
        ));
        let _ = std::fs::remove_file(&path);
        let lock = ViewStateLock::default();

        set_inspector_collapsed(&lock, &path, true).unwrap();
        set_tile(&lock, &path, 512).unwrap();

        let state = load_view_state_from_path(&path);
        assert_eq!(state.inspector_collapsed, Some(true));
        assert_eq!(state.tile, Some(512));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_view_state_file_defaults_without_failing_startup() {
        let path = std::env::temp_dir().join(format!(
            "naiad-view-state-{}-{}.json",
            std::process::id(),
            "invalid"
        ));
        std::fs::write(&path, "not json").unwrap();

        assert_eq!(
            load_view_state_from_path(&path),
            PersistedViewState::default()
        );

        let _ = std::fs::remove_file(path);
    }

    /// A save must read, mutate and write as one critical section, or a rapid
    /// double-toggle can persist the value of whichever command *started* last.
    #[test]
    fn a_view_state_save_excludes_a_concurrent_one() {
        let path = std::env::temp_dir().join(format!(
            "naiad-view-state-{}-{}.json",
            std::process::id(),
            "exclusion"
        ));
        let _ = std::fs::remove_file(&path);
        let lock = std::sync::Arc::new(ViewStateLock::default());

        let guard = lock.0.lock().unwrap();
        let saver = {
            let (lock, path) = (std::sync::Arc::clone(&lock), path.clone());
            std::thread::spawn(move || set_inspector_collapsed(&lock, &path, true).unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!path.exists(), "a save ran while the lock was held");
        drop(guard);
        saver.join().unwrap();

        assert_eq!(
            load_view_state_from_path(&path).inspector_collapsed,
            Some(true)
        );
        let _ = std::fs::remove_file(path);
    }

    use super::{
        mark_failed, mark_ready, set_inspector_collapsed, set_tile, DaemonState, LineBuffer,
        Status, ViewStateLock, RING_CAP,
    };
    use std::sync::Mutex;

    /// Declaring an app ACL manifest (`build.rs`) makes Tauri enforce the ACL on
    /// app commands, so an ungranted command is rejected at runtime — silently,
    /// because both callers fall back. Nothing else checks that
    /// `capabilities/default.json` keeps up with `APP_COMMANDS`.
    #[test]
    fn every_app_command_is_granted_by_the_default_capability() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let permissions = capability["permissions"].as_array().unwrap();

        for command in [
            "daemon_state",
            "load_view_state",
            "save_inspector_collapsed",
            "save_tile",
        ] {
            let permission =
                serde_json::Value::from(format!("allow-{}", command.replace('_', "-")));
            assert!(
                permissions.contains(&permission),
                "{command} is in generate_handler! but {permission} is missing from capabilities/default.json"
            );
        }
    }

    /// The wire format is a contract with `ui/src/loading.ts`, and no compiler
    /// checks it: rename a variant or a field here and the loading page simply
    /// spins forever, with every other test still green. Pin the exact JSON.
    #[test]
    fn daemon_state_serializes_the_shape_the_loading_page_expects() {
        let json = |s: &DaemonState| serde_json::to_string(s).unwrap();

        assert_eq!(
            json(&DaemonState::Starting {
                lines: vec![],
                seq: 0
            }),
            r#"{"kind":"starting","lines":[],"seq":0}"#
        );
        assert_eq!(
            json(&DaemonState::Starting {
                lines: vec!["opening database".into()],
                seq: 1
            }),
            r#"{"kind":"starting","lines":["opening database"],"seq":1}"#
        );
        assert_eq!(
            json(&DaemonState::Ready {
                addr: "127.0.0.1:54321".into()
            }),
            r#"{"kind":"ready","addr":"127.0.0.1:54321"}"#
        );
        assert_eq!(
            json(&DaemonState::Failed {
                message: "boom".into(),
                lines: vec!["a".into(), "b".into()],
            }),
            r#"{"kind":"failed","message":"boom","lines":["a","b"]}"#
        );
    }

    #[test]
    fn the_ring_buffer_keeps_the_last_lines_only() {
        let mut buf = LineBuffer::default();
        for i in 0..25 {
            buf.push(format!("line-{i}"));
        }
        let tail = buf.tail();
        assert_eq!(tail.len(), RING_CAP);
        assert_eq!(tail.first().unwrap(), "line-5");
        assert_eq!(tail.last().unwrap(), "line-24");
    }

    /// The loading page draws `tail.last()` and remembers it as `seq`, then drops
    /// every `daemon://line` at or below that. If `seq` ever ran ahead of the
    /// tail, the page would silently discard the event for a line it never drew
    /// and freeze on a stale one. Push and count must move together.
    #[test]
    fn seq_names_the_last_line_in_the_tail() {
        let mut buf = LineBuffer::default();
        assert_eq!(buf.emitted, 0);
        assert!(buf.tail().is_empty());

        for i in 1..=25u64 {
            let seq = buf.push(format!("line-{i}"));
            assert_eq!(seq, i);
            assert_eq!(buf.emitted, i);
            assert_eq!(buf.tail().last().unwrap(), &format!("line-{i}"));
        }
    }

    #[test]
    fn starting_transitions_to_ready() {
        let state = Mutex::new(Status::Starting);
        assert!(mark_ready(&state, "127.0.0.1:1234"));
        let guard = state.lock().unwrap();
        match &*guard {
            Status::Ready { addr } => assert_eq!(addr, "127.0.0.1:1234"),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn starting_transitions_to_failed() {
        let state = Mutex::new(Status::Starting);
        assert!(mark_failed(&state, "boom", vec!["a".into()]));
        let guard = state.lock().unwrap();
        match &*guard {
            Status::Failed { message, lines } => {
                assert_eq!(message, "boom");
                assert_eq!(lines, &vec!["a".to_string()]);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_terminal_state_is_never_clobbered() {
        // The daemon exiting *after* the window navigated must not rewrite the
        // state to Failed — the app is already running against it.
        let state = Mutex::new(Status::Starting);
        assert!(mark_ready(&state, "127.0.0.1:1234"));
        assert!(!mark_failed(&state, "exited", vec![]));
        assert!(matches!(&*state.lock().unwrap(), Status::Ready { .. }));

        // And a second failure does not overwrite the first, more informative one.
        let state = Mutex::new(Status::Starting);
        assert!(mark_failed(&state, "first", vec![]));
        assert!(!mark_failed(&state, "second", vec![]));
        let guard = state.lock().unwrap();
        match &*guard {
            Status::Failed { message, .. } => assert_eq!(message, "first"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
