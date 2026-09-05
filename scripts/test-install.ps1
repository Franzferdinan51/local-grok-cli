$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$installScript = Join-Path $PSScriptRoot 'install.ps1'
$sourceBinary = Join-Path $repoRoot 'target\release\grok-local.exe'
$installDir = Join-Path ([System.IO.Path]::GetTempPath()) ('grok-local-test-' + [Guid]::NewGuid())

try {
    & $installScript -BinaryPath $sourceBinary -InstallDir $installDir -NoPathUpdate

    $installedBinary = Join-Path $installDir 'grok-local.exe'
    if (-not (Test-Path -LiteralPath $installedBinary)) {
        throw "Expected installed binary at $installedBinary"
    }
} finally {
    if (Test-Path -LiteralPath $installDir) {
        Remove-Item -LiteralPath $installDir -Recurse -Force
    }
}

Write-Host 'PASS: Windows installer copies grok-local.exe'
