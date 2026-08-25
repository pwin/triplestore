#!/usr/bin/env sh
# Fetches the W3C RDF and SPARQL test suites used by `cargo test -p holos-conformance`.
#
# They are not committed to this tree: 33 MB of third-party fixtures under a different licence does not
# belong in this tree. Without them the conformance tests skip, so a fresh checkout still
# builds and tests green.
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
dest="$root/testsuites/rdf-tests"

# The fixtures are byte-exact test data. A Turtle literal that spells `` means a carriage
# return, and the file's own line endings are part of the value; Git for Windows ships with
# `core.autocrlf=true`, which rewrites every LF on checkout and so silently changes what the
# suite asserts. Pinning the config here makes the files land as the W3C published them
# whatever the machine's global Git settings say.
if [ -d "$dest/.git" ]; then
  echo "updating $dest"
  git -C "$dest" config core.autocrlf false
  git -C "$dest" config core.eol lf
  git -C "$dest" fetch --depth 1 origin
  # Changing the config does not rewrite files already in the working tree. Dropping the
  # index forces every one to be checked out again under the new setting.
  git -C "$dest" rm --cached -r -q . >/dev/null 2>&1 || true
  git -C "$dest" reset --hard origin/HEAD
else
  mkdir -p "$root/testsuites"
  git -c core.autocrlf=false -c core.eol=lf       clone --depth 1 --filter=blob:none --sparse       https://github.com/w3c/rdf-tests.git "$dest"
  git -C "$dest" config core.autocrlf false
  git -C "$dest" config core.eol lf
fi
git -C "$dest" sparse-checkout set rdf sparql

# Cheap proof the above worked: a CR in a fixture means the checkout is translating.
if git -C "$dest" grep -qI --cached $'' -- 'sparql/**/*.ttl' 2>/dev/null; then
  echo "warning: $dest holds CRLF line endings; expected values will not match" >&2
fi
echo "done: $dest"
