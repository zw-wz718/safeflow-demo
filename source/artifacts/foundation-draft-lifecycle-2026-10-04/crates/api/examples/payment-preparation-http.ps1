param(
    [string]$Url='http://127.0.0.1:8787',
    [Parameter(Mandatory=$true)][string]$TaskId,
    [ValidateSet('CONFIRM','REJECT')][string]$Decision,
    [switch]$QueryOnly
)
$ErrorActionPreference='Stop'
$endpoint=[Uri]$Url
if($endpoint.Scheme -ne 'http' -or $endpoint.Host -ne '127.0.0.1' -or $endpoint.AbsolutePath -ne '/' -or $endpoint.Query -or $endpoint.Fragment) {throw 'Explicit loopback API URL required'}
if($TaskId -notmatch '^[A-Za-z0-9_-]{1,200}$') {throw 'Original task ID required'}
if([string]::IsNullOrWhiteSpace($env:SAFEFLOW_SIM_LOGIN_SECRET)) {throw 'Existing private simulation login secret required'}
$httpSession=New-Object Microsoft.PowerShell.Commands.WebRequestSession
$headers=@{'x-safeflow-request'='1'}
function Invoke-PaymentHttp([string]$Method,[string]$Path,$Payload) {
    $options=@{Uri="$($Url.TrimEnd('/'))$Path";Method=$Method;WebSession=$httpSession;Headers=$headers;TimeoutSec=60}
    if($null -ne $Payload) {$options.ContentType='application/json';$options.Body=$Payload | ConvertTo-Json -Depth 15 -Compress}
    Invoke-RestMethod @options
}
Invoke-PaymentHttp POST '/api/sim/session' @{login_secret=$env:SAFEFLOW_SIM_LOGIN_SECRET} | Out-Null
$root="/api/sim/payment-preparations/$TaskId"
if($QueryOnly) {
    Invoke-PaymentHttp GET $root $null | ConvertTo-Json -Depth 15
    return
}
$draft=Invoke-PaymentHttp GET "/api/sim/draft-lifecycles/$TaskId" $null
if($draft.task_state -ne 'WAITING_USER' -or $null -eq $draft.receipt) {throw 'Current persisted successful draft required'}
$source=@{}
foreach($field in @('selection_ticket','task_revision','plan_version','facts_version')) {$source[$field]=$draft.$field}
$source.draft_id=$draft.receipt.draft_id
$source.draft_version=$draft.receipt.draft_version
$payment=Invoke-PaymentHttp POST $root @{contract_version=1;scope='PAYMENT_INTENT_ONLY';source=$source}
$payment | ConvertTo-Json -Depth 15 | Write-Host
if($payment.state -ne 'AWAITING_PAYMENT_CONFIRMATION') {throw 'Original payment intent is no longer awaiting confirmation; review its stored state'}
if(-not $Decision) {$Decision=Read-Host 'CONFIRM or REJECT this displayed payment intent (no funds execution)'}
if($Decision -notin @('CONFIRM','REJECT')) {throw 'No explicit payment decision supplied'}
$path="$root/$($payment.operation.operation_id)"
Invoke-PaymentHttp POST "$path/confirm" @{contract_version=1;scope='PAYMENT_INTENT_ONLY';confirmation_ref=$payment.confirmation_ref;display_ticket=$payment.display_ticket;action=$Decision} | Out-Null
Invoke-PaymentHttp GET $path $null | ConvertTo-Json -Depth 15
