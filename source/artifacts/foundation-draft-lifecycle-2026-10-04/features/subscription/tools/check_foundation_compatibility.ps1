param(
    [Parameter(Mandatory = $true)][string]$FoundationRoot,
    [Parameter(Mandatory = $true)][string]$WorkDir
)
$ErrorActionPreference = 'Stop'
$foundation = (Resolve-Path -LiteralPath $FoundationRoot).Path
$package = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$repo = [IO.Path]::GetFullPath((Join-Path $package '../../../..'))
$work = (Resolve-Path -LiteralPath $WorkDir).Path
foreach ($source in @($repo, $foundation)) {
    if ($work -eq $source -or $work.StartsWith($source + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'RESULT_DIRECTORY_INSIDE_SOURCE'
    }
}
$run = Join-Path $work ('subscription-r2-compatibility-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $run 'src') | Out-Null
$report = [ordered]@{
    kind = 'COMPILE_ONLY_CONNECTION_COMPATIBILITY'; passed = $false
    public_database_executed = $false; permit_issued = $false; formal_integration_verified = $false
    candidate_unchanged = $false; lock_seeded_from_candidate = $true
    tokio = '1.53.2'; chrono = '0.4.38'; sqlx = '0.7.4'
}
$protected = @('candidate-manifest.json','Cargo.toml','Cargo.lock','crates/storage/Cargo.toml','crates/storage/src/permit.rs')
$before = @{}
foreach ($relative in $protected) { $before[$relative] = (Get-FileHash -LiteralPath (Join-Path $foundation $relative) -Algorithm SHA256).Hash }
$oldTarget = $env:CARGO_TARGET_DIR
function Assert-CandidateFiles($Entries) {
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in $Entries) {
        $path = [IO.Path]::GetFullPath((Join-Path $foundation $entry.path))
        if (-not $path.StartsWith($foundation + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
            -not $seen.Add($entry.path) -or
            (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $entry.sha256) {
            throw 'CANDIDATE_CONTENT_MISMATCH'
        }
    }
    if (@(Get-ChildItem -LiteralPath $foundation -Recurse -File -Force).Count -ne $seen.Count + 1) { throw 'CANDIDATE_INVENTORY_MISMATCH' }
}
try {
    $manifest = Get-Content -LiteralPath (Join-Path $foundation 'candidate-manifest.json') -Raw | ConvertFrom-Json
    if ($manifest.source.snapshotId -ne 'sha256:9eff89113bd2b37964054cd76d9eb2446c75b356589bfb66a6a72fc428eba797') { throw 'CANDIDATE_VERSION_MISMATCH' }
    Assert-CandidateFiles $manifest.files
    $report.verified_candidate_files = @($manifest.files).Count
    $report.snapshot_id = $manifest.source.snapshotId
    $foundationPath = (Join-Path $foundation 'crates/storage').Replace('\','/') | ConvertTo-Json -Compress
    $subscriptionPath = $package.Replace('\','/') | ConvertTo-Json -Compress
    @"
[package]
name = "subscription-r2-compatibility"
version = "0.1.0"
edition = "2021"
publish = false
[dependencies]
safeflow-storage = { path = $foundationPath }
safeflow-subscription-evidence = { path = $subscriptionPath, features = ["postgres-mock-store"] }
tokio = "=1.53.2"
chrono = "=0.4.38"
"@ | Set-Content -LiteralPath (Join-Path $run 'Cargo.toml') -Encoding UTF8
    Copy-Item -LiteralPath (Join-Path $foundation 'Cargo.lock') -Destination (Join-Path $run 'Cargo.lock')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'foundation_compatibility_probe.rs') -Destination (Join-Path $run 'src/lib.rs')
    if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $run 'target' }
    & cargo check --offline --manifest-path (Join-Path $run 'Cargo.toml') *> (Join-Path $run 'check.log')
    $report.cargo_exit_code = $LASTEXITCODE
    if ($LASTEXITCODE -ne 0) { throw 'COMPATIBILITY_CHECK_FAILED' }
    # A second pass requires the resolved isolated lock to remain unchanged.
    & cargo check --locked --offline --manifest-path (Join-Path $run 'Cargo.toml') *> (Join-Path $run 'locked-check.log')
    $report.locked_cargo_exit_code = $LASTEXITCODE
    if ($LASTEXITCODE -ne 0) { throw 'LOCKED_COMPATIBILITY_CHECK_FAILED' }
    $report.passed = $true
} catch {
    $report.error = if ($_.Exception.Message -match '^[A-Z_]+$') { $_.Exception.Message } else { 'CHECK_FAILED' }
} finally {
    $env:CARGO_TARGET_DIR = $oldTarget
    $unchanged = $true
    foreach ($relative in $protected) {
        if ((Get-FileHash -LiteralPath (Join-Path $foundation $relative) -Algorithm SHA256).Hash -ne $before[$relative]) { $unchanged = $false }
    }
    if ($manifest) {
        try { Assert-CandidateFiles $manifest.files } catch { $unchanged = $false }
    } else { $unchanged = $false }
    $report.candidate_unchanged = $unchanged
    if (-not $unchanged) { $report.passed = $false; $report.error = 'CANDIDATE_CHANGED' }
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $run 'verification.json') -Encoding UTF8
}
[pscustomobject]@{ passed = $report.passed; result_file = (Join-Path $run 'verification.json') } | ConvertTo-Json -Compress
if (-not $report.passed) { exit 1 }
