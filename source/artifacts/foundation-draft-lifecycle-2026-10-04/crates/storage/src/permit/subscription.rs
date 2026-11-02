//! Closed subscription composer. Only member-validated facts can seal completion.
//! A2 currently prevents every public preparation and execution before mutation.
use super::*;
use crate::repository::{encode, update_task};
use crate::subscription::{now, write_error};
use crate::{
    new_operation_id, new_task_id, ConfirmationConfig, ConfirmationRecord, ConfirmationService,
    OperationRecord, PersistentAuditKind, SubscriptionHost, SubscriptionService,
};
use safeflow_contracts::*;
use safeflow_runtime::{
    BackendFinalPolicy, BackendFinalPolicyRegistration, Runtime, ToolSecurityProfile,
};
use safeflow_subscription_evidence::{
    confirmation_mock::*,
    reminder_mock::{MockScheduleGate, Schedule, ScheduleBinding},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const TOOL: &str = "subscription_local_change";
mod domain_audit;
pub use domain_audit::{SubscriptionA2Candidate, SubscriptionA2CandidateScope, SubscriptionA2Result};
mod registration;
pub use registration::{SubscriptionRegistrationCapture, SubscriptionRegistrationSelection};
mod public_writes;
pub use public_writes::{SubscriptionPublicScope, SubscriptionPublicRequest, SubscriptionPublicWrites};
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Input {
    input_version: u16,
    business_ref: String,
    member_digest: ArgumentsHash,
    storage_version: u64,
    expires_at: String,
    public_prepared_at: String,
    action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_evidence: Option<ArgumentsHash>,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Action {
    Registration,
    RecordCancellation,
    RevokeCancellation,
    ReminderSettings,
}
#[derive(Serialize)]
pub struct SubscriptionPreparation {
    pub task: TaskRecord,
    pub operation: OperationRecord,
    pub confirmation: ConfirmationRecord,
    pub preview: Value,
}
#[derive(Clone)]
pub(super) struct SubscriptionCompletion {
    operation: OperationRecord,
    task: TaskRecord,
    host: SubscriptionHost,
    accounts: Vec<String>,
    bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
    public_scope: Option<SubscriptionPublicScope>,
}
struct CheckedFacts(SecurityContext);
struct Policy;
impl BackendFinalPolicy for Policy {
    fn evaluate<'a>(
        &'a self,
        c: &'a SecurityContext,
        f: &'a (dyn std::any::Any + Send + Sync),
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            if c.tool_name.as_str() == TOOL
                && f.downcast_ref::<CheckedFacts>().is_some_and(|f| &f.0 == c)
                && serde_json::from_value::<Input>(c.normalized_arguments.clone()).is_ok()
            {
                Ok(vec![])
            } else {
                Err(SecurityError {
                    reason_code: ReasonCode::parse("SUBSCRIPTION_FACTS_MISMATCH").unwrap(),
                    message: "trusted subscription binding required".into(),
                })
            }
        })
    }
}
pub struct SubscriptionWriteService<'a> {
    read: SubscriptionService,
    store: PermitStore,
    runtime: &'a Runtime,
    confirmations: ConfirmationService,
    config: ConfirmationConfig,
    policy: BackendFinalPolicyRegistration,
    bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
    selection_issuer: uuid::Uuid,
    candidate_activation: Option<String>,
    public_scope: Option<SubscriptionPublicScope>,
    public_request: Option<Value>,
    public_target: Option<(String, i64)>,
    #[cfg(test)]
    stage_observer: Option<
        for<'t> fn(&'t mut PermitTransaction) -> ContractFuture<'t, Result<(), StorageError>>,
    >,
}
impl<'a> SubscriptionWriteService<'a> {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        bank: Arc<dyn safeflow_bank_gateway::BankGateway>,
        runtime: &'a Runtime,
        confirmation: ConfirmationConfig,
        permit: PermitConfig,
    ) -> Result<Self, StorageError> {
        if !runtime.final_payment_uses_session_resolver_for_backend(&sessions) {
            return Err(StorageError::Session);
        }
        let policy = BackendFinalPolicyRegistration::for_reviewed_backend(
            ToolSecurityProfile {
                tool_name: ToolName::parse(TOOL).unwrap(),
                tool_schema_version: 1,
                policy_version: PolicyVersion::parse("subscription-local-binding-v1").unwrap(),
                minimum_level: SecurityLevel::Yellow,
            },
            "subscription-local-closed-v1",
            Arc::new(Policy),
        )
        .map_err(|_| StorageError::DecisionAssemblyChanged)?;
        Ok(Self {
            read: SubscriptionService::for_trusted_backend(repository.clone(), sessions.clone()),
            store: PermitStore::for_trusted_backend(repository.clone(), sessions.clone(), permit),
            confirmations: ConfirmationService::for_trusted_backend(
                repository,
                sessions,
                confirmation.clone(),
            ),
            runtime,
            config: confirmation,
            policy,
            bank,
            selection_issuer: uuid::Uuid::new_v4(),
            candidate_activation: None,
            public_scope: None,
            public_request: None,
            public_target: None,
            #[cfg(test)]
            stage_observer: None,
        })
    }
    pub async fn prepare_change(
        &self,
        s: &SessionRef,
        host: &SubscriptionHost,
        nonce_v1: &str,
        request: SubscriptionChangeRequestMock,
    ) -> Result<SubscriptionPreparation, StorageError> {
        self.read.require_write_adoption(s)?;
        self.prepare_technical(s, host, nonce_v1, Preparation::Change(request))
            .await
    }
    /// Echo's real sealed selection only. No public JSON/capture reconstruction.
    /// Same-capture selection can supply this input; A2 still rejects public writes.
    pub async fn prepare_registration_from_selection(
        &self,
        s: &SessionRef,
        host: &SubscriptionHost,
        nonce_v1: &str,
        selection: &SubscriptionRegistrationSelection,
    ) -> Result<SubscriptionPreparation, StorageError> {
        self.read.require_write_adoption(s)?;
        self.prepare_technical(s, host, nonce_v1, Preparation::Registration(selection))
            .await
    }
    async fn prepare_technical(
        &self,
        s: &SessionRef,
        host: &SubscriptionHost,
        nonce_v1: &str,
        request: Preparation<'_>,
    ) -> Result<SubscriptionPreparation, StorageError> {
        let identity = self.read.identity(s)?;
        let simulation_fence = match &request { Preparation::Registration(s)=>s.simulation.clone(), _=>None };
        if let Preparation::Registration(selection) = &request {
            selection.check_origin(self, s, &identity)?;
        }
        // Versioned trusted request identity, not an HTTP DTO or authorization.
        let nonce = uuid::Uuid::parse_str(nonce_v1)
            .map_err(|_| StorageError::Invalid("preparation nonce v1"))?;
        if nonce.is_nil() || nonce.to_string() != nonce_v1 {
            return Err(StorageError::Invalid("preparation nonce v1"));
        }
        let request_value = preparation_identity(&request)?;
        let request_value = json!({"contract":"subscription-preparation-v1","namespace":self.read.repository.namespace,
            "owner":identity.user_id,"session_digest":hash(&("safeflow-confirmation-session:v1",s.as_str()))?,
            "binding_version":identity.binding_version,"member_source":"c163","request":request_value});
        let mut writer = self.store.repository.pool.acquire().await?;
        use sqlx::Connection;
        let mut tx = writer.begin().await?;
        domain_audit::writer_lock(&mut tx).await?;
        if let Some(source) = &self.candidate_activation { domain_audit::check_source(&mut tx, identity.user_id.as_str(), Some(source)).await?; }
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        self.read
            .check_host(&mut tx, host, identity.user_id.as_str())
            .await?;
        if let Some(fence)=&simulation_fence {fence.lock(&mut tx).await?;}
        if let Some(scope)=&self.public_scope {
            public_writes::check_live(self, scope, s, &mut tx).await?;
        }
        let existing: Option<Value> = sqlx::query_scalar(
            "SELECT request_identity FROM subscription_preparations_v1 WHERE nonce=$1",
        )
        .bind(nonce_v1)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = existing {
            if existing != request_value {
                return Err(StorageError::FactsConflict);
            }
            if let Some(fence)=&simulation_fence {self.read.check_simulation_accounts(s,self.bank.as_ref(),&identity,fence.accounts()).await?;}
            self.read.check(s, &identity)?;
            tx.commit().await?;
            drop(writer);
            return self.resume_preparation(s, nonce_v1, &request_value).await;
        }
        let task_id = new_task_id();
        let operation_id = new_operation_id();
        let node = NodeId::parse("save_subscription").unwrap();
        let review = NodeId::parse("review_subscription").unwrap();
        let time = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now());
        let mut expires = time
            .checked_add_signed(self.config.ttl)
            .ok_or(StorageError::ConfirmationExpired)?;
        if let Preparation::Registration(selection) = &request {
            selection
                .check_current(self, &mut tx, host, s, &identity, time)
                .await?;
            expires = expires.min(selection.expires);
        }
        let expires = expires.to_rfc3339();
        let context = MockOperationContext {
            user_id: identity.user_id.to_string(),
            session_ref: s.to_string(),
            task_id: task_id.to_string(),
            node_id: node.to_string(),
            operation_id: operation_id.to_string(),
            idempotency_key: operation_id.to_string(),
            plan_version: 1,
        };
        let version = self
            .read
            .load(&mut tx, identity.user_id.as_str())
            .await?
            .version_mock();
        if self.public_target.as_ref().is_some_and(|(_,v)| *v != version) {
            return Err(StorageError::FactsConflict);
        }
        let store = self.read.store()?;
        let mut work = store
            .load_work_on_connection_mock(&mut tx, identity.user_id.as_str())
            .await
            .map_err(write_error)?;
        let (binding, original, preview, action) = match request {
            Preparation::Change(r) => {
                let p = work
                    .prepare_subscription_change_mock(r, context, &time.to_rfc3339(), &expires)
                    .await
                    .map_err(write_error)?;
                let (b, a) = match p.confirmation_binding_mock() {
                    SubscriptionChangeBindingMock::Intent(b) => (
                        json!(b),
                        if b.kind == MockIntentKind::Record {
                            Action::RecordCancellation
                        } else {
                            Action::RevokeCancellation
                        },
                    ),
                    SubscriptionChangeBindingMock::Settings(b) => {
                        (json!(b), Action::ReminderSettings)
                    }
                };
                (
                    b,
                    p.original_request_mock()
                        .map_err(|_| StorageError::CorruptRecord)?,
                    json!(p),
                    a,
                )
            }
            Preparation::Registration(selection) => {
                let selection = selection.take()?;
                let p = work
                    .prepare_registration_from_selection_mock(
                        selection,
                        context,
                        &time.to_rfc3339(),
                        &expires,
                    )
                    .await
                    .map_err(write_error)?;
                let b = &p.confirmation_binding;
                (
                    json!(b),
                    OriginalOperationRequestMock::registration(b)
                        .map_err(|_| StorageError::CorruptRecord)?,
                    json!(p),
                    Action::Registration,
                )
            }
        };
        let mut input = Input {
            input_version: 2,
            business_ref: uuid::Uuid::new_v4().to_string(),
            member_digest: hash(&binding)?,
            storage_version: u64::try_from(version).map_err(|_| StorageError::FactsConflict)?,
            expires_at: expires,
            public_prepared_at: time.to_rfc3339(),
            action,
            public_evidence: None,
        };
        let public_evidence = public_writes::build_evidence(self, &identity, s, nonce_v1, &input, &binding, &request_value)?;
        if let Some(body)=&public_evidence {input.public_evidence=Some(hash(body)?);}
        self.check_accounts(s, &identity, &input, &binding).await?;
        let plan = AgentPlan::new(
            task_id.clone(),
            1,
            vec![
                PlanNode::UserDecision {
                    id: review.clone(),
                    depends_on: vec![],
                },
                PlanNode::Tool {
                    id: node.clone(),
                    tool: ToolName::parse(TOOL).unwrap(),
                    arguments: json!(input),
                    depends_on: vec![review.clone()],
                },
            ],
        )
        .map_err(|_| StorageError::PlanConflict)?;
        let task = TaskRecord {
            task_id: task_id.clone(),
            owner_id: identity.user_id.clone(),
            state: TaskState::Running,
            revision: 1,
            facts_version: input.storage_version,
            plan,
            checkpoint: RecoveryPoint::AtNode {
                node_id: node.clone(),
            },
            completed_nodes: vec![review],
        };
        let operation = OperationRecord {
            operation_id: operation_id.clone(),
            task_id: task_id.clone(),
            node_id: node,
            state: OperationState::Ready,
            revision: 2,
            plan_version: 1,
            facts_version: input.storage_version,
            arguments_hash: hash(&json!(input))?,
            business_operation_ref: None,
        };
        validate_plan(&task.plan)?;
        validate_checkpoint(&task, std::slice::from_ref(&operation))?;
        sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,$2,1,'RUNNING',1,1,$3,$4,$5,$6,$7)")
            .bind(task_id.as_str()).bind(identity.user_id.as_str()).bind(db_version(input.storage_version)?).bind(Json(&task.plan))
            .bind(hash(&(task.plan.clone(),task.facts_version))?.as_str()).bind(Json(&task.checkpoint)).bind(Json(&task.completed_nodes)).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'READY',2,1,$4,$5)")
            .bind(operation_id.as_str()).bind(task_id.as_str()).bind(operation.node_id.as_str()).bind(db_version(input.storage_version)?).bind(operation.arguments_hash.as_str()).execute(&mut *tx).await?;
        audit(
            &mut tx,
            &task,
            None,
            PersistentAuditKind::TaskCreated,
            json!({"source":"subscription-local-v1","plan_version":1}),
        )
        .await?;
        audit(
            &mut tx,
            &task,
            Some(&operation),
            PersistentAuditKind::OperationCreated,
            json!({"state":"READY","business_ref":input.business_ref}),
        )
        .await?;
        save_binding(
            &mut tx,
            &identity,
            s,
            host,
            &input,
            &binding,
            &original,
            &operation_id,
        )
        .await?;
        if let Some(body)=&public_evidence {
            public_writes::save_evidence(&mut tx, &operation_id, &input, body).await?;
        }
        sqlx::query("INSERT INTO subscription_preparations_v1(nonce,request_identity,business_ref,prepared_task,prepared_operation,preview) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(nonce_v1).bind(Json(&request_value)).bind(&input.business_ref)
            .bind(Json(&task)).bind(Json(&operation)).bind(Json(&preview)).execute(&mut *tx).await?;
        let pending = store
            .save_on_connection_mock(&mut tx, work)
            .await
            .map_err(write_error)?;
        if let Some(fence)=&simulation_fence {
            fence.lock(&mut tx).await?;
            self.read.check_host(&mut tx,host,identity.user_id.as_str()).await?;
            self.read.check_simulation_accounts(s,self.bank.as_ref(),&identity,fence.accounts()).await?;
        }
        if let Some(scope)=&self.public_scope {public_writes::check_live(self,scope,s,&mut tx).await?;}
        self.read.check(s, &identity)?;
        MemberDeadline::from_binding(&input, &binding)?
            .check(chrono::Utc::now().timestamp_millis())?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        let mut independent = self
            .store
            .repository
            .subscription_read_pool
            .acquire()
            .await?;
        let saved = pending
            .read_committed_on_connection_mock(&mut independent)
            .await
            .map_err(write_error)?;
        // The original is saved, but remains NEEDS_REVIEW until separately confirmed.
        saved
            .original_result_mock(&original)
            .map_err(|_| StorageError::CorruptRecord)?;
        drop(independent);
        drop(writer);
        self.resume_preparation(s, nonce_v1, &request_value).await
    }
    // Independent complete member read and immutable original association are
    // checked on EVERY resume, including after a committed response was lost.
    async fn resume_preparation(
        &self,
        s: &SessionRef,
        nonce: &str,
        expected: &Value,
    ) -> Result<SubscriptionPreparation, StorageError> {
        let identity = self.read.identity(s)?;
        let mut tx = self.store.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT p.*, b.operation_id FROM subscription_preparations_v1 p JOIN subscription_public_bindings_v1 b USING(business_ref) WHERE p.nonce=$1 AND b.owner_id=$2")
            .bind(nonce).bind(identity.user_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
        if row.try_get::<Value, _>("request_identity")? != *expected {
            return Err(StorageError::FactsConflict);
        }
        let task: TaskRecord = serde_json::from_value(row.try_get("prepared_task")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        let operation: OperationRecord = serde_json::from_value(row.try_get("prepared_operation")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        if operation.operation_id.as_str() != row.try_get::<String, _>("operation_id")?
            || operation.task_id != task.task_id
            || task.owner_id != identity.user_id
        {
            return Err(StorageError::CorruptRecord);
        }
        let (input, member, original) =
            load_binding(&mut tx, &identity, s, &operation.operation_id).await?;
        if input.business_ref != row.try_get::<String, _>("business_ref")?
            || operation.arguments_hash != hash(&json!(input))?
        {
            return Err(StorageError::CorruptRecord);
        }
        let deadline = MemberDeadline::from_binding(&input, &member)?;
        self.check_accounts(s, &identity, &input, &member).await?;
        if input.action == Action::Registration
            && expected["request"]["scope"] != registration::scope(&mut tx).await?
        {
            return Err(StorageError::FactsConflict);
        }
        let snapshot = self.read.load(&mut tx, identity.user_id.as_str()).await?;
        snapshot
            .original_result_mock(&original)
            .map_err(|_| StorageError::CorruptRecord)?;
        let preview: Value = row.try_get("preview")?;
        self.read.check(s, &identity)?;
        tx.commit().await?;
        let confirmation = self
            .confirmations
            .prepare_capped(
                s,
                &task.task_id,
                &operation.operation_id,
                deadline.prepared,
                deadline.expires,
            )
            .await?;
        self.read.check(s, &identity)?;
        Ok(SubscriptionPreparation {
            task,
            operation,
            confirmation,
            preview,
        })
    }
    pub async fn confirm(
        &self,
        s: &SessionRef,
        task: &TaskId,
        confirmation: &ConfirmationRef,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.read.require_write_adoption(s)?;
        self.confirm_technical(s, task, confirmation).await
    }
    async fn confirm_technical(&self,s:&SessionRef,task:&TaskId,confirmation:&ConfirmationRef)->Result<ConfirmationRecord,StorageError>{
        let original = self.confirmations.read(s, task, confirmation).await?;
        if original.binding.tool_name.as_str() != TOOL {
            return Err(StorageError::NotFound);
        }
        self.confirmations
            .record_explicit_confirmation(s, task, confirmation, &original.binding)
            .await
    }
    pub async fn execute(
        &self,
        s: &SessionRef,
        host: &SubscriptionHost,
        task: &TaskId,
        op: &OperationId,
        confirmation: &ConfirmationRef,
    ) -> Result<Value, StorageError> {
        self.read.require_write_adoption(s)?;
        self.execute_technical(s, host, task, op, confirmation)
            .await
    }
    async fn execute_technical(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        task: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
    ) -> Result<Value, StorageError> {
        let identity = self.store.identity(session)?;
        let mut unit = self.store.open(session, task).await?;
        domain_audit::writer_lock(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?).await?;
        if let Some(source) = &self.candidate_activation { domain_audit::check_source(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?, identity.user_id.as_str(), Some(source)).await?; }
        let ops = operations(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            task,
        )
        .await?;
        let current = ops
            .iter()
            .find(|o| o.operation_id == *operation)
            .ok_or(StorageError::NotFound)?;
        // Read-only replay requires the exact original confirmation/session; never consumes again.
        if current.state == OperationState::Succeeded {
            let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
            let (input, _, original) = load_binding(tx, &identity, session, operation).await?;
            let found:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=$1 AND confirmation_ref=$2 AND status='CONSUMED')")
                .bind(operation.as_str()).bind(confirmation.as_str()).fetch_one(&mut **tx).await?;
            if !found {
                return Err(StorageError::NotFound);
            }
            let saved = self.read.load(tx, identity.user_id.as_str()).await?;
            domain_audit::verify_link(tx, &identity.user_id, operation, &self.read).await?;
            let result = json!(saved
                .original_view_mock(&original)
                .map_err(|_| StorageError::CorruptRecord)?);
            unit.rollback().await;
            self.read.check(session, &identity)?;
            let _ = input;
            return Ok(result);
        }
        let binding = current_binding(&unit.task, &ops, operation)?;
        if binding.tool_name.as_str() != TOOL {
            return Err(StorageError::PermitBindingMismatch);
        }
        let (input, member, original) = load_binding(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            &identity,
            session,
            operation,
        )
        .await?;
        if let Some(scope)=&self.public_scope {
            let tx=unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
            public_writes::check_live(self,scope,session,tx).await?;
            public_writes::verify_evidence(tx, &identity.user_id, operation, Some(scope)).await?;
            public_writes::check_prepared_version(tx,&identity.user_id,&input).await?;
        }
        self.check_accounts(session, &identity, &input, &member)
            .await?;
        let node = unit
            .task
            .plan
            .nodes
            .iter()
            .find(|n| n.id() == &binding.node_id)
            .ok_or(StorageError::CorruptRecord)?;
        if !matches!(node,PlanNode::Tool{arguments,..} if *arguments==json!(input)) {
            return Err(StorageError::PermitBindingMismatch);
        }
        self.read
            .check_host(
                unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
                host,
                identity.user_id.as_str(),
            )
            .await?;
        let payload = FingerprintPayloadV1 {
            user_id: identity.user_id.clone(),
            operation_id: operation.clone(),
            tool_name: binding.tool_name.clone(),
            resource_id: ResourceId::parse(&input.business_ref)
                .map_err(|_| StorageError::CorruptRecord)?,
            normalized_arguments: json!(input),
            plan_version: binding.plan_version,
            schema_version: 1,
        };
        let request = ExecutionRequest {
            idempotency_key: IdempotencyKey::parse(operation.as_str())
                .map_err(|_| StorageError::CorruptRecord)?,
            binding: crate::PermitBinding {
                snapshot: binding.clone(),
                fingerprint: compute_operation_fingerprint(&payload)
                    .map_err(|_| StorageError::CorruptRecord)?,
            },
            payload: payload.clone(),
            business_operation_ref: BusinessOperationRef::parse(&input.business_ref)
                .map_err(|_| StorageError::CorruptRecord)?,
            business_version: 1,
        };
        let context = SecurityContext {
            contract_version: SECURITY_CONTRACT_VERSION,
            phase: SecurityPhase::PreCommit,
            task_id: task.clone(),
            node_id: binding.node_id.clone(),
            operation_id: operation.clone(),
            facts_version: u32::try_from(binding.facts_version)
                .map_err(|_| StorageError::FactsConflict)?,
            tool_name: binding.tool_name.clone(),
            plan_version: binding.plan_version,
            authenticated_user_id: identity.user_id.clone(),
            normalized_arguments: json!(input),
            arguments_hash: binding.arguments_hash.clone(),
            idempotency_key: Some(request.idempotency_key.clone()),
            confirmation_ref: Some(confirmation.clone()),
            auth_grant_ref: None,
        };
        let decision = self
            .runtime
            .final_nonfunding_policy_for_backend(
                session,
                &context,
                &self.policy,
                &CheckedFacts(context.clone()),
            )
            .await
            .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        if decision.outcome != SecurityOutcome::ReadyToExecute
            || decision.required_gates != vec![SecurityGate::ExplicitConfirmation]
        {
            return Err(StorageError::FormalAuthUnavailable);
        }
        let verified = VerifiedPermitSnapshot {
            binding,
            payload,
            decision_id: decision.decision_id.clone(),
            confirmation_ref: confirmation.clone(),
            required_gates: decision.required_gates.clone(),
            authentication: VerifiedPermitAuthentication::NotRequired,
            execution_scope: VerifiedPermitScope::Common,
            session_digest: unit.session_digest()?,
            session_version: identity.binding_version,
        };
        audit(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,&unit.task,None,PersistentAuditKind::SecurityDecisionRecorded,json!({"scope":"SUBSCRIPTION_LOCAL_V1","decision":decision,"business_ref":input.business_ref})).await?;
        let permit = unit.save(&verified).await?;
        unit.consume(&permit.id, &request.binding).await?;
        let pending = stage(
            &mut unit, &self.read, host, &input, &member, &original, &request,
        )
        .await?;
        self.check_accounts(session, &identity, &input, &member)
            .await?;
        unit.record_submission(&request).await?;
        #[cfg(test)]
        if let Some(observer) = self.stage_observer {
            observer(&mut unit).await?;
        }
        let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        // Default/public paths remain closed. The sealed isolated candidate is
        // the only native host that can reach the same real transaction chain.
        if self.candidate_activation.is_none() { return Err(StorageError::DependencyUnavailable); }
        let task_record = lock_task(tx, &identity.user_id, task).await?;
        let mut op = operations(tx, task)
            .await?
            .into_iter()
            .find(|op| op.operation_id == *operation)
            .ok_or(StorageError::NotFound)?;
        let previous_revision = op.revision;
        op.state = op
            .state
            .transition(OperationEvent::ExecutionSucceeded)
            .map_err(|_| StorageError::IllegalTransition)?;
        op.revision = op
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        sqlx::query("UPDATE operations SET state=$1,revision=$2,updated_at=clock_timestamp() WHERE operation_id=$3 AND revision=$4")
            .bind(encode(op.state)?).bind(db_version(op.revision)?).bind(operation.as_str()).bind(db_version(previous_revision)?).execute(&mut **tx).await?;
        // Existing public audit trigger atomically enqueues the raw fact. This
        // is NOT an adopted C-AUD domain event, external delivery or ACK.
        audit(tx, &task_record, Some(&op), PersistentAuditKind::OperationTransitioned, json!({"event":"EXECUTION_SUCCEEDED","source":"subscription-local-v1","business_ref":input.business_ref,"lock_authorized":false,"domain_audit_mapping_adopted":false})).await?;
        verify_result(tx, &identity.user_id, &request, &self.read).await?;
        // The actual local result already resolved this single action. Apply
        // existing state-machine events in THIS transaction; no phantom worker.
        let mut resolved = task_record.clone();
        resolved.state = resolved
            .state
            .transition(TaskEvent::ExternalResolved)
            .map_err(|_| StorageError::IllegalTransition)?;
        resolved.revision = resolved
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        resolved.checkpoint = RecoveryPoint::AtNode {
            node_id: op.node_id.clone(),
        };
        validate_checkpoint(&resolved, std::slice::from_ref(&op))?;
        update_task(tx, &resolved, task_record.revision).await?;
        audit(tx,&resolved,None,PersistentAuditKind::CheckpointSaved,json!({"event":"EXTERNAL_RESOLVED","from":task_record.state,"to":resolved.state,"source":"subscription-local-v1","operation_id":op.operation_id,"checkpoint":resolved.checkpoint,"completed_nodes":resolved.completed_nodes})).await?;
        let mut completed = resolved.clone();
        completed.state = completed
            .state
            .transition(TaskEvent::Complete)
            .map_err(|_| StorageError::IllegalTransition)?;
        completed.revision = completed
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        completed.completed_nodes = completed
            .plan
            .nodes
            .iter()
            .map(|n| n.id().clone())
            .collect();
        completed.checkpoint = RecoveryPoint::Finished;
        validate_checkpoint(&completed, std::slice::from_ref(&op))?;
        update_task(tx, &completed, resolved.revision).await?;
        audit(tx,&completed,None,PersistentAuditKind::CheckpointSaved,json!({"event":"COMPLETE","from":resolved.state,"to":completed.state,"source":"subscription-local-v1","operation_id":op.operation_id,"checkpoint":completed.checkpoint,"completed_nodes":completed.completed_nodes})).await?;
        domain_audit::append(tx, &identity.user_id, operation, &self.read).await?;
        unit.subscription_result = Some(SubscriptionCompletion {
            operation: op, task: completed, host: host.clone(),
            accounts: if let Some(scope)=&self.public_scope { scope.fence.accounts().to_vec() } else if input.action == Action::Registration { serde_json::from_value(member["requested_account_ids"].clone()).map_err(|_|StorageError::CorruptRecord)? } else { vec![] },
            bank: self.bank.clone(),
            public_scope: self.public_scope.clone(),
        });

        self.check_accounts(session, &identity, &input, &member)
            .await?;
        // Reserve a distinct backend BEFORE releasing the writer transaction.
        let mut reader = self
            .store
            .repository
            .subscription_read_pool
            .acquire()
            .await?;
        unit.finish(true).await?;
        let committed = pending
            .read_committed_on_connection_mock(&mut reader)
            .await
            .map_err(write_error)?;
        verify_receipt(&committed, &original, &member, &input)?;
        self.read.check(session, &identity)?;
        Ok(json!(committed
            .original_view_mock(&original)
            .map_err(|_| StorageError::CorruptRecord)?))
    }
    async fn check_accounts(
        &self,
        s: &SessionRef,
        identity: &SessionBinding,
        input: &Input,
        binding: &Value,
    ) -> Result<(), StorageError> {
        self.read.check(s, identity)?;
        if input.action == Action::Registration {
            let accounts: Vec<String> =
                serde_json::from_value(binding["requested_account_ids"].clone())
                    .map_err(|_| StorageError::CorruptRecord)?;
            if accounts.is_empty() {
                return Err(StorageError::CorruptRecord);
            }
            for account in accounts {
                let id = ResourceId::parse(&account).map_err(|_| StorageError::CorruptRecord)?;
                let result = self.bank.get_account_balance(&identity.user_id, &id).await;
                self.read.check(s, identity)?;
                result
                    .map_err(|_| StorageError::NotFound)?
                    .validate_for(&id)
                    .map_err(|_| StorageError::CorruptRecord)?;
            }
        }
        self.read.check(s, identity)
    }
}
enum Preparation<'a> {
    Change(SubscriptionChangeRequestMock),
    Registration(&'a SubscriptionRegistrationSelection),
}

async fn save_binding(
    tx: &mut Transaction<'_, Postgres>,
    identity: &SessionBinding,
    s: &SessionRef,
    host: &SubscriptionHost,
    input: &Input,
    binding: &Value,
    original: &OriginalOperationRequestMock,
    operation: &OperationId,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO subscription_public_bindings_v1(business_ref,owner_id,operation_id,idempotency_key,session_digest,binding_version,writer_generation,member_binding,original_request,public_parameters) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(&input.business_ref).bind(identity.user_id.as_str()).bind(operation.as_str()).bind(operation.as_str())
        .bind(hash(&("safeflow-confirmation-session:v1",s.as_str()))?.as_str()).bind(db_version(identity.binding_version)?).bind(host.generation)
        .bind(Json(binding)).bind(Json(original)).bind(Json(input)).execute(&mut **tx).await?;
    Ok(())
}
async fn load_binding(
    c: &mut PgConnection,
    identity: &SessionBinding,
    s: &SessionRef,
    operation: &OperationId,
) -> Result<(Input, Value, OriginalOperationRequestMock), StorageError> {
    let row = sqlx::query(
        "SELECT * FROM subscription_public_bindings_v1 WHERE owner_id=$1 AND operation_id=$2",
    )
    .bind(identity.user_id.as_str())
    .bind(operation.as_str())
    .fetch_optional(c)
    .await?
    .ok_or(StorageError::NotFound)?;
    if row.try_get::<String, _>("session_digest")?
        != hash(&("safeflow-confirmation-session:v1", s.as_str()))?.as_str()
        || row.try_get::<i64, _>("binding_version")? != db_version(identity.binding_version)?
    {
        return Err(StorageError::NotFound);
    }
    let raw: Value = row.try_get("public_parameters")?;
    let input: Input =
        serde_json::from_value(raw.clone()).map_err(|_| StorageError::CorruptRecord)?;
    let member: Value = row.try_get("member_binding")?;
    if input.input_version != 2
        || json!(input) != raw
        || hash(&member)? != input.member_digest
        || input.business_ref != row.try_get::<String, _>("business_ref")?
    {
        return Err(StorageError::CorruptRecord);
    }
    let original = serde_json::from_value(row.try_get("original_request")?)
        .map_err(|_| StorageError::CorruptRecord)?;
    Ok((input, member, original))
}
/// This private gate can be created only after a consumed real public permit
/// and a complete context/fingerprint/immutable association comparison.
struct Gate {
    expected: Value,
    time: String,
    used: bool,
}
impl Gate {
    fn check(&mut self, actual: Value, time: Option<&str>) -> MockGateOutcome {
        if self.used || actual != self.expected || time.is_some_and(|s| s != self.time) {
            return MockGateOutcome::Denied;
        }
        self.used = true;
        MockGateOutcome::Confirmed
    }
}
impl MockGateVerifier for Gate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        b: &MockConfirmationBinding,
        t: &str,
    ) -> MockGateOutcome {
        self.check(json!(b), Some(t))
    }
}
impl MockIntentGateVerifier for Gate {
    fn verify_and_consume_mock_intent(
        &mut self,
        b: &MockIntentBinding,
        t: &str,
    ) -> MockGateOutcome {
        self.check(json!(b), Some(t))
    }
}
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
        self.check(json!(b), None)
    }
}

