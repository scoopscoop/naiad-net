# Naiad — Dev Quickstart & Manual Inspection

A hands-on checklist for spinning up everything that's built and **eyeballing it**.
Walk it top to bottom and you'll have touched every shipped subsystem: indexing,
the gallery UI, the full CLI surface, live re-indexing, and the sync
(pull-only repo) skeleton.

> **Keep this current.** This doc is a living inspection checklist — when you add
> a command, endpoint, or screen, add the steps to verify it here in the same
> change. A stale quickstart is worse than none.

> **Shell:** commands are written for **PowerShell** (the dev box default). The
> only cross-shell gotcha is environment variables — PowerShell uses
> `$env:NAME = "value"`, where the README's `sh` snippets use `NAME=value`.
> Everything else is identical.

---

## 0. One-time build

The frequent commands are wrapped in a [`justfile`](../justfile) at the repo root
— install the runner once with `cargo install just` (or `winget install Casey.Just`).
Recipe names are symmetrical: `fe` = the Svelte frontend, `be` = the Rust backend.

```powershell
just build-fe     # ui/dist          ← npm --prefix ui run build
just build-be     # target/debug     ← cargo build (debug daemon reads ui/dist live)
just build        # both of the above
just package      # the portable zip ← scripts/package-windows.ps1
```

The raw commands are spelled out throughout this doc, so nothing here depends on
having `just` installed:

```powershell
# Rust workspace (daemon, CLI, repo server, libs)
cargo build

# Web UI → ui/dist (a debug daemon reads this from disk at runtime)
npm --prefix ui run build
```

A debug `cargo build` makes the daemon read `ui/dist` live, so you can rebuild
the UI (`npm --prefix ui run build`) and refresh the browser **without**
recompiling Rust. A `--release` build bakes `ui/dist` into the binary instead.

The built binaries land in `target/debug/`:

| Binary | What it is |
|--------|------------|
| `naiad` | daemon + CLI (one binary; `naiad daemon` boots the server, every other subcommand is a client) |
| `naiad-repo` | a sync tag **repository** node (the other side of the wire) |

---

## 1. Index a folder and run the daemon

Point it at any folder of images. It hashes each file (BLAKE3), generates
thumbnails, and starts watching the folder for changes.

```powershell
# Boot the daemon (owns the library at --db, serves the API + gallery on :8080)
cargo run -p naiad-cli -- daemon --db naiad.db
```

Leave that running. In a **second terminal**, index something:

```powershell
cargo run -p naiad-cli -- scan C:\path\to\some\images
```

**Expect:** `indexed N file(s)` (plus a count of any skipped/unreadable files).
Re-running `scan` on the same folder is idempotent — already-known files are
recognized by content hash, not re-imported.

> Only image files are indexed. The scan and the live watcher accept
> `jpg jpeg png gif webp bmp tiff tif` (case-insensitive); any other
> file in the folder is silently skipped, so a mixed folder won't pull in
> `.txt`, `.dat`, videos, or other non-images.

---

## 2. The gallery UI (the Atelier workbench)

### Quick look — daemon-served

With the daemon running, open **http://localhost:8080**. You should see the
**Atelier** three-pane workbench in the **pastel-dusk** palette (periwinkle
accent, easier on the eyes than the old amber-on-brown Darkroom): a left **nav
rail** (saved searches + namespaces), the center grid with an **inline search** on
top, and a right **inspector** peek. The title bar carries the Naiad wave logo and
gallery glyph; all chrome glyphs (window controls, search, gallery, settings,
close) are a single hand-rolled monoline set — `ui/src/components/Icon.svelte`,
with the logo in `Logo.svelte`.

The title bar also carries an **activity dot** on the right. It reflects, in
priority order, whether the daemon is unreachable, then any errored, warning, or
still-running long operation (library scans, Hydrus imports), and goes quiet when
nothing is happening. Click it (or press `Enter`/`Space` on it) to open a panel
listing running and recently finished activities; `Escape` closes the panel and
returns focus to the dot. Finished errors and warnings keep the dot lit for a few
seconds, then decay to idle. Shorter waits — running a search, waiting on tag
suggestions in the search field, loading or editing a file's tags, removing a
watched folder — show a small inline spinner instead, and only if the operation
actually takes long enough to notice. While tag suggestions load, the previous
suggestion rows stay visible (dimmed) and clickable, and `Esc` dismisses the
spinner along with the dropdown.

