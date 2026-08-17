# Assert every version site in the repo agrees with the workspace version.
#
# Four files carry the version, and they have drifted apart before (#155): a
# release shipped with the workspace at 0.2.55, tauri.conf.json at 0.2.54 and
# ui/src-tauri/Cargo.toml at 0.2.52. That mattered because the UI's version
# badge reads tauri.conf.json via app.getVersion(), so the app displayed a
# version it was not.
#
# Exits 1 and names every mismatch, so `just check-versions` (and `just test`)
# fail loudly instead of letting a partial bump through.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Get-FileVersion {
    param([string]$RelPath, [string]$Pattern)

    $full = Join-Path $root $RelPath
    if (-not (Test-Path $full)) { throw "version site missing: $RelPath" }
    $m = Select-String -Path $full -Pattern $Pattern | Select-Object -First 1
    if (-not $m) { throw "no version found in ${RelPath} (pattern: $Pattern)" }
    return $m.Matches[0].Groups[1].Value
}

# The workspace manifest is the source of truth; [workspace.package] version is
# the first `version = "x"` after the [workspace.package] header, so anchor on
# the line start to avoid matching a dependency's version.
$sites = [ordered]@{
    'Cargo.toml'                  = '^version\s*=\s*"([^"]+)"'
    'ui/package.json'             = '"version"\s*:\s*"([^"]+)"'
    'ui/src-tauri/Cargo.toml'     = '^version\s*=\s*"([^"]+)"'
    'ui/src-tauri/tauri.conf.json' = '"version"\s*:\s*"([^"]+)"'
}

$found = [ordered]@{}
foreach ($path in $sites.Keys) {
    $found[$path] = Get-FileVersion -RelPath $path -Pattern $sites[$path]
}

$expected = $found['Cargo.toml']
$bad = @()
foreach ($path in $found.Keys) {
    if ($found[$path] -ne $expected) {
        $bad += "  $path = $($found[$path])"
    }
}

if ($bad.Count -gt 0) {
    Write-Host "version mismatch (workspace Cargo.toml = $expected):" -ForegroundColor Red
    $bad | ForEach-Object { Write-Host $_ -ForegroundColor Red }
    Write-Host ''
    # ASCII only: PowerShell's default output encoding mangles an em-dash here.
    Write-Host 'Bump every site together - the UI version badge reads tauri.conf.json.'
    exit 1
}

Write-Host "all version sites agree: $expected" -ForegroundColor Green
