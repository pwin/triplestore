# Fetches the W3C RDF and SPARQL test suites used by `cargo test -p holos-conformance`.
# See fetch-testsuites.sh for why they are not committed to this tree.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'testsuites\rdf-tests'

# The fixtures are byte-exact test data. A Turtle literal that spells `` means a carriage
# return, and the file's own line endings are part of the value; Git for Windows ships with
# `core.autocrlf=true`, which rewrites every LF on checkout and so silently changes what the
# suite asserts. Pinning the config here makes the files land as the W3C published them
# whatever the machine's global Git settings say. This matters most on Windows, which is
# exactly where this script runs.
if (Test-Path (Join-Path $dest '.git')) {
    Write-Host "updating $dest"
    git -C $dest config core.autocrlf false
    git -C $dest config core.eol lf
    git -C $dest fetch --depth 1 origin
    # Changing the config does not rewrite files already in the working tree. Dropping the
    # index forces every one to be checked out again under the new setting.
    git -C $dest rm --cached -r -q . 2>$null | Out-Null
    git -C $dest reset --hard origin/HEAD
} else {
    New-Item -ItemType Directory -Force (Join-Path $root 'testsuites') | Out-Null
    git -c core.autocrlf=false -c core.eol=lf clone --depth 1 --filter=blob:none --sparse https://github.com/w3c/rdf-tests.git $dest
    git -C $dest config core.autocrlf false
    git -C $dest config core.eol lf
}
git -C $dest sparse-checkout set rdf sparql
Write-Host "done: $dest"
