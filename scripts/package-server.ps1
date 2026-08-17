<#
.SYNOPSIS
    Build and package the naiad-repo (server) Windows portable .zip release.

.DESCRIPTION
    Produces dist/Naiad-repo-<version>-windows-x64-portable.zip: the repository
    node binary plus a commented sample config and the operator guide. Run from
    anywhere; paths are resolved relative to the repo root.

    Build order (each step fails fast):
      1. Assert the four version fields agree (shared scripts/version-gate.ps1;
         single source of truth = workspace Cargo.toml [workspace.package].version).
      2. cargo build --release -p naiad-server -> target/release/naiad-repo.exe
      3. Stage naiad-repo.exe + repo.toml (sample) + README.md (copy of
         docs/operating-a-repo.md) into dist/staging/.
      4. Compress-Archive -> dist/Naiad-repo-<version>-windows-x64-portable.zip

    Distinct "Naiad-repo-" naming keeps server zips from colliding with the
    client's "Naiad-" zips in dist/.

.NOTES
    Repo PowerShell conventions: $ErrorActionPreference = 'Stop'; no 2>&1 on
    native exes (exit codes are checked via $LASTEXITCODE instead).
#>

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot

# --- Step 1: version agreement (shared gate; single source of truth) --------

. (Join-Path $PSScriptRoot 'version-gate.ps1')
$version = Assert-NaiadVersionAgreement -RepoRoot $RepoRoot
Write-Host "Packaging Naiad-repo v$version (all four version fields agree)" -ForegroundColor Green

# --- Step 2: build (fail fast) ----------------------------------------------

Write-Host '==> cargo build --release -p naiad-server' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --release -p naiad-server
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release -p naiad-server failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# --- Step 3: stage the portable payload -------------------------------------

$serverExe = Join-Path $RepoRoot 'target/release/naiad-repo.exe'
$sample    = Join-Path $RepoRoot 'scripts/repo.toml.sample'
$guide     = Join-Path $RepoRoot 'docs/operating-a-repo.md'
foreach ($f in @($serverExe, $sample, $guide)) {
    if (-not (Test-Path -LiteralPath $f)) { throw "Expected staging input missing: $f" }
}

$folderName = "Naiad-repo-$version-windows-x64"
$stageRoot  = Join-Path $RepoRoot 'dist/staging'
$stageDir   = Join-Path $stageRoot $folderName
if (Test-Path -LiteralPath $stageDir) { Remove-Item -LiteralPath $stageDir -Recurse -Force }
New-Item -ItemType Directory -Path $stageDir -Force | Out-Null

Copy-Item -LiteralPath $serverExe -Destination (Join-Path $stageDir 'naiad-repo.exe')
Copy-Item -LiteralPath $sample    -Destination (Join-Path $stageDir 'repo.toml')
Copy-Item -LiteralPath $guide     -Destination (Join-Path $stageDir 'README.md')

# --- Step 4: zip ------------------------------------------------------------

$zipPath = Join-Path $RepoRoot "dist/Naiad-repo-$version-windows-x64-portable.zip"
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path $stageDir -DestinationPath $zipPath -Force

$sizeMB = [math]::Round((Get-Item -LiteralPath $zipPath).Length / 1MB, 1)
Write-Host ''
Write-Host "Created $zipPath ($sizeMB MB)" -ForegroundColor Green
Write-Host "  contents: $folderName/{naiad-repo.exe, repo.toml, README.md}"
