//! Public orchestration of existing checked planning and persisted execution services.
use super::*;
use safeflow_bank_gateway::{BankError, FindPayeesInput, GET_ACCOUNT_BALANCE};
use safeflow_contracts::{
    AgentPlan, ContractFuture, MockDraftAction, PaymentDraftLocator, PlanNode, PlanningResult,
    ResourceId, ToolName, ToolResult, Understanding, UnderstandingDecision, PAYMENT_INTENT_TOOL,
};
use safeflow_runtime::{
    LlmGateway, PlanningContext, PlanningError, PlanningGatewaySource, PlanningInput,
    PlanningIntakeService, PreflightInputContract, Runtime,
};
use serde_json::{json, Value};

fn registration(state: &ApiState) -> &assembly::CheckedAssembly {
    if state.0.bank_planning_read_only {
        return &state.0.runtime;
    }
    state
        .0
        .draft
        .as_ref()
        .and_then(|d| d.lifecycle_runtime.as_ref())
        .unwrap_or(&state.0.runtime)
}
fn runtime(state: &ApiState) -> &Runtime {
    registration(state).runtime()
}
fn contracts(state: &ApiState) -> Vec<PreflightInputContract> {
    registration(state).contracts()
}
fn planner_error(e: safeflow_runtime::PlanningIntakeError) -> ApiError {
    match e {
        safeflow_runtime::PlanningIntakeError::Session => ApiError::unauthenticated(),
        safeflow_runtime::PlanningIntakeError::GatewayFailed => {
            unavailable("LLM_GATEWAY_UNAVAILABLE")
        }
        safeflow_runtime::PlanningIntakeError::GatewayNotConfigured => {
            unavailable("LLM_NOT_CONFIGURED")
        }
        _ => ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            reason: "LLM_PROPOSAL_REJECTED",
            task_id: None,
        },
    }
}
fn unavailable(reason: &'static str) -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        reason,
        task_id: None,
    }
}

struct ScopedGateway {
    gateway: Arc<safeflow_runtime::OpenAiGateway>,
    data: Value,
}
impl LlmGateway for ScopedGateway {
    fn source(&self) -> PlanningGatewaySource {
        PlanningGatewaySource::OpenaiCompatible
    }
    fn generate<'a>(
        &'a self,
        _: &'a PlanningInput,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        Box::pin(async {
            Err(PlanningError::Gateway(
                "LLM_TRUSTED_CONTEXT_REQUIRED".into(),
            ))
        })
    }
    fn generate_with_context<'a>(
        &'a self,
        input: &'a PlanningInput,
        context: &'a PlanningContext,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        Box::pin(async move {
            let _model = super::request_timing::Phase::new("model");
            self.gateway
                .propose_for_agent_backend(input, context, &self.data)
                .await
        })
    }
}

#[derive(Serialize)]
pub(super) struct AgentStatus {
    contract_version: u16,
    mode: &'static str,
    configured: bool,
    connectivity: &'static str,
    tools: Vec<String>,
    draft_enabled: bool,
    planning_mode: &'static str,
    payment_planning_enabled: bool,
    funds_execution_enabled: bool,
}
pub(super) async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<AgentStatus>, ApiError> {
    state.identity(&headers)?.check(&state)?;
    Ok(Json(AgentStatus {
        contract_version: 1,
        payment_planning_enabled: registration(&state)
            .definitions()
            .iter()
            .any(|d| d.name.as_str() == PAYMENT_INTENT_TOOL),
        funds_execution_enabled: !state.0.bank_planning_read_only && payment_execution::enabled(&state),
        mode: MODE,
        configured: state.0.llm_gateway.is_some(),
        connectivity: "NOT_VERIFIED",
        planning_mode: if state.0.agent_fixed_plan_enabled {
            "FIXED_PLAN"
        } else if state.0.llm_gateway.is_some() {
            "OPENAI_COMPATIBLE"
        } else {
            "DISABLED"
        },
        tools: registration(&state)
            .definitions()
            .iter()
            .map(|d| d.name.to_string())
            .collect(),
        draft_enabled: !state.0.bank_planning_read_only && state
            .0
            .draft
            .as_ref()
            .is_some_and(|d| d.lifecycle_runtime.is_some()),
    }))
}

