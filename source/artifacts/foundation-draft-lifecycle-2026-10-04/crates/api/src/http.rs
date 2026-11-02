use crate::{
    assembly,
    session::{token_reference, SimulationSessions, COOKIE_NAME},
};
use axum::{
    body::to_bytes,
    extract::{rejection::PathRejection, Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use cookie::Cookie;
use safeflow_contracts::{
    NodeId, OperationState, SecurityOutcome, SessionRef, TaskId, TaskState, UserId,
};
use safeflow_runtime::{MockLlm, RuntimeError, SessionBinding, SessionResolver};
use safeflow_storage::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};
use zeroize::Zeroizing;
mod agent;
mod bank_conversation;
mod bank_read_conversation;
mod request_timing;
mod billing_feedback;
mod card_read;
mod card_safety;
mod draft_confirmation;
mod draft_lifecycle;
mod llm_planning;
mod payment_execution;
mod payment_background;
mod payment_preparation;
#[path = "simulation_source.rs"]
mod simulation_source;
mod subscription;
mod team_consumption;
mod bank_site;
mod public_demo;
pub use public_demo::PublicDemoConfig;
#[path = "unified_fictional_source.rs"]
mod unified_fictional_source;
pub use billing_feedback::BillingFeedbackConfig;
pub use draft_confirmation::DraftConfirmationConfig;
pub use payment_execution::PaymentExecutionConfig;

const CONTRACT_VERSION: u16 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_BODY: usize = 8192;
const MODE: &str = "PROJECT_SIMULATION";

/// Trusted server configuration, never a request DTO. The login secret is not Debug/Serialize.
pub struct ApiConfig {
    pub bind: SocketAddr,
    pub frontend_origin: String,
    pub public_demo: Option<PublicDemoConfig>,
    pub demo_code_display: bool,
    pub session_ttl: Duration,
    pub request_timeout: Duration,
    pub login_secret: Zeroizing<String>,
    pub draft_confirmation: Option<DraftConfirmationConfig>,
    pub llm_gateway: Option<safeflow_runtime::OpenAiGatewayConfig>,
    pub agent_fixed_plan_enabled: bool,
    pub payment_execution: Option<PaymentExecutionConfig>,
    pub billing_feedback: Option<BillingFeedbackConfig>,
    pub card_read_source: Option<CardCatalogSourceBinding>,
}
impl ApiConfig {
    pub fn local_simulation(login_secret: Zeroizing<String>) -> Self {
        Self {
            bind: "127.0.0.1:8787".parse().expect("fixed local address"),
            frontend_origin: "http://127.0.0.1:5174".into(),
            public_demo: None,
            demo_code_display: false,
            session_ttl: Duration::from_secs(1800),
            request_timeout: Duration::from_secs(30),
            login_secret,
            draft_confirmation: None,
            llm_gateway: None,
            agent_fixed_plan_enabled: false,
            payment_execution: None,
            billing_feedback: None,
            card_read_source: None,
        }
    }
    pub fn validate(&self) -> Result<(), ApiStartupError> {
        if self.agent_fixed_plan_enabled && self.llm_gateway.is_some() {
            return Err(ApiStartupError("choose one Agent planning mode"));
        }
        let network_valid = if let Some(public) = &self.public_demo {
            public.validate(&self.frontend_origin)?;
            true
        } else {
            let origin: Uri = self.frontend_origin.parse()
                .map_err(|_| ApiStartupError("invalid local frontend origin"))?;
            origin.scheme_str() == Some("http")
                && origin.host() == Some("127.0.0.1")
                && origin.port_u16().is_some()
                && origin.path() == "/" && origin.query().is_none()
                && origin.authority().is_some_and(|a| self.frontend_origin == format!("http://{a}"))
        };
        if self.bind.ip().to_string() != "127.0.0.1" || self.bind.port() == 0 || !network_valid            || self.request_timeout.is_zero()
            || self.request_timeout.as_secs() > 120
            || !(1..=86400).contains(&self.session_ttl.as_secs())
            || !(16..=256).contains(&self.login_secret.len())
        {
            return Err(ApiStartupError("loopback simulation configuration invalid"));
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct ApiStartupError(&'static str);
impl fmt::Display for ApiStartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ApiStartupError {}

#[derive(Clone)]
pub struct ApiState(Arc<Inner>);
struct Inner {
    payment_background_host: Option<safeflow_storage::PaymentBackgroundHost>,
    card_safety_host: Option<CardSafetyHost>,
    unified_source: Option<PgUnifiedFictionalSource>,
    team_source: Option<safeflow_storage::TeamConsumptionSource>,
    subscription_public_scope: Option<SubscriptionPublicScope>,
    card_read: CardReadService,
    subscription: SubscriptionService,
    repository: PgRepository,
    runtime: assembly::CheckedAssembly,
    bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
    draft: Option<draft_confirmation::DraftAssembly>,
    sessions: Arc<SimulationSessions>,
    host: String,
    public_demo: Option<PublicDemoConfig>,
    demo_controller: Option<Zeroizing<String>>,
    frontend_origin: String,
    request_timeout: Duration,
    llm_gateway: Option<Arc<safeflow_runtime::OpenAiGateway>>,
    bank_planning_read_only: bool,
    conversations: bank_conversation::Conversations,
    read_conversations: bank_read_conversation::Conversations,
    agent_fixed_plan_enabled: bool,
    payment_execution: Option<PaymentExecutionConfig>,
    billing_feedback: Option<BillingFeedbackConfig>,
    #[cfg(test)]
    audit: Arc<safeflow_runtime::InMemoryAuditRecorder>,
}
impl ApiState {
    pub fn subscription_for_backend(&self) -> SubscriptionService {
        self.0.subscription.clone()
    }
    /// Native simulation host chooses the configured owner at startup, using
    /// the existing resolver. This is not exposed as a request parameter.
    pub fn for_project_simulation_with_owner_and_tools(
        repository: PgRepository,
        config: ApiConfig,
        owner: UserId,
        bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
        registrations: Vec<crate::ReadOnlyToolRegistration>,
    ) -> Result<Self, ApiStartupError> {
        Self::build_with_bank(repository, config, owner, bank, &registrations)
    }
    /// Local host authenticates through the very same configured session issuer.
    /// No model/HTTP DTO can call this method or nominate another owner.
    pub fn simulation_session_for_native_host(
        &self,
        secret: &str,
    ) -> Result<SessionRef, StorageError> {
        let cookie = self
            .0
            .sessions
            .issue(secret, None)
            .map_err(|_| StorageError::Session)?;
        let parsed = cookie::Cookie::parse(cookie).map_err(|_| StorageError::Session)?;
        crate::session::token_reference(parsed.value()).ok_or(StorageError::Session)
    }

    pub fn card_facts_for_backend(
        &self,
        source: Arc<dyn safeflow_card_facts::account_service::AccountDataSource + Send + Sync>,
    ) -> crate::SessionCardFacts {
        crate::SessionCardFacts::new(self.0.sessions.clone(), source)
    }

    pub fn execution_requests_for_backend(&self) -> ExecutionRequestStore {
        ExecutionRequestStore::for_trusted_backend(
            self.0.repository.clone(),
            self.0.sessions.clone(),
        )
    }

    /// Explicit trusted feedback assembly over this API's actual identity,
    /// account gateway and Runtime. No model/client registration or lock-card.
    pub fn billing_feedback_for_backend(
        &self,
        confirmation: safeflow_storage::ConfirmationConfig,
        permit: safeflow_storage::PermitConfig,
    ) -> Result<safeflow_storage::BillingFeedbackService<'_>, safeflow_storage::StorageError> {
        safeflow_storage::BillingFeedbackService::for_trusted_backend(
            self.0.repository.clone(),
            self.0.sessions.clone(),
            self.0.bank.clone(),
            self.0.runtime.runtime(),
            confirmation,
            permit,
        )
    }

    /// Process-local read navigation over this exact API repository and issuer.
    /// Returning the service grants no session, catalog source or write permit.
    pub fn card_read_for_backend(&self) -> CardReadService {
        self.0.card_read.clone()
    }

    /// A trusted explicit user-action handler can resolve the existing API
    /// cookie before using billing_feedback_for_backend. Call behind the same
    /// perimeter/CSRF checks as other handlers. Never accept a client owner or
    /// manufacture SessionRef from a displayed report. This adds no HTTP route.
    pub fn billing_feedback_session_for_backend(
        &self,
        headers: &HeaderMap,
    ) -> Result<SessionRef, StorageError> {
        self.identity(headers)
            .map(|identity| identity.reference)
            .map_err(|_| StorageError::Session)
    }

    /// Privileged worker access; never an unauthenticated outbox HTTP route.
    pub fn public_audit_outbox_for_backend(&self) -> PublicAuditOutbox {
        PublicAuditOutbox::for_trusted_backend(self.0.repository.clone())
    }

    /// Backend-only reads of received SQL using this API's actual session source.
    /// Does not register a Tool/HTTP route or provision simulated-bank accounts.
    pub fn transfer_sql_reader_for_backend(&self) -> safeflow_storage::TransferSqlReader {
        safeflow_storage::TransferSqlReader::for_trusted_backend(
            self.0.repository.clone(),
            self.0.sessions.clone(),
        )
    }

    pub fn for_project_simulation(
        repository: PgRepository,
        config: ApiConfig,
    ) -> Result<Self, ApiStartupError> {
        if config.payment_execution.is_some() {
            return Err(ApiStartupError(
                "payment execution requires explicit bank assembly",
            ));
        }
        Self::for_project_simulation_with_bank(
            repository,
            config,
            Arc::new(safeflow_mock_bank::MockBankGateway::new()),
        )
    }
    /// Trusted startup injection for local project simulation, never a request-selected source.
    /// The supplied Gateway must be a local simulation source; this does not enable funds or
    /// replace SimulationSessions with a production identity provider.
    pub fn for_project_simulation_with_bank(
        repository: PgRepository,
        config: ApiConfig,
        bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
    ) -> Result<Self, ApiStartupError> {
        Self::for_project_simulation_with_read_only_tools(repository, config, bank, vec![])
    }
    /// Adds explicitly approved read-only adapters to the same Agent and legacy read runtime.
    /// This remains PROJECT_SIMULATION; no request can register tools or supply identity.
    pub fn for_project_simulation_with_read_only_tools(
        repository: PgRepository,
        config: ApiConfig,
        bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
        registrations: Vec<crate::ReadOnlyToolRegistration>,
    ) -> Result<Self, ApiStartupError> {
        Self::build_with_bank(
            repository,
            config,
            UserId::parse("user_001").expect("existing simulation owner"),
            bank,
            &registrations,
        )
    }
    #[cfg(test)]
    fn build(
        repository: PgRepository,
        config: ApiConfig,
        owner: UserId,
    ) -> Result<Self, ApiStartupError> {
        Self::build_with_bank(
            repository,
            config,
            owner,
            Arc::new(safeflow_mock_bank::MockBankGateway::new()),
            &[],
        )
    }
    fn build_with_bank(
        repository: PgRepository,
        config: ApiConfig,
        owner: UserId,
        bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
        registrations: &[crate::ReadOnlyToolRegistration],
    ) -> Result<Self, ApiStartupError> {
        config.validate()?;
        let sessions = Arc::new(
            SimulationSessions::new(config.login_secret, owner, config.session_ttl)
                .map_err(ApiStartupError)?,
        );
        let audit = Arc::new(safeflow_runtime::InMemoryAuditRecorder::new());
        let runtime = assembly::assemble_with_read_only(
            assembly::AssemblyScope::Balance,
            sessions.clone(),
            audit.clone(),
            bank.clone(),
            None,
            registrations,
        )
        .map_err(|_| ApiStartupError("simulation Runtime unavailable"))?;
        let mut draft = config
            .draft_confirmation
            .map(|config| {
                draft_confirmation::assemble(
                    sessions.clone(),
                    audit.clone(),
                    bank.clone(),
                    config,
                    registrations,
                )
            })
            .transpose()?;
        if let (Some(payment), Some(draft)) = (&config.payment_execution, &mut draft) {
            if let Some(runtime) = draft.lifecycle_runtime.take() {
                draft.lifecycle_runtime = Some(
                    runtime
                        .with_final_payment(payment.profile.clone())
                        .map_err(ApiStartupError)?,
                );
            }
        }
        let llm_gateway = config
            .llm_gateway
            .map(safeflow_runtime::OpenAiGateway::new)
            .transpose()
            .map_err(|_| ApiStartupError("LLM Gateway configuration unavailable"))?
            .map(Arc::new);
        Ok(Self(Arc::new(Inner {
            payment_background_host: None,
            card_safety_host: None,
            unified_source: None,
            team_source: None,
            subscription_public_scope: None,
            subscription: SubscriptionService::for_trusted_backend(
                repository.clone(),
                sessions.clone(),
            ),
            card_read: CardReadService::for_trusted_backend(
                repository.clone(),
                sessions.clone(),
                config.card_read_source,
            ),
            repository,
            runtime,
            bank,
            draft,
            sessions,
            host: config.bind.to_string(),
            public_demo: config.public_demo,
            demo_controller: None,
            frontend_origin: config.frontend_origin,
            request_timeout: config.request_timeout,
            llm_gateway,
            bank_planning_read_only: false,
            conversations: Default::default(),
            read_conversations: Default::default(),
            agent_fixed_plan_enabled: config.agent_fixed_plan_enabled,
            payment_execution: config.payment_execution,
            billing_feedback: config.billing_feedback,
            #[cfg(test)]
            audit,
        })))
    }
    fn identity(&self, headers: &HeaderMap) -> Result<Identity, ApiError> {
        let reference = cookie_reference(headers)?.ok_or_else(ApiError::unauthenticated)?;
        let binding = self
            .0
            .sessions
            .resolve_binding(&reference)
            .map_err(|_| ApiError::unauthenticated())?;
        Ok(Identity { reference, binding })
    }
    async fn response(
        &self,
        identity: &Identity,
        id: &TaskId,
        replayed: bool,
    ) -> Result<TaskResponse, ApiError> {
        Ok(self.response_with_start(identity, id, replayed).await?.0)
    }
    async fn response_with_start(
        &self,
        identity: &Identity,
        id: &TaskId,
        replayed: bool,
    ) -> Result<(TaskResponse, bool), ApiError> {
        identity.check(self)?;
        let runtime = self.0.runtime.runtime();
        let view = TaskQueryService::for_trusted_backend(self.0.repository.clone(), runtime)
            .query(&identity.reference, id)
            .await
            .map_err(ApiError::storage)?;
        identity.check(self)?;
        checked_view_versions(&view)?;
        if self
            .0
            .repository
            .is_lifecycle_task(&identity.binding.user_id, id)
            .await
            .map_err(ApiError::storage)?
        {
            identity.check(self)?;
            return Ok((TaskResponse::project(view, None, replayed), false));
        }
        if billing_feedback::checked_task(self, identity, &view).await? {
            return Ok((TaskResponse::project(view, None, replayed), false));
        }
        if draft_confirmation::checked_execution_task(self, identity, &view).await? {
            return Ok((TaskResponse::project(view, None, replayed), false));
        }
        let continuation = RuntimeTaskContinuationService::for_trusted_backend(
            self.0.repository.clone(),
            runtime,
            self.0.runtime.contracts(),
        )
        .map_err(ApiError::execution)?;
        let status = match continuation
            .recover(
                &identity.reference,
                id,
                RuntimeTaskVersion {
                    task_revision: view.revision,
                    plan_version: view.plan_version,
                    facts_version: view.facts_version,
                },
            )
            .await
        {
            Ok(execution) => {
                if execution.task.revision != view.revision {
                    return Err(ApiError::conflict());
                }
                Some(execution.execution_status)
            }
            Err(safeflow_storage::RuntimeTaskExecutionError::Storage(
                StorageError::RuntimeIntakeRequired,
            )) => None,
            Err(error) => return Err(ApiError::execution(error)),
        };
        identity.check(self)?;
        let can_start = status.is_none()
            && view.task_state == TaskState::Running
            && view.revision == 1
            && view.operations.is_empty();
        Ok((TaskResponse::project(view, status, replayed), can_start))
    }
}
struct Identity {
    reference: SessionRef,
    binding: SessionBinding,
}
impl Identity {
    fn check(&self, state: &ApiState) -> Result<(), ApiError> {
        if state
            .0
            .sessions
            .resolve_binding(&self.reference)
            .map_err(|_| ApiError::unauthenticated())?
            != self.binding
        {
            return Err(ApiError::unauthenticated());
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
enum NextAction {
    Continue { node_id: NodeId },
    ReviewOriginalTask,
    Finished,
}
/// Public projection only. It contains neither stored plans nor prior authorization.
/// ```compile_fail
/// let response: safeflow_api::TaskResponse = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Serialize)]
pub struct TaskResponse {
    contract_version: u16,
    mode: &'static str,
    task_id: TaskId,
    task_state: TaskState,
    revision: u64,
    plan_version: u32,
    facts_version: u64,
    operation_state: Option<OperationState>,
    security_outcome: Option<SecurityOutcome>,
    security_scope: TaskQuerySecurityScope,
    operations: Vec<OperationProgressView>,
    recovery: TaskRecoveryHint,
    next_action: NextAction,
    replayed: bool,
}
impl TaskResponse {
    fn project(
        view: TaskProgressView,
        status: Option<RuntimeTaskExecutionStatus>,
        replayed: bool,
    ) -> Self {
        let next_action = match (&view.recovery, status) {
            (
                TaskRecoveryHint::AwaitUser { node_id },
                Some(RuntimeTaskExecutionStatus::WaitingUser),
            ) => NextAction::Continue {
                node_id: node_id.clone(),
            },
            (TaskRecoveryHint::Finished { .. }, _) => NextAction::Finished,
            _ => NextAction::ReviewOriginalTask,
        };
        Self {
            contract_version: CONTRACT_VERSION,
            mode: MODE,
            task_id: view.task_id,
            task_state: view.task_state,
            revision: view.revision,
            plan_version: view.plan_version,
            facts_version: view.facts_version,
            operation_state: if view.operations.len() == 1 {
                Some(view.operations[0].operation_state)
            } else {
                None
            },
            security_outcome: None,
            security_scope: view.security,
            operations: view.operations,
            recovery: view.recovery,
            next_action,
            replayed,
        }
    }
}
fn checked_view_versions(view: &TaskProgressView) -> Result<(), ApiError> {
    if view.revision == 0
        || view.revision > MAX_SAFE_INTEGER
        || view.plan_version == 0
        || !(1..=u32::MAX as u64).contains(&view.facts_version)
        || view.operations.iter().any(|op| {
            op.revision == 0
                || op.revision > MAX_SAFE_INTEGER
                || op.plan_version == 0
                || !(1..=u32::MAX as u64).contains(&op.facts_version)
        })
    {
        return Err(ApiError::unavailable());
    }
    Ok(())
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/bank/read-conversation", post(bank_read_conversation::message).fallback(method_rejected))
        .route("/api/bank/read-conversation/:id", get(bank_read_conversation::query).fallback(method_rejected))
        .route("/api/bank/conversation", post(bank_conversation::message).fallback(method_rejected))
        .route("/api/bank/conversation/:id", get(bank_conversation::query).fallback(method_rejected))
        .route("/api/bank/overview",get(bank_site::overview).fallback(method_rejected))
        .route("/api/bank/recovery",get(bank_site::recovery).fallback(method_rejected))
        .route("/api/bank/cards",get(bank_site::cards).fallback(method_rejected))
        .route("/api/bank/payment-draft",post(bank_site::payment_draft).fallback(method_rejected))
        .route("/api/card-safety",post(card_safety::action).fallback(method_rejected))
        .route(
            "/api/subscriptions",
            post(subscription::read).fallback(method_rejected),
        )
        .route(
            "/api/subscriptions/session",
            get(subscription::session).fallback(method_rejected),
        )
        .route(
            "/api/subscriptions/change",
            post(subscription::change).fallback(method_rejected),
        )
        .route(
            "/api/card-read",
            post(card_read::read).fallback(method_rejected),
        )
        .route(
            "/api/card-read/session",
            get(card_read::session).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/prepare",
            post(billing_feedback::prepare).fallback(method_rejected),
        )
        .route("/api/billing-feedback/revisions/prepare",post(billing_feedback::prepare_revision).fallback(method_rejected))
        .route("/api/billing-feedback/revisions/uncreated-intents/close",post(billing_feedback::close_uncreated_intent).fallback(method_rejected))
        .route("/api/billing-feedback/revisions/partial-preparations/close",post(billing_feedback::close_partial_preparation).fallback(method_rejected))
        .route("/api/billing-feedback/revisions/preparations/:nonce",get(billing_feedback::revision_preparation).fallback(method_rejected))
        .route("/api/billing-feedback/revisions/preparations/:nonce/abandon",post(billing_feedback::abandon_revision).fallback(method_rejected))
        .route("/api/billing-feedback/roots/:operation_id/head",get(billing_feedback::head).fallback(method_rejected))
        .route(
            "/api/billing-feedback/v2/context",
            get(billing_feedback::v2::context).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/v2/preparations",
            post(billing_feedback::v2::prepare).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/v2/preparations/:nonce",
            get(billing_feedback::v2::read).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/v2/operations/:operation_id/preparation",
            get(billing_feedback::v2::read_operation).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/v2/preparations/:nonce/confirm",
            post(billing_feedback::v2::confirm).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/v2/preparations/:nonce/execute",
            post(billing_feedback::v2::execute).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/v2/preparations/:nonce/abandon",
            post(billing_feedback::v2::abandon).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/tasks/:task_id/operations/:operation_id/confirm",
            post(billing_feedback::confirm).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/tasks/:task_id/operations/:operation_id/execute",
            post(billing_feedback::execute).fallback(method_rejected),
        )
        .route(
            "/api/billing-feedback/operations/:operation_id",
            get(billing_feedback::query).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id",
            post(payment_preparation::prepare)
                .get(payment_preparation::query_task)
                .fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id",
            get(payment_preparation::query).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/original-result",
            get(payment_preparation::original_result).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/execute",
            post(payment_execution::execute).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/background",
            post(payment_background::action).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/funds-result",
            get(payment_execution::query).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/execution-link",
            post(payment_preparation::register_execution)
                .get(payment_preparation::original_result)
                .fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/confirm",
            post(payment_preparation::decide).fallback(method_rejected),
        )
        .route(
            "/api/sim/payment-preparations/:task_id/:operation_id/evaluate",
            post(payment_preparation::evaluate).fallback(method_rejected),
        )
        .route(
            "/api/agent/status",
            get(agent::status).fallback(method_rejected),
        )
        .route(
            "/api/agent/messages",
            post(agent::message).fallback(method_rejected),
        )
        .route(
            "/api/agent/samples",
            post(agent::sample).fallback(method_rejected),
        )
        .route(
            "/api/agent/payees",
            post(agent::payees).fallback(method_rejected),
        )
        .route(
            "/api/agent/tasks/:task_id",
            get(agent::query).fallback(method_rejected),
        )
        .route(
            "/api/agent/tasks/:task_id/execute",
            post(agent::execute).fallback(method_rejected),
        )
        .route(
            "/api/agent/tasks/:task_id/continue",
            post(agent::continue_read).fallback(method_rejected),
        )
        .route(
            "/api/agent/tasks/:task_id/prepare",
            post(agent::prepare).fallback(method_rejected),
        )
        .route(
            "/api/team-demo/billing",
            post(team_consumption::billing).fallback(method_rejected),
        )
        .route(
            "/api/team-demo/subscriptions",
            post(team_consumption::subscriptions).fallback(method_rejected),
        )
        .route("/api/health", get(health).fallback(method_rejected))
        .route(
            "/api/sim/session",
            get(session_status)
                .post(login)
                .delete(logout)
                .fallback(method_rejected),
        )
        .route("/api/chat", post(chat).fallback(method_rejected))
        .route(
            "/api/llm/status",
            get(llm_planning::status).fallback(method_rejected),
        )
        .route(
            "/api/llm/plans",
            post(llm_planning::create).fallback(method_rejected),
        )
        .route(
            "/api/llm/plans/:task_id",
            get(llm_planning::query).fallback(method_rejected),
        )
        .route(
            "/api/llm/plans/:task_id/execute",
            post(llm_planning::execute).fallback(method_rejected),
        )
        .route("/api/tasks/:task_id", get(task).fallback(method_rejected))
        .route(
            "/api/sim/draft-lifecycles",
            post(draft_lifecycle::create).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id",
            get(draft_lifecycle::query).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id/actions",
            post(draft_lifecycle::select).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id/operations/:operation_id",
            get(draft_lifecycle::operation_query).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id/operations/:operation_id/prepare",
            post(draft_lifecycle::prepare).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id/operations/:operation_id/confirm",
            post(draft_lifecycle::confirm).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id/operations/:operation_id/execute",
            post(draft_lifecycle::execute).fallback(method_rejected),
        )
        .route("/api/bank/expire-draft-intents",post(draft_lifecycle::expire_intents).fallback(method_rejected))
        .route("/api/sim/draft-lifecycles/:task_id/operations/:operation_id/replace-intent",post(draft_lifecycle::replace_intent).fallback(method_rejected))
        .route("/api/sim/draft-lifecycles/:task_id/operations/:operation_id/end-intent",post(draft_lifecycle::abandon_intent).fallback(method_rejected))
        .route(
            "/api/sim/draft-lifecycles/:task_id/operations/:operation_id/close-nonfunding",
            post(draft_lifecycle::close_nonfunding).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-lifecycles/:task_id/operations/:operation_id/current-draft",
            get(draft_lifecycle::current).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-confirmations",
            post(draft_confirmation::create).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-confirmations/:task_id",
            get(draft_confirmation::query).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-confirmations/:task_id/prepare",
            post(draft_confirmation::prepare).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-confirmations/:task_id/execute",
            post(draft_confirmation::execute).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-executions/:task_id/operations/:operation_id",
            get(draft_confirmation::execution_query).fallback(method_rejected),
        )
        .route(
            "/api/sim/draft-executions/:task_id/operations/:operation_id/current-draft",
            get(draft_confirmation::current_query).fallback(method_rejected),
        )
        .route(
            "/api/operations/:operation_id/confirm",
            post(draft_confirmation::confirm).fallback(method_rejected),
        )
        .route(
            "/api/auth/challenges/:challenge_id/verify",
            post(disabled_mutation).fallback(method_rejected),
        )
        .route(
            "/api/audit/operations/:operation_id",
            get(disabled_read).fallback(method_rejected),
        )
        .route(
            "/api/audit/:id",
            get(disabled_read).fallback(method_rejected),
        )
        .route("/api/demo-verification", post(public_demo::display_code).get(public_demo::display_status).fallback(method_rejected))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), perimeter))
        .layer(middleware::from_fn(request_timing::observe))
        .with_state(state)
}
async fn perimeter(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host: Vec<_> = headers.get_all(header::HOST).iter().collect();
    let origin: Vec<_> = headers.get_all(header::ORIGIN).iter().collect();
    let local_valid = host.len() == 1
        && host[0].to_str().ok() == Some(state.0.host.as_str())
        && origin.len() <= 1
        && origin.first().map_or(true, |origin| {
            origin.to_str().ok().is_some_and(|value| {
                value == state.0.frontend_origin || value == format!("http://{}", state.0.host)
            })
        });
    let valid = state.0.public_demo.as_ref().map_or(local_valid, |c| c.accepts(&state.0.frontend_origin, &request));
    if !valid {
        return ApiError::forbidden().into_response();
    }
    if request.method() == Method::POST || request.method() == Method::DELETE {
        if headers.get_all("x-safeflow-request").iter().count() != 1
            || headers
                .get("x-safeflow-request")
                .and_then(|value| value.to_str().ok())
                != Some("1")
        {
            return ApiError::forbidden().into_response();
        }
        let content: Vec<_> = headers.get_all(header::CONTENT_TYPE).iter().collect();
        let json = content.len() == 1
            && content[0]
                .to_str()
                .ok()
                .and_then(|value| value.parse::<mime::Mime>().ok())
                .is_some_and(|value| value.essence_str() == "application/json");
        if !json {
            return ApiError::validation(StatusCode::UNSUPPORTED_MEDIA_TYPE).into_response();
        }
    }
    let mut response = next.run(request).await;
    if state.0.public_demo.is_some() {
        if let Some(raw) = response.headers().get(header::SET_COOKIE).and_then(|v| v.to_str().ok()) {
            let mut cookie = Cookie::parse(raw.to_owned()).map_err(|_| ()).ok();
            if let Some(cookie) = &mut cookie {
                cookie.set_secure(true);
                if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
                    response.headers_mut().insert(header::SET_COOKIE, value);
                } else { return ApiError::unavailable().into_response(); }
            } else { return ApiError::unavailable().into_response(); }
        }
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}
fn cookie_reference(headers: &HeaderMap) -> Result<Option<SessionRef>, ApiError> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().map_err(|_| ApiError::unauthenticated())?;
        if value.len() > 2048 {
            return Err(ApiError::unauthenticated());
        }
        for cookie in Cookie::split_parse(value) {
            let cookie = cookie.map_err(|_| ApiError::unauthenticated())?;
            if cookie.name() == COOKIE_NAME {
                if found.is_some() {
                    return Err(ApiError::unauthenticated());
                }
                found =
                    Some(token_reference(cookie.value()).ok_or_else(ApiError::unauthenticated)?);
            }
        }
    }
    Ok(found)
}
async fn body<T: DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    .map_err(|_| ApiError::validation(StatusCode::REQUEST_TIMEOUT))?
    .map_err(|_| ApiError::validation(StatusCode::PAYLOAD_TOO_LARGE))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
#[derive(Serialize)]
struct SessionStatus {
    mode: &'static str,
    authenticated: bool,
}
async fn session_status(State(state): State<ApiState>, headers: HeaderMap) -> Json<SessionStatus> {
    Json(SessionStatus {
        mode: MODE,
        authenticated: state.identity(&headers).is_ok(),
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    login_secret: String,
}
async fn login(State(state): State<ApiState>, request: Request) -> Result<Response, ApiError> {
    let prior = cookie_reference(request.headers())?;
    let login: Login = body(request).await?;
    let secret = Zeroizing::new(login.login_secret);
    if secret.len() > 256 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    if !state.0.sessions.check_secret(&secret){return Err(ApiError::unauthenticated());}
    if state.0.card_safety_host.is_some(){
        if let Some(prior)=prior.as_ref().filter(|p|state.0.sessions.resolve_binding(p).is_ok()){
            state.card_safety_for_backend().map_err(ApiError::storage)?.revoke_session(prior).await.map_err(ApiError::storage)?;
        }
    }
    let cookie = state
        .0
        .sessions
        .issue(&secret, prior.as_ref())
        .map_err(|error| {
            if error == "invalid login" {
                ApiError::unauthenticated()
            } else {
                ApiError::unavailable()
            }
        })?;
    let mut response = Json(SessionStatus {
        mode: MODE,
        authenticated: true,
    })
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| ApiError::unavailable())?,
    );
    Ok(response)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
async fn logout(State(state): State<ApiState>, request: Request) -> Result<Response, ApiError> {
    let identity = state.identity(request.headers())?;
    let _: Empty = body(request).await?;
    identity.check(&state)?;
    if state.0.card_safety_host.is_some(){
        state.card_safety_for_backend().map_err(ApiError::storage)?.revoke_session(&identity.reference).await.map_err(ApiError::storage)?;
    }
    state
        .0
        .sessions
        .revoke(&identity.reference)
        .map_err(|_| ApiError::unauthenticated())?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&SimulationSessions::expired_cookie())
            .map_err(|_| ApiError::unavailable())?,
    );
    Ok(response)
}
#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum ChatRequest {
    #[serde(rename = "NEW_READ_ONLY_DEMO")]
    NewReadOnlyDemo {},
    #[serde(rename = "CONTINUE")]
    Continue {
        task_id: String,
        waiting_node_id: String,
        task_revision: u64,
        plan_version: u32,
        facts_version: u64,
    },
}
async fn chat(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<TaskResponse>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ChatRequest = body(request).await?;
    identity.check(&state)?;
    match input {
        ChatRequest::NewReadOnlyDemo {} => {
            let id = new_task_id();
            let result = tokio::time::timeout(state.0.request_timeout, async {
                let runtime = state.0.runtime.runtime();
                let intake = PlanningTaskCreationService::for_trusted_backend(
                    state.0.repository.clone(),
                    runtime,
                    Some(Arc::new(MockLlm::new(assembly::proposal(&id)))),
                    state.0.runtime.contracts(),
                )
                .map_err(|_| ApiError::unavailable())?;
                let creation = intake
                    .plan_and_create(&identity.reference, &id, 2, 7, "查询我的账户余额")
                    .await;
                identity.check(&state)?;
                let initial = match creation.map_err(|error| match error {
                    PlanningTaskCreationError::Storage(error) => ApiError::storage(error),
                    PlanningTaskCreationError::Intake(
                        safeflow_runtime::PlanningIntakeError::Session,
                    ) => ApiError::unauthenticated(),
                    _ => ApiError::unavailable(),
                })? {
                    PlanningTaskCreationOutcome::TaskCreated(task) => task,
                    PlanningTaskCreationOutcome::Clarification(_) => {
                        return Err(ApiError::unavailable())
                    }
                };
                identity.check(&state)?;
                RuntimeTaskExecutionService::for_trusted_backend(
                    state.0.repository.clone(),
                    runtime,
                    state.0.runtime.contracts(),
                )
                .map_err(ApiError::execution)?
                .schedule(&identity.reference, &id, RuntimeTaskVersion::of(&initial))
                .await
                .map_err(ApiError::execution)?;
                identity.check(&state)?;
                state.response(&identity, &id, false).await
            })
            .await;
            identity
                .check(&state)
                .map_err(|error| error.with_task(&id))?;
            result
                .map_err(|_| ApiError::unavailable().with_task(&id))?
                .map(Json)
                .map_err(|error| error.with_task(&id))
        }
        ChatRequest::Continue {
            task_id,
            waiting_node_id,
            task_revision,
            plan_version,
            facts_version,
        } => {
            let id = parse_task(&task_id)?;
            let node = NodeId::parse(&waiting_node_id)
                .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
            if waiting_node_id.len() > 200
                || task_revision == 0
                || task_revision > MAX_SAFE_INTEGER
                || plan_version == 0
                || !(1..=u32::MAX as u64).contains(&facts_version)
            {
                return Err(ApiError::validation(StatusCode::BAD_REQUEST));
            }
            let result = tokio::time::timeout(state.0.request_timeout, async {
                let service = RuntimeTaskContinuationService::for_trusted_backend(
                    state.0.repository.clone(),
                    state.0.runtime.runtime(),
                    state.0.runtime.contracts(),
                )
                .map_err(ApiError::execution)?;
                let continued = service
                    .continue_task(
                        &identity.reference,
                        &id,
                        &node,
                        RuntimeTaskVersion {
                            task_revision,
                            plan_version,
                            facts_version,
                        },
                    )
                    .await
                    .map_err(ApiError::execution)?;
                identity.check(&state)?;
                state.response(&identity, &id, continued.replayed).await
            })
            .await;
            identity
                .check(&state)
                .map_err(|error| error.with_task(&id))?;
            result
                .map_err(|_| ApiError::unavailable().with_task(&id))?
                .map(Json)
                .map_err(|error| error.with_task(&id))
        }
    }
}
fn parse_task(value: &str) -> Result<TaskId, ApiError> {
    if value.len() > 200 || value.trim() != value || value.chars().any(char::is_control) {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    TaskId::parse(value).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
async fn task(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<TaskResponse>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&value)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        state.response(&identity, &id, false),
    )
    .await;
    identity
        .check(&state)
        .map_err(|error| error.with_task(&id))?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|error| error.with_task(&id))
}
#[derive(Serialize)]
struct Health {
    contract_version: u16,
    mode: &'static str,
    ready: bool,
    llm_mode: &'static str,
    funds_execution_enabled: bool,
    formal_auth_enabled: bool,
    formal_audit_enabled: bool,
}
async fn health(State(state): State<ApiState>) -> Response {
    let ready = matches!(
        tokio::time::timeout(Duration::from_secs(2), state.0.repository.check_available()).await,
        Ok(Ok(()))
    );
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(Health {
            contract_version: CONTRACT_VERSION,
            mode: MODE,
            ready,
            llm_mode: "MOCK_LLM",
            funds_execution_enabled: false,
            formal_auth_enabled: false,
            formal_audit_enabled: false,
        }),
    )
        .into_response()
}
async fn disabled_read(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.identity(&headers)?.check(&state)?;
    Err(ApiError::disabled())
}
async fn disabled_mutation(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Response, ApiError> {
    let identity = state.identity(request.headers())?;
    let _: serde_json::Value = body(request).await?;
    identity.check(&state)?;
    Err(ApiError::disabled())
}
async fn not_found() -> ApiError {
    ApiError::not_found()
}
async fn method_rejected() -> ApiError {
    ApiError::validation(StatusCode::METHOD_NOT_ALLOWED)
}

struct ApiError {
    status: StatusCode,
    reason: &'static str,
    task_id: Option<TaskId>,
}
impl ApiError {
    fn validation(status: StatusCode) -> Self {
        Self {
            status,
            reason: "VALIDATION_ERROR",
            task_id: None,
        }
    }
    fn unauthenticated() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            reason: "NOT_AUTHENTICATED",
            task_id: None,
        }
    }
    fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            reason: "FORBIDDEN",
            task_id: None,
        }
    }
    fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: "DEPENDENCY_UNAVAILABLE",
            task_id: None,
        }
    }
    fn disabled() -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            reason: "DEPENDENCY_UNAVAILABLE",
            task_id: None,
        }
    }
    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            reason: "NOT_FOUND",
            task_id: None,
        }
    }
    fn conflict() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            reason: "IDEMPOTENCY_CONFLICT",
            task_id: None,
        }
    }
    fn with_task(mut self, id: &TaskId) -> Self {
        self.task_id = Some(id.clone());
        self
    }
    fn storage(error: StorageError) -> Self {
        match error {
            StorageError::DraftIntentExpired => Self { status: StatusCode::CONFLICT, reason: "DRAFT_INTENT_EXPIRED", task_id: None },
            StorageError::DraftIntentReplaced => Self { status: StatusCode::CONFLICT, reason: "DRAFT_INTENT_REPLACED", task_id: None },
            StorageError::DraftIntentAbandoned => Self { status: StatusCode::CONFLICT, reason: "DRAFT_INTENT_ABANDONED", task_id: None },
            StorageError::Session => Self::unauthenticated(),
            StorageError::NotFound => Self::not_found(),
            StorageError::StalePlan
            | StorageError::FactsConflict
            | StorageError::PlanConflict
            | StorageError::RuntimeWaitingNodeMismatch
            | StorageError::RuntimeContinuationRequired => Self {
                status: StatusCode::CONFLICT,
                reason: "STALE_PLAN",
                task_id: None,
            },
            StorageError::RevisionConflict | StorageError::RuntimeAlreadyClaimed => {
                Self::conflict()
            }
            StorageError::Invalid(_) => Self::validation(StatusCode::BAD_REQUEST),
            _ => Self::unavailable(),
        }
    }
    fn execution(error: RuntimeTaskExecutionError) -> Self {
        match error {
            RuntimeTaskExecutionError::Storage(error) => Self::storage(error),
            RuntimeTaskExecutionError::Runtime(RuntimeError::SessionChanged) => {
                Self::unauthenticated()
            }
            RuntimeTaskExecutionError::Runtime(
                RuntimeError::SecurityGate(_) | RuntimeError::PlanRejected,
            ) => Self::forbidden(),
            _ => Self::unavailable(),
        }
    }
}
#[derive(Serialize)]
struct ErrorEnvelope {
    contract_version: u16,
    error: ErrorDetails,
}
#[derive(Serialize)]
struct ErrorDetails {
    reason_code: &'static str,
    message: &'static str,
    task_id: Option<TaskId>,
    next_action: &'static str,
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = match self.reason {
            "VALIDATION_ERROR" => "请求格式或版本无效",
            "NOT_AUTHENTICATED" => "模拟会话无效，请重新登录",
            "FORBIDDEN" => "请求来源或安全检查不允许此操作",
            "NOT_FOUND" => "任务不存在或不可查询",
            "PAYMENT_AUTHENTICATION_REQUIRED" => "本次请求未获有效付款验证，验证可能未完成或已过期；请查询原付款状态，不要另建重付",
            "PAYMENT_CODE_INACTIVE" => "本次付款验证码尚未领取、已过期、已使用或不可用；请查询原付款状态，不要另建重付",
            "PAYMENT_AUTHENTICATION_EXPIRED" => "本次请求的付款验证已过期；请查询原付款状态，不要另建重付",
            "PAYMENT_CONFIRMATION_INVALID" => "本次请求未获有效付款确认；请查询原付款状态，不要另建重付",
            "PAYMENT_PERMIT_INACTIVE" => "本次请求未获有效付款许可；请查询原付款状态，不要另建重付",
            "DRAFT_INTENT_EXPIRED" => "这份未付款草案已过期，旧确认不能继续使用；原记录保留，请查询原草案后重新办理",
            "DRAFT_INTENT_REPLACED" => "这份未付款草案已被新意图替代，旧确认不能继续使用；请查询原草案及新草案并重新确认",
            "DRAFT_INTENT_ABANDONED" => "这份未付款草案已明确放弃，旧确认不能继续使用；原记录保留，请查询原草案",
            "STALE_PLAN" | "IDEMPOTENCY_CONFLICT" => "请查询原任务并核对当前版本",
            _ => "能力未启用或结果尚不能确认，请核对原任务",
        };
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                contract_version: CONTRACT_VERSION,
                error: ErrorDetails {
                    reason_code: self.reason,
                    message,
                    next_action: if self.task_id.is_some() {
                        "QUERY_ORIGINAL_TASK"
                    } else {
                        "NONE"
                    },
                    task_id: self.task_id,
                },
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
