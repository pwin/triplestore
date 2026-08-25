#Requires -Version 7.0
#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Install holos-server as a Windows service.
.DESCRIPTION
    Windows has no direct equivalent of an ExecStart script, so this registers the binary
    itself with its flags baked into the service's command line. Re-run it after changing
    deploy\holos.env — the flags are resolved at install time, not at start time.
.EXAMPLE
    deploy\install-service.ps1 -Store C:\ProgramData\holos\store
.EXAMPLE
    deploy\install-service.ps1 -Remove
#>
[CmdletBinding()]
param(
    [string]$Name = 'holos',
    [string]$Store,
    [string]$Listen,
    [switch]$Remove
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
. (Join-Path $PSScriptRoot 'env.ps1')

if ($Remove) {
    if (Get-Service $Name -ErrorAction SilentlyContinue) {
        Stop-Service $Name -Force -ErrorAction SilentlyContinue
        # sc.exe, not Remove-Service, so this works on Windows PowerShell hosts too.
        & sc.exe delete $Name | Out-Null
        Write-Host "removed service '$Name'"
    } else {
        Write-Host "no service named '$Name'"
    }
    return
}

$cfg = Read-HolosEnv
function Cfg { param($k, $d = '') if ($cfg[$k]) { $cfg[$k] } else { $d } }
function Words { param($v) if ($v) { $v -split '\s+' | Where-Object { $_ } } else { @() } }

$exe = (Resolve-Path '.\target\release\holos-server.exe').Path
$storeDir = if ($Store) { $Store } elseif (Cfg HOLOS_STORE) { (Cfg HOLOS_STORE) } else { 'C:\ProgramData\holos\store' }
$storeDir = [IO.Path]::GetFullPath($storeDir)
New-Item -ItemType Directory -Force -Path $storeDir | Out-Null

$listenAddr = if ($Listen) { $Listen } else { Cfg HOLOS_LISTEN '127.0.0.1:7878' }

$a = @("--listen `"$listenAddr`"", "--threads $(Cfg HOLOS_THREADS '8')", "--store `"$storeDir`"")
if ((Cfg HOLOS_UI 'on') -eq 'off')               { $a += '--no-ui' }
if ((Cfg HOLOS_TRUST_FORWARDED 'off') -eq 'on')  { $a += '--trust-forwarded-identity' }
if ((Cfg HOLOS_DENY_ALL 'off') -eq 'on')         { $a += '--deny-all' }
if ((Cfg HOLOS_FAIL_CLOSED 'off') -eq 'on')      { $a += '--fail-closed' }
foreach ($g in Words (Cfg HOLOS_ALLOW_GRAPHS))    { $a += "--allow-graph `"$g`"" }
foreach ($p in Words (Cfg HOLOS_DENY_PREDICATES)) { $a += "--deny-predicate `"$p`"" }
foreach ($l in Words (Cfg HOLOS_LABEL_GRAPHS))    { $a += "--label-graph `"$l`"" }

$binPath = "`"$exe`" " + ($a -join ' ')

if (Get-Service $Name -ErrorAction SilentlyContinue) {
    Write-Host "service '$Name' exists; stopping and reconfiguring"
    Stop-Service $Name -Force -ErrorAction SilentlyContinue
    & sc.exe config $Name binPath= $binPath start= auto | Out-Null
} else {
    & sc.exe create $Name binPath= $binPath start= auto DisplayName= 'HOLOS triplestore' | Out-Null
}
& sc.exe description $Name 'RDF 1.2 triplestore with SPARQL 1.2 and policy enforced at the scan' | Out-Null

# Restart twice on failure, then give up rather than crash-loop forever.
& sc.exe failure $Name reset= 86400 actions= restart/5000/restart/15000// | Out-Null

Start-Service $Name
Write-Host "service '$Name' installed and started"
Write-Host "  listening  http://$listenAddr"
Write-Host "  store      $storeDir"
Write-Host "  flags      $($a -join ' ')"
Write-Host ''
Write-Host "verify:  deploy\smoke.ps1 -Base http://$listenAddr"
Write-Host "logs:    Get-EventLog -LogName Application -Source $Name -Newest 20"
Write-Host ''
Write-Warning 'holos-server writes diagnostics to stderr, which a bare Windows service discards.'
Write-Warning 'For real log capture, run it under NSSM or a scheduled task with redirection.'
