param(
    [string]$Url = 'http://127.0.0.1:8787',
    [switch]$ConfirmDemonstrationActions
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding
$endpoint = [Uri]$Url
if ($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1' -or $endpoint.AbsolutePath -ne '/' -or $endpoint.Query -or $endpoint.Fragment) { throw 'Explicit loopback URL required' }
if (-not $ConfirmDemonstrationActions) { throw 'Pass -ConfirmDemonstrationActions to confirm the displayed fixed demonstration draft and payment intent; no funds execution is available' }
if ([string]::IsNullOrWhiteSpace($env:SAFEFLOW_SIM_LOGIN_SECRET)) { throw 'Existing private simulation login secret required' }
$httpSession = New-Object Microsoft.PowerShell.Commands.WebRequestSession
function Invoke-AgentHttp([string]$Method, [string]$Path, $Payload) {
    $options = @{ Uri = "$($Url.TrimEnd('/'))$Path"; Method = $Method; WebSession = $httpSession; Headers = @{'x-safeflow-request' = '1'}; TimeoutSec = 60 }
    if ($null -ne $Payload) { $options.ContentType = 'application/json; charset=utf-8'; $options.Body = [Text.Encoding]::UTF8.GetBytes(($Payload | ConvertTo-Json -Depth 25 -Compress)) }
    try {
        $response = Invoke-WebRequest @options -UseBasicParsing
        [Text.Encoding]::UTF8.GetString($response.RawContentStream.ToArray()) | ConvertFrom-Json
    } catch { Write-Warning "Failed HTTP path: $Path. No automatic POST retry."; throw }
}
Invoke-AgentHttp POST '/api/sim/session' @{login_secret = $env:SAFEFLOW_SIM_LOGIN_SECRET} | Out-Null
$status = Invoke-AgentHttp GET '/api/agent/status' $null
if ($status.planning_mode -ne 'FIXED_PLAN' -or -not $status.payment_planning_enabled -or $status.funds_execution_enabled) { throw 'Fixed-plan payment planning with funds disabled is required' }
$initial = Invoke-AgentHttp POST '/api/agent/samples' @{sample = 'CREATE_200'; selection = @{account_id = 'account_001'; payee_query = [string]([char]0x5C0F) + [char]0x674E; payee_id = 'payee_008'}}
$draft = $initial.draft
$prepared = (Invoke-AgentHttp POST "/api/agent/tasks/$($draft.task_id)/prepare" @{operation_id = $draft.operation_id}).draft
$prepared.target_parameters | ConvertTo-Json -Depth 10 | Write-Host
$operationPath = "/api/sim/draft-lifecycles/$($draft.task_id)/operations/$($draft.operation_id)"
$confirmed = Invoke-AgentHttp POST "$operationPath/confirm" @{contract_version = 2; scope = 'MOCK_DRAFT_LIFECYCLE_ONLY'; confirmation_ref = $prepared.confirmation.confirmation_ref; display_ticket = $prepared.display_ticket; action = 'CONFIRM'}
$draft = Invoke-AgentHttp POST "$operationPath/execute" @{contract_version = 2; scope = 'MOCK_DRAFT_LIFECYCLE_ONLY'; confirmation_ref = $confirmed.confirmation.confirmation_ref; task_revision = $confirmed.task_revision; operation_revision = $confirmed.operation_revision; plan_version = $confirmed.plan_version; facts_version = $confirmed.facts_version}
$context = @{}
foreach ($field in @('task_id', 'task_revision', 'plan_version', 'facts_version', 'selection_ticket')) { $context[$field] = $draft.$field }
$response = Invoke-AgentHttp POST '/api/agent/samples' @{sample = 'EXECUTE_TRANSFER'; context = $context}
if ($response.kind -ne 'PAYMENT' -or $response.payment_flow.status -ne 'WAITING_PAYMENT_CONFIRMATION') { throw 'Expected independent payment confirmation' }
$payment = $response.payment
$payment.snapshot | ConvertTo-Json -Depth 15 | Write-Host
$decision = @{contract_version = 1; scope = 'PAYMENT_INTENT_ONLY'; confirmation_ref = $payment.confirmation_ref; display_ticket = $payment.display_ticket; action = 'CONFIRM'}
Invoke-AgentHttp POST $response.payment_flow.next_path $decision | Out-Null
$waiting = Invoke-AgentHttp POST "/api/agent/tasks/$($draft.task_id)/continue" @{task_revision = $draft.task_revision; plan_version = $draft.plan_version; facts_version = $draft.facts_version; waiting_node_id = $response.payment_flow.waiting_node_id}
if ($waiting.payment_flow.status -ne 'WAITING_EXECUTION_CONDITIONS' -or $waiting.payment.funds_execution_enabled -or $waiting.payment.business_preflight.observations.Count -ne 1) { throw 'Expected persisted actual preflight and safe wait, no funds submission' }
$recovered = Invoke-AgentHttp GET "/api/agent/tasks/$($draft.task_id)" $null
if ($recovered.payment.operation.operation_id -ne $payment.operation.operation_id) { throw 'Recovery changed the original payment intent' }
$recovered | ConvertTo-Json -Depth 30
