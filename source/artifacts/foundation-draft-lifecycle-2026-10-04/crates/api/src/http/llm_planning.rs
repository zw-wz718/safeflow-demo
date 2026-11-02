//! Local-session planning only. Explicit original-task execution remains read-only.
use super::*;
use safeflow_contracts::Clarification;
use safeflow_runtime::{PlanningGatewaySource, PlanningIntakeError};

#[derive(Serialize)]
pub(super) struct LlmStatus {
    contract_version: u16,
    mode: &'static str,
    planner_source: &'static str,
    configured: bool,
    connectivity: &'static str,
    reason_code: Option<&'static str>,
    tool_scope: &'static str,
}
pub(super) async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<LlmStatus>, ApiError> {
    state.identity(&headers)?.check(&state)?;
    let configured = state.0.llm_gateway.is_some();
    Ok(Json(LlmStatus {
        contract_version: 1,
        mode: MODE,
        planner_source: "OPENAI_COMPATIBLE",
        configured,
        connectivity: "NOT_VERIFIED",
        reason_code: if configured {
            None
        } else {
            Some("LLM_NOT_CONFIGURED")
        },
        tool_scope: "READ_ONLY_BALANCE",
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRequest {
    message: String,
}
#[derive(Serialize)]
pub(super) struct LlmPlanResponse {
    contract_version: u16,
    mode: &'static str,
    planner_source: &'static str,
    kind: &'static str,
    task: Option<TaskResponse>,
    clarification: Option<Clarification>,
    can_start: bool,
}
impl LlmPlanResponse {
    fn task(task: TaskResponse, can_start: bool) -> Self {
        Self {
            contract_version: 1,
            mode: MODE,
            planner_source: "OPENAI_COMPATIBLE",
            kind: "TASK",
            task: Some(task),
            clarification: None,
            can_start,
        }
    }
    fn clarification(clarification: Clarification) -> Self {
        Self {
            contract_version: 1,
            mode: MODE,
            planner_source: "OPENAI_COMPATIBLE",
            kind: "CLARIFY",
            task: None,
            clarification: Some(clarification),
            can_start: false,
        }
    }
}
fn error(status: StatusCode, reason: &'static str) -> ApiError {
    ApiError {
        status,
        reason,
        task_id: None,
    }
}
fn creation_error(error_value: PlanningTaskCreationError) -> ApiError {
    match error_value {
        PlanningTaskCreationError::Storage(error) => ApiError::storage(error),
        PlanningTaskCreationError::Intake(PlanningIntakeError::Session) => {
            ApiError::unauthenticated()
        }
        PlanningTaskCreationError::Intake(PlanningIntakeError::GatewayNotConfigured) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "LLM_NOT_CONFIGURED")
        }
        PlanningTaskCreationError::Intake(PlanningIntakeError::GatewayFailed) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "LLM_GATEWAY_UNAVAILABLE")
        }
        PlanningTaskCreationError::Intake(_) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, "LLM_PROPOSAL_REJECTED")
        }
    }
}
pub(super) async fn create(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<LlmPlanResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: PlanRequest = body(request).await?;
    identity.check(&state)?;
    if input.message.trim().is_empty() || input.message.len() > 4096 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let gateway = state
        .0
        .llm_gateway
        .clone()
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "LLM_NOT_CONFIGURED"))?;
    let deadline = gateway.timeout() + Duration::from_secs(10);
    let id = new_task_id();
    let result = tokio::time::timeout(deadline, async {
        let service = PlanningTaskCreationService::for_trusted_backend(
            state.0.repository.clone(),
            state.0.runtime.runtime(),
            Some(gateway),
            state.0.runtime.contracts(),
        )
        .map_err(creation_error)?;
        // Initial public planning versions, not a claim of any bank data version.
        let created = service
            .plan_and_create(&identity.reference, &id, 1, 1, &input.message)
            .await
            .map_err(|e| {
                let storage_may_have_been_reached =
                    matches!(&e, PlanningTaskCreationError::Storage(_));
                let error = creation_error(e);
                if storage_may_have_been_reached {
                    error.with_task(&id)
                } else {
                    error
                }
            })?;
        identity.check(&state)?;
        match created {
            PlanningTaskCreationOutcome::TaskCreated(_) => checked_task(&state, &identity, &id)
                .await
                .map_err(|e| e.with_task(&id)),
            PlanningTaskCreationOutcome::Clarification(result) => {
                Ok(LlmPlanResponse::clarification(
                    result.clarification.ok_or_else(ApiError::unavailable)?,
                ))
            }
        }
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    // Once storage may have been reached, retain the original id for reconciliation.
    result
        .map_err(|_| {
            error(StatusCode::SERVICE_UNAVAILABLE, "LLM_GATEWAY_UNAVAILABLE").with_task(&id)
        })?
        .map(Json)
}
async fn checked_task(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
) -> Result<LlmPlanResponse, ApiError> {
    identity.check(state)?;
    let provenance = TaskQueryService::for_trusted_backend(
        state.0.repository.clone(),
        state.0.runtime.runtime(),
    )
    .planning_gateway_provenance(&identity.reference, id)
    .await
    .map_err(ApiError::storage)?;
    identity.check(state)?;
    if provenance.source() != Some(PlanningGatewaySource::OpenaiCompatible) {
        return Err(ApiError::not_found());
    }
    let registration = safeflow_runtime::PlanningIntakeService::for_registered_backend(
        state.0.runtime.runtime(),
        None,
        state.0.runtime.contracts(),
    )
    .map_err(|_| ApiError::unavailable())?
    .registration_hash_for_backend()
    .map_err(|_| ApiError::unavailable())?;
    let (task, can_start) = state.response_with_start(identity, id, false).await?;
    Ok(LlmPlanResponse::task(
        task,
        can_start && provenance.matches_current_registration(&registration),
    ))
}
pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<LlmPlanResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&value)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        checked_task(&state, &identity, &id),
    )
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteRequest {
    task_revision: u64,
    plan_version: u32,
    facts_version: u64,
}
pub(super) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<LlmPlanResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&value)?;
    let input: ExecuteRequest = body(request).await?;
    if input.task_revision == 0
        || input.task_revision > MAX_SAFE_INTEGER
        || input.plan_version == 0
        || !(1..=u32::MAX as u64).contains(&input.facts_version)
    {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        checked_task(&state, &identity, &id).await?;
        RuntimeTaskExecutionService::for_trusted_backend(
            state.0.repository.clone(),
            state.0.runtime.runtime(),
            state.0.runtime.contracts(),
        )
        .map_err(ApiError::execution)?
        .schedule(
            &identity.reference,
            &id,
            RuntimeTaskVersion {
                task_revision: input.task_revision,
                plan_version: input.plan_version,
                facts_version: input.facts_version,
            },
        )
        .await
        .map_err(ApiError::execution)?;
        checked_task(&state, &identity, &id).await
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
