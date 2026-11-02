param(
    [Parameter(Mandatory = $true)][string]$WorkDir,
    [string]$Docker = 'docker',
    [string]$Image = 'postgres:16-alpine'
)

# Dedicated disposable local database only. Never uses a pre-existing DB URL.
$ErrorActionPreference = 'Stop'
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$sourceRoot = [IO.Path]::GetFullPath((Join-Path $packageRoot '../../../..'))
$resultRoot = (Resolve-Path -LiteralPath $WorkDir).Path
if ($resultRoot -eq $sourceRoot -or $resultRoot.StartsWith($sourceRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'RESULT_DIRECTORY_INSIDE_SOURCE'
}
$runId = [Guid]::NewGuid().ToString('N')
$runDir = Join-Path $resultRoot ('subscription-postgres-' + $runId)
New-Item -ItemType Directory -Path $runDir | Out-Null
$container = 'safeflow-subscription-mock-' + $runId
$previousPassword = $env:POSTGRES_PASSWORD
$previousDatabaseUrl = $env:SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL
$previousTarget = $env:CARGO_TARGET_DIR
$previousStartupExe = $env:SAFEFLOW_SUBSCRIPTION_RUNTIME_STARTUP_EXE
$previousQueryExe = $env:SAFEFLOW_SUBSCRIPTION_RUNTIME_QUERY_EXE
$previousUserViewExe = $env:SAFEFLOW_SUBSCRIPTION_USER_VIEW_EXE
$previousPhase = $env:SAFEFLOW_RUNTIME_PHASE
$previousOwner = $env:SAFEFLOW_RUNTIME_OWNER
$started = $false
$verified = $false
$report = [ordered]@{
    mode = 'DEDICATED_LOCAL_FICTIONAL_SUBSCRIPTION_RUNTIME_RECOVERY_CHECK'
    fictional = $true; production_registered = $false; notification_sent = $false
    unified_subscription_runtime_under_test = $true; full_banking_system_recovered = $false; audit_integrated = $false
    verified = $false; database_tests_executed = $false; database_tests_passed = 0
    database_restart_verified = $false
    image = $Image; cleanup_completed = $false
}
try {
    $server = & $Docker version --format '{{.Server.Version}}' 2> (Join-Path $runDir 'docker-version.stderr.log')
    if ($LASTEXITCODE -ne 0 -or -not $server) { throw 'DOCKER_ENGINE_UNAVAILABLE' }
    $report.docker_server_version = ($server -join '').Trim()
    # Credentials stay in process environment; never write them to logs/report.
    $env:POSTGRES_PASSWORD = [Guid]::NewGuid().ToString('N') + [Guid]::NewGuid().ToString('N')
    $created = & $Docker run --detach --rm --name $container --label safeflow.subscription.mock=true `
        --env POSTGRES_PASSWORD --env POSTGRES_DB=safeflow_subscription_mock_test `
        --publish '127.0.0.1::5432' $Image 2> (Join-Path $runDir 'docker-start.stderr.log')
    if ($LASTEXITCODE -ne 0 -or -not $created) { throw 'TEST_DATABASE_START_FAILED' }
    $started = $true
    $ready = $false
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        & $Docker exec $container pg_isready -U postgres -d safeflow_subscription_mock_test *> $null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $ready) { throw 'TEST_DATABASE_NOT_READY' }
    $mapping = & $Docker port $container 5432/tcp
    if ($LASTEXITCODE -ne 0 -or ($mapping -join '') -notmatch '^127\.0\.0\.1:(\d+)$') { throw 'LOCAL_DATABASE_PORT_INVALID' }
    $port = $Matches[1]
    $env:SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL = 'postgres://postgres:' + $env:POSTGRES_PASSWORD + '@127.0.0.1:' + $port + '/safeflow_subscription_mock_test'
    $env:CARGO_TARGET_DIR = if ($previousTarget) { $previousTarget } else { Join-Path $runDir 'target' }
    $manifest = Join-Path $packageRoot 'Cargo.toml'
    $log = Join-Path $runDir 'database-tests.log'
    & cargo build --locked --offline --manifest-path $manifest --features postgres-mock-store --example start_subscription_runtime_mock --example query_subscription_runtime_mock --example query_subscription_user_view_mock *> (Join-Path $runDir 'startup-build.log')
    if ($LASTEXITCODE -ne 0) { throw 'STARTUP_EXAMPLE_BUILD_FAILED' }
    $env:SAFEFLOW_SUBSCRIPTION_RUNTIME_STARTUP_EXE = Join-Path $env:CARGO_TARGET_DIR 'debug/examples/start_subscription_runtime_mock.exe'
    $env:SAFEFLOW_SUBSCRIPTION_RUNTIME_QUERY_EXE = Join-Path $env:CARGO_TARGET_DIR 'debug/examples/query_subscription_runtime_mock.exe'
    $env:SAFEFLOW_SUBSCRIPTION_USER_VIEW_EXE = Join-Path $env:CARGO_TARGET_DIR 'debug/examples/query_subscription_user_view_mock.exe'
    $report.database_tests_executed = $true
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store `
        --test subscription_runtime_recovery_mock -- --ignored --skip runtime_child --test-threads=1 --nocapture *> $log
    if ($LASTEXITCODE -ne 0) { throw 'DATABASE_TESTS_FAILED' }
    $text = Get-Content -LiteralPath $log -Raw -Encoding UTF8
    if ($text -notmatch 'test result: ok\. 59 passed; 0 failed; 0 ignored;') { throw 'DATABASE_TEST_COUNT_MISMATCH' }
    $report.database_tests_passed = 59
    foreach ($name in @('discovery_cli_accepts_consumption_and_current_ledger_without_writes_or_foreign_scope',
        'date_suggestion_cli_reads_candidate_and_registered_dates_but_does_not_guess_expired_dates',
        'registration_to_reminder_preparation_keeps_registration_and_requires_separate_confirmation')) {
        if ($text -notmatch ([regex]::Escape($name) + ' \.\.\. ok')) { throw 'DISCOVERY_DATE_PREPARATION_PROBE_MISSING' }
    }
    $report.discovery_date_and_reminder_preparation_verified = $true
    foreach ($name in @('pending_list_cli_is_readonly_and_preserves_expired_and_restarted_drafts',
        'abandonment_rolls_back_then_persists_and_rejects_old_confirmation_without_gate',
        'abandoning_settings_cancel_and_revoke_preserves_saved_business',
        'reselection_invalidates_old_plan_and_preserves_completed_original_after_restart')) {
        if ($text -notmatch ([regex]::Escape($name) + ' \.\.\. ok')) { throw 'PENDING_WORKFLOW_PROBE_MISSING' }
    }
    $report.pending_workflow_verified = $true

    foreach ($name in @('registration_preview_reads_persisted_draft_and_rejects_replaced_or_restarted_confirmation',
        'settings_preview_reads_persisted_difference_without_changing_settings_or_accepting_old_plan',
        'intent_preview_reads_local_effect_without_execution_and_old_preview_fails_after_commit')) {
        if ($text -notmatch ([regex]::Escape($name) + ' \.\.\. ok')) { throw 'CONFIRMATION_PREVIEW_PROBE_MISSING' }
    }
    $report.confirmation_previews_verified = $true
    foreach ($name in @('current_business_queries_read_empty_list_cancel_history_and_revoke_after_restart',
        'current_reminder_queries_preserve_disabled_and_cancellation_unknown_cycles_after_restart',
        'current_reminder_queries_separate_failed_and_simulated_success_without_sending')) {
        if ($text -notmatch ([regex]::Escape($name) + ' \.\.\. ok')) { throw 'CURRENT_BUSINESS_QUERY_PROBE_MISSING' }
    }
    $report.current_subscription_list_verified = $true
    $report.current_cancellation_history_verified = $true
    $report.saved_reminder_plan_and_outcomes_verified = $true
    if ($text -notmatch 'user_view_cli_with_select_only_role_rejects_writes_and_reports_revoked_read_permission \.\.\. ok') { throw 'READONLY_ROLE_PROBE_MISSING' }
    $report.select_only_role_verified = $true
    $report.user_view_permission_failure_verified = $true
    foreach ($name in @('readonly_cli_reads_registration_settings_and_cancellation_without_restart',
        'readonly_cli_pagination_resumes_after_exit_and_append_without_missing_or_cross_owner',
        'readonly_cli_recovers_after_writer_exits_without_publishing_commit_reply')) {
        if ($text -notmatch ([regex]::Escape($name) + ' \.\.\. ok')) { throw 'READONLY_QUERY_PROBE_MISSING' }
    }
    $report.readonly_query_cli_verified = $true
    $report.cross_process_page_resume_verified = $true
    $report.lost_reply_process_exit_recovery_verified = $true
    # Restart only this run's disposable container, preserving its database storage.
    $env:SAFEFLOW_RUNTIME_OWNER = 'fictional-db-restart-' + $runId
    $env:SAFEFLOW_RUNTIME_PHASE = 'write'
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store `
        --test subscription_runtime_recovery_mock -- --ignored --exact runtime_child --nocapture *> (Join-Path $runDir 'before-database-restart.log')
    if ($LASTEXITCODE -ne 0) { throw 'RESTART_SEED_FAILED' }
    $env:SAFEFLOW_RUNTIME_OWNER = 'fictional-db-cancel-' + $runId
    $env:SAFEFLOW_RUNTIME_PHASE = 'db-cancel-write'
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store `
        --test subscription_runtime_recovery_mock -- --ignored --exact runtime_child --nocapture *> (Join-Path $runDir 'before-cancel-database-restart.log')
    if ($LASTEXITCODE -ne 0) { throw 'CANCEL_RESTART_SEED_FAILED' }
    $env:SAFEFLOW_RUNTIME_OWNER = 'fictional-db-restart-' + $runId
    $label = & $Docker inspect --format '{{index .Config.Labels "safeflow.subscription.mock"}}' $container
    if ($LASTEXITCODE -ne 0 -or ($label -join '').Trim() -ne 'true') { throw 'RESTART_CONTAINER_NOT_OWNED' }
    $beforeStart = & $Docker inspect --format '{{.State.StartedAt}}' $container
    if ($LASTEXITCODE -ne 0) { throw 'RESTART_INSPECT_FAILED' }
    & $Docker restart --time 5 $container *> (Join-Path $runDir 'database-restart.log')
    if ($LASTEXITCODE -ne 0) { throw 'DATABASE_RESTART_FAILED' }
    $afterStart = & $Docker inspect --format '{{.State.StartedAt}}' $container
    if ($LASTEXITCODE -ne 0 -or ($afterStart -join '') -eq ($beforeStart -join '')) { throw 'DATABASE_NOT_RESTARTED' }
    $ready = $false
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        & $Docker exec $container pg_isready -U postgres -d safeflow_subscription_mock_test *> $null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $ready) { throw 'RESTART_DATABASE_NOT_READY' }
    $mapping = & $Docker port $container 5432/tcp
    if ($LASTEXITCODE -ne 0 -or ($mapping -join '') -notmatch '^127\.0\.0\.1:(\d+)$') { throw 'LOCAL_DATABASE_PORT_INVALID' }
    $env:SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL = 'postgres://postgres:' + $env:POSTGRES_PASSWORD + '@127.0.0.1:' + $Matches[1] + '/safeflow_subscription_mock_test'
    $env:SAFEFLOW_RUNTIME_PHASE = 'read'
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store `
        --test subscription_runtime_recovery_mock -- --ignored --exact runtime_child --nocapture *> (Join-Path $runDir 'after-database-restart.log')
    if ($LASTEXITCODE -ne 0) { throw 'RESTART_READBACK_FAILED' }
    $probes = @()
    foreach ($filename in @('before-database-restart.log','after-database-restart.log')) {
        $raw = Get-Content -LiteralPath (Join-Path $runDir $filename) -Raw -Encoding UTF8
        if ($raw -notmatch 'RUNTIME_PROBE_JSON:(\{[^\r\n]+\})') { throw 'RESTART_PROBE_MISSING' }
        $probes += ($Matches[1] | ConvertFrom-Json)
    }
    if ($probes[0].pid -eq $probes[1].pid -or -not $probes[1].registration_restored -or -not $probes[1].settings_restored -or $probes[1].resend_calls -ne 0) { throw 'RESTART_PROBE_MISMATCH' }
    $report.database_restart_verified = $true
    $report.database_started_before = ($beforeStart -join '').Trim()
    $report.database_started_after = ($afterStart -join '').Trim()
    $report.database_restart_readback = $probes[1]
    $env:SAFEFLOW_RUNTIME_OWNER = 'fictional-db-cancel-' + $runId
    $env:SAFEFLOW_RUNTIME_PHASE = 'db-cancel-read'
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store `
        --test subscription_runtime_recovery_mock -- --ignored --exact runtime_child --nocapture *> (Join-Path $runDir 'after-cancel-database-restart.log')
    if ($LASTEXITCODE -ne 0) { throw 'CANCEL_RESTART_READBACK_FAILED' }
    $cancelProbes = @()
    foreach ($filename in @('before-cancel-database-restart.log','after-cancel-database-restart.log')) {
        $raw = Get-Content -LiteralPath (Join-Path $runDir $filename) -Raw -Encoding UTF8
        if ($raw -notmatch 'CANCEL_DATABASE_PROBE_JSON:(\{[^\r\n]+\})') { throw 'CANCEL_RESTART_PROBE_MISSING' }
        $cancelProbes += ($Matches[1] | ConvertFrom-Json)
    }
    if ($cancelProbes[0].pid -eq $cancelProbes[1].pid -or -not $cancelProbes[1].cancelling_retained -or -not $cancelProbes[1].unknown_retained -or $cancelProbes[1].notification_sent) { throw 'CANCEL_RESTART_PROBE_MISMATCH' }
    $report.database_cancel_unknown_restart_verified = $true
    $report.database_cancel_unknown_readback = $cancelProbes[1]
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store --test postgres_ledger_mock -- --ignored --test-threads=1 *> (Join-Path $runDir 'ledger-tests.log')
    if ($LASTEXITCODE -ne 0) { throw 'LEDGER_TESTS_FAILED' }
    if ((Get-Content (Join-Path $runDir 'ledger-tests.log') -Raw) -notmatch 'test result: ok\. 7 passed; 0 failed;') { throw 'LEDGER_COUNT_MISMATCH' }
    $report.ledger_database_tests_passed = 7
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store --test postgres_intent_recovery_mock -- --ignored --skip restart_child --test-threads=1 *> (Join-Path $runDir 'intent-tests.log')
    if ($LASTEXITCODE -ne 0) { throw 'INTENT_TESTS_FAILED' }
    if ((Get-Content (Join-Path $runDir 'intent-tests.log') -Raw) -notmatch 'test result: ok\. 7 passed; 0 failed;') { throw 'INTENT_COUNT_MISMATCH' }
    $report.intent_database_tests_passed = 7
    & cargo run --quiet --locked --offline --manifest-path $manifest --features postgres-mock-store --example run_connection_handoff_mock -- --run-fictional > (Join-Path $runDir 'connection-handoff.json') 2> (Join-Path $runDir 'connection-handoff.stderr.log')
    if ($LASTEXITCODE -ne 0) { throw 'CONNECTION_HANDOFF_FAILED' }
    $handoff = Get-Content (Join-Path $runDir 'connection-handoff.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not $handoff.fictional -or -not $handoff.database_execution -or $handoff.caller_owned_transactions -ne 4 -or $handoff.steps.Count -ne 4 -or $handoff.notification_sent -or $handoff.production_registered) { throw 'CONNECTION_HANDOFF_MISMATCH' }
    if (($handoff.steps | Where-Object { -not $_.original_matches }).Count -ne 0) { throw 'HANDOFF_ORIGINAL_MISMATCH' }
    if ($handoff.capability_count -ne 7 -or $handoff.capabilities.Count -ne 7 -or $handoff.incremental_record_count -ne 4 -or $handoff.audit_page_count -ne 2) { throw 'SEVEN_CAPABILITY_HANDOFF_MISMATCH' }
    $report.seven_capability_handoff_verified = $true
    $report.incremental_business_records_verified = $true
    $report.connection_handoff_verified = $true
    $report.total_database_tests_passed = $report.database_tests_passed + 14
    $verified = $true
} catch {
    $code = $_.Exception.Message
    $report.error = if ($code -match '^[A-Z_]+$') { $code } else { 'POSTGRES_CHECK_FAILED' }
} finally {
    if ($started) {
        $label = & $Docker inspect --format '{{index .Config.Labels "safeflow.subscription.mock"}}' $container 2>$null
        if ($LASTEXITCODE -eq 0 -and ($label -join '').Trim() -eq 'true') {
            & $Docker stop --time 5 $container *> $null
            $report.cleanup_completed = ($LASTEXITCODE -eq 0)
        }
    } else { $report.cleanup_completed = $true }
    $env:SAFEFLOW_RUNTIME_PHASE = $previousPhase
    $env:SAFEFLOW_RUNTIME_OWNER = $previousOwner
    $env:POSTGRES_PASSWORD = $previousPassword
    $env:SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL = $previousDatabaseUrl
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:SAFEFLOW_SUBSCRIPTION_RUNTIME_STARTUP_EXE = $previousStartupExe
    $env:SAFEFLOW_SUBSCRIPTION_RUNTIME_QUERY_EXE = $previousQueryExe
    $env:SAFEFLOW_SUBSCRIPTION_USER_VIEW_EXE = $previousUserViewExe
    $report.verified = $verified -and $report.cleanup_completed
    $report.subscription_runtime_recovery_verified = $report.verified
    $report | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $runDir 'verification.json') -Encoding UTF8
    [pscustomobject]@{ verified = $report.verified; database_tests_passed = $report.database_tests_passed;
        error = $report.error; result_file = (Join-Path $runDir 'verification.json') } | ConvertTo-Json -Compress
}
if (-not $report.verified) { exit 1 }
