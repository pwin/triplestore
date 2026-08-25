#!/usr/bin/env bash
# Load RDF files into the persistent store.
#
#   deploy/load.sh data/*.ttl
#   HOLOS_STORE=/var/lib/holos/store deploy/load.sh dump.nq
#
# Stop the server first: one process at a time may hold the store directory.
set -euo pipefail

cd "$(dirname "$0")/.."
[ -f deploy/holos.env ] && . deploy/holos.env
[ -f deploy/holos.env.local ] && . deploy/holos.env.local

STORE="${HOLOS_STORE:-./var/store}"
BIN=./target/release/holos

[ $# -gt 0 ] || { echo "usage: $0 <file.ttl|file.nt|file.trig|file.nq> ..." >&2; exit 2; }
[ -x "$BIN" ] || { echo "$BIN not built — run deploy/setup.sh" >&2; exit 1; }
[ -n "$STORE" ] || { echo "HOLOS_STORE is empty; there is no persistent store to load into" >&2; exit 1; }

mkdir -p "$STORE"

# --bulk buffers writes and skips the write-ahead log: roughly 2.4x faster, at the cost of
# a part-way-interrupted load having to be discarded rather than resumed. That is the right
# trade for a load you can simply re-run, which is what this is.
ARGS=()
for f in "$@"; do
  [ -f "$f" ] || { echo "no such file: $f" >&2; exit 1; }
  ARGS+=(--data "$f")
done

echo "loading $# file(s) into $STORE"
time "$BIN" stats "${ARGS[@]}" --store "$STORE" --bulk

echo
echo "loaded. start the service with:  deploy/run.sh"
