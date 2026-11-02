param([switch]$ExamplesOnly)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL) -or
    [string]::IsNullOrWhiteSpace($env:SAFEFLOW_DATABASE_URL)) {
    throw 'Both database configurations required; no simulation checks ran.'
}
$settings = @('RUSTFLAGS','SAFEFLOW_STORAGE_SCHEMA','SAFEFLOW_CONFIRMATION_TTL_SECONDS',
    'SAFEFLOW_SIM_AUTH_CHALLENGE_TTL_SECONDS','SAFEFLOW_SIM_AUTH_GRANT_TTL_SECONDS',
    'SAFEFLOW_SIM_AUTH_MAX_FAILURES','SAFEFLOW_SIM_AUTH_SECRET_BYTES')
$previous = @{}
foreach ($name in $settings) { $previous[$name] = [Environment]::GetEnvironmentVariable($name,'Process') }
Push-Location ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..')))
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') { throw 'Use repository Rust 1.98.1.' }
    $env:RUSTFLAGS = '-Dwarnings'
    if (-not $ExamplesOnly) {
        & cargo test -p safeflow-storage --test simulation_auth --locked --offline -- --ignored --test-threads=1
        if ($LASTEXITCODE -ne 0) { throw 'Real PostgreSQL simulation authentication tests failed.' }
    }
    $env:SAFEFLOW_STORAGE_SCHEMA = 'sf_auth_sim_' + [Guid]::NewGuid().ToString('N')
    # Explicit mechanism-demo settings only, not approved product authentication rules.
    $env:SAFEFLOW_CONFIRMATION_TTL_SECONDS = '300'
    $env:SAFEFLOW_SIM_AUTH_CHALLENGE_TTL_SECONDS = '300'
    $env:SAFEFLOW_SIM_AUTH_GRANT_TTL_SECONDS = '120'
    $env:SAFEFLOW_SIM_AUTH_MAX_FAILURES = '3'
    $env:SAFEFLOW_SIM_AUTH_SECRET_BYTES = '32'
    $savedText = & cargo run -p safeflow-storage --example simulation_auth --locked --offline -- write
    if ($LASTEXITCODE -ne 0) { throw 'Creation and actual material verification failed.' }
    $saved = ($savedText -join [char]10) | ConvertFrom-Json
    $readText = & cargo run -p safeflow-storage --example simulation_auth --locked --offline -- read $saved.grant.binding.task_id $saved.grant.grant_id
    if ($LASTEXITCODE -ne 0) { throw 'Independent read/verify process failed.' }
    $read = ($readText -join [char]10) | ConvertFrom-Json
    if (($saved.grant | ConvertTo-Json -Depth 20 -Compress) -cne ($read.grant | ConvertTo-Json -Depth 20 -Compress)) { throw 'Restored grant differs.' }
    $revokeText = & cargo run -p safeflow-storage --example simulation_auth --locked --offline -- revoke $saved.grant.binding.task_id $saved.grant.grant_id
    if ($LASTEXITCODE -ne 0) { throw 'Independent revocation process failed.' }
    $revoked = ($revokeText -join [char]10) | ConvertFrom-Json
    if ($revoked.grant.status -cne 'REVOKED') { throw 'Revocation was not stored.' }
    $rejectText = & cargo run -p safeflow-storage --example simulation_auth --locked --offline -- reject $saved.grant.binding.task_id $saved.grant.grant_id
    if ($LASTEXITCODE -ne 0) { throw 'Restart accepted revoked grant.' }
    $rejected = ($rejectText -join [char]10) | ConvertFrom-Json
    if ($rejected.rejected -cne 'AUTH_GRANT_REVOKED') { throw 'Unexpected rejection.' }
    [pscustomobject]@{
        compiler=$compiler; schema=$env:SAFEFLOW_STORAGE_SCHEMA
        mode=$saved.grant.mode; task_id=$saved.grant.binding.task_id; grant_id=$saved.grant.grant_id
        separate_process_records_equal=$true; wrong_material_rejected=$true; replay_did_not_recount_or_extend=$true
        revoked_record_rejected_in_new_process=$true; formal_auth_enabled=$false; formal_permit_enabled=$false
        business_executed=$false; runtime_funds_enabled=$false
    } | ConvertTo-Json
    # Retain this single new demonstration schema for independent read-back only.
} finally {
    Pop-Location
    foreach ($name in $settings) { [Environment]::SetEnvironmentVariable($name,$previous[$name],'Process') }
}
