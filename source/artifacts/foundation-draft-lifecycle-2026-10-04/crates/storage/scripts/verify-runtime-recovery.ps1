param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_TEST_DATABASE_URL) -or
    [string]::IsNullOrWhiteSpace($env:SAFEFLOW_DATABASE_URL)) {
    throw 'Both database URL variables are required; no Runtime recovery database checks ran.'
}
$previousFlags = $env:RUSTFLAGS
$previousSchema = $env:SAFEFLOW_STORAGE_SCHEMA
Push-Location ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..')))
try {
    $compiler = & rustc --version
    if ($LASTEXITCODE -ne 0 -or $compiler -notmatch '^rustc 1\.98\.1 ') { throw 'Use repository Rust 1.98.1.' }
    $env:RUSTFLAGS = '-Dwarnings'
    & cargo test -p safeflow-storage --test runtime_recovery --locked --offline -- --ignored --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Runtime recovery PostgreSQL tests failed.' }
    $env:SAFEFLOW_STORAGE_SCHEMA = 'sf_step4b_demo_' + [Guid]::NewGuid().ToString('N')
    $savedText = & cargo run -p safeflow-storage --example runtime_wait_recovery --locked --offline -- save
    if ($LASTEXITCODE -ne 0) { throw 'Runtime save process failed.' }
    $saved = ($savedText -join [char]10) | ConvertFrom-Json
    $restoredText = & cargo run -p safeflow-storage --example runtime_wait_recovery --locked --offline -- restore $saved.task.task_id $saved.task.revision $saved.task.plan.plan_version $saved.task.facts_version
    if ($LASTEXITCODE -ne 0) { throw 'Runtime restore process failed.' }
    $restored = ($restoredText -join [char]10) | ConvertFrom-Json
    if (($saved.task | ConvertTo-Json -Depth 100 -Compress) -cne ($restored.task | ConvertTo-Json -Depth 100 -Compress)) { throw 'Restored task differs.' }
    if ($saved.tool_execution_count -ne 1 -or $restored.tool_execution_count -ne 0 -or
        $saved.task.state -ne 'WAITING_USER' -or $restored.revalidated_facts_version -ne $saved.task.facts_version -or
        $restored.automatic_execution -ne $false) { throw 'Runtime wait boundaries failed.' }
    [pscustomobject]@{
        compiler=$compiler; schema=$env:SAFEFLOW_STORAGE_SCHEMA; task_id=$saved.task.task_id
        task_revision=$saved.task.revision; plan_version=$saved.task.plan.plan_version; facts_version=$saved.task.facts_version
        completed_nodes=$saved.task.completed_nodes; waiting_node=$saved.task.checkpoint.node_id
        save_tool_execution_count=$saved.tool_execution_count; restore_tool_execution_count=$restored.tool_execution_count
        revalidated_facts_version=$restored.revalidated_facts_version; separate_process_tasks_equal=$true; automatic_execution=$false
    } | ConvertTo-Json -Depth 10
    # Keep only this newly created demo schema for independent read-back.
} finally {
    Pop-Location
    $env:RUSTFLAGS = $previousFlags
    $env:SAFEFLOW_STORAGE_SCHEMA = $previousSchema
}
