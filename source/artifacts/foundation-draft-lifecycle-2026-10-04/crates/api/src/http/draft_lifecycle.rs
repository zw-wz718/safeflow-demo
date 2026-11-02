use super::*;
use safeflow_contracts::{
    BusinessOperationRef, ConfirmationRef, ContractFuture, ExecutionContext, MockDraftAction,
    OperationId, ResourceId,
};
use safeflow_transfer_integration::{runtime_tools::*, DraftStatus, SystemClock};
use serde_json::{json, Value};

const SCOPE: &str = "MOCK_DRAFT_LIFECYCLE_ONLY";

async fn body<T: DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    .map_err(|_| ApiError::validation(StatusCode::REQUEST_TIMEOUT))?
    .map_err(|_| ApiError::validation(StatusCode::PAYLOAD_TOO_LARGE))?;
    let input = safeflow_runtime::parse_bounded_input_json_for_backend(&bytes)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    serde_json::from_value(input).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}

struct StoreObserver(Arc<MockDraftStore<SystemClock>>);
impl LifecycleDraftObserver for StoreObserver {
    fn restore<'a>(
        &'a self,
        context: &'a ExecutionContext,
        recovery: safeflow_transfer_integration::DraftRestore,
    ) -> ContractFuture<'a, Result<safeflow_transfer_integration::StoredDraft, StorageError>> {
        Box::pin(async move {
            self.0
                .restore_for_backend(context, recovery)
                .await
                .map_err(|_| StorageError::DependencyUnavailable)
        })
    }
    fn stored<'a>(
        &'a self,
        context: &'a ExecutionContext,
        draft_id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<safeflow_transfer_integration::StoredDraft, StorageError>> {
        Box::pin(async move {
            self.0
                .read(context, draft_id)
                .await
                .map_err(|_| StorageError::DependencyUnavailable)
        })
    }

    fn observe<'a>(
        &'a self,
        context: &'a ExecutionContext,
    ) -> ContractFuture<'a, Result<Option<LifecycleDraftObservation>, StorageError>> {
        Box::pin(async move {
            let Some(saved) = self
                .0
                .read_created_for_task(context)
                .await
                .map_err(|_| StorageError::DependencyUnavailable)?
            else {
                return Ok(None);
            };
            if saved.task_id != context.task_id || saved.user_id != context.authenticated_user_id {
                return Err(StorageError::DependencyUnavailable);
            }
            let details = saved.details.ok_or(StorageError::DependencyUnavailable)?;
            Ok(Some(LifecycleDraftObservation {
                draft_id: saved.draft_id,
                business_operation_id: BusinessOperationRef::parse(saved.operation_id.as_str())
                    .map_err(|_| StorageError::DependencyUnavailable)?,
                draft_version: saved.plan_version,
                status: match saved.status {
                    DraftStatus::ReadyToConfirm => "READY_TO_CONFIRM",
                    DraftStatus::Cancelled => "CANCELLED",
                    DraftStatus::Expired => "EXPIRED",
                    DraftStatus::NeedsInformation => "NEEDS_INFORMATION",
                }
                .into(),
                expires_at: chrono::DateTime::from_timestamp(
                    i64::try_from(saved.expires_at)
                        .map_err(|_| StorageError::DependencyUnavailable)?,
                    0,
                )
                .ok_or(StorageError::DependencyUnavailable)?,
                parameters: json!({"payer_account_id":details.account.account_id,"payee_id":details.payee.payee_id,"amount":details.amount,"currency":details.currency,"remark":details.remark}),
            }))
        })
    }
}

