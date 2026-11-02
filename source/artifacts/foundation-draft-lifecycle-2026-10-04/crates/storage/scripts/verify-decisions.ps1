param([switch]$ExamplesOnly)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL) -or
    [string]::IsNullOrWhiteSpace($env:SAFEFLOW_DATABASE_URL)) {
    throw 'Both server database configurations required; no checks ran.'
}
$settings = @('RUSTFLAGS','SAFEFLOW_STORAGE_SCHEMA','SAFEFLOW_DECISION_TTL_SECONDS','SAFEFLOW_DECISION_DEPLOYMENT_REVISION')
$previous = @{}
foreach ($name in $settings) { $previous[$name] = [Environment]::GetEnvironmentVariable($name,'Process') }
Push-Location ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..')))
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') { throw 'Use repository Rust 1.98.1.' }
    $env:RUSTFLAGS = '-Dwarnings'
    if (-not $ExamplesOnly) {
        & cargo test -p safeflow-storage --test decisions --locked --offline -- --ignored --test-threads=1
        if ($LASTEXITCODE -ne 0) { throw 'Real PostgreSQL decision regressions failed.' }
    }
    $env:SAFEFLOW_STORAGE_SCHEMA = 'sf_decision_demo_' + [Guid]::NewGuid().ToString('N')
    # Explicit mechanism-demo settings only, not approved product expiry or policy rules.
    $env:SAFEFLOW_DECISION_TTL_SECONDS = '300'
    $env:SAFEFLOW_DECISION_DEPLOYMENT_REVISION = 'checked-bank-demo-2026-10-05-v1'
    foreach ($mode in @('balance','payees','blocked')) {
        $savedText = & cargo run -p safeflow-storage --example security_decision --locked --offline -- write $mode
        if ($LASTEXITCODE -ne 0) { throw "Actual preflight/save failed: $mode" }
        $saved = ($savedText -join [char]10) | ConvertFrom-Json
        $readText = & cargo run -p safeflow-storage --example security_decision --locked --offline -- read $saved.record.binding.task_id $saved.record.stored_decision_id
        if ($LASTEXITCODE -ne 0) { throw 'Independent read/verify failed.' }
        $read = ($readText -join [char]10) | ConvertFrom-Json
        if (($saved.record | ConvertTo-Json -Depth 30 -Compress) -cne ($read.record | ConvertTo-Json -Depth 30 -Compress)) { throw 'Separate process record differs.' }
        $revokeText = & cargo run -p safeflow-storage --example security_decision --locked --offline -- revoke $saved.record.binding.task_id $saved.record.stored_decision_id
        if ($LASTEXITCODE -ne 0) { throw 'Independent revoke failed.' }
        $revoked = ($revokeText -join [char]10) | ConvertFrom-Json
        if ($revoked.record.status -cne 'REVOKED') { throw 'Revocation was not persisted.' }
        $rejectText = & cargo run -p safeflow-storage --example security_decision --locked --offline -- reject $saved.record.binding.task_id $saved.record.stored_decision_id
        if ($LASTEXITCODE -ne 0) { throw 'Independent terminal check failed.' }
        $rejected = ($rejectText -join [char]10) | ConvertFrom-Json
        if ($rejected.rejected -cne 'DECISION_INACTIVE' -or $saved.tool_execute_calls -ne 0 -or $read.tool_execute_calls -ne 0) { throw 'Unexpected outcome or Tool execution.' }
        [pscustomobject]@{ compiler=$compiler; schema=$env:SAFEFLOW_STORAGE_SCHEMA; demo=$mode
            stored_decision_id=$saved.record.stored_decision_id; outcome=$saved.record.decision.outcome
            separate_process_records_equal=$true; revoked_record_rejected=$true; tool_execute_calls=0
            formal_permit_enabled=$false; runtime_funds_enabled=$false } | ConvertTo-Json
    }
    # Keep only this unique demonstration schema for independent review.
} finally {
    Pop-Location
    foreach ($name in $settings) { [Environment]::SetEnvironmentVariable($name,$previous[$name],'Process') }
}