/// Deserialize only the host's stored settings binding, then roundtrip-compare
/// every field. The member independently validates it and passes it to Gate.
fn settings(raw: &Value) -> Result<ScheduleBinding, StorageError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Stored {
        minimum_level: String,
        context: MockOperationContext,
        target: safeflow_subscription_evidence::managed_ledger_mock::LedgerEntry,
        ledger_revision: u64,
        settings_revision: u64,
        schedule: StoredSchedule,
        prepared_at: String,
        expires_at: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredSchedule {
        cycle: safeflow_subscription_evidence::recognition::Cycle,
        anchor_date: chrono::NaiveDate,
        period_index: u32,
        expected_date: chrono::NaiveDate,
        enabled: bool,
        days_before: u8,
    }
    let b: Stored = serde_json::from_value(raw.clone()).map_err(|_| StorageError::CorruptRecord)?;
    if b.minimum_level != "YELLOW" {
        return Err(StorageError::CorruptRecord);
    }
    let binding = ScheduleBinding {
        minimum_level: "YELLOW",
        context: b.context,
        target: b.target,
        ledger_revision: b.ledger_revision,
        settings_revision: b.settings_revision,
        schedule: Schedule {
            cycle: b.schedule.cycle,
            anchor_date: b.schedule.anchor_date,
            period_index: b.schedule.period_index,
            expected_date: b.schedule.expected_date,
            enabled: b.schedule.enabled,
            days_before: b.schedule.days_before,
        },
        prepared_at: b.prepared_at,
        expires_at: b.expires_at,
    };
    if json!(binding) != *raw {
        return Err(StorageError::CorruptRecord);
    }
    Ok(binding)
}
async fn stage(
    unit: &mut PermitTransaction,
    read: &SubscriptionService,
    host: &SubscriptionHost,
    input: &Input,
    member: &Value,
    original: &OriginalOperationRequestMock,
    request: &ExecutionRequest,
) -> Result<PendingProposedLedgerWriteMock, StorageError> {
    let consumed = unit
        .consumed
        .as_ref()
        .ok_or(StorageError::TransactionAborted)?;
    let context: MockOperationContext = serde_json::from_value(member["context"].clone())
        .map_err(|_| StorageError::CorruptRecord)?;
    let b = &request.binding.snapshot;
    if consumed.binding != request.binding
        || b.tool_name.as_str() != TOOL
        || b.user_id != unit.identity.user_id
        || context.user_id != b.user_id.as_str()
        || context.session_ref != unit.session.as_str()
        || context.task_id != b.task_id.as_str()
        || context.node_id != b.node_id.as_str()
        || context.operation_id != b.operation_id.as_str()
        || context.idempotency_key != request.idempotency_key.as_str()
        || context.plan_version != b.plan_version
        || input.member_digest != hash(member)?
        || json!(input) != request.payload.normalized_arguments
        || member["minimum_level"] != "YELLOW"
        || member["expires_at"] != input.expires_at
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    let owner = unit.identity.user_id.to_string();
    read.check_host(unit.connection()?, host, &owner).await?;
    let store = read.store()?;
    let mut work = store
        .load_work_on_connection_mock(unit.connection()?, &owner)
        .await
        .map_err(write_error)?;
    let time = now();
    let mut gate = Gate {
        expected: member.clone(),
        time: time.clone(),
        used: false,
    };
    match input.action {
        Action::Registration => work
            .confirm_registration_mock(
                member["draft_id"]
                    .as_str()
                    .ok_or(StorageError::CorruptRecord)?,
                &context,
                &time,
                &mut gate,
            )
            .await
            .map_err(write_error)?,
        Action::RecordCancellation | Action::RevokeCancellation => {
            let kind = if input.action == Action::RecordCancellation {
                "RECORD"
            } else {
                "REVOKE"
            };
            if member["kind"] != kind {
                return Err(StorageError::PermitBindingMismatch);
            }
            work.confirm_intent_mock(
                member["draft_id"]
                    .as_str()
                    .ok_or(StorageError::CorruptRecord)?,
                &context,
                &time,
                &mut gate,
            )
            .await
            .map_err(write_error)?;
        }
        Action::ReminderSettings => work
            .confirm_settings_mock(&settings(member)?, &time, &mut gate)
            .await
            .map_err(write_error)?,
    }
    if !gate.used {
        return Err(StorageError::PermitBindingMismatch);
    }
    let pending = store
        .save_on_connection_mock(unit.connection()?, work)
        .await
        .map_err(write_error)?;
    let saved = read.load(unit.connection()?, &owner).await?;
    verify_receipt(&saved, original, member, input)?;
    Ok(pending)
}
fn verify_receipt(
    snapshot: &SubscriptionReadSnapshotMock,
    original: &OriginalOperationRequestMock,
    member: &Value,
    input: &Input,
) -> Result<(), StorageError> {
    let raw = json!(original);
    let kind = match input.action {
        Action::Registration => "Register",
        Action::RecordCancellation | Action::RevokeCancellation => "Intent",
        Action::ReminderSettings => "Settings",
    };
    if raw["command_kind"] != kind
        || raw["original_binding"] != *member
        || hash(member)? != input.member_digest
    {
        return Err(StorageError::CorruptRecord);
    }
    let OriginalResultMock::Recorded { original_receipt } = snapshot
        .original_result_mock(original)
        .map_err(|_| StorageError::CorruptRecord)?
    else {
        return Err(StorageError::ReconcileRequired);
    };
    let c: MockOperationContext = serde_json::from_value(member["context"].clone())
        .map_err(|_| StorageError::CorruptRecord)?;
    let receipt = match input.action {
        Action::Registration => json!(snapshot
            .ledger_mock()
            .confirmation_result_mock(&c.user_id, &c.operation_id)
            .map_err(|_| StorageError::CorruptRecord)?),
        Action::RecordCancellation | Action::RevokeCancellation => json!(snapshot
            .ledger_mock()
            .intent_result_mock(&c.user_id, &c.operation_id)
            .map_err(|_| StorageError::CorruptRecord)?),
        Action::ReminderSettings => json!(snapshot
            .reminders_mock()
            .settings_result_mock(snapshot.ledger_mock(), &c.user_id, &c.operation_id)
            .map_err(|_| StorageError::CorruptRecord)?),
    };
    if receipt != original_receipt
        || receipt["operation_id"] != c.operation_id
        || receipt["fictional"] != true
        || receipt["production_registered"] != false
        || receipt["notification_sent"] != false
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}
async fn verify_result(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    request: &ExecutionRequest,
    read: &SubscriptionService,
) -> Result<(), StorageError> {
    let row=sqlx::query("SELECT member_binding,original_request,public_parameters FROM subscription_public_bindings_v1 WHERE business_ref=$1 AND owner_id=$2 AND operation_id=$3")
        .bind(request.business_operation_ref.as_str()).bind(owner.as_str()).bind(request.binding.snapshot.operation_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    let input: Input = serde_json::from_value(row.try_get("public_parameters")?)
        .map_err(|_| StorageError::CorruptRecord)?;
    if json!(input) != request.payload.normalized_arguments
        || request.payload.resource_id.as_str() != input.business_ref
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    let member: Value = row.try_get("member_binding")?;
    let original = serde_json::from_value(row.try_get("original_request")?)
        .map_err(|_| StorageError::CorruptRecord)?;
    let snapshot = read.load(tx, owner.as_str()).await?;
    verify_receipt(&snapshot, &original, &member, &input)
}
#[cfg(test)]
#[path = "../subscription_tests.rs"]
mod tests;
pub(super) async fn verify_completion(
    unit: &mut PermitTransaction,
    consumed: &PermitRow,
) -> Result<(), StorageError> {
    let proof = unit
        .subscription_result
        .clone()
        .ok_or(StorageError::ReconcileRequired)?;
    let (request, receipt) = unit
        .submission
        .as_ref()
        .ok_or(StorageError::ReconcileRequired)?;
    let b = &consumed.binding.snapshot;
    let mut expected = unit.task.clone();
    expected.state = expected
        .state
        .transition(TaskEvent::AwaitExternal)
        .and_then(|s| s.transition(TaskEvent::ExternalResolved))
        .and_then(|s| s.transition(TaskEvent::Complete))
        .map_err(|_| StorageError::PermitBindingMismatch)?;
    expected.revision = expected
        .revision
        .checked_add(3)
        .ok_or(StorageError::RevisionConflict)?;
    expected.checkpoint = RecoveryPoint::Finished;
    expected.completed_nodes = expected.plan.nodes.iter().map(|n| n.id().clone()).collect();
    let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
    let task = lock_task(tx, &unit.identity.user_id, &b.task_id).await?;
    let ops = operations(tx, &b.task_id).await?;
    let op = ops.first().ok_or(StorageError::CorruptRecord)?;
    validate_plan(&task.plan)?;
    validate_checkpoint(&task, &ops)?;
    let stored = crate::execution_request::request_locked(tx, request).await?;
    if task != expected
        || proof.task != task
        || ops.len() != 1
        || task.plan.nodes.len() != 2
        || proof.operation != *op
        || op.state != OperationState::Succeeded
        || op.task_id != b.task_id
        || op.node_id != b.node_id
        || op.operation_id != b.operation_id
        || op.plan_version != b.plan_version
        || op.facts_version != b.facts_version
        || op.arguments_hash != b.arguments_hash
        || Some(op.revision) != b.operation_revision.checked_add(2)
        || op.business_operation_ref.as_ref() != Some(&request.business_operation_ref)
        || b.tool_name.as_str() != TOOL
        || consumed.execution_scope != Some(VerifiedPermitScope::Common)
        || request.binding != consumed.binding
        || stored.first_submission.as_ref() != Some(receipt)
        || receipt.operation.state != OperationState::Submitted
        || Some(receipt.operation.revision) != b.operation_revision.checked_add(1)
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    let read = crate::SubscriptionService::for_trusted_backend(
        unit.store.repository.clone(),
        unit.store.sessions.clone(),
    );
    verify_result(tx, &unit.identity.user_id, request, &read).await?;
    domain_audit::verify_link(tx, &unit.identity.user_id, &b.operation_id, &read).await?;
    Ok(())
}
pub(crate) async fn domain_original_check(tx:&mut Transaction<'_,Postgres>, owner:&UserId, operation:&OperationId, read:&SubscriptionService)->Result<(),StorageError>{
    domain_audit::verify_existing(tx,owner,operation,read).await
}
pub(super) async fn final_scope(unit:&mut PermitTransaction)->Result<(),StorageError>{
    let proof=unit.subscription_result.clone().ok_or(StorageError::ReconcileRequired)?;
    if let Some(scope)=&proof.public_scope {
        scope.fence.lock(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?).await?;
        public_writes::verify_evidence(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?, &unit.identity.user_id, &proof.operation.operation_id, Some(scope)).await?;
    }
    for a in &proof.accounts { let id=ResourceId::parse(a).map_err(|_|StorageError::CorruptRecord)?;let r=proof.bank.get_account_balance(&unit.identity.user_id,&id).await;unit.session_guard()?;r.map_err(|_|StorageError::NotFound)?.validate_for(&id).map_err(|_|StorageError::CorruptRecord)?; }
    let read=SubscriptionService::for_trusted_backend(unit.store.repository.clone(),unit.store.sessions.clone());
    read.check_host(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,&proof.host,unit.identity.user_id.as_str()).await?;
    unit.session_guard()?;
    let p=unit.consumed.clone().ok_or(StorageError::ReconcileRequired)?;let digest=unit.session_digest()?;
    unit.scope_guard().await?;
    verify_stored_confirmation(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?, &p.confirmation_ref,&p.binding.snapshot,&digest,unit.identity.binding_version,unit.store.clock.as_ref()).await?;
    let deadline=deadline_for_permit(unit).await?.ok_or(StorageError::ReconcileRequired)?;
    unit.session_guard()?;let now=unit.now()?;
    // `unit.consumed` deliberately retains the originally authorised ACTIVE
    // snapshot. final_guard/verify_completion already read the CONSUMED DB row.
    p.active_time(now)?;
    deadline.check(now)
}

#[derive(Clone)]
pub(super) struct MemberDeadline {
    prepared: chrono::DateTime<chrono::Utc>,
    expires: chrono::DateTime<chrono::Utc>,
}
impl MemberDeadline {
    fn from_binding(input: &Input, member: &Value) -> Result<Self, StorageError> {
        if input.input_version != 2
            || hash(member)? != input.member_digest
            || member["expires_at"] != input.expires_at
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        let parse = |v: &str| {
            chrono::DateTime::parse_from_rfc3339(v)
                .map(|v| v.with_timezone(&chrono::Utc))
                .map_err(|_| StorageError::CorruptRecord)
        };
        // Registration's exact member binding has no prepared_at. This separate
        // immutable public fact is allocated once, inside B/nonce's transaction.
        let prepared = parse(&input.public_prepared_at)?;
        match input.action {
            Action::Registration => {
                let fields = [
                    "minimum_level",
                    "context",
                    "draft_id",
                    "ledger_revision",
                    "requested_account_ids",
                    "as_of",
                    "expires_at",
                    "candidate",
                ];
                let object = member.as_object().ok_or(StorageError::CorruptRecord)?;
                if object.len() != fields.len() || fields.iter().any(|f| !object.contains_key(*f)) {
                    return Err(StorageError::CorruptRecord);
                }
            }
            _ => {
                if member["prepared_at"].as_str() != Some(input.public_prepared_at.as_str()) {
                    return Err(StorageError::PermitBindingMismatch);
                }
            }
        }
        let expires = parse(&input.expires_at)?;
        if expires <= prepared {
            return Err(StorageError::CorruptRecord);
        }
        Ok(Self { prepared, expires })
    }
    pub(super) fn check(&self, now_ms: i64) -> Result<(), StorageError> {
        if now_ms < self.prepared.timestamp_millis() {
            return Err(StorageError::Invalid("subscription clock moved backwards"));
        }
        // Round expiry down, so millisecond permit clocks never extend it.
        if now_ms >= self.expires.timestamp_millis() {
            return Err(StorageError::ConfirmationExpired);
        }
        Ok(())
    }
}
pub(super) async fn deadline_for_permit(
    unit: &mut PermitTransaction,
) -> Result<Option<MemberDeadline>, StorageError> {
    let Some(row) = unit.consumed.as_ref().or(unit.final_active.as_ref()) else {
        return Ok(None);
    };
    let b = row.binding.snapshot.clone();
    if b.tool_name.as_str() != TOOL {
        return Ok(None);
    }
    let (input, member, _) = load_binding(
        unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
        &unit.identity,
        &unit.session,
        &b.operation_id,
    )
    .await?;
    if hash(&json!(input))? != b.arguments_hash {
        return Err(StorageError::PermitBindingMismatch);
    }
    Ok(Some(MemberDeadline::from_binding(&input, &member)?))
}
pub(crate) fn subscription_deadline(
    raw: &Value,
    member: &Value,
) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), StorageError> {
    let input: Input =
        serde_json::from_value(raw.clone()).map_err(|_| StorageError::CorruptRecord)?;
    if json!(input) != *raw {
        return Err(StorageError::CorruptRecord);
    }
    let deadline = MemberDeadline::from_binding(&input, member)?;
    Ok((deadline.prepared, deadline.expires))
}
fn preparation_identity(request: &Preparation<'_>) -> Result<Value, StorageError> {
    Ok(match request {
        Preparation::Change(SubscriptionChangeRequestMock::CreateCancellationIntent {
            subscription_id,
        }) => json!({"action":"RECORD_CANCELLATION","subscription_id":subscription_id}),
        Preparation::Change(SubscriptionChangeRequestMock::RevokeCancellationIntent {
            subscription_id,
        }) => json!({"action":"REVOKE_CANCELLATION","subscription_id":subscription_id}),
        Preparation::Change(SubscriptionChangeRequestMock::UpdateReminderSettings {
            subscription_id,
            schedule,
        }) => {
            json!({"action":"REMINDER_SETTINGS","subscription_id":subscription_id,"schedule":schedule})
        }
        Preparation::Registration(selection) => selection.request.clone(),
    })
}
