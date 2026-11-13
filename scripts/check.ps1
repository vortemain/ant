# Ant quality gate (Windows / PowerShell).
#
# Hard gates: rustfmt, clippy with warnings denied across all targets, and
# the full workspace test suite (decision D-012).
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$failed = @()

function Invoke-Step([string]$Name, [scriptblock]$Command, [bool]$Hard = $true) {
    Write-Host "==> $Name"
    & $Command
    if ($LASTEXITCODE -ne 0) {
        if ($Hard) {
            Write-Host "FAILED: $Name" -ForegroundColor Red
            $script:failed += $Name
        } else {
            Write-Host "ADVISORY: $Name reported differences (not failing the gate)" -ForegroundColor Yellow
        }
    }
}

Invoke-Step 'cargo fmt --check' { cargo fmt --all -- --check }
Invoke-Step 'cargo clippy (deny warnings)' { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'cargo test' { cargo test --workspace }

if ($failed.Count -gt 0) {
    Write-Host "Quality gate FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host 'Quality gate passed.' -ForegroundColor Green
exit 0
