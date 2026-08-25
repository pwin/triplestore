# Fetches the W3C RDF and SPARQL test suites used by `cargo test -p holos-conformance`.
# See fetch-testsuites.sh for why they are not vendored.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'testsuites\rdf-tests'

if (Test-Path (Join-Path $dest '.git')) {
    Write-Host "updating $dest"
    git -C $dest fetch --depth 1 origin
    git -C $dest reset --hard origin/HEAD
} else {
    New-Item -ItemType Directory -Force (Join-Path $root 'testsuites') | Out-Null
    git clone --depth 1 --filter=blob:none --sparse https://github.com/w3c/rdf-tests.git $dest
}
git -C $dest sparse-checkout set rdf sparql
Write-Host "done: $dest"