Typeahead is relation-aware: it offers one canonical suggestion per concept, with
a count that merges the concept's aliases; typing an alias spelling surfaces its
canonical tag (which is what gets inserted).

Single-click any tile → it becomes the **focused** file and the right **inspector**
fills in:
- an **IDENTITY** card showing the BLAKE3 hash + path in mono,
- **namespace-colored tag chips** (add a `character:…`, `series:…`, `creator:…`
  tag and watch the dot color change),
- the dashed `+ add tag…` field.

Selection follows file-manager conventions. A plain click also **selects** that
tile alone and drops the range **anchor** on it; `Shift`-click then selects the
contiguous run from that anchor to the tile you clicked, and further
`Shift`-clicks re-range from the *same* anchor rather than from the last one.
`Ctrl`-click toggles individual tiles (and moves the anchor), arrow keys move
the focus *and* the anchor (so a shift-click ranges from where you navigated
to, Explorer-style), and dragging on
empty space rubber-bands a region — hold `Ctrl` to add to what is already
selected. A single-tile selection still opens against the whole gallery; only a
multi-file selection narrows the detail tab's next/prev sequence to that subset.

Double-click a tile (or press `Enter`, or hit the inspector's `Open ⤢`) → the full
**detail tab** opens: the image fills the pane and tags/identity move into a
resizable bottom drawer you can collapse. Middle-click opens the tab in the
background. A plain click only selects and focuses, Explorer-style.
When more tabs are open than the titlebar can show, the strip gains scroll
arrows, wheel/trackpad scrolling, and an all-tabs menu for jumping to (or
closing) any tab directly; activating a tab always scrolls it into view.

Tags with relations show a small `⇆` glyph; clicking it (or the `Relations…`
context-menu row) opens a popover listing what the tag is shown as, its
aliases, what it implies, and what implies it — click any related tag to
search for it.

### Frontend dev — hot reload

For UI work, run Vite instead. It serves on `:5173` and proxies `/api`, `/thumb`,
`/file` to the daemon on `:8080`, so **keep the daemon (step 1) running**:

```powershell
npm --prefix ui run dev      # http://localhost:5173
```

Edits to `ui/src/**` hot-reload instantly. This is the loop to use while
iterating on the Atelier styling.

> **Design reference:** the target aesthetic lives in
> `docs/Naiad (saved B + D).dc.html` (open it in a browser). The UI has since moved
> past that rough pass into the **Atelier** three-pane workbench (nav rail +
> inspector, pastel-dusk retint). Nav-rail v1 covers saved searches + namespaces;
> deferred pieces still needing API data we don't return yet: SHA-256, the
> Locations online/offline panel, the rest of the rail (contributors, repos, trust
> bands), predicate chips, filmstrip/prev-next, and the Tags/Info/Relations/Notes
> tabs.

---

## 3. CLI tour — every data command

All of these are HTTP calls to the running daemon. Use `cargo run -p naiad-cli --`
or, after `cargo build`, the shorter `./target/debug/naiad`.

> **The binary documents itself.** Running `naiad` with no arguments prints the
> command list plus worked examples, and every subcommand carries its own set —
> `naiad help search` is the full query-syntax cheat sheet (wildcards, `system:`
> predicates, quoting), `naiad help repo` covers the repo sync flow. Keep those
> `after_help` blocks in `crates/cli/src/main.rs` in step with this doc.

```powershell
# List everything: <hash>  <size>  <path>
naiad list

# Tag a file (by path OR 64-char hash)
naiad tag add C:\path\to\file.jpg character:samus series:metroid
naiad tag list <hash>                 # effective (sibling/parent-expanded) set
naiad tag list <hash> --raw           # literal stored mappings only
naiad tag remove <hash> series:metroid

# Boolean tag search: terms are AND'd, -tag negates, "a or b" groups
naiad search character:samus
naiad search character:samus -meta:wip

# Per-term exact match: =tag matches literally (no sibling/parent expansion)
# for that one term, while the rest of the query still expands.
naiad search =character:samus_aran series:metroid   # 1st literal, 2nd expands
naiad search -=meta:wip                              # exclude only the literal tag
```

### Siblings (aliases) — collapse a bad tag to its ideal form

```powershell
naiad tag sibling add samus character:samus    # alias "samus" → "character:samus"
naiad tag sibling list                          # bad -> ideal
# now tag a file with the alias and confirm `tag list` shows the canonical form:
naiad tag add <hash> samus
naiad tag list <hash>                            # should display character:samus
naiad tag sibling remove samus
```

### Parents (implications) — a child tag implies a parent

```powershell
naiad tag parent add character:samus series:metroid   # samus ⇒ metroid
naiad tag parent list                                  # child -> parent
# a file tagged only character:samus should now also show series:metroid
# in `tag list` (effective set), but NOT in `tag list --raw`.
naiad tag parent remove character:samus series:metroid
```

---

## 4. Watch-based live re-indexing

The daemon watches scanned folders ("roots") and reindexes changes live. It
also runs a **catch-up rescan** of every root at startup, right after the
watcher attaches: files added while the daemon was down (or left behind by an
import that was interrupted mid-way) are imported on the next launch. A root
whose drive is unavailable at boot is skipped — its files stay in the library
untouched.

```powershell
naiad roots list                    # folders being watched
```

**Inspect it:** with the daemon running, drop a new image into a scanned folder
(or delete one). Within a moment, `naiad list` reflects the change with **no
manual re-scan**. A removed file is *marked missing*, never auto-deleted — its
tags survive.

```powershell
naiad roots remove C:\path\to\some\images   # stop watching a folder
```

> Boot with `--no-watch` to disable live reindexing (and the startup catch-up
> rescan) for a session.

---

## 5. Repo sync — pull and submit tags across the wire (Phase 3)

A **repository node** serves a `hash → tags` snapshot; the daemon handshakes the
repo (`/repo/caps`), then either pulls the **hash-prefix buckets** covering the
files it owns (k-anonymity — your exact hashes never leave) or, for a small repo,
downloads the whole snapshot. Either way it keeps only the tags for files **you
own**, and they appear in the gallery. Clients can also sign and submit tag
add/removes back to the repo using an Ed25519 account. Accounts auto-create on
first signed submit — no registration step.

> **Running a real node?** This section is the *dev* walkthrough. For an
> operator-facing guide — public hosting, accounts, the report queue, and key
> management — see [operating-a-repo.md](operating-a-repo.md).

### a. Stand up a repository node

In a **third terminal**, seed a repo with tags for a hash you own (copy a real
hash from `naiad list`), then serve it:

```powershell
# Seed (admin path — NOT the signed-submission protocol)
cargo run -p naiad-server -- --db repo.db add <hash-you-own> character:samus
cargo run -p naiad-server -- --db repo.db add <hash-you-own> series:metroid

# Serve it on :9090 (--repo-key <hex> advertises a repo identity for derived-key
# anchoring; --k is the k-anonymity crowd-size floor; default 1000 means a small
# dev repo always serves whole-repo mode)
# NAIAD_REPO_NAME sets the display name the repo advertises in /repo/caps;
# subscribers capture it automatically as the local subscription name.
$env:NAIAD_REPO_NAME="ptr"; cargo run -p naiad-server -- --db repo.db serve --k 1000
```

Check the capabilities handshake the client uses to choose its pull strategy:
**http://127.0.0.1:9090/repo/caps** → `{ "protocol": 6, "mode": "wholerepo",
"reports": true }` for a small repo (fewer than `--k` hashes). A repo with ≥ `k`
hashes returns a `prefix_bits` in the mode, and the client pulls only the
hash-prefix buckets covering the files it owns — never sending its exact hashes
(k-anonymity, [ADR 0001](adr/0001-query-privacy-k-anonymity-buckets.md)).
To watch the bucket path locally, serve with a low floor (e.g. `--k 2`) over a
repo holding several hashes.

#### Bulk-seeding from a client export

Instead of seeding one hash at a time you can export all current mappings from a
Naiad client library and load them into the repo in two offline commands:

```powershell
# Export (daemon does not need to be running; read-only)
cargo run -p naiad-cli -- export-mappings --db naiad.db --out mappings.jsonl

