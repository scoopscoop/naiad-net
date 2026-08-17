# Naiad task runner — the frequent build/package commands, from the repo root.
#
#   Install just:  cargo install just   (or: winget install Casey.Just)
#   List recipes:  just                 (or: just --list)
#
# Naming is symmetrical: `fe` = the Svelte frontend (→ ui/dist), `be` = the Rust
# daemon backend. Test recipes will follow the same shape (test-fe / test-be).

# PowerShell is the dev-box default shell. The recipes call npm / cargo / the
# packaging script, all of which are on PATH regardless of shell.
set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

# Default: list available recipes.
default:
    @just --list

# Build the Svelte frontend → ui/dist (svelte-check + vite build).
build-fe:
    npm --prefix ui run build

# Build the Rust daemon (debug) → target/debug/naiad. A debug daemon reads
# ui/dist live, so re-running `build-fe` alone refreshes the UI without recompiling.
build-be:
    cargo build

# Build both halves: frontend, then backend.
build: build-fe build-be

# Build and zip the Windows portable release → dist/Naiad-<version>-...zip.
# Rebuilds the frontend and a --release daemon internally (see scripts/package-windows.ps1).
package:
    powershell -ExecutionPolicy Bypass -File scripts/package-windows.ps1

# Build and zip the Windows portable server release → dist/Naiad-repo-<version>-...zip.
# Rebuilds a --release naiad-repo internally (see scripts/package-server.ps1).
package-server:
    powershell -ExecutionPolicy Bypass -File scripts/package-server.ps1

# Build and tar.gz a Linux/macOS naiad-repo release → dist/naiad-repo-<version>-<target>.tar.gz.
# TARGET defaults to the container-friendly static musl triple; pass another to
# override (e.g. `just package-server-tar aarch64-apple-darwin`). The bash script
# runs everywhere git-bash/sh is on PATH, and is what a CI release job calls.
package-server-tar target='x86_64-unknown-linux-musl':
    bash scripts/package-server.sh {{target}}

# Assert all four version sites agree with the workspace version. They drifted
# apart before (#155) and the UI badge reads tauri.conf.json, so a partial bump
# ships an app that misreports its own version.
#
# Two implementations because `set windows-shell := powershell` above makes the
# dev box PowerShell, while naiad-repo's CI and deploy targets are Linux — a
# PowerShell-only recipe would make `just test` unrunnable there.
[windows]
check-versions:
    powershell -ExecutionPolicy Bypass -File scripts/check-versions.ps1

[unix]
check-versions:
    sh scripts/check-versions.sh

# Run the workspace test suite the local convention runs before every push:
# version consistency, fmt check, clippy (deny warnings), then all tests.
test: check-versions
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Run every criterion benchmark. Full runs are slow (the 1M-mapping db bench
# builds its fixture first); `just bench-quick` for a fast smoke pass.
# `--benches` auto-selects every [[bench]] target in the workspace, so a newly
# added bench is picked up without editing this recipe (fixes #79). Every lib
# and bin target sets `bench = false` in its Cargo.toml so `--benches` selects
# only the real criterion benches — libtest lib/bin harnesses would otherwise
# reject criterion's `--quick` (see #37). Note: a NEW crate added to the
# workspace must also set `bench = false` on its [lib]/[[bin]], or `bench-quick`
# will error loudly here.
bench:
    cargo bench --workspace --benches

# Smoke-pass benchmarks with criterion --quick.
# Note: --quick only shortens the measured loop; both fixture databases
# (search_scale ~1 min, pull ~10s) are still built in full.
bench-quick:
    cargo bench --workspace --benches -- --quick
