param(
    [Parameter(Mandatory = $true)][string]$WorkDir,
    [switch]$WithDatabase,
    [string]$Docker = 'docker'
)
$ErrorActionPreference = 'Stop'
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$repoRoot = [IO.Path]::GetFullPath((Join-Path $packageRoot '../../../..'))
$resultRoot = (Resolve-Path -LiteralPath $WorkDir).Path
if ($resultRoot -eq $repoRoot -or $resultRoot.StartsWith($repoRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'RESULT_DIRECTORY_INSIDE_SOURCE' }
$runDir = Join-Path $resultRoot ('subscription-check-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runDir | Out-Null
$manifest = Join-Path $packageRoot 'Cargo.toml'
$previousTarget = $env:CARGO_TARGET_DIR
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $runDir 'target' }
$report = [ordered]@{ ordinary_verified = $false; database_requested = [bool]$WithDatabase;
    database_status = 'NOT_REQUESTED'; database_verified = $false; full_validation_complete = $false;
    handoff_example_compiled = $false; handoff_database_executed = $false;
    fictional = $true; production_registered = $false; notification_sent = $false;
    requested_checks_passed = $false; stages = @() }
function Stage([string]$Name, [string[]]$Arguments) {
    & cargo @Arguments *> (Join-Path $runDir ($Name + '.log'))
    $stageExit = $LASTEXITCODE
    $report.stages += [pscustomobject]@{ name = $Name; exit_code = $stageExit; passed = ($stageExit -eq 0) }
    if ($stageExit -ne 0) { throw ($Name.ToUpperInvariant() + '_FAILED') }
}
try {
    Stage 'format' @('fmt','--manifest-path',$manifest,'--check')
    Stage 'clippy' @('clippy','--locked','--offline','--manifest-path',$manifest,'--all-targets','--all-features','--','-D','warnings')
    Stage 'tests' @('test','--locked','--offline','--manifest-path',$manifest,'--all-features')
    Stage 'query-parser' @('test','--locked','--offline','--manifest-path',$manifest,'--all-features','--example','query_subscription_runtime_mock')
    $ordinary = 0
    foreach ($match in [regex]::Matches((Get-Content (Join-Path $runDir 'tests.log') -Raw), 'test result: ok\. (\d+) passed;')) { $ordinary += [int]$match.Groups[1].Value }
    foreach ($match in [regex]::Matches((Get-Content (Join-Path $runDir 'query-parser.log') -Raw), 'test result: ok\. (\d+) passed;')) { $ordinary += [int]$match.Groups[1].Value }
    $report.ordinary_tests_passed = $ordinary
    Stage 'handoff-build' @('build','--locked','--offline','--manifest-path',$manifest,'--features','postgres-mock-store','--example','run_connection_handoff_mock')
    $report.handoff_example_compiled = $true
    $report.ordinary_verified = $true
    if ($WithDatabase) {
        $report.database_status = 'RUNNING'
        $shell = (Get-Process -Id $PID).Path
        $databaseOutput = & $shell -NoProfile -File (Join-Path $PSScriptRoot 'check_subscription_runtime_mock.ps1') -WorkDir $resultRoot -Docker $Docker 2> (Join-Path $runDir 'database.stderr.log')
        $databaseExit = $LASTEXITCODE
        $databaseOutput | Set-Content (Join-Path $runDir 'database-result.json') -Encoding UTF8
        if ($databaseExit -ne 0) { throw 'DATABASE_CHECK_FAILED' }
        $database = ($databaseOutput -join "`n") | ConvertFrom-Json
        if (-not $database.verified) { throw 'DATABASE_NOT_VERIFIED' }
        $detail = Get-Content -LiteralPath $database.result_file -Raw -Encoding UTF8 | ConvertFrom-Json
        if (-not $detail.connection_handoff_verified -or -not $detail.cleanup_completed) { throw 'DATABASE_HANDOFF_NOT_VERIFIED' }
        $report.database_status = 'PASSED'; $report.database_verified = $true
        $report.database_report = $database.result_file
        $report.handoff_database_executed = $true
        $report.full_validation_complete = $true
    }
    $report.requested_checks_passed = $true
} catch {
    if ($WithDatabase -and $report.database_status -eq 'RUNNING') { $report.database_status = 'FAILED' }
    $code = $_.Exception.Message
    $report.error = if ($code -match '^[A-Z_-]+$') { $code } else { 'CHECK_FAILED' }
} finally {
    $env:CARGO_TARGET_DIR = $previousTarget
    $report | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $runDir 'verification.json') -Encoding UTF8
    [pscustomobject]@{ requested_checks_passed = $report.requested_checks_passed; ordinary_verified = $report.ordinary_verified;
        database_status = $report.database_status; full_validation_complete = $report.full_validation_complete;
        result_file = (Join-Path $runDir 'verification.json') } | ConvertTo-Json -Compress
}
if (-not $report.requested_checks_passed) { exit 1 }
