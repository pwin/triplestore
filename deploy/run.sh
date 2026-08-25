#!/usr/bin/env bash
# Start holos-server with the settings in deploy/holos.env.
#
# The server takes flags, not environment variables. This script is the translation layer,
# so the binary keeps one inspectable surface (holos-server --help) and deployments keep
# one file to edit.
set -euo pipefail

cd "$(dirname "$0")/.."
[ -f deploy/holos.env ] && . deploy/holos.env
[ -f deploy/holos.env.local ] && . deploy/holos.env.local

BIN=./target/release/holos-server
[ -x "$BIN" ] || { echo "$BIN not built — run deploy/setup.sh" >&2; exit 1; }

ARGS=(--listen "${HOLOS_LISTEN:-127.0.0.1:7878}")
ARGS+=(--threads "${HOLOS_THREADS:-8}")

[ -n "${HOLOS_STORE:-}" ] && { mkdir -p "$HOLOS_STORE"; ARGS+=(--store "$HOLOS_STORE"); }
[ "${HOLOS_UI:-on}" = "off" ] && ARGS+=(--no-ui)

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

exec "$BIN" "${ARGS[@]}"
