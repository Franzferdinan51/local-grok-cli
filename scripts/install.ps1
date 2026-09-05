[CmdletBinding()]
param(
    [string]$BinaryPath = (Join-Path $PSScriptRoot '..\target\release\grok-local.exe'),
    [string]$InstallDir = (Join-Path $env:USERPROFILE '.local\bin'),
    [switch]$NoPathUpdate
)

$ErrorActionPreference = 'Stop'

$BinaryPath = [System.IO.Path]::GetFullPath($BinaryPath)
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
$destination = Join-Path $InstallDir 'grok-local.exe'

if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Binary not found: $BinaryPath. Build it first with: cargo build -p xai-grok-pager-bin --release"
}

New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Copy-Item -LiteralPath $BinaryPath -Destination $destination -Force

if (-not $NoPathUpdate) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    $alreadyPresent = $entries | Where-Object { $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\') }

    if (-not $alreadyPresent) {
        $entries += $InstallDir
        [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
    }

    if (($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $InstallDir.TrimEnd('\')) {
        $env:Path = "$InstallDir;$env:Path"
    }
}

Write-Host "Installed grok-local to $destination"
if (-not $NoPathUpdate) {
    Write-Host 'The current PowerShell session and future sessions can now resolve grok-local.'
}