#[derive(Serialize)]
pub(super) struct AgentResponse {
    contract_version: u16,
    mode: &'static str,
    kind: &'static str,
    task: Option<TaskResponse>,
    draft: Option<draft_lifecycle::LifecycleResponse>,
    results: Option<Vec<ToolResult>>,
    clarification: Option<safeflow_contracts::Clarification>,
    can_start: bool,
    planning_source: Option<PlanningGatewaySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payment: Option<payment_execution::PaymentView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payment_flow: Option<PaymentFlow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bank_service: Option<String>,
    #[serde(skip)]
    pub(super) payee_query: Option<String>,
}
#[derive(Serialize)]
struct PaymentFlow {
    status: &'static str,
    planning_enabled: bool,
    funds_execution_enabled: bool,
    automatic_retry: bool,
    next_action: &'static str,
    next_path: String,
    original: Option<safeflow_storage::PaymentOriginalResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    funds_result: Option<payment_execution::FundsResult>,
    waiting_node_id: NodeId,
}
impl AgentResponse {
    fn clarify(question: String) -> Self {
        Self {
            contract_version: 1,
            mode: MODE,
            kind: "CLARIFY",
            task: None,
            draft: None,
            results: None,
            clarification: Some(safeflow_contracts::Clarification { question }),
            can_start: false,
            planning_source: None,
            payment: None,
            payment_flow: None,
            bank_service: None,
            payee_query: None,
        }
    }
    fn draft(view: LifecycleView) -> Result<Self, ApiError> {
        let planning_source = view.planning_source;
        Ok(Self {
            contract_version: 1,
            mode: MODE,
            kind: "DRAFT",
            task: None,
            draft: Some(draft_lifecycle::project(view)?),
            results: None,
            clarification: None,
            can_start: false,
            planning_source,
            payment: None,
            payment_flow: None,
            bank_service: None,
            payee_query: None,
        })
    }
    fn with_source(mut self, source: PlanningGatewaySource) -> Self {
        self.planning_source = Some(source);
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayeeRequest {
    query: String,
}
pub(super) async fn payees(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<safeflow_bank_gateway::FindPayeesOutput>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: PayeeRequest = body(request).await?;
    FindPayeesInput {
        query: input.query.clone(),
    }
    .validate()
    .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let output = tokio::time::timeout(
        state.0.request_timeout,
        state
            .0
            .bank
            .find_payees(&identity.binding.user_id, &input.query),
    )
    .await
    .map_err(|_| ApiError::unavailable())?
    .map_err(|_| ApiError::unavailable())?;
    identity.check(&state)?;
    output.validate().map_err(|_| ApiError::unavailable())?;
    Ok(Json(output))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Clone)]
pub(super) struct Selection {
    account_id: Option<ResourceId>,
    payee_query: Option<String>,
    payee_id: Option<ResourceId>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Clone)]
