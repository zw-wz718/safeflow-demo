param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL)) {
    throw 'Database configuration required; no PostgreSQL permit checks ran.'
}
$previousFlags = $env:RUSTFLAGS
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
Push-Location $workspace
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') { throw 'Use repository Rust 1.98.1.' }
    $env:RUSTFLAGS = '-Dwarnings'
    & cargo test -p safeflow-storage --lib permit::tests:: --locked --offline -- --ignored --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL permit storage/transaction tests failed.' }
    # Each fixture uses and removes its own random schema; no business Tool is registered.
    [pscustomobject]@{
        compiler = $compiler
        permit_store_transactions_verified = $true
        formal_issuance_enabled = $false
        business_executed = $false
        runtime_funds_enabled = $false
    } | ConvertTo-Json
} finally {
    Pop-Location
    $env:RUSTFLAGS = $previousFlags
}
