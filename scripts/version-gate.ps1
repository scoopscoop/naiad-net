<#
.SYNOPSIS
    Shared four-manifest version gate. Dot-source, then call
    Assert-NaiadVersionAgreement -RepoRoot <path> to get the agreed version.

.DESCRIPTION
    Single source of truth = workspace Cargo.toml [workspace.package].version.
    The other three manifests (ui/src-tauri/Cargo.toml [package],
    ui/src-tauri/tauri.conf.json, ui/package.json) must agree; any drift
    aborts with a table of the four values. Used by package-windows.ps1 and
    package-server.ps1 so client and server releases can never disagree on
    what version they are.

.NOTES
    Callers must set $ErrorActionPreference = 'Stop' before dot-sourcing; the
    functions rely on Get-Content throwing on a missing manifest (otherwise a
    missing file surfaces as a misleading "Could not find version" throw).
#>

function Get-TomlVersion {
    param([string]$Path, [string]$Section)
    $inSection = $false
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        if ($line -match '^\s*\[(.+?)\]\s*$') {
            $inSection = ($Matches[1] -eq $Section)
            continue
        }
        if ($inSection -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "Could not find version in [$Section] of $Path"
}

function Get-JsonVersion {
    param([string]$Path)
    (Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json).version
}

function Assert-NaiadVersionAgreement {
    param([Parameter(Mandatory)][string]$RepoRoot)
    $ui = Join-Path $RepoRoot 'ui'
    $versions = [ordered]@{
        'Cargo.toml [workspace.package]'   = Get-TomlVersion (Join-Path $RepoRoot 'Cargo.toml') 'workspace.package'
        'ui/src-tauri/Cargo.toml [package]'= Get-TomlVersion (Join-Path $ui 'src-tauri/Cargo.toml') 'package'
        'ui/src-tauri/tauri.conf.json'     = Get-JsonVersion (Join-Path $ui 'src-tauri/tauri.conf.json')
        'ui/package.json'                  = Get-JsonVersion (Join-Path $ui 'package.json')
    }

    # @() guarantees an array: a single unique value would otherwise be a scalar
    # string, and $distinct[0] on a string returns its first *character*, not the version.
    $distinct = @($versions.Values | Sort-Object -Unique)
    if ($distinct.Count -ne 1) {
        Write-Host 'Version drift detected -- aborting before build:' -ForegroundColor Red
        foreach ($k in $versions.Keys) { Write-Host ("  {0,-34} {1}" -f $k, $versions[$k]) }
        throw 'All four version fields must agree (source of truth: Cargo.toml [workspace.package]).'
    }
    return $distinct[0]
}
