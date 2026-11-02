param([switch]$Offline)

# Run from any directory. Requires Cargo, rustfmt, Clippy and a working linker.
$ErrorActionPreference = 'Stop'
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
$repo = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../../..'))
$expectedPath = Join-Path $repo 'fixtures/subscription/SafeFlow_SUB-01_证据筛选实测_v0.1.json'
$recognitionPath = Join-Path $repo 'fixtures/subscription/SafeFlow_SUB-01_周期识别实测_v0.2.json'
$buildOptions = @('--locked', '--manifest-path', $manifest)
if ($Offline) { $buildOptions += '--offline' }
$previousConsoleEncoding = [Console]::OutputEncoding
$previousPipelineEncoding = $OutputEncoding

try {
    # Rust emits UTF-8 JSON. Windows PowerShell otherwise may decode Chinese
    # messages using the active console code page and report a false replay diff.
    [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
    $OutputEncoding = [System.Text.UTF8Encoding]::new($false)
    & cargo fmt --manifest-path $manifest --check
    if ($LASTEXITCODE -ne 0) { throw 'FORMAT_CHECK_FAILED' }

    & cargo clippy @buildOptions --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'CLIPPY_CHECK_FAILED' }

    & cargo test @buildOptions
    if ($LASTEXITCODE -ne 0) { throw 'TESTS_FAILED' }

    $output = & cargo run @buildOptions --quiet
    if ($LASTEXITCODE -ne 0) { throw 'REPLAY_FAILED' }
    $actual = ($output -join "`n") | ConvertFrom-Json
    $expected = Get-Content -LiteralPath $expectedPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if (($actual | ConvertTo-Json -Depth 100 -Compress) -cne
        ($expected | ConvertTo-Json -Depth 100 -Compress)) {
        throw 'REPLAY_DIFFERS_FROM_SAVED_RESULT'
    }
    $recognitionOutput = & cargo run @buildOptions --quiet --example recognize_mock
    if ($LASTEXITCODE -ne 0) { throw 'RECOGNITION_REPLAY_FAILED' }
    $recognition = ($recognitionOutput -join "`n") | ConvertFrom-Json
    $recognitionExpected = Get-Content -LiteralPath $recognitionPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if (($recognition | ConvertTo-Json -Depth 100 -Compress) -cne
        ($recognitionExpected | ConvertTo-Json -Depth 100 -Compress)) {
        throw 'RECOGNITION_DIFFERS_FROM_SAVED_RESULT'
    }
    $billingCapture = Join-Path $repo 'fixtures/subscription/billing-consumption-1044e918.v1.json'
    $ledgerCases = Join-Path $repo 'fixtures/subscription/managed-ledger-cases.v1.json'
    $ledgerExpectedPath = Join-Path $repo 'fixtures/subscription/managed-ledger-replay.v1.json'
    $ledgerOutput = & cargo run @buildOptions --quiet --example review_ledger_mock -- $billingCapture $ledgerCases
    if ($LASTEXITCODE -ne 0) { throw 'LEDGER_REPLAY_FAILED' }
    $ledgerActual = ($ledgerOutput -join "`n") | ConvertFrom-Json
    $ledgerExpected = Get-Content -LiteralPath $ledgerExpectedPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($ledgerActual.fictional -ne $true -or $ledgerActual.production_registered -ne $false -or
        ($ledgerActual.reports | ConvertTo-Json -Depth 100 -Compress) -cne
        ($ledgerExpected.reports | ConvertTo-Json -Depth 100 -Compress)) {
        throw 'LEDGER_REPLAY_DIFFERS_FROM_SAVED_RESULT'
    }
    # Keep the already delivered registration/intent examples under the same
    # regression check; saved metadata counts are historical, compare replay only.
    $businessReplays = @(
        @{ Example = 'replay_confirmation_mock'; Fixture = 'SafeFlow_SUB-02_确认登记模拟实测_v0.1.json' },
        @{ Example = 'replay_intents_mock'; Fixture = 'SafeFlow_SUB-02_取消意图与撤回模拟实测_v0.1.json' },
        @{ Example = 'replay_lifecycle_mock'; Fixture = 'SafeFlow_SUB-02_完整模拟流程实测_v0.2.json' },
        @{ Example = 'replay_reminders_mock'; Fixture = 'SafeFlow_SUB-03_提醒模拟实测_v0.2.json' },
        @{ Example = 'replay_subscription_handoff_mock'; Fixture = 'SafeFlow_SUB-03_串联交接演示实测_v0.1.json' }
    )
    foreach ($replay in $businessReplays) {
        $businessOutput = & cargo run @buildOptions --quiet --example $replay.Example
        if ($LASTEXITCODE -ne 0) { throw ('BUSINESS_REPLAY_FAILED: ' + $replay.Example) }
        $businessActual = ($businessOutput -join "`n") | ConvertFrom-Json
        $businessExpectedPath = Join-Path $repo ('fixtures/subscription/' + $replay.Fixture)
        $businessExpected = Get-Content -LiteralPath $businessExpectedPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if (($businessActual | ConvertTo-Json -Depth 100 -Compress) -cne
            ($businessExpected.replay | ConvertTo-Json -Depth 100 -Compress)) {
            throw ('BUSINESS_REPLAY_DIFFERS_FROM_SAVED_RESULT: ' + $replay.Example)
        }
    }
    Write-Output 'PASS: format, Clippy, tests and eight saved subscription replays including the fixed full-chain handoff demonstration.'
    exit 0
} catch {
    Write-Error $_ -ErrorAction Continue
    exit 1
} finally {
    [Console]::OutputEncoding = $previousConsoleEncoding
    $OutputEncoding = $previousPipelineEncoding
}
