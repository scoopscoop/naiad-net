# Running a Naiad Repo — An Operator's Guide

So you want to run a repo. Great. This guide walks you through it in plain
terms: what a repo is, how to start one, how to manage accounts and moderation,
and what you are (and aren't) signing up for.

You do **not** need to read the architecture docs first. If a word needs
explaining, it's explained here the first time it shows up.

> **Audience:** anyone who wants to host tags for a community. You don't need
> to be the person who wrote Naiad. You do need to be comfortable running a
> command in a terminal and leaving a program running.

---

## What a repo actually is

A **repo** (repository node) is a small server that holds **tags**, not files.

That's the whole trick. A tag is just a label attached to a file's
*fingerprint* — never the file itself. When someone hashes a picture, they get
a unique fingerprint for it (Naiad uses BLAKE3, but you don't need to care
which). Your repo stores rows that say "the file with this fingerprint has the
tag `character:samus`." It never stores, serves, or even sees the picture.

People run a Naiad client on their own machines. They point it at your repo and
ask, "do you have tags for any of the files I own?" Your repo answers with the
matching tags. Their files never move. Nothing of theirs is uploaded.

**Why this matters for you as an operator:** you are hosting text labels, not
media. The risk profile is closer to a search index than a file host. (The
project's stated goal is "torrent-tracker-level" — you point at things, you
don't serve the things.)

---

## Before you start

You need:

- The `naiad-repo` program — see the two options below.
- A folder to keep the repo's database in. A repo is just a file on disk; back
  up that file and you've backed up the repo.
- A way to leave a program running and reachable on the network (an always-on
  machine, a small VPS, a home server with a port forwarded — your call).

### Option A: download the portable release

**Windows.** Grab `Naiad-repo-<version>-windows-x64-portable.zip` from the
release page, unzip it anywhere, and you have:

- `naiad-repo.exe` — the repository node
- `repo.toml` — a commented sample config (every value shown is the default)
- `README.md` — this guide

Run `naiad-repo.exe serve` from that folder and you're serving on
`127.0.0.1:9090` with a fresh `repo.db` beside the exe. No Rust toolchain
needed.

**Linux.** Grab `naiad-repo-<version>-<target>.tar.gz` for your architecture
(e.g. `x86_64-unknown-linux-musl` — a static binary that runs on any x86-64
Linux, containers included, with no glibc version coupling). Unpack it and run:

```sh
tar xzf naiad-repo-<version>-<target>.tar.gz
cd naiad-repo-<version>-<target>
./naiad-repo serve
```

The tarball holds the same payload as the Windows zip — `naiad-repo`,
`repo.toml`, `README.md` (this guide), and `LICENSE`. No Rust toolchain needed.
For a containerised deployment the Docker image (see the repo `Dockerfile` /
`docker-compose.yml`) is usually the better fit; the tarball is for bare VPS /
home-server installs. Maintainers build these with `just package-server-tar
[target]` (see `scripts/package-server.sh`).

### Option B: build from source

If you have the Naiad source tree and a Rust toolchain, the binary is in
`target/debug/` after `cargo build` (or `target/release/` after
`cargo build --release`). During development you can also run it without
installing via `cargo run -p naiad-server -- …` — everywhere below that's the
same as typing `naiad-repo …`.

### The built-in help

`naiad-repo` with no arguments prints the command list, worked examples, and the
environment variables it reads. Each subcommand carries its own examples —
`naiad-repo help serve` shows the flags next to their `repo.toml` equivalents,
`naiad-repo help seed` shows the JSONL format, `naiad-repo help bridge` covers
the PTR mirror. This guide is the long form of the same material; when you
change one, change the other (`crates/server/src/main.rs`).

---

## Configuration (`repo.toml` and environment)

> **You can skip this section** — the defaults work fine for trying things out.
> Come back when you want to change the bind address, adjust logging, or set a
> repo identity key.

`naiad-repo serve` reads `repo.toml` from the directory of the database
(`--db`, default `repo.db` in the working directory). If the file is absent,
the first run writes a fully-commented template — the same template shipped in
the zip.

**`[serve]` keys:**

- `addr` — the address and port to listen on. Default `127.0.0.1:9090`
  (localhost only). Change to `0.0.0.0:9090` to accept connections from other
  machines.
- `k` — crowd-size floor for the k-anonymity bucket scheme (ADR 0001).
  Default `1000`. Leave it alone unless you know you want something different.
- `repo_key` — an optional 64-char hex Ed25519 public key advertised in
  `/repo/caps` so clients can pin your repo's identity. Omit it and clients
  fall back to anchoring on your URL, which works fine.
- `name` — an optional display name advertised in `/repo/caps`. Subscribers
  capture it once at subscription time as the local subscription name. Blank
  means unset; the subscriber falls back to the URL hostname.

**`[log]` keys:**

- `level` — how much to log: `trace`, `debug`, `info`, `warn`, `error`, or
  `off`. Default `info`. Override with `NAIAD_REPO_LOG_LEVEL` (bare level
  name only — a non-level bare word is a startup error). For per-target
  directives (`naiad_server=debug,info`) reach for `RUST_LOG`, which takes
  the top spot in the ladder.
- `console` — whether to write log lines to stderr. Default `true`. Set to
  `false` if your process supervisor captures stderr itself. Env:
  `NAIAD_REPO_LOG_CONSOLE`.
- `file` — an optional path for an additional append-mode log file. Relative
  paths resolve beside the database. Omit it and logs only go to stderr (or
  nowhere, if `console` is also `false`). Env: `NAIAD_REPO_LOG_FILE`.

**`[stats]` keys:**

Built-in statistics subsystem (v0.2.132+). Records request, system, store,
and sync metrics into a separate `stats.db` (beside the repo DB) and serves a
self-contained dashboard on a loopback-only second port. Privacy-preserving:
unique-user counts use a daily-rotated, memory-only salt (`blake3(salt||ip)`);
raw IPs are never written to disk.

- `enabled` — enable the stats listener, samplers, and request middleware.
  Default `true`. Env: `NAIAD_REPO_STATS_ENABLED`.
- `listen` — bind address for the stats dashboard. **Must be a loopback
  address** (`127.0.0.0/8` or `::1`). Default `127.0.0.1:9092`. A
  non-loopback address without `allow_non_loopback = true` is a fatal startup
  error. Env: `NAIAD_REPO_STATS_LISTEN`.
- `allow_non_loopback` — escape hatch: permit a non-loopback stats bind.
  Default `false`. Only set this if you deliberately front the stats port
  behind your own access control. Env: `NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK`.
- `db_path` — path for the stats database. Relative paths resolve beside the
  repo DB. Default `stats.db`. Delete the file to reset all history. Env:
  `NAIAD_REPO_STATS_DB`.

Access from a remote machine via SSH tunnel:
`ssh -L 9092:localhost:9092 <host>`, then open `http://localhost:9092`.

### Environment variables

Every `[serve]` and `[log]` setting, plus the database path, can be set
through environment variables instead of (or in addition to) `repo.toml`.
All names use the `NAIAD_REPO_` prefix that `NAIAD_REPO_BRIDGE_*` established.

| Variable | What it sets | Flag | File key |
|---|---|---|---|
| `NAIAD_REPO_DB` | Repository database path | `--db` | — (bootstrap tier) |
| `NAIAD_REPO_ADDR` | Bind address | `serve --addr` | `[serve].addr` |
| `NAIAD_REPO_K` | k-anonymity crowd floor | `serve --k` | `[serve].k` |
| `NAIAD_REPO_KEY` | Repo identity key in `/repo/caps` | `serve --repo-key` | `[serve].repo_key` |
| `NAIAD_REPO_NAME` | Optional display name advertised in `/repo/caps`; clients use it as the repo's local name when subscribing | — | `[serve].name` |
| `NAIAD_REPO_HASH_DOMAIN` | **Native** hash domain (`blake3` or `sha256`) | `serve --hash-domain` | `[serve].hash_domain` |
| `NAIAD_REPO_BRIDGE_ENABLED` | Serve an added sha256 domain (`true`/`false`) | — | `[bridge].enabled` |
| `NAIAD_REPO_BRIDGE_MODE` | sha256 backend: `sidecar`, `mirror`, or `snapshot` | — | `[bridge].mode` |
| `NAIAD_REPO_BRIDGE_SNAPSHOT_DIR` | Hydrus snapshot directory (snapshot mode) | — | `[bridge].snapshot_dir` |
| `NAIAD_REPO_BRIDGE_MAX_QUERY_BITS` | Precision ceiling for sha256 queries | — | `[bridge].max_query_bits` |
| `NAIAD_REPO_BRIDGE_MIN_QUERY_BITS` | Minimum prefix bits for sha256 bucket queries (floor) | — | `[bridge].min_query_bits` |
| `NAIAD_REPO_BULK_CACHE_MB` | Page cache for `bridge seed` (MB, 64–16384; default 256) | — | — |
| `NAIAD_REPO_LOG_LEVEL` | Log level (bare: `trace`…`off`; or `target=level` directive) | — | `[log].level` |
| `NAIAD_REPO_LOG_CONSOLE` | Emit to stderr (`true`/`false`) | — | `[log].console` |
| `NAIAD_REPO_LOG_FILE` | Append-mode log file path | — | `[log].file` |
| `RUST_LOG` | Log filter (expert, beats `NAIAD_REPO_LOG_LEVEL`) | — | — |
| `NAIAD_REPO_STATS_ENABLED` | Enable stats subsystem (`true`/`false`) | — | `[stats].enabled` |
| `NAIAD_REPO_STATS_LISTEN` | Stats dashboard bind address (`IP:port`) | — | `[stats].listen` |
| `NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK` | Permit non-loopback stats bind (`true`/`false`) | — | `[stats].allow_non_loopback` |
| `NAIAD_REPO_STATS_DB` | Stats database path | — | `[stats].db_path` |

Notes:
- `RUST_LOG` takes the top spot in the log-level ladder: `RUST_LOG` →
  `NAIAD_REPO_LOG_LEVEL` → `[log].level` → `info`. Use `NAIAD_REPO_LOG_LEVEL`
  for the deployment-shaped knob; reach for `RUST_LOG` only for per-target
  debug sessions (e.g. `RUST_LOG=sync=debug,http=trace`).
- `NAIAD_REPO_KEY` sets the **public** identity key advertised in `/repo/caps`.
  It is *not* the operator's signing key; that stays in `repo.key` beside the
  database and is never read from the environment.
- An empty `NAIAD_REPO_KEY`, `NAIAD_REPO_NAME`, or `NAIAD_REPO_LOG_FILE` is
  an explicit **"none"** (it beats the file tier and disables that setting).
  All other empty values are treated as absent (fall through to the next tier).
- `NAIAD_REPO_ADDR` expects an `IP:port` pair. A hostname is not accepted.
- A malformed value (e.g. `NAIAD_REPO_ADDR=0.0.0.0.9090`) aborts startup
  with an error naming the variable — the server refuses to start rather than
  silently fall back to the built-in default.
- The `NAIAD_REPO_BRIDGE_*` variables above mirror the `[bridge]` section; see
  "Serving Hydrus PTR tags" below. `NAIAD_REPO_BRIDGE_PTR_URL`,
  `_PTR_KEY`, `_STATE_DB` and `_SNAPSHOT_SERVICE_ID` follow the same pattern
  and are covered by `naiad-repo help bridge`. As of 0.2.80 a blank or
  whitespace-only value for `_PTR_URL`, `_PTR_KEY` or `_STATE_DB` is treated as
  absent (falls through to the file/default) like every other key — earlier
  releases passed the empty string through, which resolved `_STATE_DB=""` to the
  parent directory and failed to open.

Precedence: CLI flag → environment variable → `repo.toml` → built-in default,
**first-set-wins**. Any tier that is set and differs from the winning tier
produces a warning line at startup. The first-run template ships every key
commented, so an untouched file never triggers a cross-tier warning.

> **Upgrading from ≤0.2.48?** Earlier versions wrote `addr`, `k`, `level`, and
> `console` at their defaults (uncommented). If you now set the matching
> `NAIAD_REPO_*` variables you will see a cross-tier warning on every boot.
> Comment out any keys in `repo.toml` that you have moved to environment
> variables to silence it.

A malformed `repo.toml` is a startup error: the server refuses to start rather
than silently run with wrong settings. With
`console = false` and no `[log] file`, the server is fully silent (including
the listening line). Only `serve` reads the file; the admin subcommands
(`seed`, `account`, `report`) ignore it, though `NAIAD_REPO_DB` is honoured
by all subcommands.

---

## Step 1 — Start the server

The simplest possible repo:

```powershell
naiad-repo --db repo.db serve
```

That's it. `--db repo.db` is the database file (created for you the first
time). `serve` starts listening. By default it binds to `127.0.0.1:9090`,
which means **only your own machine can reach it** — perfect for trying it out.

> **If you're running from the unzipped folder,** `repo.db` is the default
> database location (beside the exe), so `naiad-repo.exe serve` and
> `naiad-repo.exe --db repo.db serve` do the same thing. The `--db` flag is
> only needed when you want the database somewhere else.

To let other people reach it, bind to a public address and pick your port:

```powershell
naiad-repo --db repo.db serve --addr 0.0.0.0:9090
```

`0.0.0.0` means "listen on every network interface." Now anyone who can reach
your machine on port 9090 can pull tags. Put it behind a reverse proxy
(nginx, Caddy) if you want TLS / a hostname — Naiad doesn't require it, but
it's a nice thing to add.

### The `--repo-key` option

```powershell
naiad-repo --db repo.db serve --addr 0.0.0.0:9090 --repo-key <64-char-hex>
```

`--repo-key` advertises your repo's public identity key in `GET /repo/caps`.
Clients use this key as the anchor for deriving their per-repo contributor
pseudonym — so a client's identity at your repo is stable and unlinkable to
their identity at other repos. If you omit `--repo-key`, clients fall back to
anchoring on your repo's URL, which works fine.

Generate a repo key once with `naiad-repo identity keygen` and store the output
key hex somewhere safe; pass it at every `serve`. It does not need to be secret.

### The `--k` setting

You'll see a `--k` flag. It controls a privacy feature for the *people pulling
from you*, not for you. Here's the short version:

When a client asks for tags, it doesn't want to hand you the exact fingerprints
of every file it owns — that list *is* an inventory of their library. So
instead it asks for a **bucket**: "give me everything whose fingerprint starts
with these few characters." Lots of unrelated files share that prefix, so the
client hides in the crowd. `--k` is the crowd-size floor — how many files must
share a bucket before the server uses this bucketed mode.

The default is `1000`. Small repos (fewer than `k` files) just serve everything
at once, which is fine and simple. **For most operators: leave it alone.**

---

## Step 2 — Put some tags in it

A brand-new repo is empty. You seed it with the `seed` command — one
fingerprint, one tag at a time:

```powershell
naiad-repo --db repo.db seed <64-character-fingerprint> character:samus
naiad-repo --db repo.db seed <64-character-fingerprint> series:metroid
```

You get the fingerprint from any Naiad client with `naiad list`. (In real use,
most tags won't come from you typing them — they'll come from people *submitting*
tags to your repo over the network. More on that below. `add` is for seeding and
admin work.)

To check it worked, open `http://your-host:9090/repo/snapshot` in a browser.
You'll see the tags as plain JSON. That endpoint is public on purpose — see
the trust model below.

### Bulk-seeding from a client library

If you want to pre-populate a fresh repo from an existing Naiad client library,
the two-command offline workflow is faster than typing hashes one by one:

```powershell
# Step 1 — export current mappings from the client (daemon not required)
naiad export-mappings --db C:\path\to\naiad.db --out mappings.jsonl

# Step 2 — load them into the repo (idempotent: already-current rows are skipped)
naiad-repo --db repo.db seed --from-file mappings.jsonl
# → seeded: X inserted, Y skipped, Z total
```

The export is **offline and read-only** — the daemon does not need to be running.
Each line of `mappings.jsonl` is a JSON object `{"hash":"<64-hex blake3>","tag":"..."}`.
Only active-file mappings from local services are exported.

Re-export and re-seed after another tagging session to do an incremental sync;
rows already in the repo are skipped, so re-running is safe and cheap. This
makes the pair a drop-in for periodic syncing: schedule it, script it, or just
run it manually before handing the DB to another operator.

The JSONL format is also suitable for non-Naiad sources: any tool that can
write one `{"hash":"...","tag":"..."}` JSON object per line can feed the bulk
seed. A malformed line causes a fast-fail with `file:line` and nothing is
written.

---

## Serving Hydrus PTR tags alongside your own (the bridge)

Turning the bridge on **adds** a second hash domain to your repo. It does not
convert your repo into something else: your own BLAKE3-keyed tags keep being
served exactly as before, on the same URL, to the same clients. `/repo/caps`
grows a `hash_domains` list; a client built before this change reads the old
`hash_domain` field, sees a plain BLAKE3 repo, and carries on unaffected.

(Older releases behaved differently: `[bridge].enabled = true` silently forced
`hash_domain = "sha256"` and made an explicit `blake3` a fatal startup error.
Both behaviours are gone. `[serve].hash_domain` now means only one thing: the
domain your own store is keyed by.)

There are three backends for the added SHA-256 domain.

### Bridged PTR relations — siblings & parents (0.2.120+)

The PTR ships **tag siblings** (aliases: `samus` → `character:samus aran`) and
**tag parents** (implications: `character:samus aran` → `series:metroid`)
alongside its file→tag mappings. The bridge applies these into your repo's
native, tag-keyed **relations** table — the same one `GET /repo/relations`
already serves incrementally — so clients pulling your corpus get PTR
sibling-collapse and parent-implication for free, over the existing relation
sync path. No new endpoint, no client change.

Relations are **tag-keyed and domain-independent**, so they live only in the
`RepoStore` (your native store), regardless of which SHA-256 backend serves
mappings — including sidecar deployments, which always have a native store
beside the sidecar file. The sidecar mapping file itself is untouched.

Because the relations table requires a signing author, the bridge mints one
dedicated **bridge-author key**, persisted as `bridge-author.key` beside your
`state_db` (mode `0600` on Unix). It is distinct from your operator `repo.key`,
so a client can trust-weight or mute the PTR relation source without affecting
your own relations. Keep it with your bridge state; it is regenerated (as a new
identity) if deleted, which makes clients re-evaluate the source. Bridged
relations are applied **last-writer-wins** (the PTR is authoritative), and an
edge's `seq` is bumped only when its status actually changes, so replaying an
update after a crash does not force clients to re-pull the whole graph.

- **Mirror mode:** `bridge seed` backfills the full sibling/parent tables as an
  extra seed phase, and the follow-loop keeps them fresh. Nothing extra to run.
- **Sidecar / snapshot mode:** `bridge seed` builds only the mapping file, so
  backfill the relations into the native store as a separate step:

  ```
  naiad-repo --db repo.db bridge seed-relations <snapshot-dir>
  ```

  This is idempotent — safe to re-run. The follow-loop (or `bridge sync`) then
  keeps the relation graph current.

### Sidecar mode — the recommended PTR backend (0.2.96+)

The sidecar index is a compact hash-ordered `bucket_map` that replaces the
materialized full RepoStore mirror as the standard PTR deployment. It is
roughly 26 GB self-contained at full PTR scale (vs. hundreds of GB for the
mirror), covers both coarse k-anonymity and exact-hash queries, and follows
the live PTR via the same sync loop and `bridge sync` cron as mirror mode.

```toml
[serve]
hash_domain = "blake3"        # your own repo, unchanged

[bridge]
enabled = true
mode = "sidecar"
ptr_url = "https://ptr.hydrus.network:45871"
state_db = "bridge-state.db"
```

**Workflow:**

1. Download a Hydrus PTR snapshot.
2. Set `SQLITE_TMPDIR` to a directory on a real disk (not `/tmp` on tmpfs) with
   at least 50 GB free — Phase M's sort spill lands there.  Example:
   `export SQLITE_TMPDIR=/mnt/nvme/tmp`.
3. Seed the sidecar: `naiad-repo bridge seed <snapshot-dir>` — builds the
   compact index from the snapshot.  Phase D (defs) takes roughly one hour on
   the full PTR.  Phase M runs in two sub-phases:
   - **M1 (stage):** streams `hash_id` bands off the `(hash_id, tag_id)`
     covering index sequentially (no transient per-band index build) and appends
     each band's packed rows to a sequential `staging.db` beside `sidecar.db`.
     `staging.db` is a temporary file that appears during seeding and is deleted
     on completion — do not remove it manually mid-seed.
   - **M2 (merge):** does one hash-ordered `INSERT OR REPLACE` from
     `staging.db` into `bucket_map`, visiting each B-tree leaf ~once.  This
     eliminates the random-RMW cost that made cold insertions decay to 25k rows/s
     on a large tree.  Target: ~2 h at full PTR scale (3.6 B mappings).
4. Backfill relations into the native store (siblings/parents live there, not in
   the sidecar file): `naiad-repo bridge seed-relations <snapshot-dir>`. See
   *Bridged PTR relations* above. Idempotent; the follow-loop keeps them fresh.
5. Optionally audit: `naiad-repo bridge parity-audit <snapshot-dir>` — compares
   sidecar digest vs. snapshot digest to confirm faithful ingest.
6. Start serving: `naiad-repo serve` with the config above. The follow-loop
   starts automatically when `[bridge].enabled = true` and `mode = "sidecar"`,
   keeping the index current via PTR update files. During a long catch-up
   (v0.2.126+) expect a pass-start line (pending update count), one line per
   fetched update entry, and a progress heartbeat every ~1 M applied rows with
   an interval rows/s figure — hours of silence mean a problem, not a slow
   apply.
7. Alternatively, run `naiad-repo bridge sync` as a nightly cron instead of
   the in-process follow-loop.

**`SQLITE_TMPDIR` is required for large seeds.** Phase M2 sorts the entire
`staging.db` (40-90 GB at full PTR scale) by hash using SQLite's external merge
sort.  If `SQLITE_TMPDIR` is unset, the sort spills to `/tmp`, which is often
**tmpfs (RAM)** on Linux — a 90 GB sort there would OOM a 15 GB box.  Set
`SQLITE_TMPDIR` to a directory on a **real disk** with adequate free space
before running `bridge seed`.  The seeder also applies `PRAGMA
temp_store_directory` defensively on the connection, so setting the environment
variable before startup is sufficient.

**Single-writer requirement.** The old direct-write seed binary and the new
staged-seed binary must **not** run concurrently against the same
`sidecar.db`/`staging.db`.  Stop the old binary before starting the new one.
Each binary's per-band transaction atomicity makes a mid-band kill safe — the
killed band rolls back and is re-staged on resume.

**Crash and resume.** A crash mid-M1 leaves `staging.db` with some
`band_done` rows; re-running `bridge seed` skips already-staged bands and
continues.  A crash mid-M2 leaves `staging.db` intact and `seed_merge_done`
unset; re-running re-runs the entire merge (idempotent via `INSERT OR REPLACE`).
Once `seed_merge_done` is set, Phase M is complete and subsequent `bridge seed`
calls skip to the watermark step.

**Parity audit (sidecar).** `naiad-repo bridge parity-audit <snapshot-dir>`
compares a per-band BLAKE3 digest of the sidecar against the Hydrus snapshot.
The audit streams both sides in band order and is efficient regardless of
whether the snapshot has hash-led indexes — unlike the mirror audit, which
required those indexes or fell back to a full-table sort.

**`--rebuild`.** If the snapshot changes (e.g. you pull a newer one), run
`naiad-repo bridge seed <new-snapshot-dir> --rebuild` to wipe and rebuild the
sidecar index in place.  Rebuild clears `bucket_map`, all seed markers
(including `seed_merge_done`), and deletes any leftover `staging.db`, then
rebuilds from scratch via M1+M2.  A crash during rebuild resumes cleanly.

### Snapshot mode — no seed, no extra disk

Point the repo at a downloaded Hydrus database snapshot and it answers SHA-256
queries by reading it directly. There is no seed step, no materialized store,
and no PTR network access at all.

```toml
[serve]
hash_domain = "blake3"        # your own repo, unchanged

[bridge]
enabled = true
mode = "snapshot"
snapshot_dir = "/srv/ptr-snapshot"   # holds client.db, client.master.db, client.mappings.db
# snapshot_service_id = 14           # omit to auto-discover; set it for a full client DB
max_query_bits = 256                 # 256 allows exact-hash queries
```

- **Freshness is snapshot age.** To refresh, download a newer snapshot and
  restart. There is no delta sync in this mode.
- **Service auto-discovery, and when to pin it.** Omitting
  `snapshot_service_id` auto-discovers the tag service to serve. If the snapshot
  carries Hydrus' `services` table (a real client database does), the server
  picks the tag **repository** — the PTR — even when lower service ids exist.
  If the DB has several tag repositories, or the auto-picked service is empty
  while another holds mappings, the server **refuses to start** and names the
  candidate ids: set `snapshot_service_id` (or
  `NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID`) to the id you mean. A **full Hydrus
  client database** is the common case here — its low ids are local tag services
  such as "my tags" (often empty), and the PTR is a higher id (commonly `14`).
  Pin it explicitly to be safe: `snapshot_service_id = 14`, or
  `NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID=14`. (Before 0.2.65 auto-discovery took
  the *lowest* id with a mappings table, so a full client DB silently served an
  empty local service — every bucket query came back `{"tags":{}}`.) An explicit
  id is always honoured as-is and bypasses these checks.
- **A missing or unreadable snapshot is a fatal startup error**, naming the
  configured path. The server refuses to start rather than come up and serve
  empty results.
- **`hash_domain` must not be `sha256` in this mode.** Snapshot mode *adds* a
  SHA-256 domain beside your native one, so the native domain has to be
  something else — in practice `blake3`. Configuring both
  `[serve] hash_domain = "sha256"` and `[bridge] mode = "snapshot"` is a fatal
  startup error naming both settings. (Before 0.2.56 that combination started
  successfully and then answered every SHA-256 query from the wrong store —
  a cheerful `200` with no tags. If you run a mirror keyed by SHA-256, use
  `mode = "mirror"`.)
- **No PTR follow-loop runs in this mode.** The server logs that it is skipping
  it and reads only the static snapshot. (Fixed in 0.2.56: releases 0.2.52
  through 0.2.55 started the PTR sync loop whenever `[bridge].enabled = true`,
  regardless of `mode`, which meant a snapshot-mode repo quietly replayed PTR
  history into its own native store and kept talking to the PTR. If you ran
  snapshot mode on one of those releases, check `repo.db` for unexpected growth
  and for SHA-256-keyed mapping rows.)
- **Precision ceiling.** `max_query_bits` is the finest prefix this server will
  answer a SHA-256 query at; `256` means exact-hash lookups are allowed. Values
  below 8 are raised to 8 — a query must always carry at least one byte of
  prefix. That is sound when you are the operator your own clients trust — the
  k-anonymity dance exists to protect a client from an untrusted operator.
  While a snapshot backend is configured, this value is also the prefix width
  advertised in `/repo/caps`, for the native domain too, so set it to your
  k-anonymity width instead if your repo also serves people you do not know.
- **Floor for large snapshots (`min_query_bits`).** On a large snapshot (e.g.
  the PTR, ~200 million hashes, 89 GB mappings DB on spinning disk) a coarse
  bucket query is a doomed multi-GB random-read scan. `min_query_bits` (default
  8, same as the hard minimum) sets a floor: requests below this many prefix bits
  get an immediate 400 rather than triggering the scan. Raise it for the PTR:

  ```toml
  [bridge]
  mode = "snapshot"
  snapshot_dir = "/srv/ptr-snapshot"
  max_query_bits = 256
  min_query_bits = 16    # floor: below 16 bits → fast 400, not a doomed scan
  ```

  Bench (reference server, PTR snapshot): 8 bits never completed (killed
  at ~2 h after 9.6 GB of random reads); 16 bits succeeds at ~6 MB per 4-bucket
  request. At 16 bits the k-anonymity crowd is still ~3 000 files per bucket, so
  privacy is not harmed. Small repos should keep the default (8) — a high floor
  would shrink the crowd for all queries, and the scans are fast on smaller data.
  Env override: `NAIAD_REPO_BRIDGE_MIN_QUERY_BITS`. The effective floor is
  clamped into `[8, max_query_bits]` at startup, so it can never exceed the
  ceiling or fall below the hard minimum.

  **Advertised to clients (#179).** Since v0.2.73, `min_query_bits` is included
  in the `/repo/caps` response when a snapshot backend is configured. Clients
  whose privacy ceiling (`[privacy].max_query_bits`, or `[[repos]].max_query_bits`
  per repo) is below the floor will clamp their query width UP to the floor
  rather than 400ing — the pull succeeds, and the user sees a one-time toast
  explaining the concession. Cross-reference: `min_query_bits` (enforcement, #175)
  ↔ `/repo/caps` `min_query_bits` field (advertisement, #179).
- **Push is unavailable.** A submit aimed at the SHA-256 domain returns an
  explicit "push not available in snapshot mode" error. A static snapshot is by
  definition more than two weeks behind the PTR head, and Hydrus rejects
  contributions from a client that far behind, so accepting the submission
  would be a lie.
- **The whole-repo snapshot endpoint is refused** for the SHA-256 domain — a
  full dump of the PTR is hundreds of GB. A direct `GET /repo/snapshot?domain=sha256`
  returns an explicit error pointing at `POST /repo/buckets`. Note that bucket
  enumeration can still walk the entire dataset (all 256 one-byte-prefix buckets
  cover the full keyspace); snapshot mode assumes clients trusted by the operator,
  consistent with the `max_query_bits = 256` default.

### Client-side: precise queries for snapshot repos

Snapshot mode assumes clients trusted by the operator (consistent with
`max_query_bits = 256`), but a naiad client defaults to a 24-bit privacy
ceiling. Coarse buckets force the server to range-scan the snapshot, which on
HDD-backed libraries exceeds the client's HTTP timeout — the pull fails every
time. Clients should raise the ceiling for this repo only:

```toml
[[repos]]
name = "myrepo"
url = "http://repo.example:9090"
# Full override of [privacy].max_query_bits for this repo. 256 = exact-hash
# queries: fast index seeks server-side, but reveals your exact file hashes
# to THIS repo. Only for repos whose operator you trust.
max_query_bits = 256
```

### Mirror mode — the eager materialized store

The pre-existing backend, unchanged: `naiad-repo bridge seed <snapshot-dir>`
decodes a snapshot once into a SHA-256-keyed store (hours, hundreds of GB at
full PTR scale), and `naiad-repo bridge sync --follow` keeps it fresh from PTR
update files. Queries are served at coarse k-anonymity bucket granularity.
This is the backend for a public repo serving clients who have no reason to
trust you, and the only one that can stay near-live with the PTR.

```toml
[serve]
hash_domain = "sha256"        # a mirror store is SHA-256-keyed — say so explicitly

[bridge]
enabled = true
mode = "mirror"        # the default
ptr_url = "https://ptr.hydrus.network:45871"
state_db = "bridge-state.db"
```

**Upgrading from 0.2.51 or earlier:** older releases silently forced
`hash_domain = "sha256"` whenever `[bridge].enabled = true`; from 0.2.52 the
setting means only what it says. Existing mirror deployments must add
`[serve] hash_domain = "sha256"` explicitly, or the repo will advertise blake3
and serve no matching rows to clients querying the SHA-256 domain.

**Query-bit floor (`min_query_bits`).** As of 0.2.83 the `min_query_bits` floor
applies to any **sha256-domain** bucket query — whether sha256 is served as the
native domain (mirror mode) or an added domain (snapshot mode). On a PTR-scale
mirror, set `[bridge] min_query_bits = 16` (env
`NAIAD_REPO_BRIDGE_MIN_QUERY_BITS`): sub-floor queries get a fast `400` instead of
a doomed coarse scan, the floor is advertised in `/repo/caps`, and the advised
bucket width is lifted to at least the floor so a client that follows the advice
never sends a below-floor request. Blake3-native repos are unaffected (no floor).
`max_query_bits` remains a snapshot-mode ceiling and is inert in mirror mode.
The `serve_hint` a mirror repo advertises is the raw per-bucket latency EWMA with
no `hint_bits` normalisation (snapshot repos normalise; mirror repos do not),
which clients handle by assuming the hint was measured at the advertised width.

Mirror mode is verified end-to-end (`bridge seed` → `serve` with
`hash_domain = "sha256"` → client bucket pull → PTR follow-loop) as of v0.2.75.

**Serving a mirror statically, with no outbound (0.2.83+).** To serve an
already-seeded mirror with **zero egress** — no PTR follow-loop, nothing dialed
out — set `[serve] no_egress = true` (env `NAIAD_REPO_NO_EGRESS`, flag
`--no-egress`) and leave `[bridge] enabled = false`. `no_egress` is fail-closed:
combining it with `[bridge] enabled = true` is a fatal startup error, and the
server logs `egress: DISABLED` so the guarantee is visible. This is the intended
mode for a hardened, network-isolated mirror container.

**Read-only serving mode — `[serve].read_only` (0.2.89+).** Setting
`read_only = true` (env `NAIAD_REPO_READ_ONLY`) asserts that this process does
not accept write submissions. All four write endpoints — `POST /repo/submit`,
`/repo/report`, `/repo/moderate`, and `/repo/relations/submit` — return
`403 Forbidden` with the message `"this repo is serving read-only; writes are
disabled"`. Read endpoints (`GET /repo/caps`, `POST /repo/buckets`, etc.) are
unaffected. Pooled read connections (see `read_connections` below) additionally
receive `PRAGMA query_only = ON` and a 1 GiB `PRAGMA mmap_size` for read
throughput; any accidental write on those connections is rejected at the SQLite
level.

The writer store is still opened for WAL checkpoint on clean shutdown — do not
interpret `read_only` as "the file is not opened for writing." A separate
`bridge sync` process (or the nightly `bridge sync --follow` cron) writing the
same `repo.db` from another PID is safe: SQLite WAL allows multiple readers
alongside one writer; pooled read connections see committed changes on their
next transaction. `read_only` deliberately does **not** set `SQLite
immutable=1`, which would be unsafe when another process writes the same file
(design §10.1).

Typical use: a static-mirror container where a cron job invokes
`bridge sync` once per night as a short-lived child process, then the serving
process continues with zero write paths open to public clients.

**Read connection pool — `[serve].read_connections` (0.2.89+).** Sets the
number of read-only SQLite connections opened at startup for round-robin serving
of concurrent read requests (default 4, clamped to [1, 64], env
`NAIAD_REPO_READ_CONNECTIONS`). SQLite WAL lets all pool connections proceed
concurrently; this eliminates the serialisation bottleneck on multi-core hosts.
Raise it if `GET /repo/caps` or `POST /repo/buckets` latency is high under
concurrent load.

Independently, the distinct-hash count that backs `/repo/caps` is persisted in `repo_meta` at
seed time and maintained on every write, so caps requests are a single row
lookup — no scan. Pre-upgrade stores without a persisted count get a one-shot
background compute on first startup (0.2.89+).

**Write-lock window during sync (0.2.80+).** `bridge sync --follow` applies each
PTR update index by committing mappings in 50k-row chunks over its own
read-write connection. On a large index, consecutive chunks can hold the
`repo.db` WAL write lock long enough that concurrent HTTP submission writes (on a
separate connection) must wait — bounded by the 10-second `busy_timeout`, after
which the submission errors. The window scales with the size of the index being
applied; it is inherent to the chunk-commit approach, not a bug. If you run a
mirror that also accepts writes and see occasional submission stalls during
sync, this is why. `naiad-repo bridge status` opens both `repo.db` and the state
DB **read-only** (0.2.80+), so it never contends for that lock and is always safe
to run against a busy bridge.

**Seed speed depends on your snapshot's indexes (0.2.77+).** The seed streams
mappings in hash order when the snapshot has Hydrus's native
`current_mappings_<svc>_hash_id_tag_id_index` — that turns the store's B-tree
writes into cheap appends and is worth an order of magnitude, especially on
spinning disks. A normal Hydrus `client.mappings.db` always has this index; a
carved or rebuilt snapshot may not. The seed **detects** the index per table:
with it, you get an `INFO … ordering ingest by hash_id` line; without it, a
`WARN … streaming unordered` line and the slow path. To get the fast path on an
unindexed snapshot, build the index once on a **writable copy** (the seed opens
snapshots immutable and will not modify yours):

```sql
CREATE UNIQUE INDEX current_mappings_14_hash_id_tag_id_index
    ON current_mappings_14 (hash_id, tag_id);
CREATE UNIQUE INDEX deleted_mappings_14_hash_id_tag_id_index
    ON deleted_mappings_14 (hash_id, tag_id);
-- substitute your service id for 14; minutes even at hundreds of millions of rows
```

The seed connection also runs a bulk-ingest profile automatically (256 MiB page
cache, relaxed WAL checkpointing) — no configuration needed. `--unsafe-fast`
additionally sets `synchronous=OFF` for a further margin on slow disks, at a
real cost: **a machine crash or power loss mid-seed can corrupt `repo.db`, not
just lose recent rows. If a seed run under `--unsafe-fast` dies uncleanly,
delete `repo.db` and re-seed from scratch.** The default (no flag) cannot
corrupt the store and is the right choice unless you have measured the
difference and the mirror is disposable.

**Deferred hash index on fresh seeds (0.2.78+).** When seeding a **brand-new**
store from a hash-ordered snapshot (the normal case), the seed drops the
store's hash-uniqueness index during the big current-mappings pass — appends
replace ~200M random B-tree writes — and rebuilds it once, sequentially, before
the small deleted pass. You'll see `using deferred index path` and
`hash uniqueness index built` markers in the log. This engages automatically
and only when safe (fresh store + ordered source); resumed or unordered seeds
use the ordinary path. Two operational consequences:

- **A store mid-seed refuses to serve.** Until the index is rebuilt, `serve`
  (and `bridge sync`) exit with *"repo.db has an incomplete bridge seed"*. Not
  an error — finish or resume the seed.
- **Interrupted Format-A seeds resume from the last committed chunk (0.2.91+).**
  Every ~250k-row chunk of phase 1 writes a tiny `seed_ckpt` bookmark in the same
  transaction that commits the chunk (which pass, the highest fully-written source
  `hash_id`, the service id, and a cheap snapshot fingerprint). Re-running
  `bridge seed` resumes at `hash_id >` the bookmark instead of restarting from row
  zero — the win is largest for a day-scale `--rebuild` on the full PTR, which now
  skips the destructive wipe and continues where it stopped. A crash loses at most
  the current chunk (plus, on a cold start before the first chunk commits, a fall
  back to the older self-heal path — one chunk of lost work at worst).
- **Point a resume at the original snapshot.** The fingerprint (service id + biggest
  `hash_id` + the `client.mappings.db` file size) is checked at resume. If you point
  a plain `bridge seed` at a *different or refreshed* snapshot, it stops with an
  actionable error telling you to use the original snapshot, delete the store, or
  run `--rebuild` (which is always allowed to wipe and rebuild against a new
  snapshot). The older/unordered Format-B path keeps its restart-from-scratch
  behaviour.
- **`--unsafe-fast` resume covers process death only.** Bookmarks are written even
  under `--unsafe-fast`, so an OOM kill, an `ssh` drop, or a process crash still
  resumes. A power loss or OS crash under `--unsafe-fast` can corrupt the store
  regardless — the delete-and-re-seed contract above still applies.

Upgrading an **existing populated store** to 0.2.78 triggers a one-time rebuild
of the `repo_hashes` table at next open (migration 0004): seconds to a minute
per ~10M hashes, with a transient ~2× disk spike on that table while old and
new copies coexist.

**Picking a seed target (full-scale seeds).** Parity-array NAS targets
(Unraid and similar) are **write-IOPS-bound** during seeds: every random
B-tree write becomes a read-modify-write against the parity drive, and the
measured ceiling (~870 random-write IOPS on a healthy array, far less with an
SMR parity drive) extrapolates a full-PTR seed to ~10 days. Seed on
direct-attached flash instead, then copy the finished store to the array —
the copy is sequential and parity-friendly. A full-PTR seed (3.76B mappings)
on a 16 GB desktop with a QLC NVMe target takes ~30 hours; rates, regimes,
and RAM-sizing guidance are in
`docs/perf/2026-08-09-issue-189-full-ptr-seed.md`.

### Single-writer lock

At most one bridge writer may run against a store at a time. A writer is either
a `bridge sync` (including `--follow`) or a bridge-enabled `serve` with its
in-process sync loop.

**What enforces it.** The writer holds an **exclusive OS file lock** on
`bridge.lock` in the same directory as the state DB (default: beside `--db`).

**What happens on contention.** A second `bridge sync` that finds the lock held
prints `another bridge process appears to be running` to stderr and exits **4**
— a distinct code so cron wrappers and supervisors can decide skip-vs-alert
rather than lumping it in with real errors. A bridge-enabled `serve` that cannot
acquire the lock logs an error and skips the PTR follow-loop, but continues
serving read traffic normally.

**What is never gated.** `bridge seed`, `bridge status`, and `bridge
parity-audit` never acquire the write lock — they are read-only or write to a
separate pipeline, so they are always safe to run alongside a live writer.

**Crash recovery is automatic.** An OS file lock is released by the kernel when
the holder exits — even abnormally. There is no stale `bridge.lock` to clean
up; the file is harmless to leave in place.

### Auditing mirror parity

`naiad-repo bridge parity-audit <snapshot_dir> [--service-id N] [--band <hex-prefix>]`
computes a count and BLAKE3 digest of the sorted `(hash, tag)` mappings on both
the mirror store and a Hydrus snapshot, then checks they are identical. The
command is read-only and never modifies either store.

**`--band`.** Omitting the flag audits the full hash range. Pass e.g.
`--band 00` to audit only the slice of hashes whose SHA-256 starts with those
hex digits — useful for a quick spot-check or for working through a large store
in pieces.

**Alignment guard.** The audit only compares when the mirror's last-applied PTR
update index equals the snapshot's watermark. If there is a gap — the mirror is
ahead of the snapshot, or behind it — the command prints a diagnosis naming both
positions and **exits 3 without running the comparison**. This means a FAIL (exit
2) always indicates real divergence, never temporal skew. A PASS is exit 0.

**Two recipes:**

- *Post-seed audit.* Right after `bridge seed <snapshot_dir>`, audit that same
  snapshot. Alignment is free (the seed writes the snapshot watermark as the
  applied index), so this verifies the seed without any extra steps:

  ```sh
  naiad-repo bridge seed /srv/ptr-snapshot
  naiad-repo bridge parity-audit /srv/ptr-snapshot
  ```

- *Periodic audit.* Take a fresh PTR snapshot, advance the follow-loop to its
  watermark (`bridge sync --follow` — stop once it reaches that index), then
  audit:

  ```sh
  naiad-repo bridge parity-audit /srv/ptr-snapshot-2026-08-10
  ```

**Origin filter (hybrid nodes, #198).** The audit compares only *mirror-origin*
mappings — rows whose `origin` column is NULL, the convention for data seeded
from or replayed as the PTR. Locally authored signed submissions carry a
non-NULL origin and are excluded from both the count and the digest, so a hybrid
node (one that both mirrors the PTR and accepts local write traffic, #194) audits
cleanly against a pure-PTR snapshot. On a pure mirror every current row has NULL
origin, so this filter changes nothing.

*Known limitation.* `origin` is a single last-writer-wins value per `(hash, tag)`
row. If a local signed Add *re-asserts* a pair that is also present in the
snapshot, that row's origin flips to non-NULL and it drops out of the audited
set — the audit then FAILs with the store count one short. This is inherent to
the single-value origin column and is not worked around; investigate a
one-short FAIL against your local submission log before assuming real drift.

**Notes on false mismatches.** A FAIL is always worth investigating. One benign
performance caveat applies to the **mirror-mode** audit: a full-range audit
streams both sides ordered by hash — efficient only when the mappings table has
a hash-led index (present after a normal seed); on a snapshot lacking it the
audit still works but may sort the whole band on disk, so keep the index or
audit narrower `--band` slices. The **sidecar** audit streams both sides in
band order and does not have this sort-cost caveat.
(Historically, two distinct Hydrus tags that normalise to the same naiad string
produced a spurious count mismatch; as of 0.2.83 both sides dedup the normalised
tag set per hash, so this no longer false-FAILs — a FAIL now reflects a genuine
mapping difference.)

**Observability.** The follow-loop now logs a one-line summary for each PTR
update index it applies (tag counts in/out, timing). A `warn`-level line
mentioning "possible PTR format drift" means Hydrus sent a content or definition
packet whose type the mirror does not recognise — the packet is skipped to avoid
corrupting the store, but recurring drift is worth investigating against the PTR
changelog.

### Which one

| | sidecar mode | snapshot mode | mirror mode |
|---|---|---|---|
| Disk | ~26 GB self-contained index | none beyond the snapshot | full materialized store (hundreds of GB) |
| Setup | `bridge seed`, then `sync` or follow-loop | download, point config at it | `bridge seed`, then `sync` |
| Freshness | PTR delta sync, near-live | snapshot age | PTR delta sync, near-live |
| Precision | coarse + exact-hash | up to exact-hash (`max_query_bits`) | coarse k-anon buckets |
| Trust model | operator untrusted (k-anon) | operator trusted by clients | operator untrusted |
| PTR network access | required for `sync` / follow-loop | none | required for `sync` |
| Recommended for PTR | **yes** — standard PTR backend | operator-trusted, static queries | native sha256 RepoStore only |

---

## Accounts and the trust model

### Reads are anonymous

Anyone can pull tags from your repo without logging in, signing up, or
identifying themselves. The read endpoints (`/repo/snapshot`, `/repo/caps`,
`/repo/buckets`, `/repo/relations`) take no identity at all. This is also why a
repo has **no "only my own machine" guard** — it's *meant* to be reachable by
strangers.

### Writes are signed, accounts auto-create

When someone submits a tag to your repo, that submission is signed with
their client's derived key. Your repo checks the signature before storing
anything. If the account doesn't exist yet, it is created automatically at
that first valid submit — no registration step needed.

An account has two properties you care about as an operator:

| Property | Values | Meaning |
|----------|--------|---------|
| `role` | `contributor` (default), `moderator` | Moderators can see the report queue and take moderation actions |
| `banned` | false (default), true | Banned accounts are rejected at submission |

Put together: **anonymous to read, accountable to write.** That's the shape of
trust here.

> **Upgrading to 0.2.57 — clients and server must move together.** The signature
> a client puts on a write now covers which hash domain the write is aimed at,
> not just the method, path, timestamp, and body. That closes a hole where a
> network attacker could append `?domain=` to somebody's signed submission and
> steer it at a different hash domain without invalidating their signature. It is
> also a wire break: the protocol version goes 6 → 7, so a 0.2.56-or-earlier
> client gets a `401 auth failed` from a 0.2.57 server, and vice versa. Upgrade
> both. Nothing in `repo.toml` or your database changes.

> **Upgrading a client to 0.2.58 — the first pull after upgrading is a full one.**
> This is a client-side change only: nothing about the repo, `repo.toml` or the
> wire moves, and a 0.2.57 client keeps working against a 0.2.58 server and vice
> versa.
>
> Client mapping rows now record *which hash domain supplied them*, which is what
> lets a client subscribed to a dual-domain repo (one serving BLAKE3 **and** a
> bridged SHA-256 domain) pull each domain independently. Before this, co-serving
> a SHA-256 domain silently forced every subscriber into a full pull of **both**
> domains on every single pull, forever — see #151.
>
> Existing rows record nothing about their source and it cannot be inferred, so
> migration 0034 discards pulled mappings and rewinds each subscription's cursor
> rather than guessing. **Your own local tags are untouched** — only tags pulled
> from a repo are re-fetched. Expect one full pull per subscription, after which
> pulls are incremental again; on a large library that first pull takes a while,
> and tags from that repo are missing until it finishes.

> **Upgrading a repo to 0.2.76 — the store is rewritten in place; budget disk
> for it.** The first start after upgrading runs a one-shot migration that
> interns every hash and tag in the repo store: hashes become 32-byte blobs
> stored once each, mapping rows shrink to small integer pairs, and a redundant
> index is dropped. The migration is a single transaction — if it fails or the
> disk fills, it rolls back cleanly and the old store is untouched.
>
> Before upgrading, make sure the volume holding `repo.db` has **about 1.5× the
> file's current size free**: the old and new tables coexist until the final
> swap. The file does **not** shrink on its own afterwards — the freed pages are
> reused for future writes. To hand the space back to the OS, stop the repo and
> run `VACUUM` on `repo.db` once (it needs free space of roughly the
> post-migration size while it runs). Nothing on the wire changes: clients,
> cursors, and signatures are unaffected. Mirror operators re-seeding from a
> snapshot get the compact layout automatically — a fresh seed needs no
> migration and roughly **six times less disk** than 0.2.75.

### Promoting and banning accounts

```powershell
naiad-repo --db repo.db account list                 # all accounts + role + banned status
naiad-repo --db repo.db account promote <pubkey-hex> # → moderator
naiad-repo --db repo.db account ban    <pubkey-hex>  # reject future submits from this key
```

You identify accounts by their public key (a 64-character hex string). Clients
display their own key with `naiad account`.

---

## Tags vs. relations

Your repo holds two kinds of thing:

- **Tags** — "this fingerprint has this label." Pulls use the bucket trick
  above. After a client's first pull, it asks each bucket only for *what changed
  since last time* (a cursor rides along in the bucket request), so repeat pulls
  are cheap for you to serve. Old clients that don't know about cursors just keep
  getting the full bucket.
- **Relations** — rules *between* tags, with no file attached. Two kinds:
  - a **sibling** says "treat tag A as an alias of tag B" (e.g. `samus` really
    means `character:samus`),
  - a **parent** says "tag A implies tag B" (e.g. `character:samus` implies
    `series:metroid`).

Relations mention no files, so they reveal nothing about anyone's library. That
means they sync in **bulk** — a client just grabs your repo's entire relation
graph in one go, no bucket dance needed. Incremental deltas are also supported
(`GET /repo/relations?since=N`); your repo advertises this in `/repo/caps`.

You don't have to do anything special to host relations — `serve` handles both.

---

## Reports and moderation

### What reports are

A **report** is a signed request from a client saying "this `(hash, tag)` pair
is wrong, please look at it." It is fire-and-forget on the client side — the
client sends it and moves on; there is no status tracking.

Reports land in a queue only moderators can see. They do not affect what other
clients see until a moderator acts.

### Viewing the report queue

```powershell
naiad-repo --db repo.db report list
```

Lists every open report (hash, tag, reporter pubkey, note, timestamp).

### Acting on a report

```powershell
# Delete the mapping: status → deleted, propagates to clients via delta stream.
# Also auto-closes all open reports for that (hash, tag).
naiad-repo --db repo.db report delete <hash> <tag>

# Ban the reporter: reject future submits and reports from that key.
naiad-repo --db repo.db report ban <reporter-pubkey-hex>

# Dismiss: close the report without changing anything.
naiad-repo --db repo.db report dismiss <report-id>
```

**Delete is non-sticky.** Unlike a permanent block, a moderator delete is a
plain tombstone — the same shape as a contributor's own removal. A subsequent
`Add` by any contributor resurrects the mapping. If an account keeps re-adding
a deleted mapping, the right tool is `account ban`, not repeated deletes.

**Delete auto-closes reports.** When you delete a mapping, all open reports for
that `(hash, tag)` are automatically closed so your queue stays clean.

### Who can moderate

Any account with `role = moderator` can access the report queue and take
actions. Promote an account:

```powershell
naiad-repo --db repo.db account promote <pubkey-hex>
```

There is no separate "moderator key" or config file. Moderation authority is
just an account role, checked on every request.

---

## The bigger picture: independent islands

Naiad deliberately avoids the "one giant database everybody must download"
model. Instead the idea is many small repos, run by many people, that clients
can pull from independently — more like a selection of curated sources than a
single giant database.

The bucket design helps here: because a bucket is a stable unit (the same
prefix always returns the same answer), a small, cheaply-hosted repo can serve a
lot of pullers without melting. **You do not need a beefy server to be a useful
node.** A modest box hosting tags for a community you care about is exactly the
intended shape.

In v0.2.0 there is no server-to-server mirroring or peer discovery — each repo
is an independent island. The internal signed submission log is kept as the
foundation for a future mirroring feature, but nothing is built on it yet.

---

## Day-to-day operation

- **Backups.** Stop the server first, then copy `repo.db` — that is the
  entire state.  A clean shutdown checkpoints the WAL and the files are
  deleted by SQLite when the last connection closes, so a stopped server
  leaves only `repo.db`.
  If you must back up a live server, copy **all three** files together:
  `repo.db` + `repo.db-wal` + `repo.db-shm`.  Copying `repo.db` alone
  while the server is running is unsafe: recent commits that live only in
  the WAL will be missing.  Alternatively use `sqlite3 repo.db .backup
  dest.db` or `VACUUM INTO 'dest.db'`, which SQLite makes crash-safe.
- **Restarts.** Stop the program, start it again with the same `--db`. Nothing
  is lost; the database is on disk.
- **Moving hosts.** Copy the database file to the new machine and run `serve`
  there. If you used `--repo-key`, pass the same key.
- **Checking it's alive.** `GET /health` is the liveness probe: it returns 200
  with `{"status":"ok","server_version":"<version>"}` and never touches the
  database. Use it for Docker HEALTHCHECK, Kubernetes liveness probes, or any
  external monitor. `GET /repo/caps` also works and tells you the pull mode,
  protocol version, and `server_version`, but it runs a mapping-count query
  against the database. For pure liveness, prefer `/health`.

---

## Running it as a service (so it survives reboots and crashes)

Everything above assumes you start `naiad-repo` in a terminal and leave it
running. That's fine for trying things out, but a real node should start on
boot and come back up on its own if it ever crashes. You hand that job to a
**process supervisor** — systemd on Linux, a service wrapper on Windows.

**Shutting the server down is always safe.** Naiad-repo has no in-memory state
to lose: every tag, submission, and report is written to the database file as
it happens (SQLite commits per request). Stopping the process — cleanly or with
a hard kill — never corrupts or loses data. That's what lets a supervisor
restart it freely.

**Graceful drain.** `naiad-repo` handles SIGTERM (Linux / containers) and
Ctrl-C (all platforms) cleanly. When a stop signal arrives, the server stops
accepting new connections and waits for in-flight requests to finish. It then
runs `PRAGMA wal_checkpoint(TRUNCATE)` on the writer connection, which
truncates `-wal` to zero bytes; the `-wal` and `-shm` files are deleted by
SQLite when the last connection closes at process exit. The drain has a hard
cap of **8 seconds**: if in-flight requests haven't finished by then, the
process exits anyway. A client whose request is cut off will retry automatically.

**Bridge caveat.** If `[bridge].enabled = true` in `repo.toml`, the PTR
follow-loop thread keeps its own writer connection on `repo.db` open for the
life of the process — shutdown never stops it. The checkpoint call may
therefore return `busy = 1` and leave `-wal`/`-shm` files behind after a
clean stop in that configuration. This is harmless for data integrity; just
include both files when doing a cold backup of a bridge-enabled repo.

- `docker stop <container>` sends SIGTERM and waits for the container's
  grace period (default 10 s, well above the 8 s cap). The server exits
  cleanly within that window. For busy repos under heavy load, use
  `docker stop -t 30 <container>` to give the drain more room.
- `systemctl stop naiad-repo` sends SIGTERM; the process exits cleanly and
  systemd records a successful stop.
- Ctrl-C in a terminal also triggers the graceful drain.
- On Unix, if the server is unresponsive after a SIGTERM, escalate to
  **SIGKILL** (`kill -9 <pid>` or `docker kill <container>`). Sending a
  second SIGTERM will not help: tokio installs its signal handler
  process-wide and never removes it, so additional SIGTERM/SIGINT signals
  are swallowed for the life of the process.

Because the server handles SIGTERM itself, running it as PID 1 in a Docker
container without an init process (e.g. `tini`) works correctly.

### Configuration under a supervisor

One gotcha before the unit files. A supervised process starts with a **clean,
minimal environment** — it does *not* inherit the environment of the shell you
installed it from. `NAIAD_REPO_*` variables and `RUST_LOG` have no effect if
you just export them in your terminal and then start the service. Under a
supervisor, inject them explicitly:

- **systemd:** add `Environment=NAIAD_REPO_LOG_LEVEL=debug` (or
  `EnvironmentFile=/etc/naiad-repo.env`) under `[Service]`.
- **NSSM:** `nssm set NaiadRepo AppEnvironmentExtra NAIAD_REPO_LOG_LEVEL=debug`.
- **`sc.exe`:** no clean per-service environment mechanism — another reason to
  prefer NSSM.

`repo.toml` is equally valid and also survives restarts regardless of
the supervisor — choose whichever is more natural for your deployment.

### Linux — a systemd unit

Create `/etc/systemd/system/naiad-repo.service`. This example assumes the
database and `repo.toml` live in `/var/lib/naiad-repo/` and the binary is at
`/usr/local/bin/naiad-repo`:

```ini
[Unit]
Description=Naiad tag repository node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# Run as an unprivileged user, not root. Create it once with:
#   sudo useradd --system --home /var/lib/naiad-repo --shell /usr/sbin/nologin naiad
User=naiad
Group=naiad
# WorkingDirectory must be the folder holding repo.db, because repo.toml is
# read from beside the database and a bare `repo.db` path resolves here.
WorkingDirectory=/var/lib/naiad-repo
ExecStart=/usr/local/bin/naiad-repo --db repo.db serve
Restart=on-failure
RestartSec=5
# The server's only job is to serve tags; lock down everything else.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/naiad-repo

[Install]
WantedBy=multi-user.target
```

Then:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now naiad-repo      # start now, and on every boot
sudo systemctl status naiad-repo            # is it up?
journalctl -u naiad-repo -f                 # follow its logs
```

Notes:

- **`WorkingDirectory` is the important line.** `repo.toml` is always read from
  the directory of the database file, and `--db repo.db` (a relative path)
  resolves against the working directory. Point both at the folder that holds
  your `repo.db` and the config is picked up automatically.
- **You don't need a `[log] file`.** With `console = true` (the default),
  every log line goes to stderr, and systemd captures stderr into the journal
  for you. `journalctl` handles rotation and disk limits itself — see log
  rotation below. Only set a `[log] file` if you specifically want a plain-text
  file *in addition* to the journal.
- **`Restart=on-failure`** brings the server back if it exits non-zero (a
  crash, a bind failure once the port frees up). It deliberately does *not*
  restart on a clean `systemctl stop`.

### Windows — a service wrapper

The Windows portable build (`naiad-repo.exe`) is a plain console program; it
has no built-in "install as a service" mode. Use a wrapper.

**NSSM (recommended).** [NSSM](https://nssm.cc) is a tiny, well-worn service
host that turns any exe into a Windows service, restarts it on crash, and can
capture and rotate its console output. Assuming the exe and `repo.db` live in
`C:\naiad-repo\`:

```powershell
# Install (run this shell as Administrator)
nssm install NaiadRepo C:\naiad-repo\naiad-repo.exe --db repo.db serve
nssm set NaiadRepo AppDirectory C:\naiad-repo          # = WorkingDirectory: repo.toml is read from here
nssm set NaiadRepo Start SERVICE_AUTO_START            # start on boot
nssm set NaiadRepo AppExit Default Restart             # restart if it dies

# Capture stderr to a file and let NSSM rotate it (see log rotation below)
nssm set NaiadRepo AppStderr C:\naiad-repo\naiad-repo.log
nssm set NaiadRepo AppRotateFiles 1
nssm set NaiadRepo AppRotateBytes 10485760             # rotate at ~10 MB

nssm start NaiadRepo
```

`AppDirectory` is the Windows equivalent of systemd's `WorkingDirectory` — set
it to the folder holding `repo.db` so `repo.toml` is found. Manage the service
afterward with `nssm restart NaiadRepo`, `nssm stop NaiadRepo`, or the normal
`services.msc` console.

**`sc.exe` (built-in, no download).** Windows' native `sc.exe` can register a
service without NSSM, but it expects a real service binary and won't cleanly
supervise a plain console exe — it can start it, but crash-restart and output
capture are clumsier. If you go this route:

```powershell
sc.exe create NaiadRepo binPath= "C:\naiad-repo\naiad-repo.exe --db C:\naiad-repo\repo.db serve" start= auto
sc.exe failure NaiadRepo reset= 60 actions= restart/5000    # restart 5s after a crash
sc.exe start NaiadRepo
```

Note the mandatory spaces after `binPath=` and `start=` — that's `sc.exe`
syntax, not a typo. Because `sc.exe` can't set a working directory, pass an
**absolute** `--db` path so `repo.toml` is still found beside it. For most
operators NSSM is the less fiddly choice.

### Log rotation (the file sink grows forever)

If you set a `[log] file` in `repo.toml`, that file is opened in **append
mode** and never rotated by Naiad itself — it grows without bound until you do
something about it. One important detail drives how you rotate it: **the server
holds the log file open for its entire run.** It does not re-open the file on a
signal.

That single fact rules out the naive approach. On Linux the default `logrotate`
behavior is to *rename* the old file and create a fresh one — but Naiad keeps
writing to the renamed file (same open handle / inode), and the new file stays
empty until the next restart. You have two correct options:

**Option A — let the supervisor own the logs (simplest).** Don't set a
`[log] file` at all. Leave `console = true` and let systemd (journal) or NSSM
(`AppStderr` + `AppRotateFiles`, shown above) capture and rotate stderr. This
is the recommended path — the rotation problem becomes the supervisor's, and
both journald and NSSM handle it well.

**Option B — rotate the file sink with `copytruncate`.** If you do want a
standalone log file, tell `logrotate` to copy-then-truncate instead of rename,
so the server's open handle keeps pointing at the same (now-emptied) file.
Create `/etc/logrotate.d/naiad-repo`:

```
/var/lib/naiad-repo/repo.log {
    weekly
    rotate 8
    compress
    missingok
    notifempty
    copytruncate
}
```

`copytruncate` has a tiny race window (log lines written during the copy can be
lost), which is harmless for an operational log. The alternative — rotate by
rename and then `systemctl restart naiad-repo` in a `postrotate` block — gives
exact rotation at the cost of a brief restart. On Windows the file is *locked*
while the server runs, so you can't rotate a `[log] file` in place at all;
prefer Option A (NSSM capturing stderr) there.

---

## Running it in a container

The repository root ships a `Dockerfile`, `docker-compose.yml`, and
`.dockerignore` as a reference deployment. The image is a standard two-stage
build — the host needs only Docker, no Rust toolchain.

```bash
docker compose up -d --build
curl http://localhost:9090/health    # liveness probe
```

The image sets `NAIAD_REPO_DB=/data/repo.db` and `NAIAD_REPO_ADDR=0.0.0.0:9090`
as `ENV` rather than baking `--db`/`--addr` into the entrypoint. This means:

- `docker exec <container> naiad-repo account list` finds the right database
  without `--db`.
- Every setting is overridable with `-e` or a compose `environment:` block, no
  rebuild needed:

```yaml
environment:
  NAIAD_REPO_LOG_LEVEL: "debug"
  NAIAD_REPO_K: "500"
```

**Backups:** the state (`repo.db`, `repo.toml`, `repo.key`) lives in the
`repo-data` named volume. `docker compose down -v` wipes it entirely.
For a live backup, use `docker exec <c> sqlite3 /data/repo.db .backup dest.db`.

**Admin:** `docker stop <container>` sends SIGTERM. The server handles
SIGTERM itself (graceful drain, then clean exit), so no init-shim is needed.
For busy repos under heavy load, use `docker stop -t 30 <container>` to give
the drain more headroom.

**Healthcheck:** the `HEALTHCHECK` in the Dockerfile points at `/health` (the
lightweight liveness endpoint added in v0.2.48). If you override
`NAIAD_REPO_ADDR`, update the healthcheck port to match.

**TLS / reverse proxy:** the container serves plain HTTP. Put nginx or Caddy
in front for TLS and a stable hostname.

### Scheduled nightly sync

A static mirror (`bridge.enabled = false`) holds the cursor at whatever PTR
update index it was seeded to — it never advances on its own. To keep it
fresh, schedule a one-shot `bridge sync` from outside the container. The
#193 single-writer lock means overlapping runs are safe: the second invocation
finds the lock held, prints a message to stderr, and exits **4** — it does not
corrupt the store or fight the first writer.

Below is an Unraid User Scripts example. Set the schedule to `30 3 * * *`
(03:30 UTC nightly) and paste the script body verbatim:

```sh
#!/bin/bash
# naiad-repo nightly PTR sync — Unraid User Scripts, cron: 30 3 * * *
LOG=/mnt/disk3/appdata/naiad-repo/sync.log
ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
out=$(docker exec naiad-repo-full naiad-repo bridge sync 2>&1)
rc=$?
case $rc in
  0) line=$(printf '%s\n' "$out" | grep -m1 '^sync ok:' || printf '%s\n' "$out" | tail -n1) ;;
  4) line="skipped: another bridge writer active" ;;
  *) line="FAILED exit $rc: $(printf '%s\n' "$out" | tail -n1)"
     /usr/local/emhttp/webGui/scripts/notify -e "naiad-repo sync" \
       -s "nightly sync failed" -d "exit $rc — see sync.log" -i warning ;;
esac
echo "$ts  $line" >> "$LOG"
tail -n 1000 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
```

**Exit codes:**

| exit | meaning | log line | notify? |
|------|---------|----------|---------|
| 0 | sync ran (may be idle) | binary summary line | no |
| 4 | another bridge writer active (#193 lock) | `skipped: another bridge writer active` | no |
| other | sync failed | `FAILED exit N: …` | yes (warning) |

**Summary line.** On a successful run the binary prints one line to stdout:

- Active sync: `sync ok: cursor 1234→1240 (6 updates, 152340 mappings) in 84s`
- No new updates: `sync ok: cursor 1240 (no new updates) in 2s`

(The `→` is U+2192.) The script captures that line as the log entry, so
`tail -1 sync.log` is the quickest freshness check.

**PTR key.** No env var is required for the public PTR: `bridge.ptr_key`
defaults to the built-in public read key. Set `NAIAD_REPO_BRIDGE_PTR_KEY`
only if you are mirroring a non-default key. Setting it never starts a
follow-loop — `bridge.enabled` stays false and the container's serve process
does not dial out.

**`no_egress` and the sync subcommand.** `serve.no_egress` gates only the
serve process's built-in follow-loop; it never gates the operator-invoked
`bridge sync` subcommand. An `exec`'d sync works correctly even in a container
whose environment carries `NAIAD_REPO_NO_EGRESS=true`. This is deliberate —
the operator who runs `docker exec` is asserting intent; the env flag is a
guard on the always-on serve loop, not a kill switch for explicit commands.

**Future egress carve-out.** If you apply default-deny egress to the
container's network (the intended hardened deployment, see #190), you must
add an outbound `ACCEPT` for `ptr.hydrus.network:45871` before the catch-all
drop rule, or the nightly sync will silently start failing as soon as the
firewall is in place.

---

## Quick reference

```powershell
# Start a public repo on port 9090
naiad-repo --db repo.db serve --addr 0.0.0.0:9090

# Seed a tag (admin/seeding; bypasses the signed-submission protocol)
naiad-repo --db repo.db seed <fingerprint> character:samus

# Liveness probe (no DB touch) and capability check (runs a mapping count)
#   http://your-host:9090/health
#   http://your-host:9090/repo/caps
#   http://your-host:9090/repo/snapshot

# Account management
naiad-repo --db repo.db account list
naiad-repo --db repo.db account promote <pubkey-hex>
naiad-repo --db repo.db account ban    <pubkey-hex>

# Report queue
naiad-repo --db repo.db report list
naiad-repo --db repo.db report delete  <hash> <tag>
naiad-repo --db repo.db report ban     <reporter-pubkey-hex>
naiad-repo --db repo.db report dismiss <report-id>
```

| Endpoint | Method | Auth | What it's for |
|----------|--------|------|---------------|
| `/health` | GET | none | liveness probe; returns `{"status":"ok","server_version":"..."}`, no DB touch |
| `/repo/caps` | GET | none | protocol version, pull mode, `repo_key`, `reports` flag, `server_version`, optional `name` (display name set via `NAIAD_REPO_NAME` / `[serve].name`; omitted when unset), optional `serve_hint` (per-domain ms-per-bucket EWMA, advisory, omitted until the first bucket request is served; see note below) |
| `/repo/snapshot` | GET | none | the whole tag set (small repos / debugging) |
| `/repo/buckets` | POST | none | tags for the hash-prefix buckets a client asks for |
| `/repo/relations` | GET | none | the relation (sibling/parent) graph; `?since=N` for incremental |
| `/repo/submit` | POST | signed | a client adds/removes a tag or relation |
| `/repo/report` | POST | signed | a client files a report against a `(hash, tag)` |
| `/repo/reports` | GET | moderator-signed | the open report queue |
| `/repo/moderate` | POST | moderator-signed | delete/ban/dismiss a reported key |

**`serve_hint` width normalisation (#178).** The serve-cost EWMA stored in
`serve_hint` is normalised to the repo's advertised bucketed prefix width
(snapshot repos only). The width travels with the hint as `hint_bits`; clients
re-scale the cost onto their actual `requested_bits` via
`ms_per_bucket × 2^(hint_bits − requested_bits)`, so a client pulling at a
coarser width correctly expects dearer buckets and sizes a smaller first
window. A pre-#178 server omits `hint_bits`; a new client falls back to
assuming the hint was measured at the server's advertised width. Lineage:
bucket-width campaign #170 → sync telegraphing
#173 → width-aware serve hint #178.

---

Want the user side of this — pulling, submitting, reporting from a client?
That's in [quickstart.md](quickstart.md) §5. Want the *why* behind the privacy
design? See [ADR 0001](adr/0001-query-privacy-k-anonymity-buckets.md). Want the
report/moderation design in full? See
[ADR 0015](adr/0015-reports-and-moderation.md).
