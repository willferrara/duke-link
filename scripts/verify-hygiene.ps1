[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    Write-Output "=== [1/3] cargo fmt --all --check ==="
    & cargo fmt --all --check
    if ($LASTEXITCODE -ne 0) {
        Write-Error "cargo fmt check failed (exit code $LASTEXITCODE)"
        exit $LASTEXITCODE
    }

    Write-Output "=== [2/3] cargo clippy --workspace --all-targets -- -D warnings ==="
    & cargo clippy --workspace --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        Write-Error "cargo clippy failed (exit code $LASTEXITCODE)"
        exit $LASTEXITCODE
    }

    Write-Output "=== [3/3] cargo test --workspace ==="
    & cargo test --workspace
    if ($LASTEXITCODE -ne 0) {
        Write-Error "cargo test failed (exit code $LASTEXITCODE)"
        exit $LASTEXITCODE
    }

    Write-Output "=== ALL HYGIENE CHECKS PASSED ==="
    exit 0
}
finally {
    Pop-Location
}
