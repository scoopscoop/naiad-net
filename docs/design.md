# Naiad — Design

This is the detailed design — the why behind the how. You don't need any of it to use Naiad; read on if you're curious how it works under the hood.

## Table of Contents

1. [Vision & Goals](#1-vision--goals)
2. [Guiding Principles](#2-guiding-principles)
3. [Architecture Overview](#3-architecture-overview)
4. [Data Model](#4-data-model)
5. [Search & Indexing](#5-search--indexing)
6. [Distributed Tag Protocol](#6-distributed-tag-protocol)
7. [Safety & Privacy Model](#7-safety--privacy-model)
8. [Repository Layout](#8-repository-layout)
9. [Roadmap](#9-roadmap)
10. [Testing Strategy](#10-testing-strategy)
11. [Tech Stack](#11-tech-stack)
12. [Status & Contributing](#12-status--contributing)

---

## 1. Vision & Goals

Naiad helps you take a large, messy pile of local media files and turn it into a fast, searchable,
richly-tagged library — and to *share tags* (not files) with a community so you don't have to tag
everything yourself.

It is **local-first**: your files and your library live on your machine. The network exists only to
exchange `hash → tags` mappings, on demand, for the files you actually have.

### Goals

- **Fast** at every step: importing, indexing, hashing, searching, and tagging.
- **Simple core to start**: index added folders → view media as a gallery → download tags from a
  network.
- **Tags keyed by content hash**, so the same file is recognized anywhere.
- **An efficient local database** for hashes, tags, and their relationships.
- **Decentralized tag sharing** that does not require trusting (or funding) a single operator.
- **Targeted tag retrieval**: pull tags only for the hashes you own — never a full-database sync.
- **Safe by default**: it must be effectively impossible to leak private data by accident.
- **Extensible and maintainable**: starts solo, designed to become a healthy open-source project.
- **Cross-platform**: first-class Windows and Linux client *and* server.
- **Buildable by humans and agents**: clear module boundaries and documented contracts.

### Non-Goals

- Not a cloud service and not a place to store your files online.
- Not a file-sharing network — Naiad never moves media content across the network, only tags.
- Not dependent on a single central server or a single maintainer.
- Not a general-purpose social network; the "social" surface is curated tag repositories.

---

## 2. Guiding Principles

- **Fast is the feature.** The backend is Rust. I/O is batched, hashing is parallel, the database is
  indexed for the queries we actually run. Performance regressions are bugs.
- **Local-first and safe by default.** Nothing leaves your machine unless you take an explicit,
  visible action. The default state of every piece of data is *private*.
- **Decentralized.** No single point of failure. The original developer must not be load-bearing
  infrastructure. Anyone can run a server; the network survives any one node going away.
- **Extensible & maintainable.** Small, focused crates with clean boundaries. The daemon ↔ UI API is
  a stable contract. Design decisions are recorded as ADRs (maintained in the upstream private
  repository) so future contributors (human or agent) understand *why*, not just *what*.

---

## 3. Architecture Overview

Naiad is three separable components. The UI and the backend are distinct processes that talk over
a local API, so you can run a headless daemon on one machine and point a UI at it from another.

```
        ┌─────────────────────────────────────────────────────────┐
        │                     YOUR MACHINE                         │
        │                                                          │
        │   ┌──────────────┐        local API        ┌─────────┐   │
        │   │              │  (HTTP + WebSocket over  │         │   │
        │   │  Web UI      │◄────loopback/named pipe─►│ Client  │   │
        │   │ (Tauri or    │                          │ Daemon  │   │
        │   │  browser)    │                          │ (Rust)  │   │
        │   └──────────────┘                          └────┬────┘   │
        │                                                  │        │
        │                         ┌────────────────────────┼─────┐  │
        │                         │  indexer · hashing ·    │     │  │
        │                         │  naiad.db · thumbs.db   │     │  │
        │                         └─────────────────────────┘     │  │
        └──────────────────────────────────────────────────┼──────┘
                                                            │
                                  targeted pull / signed    │  (tags only — never files)
                                  submission by hash        │
                                                            ▼
        ┌─────────────────────────────────────────────────────────────┐
        │         TAG REPOSITORIES — independent islands (run by anyone) │
        │                                                               │
        │   repo A          repo B          repo C                      │
        │   (hash → tags)   (hash → tags)   (hash → tags)              │
        │   clients pick which repos to subscribe to; repos don't talk  │
        │   to each other                                               │
        └─────────────────────────────────────────────────────────────┘
```

### Client daemon (Rust)

Owns the library. Responsibilities:

- Watch and index configured folders.
- Hash files, generate thumbnails.
- Store hashes, tags, and mappings in a local database.
- Run searches.
- Talk to tag repositories (pull/submit) on the user's behalf.
- Expose a **local API** (HTTP + WebSocket over loopback or a named pipe) — the single contract every
  UI speaks to.

### UI (web)

A web frontend (TypeScript + a framework such as Svelte or React) that talks only to the daemon API.
It ships two ways from the same codebase:

- **Tauri desktop app** — bundles UI + daemon into a native Windows/Linux application.
- **Browser** — point any browser at a running (possibly remote/headless) daemon.

Because the UI never touches the database or files directly, alternative UIs (CLI, TUI, mobile) are
just other API clients.

The browser UI lives in `ui/` (Svelte 5 + Vite + TypeScript) and is a pure client of the daemon API.

**Develop** (hot reload; proxies the API to a running daemon on `:8080`):

```sh
cd ui
npm install      # first time only
npm run dev      # http://localhost:5173
```

**Build and serve from the daemon:**

```sh
cd ui && npm run build   # emits ui/dist
naiad daemon             # serves the bundled UI at /
```

`naiad daemon` serves the Svelte UI by default: a debug `cargo run` reads `ui/dist` live (rebuild the
UI without recompiling Rust), and a `--release` binary embeds `ui/dist` at compile time — so **build
the UI before `cargo build --release`** to bake the real app into the binary. If the UI hasn't been
built, the daemon serves a small placeholder page. Pass `--ui-dir <path>` to serve a specific build
instead; a `--ui-dir` without an `index.html` is a startup error.

### Tag repository server (Rust)

An optional, independently-run node that stores `hash → tags` mappings for a community and serves
them by hash. Anyone can run one. Repositories can federate with each other (see §6).

---

## 4. Data Model

### File identity — content hash

Files are identified by a **content hash**, so the same bytes are recognized on any machine.

- **Primary hash: BLAKE3-256.** Chosen for speed — BLAKE3 is dramatically faster than SHA-256 and is
  trivially parallelizable, which directly serves the "fast import" goal.
- **Optional SHA-256** stored alongside, for interop with the existing Hydrus and torrent ecosystems
  (which are SHA-256 based). This lets Naiad ingest existing tag sets and torrents keyed on SHA-256
  without forcing everyone onto one algorithm. It is never an identity: nothing is keyed by it, and it
  is backfilled lazily, only for the users who ask for a Hydrus import.

### File records — content vs. locations

A file's **content** and its **location(s) on disk** are modeled separately, because the same bytes
can live in several folders and a folder can come and go (a removable drive, a moved directory):

- **Content** (`files`): one row per distinct hash. Holds the hash, size, extracted **metadata**
  (`mime`, `width`, `height`, `duration_ms` — nullable, filled by an extraction pass *after* hashing
  so import stays fast), and a **library state** (`active` / `archived` / `trashed`).
- **Locations** (`file_locations`): many rows per file. Each holds a path, `mtime`, and whether the
  copy was `present` at the last scan. Paths are stored as **raw OS bytes**, not lossy text, so
  non-UTF-8 names (legal on Windows and Linux) round-trip and stay findable.

**Files that vanish from disk are marked missing, never auto-deleted.** Their tags and metadata
survive an unplugged drive or a moved folder; only an explicit "remove from library" deletes the
content row.

### Tags

Tags are **namespaced strings** of the form `namespace:value`, e.g. `character:samus`,
`creator:nintendo`, `series:metroid`. An un-namespaced tag (e.g. `blue_sky`) is allowed. Tag text is
deduplicated through a dictionary table (`tag_id ↔ text`) so a tag string is stored once.

### Mappings

A **mapping** ties a file to a tag within a service:

```
mapping = (file_hash_id, tag_id, service_id, status, timestamp)
```

- `status` distinguishes *current*, *pending* (queued to publish), *deleted*, etc.
- On the wire, pulled mappings are plain `(hash, tag, status, seq)` — no authorship or tool
  provenance is exposed by the server's public endpoints. The server retains submitter pubkeys
  internally for moderation and accountability.

### Tag relations — siblings & parents

Two tag-to-tag relations give network tags their quality (and mirror Hydrus):

- **Siblings** are aliases: `samus`, `samus_aran`, and `character:samus aran` collapse to one
  canonical display tag, killing fragmentation.
- **Parents** are implications: `character:samus aran` implies `series:metroid`, so tagging the child
  virtually adds the parent.

Relations are stored per-service exactly like mappings (`tag_siblings`, `tag_parents`), so private
relations stay in a local-only service and shared ones are signed and federated. They are **applied at
query/display time** — not baked into stored mappings — so editing one rule is instant rather than a
mass rewrite of every affected file. Cross-service disagreements are resolved by user-ordered service
priority; cycles are detected and broken when applied. See
ADR 0002.

### Tag services & scopes — the backbone of safety

Every tag belongs to a **service**, and every service has a scope:

- **Local-only service** — never leaves the machine. The default. Your private tags live here.
- **Shared service** — bound to a specific repository; tags here may be published (opt-in, explicit).

This separation is physical, not just a flag in the UI: a tag in a local-only service has no path to
the network. To share something you must deliberately put it in (or copy it to) a shared service.
See §7.

### Storage

- **Client:** **SQLite** via `rusqlite`, with carefully chosen indices for fast tag/text search.
  Search and completion are plain SQL — prefix completion (the hot path) is index-assisted, and
  substring completion stays sub-millisecond at realistic client scale, so an FTS5 index isn't worth
  its cost. SQLite is proven at exactly this workload (it's what Hydrus uses) and keeps the client a
  single-file, zero-admin database.
- **Repository servers:** SQLite for small/hobby operators; the schema is kept portable so large
  operators can back the same logic with PostgreSQL.

---

## 5. Search & Indexing

### Indexing

- **Folder scan + filesystem watch**: configured folders are scanned on add, then watched for changes
  so the index stays current incrementally (no full rescans).
- **Parallel hashing & thumbnailing**: a worker pool hashes new files (BLAKE3) and generates
  thumbnails concurrently, saturating disk and CPU.
- **Metadata extraction**: after hashing, a separate pass fills `mime`, dimensions, and duration so
  system predicates have data to filter on. Hashing is never blocked on it — import stays fast.
- Files are content-addressed, so moving or renaming a file doesn't re-import it — the hash already
  matches. A copy appearing in a new folder adds a *location*, not a duplicate.

### Search

- **Inverted index** over tags for fast lookup.
- **Namespace-aware queries**: filter by namespace (`character:*`), exact tags, or wildcards.
- **Wildcards anywhere in the subtag**: `*` matches any run of characters at any position —
  `samus*` (prefix), `*samus` (suffix), `sam*us` (interior), `*samus*` (contains). The namespace side
  is still matched literally (`character:*aran` is fine; `char*:x` is not). A bare `*` is rejected.
- **Quoted phrases for multi-word tags**: tags can contain spaces, so wrap them in double quotes to
  keep the phrase as one term — `"zero mission"`, `character:"zero mission"`. Without quotes the query
  splits on whitespace and each word becomes its own AND'd predicate. Operators still work inside a
  phrase (`"zero mission*"`).
- **Boolean logic**: AND / OR / NOT combinations of tag predicates.
- **System predicates**: filter/sort by size, dimensions, duration, import date, etc. (backed by the
  metadata columns above).
- **Origin filtering**: `system:origin=<name>` matches files with a tag made by a given tool
  (`system:origin=wd14-tagger`), `system:origin=manual` matches hand-typed (origin-less) tags, and
  `-system:origin=…` excludes. Matches nothing until a producer or a repo pull stamps origins
  (ADR 0026); a plain `origin:foo` token stays an ordinary tag.
- **Sibling/parent expansion at query time**: searches and displayed tags resolve siblings (to the
  canonical tag) and parents (implications) on the fly, per service priority.
- Designed so the common case (a handful of AND'd tags over a large library) is near-instant. A
  cached display layer for relation expansion is added only when a benchmark shows it is needed.

---

## 6. Distributed Tag Protocol

This is the heart of the project.

### The problem we're solving

Community tag sharing today generally means replicating an entire repository — hundreds of
gigabytes — before the first tag is usable. That model is simple and proven, but heavy for
casual users and centralizing in practice. Naiad explores a different trade-off: fetch only
what your library needs, in privacy-preserving slices.

### The model: independent servers with targeted, per-bucket retrieval

- **Repositories** are community-run servers holding `hash → tags` mappings. A client **subscribes**
  to one or more repositories it trusts. Each server is an independent island — no server-to-server
  communication or automatic synchronization between them (v0.2.0).
- **Targeted pull via k-anonymity buckets.** To get tags, the client does **not** send its exact
  hashes (that would hand the operator an inventory of your library — see *Query privacy* below).
  Instead it asks for **hash-prefix buckets**: the leading bits of each hash it owns. The repository
  returns mappings for every hash in the bucket, and the client filters locally to the hashes it
  actually owns. The operator only ever sees "someone wants something in bucket `0x3A7`," hiding you
  in a crowd of ≥~1,000 hashes. The prefix length is advertised by the repo and grows with its
  database to keep the crowd size roughly constant. See
  ADR 0001.
- **Incremental sync.** After the first pull, clients fetch only deltas, keyed **per bucket** (not per
  account or per hash). Because bucket requests are identical for every user, responses are
  content-addressed and genuinely shareable — they sit comfortably behind a CDN. Implemented for
  both mappings (ADR 0014) and relations (ADR 0005); old↔new client/repo pairs degrade to
  the full pull with no flag day.
- **Relation sync (siblings/parents) is bulk, not bucketed.** Tag relations are keyed by *tag*, not by
  file hash, and the relation graph is small, text, and **library-independent** — downloading all of
  it reveals nothing about your files. So clients bulk-download the whole relation graph for each
  subscribed repo (incremental deltas after). Mappings are pulled privately by bucket; relations are
  fetched in full.
- **Submissions.** Clients submit **signed** tag mappings and relations (additions and removals) to a
  repository. Accounts auto-create on first signed submit — no registration step. The **read path is
  anonymous** (bucket pulls carry no account identity); only the **write path is signed** — otherwise
  the operator could re-link buckets back to you. The wire format uses an explicit versioning policy:
  `ensure_supported` checks a `MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION` range; additive changes
  never bump `PROTOCOL_VERSION`.
- **Reports.** When a pulled tag is wrong, the client can file a **report** — a fire-and-forget
  signed note to the origin server with an optional reason. The server queues it for a moderator.
  Moderators can delete the mapping (propagates via the normal delta stream), ban the account, or
  dismiss. No local tracking, no status polling. See
  ADR 0015.

### Query privacy

The original "send your exact hashes" design had an unstated flaw: since you only query hashes for
files you own, the query set *is* an inventory of your library. Bucketed pull (above) fixes the common
case — per query, you are hidden in a crowd. One **residual, accepted** limitation remains for v1:
over many queries, the *set of buckets* you touch is a coarse fingerprint of your collection, though
it cannot be resolved down to individual files. Future mitigations (decoy buckets, Tor routing) are
noted in ADR 0001; cryptographic approaches (PSI/PIR) are deliberately out of scope as
over-engineering for v1.

#### Choosing a cover crowd

You control how many other files hide each hash you look up — the "cover crowd." A larger crowd means
more privacy (the repo sees less signal about your exact library) at the cost of downloading more tags
for files you don't own; the desktop app shows an estimated per-lookup size as you adjust the slider.
The default crowd of roughly 1,000 hashes is a practical balance for most users on an untrustworthy
network. Dropping below ~1,000 cover files — where individual hash correlation becomes realistic —
requires an explicit "naked pulls" opt-in, intended only for situations where the extra privacy is
unnecessary (for example, pulling over a VPN or a fully trusted private repo).

### How trusted operators emerge from the community

This is intentionally a social/reputation system, not a fixed hierarchy:

- Clients can **share and curate repository lists**, so good repositories get discovered.
- Reputation accrues to repositories whose tags people actually keep and rely on.
- The result: a contributor who runs a well-moderated, high-quality repository naturally **rises into
  a trusted operator**, without any central authority anointing them. Communities can fork or migrate
  if an operator goes bad.

### Why plain HTTP (and not a DHT)

A DHT (BitTorrent-style) was considered. Plain HTTP was chosen because it is simpler to build, gives
natural moderation points, keeps bucket responses cacheable by any CDN, and matches the desired social
model.

---

## 7. Safety & Privacy Model

A core requirement: **it must be effectively impossible to leak private data by accident**, and
servers must never hold data that creates focused legal exposure. The risk posture target is
"torrent-level" — sharing tags, like a tracker shares hashes, not files.

### Local by default

- Files **never** leave your machine. The protocol moves only `hash → tag text`.
- Every tag starts in a **local-only service** with no path to the network.

### No accidental sharing

- Local-only and shared services are **physically separate** in the data model, not merely a checkbox.
- The UI **visibly distinguishes** local vs shared tags at all times.
- Publishing is an **explicit, confirmable action** — there is no background "sync everything" that
  could sweep up private tags.

### Legal posture (torrent-like)

- Repositories store **only opaque hashes + tag text** — never file content. A repository operator
  does not possess or distribute the underlying media.
- **No PII in the protocol.** Accounts are keypairs, not identities.
- Operators can apply **blocklists** to refuse hashes/tags they don't want to host.
- The intended risk level is comparable to running a torrent tracker or DHT node: you're indexing
  references, not serving content.

### Query privacy is bounded, not absolute

The safety promises above protect *what you send* (files never leave; tags only on explicit action).
A distinct axis is *what you reveal by asking* for tags. Naiad does not send your exact hashes — it
uses k-anonymity buckets (§6) so a repository cannot read off your library from a query. The honest
bound: a repository can still observe the *coarse* distribution of buckets you fetch over time. This
is by design for v1 and documented, not hidden; stronger options are recorded in
ADR 0001.

### Signed and revocable

- Network tags are **signed** by their contributor's key, so the server can verify provenance and
  attribute submissions to accounts.
- A user can **drop a service** at any time. By default this **detaches** the repo and keeps
  its pulled tags in the library (data-safe default). Explicitly requesting a purge — via the
  UI checkbox, `naiad repo remove --purge`, or `DELETE /api/repos?purge=true` — removes the
  pulled tags.
- Submissions can be retracted/superseded; a moderator can delete a mapping and/or ban an abusive
  account, with the deletion propagating to clients via the normal delta stream.

---

## 8. Repository Layout

A Cargo workspace whose crate boundaries map onto the architecture. This makes the codebase easy to
navigate, lets work proceed in parallel, and gives both human and agent contributors clear ownership
and stable contracts to work against.

```
naiad/
  Cargo.toml              # workspace root
  crates/
    core/                 # shared domain types: hashes, tags, services, errors
    db/                   # SQLite storage + migrations (rusqlite, plain SQL search)
    indexer/              # folder scan, filesystem watch, hashing (blake3), thumbnails
    daemon/               # client daemon: orchestration + local API server
    api/                  # API request/response types (shared by daemon and UI clients)
    cli/                  # headless control CLI (an API client)
    test-support/         # shared test helpers: temp DB factory, fixture files, test server builder
    netproto/             # federation protocol types + repository client
    server/               # federated tag repository server (naiad-repo)
    plugin/               # in-process plugin contract: capability traits + registry + Sink
    plugin-hydrus/        # first plugin: imports tags/relations from a Hydrus DB (Tagger + Source)
  ui/                     # web UI (Svelte/TS) + Tauri desktop shell (src-tauri/)
  docs/                   # user documentation
  README.md               # project splash and quick start
```

### Conventions

- **Small, focused crates.** Each crate has one job; cross-crate contracts live in `core` and `api`.
- **The API is the contract.** The UI and CLI never touch `db` or `indexer` directly — only `api`.
- **Architecture Decision Records (upstream).** Significant decisions (e.g. federation vs DHT,
  BLAKE3 vs SHA-256) are recorded as short ADRs maintained in the upstream private repository
  so future contributors understand the reasoning.
- **Built for agents too.** Clear boundaries, typed contracts, and recorded rationale make the
  codebase tractable for AI-assisted development, not just humans.

---

## 9. Roadmap

| Phase | Theme | Delivers |
|-------|-------|----------|
| ~~**0**~~ | ~~Scaffold~~ | ~~Cargo workspace, `core` types, DB schema + migrations, BLAKE3 hashing.~~ ✓ |
| ~~**1**~~ | ~~Local MVP~~ | ~~Index folders, hash, store, **gallery view**, local tagging, fast search. No network.~~ ✓ |
| ~~**2**~~ | ~~Daemon/UI split~~ | ~~Stabilized local API, browser UI, thumbnailing, watch-based reindex, portable Tauri desktop packaging.~~ ✓ |
| **3** ◐ | First repository | Signed submissions, **targeted pull by hash**, accounts, delta sync, bucket deltas, relation sync, block list, per-mapping rejection — all delivered. In v0.2.0, the entire client/server pivot (ADR 0021): plain wire (v6), fire-and-forget reports, moderator queue, simple account model. |
| **4** | Ecosystem quality | Up/down votes on mappings (future; internal signed log is the foundation); mirroring (future; same foundation); perf hardening; downloader/parser plugins; repo-list sharing. |
| **5** ◐ | Packaging | Portable Windows release ✓; in-process plugin system ✓; Hydrus importer ✓; import UI, Linux packaging, open-sourcing remain. |

Phase 1 is the "functionality for starters" the project is built around; Phase 3 is the first time
tags cross the network.

---

## 10. Testing Strategy

Agentic development moves fast and introduces subtle bugs. A strong test harness is not optional —
it is the mechanism by which generated code gets verified before it ships. Every crate ships with
tests; CI must be green before anything merges.

### Layers

**Unit tests** (`#[test]` in each crate)
- Pure logic: hash encoding/decoding, tag parsing and normalization, query AST construction, bloom
  filter operations, signature verification.
- No I/O, no disk, no network. Fast and total.

**Integration tests** (`tests/` in each crate, real SQLite)
- `db` crate: schema migrations, tag search, mapping insert/delete/query, service-scope
  separation.
- `indexer` crate: scan a temp folder, detect added/removed/moved files, verify hashes match.
- `daemon` crate: spin up a daemon against a temp DB; exercise the full API over loopback HTTP.
- `server` crate: spin up a test repo server; submit tags, pull by hash batch, verify delta sync.
- **No mocked database.** Tests that mock SQLite have historically passed while prod migrations
  failed. Tests hit a real (temp) SQLite file.

**End-to-end tests** (`crates/daemon/tests/`)
- Daemon + repo server running in the same process (or child process); client pulls tags for a set
  of known hashes; assert mappings arrive correctly, local-only tags never leave.
- Report flow end-to-end: file a report, moderator queue, delete/ban/dismiss actions.
- These live as flat integration tests in `crates/daemon/tests/` (e.g. `submit.rs`, `repos.rs`,
  `report_e2e.rs`, `bucketed_pull.rs`) rather than a separate `tests/e2e/` tree.

**Property-based tests** (`proptest`)
- Hash round-trips, tag serialization, query evaluation against a naive reference implementation,
  delta sync convergence (any interleaving of submit/pull reaches the same final state).
- Live in each crate's `tests/`: `core/tests/props.rs` (tag/hash/bucket/path invariants),
  `netproto/tests/sign_props.rs` (sign/verify tamper-rejection), `db/tests/query_props.rs`
  (search vs a naive reference), `server/tests/delta_props.rs` (delta-sync convergence).

**Benchmarks** (`benches/` using `criterion`)
- Speed is the #1 requirement, so regressions are caught automatically.
- Key benchmarks: BLAKE3 throughput on large files, tag search + completion over 1,000,000
  mappings (plain SQL — the db does not use FTS5), repo snapshot/bucket/delta pull response time,
  indexer throughput (files/sec on a temp folder).
- Regenerate with `just bench` (or `just bench-quick` for a smoke pass).

### Conventions

- Every new crate gets a `tests/` directory and at least one integration test before any feature
  lands in it.
- The API types in `api/` have snapshot tests (e.g. `insta`) so wire-format changes are always
  explicit and reviewed.
- CI runs on GitHub Actions (fmt + clippy + full test suite on Linux and Windows, plus the
  frontend checks). `just test` runs the same gate locally before every push.
- Test helpers (temp DB factory, fixture file sets, test repo server builder) live in a
  `crates/test-support/` crate, not duplicated across test files.

---

## 11. Tech Stack

| Layer | Choice | Why |
|-------|--------|-----|
| Backend / daemon | **Rust** (`tokio`, `axum`, `rusqlite`, `blake3`) | Speed and safety; the #1 requirement. |
| Local database | **SQLite** (plain SQL search, indexed) | Proven for this exact workload; zero-admin, single file. |
| Networking | **HTTP** (`axum`) / async Rust | Federation needs no p2p transport; bucket reads stay CDN-cacheable and anonymous (ADR 0017). |
| UI | **TypeScript + Svelte/React + Tauri** | Rich gallery UX; one codebase for desktop *and* browser. |
| Repository server | **Rust** (`axum`); SQLite → PostgreSQL for scale | Same language as the client; portable schema. |

### Alternatives considered

- **DHT vs federation** for tag sharing → **federation** first (simpler, moderatable, matches the
  community/trusted-operator model). DHT discovery is a possible later addition.
  (ADR 0017)
- **libp2p vs plain HTTP** for the federation wire → **HTTP** (`axum`): bucket responses are
  content-addressed and cache behind a CDN, and no durable peer identity attaches to an anonymous
  read. (ADR 0017)
- **SHA-256 vs BLAKE3** for file identity → **BLAKE3-256**, with SHA-256 kept alongside as an optional
  interop key for Hydrus and torrents. (ADR 0018)
- **Native Rust UI (egui) vs web UI** → **web UI** for richer media/gallery presentation and a clean
  UI/backend split. (egui remains viable for a lightweight alternate frontend.)

Rationale for these is recorded in Architecture Decision Records maintained in the upstream private repository; this document covers the design overview.

---

## 12. Status & Contributing

**Pre-alpha — v0.3.4.** This document is the design spec (the *why*); Phases 0–2 are complete and
Phases 3–5 are in progress — see the [Roadmap](#9-roadmap) for delivered vs remaining. Networking is
implemented: signed submissions, accounts, k-anonymous bucket pulls, search/display, and delta
sync all ship today; in v0.2.0 the federation model was replaced with a simple client/server
design (ADR 0021). Remaining work is the Phase 4/5 tail (up/down votes, mirroring,
downloader plugins, perf hardening, Linux packaging).

Build instructions live in the [README](../README.md). The intent is to start as a solo project and
grow into a maintained open-source tool — the architecture is deliberately structured so additional
contributors can pick up whole crates with minimal coordination.
