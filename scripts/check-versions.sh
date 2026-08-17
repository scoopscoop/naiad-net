#!/usr/bin/env sh
# POSIX counterpart of check-versions.ps1, so `just test` works on Linux —
# where naiad-repo is headed. Same rule: every version site must match the
# workspace version, because the UI's version badge reads tauri.conf.json and a
# partial bump ships an app that misreports itself (#155).
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# First `^version = "..."` in a Cargo.toml is the package/workspace one;
# anchoring on the line start avoids matching a dependency's inline version.
cargo_version() {
    sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$1" | head -n1
}
json_version() {
    sed -n 's/.*"version" *: *"\([^"]*\)".*/\1/p' "$1" | head -n1
}

expected=$(cargo_version "$root/Cargo.toml")
[ -n "$expected" ] || { echo "no version found in Cargo.toml" >&2; exit 1; }

bad=''
check() {
    [ -n "$2" ] || { echo "no version found in $1" >&2; exit 1; }
    [ "$2" = "$expected" ] || bad="$bad  $1 = $2
"
}

check 'ui/package.json'              "$(json_version  "$root/ui/package.json")"
check 'ui/src-tauri/Cargo.toml'      "$(cargo_version "$root/ui/src-tauri/Cargo.toml")"
check 'ui/src-tauri/tauri.conf.json' "$(json_version  "$root/ui/src-tauri/tauri.conf.json")"

if [ -n "$bad" ]; then
    echo "version mismatch (workspace Cargo.toml = $expected):"
    printf '%s' "$bad"
    echo 'Bump every site together - the UI version badge reads tauri.conf.json.'
    exit 1
fi

echo "all version sites agree: $expected"
