# Logging in naiad

naiad uses the `tracing` facade across the workspace. Library crates (`db`,
`indexer`, `netproto`) take the facade only — they install no subscriber, so
under tests/benches every macro is a no-op. The `core` crate is deliberately
silent (no `tracing` dependency). Subscribers are installed by the binaries:
the daemon (`init_tracing`), the repo server (`init_tracing`), and the CLI
(a `RUST_LOG`-gated stderr subscriber in the client subcommands only).

## Targets

Every event picks exactly one target from this closed set. New work reuses an
existing target, or the set is amended here and in
`crates/daemon/src/lib.rs` together.

| Target | Covers |
| --- | --- |
| `startup` | boot sequence, phase milestones, cache-warmup / catch-up completion |
| `db` | schema open/migrate, batch writers, search, relation graph, caches, vacuum, repo store reads/writes |
| `scan` | filesystem walk, hashing, metadata extraction, scan orchestration |
| `tags` | tag interning, sibling/parent merges, mapping merges |
| `watch` | file watcher lifecycle and events |
| `thumb` | thumbnail generation / serving |
| `search` | query execution surfaced to the user |
| `sync` | repo pull/submit orchestration, caps cache, all `RepoClient` round-trips |
| `http` | inbound HTTP request/response spans (both daemon and repo server) |
| `settings` | settings load / scaffold / migrate / reconcile |
| `hydrus` | Hydrus import paths: the daemon import driver, plus the `plugin-hydrus` source-read events (per-service relation/mapping boundaries, seed index-order checks) |
| `bridge` | PTR bridge subsystem: upstream PTR round-trips (session/metadata/update fetch in `ptr_client`), seed + sidecar ingest, both sync paths' per-index apply lines — the follow-loop (`sync.rs`) and the sidecar freshness path (`sidecar_sync.rs`), each `info` on a clean index and `warn` ("possible PTR format drift") on unknown content/def kinds, carrying `siblings`/`parents` counts (now the number of bridged relation rows **applied** to the RepoStore this index, #225, not skipped), `dropped_relations` (relation rows dropped for an unparseable/unknown endpoint) and `dropped_tags` — the seed relations phase's start/end `info` (`streamed`/`applied`/`siblings`/`parents`) — and the single-writer lock (#193) acquire/release (`debug`). Store-level relation writes (`apply_bridge_relations`) log one `debug` under `db` carrying `applied`/`unchanged`/`author`, mirroring `apply_relation` |
| `repo` | repo-store caps handler fallbacks (distinct-hash-count / store-generation reads in `http.rs`) |
| `cli` | CLI→daemon local-API round-trips: the blocking `ureq` client in `crates/cli/src/client.rs` (one `debug` line per request: `method`/`path`/`status`/`elapsed_ms`) |

## Levels

| Level | Contract |
| --- | --- |
| `error` | a failure that currently returns silently or aborts an operation |
| `warn` | a degradation the user should know about but that does not abort (dropped error arms, guard re-fetches, repo rejections, auth/ban denials) |
| `info` | one line per user-meaningful operation, carrying counts + elapsed |
| `debug` | per-branch and per-batch detail (which pull mode, per-merge-batch counts) |
| `trace` | per-item hot paths (per file, per row, per HTTP request) |

`error!` is live in the daemon on the double-failure rollback paths in
`server.rs` (repo subscribe and unsubscribe): when the `naiad.toml` write fails
*and* its DB rollback (detach / re-attach) also fails, persisted DB and toml
state diverge and the operation aborts — the one class of daemon failure that
leaves inconsistent state rather than degrading cleanly. The daemon's
`internal()` funnel (`server.rs`) additionally emits one `error!` under target
`http` (`request failed: internal server error (500)`, carrying the `error`
message) whenever any `.map_err(internal)` maps an unexpected error to a
`500`, so a daemon 500 that previously returned silently is now logged.

**Hard rule:** nothing above `trace` may be emitted from inside a per-item loop.
A loop is summarised once, at its boundary, with the aggregate count and
duration. This keeps default output bounded by the number of *operations*, not
the number of *items*, and keeps per-item instrumentation free when `trace` is
disabled.

Fields: durations as `elapsed_ms`; counts as structured fields (`rows`,
`files`, `mappings`, `edges`, `matched`), never interpolated into the message,
so a JSON subscriber can consume them.

## HTTP request tracing

Both the daemon and the repo server attach a `tower-http` `TraceLayer`
outermost, emitting request spans at `trace` under target `http` with
`method` / `path` / `status` / `latency_ms`. At the default `info` level this is
off, so there is no measurable per-request overhead in production.

Above that `trace` span, the repo server emits one `debug` line under `http` per
*successful* anonymous read (`served snapshot`/`served caps`/`served buckets`),
carrying the client IP (bare address, no port — a local debug aid, off at the
default `info`), the requested `domain` and bucket count, and the returned byte
size plus hash/tag or change counts. On the write path the repo server also emits one `warn` line under `http` when a signed submission or relation is rejected for a bad signature (`submission signature rejected` / `relation signature rejected`, carrying `author` and `error`), mirroring the existing `auth failed` warning. The read path likewise emits one `warn` line under `http` (`bucket request rejected: unsupported protocol version`, carrying `client`/`version`/`error`) when a bucket request's protocol version falls outside the server's supported range. The same handler additionally emits a `warn` under `http` on each of its other bucket-request validation rejections — since-length mismatch, a sha256 prefix-bits floor violation, an unsupported `since` on a snapshot-mode domain, and a malformed bucket key (`bucket request rejected: ...`, each carrying `client`) — so no bucket-read 400 is silent. The repo server also emits one fieldless `warn` line under `http` when a write handler (`submit`/`relations submit`/`report`/`moderate`) is refused because the repo is serving read-only (#202), messages `submit rejected: repo is read-only` / `relation submit rejected: repo is read-only` / `report rejected: repo is read-only` / `moderate rejected: repo is read-only`. The three authenticated write handlers that parse their body by hand (`submit`/`report`/`moderate`) each also emit one `warn` line under `http` when that body is malformed JSON (`submission body rejected: malformed JSON` / `report body rejected: malformed JSON` / `moderate action body rejected: malformed JSON`, carrying the authenticated `key` and the parse `error`), so a post-auth 400 on a bad body is no longer silent. On the daemon side, the two request-guard middlewares now each emit one `warn` line under `http` when they reject a request: the cross-origin/CSRF guard (`request rejected: cross-origin (CSRF guard)`, carrying `method`/`path`/`sec_fetch_site`/`origin`) and the non-local connection guard (`connection rejected: non-local peer`, carrying `peer`/`method`/`path`), so a daemon 403 from either guard is no longer silent. The daemon's `GET /file/{hash}` handler additionally logs its failure paths under `http` (previously silent, unlike the sibling thumbnail handler): a `debug` line (`file request: no present location`, carrying `hash`) when the hash has no present location, and a `warn` line (`file read failed for present location`, carrying `hash` and the IO `error`) when the on-disk read fails despite the DB reporting the location present — a genuine degradation that was invisible. The repo server's `relations_handler` now also logs its 500 funnel under `http` at `error`: `"relations: store error"` (carrying `error`) when the store call fails, and `"relations: task join error"` (carrying `error`) when the blocking task panics — mirroring `snapshot_handler` exactly and closing the last silent read-handler 500 in the repo server. On the client side the `sync` round-trip
lines now also carry `tags` (all of them) and `body_len` (`fetched snapshot` and
the per-chunk `fetched bucket chunk` line), and the daemon's incremental
`pull_domain_delta` logs a `debug` boundary summary
(`keys`/`since_zero`/`changes`/`mappings`/`cursor`). The `RepoClient` write/relation/report methods (`submit`/`submit_relation`/`fetch_relations`/`fetch_relations_since`/`report`/`fetch_reports`/`moderate`) already emit a `warn` under `sync` when the repo rejects a request (`repo rejected submission` / `repo rejected relation` / `repo rejected report` / `repo rejected report fetch` / `repo rejected moderation`, carrying `url`/`code`/`reason`); they now also emit a `warn` under `sync` when the repo is unreachable (a transport failure such as connection refused, DNS failure, or timeout, carrying `url`/`error`), mirroring those repo-rejection warns so a repo-unreachable failure is no longer silent while a repo-rejection is logged. Additionally, the daemon
emits one `debug` line per request window during bucketed pulls at level `debug`
under target `sync`, message `pull window`, with fields `repo`/`domain`/`window`/`done`/`total`/`chunk_bytes`/`cumulative`/`hashes`/`tags`/`request_ms` — one line per window request, satisfying the same per-operation boundedness contract as the `#172` lines above.
On a window-level retry (#177) one additional `debug` line is emitted per shrink attempt under the same target `sync`, message `pull window retry`, with fields `repo`/`domain`/`done`/`total`/`old_window`/`new_window`/`attempt`/`reason`/`retries` — one line per retry, greppable to observe a cold-region shrink-retry recovery.
Before the first window of each domain pull (#178) one `debug` line is emitted under target `sync`, message `first-window seed`, with fields `domain`/`advertised`/`requested_bits`/`hint_bits`/`seed_ms` — one line per domain per pull, recording which width-normalised hint (or bootstrap default) was used to size the opening window.
The CLI's own client (`crates/cli/src/client.rs`) emits one `debug` line per daemon round-trip under target `cli`, off at the default `warn` filter and surfaced with `RUST_LOG=cli=debug`.

## Startup progress protocol

While it boots, the daemon prints five machine-parseable lines to **stderr**
via plain `eprintln!` (deliberately not `tracing`, so they are immune to
`RUST_LOG` and format-stable). Grammar:

```
naiad-startup <step>/<total> <label>
```

- `<step>` and `<total>` are decimal integers; `<total>` is always `5`.
- `<label>` is free human text (may contain spaces and parentheticals).
- Produced by the single shared function `startup_progress_line` in
  `crates/daemon/src/lib.rs`, pinned by a format test so it cannot drift.

| Step | Label | Phase |
| --- | --- | --- |
| 1/5 | `opening database (migrations may take a while)` | opening + migrating the library DB |
| 2/5 | `loading settings` | scaffold / trust-floor migrate / repo reconcile |
| 3/5 | `opening read pool` | read-only connection pool |
| 4/5 | `preparing file watcher` (or `file watching off` with `--no-watch`) | build the (empty) watcher; roots register in the background |
| 5/5 | `binding server address` | bind the HTTP listener |

The stdout handshake `naiad daemon on http://{bound}` remains the **sole
readiness signal**; progress lines are stderr. The Tauri shell parses the
progress lines (`parse_startup_progress`) into a `daemon://progress` event and
the loading page draws a determinate bar; an old daemon (no lines) or an old
shell both degrade cleanly to the previous indeterminate behaviour.

## RUST_LOG / `[log].level` precedence

The filter directive resolves `RUST_LOG` (when set and non-blank) over
`naiad.toml` `[log].level` over the built-in default. The full precedence
ladder and TOML control surface are documented in
`docs/superpowers/specs/2026-07-06-expanded-logging-toml-control-design.md`.
Per-target tuning works as usual, e.g. `RUST_LOG=sync=debug,http=trace`.

On `naiad-repo`, the ladder is extended: `RUST_LOG` → `NAIAD_REPO_LOG_LEVEL`
→ `[log].level` → `info` (ADR 0025). `RUST_LOG` stays on top so existing
debugging habits and supervisor snippets work unchanged; `NAIAD_REPO_LOG_LEVEL`
is the deployment-shaped knob for container and service deployments.
