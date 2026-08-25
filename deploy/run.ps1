#Requires -Version 7.0
<#
.SYNOPSIS
    Start holos-server with the settings in deploy\holos.env.
.DESCRIPTION
    The server takes flags, not environment variables. This script is the translation
    layer, so the binary keeps one inspectable surface (holos-server --help) and
    deployments keep one file to edit.
#>
[CmdletBinding()]
param([string]$Listen, [string]$Store, [switch]$NoUi)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
. (Join-Path $PSScriptRoot 'env.ps1')
$cfg = Read-HolosEnv

$bin = '.\target\release\holos-server.exe'
if (-not (Test-Path $bin)) { throw "$bin not built — run deploy\setup.ps1" }

function Cfg { param($k, $d = '') if ($cfg[$k]) { $cfg[$k] } else { $d } }
function Words { param($v) if ($v) { $v -split '\s+' | Where-Object { $_ } } else { @() } }

$listenAddr = if ($Listen) { $Listen } else { Cfg HOLOS_LISTEN '127.0.0.1:7878' }
$storeDir   = if ($Store)  { $Store }  else { Cfg HOLOS_STORE }

$a = @('--listen', $listenAddr, '--threads', (Cfg HOLOS_THREADS '8'))

if ($storeDir) {
    New-Item -ItemType Directory -Force -Path $storeDir | Out-Null
    $a += @('--store', $storeDir)
}
if ($NoUi -or (Cfg HOLOS_UI 'on') -eq 'off') { $a += '--no-ui' }

foreach ($f in Words (Cfg HOLOS_DATA)) { $a += @('--data', $f) }

# Identity. The server refuses to read forwarded headers unless asked, so this flag is the
# whole difference between "every request is anonymous" and "the front door decides".
if ((Cfg HOLOS_TRUST_FORWARDED 'off') -eq 'on') {
    $a += '--trust-forwarded-identity'
    if ($listenAddr -notmatch '^(127\.0\.0\.1|localhost|\[::1\]):') {
        Write-Warning "--trust-forwarded-identity with a non-loopback bind address."
        Write-Warning "Any client that can reach $listenAddr can now name its own roles."
        Write-Warning "Bind to loopback and put a front door in front. See OPERATIONS.md."
    }
}

foreach ($r in Words (Cfg HOLOS_DEV_ROLES)) {
    Write-Warning "HOLOS_DEV_ROLES grants '$r' to every request, authenticated or not."
    $a += @('--role', $r)
}
if (Cfg HOLOS_DEV_CLEARANCE) {
    Write-Warning "HOLOS_DEV_CLEARANCE grants clearance $(Cfg HOLOS_DEV_CLEARANCE) to every request."
    $a += @('--clearance', (Cfg HOLOS_DEV_CLEARANCE))
}

# Policy.
if ((Cfg HOLOS_DENY_ALL 'off')    -eq 'on') { $a += '--deny-all' }
if ((Cfg HOLOS_FAIL_CLOSED 'off') -eq 'on') { $a += '--fail-closed' }
foreach ($g in Words (Cfg HOLOS_ALLOW_GRAPHS))    { $a += @('--allow-graph', $g) }
foreach ($p in Words (Cfg HOLOS_DENY_PREDICATES)) { $a += @('--deny-predicate', $p) }
foreach ($l in Words (Cfg HOLOS_LABEL_GRAPHS))    { $a += @('--label-graph', $l) }

& $bin @a