# Bulk-seed (idempotent — already-current rows are skipped)
cargo run -p naiad-server -- --db repo.db seed --from-file mappings.jsonl
# → seeded: X inserted, Y skipped, Z total
```

Re-export and re-seed after another tagging session for an incremental sync —
only net-new rows are inserted. Malformed lines fail-fast with `file:line`,
nothing written. The JSONL format (`{"hash":"<64-hex>","tag":"..."}` per line)
also accepts input from non-Naiad sources.

> For an operator-facing walkthrough of this workflow see the "Bulk-seeding
> from a client library" section in [operating-a-repo.md](operating-a-repo.md).

### b. Subscribe and pull from the daemon side

```powershell
naiad repo add http://127.0.0.1:9090        # subscribe (the repo names itself)
naiad repo list                              # name + url
naiad repo pull                              # pull all (or `naiad repo pull <name>` from `repo list`)
```

**Expect:** `ptr: 1 file(s) tagged, 2 mapping(s) added`. Now:

```powershell
naiad tag list <hash-you-own>        # the pulled character:samus / series:metroid appear
naiad repo pull ptr                  # re-pull is idempotent → 0 mapping(s) added
naiad repo remove ptr                # detach (keeps pulled tags by default)
naiad repo remove ptr --purge        # detach AND delete all pulled tags
```

Tags the repo had for hashes you **don't** own are downloaded and discarded —
never stored. After `repo remove` (without `--purge`), the pulled tags stay in
your library under the now-detached service; pass `--purge` to delete them, or
use the "also delete its pulled tags" checkbox in the Settings Repos UI.

### c. Publish a tag (signed submission)

Your client has an Ed25519 account created lazily on first submit (key stored as
`naiad.key` beside the DB — never inside it). Submit a tag for a file you own:

```powershell
naiad account                                    # shows your public key (or "no account yet")
naiad repo submit ptr <file-or-hash> character:samus
naiad repo pull ptr                              # the tag returns, attributed to you
naiad repo submit ptr <file-or-hash> character:samus --remove
naiad repo pull ptr                              # authoritative re-pull drops it
```

**Expect:** after `submit`, `naiad account` reports a 64-char public key, and the
repo's `/repo/snapshot` carries the tag under that key. `--remove` retracts your
own assertion; the next pull prunes it.

### d. Submit and pull tag relations (siblings/parents)

Relations are tag-keyed, so they sync in **bulk** — there is no privacy reason to
bucket them (they say nothing about which files you own). Sign a sibling or parent
edge and submit it, then bulk-pull the repo's whole relation graph into the shared
service:

```powershell
# Alias "samus" → "character:samus" (sibling); --remove retracts your own edge.
naiad relation submit ptr sibling samus character:samus
# Imply "series:metroid" from "character:samus" (parent).
naiad relation submit ptr parent character:samus series:metroid

