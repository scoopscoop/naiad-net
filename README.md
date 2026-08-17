# Naiad

![CI](https://github.com/scoopscoop/naiad-net/actions/workflows/ci.yml/badge.svg)

**A fast local media tagger that can share tags with the community.**

Naiad is a lightweight media tagger inspired by
[Hydrus Network](https://hydrusnetwork.github.io/hydrus/). Point it at your folders and it
turns them into a searchable, taggable gallery — and when you want more than your own
tags, it can pull community tags from shared servers, fetching only the small slices that
match your library.

- **Your files stay yours.** Nothing is uploaded, moved, or renamed. Naiad reads your
  folders and keeps everything it knows in one database file.
- **Fast to browse.** Search-as-you-type, thumbnails, a keyboard-first gallery.
- **Tag it your way.** Your own tags live in a private scope; community tags stay clearly
  separated and never mix into what you wrote.
- **Community tags without the bulk.** Subscribe to a tag server and fetch just what your
  library needs — no giant database download to get started.
- **Private by design.** Servers never learn which files you have: requests are blurred
  into crowds of a thousand-plus files.

> **Status:** pre-alpha. Indexing, search, tagging, the desktop app, and tag servers with
> signed submissions all work today. Expect rough edges and breaking changes — see the
> [Roadmap](docs/design.md#9-roadmap) for what's done and what's next.

---

## Get the desktop app

Naiad ships a portable desktop app that bundles the daemon — no separate server process to
start, and no installer.

Download the latest portable zip from
[github.com/scoopscoop/naiad-net/releases/latest](https://github.com/scoopscoop/naiad-net/releases/latest),
unzip it anywhere, and double-click `naiad-desktop.exe` to launch. WebView2 is required
(standard on Windows 10/11 — no separate installation needed). The daemon starts
automatically in the background.

**Build it yourself:**

```sh
npm --prefix ui run package        # → dist/Naiad-<version>-windows-x64-portable.zip
```

This builds the UI, compiles the daemon, assembles the Tauri shell, and zips everything.
Unzip anywhere and double-click `naiad-desktop.exe` — the daemon starts automatically in
the background.

The app uses the system's evergreen WebView2 runtime (standard on Windows 10/11). There is
no installer and no embedded WebView2.

**What's in the zip:** `naiad-desktop.exe` (the Tauri launcher) and `naiad.exe` (the daemon
it spawns as a sidecar). Keep them together. The app creates `naiad.db` and `thumbs.db`
beside the executable on first run — that folder is the whole self-contained, movable app.

The window appears immediately on launch: it opens on a bundled loading page that shows the
daemon's most recent output line while it starts, then navigates to the gallery once the
daemon is ready. The desktop shell remembers its last window size and inspector state across
restarts.

**If you prefer to build and run directly** (without packaging):

```sh
npm --prefix ui run build       # ui/dist (the daemon serves it)
cargo build                     # builds the naiad sidecar binary
npm --prefix ui run tauri dev   # opens the desktop window
```

**Database path** (first-set-wins): `--db` flag → `NAIAD_DB` environment variable →
`<exe dir>/naiad.db`.

**Debug console (Windows):** pass `--console` or set `NAIAD_CONSOLE=1` to open a debug
console that relays the daemon's output for the app's lifetime. Set `RUST_LOG` (or
`[log] level` in `naiad.toml`) before launching to adjust log verbosity.

> The `ui/src-tauri` shell crate is excluded from the Cargo workspace and is built by
> `tauri dev` / `tauri build` (the `naiad-ui` npm scripts), **not** by `cargo build` or
> `cargo test --workspace`. Build the `naiad` binary with `cargo build` first so the sidecar exists.

---

## Quick Start

### 1. Open the app and add a folder

Launch `naiad-desktop.exe`. The gallery opens in a native window. Point Naiad at a folder
of media files — it hashes and indexes them, generates thumbnails, and fills the gallery.
From then on, any file you add, change, or delete inside that folder is picked up
automatically; no manual rescans.

The gallery is a **three-pane workbench**: a left nav rail (saved searches + namespaces),
a center grid with inline search, and a right inspector. Click a file to preview it;
double-click (or `Enter`) for the full detail view with a resizable tag drawer. Multi-select
with ctrl+click, shift+click, or drag a selection box. Ctrl+A selects all; Esc clears.

### 2. Browse, search, and tag

Search-as-you-type across tags, namespaces, and system predicates. Wildcards, boolean
logic, and quoted phrases all work (`character:samus*`, `-series:metroid`,
`"zero mission"`). Sort the result set by import date, file date, name, size, or type.

Tag files from the detail view. Your tags are private by default and never leave your
machine.

### 3. Community tags — included out of the box

A fresh install comes **preconfigured with `naiad-net`**
(`https://v2202608398476500144.ultrasrv.de`) — the project's community tag repository.
Tags for files the community has already tagged arrive without any extra setup.

Pulls are **k-anonymous**: Naiad never sends your exact file list to a server. Instead it
fetches hash-prefix buckets, hiding your library in a crowd of a thousand or more hashes —
the server sees only "someone wants bucket `0x3A7`," not which files you own. See
[docs/design.md §7](docs/design.md#7-safety--privacy-model) for the full privacy model.

To remove or change the community repo, edit `[sync]` in `naiad.toml` (created beside the
database on first run), or use the Repos panel in the app.

### Prefer a terminal?

The daemon and CLI run headlessly without the desktop app:

```sh
# Build
cargo build --release

# Start the daemon (serves the API and web UI at http://localhost:8080)
./target/release/naiad daemon --db naiad.db

# In another terminal:
naiad scan /path/to/media                           # index a folder
naiad list                                          # list indexed files
naiad search character:samus series:metroid         # AND'd tag search (-tag negates)
naiad tag add /path/to/file.jpg character:samus     # tag a file
naiad tag list <hash>                               # show a file's tags
```

Scanned folders are watched automatically — add, change, or delete a file and the daemon
reindexes it on its own. Use `naiad roots list` / `naiad roots remove <path>` to manage
watched folders, or start the daemon with `--no-watch` to disable live watching.

See [docs/quickstart.md](docs/quickstart.md) for the full CLI reference and configuration
options.

---

## Diving deeper

- **[Design & architecture](docs/design.md)** — the detailed *why* behind the how: data
  model, search, the distributed tag protocol, safety model, roadmap, and tech stack.
- **[Quickstart guide](docs/quickstart.md)** — full CLI reference, configuration, and
  advanced setup.
- **[Logging](docs/logging.md)** — log targets, levels, and persistent file output.
- **[Operating a repo server](docs/operating-a-repo.md)** — deploy, configure, and run
  a community tag repository.

---

## License

WTFPL v2 — see [LICENSE](LICENSE). Do what the fuck you want.

## Contributing

naiad's day-to-day development happens in a private repository; this GitHub repo is a
curated public mirror so you can read the code and build it yourself. Bug reports and
issues are very welcome here. Feature pull requests aren't being accepted yet.
