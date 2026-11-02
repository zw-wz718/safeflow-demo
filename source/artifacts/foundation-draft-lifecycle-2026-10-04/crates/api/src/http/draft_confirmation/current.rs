//! Current in-process draft observation. It never authorizes lifecycle execution.
use super::*;
use safeflow_contracts::{BusinessOperationRef, ResourceId};
use safeflow_transfer_integration::{DraftStatus, StoredDraft};

#[derive(Serialize)]
pub(in crate::http) struct CurrentDraftResponse {
    contract_version: u16,
    mode: &'static str,
    scope: &'static str,
    task_id: TaskId,
    operation_id: OperationId,
    current_draft_verified: bool,
    draft: CurrentDraft,
    lifecycle_execution_enabled: bool,
    lifecycle_unavailable_reason: &'static str,
}

#[derive(Serialize)]
struct CurrentDraft {
    draft_id: ResourceId,
    business_operation_id: BusinessOperationRef,
    draft_version: u32,
    status: &'static str,
    expires_at: chrono::DateTime<chrono::Utc>,
}

fn project_current(
    view: safeflow_storage::MockDraftExecutionView,
    saved: StoredDraft,
) -> Result<CurrentDraftResponse, ApiError> {
    if saved.task_id != view.task.task_id
        || saved.user_id != view.task.owner_id
        || saved.plan_version == 0
        || saved.expires_at <= 0
    {
        return Err(ApiError::unavailable());
    }
    if let Some(receipt) = &view.receipt {
        if saved.draft_id != receipt.draft_id
            || saved.operation_id.as_str() != receipt.business_operation_id.as_str()
            || saved.plan_version < receipt.draft_version
        {
            return Err(ApiError::unavailable());
        }
    }
    Ok(CurrentDraftResponse {
        contract_version: 1,
        mode: MODE,
        scope: "MOCK_DRAFT_CURRENT_STATE_ONLY",
        task_id: view.task.task_id,
        operation_id: view.operation.operation_id,
        current_draft_verified: true,
        draft: CurrentDraft {
            draft_id: saved.draft_id,
            business_operation_id: BusinessOperationRef::parse(saved.operation_id.as_str())
                .map_err(|_| ApiError::unavailable())?,
            draft_version: saved.plan_version,
            status: match saved.status {
                DraftStatus::ReadyToConfirm => "READY_TO_CONFIRM",
                DraftStatus::NeedsInformation => "NEEDS_INFORMATION",
                DraftStatus::Expired => "EXPIRED",
                DraftStatus::Cancelled => "CANCELLED",
            },
            expires_at: chrono::DateTime::from_timestamp(
                i64::try_from(saved.expires_at).map_err(|_| ApiError::unavailable())?,
                0,
            )
            .ok_or_else(ApiError::unavailable)?,
        },
        lifecycle_execution_enabled: false,
        lifecycle_unavailable_reason: "DEPENDENCY_UNAVAILABLE",
    })
}

pub(in crate::http) async fn current_query(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<CurrentDraftResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    if uri.query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let Path((raw_task, raw_operation)) =
        path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let task = parse_task(&raw_task)?;
    let operation = OperationId::parse(raw_operation)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    if operation.as_str().len() > 200 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let result = tokio::time::timeout(state.0.request_timeout, async {
        let view = service(&state)?
            .query_mock_draft_execution(&identity.reference, &task, &operation)
            .await
            .map_err(error)?;
        identity.check(&state)?;
        let context = ExecutionContext {
            task_id: view.task.task_id.clone(),
            node_id: view.operation.node_id.clone(),
            operation_id: view.operation.operation_id.clone(),
            authenticated_user_id: identity.binding.user_id.clone(),
            plan_version: view.task.plan.plan_version,
            idempotency_key: None,
        };
        // A committed claim can have saved a draft without a settled receipt.
        // Only the actual store can answer; absence never falls back to history.
        let saved = state
            .0
            .draft
            .as_ref()
            .ok_or_else(ApiError::disabled)?
            .store
            .read_created_for_task(&context)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::unavailable)?;
        identity.check(&state)?;
        project_current(view, saved)
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&task))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&task))?
        .map(Json)
        .map_err(|e| e.with_task(&task))
}
