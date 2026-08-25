#Requires -Version 7.0
<#
.SYNOPSIS
    Verify a running instance. Exits non-zero on the first failure, so it works as a
    deployment gate.
.EXAMPLE
    deploy\smoke.ps1
    deploy\smoke.ps1 -Base http://host:7878
#>
[CmdletBinding()]
param([string]$Base = 'http://127.0.0.1:7878')

$pass = 0; $fail = 0

function Check {
    param([string]$What, [scriptblock]$Get, [string]$Want)
    try { $got = (& $Get) -join "`n" } catch { $got = "$_" }
    if ($got -like "*$Want*") {
        Write-Host "  pass  $What" -ForegroundColor Green; $script:pass++
    } else {
        Write-Host "  FAIL  $What" -ForegroundColor Red
        Write-Host "        wanted to find: $Want" -ForegroundColor DarkGray
        Write-Host "        got:            $($got.Substring(0, [Math]::Min(200, $got.Length)))" -ForegroundColor DarkGray
        $script:fail++
    }
}

function Q {
    param([string]$Sparql, [string]$Accept = 'application/sparql-results+json')
    (Invoke-WebRequest -Uri "$Base/query" -Method Post -UseBasicParsing -TimeoutSec 30 `
        -Headers @{ Accept = $Accept } `
        -Body @{ query = $Sparql }).Content
}

Write-Host "smoke testing $Base"

Check 'GET /health' { (Invoke-WebRequest "$Base/health" -UseBasicParsing -TimeoutSec 10).Content } 'ok'
Check 'GET /stats returns JSON' { (Invoke-WebRequest "$Base/stats" -UseBasicParsing -TimeoutSec 10).Content } '"quads"'
Check 'GET /query answers ASK' { Q 'ASK { ?s ?p ?o }' } '"boolean"'
Check 'POST /query, form-encoded' { Q 'SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }' 'text/csv' } 'n'
Check 'CONSTRUCT negotiates turtle' { Q 'CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o } LIMIT 1' 'text/turtle'; 'ok' } 'ok'

Check 'a syntax error is a 400, not a 500' {
    try { Invoke-WebRequest "$Base/query?query=SELECT%20nonsense" -UseBasicParsing -TimeoutSec 10 | Out-Null; '200' }
    catch { [int]$_.Exception.Response.StatusCode }
} '400'

Check 'POST /update answers 501' {
    try { Invoke-WebRequest "$Base/update" -Method Post -UseBasicParsing -TimeoutSec 10 | Out-Null; '200' }
    catch { [int]$_.Exception.Response.StatusCode }
} '501'

Write-Host ''
if ($fail -eq 0) { Write-Host "$pass passed" -ForegroundColor Green; exit 0 }
else { Write-Host "$pass passed, $fail failed" -ForegroundColor Red; exit 1 }
