# Update the installed webcraft.exe + webcraft-host.exe in place, then relaunch.
#
# Why this exists: the native-messaging manifests point at loose .exe files
# (dist\webcraft-host.exe here), and free-running browsers keep respawning the
# host from whatever is on disk. Copying over the live path fails on locks, and
# "wait until nothing is running" never wins because Chrome/Firefox relaunch
# the host on extension activity. So the swap is a tight kill->swap retry loop:
# kill every webcraft/host process, immediately swap both files, repeat while
# a browser respawn sneaks a process in first (holding the file lock).
#
# Usage:
#   .\scripts\update-windows.ps1
#       Update from target\release\ next to the repo (default source).
#   .\scripts\update-windows.ps1 -SourceDir <dir>
#       Update from a directory containing webcraft.exe + webcraft-host.exe.
#   .\scripts\update-windows.ps1 -InstallDir <dir>
#       Override the install dir (default: dist\ next to the repo).
#   .\scripts\update-windows.ps1 -NoLaunch
#       Install only; do not relaunch the GUI.
#   .\scripts\update-windows.ps1 -NoCleanup
#       Skip the stray-.exe sweep (default: remove duplicate/stale webcraft
#       exes in the install dir and under target\, e.g. "webcraft-host (2).exe",
#       *.new/*.older leftovers, target\debug copies). Locked files are
#       skipped and reported, never fatal.

param(
    [string]$SourceDir,
    [string]$InstallDir,
    [switch]$NoLaunch,
    [switch]$NoCleanup
)

$ErrorActionPreference = 'Stop'

$ExeNames = @('webcraft.exe', 'webcraft-host.exe')
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $SourceDir)  { $SourceDir  = Join-Path $RepoRoot 'target\release' }
if (-not $InstallDir) { $InstallDir = Join-Path $RepoRoot 'dist' }
$SourceDir  = (Resolve-Path $SourceDir).Path
$InstallDir = (Resolve-Path $InstallDir).Path

function Get-WebcraftProcesses {
    Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -in $ExeNames }
}

function Stop-WebcraftOnce {
    # Graceful close for the GUI (may hold unsaved task state) on the first
    # pass only; hosts are stateless and are force-killed every pass.
    foreach ($p in @(Get-WebcraftProcesses)) {
        if ($p.Name -eq 'webcraft.exe') {
            try { $null = Invoke-CimMethod -InputObject $p -MethodName CloseMainWindow -ErrorAction SilentlyContinue } catch {}
        }
    }
    Start-Sleep -Milliseconds 700
    foreach ($p in @(Get-WebcraftProcesses)) {
        try { Stop-Process -Id $p.ProcessId -Force -ErrorAction Stop } catch {}
    }
}

# Stray webcraft exes: browser "save as" duplicates ("webcraft-host (2).exe"),
# staged/rollback leftovers (*.exe.new, *.exe.older), and build-output copies
# under target\ (debug/deps). The canonical binaries (install dir + the
# -SourceDir pair) are never touched. Locked files (a respawned host can hold
# a *.older open) are skipped and reported; cleanup is never fatal.
function Remove-StrayExes {
    param([string[]]$Keep)
    $keepSet = @{}
    foreach ($k in $Keep) { if ($k) { $keepSet[(Resolve-Path $k -ErrorAction SilentlyContinue).Path] = $true } }
    $candidates = @()
    # Install dir: anything webcraft*.exe* that is not a canonical binary.
    $candidates += Get-ChildItem $InstallDir -Filter 'webcraft*.exe*' -File -ErrorAction SilentlyContinue
    # Repo build output: every webcraft exe under target\ except the source pair.
    $targetDir = Join-Path $RepoRoot 'target'
    if (Test-Path $targetDir) {
        $candidates += Get-ChildItem $targetDir -Recurse -Filter 'webcraft*.exe' -File -ErrorAction SilentlyContinue
    }
    $removed = 0; $skipped = 0
    foreach ($f in @($candidates | Sort-Object FullName -Unique)) {
        if ($keepSet.ContainsKey($f.FullName)) { continue }
        try {
            Remove-Item $f.FullName -Force -ErrorAction Stop
            Write-Host "cleaned stray: $($f.FullName)"
            $removed++
        } catch {
            Write-Host "cleanup skipped (locked?): $($f.FullName)"
            $skipped++
        }
    }
    Write-Host "cleanup: $removed removed, $skipped skipped"
}

