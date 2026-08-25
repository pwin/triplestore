#Requires -Version 7.0
<#
.SYNOPSIS
    Load RDF files into the persistent store.
.EXAMPLE
    deploy\load.ps1 data\people.ttl data\orgs.ttl
.NOTES
    Stop the server first: one process at a time may hold the store directory.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Files,
    [string]$Store
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
. (Join-Path $PSScriptRoot 'env.ps1')

$cfg   = Read-HolosEnv
$store = if ($Store) { $Store } elseif ($cfg.HOLOS_STORE) { $cfg.HOLOS_STORE } else { '.\var\store' }
$bin   = '.\target\release\holos.exe'

if (-not (Test-Path $bin)) { throw "$bin not built — run deploy\setup.ps1" }

New-Item -ItemType Directory -Force -Path $store | Out-Null

$args = @()
foreach ($f in $Files) {
    if (-not (Test-Path $f)) { throw "no such file: $f" }
    $args += @('--data', $f)
}

Write-Host "loading $($Files.Count) file(s) into $store"
# --bulk buffers writes and skips the write-ahead log: roughly 2.4x faster, at the cost of
# a part-way-interrupted load having to be discarded rather than resumed.
$sw = [Diagnostics.Stopwatch]::StartNew()
& $bin stats @args --store $store --bulk
$sw.Stop()
if ($LASTEXITCODE -ne 0) { throw 'load failed' }

Write-Host ("`nloaded in {0:n1}s. start the service with:  deploy\run.ps1" -f $sw.Elapsed.TotalSeconds)
