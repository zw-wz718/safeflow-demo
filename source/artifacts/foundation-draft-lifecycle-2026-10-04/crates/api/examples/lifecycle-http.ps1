param(
    [string]$Url = 'http://127.0.0.1:8787',
    [switch]$ConfirmDemonstrationActions
)
$ErrorActionPreference = 'Stop'
$endpoint = [Uri]$Url
if ($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1' -or $endpoint.AbsolutePath -ne '/' -or $endpoint.Query -or $endpoint.Fragment) { throw 'Explicit loopback API URL required' }
if (-not $ConfirmDemonstrationActions) { throw 'This example creates, modifies and cancels a simulated draft. Explicit -ConfirmDemonstrationActions required.' }
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_SIM_LOGIN_SECRET)) { throw 'Existing private SAFEFLOW_SIM_LOGIN_SECRET required' }
$httpSession = New-Object Microsoft.PowerShell.Commands.WebRequestSession
$headers = @{ 'x-safeflow-request' = '1' }
$taskView = $null
$actionView = $null
function Invoke-LifecycleHttp([string]$Method, [string]$Path, $Payload) {
    $script:lastRequestPath = $Path
    $options = @{ Uri = "$($Url.TrimEnd('/'))$Path"; Method = $Method; WebSession = $httpSession; Headers = $headers; TimeoutSec = 60 }
    if ($null -ne $Payload) { $options.ContentType = 'application/json'; $options.Body = $Payload | ConvertTo-Json -Depth 10 -Compress }
    Invoke-RestMethod @options
}
function Get-Scope { @{ contract_version = 2; scope = 'MOCK_DRAFT_LIFECYCLE_ONLY' } }
function Get-OperationPath($View) { "/api/sim/draft-lifecycles/$($View.task_id)/operations/$($View.operation_id)" }
function Invoke-ExplicitAction($View) {
    $path = Get-OperationPath $View
    $prepared = Invoke-LifecycleHttp POST "$path/prepare" (Get-Scope)
    Write-Host "Explicit simulated action: $($prepared.action)"
    $prepared.parameters | ConvertTo-Json -Depth 8 | Write-Host
    if ($null -ne $prepared.target_parameters) { $prepared.target_parameters | ConvertTo-Json -Depth 8 | Write-Host }
    $confirm = Get-Scope
    $confirm.confirmation_ref = $prepared.confirmation.confirmation_ref
    $confirm.display_ticket = $prepared.display_ticket
    $confirm.action = 'CONFIRM'
    $confirmed = Invoke-LifecycleHttp POST "$path/confirm" $confirm
    $execute = Get-Scope
    foreach ($field in @('task_revision', 'operation_revision', 'plan_version', 'facts_version')) { $execute[$field] = $confirmed.$field }
    $execute.confirmation_ref = $confirmed.confirmation.confirmation_ref
    $settled = Invoke-LifecycleHttp POST "$path/execute" $execute
    if ($settled.execution_status -ne 'SUCCEEDED') { throw "Result unknown: query original $path" }
    return $settled
}
try {
    Invoke-LifecycleHttp POST '/api/sim/session' @{ login_secret = $env:SAFEFLOW_SIM_LOGIN_SECRET } | Out-Null
    $taskView = Invoke-LifecycleHttp POST '/api/sim/draft-lifecycles' (Get-Scope)
    $actionView = $taskView
    $created = Invoke-ExplicitAction $taskView | Select-Object -Last 1
    foreach ($action in @('MODIFY_MOCK_DRAFT', 'CANCEL_MOCK_DRAFT')) {
        $request = Get-Scope
        foreach ($field in @('selection_ticket', 'task_revision', 'plan_version', 'facts_version')) { $request[$field] = $created.$field }
        $request.action = $action
        $request.parameters = @{ draft_id = $created.receipt.draft_id; expected_version = $created.receipt.draft_version }
        if ($action -eq 'MODIFY_MOCK_DRAFT') { $request.parameters.changes = @{ amount = '300.00' } }
        $actionView = Invoke-LifecycleHttp POST "/api/sim/draft-lifecycles/$($created.task_id)/actions" $request
        $created = Invoke-ExplicitAction $actionView | Select-Object -Last 1
    }
    if ($created.task_state -ne 'CANCELLED' -or $created.receipt.draft_version -ne 2) { throw 'Unexpected lifecycle result; query original task' }
    Invoke-LifecycleHttp GET (Get-OperationPath $taskView) $null | ConvertTo-Json -Depth 10
    Invoke-LifecycleHttp GET "$(Get-OperationPath $actionView)/current-draft" $null | ConvertTo-Json -Depth 10
} catch {
    Write-Warning "Failed request: $script:lastRequestPath"
    if ($null -ne $_.Exception.Response) { Write-Warning "HTTP status: $([int]$_.Exception.Response.StatusCode)" }
    if ($null -ne $taskView) { Write-Warning "No automatic POST retry. Original task: $($taskView.task_id); operation: $($actionView.operation_id)" }
    throw 'Lifecycle demonstration stopped. Query the original task/operation before any further action.'
}
