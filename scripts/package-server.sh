#!/usr/bin/env bash
# Build and package a naiad-repo (server) Linux/macOS .tar.gz release.
#
# POSIX/bash counterpart of scripts/package-server.ps1 (the Windows .zip). Ships
# the same payload shape as the Windows zip: the server binary + a commented
# repo.toml sample + the operator guide as README.md + LICENSE. Produces:
#
#     dist/naiad-repo-<version>-<target>.tar.gz
#
# which extracts to a single self-named folder:
#
#     naiad-repo-<version>-<target>/
#       naiad-repo      -- the repository node (chmod +x)
#       repo.toml       -- commented sample config (scripts/repo.toml.sample)
#       README.md       -- copy of docs/operating-a-repo.md
#       LICENSE         -- copy of the repo LICENSE
#
# Usage:
#     scripts/package-server.sh [TARGET_TRIPLE]
#
#   TARGET_TRIPLE defaults to x86_64-unknown-linux-musl -- a static build with
#   no libc version coupling, the friendliest artifact for containers and for
#   "download and run anywhere" VPS deployments (the primary naiad-repo target;
#   see the deployment note in docs/operating-a-repo.md). Pass another triple to
#   override, e.g.:
#
#     scripts/package-server.sh x86_64-unknown-linux-gnu      # glibc, dynamic
#     scripts/package-server.sh aarch64-unknown-linux-musl    # arm64 static
#
# (macOS is deferred until a macOS runner exists -- ADR 0027 sec 3. The script
# imposes no allowlist, so an *-apple-darwin triple still works if run on a mac,
# but macOS is intentionally not advertised in the user-facing docs.)
#
# Build order (each step fails fast, mirroring package-server.ps1):
#   1. Assert every version site agrees (shared scripts/check-versions.sh; the
#      single source of truth is workspace Cargo.toml [workspace.package].version).
#   2. Ensure the target toolchain is installed (rustup target add, best effort).
#   3. cargo build --release -p naiad-server --target <triple>.
#   4. Stage naiad-repo + repo.toml + README.md + LICENSE into dist/staging/.
#   5. tar czf -> dist/naiad-repo-<version>-<target>.tar.gz, print the sha256.
#
# CI: a release job can call
# this directly on a Linux runner, e.g.
#     script:
#       - rustup target add x86_64-unknown-linux-musl
#       - apt-get update && apt-get install -y musl-tools   # musl linker
#       - scripts/package-server.sh x86_64-unknown-linux-musl
# then publish dist/naiad-repo-*.tar.gz as release assets.
set -euo pipefail

target="${1:-x86_64-unknown-linux-musl}"

# --- Resolve the repo root from this script's location (run from anywhere) ---
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root"

log()  { printf '%s\n' "$*"; }
step() { printf '==> %s\n' "$*"; }
die()  { printf 'package-server: %s\n' "$*" >&2; exit 1; }

# --- Step 1: version agreement (shared gate; single source of truth) ---------
step "version gate (scripts/check-versions.sh)"
sh "$script_dir/check-versions.sh"

# First `^version = "..."` in the workspace Cargo.toml is the [workspace.package]
# one; anchoring on line start avoids matching a dependency's inline version.
# (Same extraction check-versions.sh uses as its source of truth.)
version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$repo_root/Cargo.toml" | head -n1)
[ -n "$version" ] || die "could not read version from Cargo.toml"
log "Packaging naiad-repo v$version for $target"

# --- Step 2: ensure the target toolchain is present (best effort) ------------
if command -v rustup >/dev/null 2>&1; then
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
        step "rustup target add $target"
        rustup target add "$target" || die "failed to add target $target"
    fi
else
    log "note: rustup not found; assuming target $target is already available"
fi

case "$target" in
    *-musl)
        # The musl target needs a musl-capable linker (musl-gcc / musl-tools on
        # Debian/Ubuntu, musl-cross on macOS via brew). Warn early rather than
        # letting the linker fail with a cryptic message mid-build.
        if ! command -v musl-gcc >/dev/null 2>&1 \
           && ! command -v "${target%%-*}-linux-musl-gcc" >/dev/null 2>&1; then
            log "note: no musl linker on PATH (musl-gcc). If the build fails to"
            log "      link, install it: apt-get install musl-tools (Debian/Ubuntu)."
        fi
        ;;
esac

# --- Step 3: build (fail fast) -----------------------------------------------
step "cargo build --release -p naiad-server --target $target"
cargo build --release -p naiad-server --target "$target"

bin_src="target/$target/release/naiad-repo"
[ -f "$bin_src" ] || die "expected binary missing after build: $bin_src"

# --- Step 4: stage the tarball payload ---------------------------------------
sample="$repo_root/scripts/repo.toml.sample"
guide="$repo_root/docs/operating-a-repo.md"
license="$repo_root/LICENSE"
for f in "$sample" "$guide" "$license"; do
    [ -f "$f" ] || die "expected staging input missing: $f"
done

folder="naiad-repo-$version-$target"
stage_root="$repo_root/dist/staging"
stage_dir="$stage_root/$folder"
rm -rf "$stage_dir"
mkdir -p "$stage_dir"

cp "$bin_src" "$stage_dir/naiad-repo"
chmod +x "$stage_dir/naiad-repo"
cp "$sample"  "$stage_dir/repo.toml"
cp "$guide"   "$stage_dir/README.md"
cp "$license" "$stage_dir/LICENSE"

# --- Step 5: tar ------------------------------------------------------------
tarball="$repo_root/dist/$folder.tar.gz"
rm -f "$tarball"
# -C into staging so the archive holds the self-named folder, not dist/staging/.
tar -czf "$tarball" -C "$stage_root" "$folder"

size=$(du -h "$tarball" | cut -f1)
log ""
log "Created $tarball ($size)"
log "  contents: $folder/{naiad-repo, repo.toml, README.md, LICENSE}"

# Best-effort checksum line for release notes (sha256sum on Linux, shasum on mac).
if command -v sha256sum >/dev/null 2>&1; then
    log "  sha256:   $(sha256sum "$tarball" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
    log "  sha256:   $(shasum -a 256 "$tarball" | cut -d' ' -f1)"
fi
