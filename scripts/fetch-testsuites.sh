#!/usr/bin/env sh
# Fetches the W3C RDF and SPARQL test suites used by `cargo test -p holos-conformance`.
#
# They are not vendored: 33 MB of third-party fixtures under a different licence does not
# belong in this tree. Without them the conformance tests skip, so a fresh checkout still
# builds and tests green.
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
dest="$root/testsuites/rdf-tests"

if [ -d "$dest/.git" ]; then
  echo "updating $dest"
  git -C "$dest" fetch --depth 1 origin
  git -C "$dest" reset --hard origin/HEAD
else
  mkdir -p "$root/testsuites"
  git clone --depth 1 --filter=blob:none --sparse https://github.com/w3c/rdf-tests.git "$dest"
fi
git -C "$dest" sparse-checkout set rdf sparql
echo "done: $dest"
