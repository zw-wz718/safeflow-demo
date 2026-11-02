param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL) -or
    [string]::IsNullOrWhiteSpace($env:SAFEFLOW_DATABASE_URL)) {
    throw 'Both database configurations are required; no PostgreSQL checks ran.'
}
$previousFlags = $env:RUSTFLAGS
$previousSchema = $env:SAFEFLOW_STORAGE_SCHEMA
$previousTtl = $env:SAFEFLOW_CONFIRMATION_TTL_SECONDS
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
Push-Location $workspace
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') { throw 'Use repository Rust 1.98.1.' }
    $env:RUSTFLAGS = '-Dwarnings'
    & cargo test -p safeflow-storage --test confirmations --locked --offline -- --ignored --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL confirmation tests failed.' }
    $env:SAFEFLOW_STORAGE_SCHEMA = 'sf_confirmation_demo_' + [Guid]::NewGuid().ToString('N')
    # Isolated foundation test configuration, not an approved product TTL.
    $env:SAFEFLOW_CONFIRMATION_TTL_SECONDS = '300'
    $savedText = & cargo run -p safeflow-storage --example confirmation_record --locked --offline -- write
    if ($LASTEXITCODE -ne 0) { throw 'Preparation/confirmation process failed.' }
    $savedJson = $savedText -join [char]10
    $saved = $savedJson | ConvertFrom-Json
    $readText = & cargo run -p safeflow-storage --example confirmation_record --locked --offline -- read $saved.binding.task_id $saved.confirmation_ref
    if ($LASTEXITCODE -ne 0) { throw 'Independent read/verify process failed.' }
    if (($readText -join [char]10) -cne $savedJson) { throw 'Restart changed confirmation record.' }
    $revokedText = & cargo run -p safeflow-storage --example confirmation_record --locked --offline -- revoke $saved.binding.task_id $saved.confirmation_ref
    if ($LASTEXITCODE -ne 0) { throw 'Revocation process failed.' }
    $revoked = ($revokedText -join [char]10) | ConvertFrom-Json
    if ($revoked.status -cne 'REVOKED') { throw 'Revocation was not stored.' }
    $rejectText = & cargo run -p safeflow-storage --example confirmation_record --locked --offline -- reject $saved.binding.task_id $saved.confirmation_ref
    if ($LASTEXITCODE -ne 0) { throw 'Restart accepted revoked confirmation.' }
    $rejected = ($rejectText -join [char]10) | ConvertFrom-Json
    if ($rejected.rejected -cne 'CONFIRMATION_REVOKED') { throw 'Unexpected rejection result.' }
    [pscustomobject]@{
        compiler = $compiler
        schema = $env:SAFEFLOW_STORAGE_SCHEMA
        task_id = $saved.binding.task_id
        confirmation_ref = $saved.confirmation_ref
        separate_process_records_equal = $true
        revoked_record_rejected_in_new_process = $true
        test_ttl_seconds = 300
        business_executed = $false
        runtime_funds_enabled = $false
    } | ConvertTo-Json
    # Keep only this unique example schema for independent checking.
} finally {
    Pop-Location
    $env:RUSTFLAGS = $previousFlags
    $env:SAFEFLOW_STORAGE_SCHEMA = $previousSchema
    $env:SAFEFLOW_CONFIRMATION_TTL_SECONDS = $previousTtl
}
