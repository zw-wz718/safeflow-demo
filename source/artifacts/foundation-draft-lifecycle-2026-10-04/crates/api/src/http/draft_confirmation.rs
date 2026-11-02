use super::*;
mod current;
mod execution;
pub(super) use current::current_query;
pub(super) use execution::{checked_execution_task, execute, execution_query};
use safeflow_contracts::{
    AgentPlan, ConfirmationRef, ExecutionContext, OperationId, PlanNode, PlanningResult,
    SecurityGate, ToolName, Understanding, UnderstandingDecision, MOCK_DRAFT_CREATE_TOOL,
};
use safeflow_runtime::{InMemoryAuditRecorder, MockDraftAuthority, MockDraftClock};
use safeflow_transfer_integration::{runtime_tools::*, SystemClock};
use serde_json::json;

const SCOPE: &str = "MOCK_DRAFT_CONFIRMATION_ONLY";
pub(super) const FIXED_PAYEE_QUERY: &str = "小李";
#[derive(Clone)]
pub struct DraftConfirmationConfig {
    decision: DecisionConfig,
    pub(in crate::http) confirmation: ConfirmationConfig,
    draft_store_ttl_secs: u64,
    decision_ttl: Duration,
    deployment_revision: String,
    pub(in crate::http) lifecycle_decision: Option<DecisionConfig>,
}
impl DraftConfirmationConfig {
    /// Explicit simulation settings. These lifetimes are not production policy defaults.
    pub fn new(
        confirmation_ttl: Duration,
        decision_ttl: Duration,
        draft_store_ttl_secs: u64,
        deployment_revision: &str,
    ) -> Result<Self, ApiStartupError> {
        if draft_store_ttl_secs == 0 {
            return Err(ApiStartupError("draft store lifetime required"));
        }
        Ok(Self {
            decision: DecisionConfig::new(
                decision_ttl,
                deployment_revision,
                assembly::AssemblyScope::LegacyDraft.contracts(),
            )
            .map_err(|_| ApiStartupError("invalid draft decision configuration"))?,
            confirmation: ConfirmationConfig::new(confirmation_ttl)
                .map_err(|_| ApiStartupError("invalid confirmation configuration"))?,
            draft_store_ttl_secs,
            decision_ttl,
            deployment_revision: deployment_revision.into(),
            lifecycle_decision: None,
        })
    }
    pub fn with_lifecycle_v2(mut self) -> Result<Self, ApiStartupError> {
        self.lifecycle_decision = Some(
            DecisionConfig::new(
                self.decision_ttl,
                &self.deployment_revision,
                assembly::AssemblyScope::AgentLifecycle.draft_contracts(),
            )
            .map_err(|_| ApiStartupError("invalid lifecycle decision configuration"))?,
        );
        Ok(self)
    }
}
struct AuthorityClock;
impl MockDraftClock for AuthorityClock {
    fn now_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|v| v.as_secs())
            .unwrap_or(0)
    }
    fn now_millis(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
    }
}
pub(super) struct DraftAssembly {
    runtime: assembly::CheckedAssembly,
    pub(in crate::http) store: Arc<MockDraftStore<SystemClock>>,
    pub(in crate::http) config: DraftConfirmationConfig,
    pub(in crate::http) lifecycle_runtime: Option<assembly::CheckedAssembly>,
}
pub(super) fn assemble(
    sessions: Arc<SimulationSessions>,
    audit: Arc<InMemoryAuditRecorder>,
    bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
    config: DraftConfirmationConfig,
    registrations: &[crate::ReadOnlyToolRegistration],
) -> Result<DraftAssembly, ApiStartupError> {
    let store = Arc::new(
        MockDraftStore::new(bank.clone(), SystemClock, config.draft_store_ttl_secs)
            .map_err(|_| ApiStartupError("existing draft store unavailable"))?,
    );
    let authority = Arc::new(MockDraftAuthority::new(
        sessions.clone(),
        Arc::new(AuthorityClock),
    ));
    let runtime = assembly::assemble(
        assembly::AssemblyScope::LegacyDraft,
        sessions.clone(),
        audit.clone(),
        bank.clone(),
        Some((store.clone(), authority.clone())),
    )
    .map_err(ApiStartupError)?;
    let lifecycle_runtime = if config.lifecycle_decision.is_some() {
        Some(
            assembly::assemble_with_read_only(
                assembly::AssemblyScope::AgentLifecycle,
                sessions,
                audit,
                bank,
                Some((store.clone(), authority)),
                registrations,
            )
            .map_err(ApiStartupError)?,
        )
    } else {
        None
    };
    Ok(DraftAssembly {
        runtime,
        store,
        config,
        lifecycle_runtime,
    })
}
fn parameters() -> serde_json::Value {
    json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00",
        "currency":"CNY","remark":"","replaces":null})
}
fn proposal(id: &TaskId) -> PlanningResult {
    PlanningResult::for_plan(
        Understanding {
            intent: "MOCK_DRAFT_CREATE".into(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        AgentPlan::new(
            id.clone(),
            1,
            vec![PlanNode::Tool {
                id: NodeId::parse("prepare_draft").expect("fixed node"),
                tool: ToolName::parse(MOCK_DRAFT_CREATE_TOOL).expect("existing Tool"),
                arguments: parameters(),
                depends_on: vec![],
            }],
        )
        .expect("single fixed creation plan"),
    )
}
fn service(state: &ApiState) -> Result<PlanningConfirmationService<'_>, ApiError> {
    let draft = state.0.draft.as_ref().ok_or_else(ApiError::disabled)?;
    PlanningConfirmationService::for_trusted_backend(
        state.0.repository.clone(),
        draft.runtime.runtime(),
        state.0.sessions.clone(),
        draft.runtime.contracts(),
        draft.config.decision.clone(),
        draft.config.confirmation.clone(),
    )
    .map_err(error)
}
pub(super) async fn candidates(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
) -> Result<(), ApiError> {
    identity.check(state)?;
    let context = ExecutionContext {
        task_id: id.clone(),
        node_id: NodeId::parse("prepare_draft").expect("fixed node"),
        operation_id: new_operation_id(),
        authenticated_user_id: identity.binding.user_id.clone(),
        plan_version: 1,
        idempotency_key: None,
    };
    let output = state
        .0
        .draft
        .as_ref()
        .ok_or_else(ApiError::disabled)?
        .store
        .find_candidates(&context, FIXED_PAYEE_QUERY)
        .await
        .map_err(|_| ApiError::unavailable())?;
    identity.check(state)?;
    if !output
        .candidates
        .iter()
        .any(|payee| payee.payee_id.as_str() == "payee_008")
    {
        return Err(ApiError::unavailable());
    }
    Ok(())
}
#[derive(Serialize)]
pub(super) struct DraftResponse {
    contract_version: u16,
    mode: &'static str,
    scope: &'static str,
    task_id: TaskId,
    task_state: TaskState,
    operation_id: Option<OperationId>,
    operation_state: Option<OperationState>,
    task_revision: u64,
    operation_revision: Option<u64>,
    plan_version: u32,
    facts_version: u64,
    parameters: serde_json::Value,
    confirmation: Option<PublicConfirmation>,
    preflight: Option<PublicPreflight>,
    display_ticket: Option<String>,
    next_action: NextAction,
}
#[derive(Serialize)]
struct PublicConfirmation {
    confirmation_ref: ConfirmationRef,
    status: ConfirmationStatus,
    prepared_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
    confirmed_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}
#[derive(Serialize)]
struct PublicPreflight {
    evidence_kind: &'static str,
    pre_tool_outcome: SecurityOutcome,
    pre_auth_outcome: SecurityOutcome,
    required_gates: Vec<SecurityGate>,
}
#[derive(Serialize)]
struct NextAction {
    kind: &'static str,
}
fn project(view: PlanningConfirmationView) -> Result<DraftResponse, ApiError> {
    if view.task.state != TaskState::Running
        || view.parameters != parameters()
        || view.task.revision == 0
        || view.task.revision > MAX_SAFE_INTEGER
        || view.task.plan.plan_version == 0
        || !(1..=u32::MAX as u64).contains(&view.task.facts_version)
        || view.operation.as_ref().is_some_and(|o| {
            o.state != OperationState::Draft || o.revision == 0 || o.revision > MAX_SAFE_INTEGER
        })
    {
        return Err(ApiError::unavailable());
    }
    let next = match view.confirmation.as_ref().map(|c| c.status) {
        None => "PREPARE",
        Some(ConfirmationStatus::Prepared) => "CONFIRM",
        Some(ConfirmationStatus::Confirmed | ConfirmationStatus::Revoked) => "NONE",
    };
    let confirmation = view.confirmation.map(|c| PublicConfirmation {
        confirmation_ref: c.confirmation_ref,
        status: c.status,
        prepared_at: c.prepared_at,
        expires_at: c.expires_at,
        confirmed_at: c.confirmed_at,
        revoked_at: c.revoked_at,
    });
    let preflight = match (view.pre_tool, view.pre_auth) {
        (Some(a), Some(b)) => Some(PublicPreflight {
            evidence_kind: "PREFLIGHT_ONLY",
            pre_tool_outcome: a.decision.outcome,
            pre_auth_outcome: b.decision.outcome,
            required_gates: a.decision.required_gates,
        }),
        (None, None) if confirmation.is_none() => None,
        _ => return Err(ApiError::unavailable()),
    };
    Ok(DraftResponse {
        contract_version: 1,
        mode: MODE,
        scope: SCOPE,
        task_id: view.task.task_id,
        task_state: view.task.state,
        operation_id: view.operation.as_ref().map(|o| o.operation_id.clone()),
        operation_state: view.operation.as_ref().map(|o| o.state),
        task_revision: view.task.revision,
        operation_revision: view.operation.as_ref().map(|o| o.revision),
        plan_version: view.task.plan.plan_version,
        facts_version: view.task.facts_version,
        parameters: view.parameters,
        confirmation,
        preflight,
        display_ticket: view.display_ticket,
        next_action: NextAction { kind: next },
    })
}
pub(super) fn error(value: StorageError) -> ApiError {
    match value {
        StorageError::ConfirmationExpired
        | StorageError::ConfirmationRevoked
        | StorageError::ConfirmationBindingMismatch
        | StorageError::DecisionBindingMismatch
        | StorageError::DecisionAssemblyChanged
        | StorageError::DecisionExpired
        | StorageError::DecisionInactive => ApiError {
            status: StatusCode::CONFLICT,
            reason: "STALE_PLAN",
            task_id: None,
        },
        StorageError::DecisionConflict => ApiError::conflict(),
        StorageError::RuntimeIntakeRequired => ApiError::forbidden(),
        _ => ApiError::storage(value),
    }
}
pub(super) async fn create(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<DraftResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let _: Empty = body(request).await?;
    identity.check(&state)?;
    let service = service(&state)?;
    let id = new_task_id();
    let result = tokio::time::timeout(state.0.request_timeout, async {
        let draft = state.0.draft.as_ref().ok_or_else(ApiError::disabled)?;
        let intake = safeflow_runtime::PlanningIntakeService::for_trusted_backend(
            draft.runtime.runtime(),
            Some(Arc::new(MockLlm::new(proposal(&id)))),
            draft.runtime.contracts(),
        )
        .map_err(|_| ApiError::unavailable())?;
        let review = intake
            .plan_for_persistence(
                &identity.reference,
                &id,
                1,
                "向已核对的收款人准备200元模拟草案",
            )
            .await
            .map_err(|value| {
                if value == safeflow_runtime::PlanningIntakeError::Session {
                    ApiError::unauthenticated()
                } else {
                    ApiError::unavailable()
                }
            })?;
        service.create_review(review, 7).await.map_err(error)?;
        candidates(&state, &identity, &id).await?;
        project(
            service
                .prepare(&identity.reference, &id)
                .await
                .map_err(error)?,
        )
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<DraftResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path(id) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let service = service(&state)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service.query(&identity.reference, &id),
    )
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map_err(error)
        .and_then(project)
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
pub(super) async fn prepare(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<DraftResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path(id) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let _: Empty = body(request).await?;
    let service = service(&state)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        service
            .query(&identity.reference, &id)
            .await
            .map_err(error)?;
        candidates(&state, &identity, &id).await?;
        project(
            service
                .prepare(&identity.reference, &id)
                .await
                .map_err(error)?,
        )
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmRequest {
    scope: Option<String>,
    task_id: Option<String>,
    confirmation_ref: Option<String>,
    display_ticket: Option<String>,
    action: Option<ConfirmAction>,
}
#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ConfirmAction {
    Confirm,
    Reject,
}
pub(super) async fn confirm(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<DraftResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ConfirmRequest = body(request).await?;
    if input.scope.as_deref() != Some(SCOPE) {
        return Err(ApiError::disabled());
    }
    let task_id = input
        .task_id
        .ok_or_else(|| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let confirmation_ref = input
        .confirmation_ref
        .ok_or_else(|| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let display_ticket = input
        .display_ticket
        .ok_or_else(|| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let action = input
        .action
        .ok_or_else(|| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let Path(operation) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let operation =
        OperationId::parse(operation).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&task_id)?;
    if operation.as_str().len() > 200
        || confirmation_ref.len() > 200
        || display_ticket.len() != 40
        || !display_ticket.starts_with("display_")
        || !display_ticket[8..]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST).with_task(&id));
    }
    let reference = ConfirmationRef::parse(confirmation_ref)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST).with_task(&id))?;
    let action = match action {
        ConfirmAction::Confirm => PlanningConfirmationAction::Confirm,
        ConfirmAction::Reject => PlanningConfirmationAction::Reject,
    };
    let service = service(&state)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service.act(
            &identity.reference,
            &id,
            &operation,
            &reference,
            &display_ticket,
            action,
        ),
    )
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map_err(error)
        .and_then(project)
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
