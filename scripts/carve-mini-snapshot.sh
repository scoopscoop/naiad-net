#!/bin/sh
# carve-mini-snapshot.sh — reduce a real Hydrus PTR snapshot to a few-GB
# mini-snapshot with intact referential integrity, for mirror-mode E2E testing.
#
# Keeps only hashes whose FIRST BYTE < BAND_HI_BYTE (a hash-prefix band), drops
# every row orphaned by that reduction, and leaves the repository_updates_*
# watermark tables intact so `recover_watermark` still returns the real PTR
# index. Operates on COPIES only; never modifies the source snapshot.
#
# Usage: carve-mini-snapshot.sh SRC_DIR DEST_DIR SVC_ID BAND_HI_BYTE
#   SRC_DIR       directory holding client.db, client.master.db, client.mappings.db
#   DEST_DIR      output directory (MUST NOT already exist)
#   SVC_ID        Hydrus tag-service id inside the snapshot (e.g. 9, or the PTR id)
#   BAND_HI_BYTE  exclusive upper bound on hash byte 0, hex (e.g. 0x10 = keep 1/16)
#
# Example: carve-mini-snapshot.sh /srv/ptr-snapshot /srv/ptr-mini 9 0x10
set -eu

usage() {
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-2}"
}

[ "${1:-}" = "--help" ] && usage 0
[ "$#" -eq 4 ] || usage 2

SRC_DIR=$1
DEST_DIR=$2
SVC=$3
BAND_HI_BYTE=$4

command -v sqlite3 >/dev/null 2>&1 || { echo "error: sqlite3 not found on PATH" >&2; exit 1; }
[ -d "$SRC_DIR" ] || { echo "error: SRC_DIR does not exist: $SRC_DIR" >&2; exit 1; }
[ -e "$DEST_DIR" ] && { echo "error: DEST_DIR already exists, refusing to overwrite: $DEST_DIR" >&2; exit 1; }
for f in client.db client.master.db client.mappings.db; do
    [ -f "$SRC_DIR/$f" ] || { echo "error: missing snapshot file: $SRC_DIR/$f" >&2; exit 1; }
done
case "$SVC" in ''|*[!0-9]*) echo "error: SVC_ID must be an integer: $SVC" >&2; exit 1;; esac

# Normalise BAND_HI_BYTE (accept 0x10 or 10) to a two-hex-digit string.
HB=$(printf '%s' "$BAND_HI_BYTE" | sed 's/^0[xX]//')
case "$HB" in
    [0-9a-fA-F][0-9a-fA-F]) : ;;
    [0-9a-fA-F]) HB="0$HB" ;;
    *) echo "error: BAND_HI_BYTE must be one byte of hex (e.g. 0x10): $BAND_HI_BYTE" >&2; exit 1 ;;
esac
# A 32-byte upper-bound blob: <HB> followed by 31 zero bytes = smallest hash NOT in band.
HI_BLOB="X'${HB}$(printf '%062d' 0)'"

echo "carve: copying snapshot to $DEST_DIR ..." >&2
mkdir -p "$DEST_DIR"
cp -p "$SRC_DIR/client.db"          "$DEST_DIR/client.db"
cp -p "$SRC_DIR/client.master.db"   "$DEST_DIR/client.master.db"
cp -p "$SRC_DIR/client.mappings.db" "$DEST_DIR/client.mappings.db"

echo "carve: reducing (service $SVC, band < 0x$HB) ..." >&2
# Drive from client.master.db as the main DB; ATTACH the mappings DB. The
# watermark tables live in client.db and are deliberately NOT touched.
sqlite3 "$DEST_DIR/client.master.db" <<SQL
PRAGMA foreign_keys=OFF;
ATTACH '$DEST_DIR/client.mappings.db' AS mappings;

-- 1. Keep only in-band hashes.
DELETE FROM hashes WHERE hash >= $HI_BLOB;

-- 2. Drop mapping rows referencing a now-absent hash_id (current + deleted).
DELETE FROM mappings.current_mappings_$SVC
  WHERE hash_id NOT IN (SELECT hash_id FROM hashes);
DELETE FROM mappings.deleted_mappings_$SVC
  WHERE hash_id NOT IN (SELECT hash_id FROM hashes);

-- 3. Drop tag defs no longer referenced by any surviving mapping.
DELETE FROM tags WHERE tag_id NOT IN (
    SELECT tag_id FROM mappings.current_mappings_$SVC
    UNION SELECT tag_id FROM mappings.deleted_mappings_$SVC);
DELETE FROM namespaces WHERE namespace_id NOT IN (SELECT namespace_id FROM tags);
DELETE FROM subtags    WHERE subtag_id    NOT IN (SELECT subtag_id    FROM tags);

-- 4. Prune the service-id -> master-id maps to surviving ids (follow-loop defs).
DELETE FROM repository_hash_id_map_$SVC WHERE hash_id NOT IN (SELECT hash_id FROM hashes);
DELETE FROM repository_tag_id_map_$SVC  WHERE tag_id  NOT IN (SELECT tag_id  FROM tags);

-- 5. Watermark tables (client.repository_updates_$SVC / _processed_$SVC) untouched.
SQL

# VACUUM needs ~file-size temp space; client.db is VACUUMed for uniformity though untouched.
echo "carve: VACUUM ..." >&2
sqlite3 "$DEST_DIR/client.master.db"   'VACUUM;'
sqlite3 "$DEST_DIR/client.mappings.db" 'VACUUM;'
sqlite3 "$DEST_DIR/client.db"          'VACUUM;'

echo "carve: integrity + counts ..." >&2
for f in client.db client.master.db client.mappings.db; do
    printf '%s: ' "$f"
    sqlite3 "$DEST_DIR/$f" 'PRAGMA integrity_check;'
done
echo "surviving master.hashes:            $(sqlite3 "$DEST_DIR/client.master.db" 'SELECT COUNT(*) FROM hashes;')"
echo "surviving mappings.current_$SVC:    $(sqlite3 "$DEST_DIR/client.mappings.db" "SELECT COUNT(*) FROM current_mappings_$SVC;")"
echo "surviving master.tags:              $(sqlite3 "$DEST_DIR/client.master.db" 'SELECT COUNT(*) FROM tags;')"
ls -lh "$DEST_DIR"
echo "carve: done -> $DEST_DIR" >&2
