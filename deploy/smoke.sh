#!/usr/bin/env bash
# Verify a running instance. Exits non-zero on the first failure, so it works as a
# deployment gate or a container health check.
#
#   deploy/smoke.sh                       # against 127.0.0.1:7878
#   deploy/smoke.sh http://host:7878      # against somewhere else
set -uo pipefail

BASE="${1:-http://127.0.0.1:7878}"
PASS=0; FAIL=0

check() {
  local what="$1" got="$2" want="$3"
  if [[ "$got" == *"$want"* ]]; then
    printf '  \033[32mpass\033[0m  %s\n' "$what"; PASS=$((PASS+1))
  else
    printf '  \033[31mFAIL\033[0m  %s\n' "$what"
    printf '        wanted to find: %s\n' "$want"
    printf '        got:            %s\n' "${got:0:200}"
    FAIL=$((FAIL+1))
  fi
}

echo "smoke testing $BASE"

check "GET /health" "$(curl -fsS --max-time 10 "$BASE/health" 2>&1)" "ok"

check "GET /stats returns JSON" \
      "$(curl -fsS --max-time 10 "$BASE/stats" 2>&1)" '"quads"'

check "GET /query answers ASK" \
      "$(curl -fsS --max-time 30 -H 'Accept: application/sparql-results+json' \
         --get --data-urlencode 'query=ASK { ?s ?p ?o }' "$BASE/query" 2>&1)" '"boolean"'

check "POST /query, form-encoded" \
      "$(curl -fsS --max-time 30 -H 'Accept: text/csv' \
         --data-urlencode 'query=SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }' "$BASE/query" 2>&1)" "n"

check "POST /query, application/sparql-query" \
      "$(curl -fsS --max-time 30 -H 'Content-Type: application/sparql-query' \
         -H 'Accept: text/csv' --data 'SELECT * WHERE { ?s ?p ?o } LIMIT 1' "$BASE/query" 2>&1)" "s"

check "CONSTRUCT negotiates turtle" \
      "$(curl -fsS --max-time 30 -H 'Accept: text/turtle' \
         --get --data-urlencode 'query=CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o } LIMIT 1' "$BASE/query" 2>&1; echo ok)" "ok"

check "a syntax error is a 400, not a 500" \
      "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
         --get --data-urlencode 'query=SELECT nonsense' "$BASE/query")" "400"

check "POST /update answers 501" \
      "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -X POST "$BASE/update")" "501"

echo
if [ "$FAIL" -eq 0 ]; then
  printf '\033[32m%d passed\033[0m\n' "$PASS"; exit 0
else
  printf '\033[31m%d passed, %d failed\033[0m\n' "$PASS" "$FAIL"; exit 1
fi
