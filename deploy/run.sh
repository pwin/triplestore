#!/usr/bin/env bash
# Start holos-server with the settings in deploy/holos.env.
#
# The server takes flags, not environment variables. This script is the translation layer,
# so the binary keeps one inspectable surface (holos-server --help) and deployments keep
# one file to edit.
set -euo pipefail

cd "$(dirname "$0")/.."
# The environment wins over the files, as it does for the PowerShell scripts, so a service
# manager or a container can override any one setting without editing anything on disk.
# Sourcing would clobber it, so whatever was set beforehand is put back afterwards.
HOLOS_SET_BEFORE="$(env | grep '^HOLOS_' || true)"
[ -f deploy/holos.env ] && . deploy/holos.env
[ -f deploy/holos.env.local ] && . deploy/holos.env.local
while IFS= read -r kv; do [ -n "$kv" ] && export "$kv"; done <<< "$HOLOS_SET_BEFORE"

BIN=./target/release/holos-server
[ -x "$BIN" ] || { echo "$BIN not built — run deploy/setup.sh" >&2; exit 1; }

ARGS=(--listen "${HOLOS_LISTEN:-127.0.0.1:7878}")
ARGS+=(--threads "${HOLOS_THREADS:-8}")

[ -n "${HOLOS_STORE:-}" ] && { mkdir -p "$HOLOS_STORE"; ARGS+=(--store "$HOLOS_STORE"); }
[ "${HOLOS_UI:-on}" = "off" ] && ARGS+=(--no-ui)
[ -n "${HOLOS_UI_TILES:-}" ] && ARGS+=(--ui-tiles "$HOLOS_UI_TILES")

for f in ${HOLOS_DATA:-}; do ARGS+=(--data "$f"); done

# Identity. The server refuses to read forwarded headers unless asked, so this flag is the
# whole difference between "every request is anonymous" and "the front door decides".
if [ "${HOLOS_TRUST_FORWARDED:-off}" = "on" ]; then
  ARGS+=(--trust-forwarded-identity)
  case "${HOLOS_LISTEN:-127.0.0.1:7878}" in
    127.0.0.1:*|localhost:*|[::1]:*) ;;
    *) echo "WARNING: --trust-forwarded-identity with a non-loopback bind address." >&2
       echo "         Any client that can reach $HOLOS_LISTEN can now name its own roles." >&2
       echo "         Bind to loopback and put a front door in front. See OPERATIONS.md." >&2 ;;
  esac
fi

for r in ${HOLOS_DEV_ROLES:-}; do
  echo "WARNING: HOLOS_DEV_ROLES grants '$r' to every request, authenticated or not." >&2
  ARGS+=(--role "$r")
done
[ -n "${HOLOS_DEV_CLEARANCE:-}" ] && {
  echo "WARNING: HOLOS_DEV_CLEARANCE grants clearance ${HOLOS_DEV_CLEARANCE} to every request." >&2
  ARGS+=(--clearance "$HOLOS_DEV_CLEARANCE")
}

# Policy.
[ "${HOLOS_DENY_ALL:-off}" = "on" ]     && ARGS+=(--deny-all)
[ "${HOLOS_FAIL_CLOSED:-off}" = "on" ]  && ARGS+=(--fail-closed)
for g in ${HOLOS_ALLOW_GRAPHS:-};     do ARGS+=(--allow-graph "$g");     done
for p in ${HOLOS_DENY_PREDICATES:-};  do ARGS+=(--deny-predicate "$p");  done
for l in ${HOLOS_LABEL_GRAPHS:-};     do ARGS+=(--label-graph "$l");     done

# Serving.
[ "${HOLOS_READ_ONLY:-off}" = "on" ] && ARGS+=(--read-only)
[ "${HOLOS_REORDER:-off}" = "on" ]   && ARGS+=(--reorder)
[ -n "${HOLOS_TIMEOUT:-}" ]          && ARGS+=(--timeout "$HOLOS_TIMEOUT")
[ -n "${HOLOS_MAX_QUERY_MEMORY:-}" ] && ARGS+=(--max-query-memory "$HOLOS_MAX_QUERY_MEMORY")
[ -n "${HOLOS_BACKUP_DIR:-}" ]       && { mkdir -p "$HOLOS_BACKUP_DIR"; ARGS+=(--backup-dir "$HOLOS_BACKUP_DIR"); }
[ -n "${HOLOS_BACKUP_ROLE:-}" ]      && ARGS+=(--backup-role "$HOLOS_BACKUP_ROLE")

# Anything else the server accepts, verbatim. `holos-server --help` is the list.
# shellcheck disable=SC2206
[ -n "${HOLOS_EXTRA_ARGS:-}" ] && ARGS+=(${HOLOS_EXTRA_ARGS})

exec "$BIN" "${ARGS[@]}"
