//! Explicit customer command over the reviewed closed payment composer.
use super::*;
use safeflow_contracts::{ArgumentsHash, ConfirmationRef, OperationId};
use safeflow_runtime::ToolSecurityProfile;
use safeflow_transfer_integration::mock_execution::{MockBehavior, MockTransferRequest};

const SCOPE: &str = "PAYMENT_EXECUTION_ONLY";

/// Deployment code only. No Deserialize, environment/file loader or request-selected policy.
pub struct PaymentExecutionConfig {
    pub(super) source: Option<CardCatalogSourceBinding>,
    pub(super) profile: ToolSecurityProfile,
    pub(super) permit: PermitConfig,
    pub(super) behavior: MockBehavior,
}
impl PaymentExecutionConfig {
    pub fn for_reviewed_project_simulation(
        source: Option<CardCatalogSourceBinding>,
        profile: ToolSecurityProfile,
        permit: PermitConfig,
        behavior: MockBehavior,
    ) -> Result<Self, ApiStartupError> {
        payment_final_registration_for_backend(profile.clone())
            .map_err(|_| ApiStartupError("invalid reviewed final payment profile"))?;
        Ok(Self {
            source,
            profile,
            permit,
            behavior,
        })
    }
}

#[derive(Serialize)]
pub(super) struct ExecutionCapability {
    enabled: bool,
    reason: &'static str,
    execute_path: String,
    result_path: String,
    automatic_retry: bool,
}
#[derive(Serialize)]
pub(super) struct PaymentView {
    #[serde(flatten)]
    pub(super) payment: PaymentPreparationView,
    execution: ExecutionCapability,
}
pub(super) fn enabled(state: &ApiState) -> bool {
    state
        .0
        .payment_execution
        .as_ref()
        .is_some_and(|c| c.source.is_some())
        && state
            .0
            .draft
            .as_ref()
            .is_some_and(|d| d.lifecycle_runtime.is_some())
}
pub(super) fn project(state: &ApiState, mut payment: PaymentPreparationView) -> PaymentView {
    let enabled = enabled(state);
    payment.funds_execution_enabled = enabled;
    if enabled {
        payment.execution_conditions.clear();
    }
    let base = format!(
        "/api/sim/payment-preparations/{}/{}",
        payment.task_id, payment.operation.operation_id
    );
    PaymentView {
        payment,
        execution: ExecutionCapability {
            enabled,
            reason: if enabled {
                "RECHECKED_AT_EXPLICIT_EXECUTION"
            } else {
                "PAYMENT_DEPENDENCY_NOT_ADOPTED"
            },
            execute_path: format!("{base}/execute"),
            result_path: format!("{base}/funds-result"),
            automatic_retry: false,
        },
    }
}

/// No internal keys, permit, fingerprint, authority, source or business-store record.
#[derive(Serialize)]
pub(super) struct FundsResult {
    contract_version: u16,
    scope: &'static str,
    task_id: TaskId,
    preparation_operation_id: OperationId,
    execution_operation_id: Option<OperationId>,
    snapshot_digest: ArgumentsHash,
    transfer: MockTransferRequest,
    state: PaymentOriginalState,
    submitted: bool,
    automatic_retry: bool,
}
fn project_original(o: PaymentOriginalResult) -> FundsResult {
    FundsResult {
        contract_version: 1,
        scope: "PAYMENT_FUNDS_RESULT_ONLY",
        task_id: o.task_id,
        preparation_operation_id: o.binding.preparation_operation_id,
        execution_operation_id: o.binding.execution_operation_id,
        snapshot_digest: o.binding.preparation_arguments_hash,
        transfer: o.binding.normalized_arguments,
        state: o.state,
        submitted: o.first_public_submission.is_some() || o.verified_business_result.is_some(),
        automatic_retry: false,
    }
}
pub(super) async fn read(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
    op: &OperationId,
) -> Result<FundsResult, ApiError> {
    let result = PaymentOriginalReader::for_trusted_backend(
        state.0.repository.clone(),
        state.0.sessions.clone(),
    )
    .query(&identity.reference, id, op)
    .await
    .map_err(payment_preparation::error)?;
    identity.check(state)?;
    Ok(project_original(result))
}
pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<FundsResult>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path((id, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let op = payment_preparation::operation(&op)?;
    if uri.query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    tokio::time::timeout(state.0.request_timeout, read(&state, &identity, &id, &op))
        .await
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    contract_version: u16,
    scope: String,
    confirmation_ref: ConfirmationRef,
    display_ticket: String,
}

pub(super) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<FundsResult>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path((id, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let op = payment_preparation::operation(&op)?;
    if request.uri().query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let body: Command = payment_preparation::input(request).await?;
    if body.contract_version != 1 || body.scope != SCOPE || body.display_ticket.len() > 200 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    if !enabled(&state) {
        return Err(ApiError::disabled().with_task(&id));
    }
    let config = state
        .0
        .payment_execution
        .as_ref()
        .ok_or_else(ApiError::disabled)?;
    let runtime = state
        .0
        .draft
        .as_ref()
        .and_then(|d| d.lifecycle_runtime.as_ref())
        .ok_or_else(ApiError::disabled)?;
    let service = PaymentExecutionService::for_project_simulation(
        state.0.repository.clone(),
        state.0.sessions.clone(),
        runtime.runtime(),
        config.source.clone(),
        config.profile.clone(),
        config.permit.clone(),
        config.behavior,
    )
    .map_err(payment_preparation::error)?;
    // Timeout may race with COMMIT. Every uncertain response requires GET of the
    // original; no retry, success callback or inferred failure is scheduled here.
    tokio::time::timeout(state.0.request_timeout, async {
        service
            .execute(
                &identity.reference,
                &id,
                &op,
                &body.confirmation_ref,
                &body.display_ticket,
            )
            .await
            .map_err(payment_preparation::error)?;
        read(&state, &identity, &id, &op).await
    })
    .await
    .map_err(|_| ApiError::unavailable().with_task(&id))?
    .map(Json)
    .map_err(|e| e.with_task(&id))
}
