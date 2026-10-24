# Provide the POSIX shell required by the detached-process fixtures.
param([Parameter(Mandatory = $true)][string]$PosixBin)
$ErrorActionPreference = "Stop"
$root = [System.IO.Path]::GetPathRoot((Get-Location).Path)
$bin = Join-Path $root "bin"
if (Test-Path -LiteralPath (Join-Path $bin "sh.exe")) {
    return
}
if (-not (Test-Path -LiteralPath (Join-Path $PosixBin "sh.exe"))) {
    throw "The POSIX shell target is unavailable at $PosixBin"
}
# Get-Item also finds dangling junctions, which Test-Path may treat as absent.
$existing = Get-Item -LiteralPath $bin -Force -ErrorAction SilentlyContinue
if ($null -ne $existing) {
    if ($existing.LinkType -ne "Junction") {
        throw "Cannot replace an existing non-junction at $bin; provide $bin/sh.exe"
    }
    # Remove only the junction itself, without traversing or deleting its target.
    Remove-Item -LiteralPath $bin -Force
}
New-Item -ItemType Junction -Path $bin -Target $PosixBin | Out-Null
if (-not (Test-Path -LiteralPath (Join-Path $bin "sh.exe"))) {
    throw "The native /bin/sh fixture is unavailable at $bin"
}
