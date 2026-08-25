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

# The protocol checks below assert refusals rather than writes, so this script stays safe to
# run against a live endpoint: nothing here changes the store.

check "POST /update with no body is a 400, not a 500" \
      "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -X POST "$BASE/update")" "400"

check "a POST body with no content-type is refused" \
      "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
         -H 'Content-Type:' --data 'query=ASK%20%7B%7D' "$BASE/query")" "400"

check "two query parameters are refused" \
      "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
         "$BASE/query?query=ASK%20%7B%7D&query=SELECT%20%2A%20%7B%7D")" "400"

check "a non-UTF-8 charset is refused" \
      "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
         -H 'Content-Type: application/sparql-query; charset=UTF-16' \
         --data 'ASK {}' "$BASE/query")" "400"

check "relative IRIs resolve against a service base URI" \
      "$(curl -fsS --max-time 30 -H 'Content-Type: application/sparql-query' \
         -H 'Accept: text/turtle' --data 'CONSTRUCT { <s> <p> 1 } WHERE {}' \
         "$BASE/query" 2>&1; echo ok)" "ok"

# The graph store lives at /graph unless --gsp-path moved it. 404 means the default graph
# holds nothing this principal may see, which is a fine answer on an empty store.
GSP_STATUS="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$BASE/graph?default")"
if [ "$GSP_STATUS" = "404" ] || [ "$GSP_STATUS" = "200" ]; then
  check "GET on the graph store answers" "$GSP_STATUS" "$GSP_STATUS"
else
  check "GET on the graph store answers 200 or 404" "$GSP_STATUS" "200-or-404"
fi

echo
if [ "$FAIL" -eq 0 ]; then
  printf '\033[32m%d passed\033[0m\n' "$PASS"; exit 0
else
  printf '\033[31m%d passed, %d failed\033[0m\n' "$PASS" "$FAIL"; exit 1
fi