naiad relation pull ptr        # → "ptr: N sibling(s), M parent(s)"
```

The pull is **authoritative**: the shared service's relations are replaced by the
repo's current graph each time, so a retracted edge disappears on the next pull.
Conflicting sibling ideals (two authors aliasing the same tag differently) collapse
deterministically to the lexicographically-smallest ideal — the local schema allows
one ideal per alias per service.

Inspect what landed and where it came from:

```powershell
naiad relation list                 # every edge: KIND, FROM → TO, SERVICE, AUTHOR
naiad relation list --kind sibling  # filter by kind; --service <name> filters by repo
naiad relation status               # per-service sibling/parent counts + last pull time
```

Pulled edges show the submitter's author (short public-key hex) and their owning
service; locally-created edges show `(local)`. `relation status` reports `never`
for a service whose relations have not been pulled yet.

> Pulled relations and mappings are now merged into search and display: an alias
> or implication you pulled will match its canonical/child files, and pulled-only
> tags show with a `*` sigil. Add `--raw` to a search to match stored tags
> literally (no sibling/parent expansion), or `--local-only` to ignore the shared
> service entirely. A per-term `=tag` (e.g. `=character:samus_aran`) matches that
> single predicate literally while the rest of the query still expands — the
> granular counterpart to whole-query `--raw`.

### Hiding tags you don't want (block list)

Pulled repositories can carry tags, patterns, or whole submitters you'd rather
never see. Block them locally — your own tags are never affected:

```powershell
naiad block add --tag meme:bad           # one exact tag
naiad block add --pattern 'spam:*'       # a whole namespace
naiad block add --author <64-hex-key>    # everything one submitter asserts
naiad block list                         # show rules (with ids)
naiad block remove <id>                  # lift a block
```

Blocks filter both display and search; they only touch pulled (shared) services,
so a tag you applied yourself is always visible.

### Rejecting a mapping

Sometimes a pulled tag is wrong on *this specific file* — not wrong enough to
block the tag everywhere or distrust the contributor globally, just wrong here.
The reject action handles exactly that case.

**How to reject — UI only (there is no `naiad reject` CLI subcommand):**

- In the detail view or inspector, right-click a pulled tag chip (or press
  `Shift+F10` / the Menu key on a focused chip) and choose **Hide from repo ⊘**,
  or simply focus the chip and press `r`, to reject the mapping. A "Rejected
  {tag} · Undo" flash appears immediately; clicking **Undo** in the flash window
  restores it. The same control appears as a **Restore** link in the per-file
  "Rejected" disclosure once the flash fades.
- **The Hide item only appears on tags with `origin = 'pulled'`.** A tag you
  applied yourself that a repo *also* asserts will show `origin = 'both'` —
  the menu item is absent in that case, because rejecting the repo's copy would
  leave your local tag visible, reading as "rejected but still there". Delete
  your own local copy if you do not want it; **Hide from repo** will appear on
  the next pull.
- **Rejection is purely local.** The rejection row lives in your client DB; it
  never leaves the machine. The repo never learns you rejected the mapping.
- **Raw views stay unfiltered.** `GET /api/tags?raw=true` still returns the
  rejected mapping. Rejection shapes the effective display and search paths
  only; it is not a deletion.
- **Rejection is reversible.** Undo removes the rejection row; the mapping
  reappears in display and search immediately.
- **The local service is exempt.** If you do not want your own local tag,
  delete it directly.

The daemon exposes `POST /api/reject` (reject) and `DELETE /api/reject` (undo)
for scripting or tooling built on the daemon HTTP API; `GET /api/rejections`
lists current rejections, optionally scoped to one file with `?hash=<hash>`.
These are daemon HTTP routes, not CLI subcommands.

### Reporting a mapping to the repo

After rejecting a mapping you may want to escalate: tell the origin repo that
the tag is wrong so moderators can act for everyone. This is a **separate,
explicit step** — rejecting never automatically sends anything to the network.

The UI offers to file a report after you reject a mapping (via `ReportModal`).
Enter an optional reason and send. The report is fire-and-forget — no local
tracking, no status polling. A success toast confirms the network send. The
report reveals the file's BLAKE3 hash to the origin server.

**Report and reject are intentionally unbundled:** the local rejection is silent
and immediate; the report is a separate, confirmed network act. Rejecting without
reporting is always valid.

### Per-repo derived contributor keys

When you submit tags or file reports, your client signs with a key derived from
a local master seed (`naiad.master`, beside `naiad.db`). The derivation is
per-repo: `BLAKE3::derive_key("naiad-contributor:v1", master ‖ repo_anchor)` →
an Ed25519 account. The result:

- **Reading is still completely anonymous.** Bucket pulls carry no identity, as
  always.
- **Writing is pseudonymous, scoped per repo.** The repo sees *some* pubkey;
  that key is stable within the repo (giving you accountability and ownership of
  your submissions) but is unrelated to the key any *other* repo sees. No
  cross-repo observer can join your activity into one profile.
- **The anchor** is `caps.repo_key` if the server advertises one, else the
  normalized URL. It is frozen write-once on first submit.

Client settings live in `naiad.toml` beside your database. On first run the
daemon scaffolds a fully-commented file documenting every editable setting at
its default. The Hydrus DB directory (`[hydrus] dir`) is also stored here, so it
persists across restarts — no need to re-enter it each session.

Logging is configured under `[log]`:

```toml
[log]
level = "debug"        # bare level (error|warn|info|debug|trace) or full
                       # per-target directives, e.g. "info,db=debug,scan=debug".
                       # Default: info.
