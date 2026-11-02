param([ValidateSet('Demo', 'Consumption', 'Test')][string]$Mode = 'Demo')
$ErrorActionPreference = 'Stop'
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../../..'))
$fixture = Join-Path $packageRoot 'fixtures/billing/upstream/mock_transfer_200.2a72b5d.json'
$savedCargo = $env:CARGO_HOME
$savedRustup = $env:RUSTUP_HOME
$savedPath = $env:PATH
try {
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    if ($cargoCommand) {
        $cargoExe = $cargoCommand.Source
    } else {
        $workspaceRoot = [IO.Path]::GetFullPath((Join-Path $packageRoot '../..'))
        $isolatedRust = Join-Path $workspaceRoot 'tools/safeflow-rust'
        $cargoExe = Join-Path $isolatedRust 'cargo/bin/cargo.exe'
        if (-not (Test-Path -LiteralPath $cargoExe)) {
            throw '未找到 Cargo。请在已配置 Rust 的团队电脑执行 README 中的命令。'
        }
        $env:CARGO_HOME = Join-Path $isolatedRust 'cargo'
        $env:RUSTUP_HOME = Join-Path $isolatedRust 'rustup'
        $env:PATH = (Join-Path $env:CARGO_HOME 'bin') + ';' + $savedPath
    }
    if ($Mode -eq 'Test') {
        & $cargoExe test --locked --manifest-path $manifest
    } elseif ($Mode -eq 'Consumption') {
        $consumptionFixture = Join-Path $packageRoot 'fixtures/billing/consumption_pages.v1.json'
        & $cargoExe run --locked --manifest-path $manifest --bin billing-consumption -- --fixture $consumptionFixture
    } else {
        & $cargoExe run --locked --manifest-path $manifest -- --fixture $fixture --case all
    }
    if ($LASTEXITCODE -ne 0) { throw "账单程序执行失败，退出码 $LASTEXITCODE" }
} finally {
    $env:CARGO_HOME = $savedCargo
    $env:RUSTUP_HOME = $savedRustup
    $env:PATH = $savedPath
}
