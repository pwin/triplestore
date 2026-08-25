# Shared by the PowerShell scripts: read deploy/holos.env (and holos.env.local, which wins)
# into a hashtable. Keeps the same single configuration file as the shell scripts.
function Read-HolosEnv {
    [CmdletBinding()]
    param([string]$Dir = $PSScriptRoot)

    $cfg = @{}
    foreach ($name in @('holos.env', 'holos.env.local')) {
        $path = Join-Path $Dir $name
        if (-not (Test-Path $path)) { continue }
        foreach ($line in Get-Content $path) {
            $t = $line.Trim()
            if ($t -eq '' -or $t.StartsWith('#')) { continue }
            $i = $t.IndexOf('=')
            if ($i -lt 1) { continue }
            $k = $t.Substring(0, $i).Trim()
            $v = $t.Substring($i + 1).Trim().Trim('"').Trim("'")
            $cfg[$k] = $v
        }
    }
    # The environment wins over the file, so a service manager or container can override
    # any single setting without editing anything on disk.
    foreach ($k in @($cfg.Keys)) {
        $fromEnv = [Environment]::GetEnvironmentVariable($k)
        if ($fromEnv) { $cfg[$k] = $fromEnv }
    }
    return $cfg
}
