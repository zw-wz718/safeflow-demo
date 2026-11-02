param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL) -or
    [string]::IsNullOrWhiteSpace($env:SAFEFLOW_DATABASE_URL)) {
    throw 'Both SAFEFLOW_TEST_DATABASE_URL and SAFEFLOW_DATABASE_URL are required; no database checks ran.'
}
$previousFlags = $env:RUSTFLAGS
$previousSchema = $env:SAFEFLOW_STORAGE_SCHEMA
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
Push-Location $workspace
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') {
        throw 'Use the repository toolchain Rust 1.98.1.'
    }
    $env:RUSTFLAGS = '-Dwarnings'
    & cargo test -p safeflow-storage --test postgres --locked --offline -- --ignored --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Real PostgreSQL tests failed.' }
    $env:SAFEFLOW_STORAGE_SCHEMA = 'sf_step4a_demo_' + [Guid]::NewGuid().ToString('N')
    # Two cargo invocations launch separate example processes, with no shared memory.
    $savedText = & cargo run -p safeflow-storage --example recovery_checkpoint --locked --offline -- write
    if ($LASTEXITCODE -ne 0) { throw 'First process failed to save its checkpoint.' }
    $savedJson = $savedText -join [char]10
    $saved = $savedJson | ConvertFrom-Json
    $restoredText = & cargo run -p safeflow-storage --example recovery_checkpoint --locked --offline -- read $saved.task.task_id
    if ($LASTEXITCODE -ne 0) { throw 'Second process failed to read the checkpoint.' }
    $restoredJson = $restoredText -join [char]10
    if ($savedJson -cne $restoredJson) { throw 'Restored task/operation/audit/progress differed from the first process.' }
    [pscustomobject]@{
        compiler = $compiler
        schema = $env:SAFEFLOW_STORAGE_SCHEMA
        task_id = $saved.task.task_id
        operation_id = $saved.operations[0].operation_id
        task_state = $saved.task.state
        task_revision = $saved.task.revision
        recovery_node = $saved.task.checkpoint.node_id
        audit_count = $saved.audit.Count
        separate_process_snapshots_equal = $true
        automatic_execution = $false
    } | ConvertTo-Json
    # Retain only this demo's unique schema for independent review. Tests remove
    # their own unique schemas and never remove an existing database/schema.
} finally {
    Pop-Location
    $env:RUSTFLAGS = $previousFlags
    $env:SAFEFLOW_STORAGE_SCHEMA = $previousSchema
}
