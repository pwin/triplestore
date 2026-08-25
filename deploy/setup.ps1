#Requires -Version 7.0
<#
.SYNOPSIS
    Check prerequisites and build HOLOS. Idempotent; safe to re-run.
#>
[CmdletBinding()]
param([switch]$SkipTests)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$root = $PWD.Path
$failed = $false

function Say  { param($m) Write-Host $m -ForegroundColor White }
function Ok   { param($m) Write-Host "  ok    $m" -ForegroundColor Green }
function Bad  { param($m) Write-Host "  miss  $m" -ForegroundColor Red; $script:failed = $true }
function Note { param($m) Write-Host "        $m" -ForegroundColor DarkGray }

Say 'Prerequisites'

if (Get-Command cargo -ErrorAction SilentlyContinue) {
    $v = (rustc --version) -replace '^rustc (\S+).*$', '$1'
    Ok "rust $v"
    if ([version]($v -replace '-.*$','') -lt [version]'1.87') {
        Bad "rust 1.87 or newer is required (found $v)"
        Note 'rustup update stable'
    }
} else {
    Bad 'rust — install from https://rustup.rs'
}

# RocksDB builds C++ and generates bindings with libclang. On Windows this is the single
# most common cause of a failed first build, and the error it produces is unhelpful.
$clang = Get-Command clang -ErrorAction SilentlyContinue
if ($clang -or $env:LIBCLANG_PATH) {
    Ok 'clang / libclang (needed by the rocksdb bindings)'
} else {
    Bad 'clang — needed to build the rocksdb feature'
    Note 'winget install LLVM.LLVM'
    Note 'then:  $env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"'
    Note 'or build without persistence:  cargo build --release --no-default-features'
}

# The MSVC toolchain needs the C++ build tools for the same reason.
if (Get-Command link.exe -ErrorAction SilentlyContinue) {
    Ok 'msvc linker'
} else {
    Note 'link.exe not on PATH — if the build fails, install "Desktop development with C++"'
    Note 'from the Visual Studio Build Tools installer.'
}

if ($failed) { Write-Host "`nInstall what is missing above, then re-run." -ForegroundColor Yellow; exit 1 }

Write-Host ''
Say 'Building (release)'
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

if (-not $SkipTests) {
    Write-Host ''
    Say 'Verifying'
    cargo test --workspace --quiet 2>&1 | Select-Object -Last 20
}

Write-Host ''
Say 'Built'
Note "$root\target\release\holos.exe          command line"
Note "$root\target\release\holos-server.exe   http service"
Write-Host ''
Note 'Next:  deploy\load.ps1 <file.ttl>    then   deploy\run.ps1'
