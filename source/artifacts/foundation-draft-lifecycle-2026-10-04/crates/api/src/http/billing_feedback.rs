//! Explicit non-funding HTTP actions. No model tool or client evidence upload.
use super::*;
use safeflow_billing_source_reader::{
    feedback::{FeedbackChoice, FeedbackIntent},
    spending::SourceKey,
};
use safeflow_contracts::{ConfirmationRef, OperationId};
use serde_json::{json, Value};

/// Deployment-only opt in. Both TTLs must be chosen by the trusted assembler.
/// Default ApiConfig has None; no environment/client switch enables this path.
pub struct BillingFeedbackConfig {
    confirmation: ConfirmationConfig,
    permit: PermitConfig,
    recovery: Option<BillingRecoveryConfig>,
}
impl BillingFeedbackConfig {
    pub fn for_reviewed_backend(confirmation: ConfirmationConfig, permit: PermitConfig) -> Self {
        Self {
            confirmation,
            permit,
            recovery: None,
        }
    }
    pub fn with_recovery_v2(mut self, recovery: BillingRecoveryConfig) -> Self {
        self.recovery = Some(recovery);
        self
    }
}
pub(super) mod v2;
pub(super) fn service(state: &ApiState) -> Result<BillingFeedbackService<'_>, ApiError> {
    let config = state
        .0
        .billing_feedback
        .as_ref()
        .ok_or_else(ApiError::disabled)?;
    state
        .billing_feedback_for_backend(config.confirmation.clone(), config.permit.clone())
        .map_err(error)
}
pub(super) async fn checked_task(
    state: &ApiState,
    identity: &Identity,
    view: &TaskProgressView,
) -> Result<bool, ApiError> {
    if state.0.billing_feedback.is_none() {
        return Ok(false);
    }
    let owned = service(state)?
        .owns_task(&identity.reference, &view.task_id, view.revision)
        .await
        .map_err(error)?;
    identity.check(state)?;
    Ok(owned)
}
fn error(error: StorageError) -> ApiError {
    match error {
        StorageError::ReconcileRequired => ApiError {
            status: StatusCode::CONFLICT,
            reason: "RECONCILE_REQUIRED",
            task_id: None,
        },
        StorageError::ConfirmationRequired
        | StorageError::ConfirmationExpired
        | StorageError::ConfirmationRevoked
        | StorageError::ConfirmationBindingMismatch
        | StorageError::PermitBindingMismatch
        | StorageError::ExecutionKeyConflict
        | StorageError::ExecutionOperationConflict => ApiError::conflict(),
        StorageError::FormalAuthUnavailable => ApiError::forbidden(),
        other => ApiError::storage(other),
    }
}
fn plain(uri: &Uri) -> Result<(), ApiError> {
    if uri.query().is_some() {
        Err(ApiError::validation(StatusCode::BAD_REQUEST))
    } else {
        Ok(())
    }
}
fn operation(value: &str) -> Result<OperationId, ApiError> {
    if value.len() > 200 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    OperationId::parse(value).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    input_version: u16,
    report_id: String,
    transaction: SourceKey,
    choice: FeedbackChoice,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    confirmation_ref: ConfirmationRef,
}
const SCOPE: &str = "BILLING_FEEDBACK_LOCAL_ONLY";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionPrepare {
    input_version: u16,
    root_operation_id: String,
    expected_revision: u64,
    action: safeflow_storage::BillingFeedbackRevisionAction,
    choice: Option<FeedbackChoice>,
    nonce: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseUncreatedIntent { intent:RevisionPrepare, acknowledgement:String }
pub(super) async fn close_partial_preparation(State(state):State<ApiState>,uri:Uri,request:Request)->Result<Json<Value>,ApiError>{
    let identity=state.identity(request.headers())?;plain(&uri)?;
    let body:CloseUncreatedIntent=body(request).await?;
    if body.acknowledgement!="CLOSE_EXACT_PARTIAL_REVISION_PREPARATION" || body.intent.input_version!=1{return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let i=body.intent;let root=operation(&i.root_operation_id)?;let service=service(&state)?;
    let proof=tokio::time::timeout(state.0.request_timeout,service.close_partial_revision_preparation(&identity.reference,&root,i.expected_revision,i.action,i.choice,&i.nonce)).await.map_err(|_|ApiError::unavailable())?.map_err(|e|match e{StorageError::ReconcileRequired=>ApiError::conflict(),other=>error(other)})?;
    identity.check(&state)?;Ok(Json(proof))
}
pub(super) async fn close_uncreated_intent(State(state):State<ApiState>,uri:Uri,request:Request)->Result<Json<Value>,ApiError>{
    let identity=state.identity(request.headers())?;plain(&uri)?;
    let body:CloseUncreatedIntent=body(request).await?;
    if body.acknowledgement!="END_EXACT_STALE_UNCREATED_INTENT" || body.intent.input_version!=1{return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let i=body.intent;let root=operation(&i.root_operation_id)?;let service=service(&state)?;
    let proof=tokio::time::timeout(state.0.request_timeout,service.close_uncreated_revision_intent(&identity.reference,&root,i.expected_revision,i.action,i.choice,&i.nonce)).await.map_err(|_|ApiError::unavailable())?.map_err(error)?;
    identity.check(&state)?;Ok(Json(proof))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbandonRevision {root_operation_id:String,acknowledgement:String}
pub(super) async fn abandon_revision(State(state):State<ApiState>,path:Result<Path<String>,PathRejection>,uri:Uri,request:Request)->Result<Json<Value>,ApiError>{
    let identity=state.identity(request.headers())?;plain(&uri)?;
    let Path(nonce)=path.map_err(|_|ApiError::validation(StatusCode::BAD_REQUEST))?;
    let input:AbandonRevision=body(request).await?;
    if input.acknowledgement!="CLOSE_UNSUBMITTED_REVISION_ONLY"{return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let root=operation(&input.root_operation_id)?;let service=service(&state)?;
    let v=tokio::time::timeout(state.0.request_timeout,service.abandon_revision_preparation(&identity.reference,&nonce,&root)).await.map_err(|_|ApiError::unavailable())?.map_err(error)?;
    identity.check(&state)?;Ok(Json(v))
}
pub(super) async fn prepare_revision(State(state):State<ApiState>,uri:Uri,request:Request)->Result<Json<Value>,ApiError>{
    let identity=state.identity(request.headers())?;plain(&uri)?;
    let input:RevisionPrepare=body(request).await?;
    if input.input_version!=1 {return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let root=operation(&input.root_operation_id)?;
    let service=service(&state)?;
    let p=tokio::time::timeout(state.0.request_timeout,service.prepare_revision(&identity.reference,&root,input.expected_revision,input.action,input.choice,&input.nonce)).await.map_err(|_|ApiError::unavailable())?.map_err(error)?;
    identity.check(&state)?;
    Ok(Json(json!({"contract_version":1,"scope":SCOPE,"task_id":p.task.task_id,"operation_id":p.operation.operation_id,
        "parameters":p.parameters,"confirmation":{"confirmation_ref":p.confirmation.confirmation_ref,"status":p.confirmation.status,"expires_at":p.confirmation.expires_at},"lock_authorized":false,"automatic_retry":false})))
}
pub(super) async fn head(State(state):State<ApiState>,path:Result<Path<String>,PathRejection>,headers:HeaderMap,uri:Uri)->Result<Json<Value>,ApiError>{
    let identity=state.identity(&headers)?;plain(&uri)?;let Path(root)=path.map_err(|_|ApiError::validation(StatusCode::BAD_REQUEST))?;
    let root=operation(&root)?;let service=service(&state)?;
    let v=tokio::time::timeout(state.0.request_timeout,service.feedback_head(&identity.reference,&root)).await.map_err(|_|ApiError::unavailable())?.map_err(error)?;
    identity.check(&state)?;Ok(Json(v))
}
pub(super) async fn revision_preparation(State(state):State<ApiState>,path:Result<Path<String>,PathRejection>,headers:HeaderMap,uri:Uri)->Result<Json<Value>,ApiError>{
    let identity=state.identity(&headers)?;plain(&uri)?;let Path(nonce)=path.map_err(|_|ApiError::validation(StatusCode::BAD_REQUEST))?;
    let service=service(&state)?;
    let v=tokio::time::timeout(state.0.request_timeout,service.revision_preparation(&identity.reference,&nonce)).await.map_err(|_|ApiError::unavailable())?.map_err(error)?;
    identity.check(&state)?;Ok(Json(v))
}

pub(super) async fn prepare(
    State(state): State<ApiState>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    let feedback = service(&state)?;
    let input: Prepare = body(request).await?;
    if input.input_version != 1 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let prepared = tokio::time::timeout(
        state.0.request_timeout,
        feedback.prepare(
            &identity.reference,
            FeedbackIntent {
                report_id: input.report_id,
                transaction: input.transaction,
                choice: input.choice,
            },
        ),
    )
    .await
    .map_err(|_| ApiError::unavailable())?
    .map_err(error)?;
    identity.check(&state)?;
    Ok(Json(
        json!({"contract_version":1,"scope":SCOPE,"task_id":prepared.task.task_id,"operation_id":prepared.operation.operation_id,
        "parameters":prepared.parameters,"confirmation":{"confirmation_ref":prepared.confirmation.confirmation_ref,"status":prepared.confirmation.status,"expires_at":prepared.confirmation.expires_at},
        "lock_authorized":false,"automatic_retry":false}),
    ))
}
pub(super) async fn confirm(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    let feedback = service(&state)?;
    let Path((task, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let task = parse_task(&task)?;
    let op = operation(&op)?;
    let input: Action = body(request).await?;
    let confirmed = tokio::time::timeout(
        state.0.request_timeout,
        feedback.confirm_original(&identity.reference, &task, &op, &input.confirmation_ref),
    )
    .await
    .map_err(|_| ApiError::unavailable().with_task(&task))?
    .map_err(error)?;
    identity.check(&state)?;
    Ok(Json(
        json!({"contract_version":1,"scope":SCOPE,"task_id":task,"operation_id":op,"confirmation_ref":confirmed.confirmation_ref,"status":confirmed.status,"lock_authorized":false,"automatic_retry":false}),
    ))
}
fn result(view: BillingFeedbackView) -> Value {
    json!({"contract_version":1,"scope":SCOPE,"task_id":view.operation.task_id,"task_state":view.task_state,"operation_id":view.operation.operation_id,"operation_state":view.operation.state,"saved":view.saved,"replayed":view.replayed,"domain_audit":view.domain_audit,"lock_authorized":false,"automatic_retry":false})
}
pub(super) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    let feedback = service(&state)?;
    let Path((task, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let task = parse_task(&task)?;
    let op = operation(&op)?;
    let input: Action = body(request).await?;
    // On timeout the entire composer is cancelled. COMMIT may be uncertain;
    // report the ORIGINAL references and require GET, never retry a write here.
    let saved = tokio::time::timeout(
        state.0.request_timeout,
        feedback.execute(&identity.reference, &task, &op, &input.confirmation_ref),
    )
    .await
    .map_err(|_| error(StorageError::ReconcileRequired).with_task(&task))?
    .map_err(|e| error(e).with_task(&task))?;
    identity.check(&state)?;
    Ok(Json(result(saved)))
}
pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(&headers)?;
    plain(&uri)?;
    let feedback = service(&state)?;
    let Path(op) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let op = operation(&op)?;
    let saved = tokio::time::timeout(
        state.0.request_timeout,
        feedback.query(&identity.reference, &op),
    )
    .await
    .map_err(|_| ApiError::unavailable())?
    .map_err(error)?
    .ok_or_else(ApiError::not_found)?;
    identity.check(&state)?;
    Ok(Json(result(saved)))
}
