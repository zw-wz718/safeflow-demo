param([switch]$ExamplesOnly)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_DATABASE_URL) -or
    [string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL)) {
    throw 'Trusted PostgreSQL configuration required; no query checks ran.'
}
$previousFlags = $env:RUSTFLAGS
$previousSchema = $env:SAFEFLOW_STORAGE_SCHEMA
Push-Location ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..')))
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') { throw 'Use repository Rust 1.98.1.' }
    $env:RUSTFLAGS = '-Dwarnings'
    if (-not $ExamplesOnly) {
        & cargo test -p safeflow-storage --test task_query --locked --offline -- --ignored --test-threads=1
        if ($LASTEXITCODE -ne 0) { throw 'Task query PostgreSQL regression failed.' }
    }
    $env:SAFEFLOW_STORAGE_SCHEMA = 'sf_task_query_demo_' + [Guid]::NewGuid().ToString('N')
    $preparedText = & cargo run -p safeflow-storage --example task_progress_query --locked --offline -- prepare
    if ($LASTEXITCODE -ne 0) { throw 'Read-only waiting point preparation failed.' }
    $prepared = ($preparedText -join [char]10) | ConvertFrom-Json
    $responses = @()
    for ($read = 0; $read -lt 2; $read++) {
        $queryText = & cargo run -p safeflow-storage --example task_progress_query --locked --offline -- query $prepared.view.task_id
        if ($LASTEXITCODE -ne 0) { throw 'Independent query process failed.' }
        $queried = ($queryText -join [char]10) | ConvertFrom-Json
        if (($prepared.view | ConvertTo-Json -Depth 30 -Compress) -cne ($queried.view | ConvertTo-Json -Depth 30 -Compress)) { throw 'Query views differ across processes.' }
        if ($queried.query_tool_executions -ne 0 -or $queried.query_bank_reads -ne 0 -or
            $queried.preparation_wrote_checkpoint -ne $false -or $queried.automatic_execution -ne $false -or
            $queried.view.security -ne 'CURRENT_DECISION_NOT_PROVIDED' -or $queried.view.task_state -ne 'WAITING_USER') { throw 'Query crossed read-only boundary.' }
        $responses += $queried.view
    }
    if ($prepared.preparation_tool_executions -ne 1 -or $prepared.preparation_wrote_checkpoint -ne $true) { throw 'Preparation did not use actual Runtime query.' }
    [pscustomobject]@{ compiler=$compiler; schema=$env:SAFEFLOW_STORAGE_SCHEMA; view=$responses[0];
        independent_query_processes=2; identical_views=$true; query_tool_executions=0; query_bank_reads=0;
        preparation_wrote_checkpoint=$true; query_writes=$false; automatic_execution=$false } | ConvertTo-Json -Depth 30
    # Retain this newly created schema for independent verification.
} finally {
    Pop-Location
    $env:RUSTFLAGS = $previousFlags
    $env:SAFEFLOW_STORAGE_SCHEMA = $previousSchema
}
