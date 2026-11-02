use super::*;
use safeflow_storage::{
    HistoricalMockDraftReceipt, MockDraftExecutionVersion, MockDraftExecutionView,
};

const EXECUTION_SCOPE: &str = "MOCK_DRAFT_EXECUTION_ONLY";

pub(in crate::http) async fn checked_execution_task(
    state: &ApiState,
    identity: &Identity,
    view: &TaskProgressView,
) -> Result<bool, ApiError> {
    let [operation] = view.operations.as_slice() else {
        return Ok(false);
    };
    if state.0.draft.is_none() {
        return Ok(false);
    }
    let execution = match service(state)?
        .query_mock_draft_execution(&identity.reference, &view.task_id, &operation.operation_id)
        .await
    {
        Ok(execution) => execution,
        Err(StorageError::NotFound) => return Ok(false),
        Err(failure) => return Err(error(failure)),
    };
    if execution.task.task_id != view.task_id
        || execution.task.state != view.task_state
        || execution.task.revision != view.revision
        || execution.task.plan.plan_version != view.plan_version
        || execution.task.facts_version != view.facts_version
        || execution.operation.operation_id != operation.operation_id
        || execution.operation.node_id != operation.node_id
        || execution.operation.state != operation.operation_state
        || execution.operation.revision != operation.revision
        || execution.operation.plan_version != operation.plan_version
        || execution.operation.facts_version != operation.facts_version
    {
        return Err(ApiError::conflict());
    }
    identity.check(state)?;
    Ok(true)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteRequest {
    scope: String,
    operation_id: OperationId,
    confirmation_ref: ConfirmationRef,
    task_revision: u64,
    operation_revision: u64,
    plan_version: u32,
    facts_version: u64,
}
#[derive(Serialize)]
pub(in crate::http) struct ExecutionResponse {
    contract_version: u16,
    mode: &'static str,
    scope: &'static str,
    task_id: TaskId,
    task_state: TaskState,
    operation_id: OperationId,
    operation_state: OperationState,
    task_revision: u64,
    operation_revision: u64,
    plan_version: u32,
    facts_version: u64,
    execution_status: &'static str,
    receipt_scope: &'static str,
    current_draft_verified: bool,
    receipt: Option<HistoricalMockDraftReceipt>,
    replayed: bool,
    next_action: NextAction,
}
fn project_execution(view: MockDraftExecutionView) -> Result<ExecutionResponse, ApiError> {
    if view.task.revision == 0
        || view.task.revision > MAX_SAFE_INTEGER
        || view.operation.revision == 0
        || view.operation.revision > MAX_SAFE_INTEGER
        || !(1..=u32::MAX as u64).contains(&view.task.facts_version)
        || view.task.plan.plan_version == 0
    {
        return Err(ApiError::unavailable());
    }
    let completed = view.receipt.is_some();
    if completed
        && (view.task.state != TaskState::Completed
            || view.operation.state != OperationState::Succeeded)
        || !completed
            && (view.task.state != TaskState::Running
                || view.operation.state != OperationState::Draft)
    {
        return Err(ApiError::unavailable());
    }
    Ok(ExecutionResponse {
        contract_version: 1,
        mode: MODE,
        scope: EXECUTION_SCOPE,
        task_id: view.task.task_id,
        task_state: view.task.state,
        operation_id: view.operation.operation_id,
        operation_state: view.operation.state,
        task_revision: view.task.revision,
        operation_revision: view.operation.revision,
        plan_version: view.task.plan.plan_version,
        facts_version: view.task.facts_version,
        execution_status: if completed {
            "COMPLETED"
        } else {
            "REQUIRES_ORIGINAL_OPERATION_REVIEW"
        },
        receipt_scope: "HISTORICAL_EXECUTION_RECEIPT",
        current_draft_verified: false,
        receipt: view.receipt,
        replayed: view.replayed,
        next_action: NextAction {
            kind: if completed {
                "NONE"
            } else {
                "QUERY_ORIGINAL_OPERATION"
            },
        },
    })
}
pub(in crate::http) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<ExecutionResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ExecuteRequest = body(request).await?;
    let Path(raw_task) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let task = parse_task(&raw_task)?;
    if input.scope != EXECUTION_SCOPE {
        return Err(ApiError::disabled().with_task(&task));
    }
    if input.task_revision == 0
        || input.task_revision > MAX_SAFE_INTEGER
        || input.operation_revision == 0
        || input.operation_revision > MAX_SAFE_INTEGER
        || input.plan_version == 0
        || !(1..=u32::MAX as u64).contains(&input.facts_version)
        || input.operation_id.as_str().len() > 200
        || input.confirmation_ref.as_str().len() > 200
    {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST).with_task(&task));
    }
    let service = service(&state)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service.execute_confirmed_mock_draft(
            &identity.reference,
            &task,
            &input.operation_id,
            &input.confirmation_ref,
            &MockDraftExecutionVersion {
                task_revision: input.task_revision,
                operation_revision: input.operation_revision,
                plan_version: input.plan_version,
                facts_version: input.facts_version,
            },
        ),
    )
    .await;
    identity
        .check(&state)
        .map_err(|error| error.with_task(&task))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&task))?
        .map_err(error)
        .and_then(project_execution)
        .map(Json)
        .map_err(|error| error.with_task(&task))
}
pub(in crate::http) async fn execution_query(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<ExecutionResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path((raw_task, raw_operation)) =
        path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let task = parse_task(&raw_task)?;
    let operation = OperationId::parse(raw_operation)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    if operation.as_str().len() > 200 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service(&state)?.query_mock_draft_execution(&identity.reference, &task, &operation),
    )
    .await;
    identity
        .check(&state)
        .map_err(|error| error.with_task(&task))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&task))?
        .map_err(error)
        .and_then(project_execution)
        .map(Json)
        .map_err(|error| error.with_task(&task))
}
