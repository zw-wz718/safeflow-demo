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
$started = $false
$verified = $false
$report = [ordered]@{
    mode = 'DEDICATED_LOCAL_FICTIONAL_INTENT_RECOVERY_CHECK'
    fictional = $true; production_registered = $false; notification_sent = $false
    committed_intent_persistence_under_test = $true; full_business_recovery = $false; audit_integrated = $false
    verified = $false; database_tests_executed = $false; database_tests_passed = 0
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
    $env:CARGO_TARGET_DIR = Join-Path $runDir 'target'
    $manifest = Join-Path $packageRoot 'Cargo.toml'
    $log = Join-Path $runDir 'database-tests.log'
    $report.database_tests_executed = $true
    & cargo test --locked --offline --manifest-path $manifest --features postgres-mock-store `
        --test postgres_intent_recovery_mock -- --ignored --skip restart_child --test-threads=1 --nocapture *> $log
    if ($LASTEXITCODE -ne 0) { throw 'DATABASE_TESTS_FAILED' }
    $text = Get-Content -LiteralPath $log -Raw -Encoding UTF8
    if ($text -notmatch 'test result: ok\. 7 passed; 0 failed; 0 ignored;') { throw 'DATABASE_TEST_COUNT_MISMATCH' }
    $report.database_tests_passed = 7
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
    $env:POSTGRES_PASSWORD = $previousPassword
    $env:SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL = $previousDatabaseUrl
    $env:CARGO_TARGET_DIR = $previousTarget
    $report.verified = $verified -and $report.cleanup_completed
    $report.cancel_intent_recovery_verified = $report.verified
    $report | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $runDir 'verification.json') -Encoding UTF8
    [pscustomobject]@{ verified = $report.verified; database_tests_passed = $report.database_tests_passed;
        error = $report.error; result_file = (Join-Path $runDir 'verification.json') } | ConvertTo-Json -Compress
}
if (-not $report.verified) { exit 1 }