console = true         # auto-open the desktop debug console at launch.
file = "naiad.log"     # also append logs to this file. Relative = beside the DB.
```

`level` is the default filter when `RUST_LOG` is unset (`RUST_LOG` overrides it,
and it takes effect on the next daemon start). `console = true` opens the debug
console without needing the `--console` flag or `NAIAD_CONSOLE` env var (see
below). `file` adds a second sink that appends every log line to the given path
(stderr always stays); a relative path is resolved next to the database, and the
file is opened in append mode so runs accumulate.

---

## 6. Desktop app (Tauri)

The portable desktop shell spawns the `naiad` daemon as a bundled sidecar on a
free port — no separate `naiad daemon` to start.

```powershell
npm --prefix ui run build        # ui/dist (the sidecar serves it)
cargo build                      # builds the naiad sidecar binary
npm --prefix ui run tauri dev    # opens the native window
```

The window opens right away on a bundled loading page (`ui/loading.html`, built as
a second Vite entry alongside `index.html`) rather than waiting for the daemon. It
shows the daemon's own latest output line — `still opening database...` and the
like — and navigates to the gallery once the daemon binds its port. A daemon that
fails to start turns that page into an error state with the message, the last 20
lines of daemon output, and **Copy details** / **Quit** buttons; the app no longer
puts up a blocking native dialog and exits.

The shell also remembers where you left it. Its size and position land in
`window-state.json`, and whether the inspector is collapsed lands in
`view-state.json` — both in the app config directory
(`%APPDATA%\app.naiad.desktop\` on Windows), separate from the daemon's
`naiad.toml`. The window is created hidden and shown once the saved bounds are
applied, so it never flashes at the default 1200×800 first. Bounds that no longer
make sense — smaller than the enforced 700×500 minimum, or on a monitor you have
since unplugged, leaving no grabbable strip of title bar on any display — are
discarded and the window is centred instead. Persistence is best-effort: a state
file that cannot be read or written logs to stderr and never blocks startup, and
deleting either file restores the defaults. In a browser or under `vite dev` the
inspector preference falls back to `localStorage`, so the UI still works
standalone.

Point it at an existing library during dev:

```powershell
$env:NAIAD_DB = "C:\path\to\naiad.db"
npm --prefix ui run tauri dev
```

**Database path resolution** (first-set-wins ladder, same across desktop shell and CLI):
`--db` flag → `NAIAD_DB` env (non-empty) → `<exe dir>/naiad.db`.
The CLI now honors `NAIAD_DB` and defaults to `<exe dir>/naiad.db`; the old
cwd-relative `naiad.db` default is gone (breaking change for scripts).

### Debug console

To watch the daemon while the desktop app runs, use the **first-set-wins**
ladder: `--console`/`--no-console` flag → `NAIAD_CONSOLE` env → `[log].console`
in `naiad.toml` → off.

```powershell
.\naiad-desktop.exe --console          # flag wins
.\naiad-desktop.exe --no-console       # turns off even if TOML says true
$env:NAIAD_CONSOLE = "1"               # env tier; "0" or "false" = off
# or persistently: console = true in naiad.toml [log] section
```

`NAIAD_CONSOLE=0` now **overrides** `console = true` in `naiad.toml`. The old
behavior (any layer turning it on was irreversible) has been replaced with a
strict first-set-wins resolution.

On Windows this opens a real console window; the app relays every daemon
stdout/stderr line into it, prefixed `[daemon]`, for the app's lifetime. On
other platforms the option is a no-op — launch from a terminal instead. The
daemon logs its operations (imports, scans, tag add/remove, thumbnail
generation, Hydrus imports, searches, watch events, root changes) via `tracing`
at `info` by default; set `RUST_LOG` (or `[log] level`) to change the filter.
Each subsystem has its own target, so you can turn up just one:
`RUST_LOG=naiad_daemon=info,thumb=debug,search=debug` — targets are `scan`,
`tags`, `watch`, `thumb`, `search`, `hydrus`, `settings`, `startup`, `db`. The sidecar
daemon inherits the environment.

`npm --prefix ui run tauri build` compiles **only the desktop shell** to
`ui/src-tauri/target/release/naiad-desktop.exe` — it does *not* produce the
`naiad.exe` sidecar (that's `cargo build --release`) and does *not* assemble the
two into a distributable (`bundle.active` is `false`). For the full one-command
portable zip, use `npm run package` below — don't ship the bare `tauri build` exe
on its own; it can't find a sidecar to spawn.

> `ui/src-tauri` is **excluded from the Cargo workspace** — it's built by the
> `tauri` npm scripts, not `cargo build`/`cargo test --workspace`. Build the
> `naiad` binary with `cargo build` first so the sidecar exists.

**Where each build command puts its output:**

| Command | Output | Used by |
|---------|--------|---------|
| `npm --prefix ui run build` | `ui/dist/` | debug daemon serves it live; `--release`/Tauri bake it in |
| `cargo build` | `target/debug/naiad`, `naiad-repo` | dev daemon + CLI, repo node |
| `cargo build --release` | `target/release/naiad.exe` | the sidecar daemon shipped in the zip |
| `npm --prefix ui run tauri build` | `ui/src-tauri/target/release/naiad-desktop.exe` | the desktop shell (needs a sidecar beside it) |
| `npm --prefix ui run package` | `dist/Naiad-<version>-windows-x64-portable.zip` | the shippable portable release |

### Package the portable zip

One command builds and zips a versioned, self-contained release:

```powershell
just package                   # → dist/Naiad-<version>-windows-x64-portable.zip
# equivalently: npm --prefix ui run package
```

It asserts the version is consistent across all four manifests (source of
truth: workspace `Cargo.toml`), runs the build order, and hand-assembles
`naiad-desktop.exe` + `naiad.exe` into the zip. Unzip anywhere and double-click
**`naiad-desktop.exe`**; `naiad.db` and `thumbs.db` are created beside it.
WebView2 (the evergreen system runtime on Win10/11) is the only prerequisite.
See [ADR 0011](adr/0011-windows-portable-release.md).

The server (repo node) has its own releases: `just package-server` builds the
Windows zip `dist/Naiad-repo-<version>-windows-x64-portable.zip`, and
`just package-server-tar [target]` builds the Linux tarball
`dist/naiad-repo-<version>-<target>.tar.gz` (default
`x86_64-unknown-linux-musl`). Both contain the server binary, a sample
`repo.toml`, and the operator guide. See `docs/operating-a-repo.md`,
[ADR 0022](adr/0022-server-portable-release-and-repo-toml.md), and
[ADR 0027](adr/0027-server-linux-macos-tarball.md).

> **Importing from the desktop app.** For the common case you no longer need the
> terminal: click the **settings** button (the sliders glyph) in the top bar to
> open **Settings**, and use its
> **Folders** section — pick a folder with the native picker (or paste a path)
> and **Scan**. It indexes into the *same* daemon the GUI already uses, then the
> gallery refreshes. While a scan runs you'll see a live **indexed N · M skipped**
> count tick up. The **Folders** section also lists every folder being watched and
> lets you stop watching one with the **×** button. That opens a confirm dialog:
> **Keep files** leaves them indexed and visible, **Hide files** drops them from
> the gallery (marked missing — nothing is deleted, and a re-scan brings them
> back). Either way live-watching stops. The CLI rendezvous below is only
> needed for scripted or headless scans.
>
> **Settings.** The **Settings** modal (the sliders glyph, top bar) is organized into tabs. **Display** holds the thumbnail size and a **local tags only** toggle that hides pulled (synced) tags from the gallery and search. **Library** holds the Folders controls (scan + watched roots). Changes autosave — a brief "Saved." confirms each.
>
> **CLI vs. the desktop app — mind the port.** The desktop app spawns its daemon
> on an **ephemeral** port (discovered internally), while the standalone CLI
> defaults to `127.0.0.1:8080`. So `naiad scan …` in a separate terminal will
> **not** reach the running desktop app's daemon (`Connection refused`). To use
> the CLI against the same library, close the app and run your own daemon on the
> default port from the unzipped folder — `.\naiad.exe daemon` (uses `.\naiad.db`
> on `:8080`) — then `.\naiad.exe scan <folder>` in a second terminal.

---

## 7. Quality gates (run before calling anything done)

```powershell
cargo test --workspace           # all Rust crates
cargo fmt --all -- --check       # formatting
cargo clippy --workspace --all-targets   # lints (workspace denies warnings)

npm --prefix ui run build        # svelte-check (0 errors) + production build
npm --prefix ui test             # Vitest UI unit tests
```

---

## 8. Reset / clean slate

Everything is files on disk — delete them for a fresh start:

```powershell
Remove-Item naiad.db, repo.db, thumbs.db -ErrorAction SilentlyContinue
```

(WAL sidecars `naiad.db-wal` / `naiad.db-shm` and `thumbs.db-wal` / `thumbs.db-shm`
are recreated automatically; remove them too if you want a truly clean directory.)

> **Upgraded from ≤ 0.2.46?** The old per-file thumbnail cache (`thumbnails/` beside
> the database) is no longer used — thumbnails are now stored in `thumbs.db`. The old
> directory is harmless but takes disk space; delete it to reclaim it:
> `Remove-Item thumbnails -Recurse -Force -ErrorAction SilentlyContinue`
