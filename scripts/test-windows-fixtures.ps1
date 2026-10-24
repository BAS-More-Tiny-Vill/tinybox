# Exercise junction recovery without changing the runner's drive-root /bin.
$ErrorActionPreference = "Stop"
$fixture = @{}
$root = [System.IO.Path]::GetPathRoot((Get-Location).Path)
$bin = Join-Path $root "bin"
$posixBin = Join-Path $root "fixture-posix"

# Mock only filesystem operations, so these cases also run on Unix PowerShell.
function Test-Path {
    param([string]$LiteralPath, [string]$Path)
    $value = if ($LiteralPath) { $LiteralPath } else { $Path }
    if ($value -eq (Join-Path $posixBin "sh.exe")) { return $fixture.TargetValid }
    if ($value -eq (Join-Path $bin "sh.exe")) { return $fixture.Valid }
    if ($value -eq $bin) { return $fixture.Exists }
    throw "Unexpected fixture path: $value"
}
function Get-Item {
    [CmdletBinding()]
    param([string]$LiteralPath, [switch]$Force)
    if ($LiteralPath -ne $bin) { throw "Unexpected item path: $LiteralPath" }
    if ($fixture.Exists -or $fixture.LinkType) {
        return [pscustomobject]@{ LinkType = $fixture.LinkType }
    }
    return $null
}
function New-Item {
    param([string]$ItemType, [string]$Path, [string]$Target)
    if ($ItemType -ne "Junction" -or $Path -ne $bin -or $Target -ne $posixBin) {
        throw "Unexpected junction creation"
    }
    if ($fixture.Exists -or $fixture.LinkType) { throw "Old item was not removed" }
    $fixture.Created = $true
    $fixture.Valid = $true
    $fixture.Exists = $true
    $fixture.LinkType = "Junction"
}
function Remove-Item {
    param([string]$LiteralPath, [switch]$Force)
    if ($LiteralPath -ne $bin -or $fixture.LinkType -ne "Junction") {
        throw "Must only remove the junction, never a real directory"
    }
    $fixture.Removed = $true
    $fixture.Exists = $false
    $fixture.LinkType = $null
}

$cases = @(
    @{ Name = "preserves a valid existing shell"; Exists = $true; Valid = $true; LinkType = $null; TargetValid = $true; Removed = $false; Created = $false; Fails = $false },
    @{ Name = "creates a missing junction"; Exists = $false; Valid = $false; LinkType = $null; TargetValid = $true; Removed = $false; Created = $true; Fails = $false },
    @{ Name = "replaces a junction without a shell"; Exists = $true; Valid = $false; LinkType = "Junction"; TargetValid = $true; Removed = $true; Created = $true; Fails = $false },
    @{ Name = "replaces a dangling junction"; Exists = $false; Valid = $false; LinkType = "Junction"; TargetValid = $true; Removed = $true; Created = $true; Fails = $false },
    @{ Name = "preserves a real directory without a shell"; Exists = $true; Valid = $false; LinkType = $null; TargetValid = $true; Removed = $false; Created = $false; Fails = $true },
    @{ Name = "validates the target before removing a junction"; Exists = $true; Valid = $false; LinkType = "Junction"; TargetValid = $false; Removed = $false; Created = $false; Fails = $true }
)
foreach ($case in $cases) {
    $fixture = @{ Exists = $case.Exists; Valid = $case.Valid; LinkType = $case.LinkType; TargetValid = $case.TargetValid; Removed = $false; Created = $false }
    $failed = $false
    try { & "$PSScriptRoot/prepare-windows-fixtures.ps1" -PosixBin $posixBin }
    catch { $failed = $true }
    if ($failed -ne $case.Fails -or $fixture.Removed -ne $case.Removed -or $fixture.Created -ne $case.Created) {
        throw "Fixture case failed: $($case.Name) (failed=$failed, removed=$($fixture.Removed), created=$($fixture.Created))"
    }
    Write-Output "Passed: $($case.Name)"
}
