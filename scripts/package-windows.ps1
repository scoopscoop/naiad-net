<#
.SYNOPSIS
    Build and package the Naiad Windows portable .zip release.

.DESCRIPTION
    Produces dist/Naiad-<version>-windows-x64-portable.zip: a self-contained,
    no-installer, no-registry build of the Tauri desktop shell plus its daemon
    sidecar. Run from anywhere; paths are resolved relative to the repo root.

    Build order (each step fails fast):
      1. Assert the four version fields agree (shared scripts/version-gate.ps1;
         single source of truth = workspace Cargo.toml [workspace.package].version).
         Abort on any drift.
      2. npm --prefix ui run build      -> ui/dist (the daemon serves this)
      3. cargo build --release          -> target/release/naiad.exe (the daemon)
      4. npm --prefix ui run tauri build-> ui/src-tauri/target/release/naiad-desktop.exe (the shell)
      5. Hand-assemble the payload: naiad-desktop.exe + naiad.exe in one folder.
         (bundle.active is false, so tauri build emits a bare exe into the cargo
         target dir, not a distributable folder -- we assemble it ourselves.)
      6. Compress-Archive -> dist/Naiad-<version>-windows-x64-portable.zip

    WebView2 is the system-provided evergreen runtime (standard on Win10/11);
    it is a documented prerequisite, not embedded.

.NOTES
    Repo PowerShell conventions: $ErrorActionPreference = 'Stop'; no 2>&1 on
    native exes (exit codes are checked via $LASTEXITCODE instead).
#>

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$UiDir    = Join-Path $RepoRoot 'ui'

# --- Step 1: version agreement (shared gate; single source of truth) --------

. (Join-Path $PSScriptRoot 'version-gate.ps1')
$version = Assert-NaiadVersionAgreement -RepoRoot $RepoRoot
Write-Host "Packaging Naiad v$version (all four version fields agree)" -ForegroundColor Green

# --- Steps 2-4: build (fail fast on each) -----------------------------------

Write-Host '==> npm --prefix ui run build' -ForegroundColor Cyan
& npm --prefix $UiDir run build
if ($LASTEXITCODE -ne 0) { throw "ui build failed (exit $LASTEXITCODE)" }

Write-Host '==> cargo build --release' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# tauri-build asserts the sidecar exists at ui/src-tauri/binaries/<name>-<triple>.exe
# at build time. The directory is gitignored, so populate it from the fresh daemon
# build every run (a stale or absent copy breaks fresh checkouts, e.g. CI runners).
$sidecarDir = Join-Path $UiDir 'src-tauri/binaries'
New-Item -ItemType Directory -Force -Path $sidecarDir | Out-Null
Copy-Item -LiteralPath (Join-Path $RepoRoot 'target/release/naiad.exe') `
    -Destination (Join-Path $sidecarDir 'naiad-x86_64-pc-windows-msvc.exe') -Force

Write-Host '==> npm --prefix ui run tauri build' -ForegroundColor Cyan
& npm --prefix $UiDir run tauri build
if ($LASTEXITCODE -ne 0) { throw "tauri build failed (exit $LASTEXITCODE)" }

# --- Step 5: hand-assemble the portable payload -----------------------------

$shellExe  = Join-Path $UiDir 'src-tauri/target/release/naiad-desktop.exe'
$daemonExe = Join-Path $RepoRoot 'target/release/naiad.exe'
foreach ($exe in @($shellExe, $daemonExe)) {
    if (-not (Test-Path -LiteralPath $exe)) { throw "Expected build output missing: $exe" }
}

$folderName = "Naiad-$version-windows-x64"
$stageRoot  = Join-Path $RepoRoot 'dist/staging'
$stageDir   = Join-Path $stageRoot $folderName
if (Test-Path -LiteralPath $stageDir) { Remove-Item -LiteralPath $stageDir -Recurse -Force }
New-Item -ItemType Directory -Path $stageDir -Force | Out-Null

# Both exes must sit in the same folder: Tauri resolves the sidecar by base
# name ("naiad.exe") next to the shell executable at runtime.
Copy-Item -LiteralPath $shellExe  -Destination (Join-Path $stageDir 'naiad-desktop.exe')
Copy-Item -LiteralPath $daemonExe -Destination (Join-Path $stageDir 'naiad.exe')

# --- Step 6: zip ------------------------------------------------------------

$zipPath = Join-Path $RepoRoot "dist/Naiad-$version-windows-x64-portable.zip"
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path $stageDir -DestinationPath $zipPath -Force

$sizeMB = [math]::Round((Get-Item -LiteralPath $zipPath).Length / 1MB, 1)
Write-Host ''
Write-Host "Created $zipPath ($sizeMB MB)" -ForegroundColor Green
Write-Host "  contents: $folderName/{naiad-desktop.exe, naiad.exe}"
