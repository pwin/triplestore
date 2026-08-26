#!/usr/bin/env bash
# Back up the store, without stopping the service.
#
#   deploy/backup.sh /backups
#
# Uses a RocksDB checkpoint: the log is flushed and the SST files are hard-linked into a new
# directory, so the snapshot is consistent, near-instant, and initially costs almost no disk.
# This works on a store the service has open and is writing to. Copying the directory could
# not, which is why this script used to require `systemctl stop holos`.
#
# Two consequences of hard links, both of which matter:
#
#   * A checkpoint on the SAME filesystem as the store shares its files. That makes it cheap
#     and makes it NOT an off-machine backup — losing the disk loses both. Copy or replicate
#     the result somewhere else if that is what you need. On a different filesystem RocksDB
#     copies instead: correct, no longer instant, and genuinely independent.
#   * A checkpoint PINS the files it links, so compaction cannot delete them. Disk use climbs
#     as the snapshot and the live store diverge. Old checkpoints must be removed — see
#     KEEP below.
set -euo pipefail

cd "$(dirname "$0")/.."
[ -f deploy/holos.env ] && . deploy/holos.env
[ -f deploy/holos.env.local ] && . deploy/holos.env.local

DEST="${1:-./var/backups}"
STORE="${HOLOS_STORE:-./var/store}"
# How many checkpoints to keep. Retention is part of the job, not an afterthought: each one
# holds disk against compaction for as long as it exists.
KEEP="${HOLOS_BACKUP_KEEP:-7}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$DEST/holos-$STAMP"

[ -d "$STORE" ] || { echo "no store at $STORE" >&2; exit 1; }
[ -e "$OUT" ] && { echo "$OUT already exists" >&2; exit 1; }

HOLOS="${HOLOS_BIN:-./target/release/holos}"
[ -x "$HOLOS" ] || HOLOS="$(command -v holos || true)"
[ -n "$HOLOS" ] && [ -x "$HOLOS" ] || {
  echo "cannot find the holos binary; set HOLOS_BIN" >&2
  exit 1
}

mkdir -p "$DEST"
echo "checkpointing $STORE -> $OUT"
"$HOLOS" backup --store "$STORE" --to "$OUT"

# Record what produced it. A backup whose format version is unknown is a puzzle, not a backup.
{
  echo "taken:    $STAMP"
  echo "source:   $(cd "$STORE" && pwd)"
  echo "host:     $(hostname)"
  echo "method:   rocksdb checkpoint (service not stopped)"
  echo "binary:   $("$HOLOS" --help 2>&1 | head -1 || echo unknown)"
} > "$OUT/BACKUP-INFO.txt"

# `du` on a hard-linked checkpoint reports the space it would take if the links were copies,
# which is the number that matters when moving it and not the number it currently occupies.
echo "size: $(du -sh "$OUT" | cut -f1) (shared with the live store where hard-linked)"

if [ "$KEEP" -gt 0 ]; then
  # Oldest first, drop everything past KEEP. Names sort chronologically because the stamp is
  # ISO-8601 UTC, which is the reason for that format rather than a local one.
  mapfile -t OLD < <(find "$DEST" -maxdepth 1 -type d -name 'holos-*' | sort | head -n -"$KEEP")
  for dir in "${OLD[@]:-}"; do
    [ -n "$dir" ] || continue
    echo "removing old checkpoint $dir"
    rm -rf "$dir"
  done
fi

echo
echo "restore: point --store at it, or copy it back over $STORE with the service stopped"