# ---------------------------------------------------------------------- main
$srcGui = Join-Path $SourceDir 'webcraft.exe'
foreach ($name in $ExeNames) {
    if (-not (Test-Path (Join-Path $SourceDir $name))) {
        throw "source binary missing: $(Join-Path $SourceDir $name). Build first (cargo build --release)."
    }
}
Write-Host "source:  $SourceDir"
Write-Host "install: $InstallDir"

# Stage beside the live files (never locked by a running browser-launched host).
foreach ($name in $ExeNames) {
    Copy-Item (Join-Path $SourceDir $name) (Join-Path $InstallDir "$name.new") -Force
}

# Kill -> swap retry loop (up to 30s). Each attempt: kill, small settle, swap.
# A file lock from a respawned host fails one Move-Item: kill again and retry.
$deadline = (Get-Date).AddSeconds(30)
$swapped = $false
$attempt = 0
while (-not $swapped -and (Get-Date) -lt $deadline) {
    $attempt++
    Stop-WebcraftOnce
    Start-Sleep -Milliseconds 300
    try {
        foreach ($name in $ExeNames) {
            $dst = Join-Path $InstallDir $name
            # Back up current file for rollback; -Force overwrites a stale .older.
            if (Test-Path $dst) { Copy-Item $dst "$dst.older" -Force }
        }
        foreach ($name in $ExeNames) {
            Move-Item -Force (Join-Path $InstallDir "$name.new") (Join-Path $InstallDir $name)
        }
        $swapped = $true
    } catch {
        Write-Host "swap attempt $attempt blocked ($($_.Exception.Message)); retrying..."
        Start-Sleep -Milliseconds 300
    }
}

if (-not $swapped) {
    # Roll back from .older backups if the GUI was replaced but the host not.
    foreach ($name in $ExeNames) {
        $older = Join-Path $InstallDir "$name.older"
        if (Test-Path $older) {
            try { Copy-Item $older (Join-Path $InstallDir $name) -Force -ErrorAction Stop } catch {}
        }
    }
    throw 'could not swap binaries after 30s (browser keeps respawning the host). Close Chrome/Firefox and re-run.'
}

# Cleanup: .older removal is non-fatal (a freshly respawned host may hold it).
foreach ($name in $ExeNames) {
    Remove-Item (Join-Path $InstallDir "$name.older") -Force -ErrorAction SilentlyContinue
}

# Verify by hash, not by timestamps or version strings (the GUI is a Windows
# subsystem binary: --version prints nothing to a console, and the version
# string is not a reliable binary scan).
foreach ($name in $ExeNames) {
    $a = (Get-FileHash (Join-Path $SourceDir $name)).Hash
    $b = (Get-FileHash (Join-Path $InstallDir $name)).Hash
    if ($a -ne $b) { throw "hash mismatch after swap: $name" }
    Write-Host "updated $name ($($b.Substring(0,12))...)"
}

& (Join-Path $InstallDir 'webcraft.exe') --register-hosts | Write-Host

# Sweep strays last: the swap above (and any browser respawn during it) is
# what creates them; canonical install + source binaries are kept.
if (-not $NoCleanup) {
    $keep = @($ExeNames | ForEach-Object { Join-Path $InstallDir $_ }) +
            @($ExeNames | ForEach-Object { Join-Path $SourceDir $_ })
    Remove-StrayExes -Keep $keep
}

if (-not $NoLaunch) {
    Start-Process -FilePath (Join-Path $InstallDir 'webcraft.exe') -ArgumentList '--show' `
        -WorkingDirectory $InstallDir | Out-Null
    Write-Host "launched $(Join-Path $InstallDir 'webcraft.exe')"
}
Write-Host 'done. If a browser already had the old host running, its next action uses the new binary.'