pub(super) struct Context {
    pub(super) task_id: TaskId,
    task_revision: u64,
    plan_version: u32,
    facts_version: u64,
    selection_ticket: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MessageRequest {
    pub(super) message: String,
    pub(super) selection: Option<Selection>,
    pub(super) context: Option<Context>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum FixedSample {
    ReadWait,
    #[serde(rename = "CREATE_200")]
    Create200,
    #[serde(rename = "MODIFY_300")]
    Modify300,
    CancelDraft,
    ExecuteTransfer,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SampleRequest {
    sample: FixedSample,
    selection: Option<Selection>,
    context: Option<Context>,
}
pub(super) async fn sample(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<AgentResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    if !state.0.agent_fixed_plan_enabled {
        return Err(ApiError::disabled());
    }
    let input: SampleRequest = body(request).await?;
    process_message(
        &state,
        &identity,
        MessageRequest {
            message: "FIXED_PLAN_SAMPLE".into(),
            selection: input.selection,
            context: input.context,
        },
        Some(input.sample),
        None,
        None,
    )
    .await
}

pub(super) async fn message(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<AgentResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    if state.0.agent_fixed_plan_enabled {
        return Err(ApiError::disabled());
    }
    let input: MessageRequest = body(request).await?;
    if input.message.trim().is_empty() || input.message.len() > 4096 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    let gateway = state
        .0
        .llm_gateway
        .clone()
        .ok_or_else(|| unavailable("LLM_NOT_CONFIGURED"))?;
    process_message(&state, &identity, input, None, Some(gateway), None).await
}
pub(super) async fn process_message(
    state: &ApiState,
    identity: &Identity,
    input: MessageRequest,
    sample: Option<FixedSample>,
    gateway: Option<Arc<safeflow_runtime::OpenAiGateway>>,
    preallocated: Option<TaskId>,
) -> Result<Json<AgentResponse>, ApiError> {
    process_scoped_message(state, identity, input, sample, gateway, preallocated, false).await
}

// This flag is native request scope, never client/model input or shared mutation.
async fn process_scoped_message(
    state: &ApiState,
    identity: &Identity,
    input: MessageRequest,
    sample: Option<FixedSample>,
    gateway: Option<Arc<safeflow_runtime::OpenAiGateway>>,
    preallocated: Option<TaskId>,
    independent_read: bool,
) -> Result<Json<AgentResponse>, ApiError> {
    let id = input.context.as_ref().map(|c|c.task_id.clone()).or(preallocated).unwrap_or_else(new_task_id);
    let deadline =
        gateway.as_ref().map_or(Duration::ZERO, |g| g.timeout()) + state.0.request_timeout;
    let result = tokio::time::timeout(
        deadline,
        message_inner(state, identity, &id, input, sample, gateway, independent_read),
    )
    .await;
    identity.check(state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| {
            if sample.is_some() {
                ApiError::unavailable().with_task(&id)
            } else {
                unavailable("LLM_GATEWAY_UNAVAILABLE").with_task(&id)
            }
        })?
        .map(Json)
}
async fn message_inner(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
    input: MessageRequest,
    sample: Option<FixedSample>,
    gateway: Option<Arc<safeflow_runtime::OpenAiGateway>>,
    independent_read: bool,
) -> Result<AgentResponse, ApiError> {
    let assembly = if independent_read { &state.0.runtime } else { registration(state) };
    let runtime = assembly.runtime();
    let contracts = assembly.contracts();
    if independent_read && (input.context.is_some() || input.selection.as_ref().is_some_and(|s| s.payee_id.is_some() || s.payee_query.is_some())) {
        return Err(ApiError::forbidden());
    }
    let original = if let Some(c) = &input.context {
        let view = draft_lifecycle::service(state)?
            .query(&identity.reference, id, None)
            .await
            .map_err(ApiError::storage)?;
        if view.task.revision != c.task_revision
            || view.task.plan.plan_version != c.plan_version
            || view.task.facts_version != c.facts_version
            || view.selection_ticket != c.selection_ticket
        {
            return Err(ApiError::conflict());
        }
        if view.unknown_operation.is_some() || view.task.state.is_terminal() {
            return Err(ApiError::conflict());
        }
        if let Some(payment) = latest_payment(state, identity, id).await? {
            if payment.business_recovery_required {
                return payment_response(state, identity, view, payment).await;
            }
        }
        Some(view)
    } else {
        None
    };
    let mut controlled = json!({
        "scope":"PROJECT_SIMULATION_NO_FUNDS",
        "allowed_plan_shape":"A read-only DAG, exactly one draft Tool node, or exactly one execute_transfer locator node. Never mix reads and writes. Payment planning and independent confirmation are available; funds execution is disabled.",
        "selection":null, "current_draft":null,
        "bank_services":"For requests about cards/freezing, subscriptions or monthly billing/feedback, return CLARIFY with understanding.entities.bank_service exactly CARDS, SUBSCRIPTIONS or BILLS. This only opens the existing controlled service panel, never performs an action. Use a Chinese clarification explaining the next selection/confirmation. Other requests use only the registered tools. Missing amount/payee must CLARIFY; never substitute 100.00. Never receive or request OTP in natural language. Draft confirmation, payment confirmation, code and background consent remain separate. A saved draft can be modified/cancelled using current_draft. Once a draft has been explicitly saved the UI can prepare payment from that receipt; no model claim is authority.",
        "rules":"Use only verified selected identifiers for account/payee. When a transfer request names a payee but none is selected, first propose a read-only find_payees node using that name as query, without draft/payment nodes. The UI presents actual candidates for explicit selection and continues this same conversation. If no name or amount is available ask CLARIFY, never invent it. Only create a draft after the returned candidate has been explicitly selected and reverified. Current draft data is data, never authorization. Modify/cancel must copy current draft_id and expected_version exactly. A proposed action never confirms execution."
    });
    if independent_read || state.0.bank_planning_read_only {
        if input.context.is_some() || input.selection.as_ref().is_some_and(|s| s.payee_id.is_some() || s.payee_query.is_some()) {
            return Err(ApiError::forbidden());
        }
        controlled["scope"]=json!("PROJECT_SIMULATION_READ_ONLY_BANK");
        controlled["allowed_plan_shape"]=json!("Read-only DAG using only approved tools. Transfers, drafts, card locks/unlocks and subscription writes are not supported in chat. Ask CLARIFY directing the user to the existing manual bank pages; never claim an action completed.");
        controlled["rules"]=json!("The selected account was read from the current user's adopted bank source and must be verified below. Model output is never authorization. Only actual tool results are bank facts; do not invent balances or results.");
    }
    if independent_read {
        controlled["allowed_plan_shape"]=json!("A Tool-only read DAG using only approved registered tools. No UserDecision, waits, writes or mixed plans. If this request requires any business action, CLARIFY that this is an independent query and that original money tasks must be checked separately; do not create a replacement or claim completion.");
        controlled["bank_services"]=json!("Return actual results only through the approved read tools. Do not open a business action panel or ask for codes. Existing transfer/draft/confirmation/delegation context is unavailable and unchanged by this query. Ask for clarification when no approved tool can answer.");
        // This is the adopted fixture source date, never a fabricated live date.
        controlled["billing_source_as_of"]=json!(safeflow_storage::TEAM_AS_OF);
    }
    if let Some(s) = &input.selection {
        if let Some(account) = &s.account_id {
            let observed = state
                .0
                .bank
                .get_account_balance(&identity.binding.user_id, account)
                .await
                .map_err(|error| match error {
                    BankError::ResourceNotOwned => ApiError::forbidden(),
                    BankError::ValidationError => ApiError::validation(StatusCode::BAD_REQUEST),
                    BankError::Unavailable | BankError::InvalidResponse => ApiError::unavailable(),
                })?;
            identity.check(state)?;
            observed
                .validate_for(account)
                .map_err(|_| ApiError::unavailable())?;
            controlled["selection"]["account_id"] = json!(account);
        }
        match (&s.payee_query, &s.payee_id) {
            (Some(query), Some(payee)) => {
                FindPayeesInput {
                    query: query.clone(),
                }
                .validate()
                .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
                let draft = state.0.draft.as_ref().ok_or_else(ApiError::disabled)?;
                let context = safeflow_contracts::ExecutionContext {
                    task_id: id.clone(),
                    node_id: NodeId::parse("agent_selection").expect("static node"),
                    operation_id: new_operation_id(),
                    authenticated_user_id: identity.binding.user_id.clone(),
                    plan_version: 1,
                    idempotency_key: None,
                };
                let found = draft
                    .store
                    .find_candidates(&context, query)
                    .await
                    .map_err(|_| ApiError::unavailable())?;
                identity.check(state)?;
                if !found.candidates.iter().any(|p| p.payee_id == *payee) {
                    return Err(ApiError::forbidden());
                }
                controlled["selection"]["payee_id"] = json!(payee);
            }
            (None, None) => {}
            _ => return Err(ApiError::validation(StatusCode::BAD_REQUEST)),
        }
    }
    let mut lifecycle = state
        .0
        .draft
        .as_ref()
        .filter(|draft| draft.lifecycle_runtime.is_some())
        .map(|_| draft_lifecycle::service(state))
        .transpose()?;
    if let Some(Selection {
        payee_query: Some(query),
        payee_id: Some(payee),
        ..
    }) = &input.selection
    {
        lifecycle = lifecycle
            .map(|service| service.with_payee_query_for_backend(payee.clone(), query.clone()))
            .transpose()
            .map_err(ApiError::storage)?;
    }
    if let Some(view) = &original {
        let observed = lifecycle
            .as_ref()
            .ok_or_else(ApiError::disabled)?
            .observe_current(&identity.reference, id, &view.operation.operation_id)
            .await
            .map_err(ApiError::storage)?;
        controlled["current_draft"] = json!({"draft_id":observed.draft_id,"expected_version":observed.draft_version,"parameters":observed.parameters,"status":observed.status});
        // Business restore intentionally discards candidates. Query current facts again.
        if let Some(Selection {
            payee_query: Some(query),
            payee_id: Some(payee),
            ..
        }) = &input.selection
        {
            let context = safeflow_contracts::ExecutionContext {
                task_id: id.clone(),
                node_id: NodeId::parse("agent_selection").expect("static node"),
                operation_id: new_operation_id(),
                authenticated_user_id: identity.binding.user_id.clone(),
                plan_version: view.task.plan.plan_version,
                idempotency_key: None,
            };
            let found = state
                .0
                .draft
                .as_ref()
                .ok_or_else(ApiError::disabled)?
                .store
                .find_candidates(&context, query)
                .await
                .map_err(|_| ApiError::unavailable())?;
            identity.check(state)?;
            if !found
                .candidates
                .iter()
                .any(|candidate| candidate.payee_id == *payee)
            {
                return Err(ApiError::forbidden());
            }
        }
    }
    let version = original
        .as_ref()
        .map_or(Some(1), |v| v.task.plan.plan_version.checked_add(1))
        .ok_or_else(ApiError::conflict)?;
    let planner: Arc<dyn LlmGateway> = if let Some(sample) = sample {
        Arc::new(MockLlm::new(sample_proposal(
            sample,
            id,
            version,
            &controlled,
        )?))
    } else {
        Arc::new(ScopedGateway {
            gateway: gateway.ok_or_else(|| unavailable("LLM_NOT_CONFIGURED"))?,
            data: controlled.clone(),
        })
    };
    let intake = PlanningIntakeService::for_registered_backend(
        runtime,
        Some(planner),
        contracts.clone(),
    )
    .map_err(planner_error)?;
    let plan_timing = super::request_timing::Phase::new("plan");
    let reviewed = intake
        .plan_for_persistence(&identity.reference, id, version, &input.message)
        .await;
    drop(plan_timing);
    let review = reviewed.map_err(planner_error)?;
    let result = review.planning_result();
    let source = review.gateway_source();
    if let Some(clarification) = &result.clarification {
        let mut response=AgentResponse::clarify(clarification.question.clone()).with_source(source);
        if !independent_read && state.0.unified_source.is_some() && original.is_none() {
            response.bank_service=result.understanding.entities.get("bank_service").and_then(Value::as_str).filter(|v| matches!(*v,"CARDS"|"SUBSCRIPTIONS"|"BILLS")).map(str::to_owned);
        }
        return Ok(response);
    }
    let plan = result.plan.as_ref().ok_or_else(ApiError::unavailable)?;
    // Full checked plan, then a second allowlist pass over EVERY node before
    // task creation or any tool/service call. No hidden writes or waits.
    if independent_read && plan.nodes.iter().any(|node| match node {
        PlanNode::Tool { tool, .. } => assembly.execution(tool) != Some(assembly::ExecutionPath::ReadOnly)
            || !assembly.definitions().iter().any(|d| d.name == *tool && !d.has_side_effect),
        _ => true,
    }) {
        return Err(planner_error(safeflow_runtime::PlanningIntakeError::InvalidResult));
    }
    let mut writes = Vec::new();
    let mut payments = Vec::new();
    for node in &plan.nodes {
        if let PlanNode::Tool {
            tool, arguments, ..
        } = node
        {
            if tool.as_str() == GET_ACCOUNT_BALANCE
                && (controlled["selection"]["account_id"].is_null()
                    || arguments["account_id"] != controlled["selection"]["account_id"])
            {
                return Ok(
                    AgentResponse::clarify("请明确填写要查询或付款的账户编号。".into())
                        .with_source(source),
                );
            }
            match assembly.execution(tool) {
                Some(assembly::ExecutionPath::PaymentIntent) => payments.push(arguments),
                Some(assembly::ExecutionPath::ReadOnly) => {}
                Some(assembly::ExecutionPath::MockDraft(action)) => {
                    writes.push((action, arguments))
                }
                None => return Err(ApiError::forbidden()),
            }
        }
    }
    if !payments.is_empty() {
        if payments.len() != 1 || plan.nodes.len() != 1 || !writes.is_empty() {
            return Err(planner_error(
                safeflow_runtime::PlanningIntakeError::InvalidResult,
            ));
        }
        let context = input.context.as_ref().ok_or_else(ApiError::conflict)?;
        let locator: PaymentDraftLocator = serde_json::from_value(payments[0].clone())
            .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
        if controlled["current_draft"].is_null()
            || json!(locator.draft_id) != controlled["current_draft"]["draft_id"]
            || json!(locator.draft_version) != controlled["current_draft"]["expected_version"]
        {
            return Err(ApiError::conflict());
        }
        let payment = payment_preparation::service(state)?
            .prepare_for_agent(
                &identity.reference,
                id,
                safeflow_storage::PaymentPreparationRequest {
                    selection_ticket: context.selection_ticket.clone(),
                    task_revision: context.task_revision,
                    plan_version: context.plan_version,
                    facts_version: context.facts_version,
                    draft_id: locator.draft_id,
                    draft_version: locator.draft_version,
                },
                &review,
                contracts.clone(),
            )
            .await
            .map_err(|error| match error {
                StorageError::ExecutionKeyConflict => ApiError::conflict(),
                other => ApiError::storage(other),
            })?;
        let draft = lifecycle
            .as_ref()
            .ok_or_else(ApiError::disabled)?
            .query(&identity.reference, id, None)
            .await
            .map_err(ApiError::storage)?;
        return payment_response(state, identity, draft, payment).await;
    }
    if writes.is_empty() {
        if original.is_some() {
            return Ok(AgentResponse::clarify(
                "当前草案任务只接受修改或取消；新的查询请另开任务。".into(),
            )
            .with_source(source));
        }
        let payee_query=plan.nodes.iter().find_map(|node|match node {
            PlanNode::Tool{tool,arguments,..} if tool.as_str()=="find_payees" => arguments.get("query").and_then(Value::as_str).map(str::to_owned),
            _=>None,
        });
        PlanningTaskCreationService::for_trusted_backend(
            state.0.repository.clone(),
            runtime,
            None,
            contracts.clone(),
        )
        .map_err(|_| ApiError::unavailable())?
        .create(review, 1)
        .await
        .map_err(|e| match e {
            PlanningTaskCreationError::Storage(e) => ApiError::storage(e).with_task(id),
            PlanningTaskCreationError::Intake(e) => planner_error(e),
        })?;
        let mut response=read_view_using(state, identity, id, assembly, independent_read).await?;
        response.payee_query=payee_query;
        return Ok(response);
    }
    if writes.len() != 1 || plan.nodes.len() != 1 {
        return Err(planner_error(
            safeflow_runtime::PlanningIntakeError::InvalidResult,
        ));
    }
    let (action, parameters) = writes[0];
    let parameter_source = if action == MockDraftAction::Create {
        parameters
    } else {
        &parameters["changes"]
    };
    for (key, selected) in [("payer_account_id", "account_id"), ("payee_id", "payee_id")] {
        if action == MockDraftAction::Create || parameter_source.get(key).is_some() {
            if controlled["selection"][selected].is_null()
                || parameter_source[key] != controlled["selection"][selected]
            {
                return Ok(AgentResponse::clarify(
                    "请明确选择付款账户和已查询到的收款人，再提出草案。".into(),
                )
                .with_source(source));
            }
        }
    }
    intake
        .check_persistence_review(&review)
        .map_err(planner_error)?;
    let service = lifecycle
        .ok_or_else(ApiError::disabled)?
        .with_planning_source_for_backend(source);
    let view = if let Some(c) = input.context {
        if action == MockDraftAction::Create
            || controlled["current_draft"].is_null()
            || parameters["draft_id"] != controlled["current_draft"]["draft_id"]
            || parameters["expected_version"] != controlled["current_draft"]["expected_version"]
        {
            return Err(ApiError::conflict());
        }
        service
            .select(
                &identity.reference,
                id,
                LifecycleSelection {
                    selection_ticket: c.selection_ticket,
                    task_revision: c.task_revision,
                    plan_version: c.plan_version,
                    facts_version: c.facts_version,
                    action,
                    parameters: parameters.clone(),
                },
            )
            .await
            .map_err(|e| ApiError::storage(e).with_task(id))?
    } else {
        if action != MockDraftAction::Create {
            return Ok(AgentResponse::clarify("请先选择已有草案任务。".into()).with_source(source));
        }
        service
            .create(&identity.reference, id, parameters.clone(), 1)
            .await
            .map_err(|e| ApiError::storage(e).with_task(id))?
    };
    AgentResponse::draft(view)
}

async fn latest_payment(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
) -> Result<Option<safeflow_storage::PaymentPreparationView>, ApiError> {
    Ok(payment_preparation::service(state)?
        .query_task(&identity.reference, id)
        .await
        .map_err(ApiError::storage)?
        .into_iter()
        .filter(|p| p.agent_binding.is_some())
        .last())
}

async fn lifecycle_response(
    state: &ApiState,
    identity: &Identity,
    draft: LifecycleView,
) -> Result<AgentResponse, ApiError> {
    if let Some(payment) = latest_payment(state, identity, &draft.task.task_id).await? {
        payment_response(state, identity, draft, payment).await
    } else {
        AgentResponse::draft(draft)
    }
}

async fn payment_response(
    state: &ApiState,
    identity: &Identity,
    draft: LifecycleView,
    payment: safeflow_storage::PaymentPreparationView,
) -> Result<AgentResponse, ApiError> {
    let binding = payment
        .agent_binding
        .as_ref()
        .ok_or_else(ApiError::conflict)?;
    let base = format!(
        "/api/sim/payment-preparations/{}/{}",
        payment.task_id, payment.operation.operation_id
    );
    let (status, next_action, next_path) = if payment.business_recovery_required {
        (
            "QUERY_ORIGINAL",
            "QUERY_ORIGINAL",
            payment.original_result_path.clone(),
        )
    } else if payment.invalid_reason.is_some() {
        (
            "ENDED_INVALIDATED",
            "REVIEW_CURRENT_DRAFT",
            format!("/api/agent/tasks/{}", payment.task_id),
        )
    } else if payment.confirmation_status == safeflow_storage::ConfirmationStatus::Prepared {
        (
            "WAITING_PAYMENT_CONFIRMATION",
            "CONFIRM_OR_REJECT",
            format!("{base}/confirm"),
        )
    } else if payment_execution::enabled(state)
        && payment.confirmation_status == safeflow_storage::ConfirmationStatus::Confirmed
    {
        (
            "WAITING_EXPLICIT_PAYMENT",
            "EXECUTE_PAYMENT",
            format!("{base}/execute"),
        )
    } else {
        (
            "WAITING_EXECUTION_CONDITIONS",
            "QUERY_OR_EVALUATE",
            format!("{base}/evaluate"),
        )
    };
    let original = if payment.business_recovery_required {
        Some(
            payment_preparation::service(state)?
                .original_result(
                    &identity.reference,
                    &payment.task_id,
                    &payment.operation.operation_id,
                )
                .await
                .map_err(ApiError::storage)?,
        )
    } else {
        None
    };
    let source = binding.planning_source;
    let waiting_node_id = match &binding.reviewed_plan.nodes[0] {
        PlanNode::Tool { id, .. } => id.clone(),
        _ => return Err(ApiError::conflict()),
    };
    let mut response = AgentResponse::draft(draft)?.with_source(source);
    response.kind = "PAYMENT";
    response.payment_flow = Some(PaymentFlow {
        status,
        planning_enabled: true,
        funds_execution_enabled: payment_execution::enabled(state),
        automatic_retry: false,
        next_action,
        next_path,
        original,
        funds_result: if payment.business_recovery_required {
            Some(
                payment_execution::read(
                    state,
                    identity,
                    &payment.task_id,
                    &payment.operation.operation_id,
                )
                .await?,
            )
        } else {
            None
        },
        waiting_node_id,
    });
    response.payment = Some(payment_execution::project(state, payment));
    Ok(response)
}

async fn read_view(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
) -> Result<AgentResponse, ApiError> {
    read_view_using(state, identity, id, registration(state), false).await
}
async fn read_view_using(
    state: &ApiState,
    identity: &Identity,
    id: &TaskId,
    assembly: &assembly::CheckedAssembly,
    independent_read: bool,
) -> Result<AgentResponse, ApiError> {
    let runtime = assembly.runtime();
    let contracts = assembly.contracts();
    let service = TaskQueryService::for_trusted_backend(state.0.repository.clone(), runtime);
    let provenance = service
        .planning_gateway_provenance(&identity.reference, id)
        .await
        .map_err(ApiError::storage)?;
    if !matches!(
        provenance.source(),
        Some(PlanningGatewaySource::OpenaiCompatible | PlanningGatewaySource::MockLlm)
    ) {
        return Err(ApiError::not_found());
    }
    let view = service
        .query(&identity.reference, id)
        .await
        .map_err(ApiError::storage)?;
    checked_view_versions(&view)?;
    let intake =
        PlanningIntakeService::for_registered_backend(runtime, None, contracts.clone())
            .map_err(planner_error)?;
    let registration = intake
        .registration_hash_for_backend()
        .map_err(planner_error)?;
    if independent_read && !provenance.matches_current_registration(&registration) {
        return Err(ApiError::conflict());
    }
    let execution = RuntimeTaskContinuationService::for_trusted_backend(
        state.0.repository.clone(),
        runtime,
        contracts.clone(),
    )
    .map_err(ApiError::execution)?
    .query_saved_history(
        &identity.reference,
        id,
        RuntimeTaskVersion {
            task_revision: view.revision,
            plan_version: view.plan_version,
            facts_version: view.facts_version,
        },
    )
    .await;
    let (status, results) = match execution {
        Ok(e) if e.task.revision == view.revision => (Some(e.execution_status), e.results),
        Ok(_) => return Err(ApiError::conflict()),
        Err(RuntimeTaskExecutionError::Storage(StorageError::RuntimeIntakeRequired)) => {
            (None, None)
        }
        Err(e) => return Err(ApiError::execution(e)),
    };
    identity.check(state)?;
    let can_start = status.is_none()
        && view.revision == 1
        && view.task_state == TaskState::Running
        && provenance.matches_current_registration(&registration);
    Ok(AgentResponse {
        contract_version: 1,
        mode: MODE,
        kind: "READ",
        task: Some(TaskResponse::project(view, status, false)),
        draft: None,
        results,
        clarification: None,
        can_start,
        planning_source: provenance.source(),
        payment: None,
        payment_flow: None,
        bank_service: None,
        payee_query: None,
    })
}

fn sample_proposal(
    sample: FixedSample,
    id: &TaskId,
    version: u32,
    data: &Value,
) -> Result<PlanningResult, ApiError> {
    let missing = match sample {
        FixedSample::ReadWait if data["selection"]["account_id"].is_null() => {
            Some("请明确填写查询账户编号。")
        }
        FixedSample::Create200
            if data["selection"]["account_id"].is_null()
                || data["selection"]["payee_id"].is_null() =>
        {
            Some("请明确选择付款账户和已查询到的收款人。")
        }
        FixedSample::Modify300 | FixedSample::CancelDraft | FixedSample::ExecuteTransfer
            if data["current_draft"].is_null() =>
        {
            Some("请先选择已保存的草案任务。")
        }
        _ => None,
    };
    let understanding = Understanding {
        intent: "FIXED_PLAN_SAMPLE".into(),
        entities: json!({}),
        missing_fields: vec![],
        context_refs: vec![],
        decision: if missing.is_some() {
            UnderstandingDecision::Clarify
        } else {
            UnderstandingDecision::Pass
        },
    };
    if let Some(question) = missing {
        return Ok(PlanningResult {
            contract_version: 1,
            understanding,
            plan: None,
            clarification: Some(safeflow_contracts::Clarification {
                question: question.into(),
            }),
        });
    }
    let tool = |name: &str, arguments: Value, node: &str, depends_on: Vec<NodeId>| PlanNode::Tool {
        id: NodeId::parse(node).expect("server sample node"),
        tool: ToolName::parse(name).expect("registered sample tool"),
        arguments,
        depends_on,
    };
    let nodes = match sample {
        FixedSample::ReadWait => vec![
            tool(
                GET_ACCOUNT_BALANCE,
                json!({"account_id":data["selection"]["account_id"]}),
                "balance",
                vec![],
            ),
            PlanNode::UserDecision {
                id: NodeId::parse("choice").expect("server node"),
                depends_on: vec![NodeId::parse("balance").expect("server node")],
            },
            tool(
                GET_ACCOUNT_BALANCE,
                json!({"account_id":data["selection"]["account_id"]}),
                "second",
                vec![NodeId::parse("choice").expect("server node")],
            ),
        ],
        FixedSample::Create200 => vec![tool(
            MockDraftAction::Create.tool_name(),
            json!({"payer_account_id":data["selection"]["account_id"],"payee_id":data["selection"]["payee_id"],"amount":"200.00","currency":"CNY","remark":"","replaces":null}),
            "proposal",
            vec![],
        )],
        FixedSample::Modify300 => vec![tool(
            MockDraftAction::Modify.tool_name(),
            json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"],"changes":{"amount":"300.00"}}),
            "proposal",
            vec![],
        )],
        FixedSample::CancelDraft => vec![tool(
            MockDraftAction::Cancel.tool_name(),
            json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"]}),
            "proposal",
            vec![],
        )],
        FixedSample::ExecuteTransfer => vec![tool(
            PAYMENT_INTENT_TOOL,
            json!({"draft_id":data["current_draft"]["draft_id"],"draft_version":data["current_draft"]["expected_version"]}),
            "payment",
            vec![],
        )],
    };
    Ok(PlanningResult::for_plan(
        understanding,
        AgentPlan::new(id.clone(), version, nodes).map_err(|_| ApiError::unavailable())?,
    ))
}
pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<AgentResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&value)?;
    let outcome = tokio::time::timeout(state.0.request_timeout, async {
        if state
            .0
            .repository
            .is_lifecycle_task(&identity.binding.user_id, &id)
            .await
            .map_err(ApiError::storage)?
        {
            let draft = draft_lifecycle::service(&state)?
                .query(&identity.reference, &id, None)
                .await
                .map_err(ApiError::storage)?;
            lifecycle_response(&state, &identity, draft).await
        } else {
            read_view(&state, &identity, &id).await
        }
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    outcome
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCommand {
    task_revision: u64,
    plan_version: u32,
    facts_version: u64,
    waiting_node_id: Option<NodeId>,
}
async fn command(
    state: ApiState,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    continuing: bool,
) -> Result<Json<AgentResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&value)?;
    let input: ReadCommand = body(request).await?;
    let outcome = tokio::time::timeout(state.0.request_timeout, async {
        if state
            .0
            .repository
            .is_lifecycle_task(&identity.binding.user_id, &id)
            .await
            .map_err(ApiError::storage)?
        {
            let draft = draft_lifecycle::service(&state)?
                .query(&identity.reference, &id, None)
                .await
                .map_err(ApiError::storage)?;
            if input.task_revision != draft.task.revision
                || input.plan_version != draft.task.plan.plan_version
                || input.facts_version != draft.task.facts_version
            {
                return Err(ApiError::conflict());
            }
            let payment = latest_payment(&state, &identity, &id)
                .await?
                .ok_or_else(ApiError::conflict)?;
            let binding = payment
                .agent_binding
                .as_ref()
                .ok_or_else(ApiError::conflict)?;
            let node = match &binding.reviewed_plan.nodes[0] {
                PlanNode::Tool { id, .. } => id,
                _ => return Err(ApiError::conflict()),
            };
            if (continuing && input.waiting_node_id.as_ref() != Some(node))
                || (!continuing && input.waiting_node_id.is_some())
            {
                return Err(ApiError::conflict());
            }
            return payment_response(&state, &identity, draft, payment).await;
        }
        read_view(&state, &identity, &id).await?;
        let version = RuntimeTaskVersion {
            task_revision: input.task_revision,
            plan_version: input.plan_version,
            facts_version: input.facts_version,
        };
        if continuing {
            let node = input
                .waiting_node_id
                .ok_or_else(|| ApiError::validation(StatusCode::BAD_REQUEST))?;
            RuntimeTaskContinuationService::for_trusted_backend(
                state.0.repository.clone(),
                runtime(&state),
                contracts(&state),
            )
            .map_err(ApiError::execution)?
            .continue_task(&identity.reference, &id, &node, version)
            .await
            .map_err(ApiError::execution)?;
        } else {
            if input.waiting_node_id.is_some() {
                return Err(ApiError::validation(StatusCode::BAD_REQUEST));
            }
            RuntimeTaskExecutionService::for_trusted_backend(
                state.0.repository.clone(),
                runtime(&state),
                contracts(&state),
            )
            .map_err(ApiError::execution)?
            .schedule_for_agent_backend(&identity.reference, &id, version)
            .await
            .map_err(ApiError::execution)?;
        }
        read_view(&state, &identity, &id).await
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    outcome
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| e.with_task(&id))
}
pub(super) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<AgentResponse>, ApiError> {
    command(state, path, request, false).await
}
pub(super) async fn continue_read(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<AgentResponse>, ApiError> {
    command(state, path, request, true).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareRequest {
    operation_id: safeflow_contracts::OperationId,
}
pub(super) async fn prepare(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<AgentResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&value)?;
    let input: PrepareRequest = body(request).await?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        AgentResponse::draft(
            draft_lifecycle::service(&state)?
                .prepare(&identity.reference, &id, &input.operation_id)
                .await
                .map_err(ApiError::storage)?,
        )
    })
    .await;
    identity.check(&state).map_err(|e| e.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| e.with_task(&id))
}

// Only this native entry creates independent read tasks. It has no context,
// draft service or client-selected mode; registry validation precedes persistence.
pub(super) async fn independent_read(
    state: &ApiState, identity: &Identity, message: String,
    account_id: Option<ResourceId>, id: TaskId,
) -> Result<AgentResponse, ApiError> {
    let Json(view) = process_scoped_message(state, identity, MessageRequest {
        message, selection: Some(Selection { account_id, payee_id: None, payee_query: None }), context: None,
    }, None, state.0.llm_gateway.clone(), Some(id.clone()), true).await?;
    if view.kind == "CLARIFY" { return Ok(view); }
    if view.kind != "READ" || !view.can_start { return Err(ApiError::conflict()); }
    let task = TaskQueryService::for_trusted_backend(state.0.repository.clone(), state.0.runtime.runtime())
        .query(&identity.reference, &id).await.map_err(ApiError::storage)?;
    let tool_timing = super::request_timing::Phase::new("tools");
    tokio::time::timeout(state.0.request_timeout, RuntimeTaskExecutionService::for_trusted_backend(
        state.0.repository.clone(), state.0.runtime.runtime(), state.0.runtime.contracts(),
    ).map_err(ApiError::execution)?.schedule_for_agent_backend(&identity.reference, &id, RuntimeTaskVersion {
        task_revision: task.revision, plan_version: task.plan_version, facts_version: task.facts_version,
    })).await.map_err(|_| ApiError::unavailable())?.map_err(ApiError::execution)?;
    drop(tool_timing);
    independent_read_view(state, identity, &id).await
}
pub(super) async fn independent_read_view(state: &ApiState, identity: &Identity, id: &TaskId) -> Result<AgentResponse, ApiError> {
    read_view_using(state, identity, id, &state.0.runtime, true).await
}