pub(super) fn service(state: &ApiState) -> Result<LifecycleService<'_>, ApiError> {
    let draft = state.0.draft.as_ref().ok_or_else(ApiError::disabled)?;
    let runtime = draft
        .lifecycle_runtime
        .as_ref()
        .ok_or_else(ApiError::disabled)?;
    let decision = draft
        .config
        .lifecycle_decision
        .as_ref()
        .ok_or_else(ApiError::disabled)?;
    LifecycleService::for_trusted_backend(
        state.0.repository.clone(),
        runtime.runtime(),
        state.0.sessions.clone(),
        Arc::new(StoreObserver(draft.store.clone())),
        runtime.draft_contracts(),
        decision.clone(),
        draft.config.confirmation.clone(),
    )
    .map(LifecycleService::with_transfer_sql_bridge_for_backend)
    .map_err(error)
}
fn error(error: StorageError) -> ApiError {
    super::draft_confirmation::error(error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScopeRequest {
    contract_version: u16,
    scope: String,
}
fn check_scope(version: u16, scope: &str) -> Result<(), ApiError> {
    if version != 2 || scope != SCOPE {
        Err(ApiError::disabled())
    } else {
        Ok(())
    }
}
fn comparison(revision: u64, version: u32, facts: u64) -> Result<(), ApiError> {
    if !(1..=MAX_SAFE_INTEGER).contains(&revision)
        || version == 0
        || !(1..=u32::MAX as u64).contains(&facts)
    {
        Err(ApiError::validation(StatusCode::BAD_REQUEST))
    } else {
        Ok(())
    }
}
fn parse_operation(input: String) -> Result<OperationId, ApiError> {
    if input.is_empty()
        || input.len() > 200
        || input.trim() != input
        || input.chars().any(char::is_control)
    {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    OperationId::parse(input).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
fn ids(
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<(TaskId, OperationId), ApiError> {
    let Path((task, operation)) =
        path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    Ok((parse_task(&task)?, parse_operation(operation)?))
}
fn fixed_parameters() -> Value {
    json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null})
}

#[derive(Serialize)]
pub(super) struct LifecycleResponse {
    contract_version: u16,
    mode: &'static str,
    scope: &'static str,
    task_id: TaskId,
    task_state: TaskState,
    task_revision: u64,
    plan_version: u32,
    facts_version: u64,
    operation_id: OperationId,
    active_operation_id: OperationId,
    operation_state: OperationState,
    operation_revision: u64,
    operation_plan_version: u32,
    action: &'static str,
    is_current_action: bool,
    parameters: Value,
    observed_draft: Option<LifecycleDraftObservation>,
    target_parameters: Option<Value>,
    selection_ticket: String,
    display_ticket: Option<String>,
    confirmation: Option<PublicConfirmation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmation_recipient: Option<safeflow_storage::ConfirmationRecipient>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    recipient_reconfirmation_required: bool,
    #[serde(skip_serializing_if="Option::is_none")]
    intent_termination: Option<Value>,
    #[serde(skip_serializing_if="Option::is_none")]
    intent_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    execution_status: &'static str,
    unknown_operation_id: Option<OperationId>,
    receipt_scope: Option<&'static str>,
    receipt: Option<PublicReceipt>,
    current_draft_verified: bool,
    next_action: Value,
    replayed: bool,
}
#[derive(Serialize)]
struct PublicConfirmation {
    confirmation_ref: ConfirmationRef,
    status: ConfirmationStatus,
    prepared_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Serialize)]
struct PublicReceipt {
    action: String,
    operation_id: OperationId,
    plan_version: u32,
    expected_draft_version: Option<u32>,
    draft_id: ResourceId,
    business_operation_id: BusinessOperationRef,
    draft_version: u32,
    tool_result_ref: safeflow_contracts::ToolResultRef,
    executed_at: chrono::DateTime<chrono::Utc>,
}
pub(super) fn project(view: LifecycleView) -> Result<LifecycleResponse, ApiError> {
    comparison(
        view.task.revision,
        view.task.plan.plan_version,
        view.task.facts_version,
    )?;
    comparison(
        view.operation.revision,
        view.operation.plan_version,
        view.operation.facts_version,
    )?;
    let next = if view.intent_termination.is_some() {json!({"kind":"FINISHED"})} else if let Some(operation) = &view.unknown_operation {
        json!({"kind":"QUERY_ORIGINAL_OPERATION","operation_id":operation})
    } else if view.task.state.is_terminal() {
        json!({"kind":"FINISHED"})
    } else if !view.is_current_action {
        json!({"kind":"REVIEW_ORIGINAL_TASK"})
    } else if view.recipient_reconfirmation_required {
        json!({"kind":"RECONFIRM_RECIPIENT"})
    } else if view.task.state == TaskState::WaitingUser {
        json!({"kind":"SELECT_ACTION"})
    } else {
        match view.confirmation.as_ref().map(|c| c.status) {
            Some(ConfirmationStatus::Confirmed) => json!({"kind":"EXECUTE"}),
            Some(ConfirmationStatus::Prepared) => json!({"kind":"CONFIRM"}),
            _ => json!({"kind":"PREPARE_OR_REPLACE_ACTION"}),
        }
    };
    let target_parameters = if let Some(original) = &view.observed_draft {
        let mut target = original.parameters.clone();
        if view.action == MockDraftAction::Modify {
            for (key, value) in view.parameters["changes"]
                .as_object()
                .ok_or_else(ApiError::unavailable)?
            {
                target
                    .as_object_mut()
                    .ok_or_else(ApiError::unavailable)?
                    .insert(key.clone(), value.clone());
            }
        }
        Some(target)
    } else {
        None
    };
    Ok(LifecycleResponse {
        contract_version: 2,
        mode: MODE,
        scope: SCOPE,
        task_id: view.task.task_id,
        task_state: view.task.state,
        task_revision: view.task.revision,
        plan_version: view.task.plan.plan_version,
        facts_version: view.task.facts_version,
        operation_id: view.operation.operation_id,
        active_operation_id: view.active_operation_id,
        operation_state: view.operation.state,
        operation_revision: view.operation.revision,
        operation_plan_version: view.operation.plan_version,
        action: view.action.action(),
        is_current_action: view.is_current_action,
        parameters: view.parameters,
        observed_draft: view.observed_draft,
        target_parameters,
        selection_ticket: view.selection_ticket,
        display_ticket: view.display_ticket.clone(),
        recipient_reconfirmation_required: view.recipient_reconfirmation_required,
        intent_termination: view.intent_termination,
        intent_expires_at: view.intent_expires_at,
        confirmation_recipient: if view.confirmation.is_some() && view.display_ticket.is_some() && view.is_current_action && view.unknown_operation.is_none() {view.confirmation_recipient} else {None},
        confirmation: view.confirmation.map(|c| PublicConfirmation {
            confirmation_ref: c.confirmation_ref,
            status: c.status,
            prepared_at: c.prepared_at,
            expires_at: c.expires_at,
        }),
        execution_status: if view.receipt.is_some() {
            "SUCCEEDED"
        } else if view.unknown_operation.is_some() {
            "RESULT_UNKNOWN"
        } else {
            "NOT_CLAIMED"
        },
        unknown_operation_id: view.unknown_operation,
        receipt_scope: view
            .receipt
            .as_ref()
            .map(|_| "HISTORICAL_EXECUTION_RECEIPT"),
        receipt: view.receipt.map(|r| PublicReceipt {
            action: r.action,
            operation_id: r.operation_id,
            plan_version: r.plan_version,
            expected_draft_version: r.expected_draft_version,
            draft_id: r.draft_id,
            business_operation_id: r.business_operation_id,
            draft_version: r.draft_version,
            tool_result_ref: r.tool_result_ref,
            executed_at: r.executed_at,
        }),
        current_draft_verified: false,
        next_action: next,
        replayed: view.replayed,
    })
}

pub(super) async fn create(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ScopeRequest = body(request).await?;
    check_scope(input.contract_version, &input.scope)?;
    let id = new_task_id();
    let outcome = tokio::time::timeout(state.0.request_timeout, async {
        draft_confirmation::candidates(&state, &identity, &id).await?;
        let view = service(&state)?
            .with_payee_query_for_backend(
                ResourceId::parse("payee_008").expect("fixed sample payee"),
                draft_confirmation::FIXED_PAYEE_QUERY.into(),
            )
            .map_err(error)?
            .create(&identity.reference, &id, fixed_parameters(), 7)
            .await
            .map_err(error)?;
        project(view)
    })
    .await;
    finish(&state, &identity, &id, outcome)
}
fn finish(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
    result: Result<Result<LifecycleResponse, ApiError>, tokio::time::error::Elapsed>,
) -> Result<Json<LifecycleResponse>, ApiError> {
    identity.check(state).map_err(|e| e.with_task(id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(id))?
        .map(Json)
        .map_err(|e| e.with_task(id))
}

pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    if uri.query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let Path(raw) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&raw)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        project(
            service(&state)?
                .query(&identity.reference, &id, None)
                .await
                .map_err(error)?,
        )
    })
    .await;
    finish(&state, &identity, &id, result)
}
pub(super) async fn operation_query(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    if uri.query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let (id, operation) = ids(path)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        project(
            service(&state)?
                .query(&identity.reference, &id, Some(&operation))
                .await
                .map_err(error)?,
        )
    })
    .await;
    finish(&state, &identity, &id, result)
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum RequestedAction {
    CreateMockDraft,
    ModifyMockDraft,
    CancelMockDraft,
}
impl RequestedAction {
    fn action(self) -> MockDraftAction {
        match self {
            Self::CreateMockDraft => MockDraftAction::Create,
            Self::ModifyMockDraft => MockDraftAction::Modify,
            Self::CancelMockDraft => MockDraftAction::Cancel,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectRequest {
    contract_version: u16,
    scope: String,
    selection_ticket: String,
    task_revision: u64,
    plan_version: u32,
    facts_version: u64,
    action: RequestedAction,
    parameters: Value,
}
pub(super) async fn select(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: SelectRequest = body(request).await?;
    check_scope(input.contract_version, &input.scope)?;
    comparison(input.task_revision, input.plan_version, input.facts_version)?;
    let Path(raw) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&raw)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        project(
            service(&state)?
                .select(
                    &identity.reference,
                    &id,
                    LifecycleSelection {
                        selection_ticket: input.selection_ticket,
                        task_revision: input.task_revision,
                        plan_version: input.plan_version,
                        facts_version: input.facts_version,
                        action: input.action.action(),
                        parameters: input.parameters,
                    },
                )
                .await
                .map_err(error)?,
        )
    })
    .await;
    finish(&state, &identity, &id, result)
}
pub(super) async fn prepare(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ScopeRequest = body(request).await?;
    check_scope(input.contract_version, &input.scope)?;
    let (id, operation) = ids(path)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        let service = service(&state)?;
        let view = service
            .query(&identity.reference, &id, Some(&operation))
            .await
            .map_err(error)?;
        if view.action == MockDraftAction::Create && view.intent_termination.is_none() {
            draft_confirmation::candidates(&state, &identity, &id).await?;
        }
        project(
            service
                .prepare(&identity.reference, &id, &operation)
                .await
                .map_err(error)?,
        )
    })
    .await;
    finish(&state, &identity, &id, result)
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ConfirmAction {
    Confirm,
    Reject,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmRequest {
    contract_version: u16,
    scope: String,
    confirmation_ref: String,
    display_ticket: String,
    action: ConfirmAction,
}
pub(super) async fn confirm(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ConfirmRequest = body(request).await?;
    check_scope(input.contract_version, &input.scope)?;
    let (id, operation) = ids(path)?;
    let reference = ConfirmationRef::parse(input.confirmation_ref)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    if reference.as_str().len() > 200
        || input.display_ticket.len() != 42
        || !input.display_ticket.starts_with("lifecycle_")
    {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let action = match input.action {
        ConfirmAction::Confirm => PlanningConfirmationAction::Confirm,
        ConfirmAction::Reject => PlanningConfirmationAction::Reject,
    };
    let result = tokio::time::timeout(state.0.request_timeout, async {
        project(
            service(&state)?
                .act(
                    &identity.reference,
                    &id,
                    &operation,
                    &reference,
                    &input.display_ticket,
                    action,
                )
                .await
                .map_err(error)?,
        )
    })
    .await;
    finish(&state, &identity, &id, result)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteRequest {
    contract_version: u16,
    scope: String,
    confirmation_ref: String,
    task_revision: u64,
    operation_revision: u64,
    plan_version: u32,
    facts_version: u64,
}
pub(super) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ExecuteRequest = body(request).await?;
    check_scope(input.contract_version, &input.scope)?;
    comparison(input.task_revision, input.plan_version, input.facts_version)?;
    comparison(
        input.operation_revision,
        input.plan_version,
        input.facts_version,
    )?;
    let reference = ConfirmationRef::parse(input.confirmation_ref)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    if reference.as_str().len() > 200 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let (id, operation) = ids(path)?;
    let expected = MockDraftExecutionVersion {
        task_revision: input.task_revision,
        operation_revision: input.operation_revision,
        plan_version: input.plan_version,
        facts_version: input.facts_version,
    };
    let result = tokio::time::timeout(state.0.request_timeout, async {
        project(
            service(&state)?
                .execute(&identity.reference, &id, &operation, &reference, &expected)
                .await
                .map_err(error)?,
        )
    })
    .await;
    finish(&state, &identity, &id, result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    contract_version:u16, scope:String, acknowledgement:String,
    task_revision:u64,operation_revision:u64,plan_version:u32,facts_version:u64,
}
pub(super) async fn close_nonfunding(
    State(state): State<ApiState>,
    path: Result<Path<(String,String)>,PathRejection>,request:Request,
)->Result<Json<Value>,ApiError>{
    let identity=state.identity(request.headers())?;
    let input:CloseRequest=body(request).await?;
    check_scope(input.contract_version,&input.scope)?;
    if input.acknowledgement!="PRESERVE_UNKNOWN_CLOSE_NONFUNDING_DRAFT" {return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    comparison(input.task_revision,input.plan_version,input.facts_version)?;
    comparison(input.operation_revision,input.plan_version,input.facts_version)?;
    let (id,op)=ids(path)?;
    let expected=MockDraftExecutionVersion{task_revision:input.task_revision,operation_revision:input.operation_revision,plan_version:input.plan_version,facts_version:input.facts_version};
    let result=tokio::time::timeout(state.0.request_timeout,
        service(&state)?.close_unknown_nonfunding_draft(&identity.reference,&id,&op,&expected)).await;
    identity.check(&state).map_err(|e|e.with_task(&id))?;
    Ok(Json(result.map_err(|_|ApiError::unavailable().with_task(&id))?.map_err(error).map_err(|e|e.with_task(&id))?))
}

#[derive(Serialize)]
pub(super) struct CurrentResponse {
    contract_version: u16,
    mode: &'static str,
    scope: &'static str,
    task_id: TaskId,
    operation_id: OperationId,
    current_draft_verified: bool,
    draft: LifecycleDraftObservation,
}
pub(super) async fn current(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<CurrentResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    if uri.query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let (id, operation) = ids(path)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service(&state)?.observe_current(&identity.reference, &id, &operation),
    )
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    let draft = result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map_err(error)
        .map_err(|e| e.with_task(&id))?;
    Ok(Json(CurrentResponse {
        contract_version: 2,
        mode: MODE,
        scope: "MOCK_DRAFT_LIFECYCLE_CURRENT_ONLY",
        task_id: id,
        operation_id: operation,
        current_draft_verified: true,
        draft,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentRequest { contract_version:u16,scope:String,acknowledgement:String,task_revision:u64,operation_revision:u64,plan_version:u32,facts_version:u64,replacement:IntentReplacement }
pub(super) async fn replace_intent(State(state):State<ApiState>,path:Result<Path<(String,String)>,PathRejection>,request:Request)->Result<Json<Value>,ApiError>{
 let identity=state.identity(request.headers())?;let input:IntentRequest=body(request).await?;
 check_scope(input.contract_version,&input.scope)?;
 if input.acknowledgement!="REPLACE_UNEXECUTED_INTENT_ONLY"{return Err(ApiError::validation(StatusCode::BAD_REQUEST))}
 let (id,op)=ids(path)?;comparison(input.task_revision,input.plan_version,input.facts_version)?;comparison(input.operation_revision,input.plan_version,input.facts_version)?;
 let expected=MockDraftExecutionVersion{task_revision:input.task_revision,operation_revision:input.operation_revision,plan_version:input.plan_version,facts_version:input.facts_version};
 let value=service(&state)?.terminate_unexecuted_intent(&identity.reference,&id,&op,&expected,Some(input.replacement)).await.map_err(error)?;identity.check(&state)?;Ok(Json(value))
}
pub(super) async fn expire_intents(State(state):State<ApiState>,request:Request)->Result<Json<Value>,ApiError>{
 let identity=state.identity(request.headers())?;let input:ScopeRequest=body(request).await?;check_scope(input.contract_version,&input.scope)?;
 let value=service(&state)?.expire_unexecuted_intents(&identity.reference).await.map_err(error)?;identity.check(&state)?;Ok(Json(value))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbandonRequest {contract_version:u16,scope:String,acknowledgement:String,task_revision:u64,operation_revision:u64,plan_version:u32,facts_version:u64}
pub(super) async fn abandon_intent(State(state):State<ApiState>,path:Result<Path<(String,String)>,PathRejection>,request:Request)->Result<Json<Value>,ApiError>{
 let identity=state.identity(request.headers())?;let input:AbandonRequest=body(request).await?;check_scope(input.contract_version,&input.scope)?;
 if input.acknowledgement!="END_THIS_PROVEN_UNPAID_INTENT_ONLY"{return Err(ApiError::validation(StatusCode::BAD_REQUEST))}
 let (id,op)=ids(path)?;comparison(input.task_revision,input.plan_version,input.facts_version)?;comparison(input.operation_revision,input.plan_version,input.facts_version)?;
 let expected=MockDraftExecutionVersion{task_revision:input.task_revision,operation_revision:input.operation_revision,plan_version:input.plan_version,facts_version:input.facts_version};
 let value=service(&state)?.abandon_unexecuted_intent(&identity.reference,&id,&op,&expected).await.map_err(error)?;identity.check(&state)?;Ok(Json(value))
}
