#!/usr/bin/env bash
# Back up the store.
#
#   deploy/backup.sh /backups
#
# RocksDB checkpoints are named in DESIGN.md §6.1 but are NOT built yet, so there is no
# way to take a consistent snapshot of a store that is open. This script therefore does
# the only honest thing: it requires the service to be stopped.
#
# When checkpoints land this becomes a hard-linked online copy and the stop goes away.
set -euo pipefail

cd "$(dirname "$0")/.."
[ -f deploy/holos.env ] && . deploy/holos.env
[ -f deploy/holos.env.local ] && . deploy/holos.env.local

DEST="${1:-./var/backups}"
STORE="${HOLOS_STORE:-./var/store}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$DEST/holos-$STAMP"

[ -d "$STORE" ] || { echo "no store at $STORE" >&2; exit 1; }

# The lock file is held for the lifetime of the process. If we can take it, nobody else
# has it, which is exactly the condition for a safe copy.
if command -v fuser >/dev/null 2>&1 && fuser "$STORE/LOCK" >/dev/null 2>&1; then
  echo "ERROR: $STORE is open by another process." >&2
  echo "       Stop the service first:  systemctl stop holos" >&2
  echo "       An online copy is not consistent — checkpoints are not built yet." >&2
  exit 1
fi

mkdir -p "$OUT"
echo "copying $STORE -> $OUT"
cp -a "$STORE/." "$OUT/"

# Record what produced it. A backup whose format version is unknown is a puzzle, not a backup.
{
  echo "taken:    $STAMP"
  echo "source:   $(cd "$STORE" && pwd)"
  echo "host:     $(hostname)"
  echo "binary:   $(./target/release/holos-server --help 2>&1 | head -1 || echo unknown)"
} > "$OUT/BACKUP-INFO.txt"

echo "size: $(du -sh "$OUT" | cut -f1)"
echo
echo "restore by stopping the service and copying it back over $STORE"
