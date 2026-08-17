<#
.SYNOPSIS
Reproduces issue #117 thumbnail-stream cold-cache measurements against an isolated scratch library.

.DESCRIPTION
The caller must prepare ScratchDir and its exact-content .naiad-perf-scratch sentinel as documented
in docs/perf/2026-07-21-issue-117-thumbnail-stream.md. The script rejects reparse points in every
existing ancestor chain it uses, requires OutputPath inside ScratchDir, and deletes only the direct
ScratchDir\thumbs.db (plus -wal/-shm siblings) between modes. It never accepts or modifies a
source-library path.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $ScratchDir,

    [Parameter(Mandatory)]
    [string] $DaemonExe,

    [Parameter(Mandatory)]
    [string] $DaemonCommit,

    [Parameter(Mandatory)]
    [string] $OutputPath,

    [ValidateSet('ClientPath', 'Raw', 'Both')]
    [string] $Mode = 'Both',

    [ValidateRange(1024, 65535)]
    [int] $Port = 18081,

    [ValidateRange(1, 124)]
    [int] $TransientLifetimeMs = 20,

    [ValidateRange(1, 60000)]
    [int] $ObservationMs = 2000,

    [switch] $PreflightOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http

function Assert-NoReparseChain {
    param([Parameter(Mandatory)][string] $Path)

    $current = [IO.Path]::GetFullPath($Path)
    while (-not (Test-Path -LiteralPath $current)) {
        $parent = [IO.Path]::GetDirectoryName($current.TrimEnd('\'))
        if ([string]::IsNullOrEmpty($parent) -or $parent -eq $current) {
            throw "No existing ancestor for path: $Path"
        }
        $current = $parent
    }
    while (-not [string]::IsNullOrEmpty($current)) {
        $item = Get-Item -Force -LiteralPath $current
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Reparse points are forbidden in measurement paths: $current"
        }
        $trimmed = $current.TrimEnd('\')
        $parent = [IO.Path]::GetDirectoryName($trimmed)
        if ([string]::IsNullOrEmpty($parent) -or $parent -eq $current) {
            break
        }
        $current = $parent
    }
}

Assert-NoReparseChain -Path $ScratchDir
$scratch = (Resolve-Path -LiteralPath $ScratchDir).Path.TrimEnd('\')
$scratchRoot = [IO.Path]::GetPathRoot($scratch).TrimEnd('\')
if ($scratch -eq $scratchRoot) {
    throw 'ScratchDir must not be a drive root.'
}
Assert-NoReparseChain -Path $scratch
Assert-NoReparseChain -Path $DaemonExe
$daemon = (Resolve-Path -LiteralPath $DaemonExe).Path
$database = Join-Path $scratch 'naiad.db'
$manifestPath = Join-Path $scratch 'media-manifest.json'
$thumbsDb = Join-Path $scratch 'thumbs.db'
$sentinel = Join-Path $scratch '.naiad-perf-scratch'
$sentinelContent = 'naiad issue-117 perf scratch v1'
$sentinelFileContent = $sentinelContent + "`r`n"
$output = [IO.Path]::GetFullPath($OutputPath)
if (-not (Test-Path -LiteralPath $database -PathType Leaf)) {
    throw "Missing scratch database: $database"
}
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "Missing scratch manifest: $manifestPath"
}

function Assert-InScratch {
    param(
        [Parameter(Mandatory)][string] $Path,
        [switch] $AllowMissingLeaf
    )

    $full = [IO.Path]::GetFullPath($Path)
    $prefix = $scratch + [IO.Path]::DirectorySeparatorChar
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Path escapes ScratchDir: $full"
    }
    Assert-NoReparseChain -Path $full
    if ($AllowMissingLeaf) {
        return $full
    }
    return (Resolve-Path -LiteralPath $full).Path
}

function Assert-Sentinel {
    Assert-NoReparseChain -Path $sentinel
    if (-not (Test-Path -LiteralPath $sentinel -PathType Leaf)) {
        throw "Missing required scratch sentinel: $sentinel"
    }
    $actual = Get-Content -Raw -LiteralPath $sentinel
    if ($actual -cne $sentinelFileContent) {
        throw "Scratch sentinel has wrong content; recreate it with the documented Set-Content command."
    }
}

Assert-Sentinel
Assert-InScratch -Path $database | Out-Null
Assert-InScratch -Path $manifestPath | Out-Null
$output = Assert-InScratch -Path $output -AllowMissingLeaf
if (Test-Path -LiteralPath $output) {
    throw "OutputPath already exists; refusing to overwrite: $output"
}
$outputParent = [IO.Path]::GetDirectoryName($output)
if ($outputParent -eq $scratch) {
    Assert-NoReparseChain -Path $outputParent
}
else {
    Assert-InScratch -Path $outputParent | Out-Null
}

$manifestJson = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
$manifest = @()
foreach ($entry in $manifestJson) {
    $manifest += $entry
}
if ($manifest.Count -ne 104) {
    throw "Expected exactly 104 manifest entries, got $($manifest.Count)."
}
foreach ($entry in $manifest) {
    if ($entry.hash -notmatch '^[0-9a-f]{64}$') {
        throw "Manifest contains a non-canonical hash: $($entry.hash)"
    }
    Assert-InScratch -Path $entry.scratch | Out-Null
}

$expectedThumbsDb = Assert-InScratch -Path (Join-Path $scratch 'thumbs.db') -AllowMissingLeaf
if ([IO.Path]::GetDirectoryName($expectedThumbsDb) -ne $scratch) {
    throw "Unexpected thumbnail-cache target: $expectedThumbsDb"
}

function Clear-ScratchThumbnails {
    # Revalidate the sentinel and scratch root immediately before any deletion.
    # This closes the gap between startup checks and use.
    Assert-Sentinel
    Assert-NoReparseChain -Path $scratch
    $candidate = Assert-InScratch -Path $thumbsDb -AllowMissingLeaf
    if ($candidate -ne $expectedThumbsDb -or [IO.Path]::GetDirectoryName($candidate) -ne $scratch) {
        throw "Refusing to delete unexpected thumbnail-cache path: $candidate"
    }
    Assert-Sentinel
    # Delete thumbs.db and its WAL sidecars so the next daemon run starts cold.
    foreach ($suffix in @('', '-wal', '-shm')) {
        $target = $thumbsDb + $suffix
        if (Test-Path -LiteralPath $target -PathType Leaf) {
            $resolved = (Resolve-Path -LiteralPath $target).Path
            Assert-NoReparseChain -Path $resolved
            Remove-Item -LiteralPath $resolved -Force
        }
    }
}

function Wait-DaemonReady {
    param([Parameter(Mandatory)][string] $BaseUrl)

    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    do {
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/api/roots" -TimeoutSec 2
            if ($response.StatusCode -eq 200) {
                return
            }
        }
        catch {
            Start-Sleep -Milliseconds 250
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Scratch daemon did not become ready at $BaseUrl."
}

function Start-ScratchDaemon {
    param([Parameter(Mandatory)][string] $RunName)

    $runId = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
    $stdout = Assert-InScratch -Path (Join-Path $scratch "$RunName-$runId.stdout.log") `
        -AllowMissingLeaf
    $stderr = Assert-InScratch -Path (Join-Path $scratch "$RunName-$runId.stderr.log") `
        -AllowMissingLeaf
    Assert-NoReparseChain -Path $daemon
    Assert-InScratch -Path $database | Out-Null
    Assert-Sentinel
    $oldRustLog = $env:RUST_LOG
    $oldNaiadDb = $env:NAIAD_DB
    try {
        $env:RUST_LOG = 'thumb=trace,naiad_daemon=info'
        $env:NAIAD_DB = $null
        $process = Start-Process -FilePath $daemon -ArgumentList @(
            'daemon', '--addr', "127.0.0.1:$Port", '--db', $database,
            '--no-watch', '--thumb-size', '360'
        ) -WorkingDirectory $scratch -RedirectStandardOutput $stdout `
            -RedirectStandardError $stderr -WindowStyle Hidden -PassThru
    }
    finally {
        $env:RUST_LOG = $oldRustLog
        $env:NAIAD_DB = $oldNaiadDb
    }
    return [PSCustomObject]@{ Process = $process; Stdout = $stdout; Stderr = $stderr }
}

function Send-Text {
    param(
        [Parameter(Mandatory)][System.Net.WebSockets.ClientWebSocket] $Socket,
        [Parameter(Mandatory)][string] $Text
    )

    $bytes = [Text.Encoding]::UTF8.GetBytes($Text)
    $segment = [ArraySegment[byte]]::new($bytes)
    $Socket.SendAsync(
        $segment,
        [System.Net.WebSockets.WebSocketMessageType]::Text,
        $true,
        [Threading.CancellationToken]::None
    ).GetAwaiter().GetResult() | Out-Null
}

function Receive-Message {
    param(
        [Parameter(Mandatory)][System.Net.WebSockets.ClientWebSocket] $Socket,
        [Parameter(Mandatory)][Threading.CancellationToken] $CancellationToken
    )

    $stream = [IO.MemoryStream]::new()
    try {
        do {
            $buffer = New-Object byte[] 1048576
            $segment = [ArraySegment[byte]]::new($buffer)
            $part = $Socket.ReceiveAsync($segment, $CancellationToken).GetAwaiter().GetResult()
            if ($part.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
                throw 'WebSocket closed before the visible set completed.'
            }
            $stream.Write($buffer, 0, $part.Count)
        } while (-not $part.EndOfMessage)
        return [PSCustomObject]@{ Type = $part.MessageType; Bytes = $stream.ToArray() }
    }
    finally {
        $stream.Dispose()
    }
}

function Get-CacheFiles {
    # Return FileInfo objects for thumbs.db and its WAL/SHM siblings that exist.
    # Callers use Length and LastWriteTimeUtc from these objects for stabilization.
    $files = @()
    foreach ($suffix in @('', '-wal', '-shm')) {
        $p = $thumbsDb + $suffix
        if (Test-Path -LiteralPath $p -PathType Leaf) {
            $files += Get-Item -LiteralPath $p
        }
    }
    return $files
}

function Wait-CacheStable {
    param([Parameter(Mandatory)][Diagnostics.Stopwatch] $RunClock)

    $lastSignature = $null
    $stableClock = [Diagnostics.Stopwatch]::StartNew()
    do {
        $files = @(Get-CacheFiles)
        $signature = (($files | Sort-Object FullName | ForEach-Object {
            "$($_.FullName)|$($_.Length)|$($_.LastWriteTimeUtc.Ticks)"
        }) -join ';')
        if ($signature -ne $lastSignature) {
            $lastSignature = $signature
            $stableClock.Restart()
        }
        if ($stableClock.ElapsedMilliseconds -ge $ObservationMs) {
            return $files
        }
        Start-Sleep -Milliseconds 100
    } while ($RunClock.Elapsed.TotalSeconds -lt 30)
    throw 'Thumbnail cache did not stabilize within 30 seconds.'
}

function Invoke-Measurement {
    param([Parameter(Mandatory)][ValidateSet('ClientPath', 'Raw')][string] $RunMode)

    Clear-ScratchThumbnails
    $baseUrl = "http://127.0.0.1:$Port"
    $streamUrl = "ws://127.0.0.1:$Port/thumb-stream"
    $daemonRun = Start-ScratchDaemon -RunName $RunMode.ToLowerInvariant()
    $socket = $null
    $http = $null
    $result = $null
    try {
        Wait-DaemonReady -BaseUrl $baseUrl
        $activeJson = (Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/api/search?q=" `
            -TimeoutSec 60).Content | ConvertFrom-Json
        $active = @()
        foreach ($file in $activeJson) {
            $active += $file
        }
        if ($active.Count -ne 104) {
            throw "Scratch daemon returned $($active.Count) active files; expected 104."
        }
        foreach ($file in $active) {
            Assert-InScratch -Path $file.path | Out-Null
        }

        $stale = @($manifest[0..99].hash)
        $visible = @($manifest[100..103].hash)
        $socket = [System.Net.WebSockets.ClientWebSocket]::new()
        $socket.ConnectAsync(
            [Uri]$streamUrl,
            [Threading.CancellationToken]::None
        ).GetAwaiter().GetResult() | Out-Null

        if ($RunMode -eq 'ClientPath') {
            # Model the actual transport boundary: every transient tile dies before its 125 ms
            # load-thumb timer, so none opens a subscription or sends a WebSocket command.
            foreach ($hash in $stale) {
                Start-Sleep -Milliseconds $TransientLifetimeMs
            }
            Start-Sleep -Milliseconds 125
            $recipe = "100 transient tile lifetimes at $TransientLifetimeMs ms (<125 ms); " +
                '0 obsolete wants; 4 visible wants after 125 ms; one WebSocket'
        }
        else {
            foreach ($hash in $stale) {
                Send-Text -Socket $socket -Text "want $hash"
                Start-Sleep -Milliseconds 2
            }
            foreach ($hash in $stale) {
                Send-Text -Socket $socket -Text "cancel $hash"
            }
            $recipe = '100 wants at 2 ms cadence; 100-cancel burst; ' +
                '4 visible wants immediately; one WebSocket'
        }

        $visibleStartUtc = [DateTime]::UtcNow
        $runClock = [Diagnostics.Stopwatch]::StartNew()
        foreach ($hash in $visible) {
            Send-Text -Socket $socket -Text "want $hash"
        }

        $http = [System.Net.Http.HttpClient]::new()
        $fileClock = [Diagnostics.Stopwatch]::StartNew()
        $fileResponse = $http.GetAsync(
            "$baseUrl/file/$($visible[0])",
            [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead
        ).GetAwaiter().GetResult()
        $fileLatencyMs = [Math]::Round($fileClock.Elapsed.TotalMilliseconds, 1)
        $fileStatus = [int]$fileResponse.StatusCode
        $fileLength = $fileResponse.Content.Headers.ContentLength
        $fileResponse.Dispose()

        $seen = @{}
        $receiveCts = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds(20))
        try {
            while ($seen.Count -lt 4) {
                $message = Receive-Message -Socket $socket `
                    -CancellationToken $receiveCts.Token
                if ($message.Type -ne [System.Net.WebSockets.WebSocketMessageType]::Binary) {
                    continue
                }
                $frame = $message.Bytes
                if ($frame.Length -lt 36) {
                    throw "Short thumbnail frame: $($frame.Length) bytes."
                }
                $hash = [BitConverter]::ToString($frame[0..31]).Replace('-', '').ToLowerInvariant()
                $declared = ([uint32]$frame[32] -shl 24) -bor
                    ([uint32]$frame[33] -shl 16) -bor
                    ([uint32]$frame[34] -shl 8) -bor [uint32]$frame[35]
                if ($declared -ne ($frame.Length - 36)) {
                    throw "Frame length mismatch for $hash."
                }
                if (($visible -contains $hash) -and -not $seen.ContainsKey($hash)) {
                    if ($declared -eq 0) {
                        throw "Visible thumbnail failed for $hash."
                    }
                    $seen[$hash] = [Math]::Round($runClock.Elapsed.TotalMilliseconds, 1)
                }
            }
        }
        finally {
            $receiveCts.Dispose()
        }

        $lastVisibleMs = ($seen.Values | Measure-Object -Maximum).Maximum
        $cacheFiles = @(Wait-CacheStable -RunClock $runClock)
        $maxWrite = ($cacheFiles | Measure-Object LastWriteTimeUtc -Maximum).Maximum
        $lastWriteMs = if ($null -eq $maxWrite) {
            $null
        }
        else {
            [Math]::Round(($maxWrite - $visibleStartUtc).TotalMilliseconds, 1)
        }
        $postVisibleTailMs = if ($null -eq $lastWriteMs) {
            $null
        }
        else {
            [Math]::Max(0, [Math]::Round($lastWriteMs - $lastVisibleMs, 1))
        }
        $result = [ordered]@{
            mode = $RunMode
            recipe = $recipe
            daemon_version = (& $daemon --version | Out-String).Trim()
            daemon_commit = $DaemonCommit
            thumbnail_websockets = 1
            visible_latency_ms = @($seen.Values | Sort-Object)
            visible_min_ms = ($seen.Values | Measure-Object -Minimum).Minimum
            visible_max_ms = $lastVisibleMs
            file_response_header_ms = $fileLatencyMs
            file_status = $fileStatus
            file_content_length = $fileLength
            cache_db_files = $cacheFiles.Count
            last_cache_write_from_visible_wants_ms = $lastWriteMs
            post_visible_generation_tail_ms = $postVisibleTailMs
            stable_observation_ms = $ObservationMs
            daemon_stderr = $daemonRun.Stderr
        }
    }
    finally {
        if ($null -ne $socket) {
            $socket.Dispose()
        }
        if ($null -ne $http) {
            $http.Dispose()
        }
        if (-not $daemonRun.Process.HasExited) {
            Stop-Process -Id $daemonRun.Process.Id
            Wait-Process -Id $daemonRun.Process.Id -ErrorAction SilentlyContinue
        }
    }

    if ($null -eq $result) {
        throw "$RunMode measurement did not produce a result."
    }
    $trace = Get-Content -LiteralPath $daemonRun.Stderr
    $result['generated_trace_events'] = @(
        $trace | Select-String 'thumbnail generated|thumbnail generation slow'
    ).Count
    $result['cancelled_trace_events'] = @($trace | Select-String 'job cancelled').Count
    $result['skipped_trace_events'] = @($trace | Select-String 'skipped before decode').Count
    $result['stale_suppressed_trace_events'] = @(
        $trace | Select-String 'stale thumbnail stream completion suppressed'
    ).Count
    # Count "thumbnail generated" trace hits whose 12-char hash prefix matches a stale hash.
    # This measures whether cancelled/stale wants slipped through and actually generated a
    # thumbnail (evidence the suppression logic is working when this is 0).
    # The daemon logs: hash=<first 12 hex chars> on every "thumbnail generated" DEBUG line.
    $stalePrefixes = @($stale | ForEach-Object { $_.Substring(0, 12) })
    $generatedLines = @($trace | Select-String 'thumbnail generated|thumbnail generation slow')
    $result['obsolete_cache_files'] = @($generatedLines | Where-Object {
        $m = [System.Text.RegularExpressions.Regex]::Match(
            $_.ToString(), 'hash=([0-9a-f]{12})')
        $m.Success -and ($stalePrefixes -contains $m.Groups[1].Value)
    }).Count
    return [PSCustomObject]$result
}

$processor = Get-ItemProperty -LiteralPath `
    'HKLM:\HARDWARE\DESCRIPTION\System\CentralProcessor\0' -ErrorAction SilentlyContinue
$metadata = [PSCustomObject]@{
    measured_at_utc = [DateTime]::UtcNow.ToString('o')
    scratch_dir = $scratch
    cpu = if ($null -eq $processor) { $null } else { $processor.ProcessorNameString.Trim() }
    logical_processors = $env:NUMBER_OF_PROCESSORS
    architecture = $env:PROCESSOR_ARCHITECTURE
    os = [Environment]::OSVersion.VersionString
    transient_lifetime_ms = $TransientLifetimeMs
    debounce_ms = 125
}

if ($PreflightOnly) {
    [PSCustomObject]@{
        preflight = 'ok'
        scratch_dir = $scratch
        output_path = $output
        manifest_entries = $manifest.Count
        sentinel = $sentinel
    } | ConvertTo-Json -Depth 3
    exit 0
}

$modes = switch ($Mode) {
    'ClientPath' { @('ClientPath') }
    'Raw' { @('Raw') }
    default { @('ClientPath', 'Raw') }
}
$measurements = foreach ($runMode in $modes) {
    Invoke-Measurement -RunMode $runMode
}
$payload = [PSCustomObject]@{
    metadata = $metadata
    measurements = @($measurements)
}
$json = $payload | ConvertTo-Json -Depth 6

# Revalidate immediately before the only result write. Refuse replacement even if another process
# created the requested path during the run.
Assert-Sentinel
$validatedOutput = Assert-InScratch -Path $output -AllowMissingLeaf
if ($validatedOutput -ne $output -or (Test-Path -LiteralPath $output)) {
    throw "OutputPath became unsafe or already exists: $output"
}
[IO.File]::WriteAllText($output, $json, [Text.UTF8Encoding]::new($false))
Write-Output $json
