//! Closed non-funding composer. No caller-selected result, callback or card write.
use super::*;
use crate::billing_report::{load_original, restore_original, RestoredReport};
use crate::repository::{encode, update_task};
use crate::{
    new_operation_id, new_task_id, CheckpointChange, ConfirmationConfig, ConfirmationRecord,
    ConfirmationService, OperationChange, OperationRecord, PersistenceService,
};
use safeflow_bank_gateway::{BankError, BankGateway, GetAccountBalanceInput};
use safeflow_billing_source_reader::{
    feedback::{prepare_feedback, FeedbackChoice, FeedbackIntent},
    feedback_store::{
        load_feedback, stage_feedback, FeedbackProjection, SavedFeedback, StoreError,
    },
    spending::SourceKey,
    TrustedScope,
};
use safeflow_contracts::*;
use safeflow_runtime::{
    BackendFinalPolicy, BackendFinalPolicyRegistration, Runtime, ToolSecurityProfile,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const TOOL: &str = "save_billing_feedback";
const POLICY: &str = "billing-feedback-original-scope-v1";
const REVISION_TOOL: &str = "revise_billing_feedback";
mod connection_reader;
mod revisions;
pub use revisions::RevisionAction as BillingFeedbackRevisionAction;
mod domain_audit;
pub(crate) mod recovery;
pub use domain_audit::{BillingDomainAuditInstallation, BillingDomainAuditStatus};
pub use recovery::{BillingRecoveryConfig, BillingRecoveryNonce};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    input_version: u16,
    report_id: String,
    original_digest: ArgumentsHash,
    transaction: SourceKey,
    choice: FeedbackChoice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<revisions::Revision>,
}
impl Input {
    fn valid(&self) -> bool { matches!((self.input_version, &self.revision), (1, None) | (2, Some(_))) }
    fn tool(&self) -> &str { if self.revision.is_some() { REVISION_TOOL } else { TOOL } }
    fn intent(&self) -> FeedbackIntent {
        FeedbackIntent {
            report_id: self.report_id.clone(),
            transaction: self.transaction.clone(),
            choice: self.choice.clone(),
        }
    }
}
#[derive(Serialize)]
pub struct BillingFeedbackPreparation {
    pub task: TaskRecord,
    pub operation: OperationRecord,
    pub confirmation: ConfirmationRecord,
    pub parameters: Value,
}
#[derive(Serialize)]
pub struct BillingFeedbackView {
    pub operation: OperationRecord,
    pub task_state: TaskState,
    /// Opaque member serialization, NOT an adopted card projection/feedback ID.
    pub saved: Value,
    pub replayed: bool,
    pub domain_audit: BillingDomainAuditStatus,
}
// True member object, never constructed from the display Value. Private to the
// public backend. The stamp only detects replacement of immutable local rows;
// it is NOT a feedback live revision, correction/revocation API or permit.
pub(crate) struct CheckedBillingFeedback {
    pub(crate) saved: SavedFeedback,
    pub(crate) scope: TrustedScope,
    pub(crate) operation: OperationRecord,
    pub(crate) task_state: TaskState,
    pub(crate) fingerprint: ArgumentsHash,
    pub(crate) stamp: String,
    domain_audit: BillingDomainAuditStatus,
}
impl CheckedBillingFeedback {
    pub(crate) fn projection(&self) -> Result<FeedbackProjection<'_>, StorageError> {
        self.saved
            .checked_projection(&self.scope)
            .map_err(store_error)
    }
    pub(crate) fn same_original(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint && self.stamp == other.stamp
    }
}
/// Only the closed composer can construct this exact terminal snapshot.
#[derive(Clone)]
pub(super) struct BillingCompletion {
    operation: OperationRecord,
    task: TaskRecord,
    accounts: Vec<String>,
    bank: Arc<dyn BankGateway>,
}
struct CheckedFacts(SecurityContext);
struct Policy;
impl BackendFinalPolicy for Policy {
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
        facts: &'a (dyn std::any::Any + Send + Sync),
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let valid = facts
                .downcast_ref::<CheckedFacts>()
                .is_some_and(|facts| &facts.0 == context)
                && serde_json::from_value::<Input>(context.normalized_arguments.clone()).is_ok_and(|i| i.valid() && context.tool_name.as_str() == i.tool());
            if valid {
                Ok(vec![])
            } else {
                Err(SecurityError {
                    reason_code: ReasonCode::parse("BILLING_FACTS_MISMATCH").unwrap(),
                    message: "trusted billing facts required".into(),
                })
            }
        })
    }
}
pub struct BillingFeedbackService<'a> {
    store: PermitStore,
    runtime: &'a Runtime,
    bank: Arc<dyn BankGateway>,
    confirmations: ConfirmationService,
    policy: BackendFinalPolicyRegistration,
    revision_policy: BackendFinalPolicyRegistration,
}
impl<'a> BillingFeedbackService<'a> {
    /// Explicit trusted clock injection, shared by original confirmation and
    /// permit guards. This accepts no HTTP/session/client clock value.
    pub fn for_trusted_backend_with_clock(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        bank: Arc<dyn BankGateway>,
        runtime: &'a Runtime,
        confirmation_config: ConfirmationConfig,
        permit_config: PermitConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Result<Self, StorageError> {
        let mut service = Self::for_trusted_backend(
            repository.clone(),
            sessions.clone(),
            bank,
            runtime,
            confirmation_config.clone(),
            permit_config.clone(),
        )?;
        service.confirmations = ConfirmationService::for_trusted_backend_with_clock(
            repository.clone(),
            sessions.clone(),
            confirmation_config,
            clock.clone(),
        );
        service.store =
            PermitStore::for_trusted_backend_with_clock(repository, sessions, permit_config, clock);
        Ok(service)
    }
    /// Trusted explicit backend assembly only. Installs no HTTP/LLM/lock-card tool.
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        bank: Arc<dyn BankGateway>,
        runtime: &'a Runtime,
        confirmation_config: ConfirmationConfig,
        permit_config: PermitConfig,
    ) -> Result<Self, StorageError> {
        if !runtime.final_payment_uses_session_resolver_for_backend(&sessions) {
            return Err(StorageError::Session);
        }
        let profile = ToolSecurityProfile {
            tool_name: ToolName::parse(TOOL).unwrap(),
            tool_schema_version: 1,
            policy_version: PolicyVersion::parse(POLICY).unwrap(),
            minimum_level: SecurityLevel::Yellow,
        };
        let policy = BackendFinalPolicyRegistration::for_reviewed_backend(
            profile,
            "billing-feedback-closed-v1",
            Arc::new(Policy),
        )
        .map_err(|_| StorageError::DecisionAssemblyChanged)?;
        let revision_policy = BackendFinalPolicyRegistration::for_reviewed_backend(
            ToolSecurityProfile { tool_name:ToolName::parse(REVISION_TOOL).unwrap(), tool_schema_version:1,
                policy_version:PolicyVersion::parse("billing-feedback-revision-scope-v1").unwrap(), minimum_level:SecurityLevel::Yellow },
            "billing-feedback-revision-closed-v1", Arc::new(Policy),
        ).map_err(|_|StorageError::DecisionAssemblyChanged)?;
        Ok(Self {
            confirmations: ConfirmationService::for_trusted_backend(
                repository.clone(),
                sessions.clone(),
                confirmation_config,
            ),
            store: PermitStore::for_trusted_backend(repository, sessions, permit_config),
            runtime,
            bank,
            policy,
            revision_policy,
        })
    }
    fn guard(&self, session: &SessionRef, binding: &SessionBinding) -> Result<(), StorageError> {
        if self.store.identity(session)? != *binding {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    async fn scope(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        accounts: &[String],
    ) -> Result<TrustedScope, StorageError> {
        self.guard(session, identity)?;
        for account in accounts {
            let account = ResourceId::parse(account).map_err(|_| StorageError::CorruptRecord)?;
            let output = self
                .bank
                .get_account_balance(&identity.user_id, &account)
                .await;
            self.guard(session, identity)?;
            let output = output.map_err(|error| match error {
                BankError::ResourceNotOwned => StorageError::NotFound,
                BankError::Unavailable => StorageError::DependencyUnavailable,
                BankError::InvalidResponse => StorageError::CorruptRecord,
                BankError::ValidationError => StorageError::Invalid("billing account"),
            })?;
            output
                .validate_for(&account)
                .map_err(|_| StorageError::CorruptRecord)?;
            GetAccountBalanceInput {
                account_id: account,
            }
            .validate()
            .map_err(|_| StorageError::CorruptRecord)?;
        }
        TrustedScope::from_verified_context(
            identity.user_id.as_str(),
            accounts.iter().map(String::as_str),
        )
        .map_err(|_| StorageError::NotFound)
    }
    async fn report(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        id: &str,
    ) -> Result<(RestoredReport, TrustedScope), StorageError> {
        valid_id(id)?;
        let result = self
            .store
            .repository
            .original_billing_report(&identity.user_id, id)
            .await;
        self.guard(session, identity)?;
        let (value, accounts) = result?;
        let scope = self.scope(session, identity, &accounts).await?;
        Ok((restore_original(value, &scope)?, scope))
    }
    /// Rebuild the SPECIFIC original snapshot from archived source calls. No
    /// current reader, display report or JSON input can be supplied here.
    pub async fn original_report(
        &self,
        session: &SessionRef,
        id: &str,
    ) -> Result<Value, StorageError> {
        let identity = self.store.identity(session)?;
        let (restored, _) = self.report(session, &identity, id).await?;
        self.guard(session, &identity)?;
        serde_json::to_value(restored.report).map_err(|_| StorageError::CorruptRecord)
    }

    /// Route this dedicated backend task's ordinary read projection without
    /// attempting to recover an unregistered model Tool. This grants no action.
    pub async fn owns_task(
        &self,
        session: &SessionRef,
        task: &TaskId,
        revision: u64,
    ) -> Result<bool, StorageError> {
        let identity = self.store.identity(session)?;
        let snapshot = PersistenceService::for_trusted_backend(
            self.store.repository.clone(),
            self.store.sessions.clone(),
        )
        .recover(session, task)
        .await?;
        let nodes = &snapshot.task.plan.nodes;
        if !nodes
            .iter()
            .any(|n| matches!(n,PlanNode::Tool{tool,..} if matches!(tool.as_str(),TOOL|REVISION_TOOL)))
        {
            return Ok(false);
        }
        if snapshot.task.revision != revision {
            return Err(StorageError::RevisionConflict);
        }
        let [PlanNode::UserDecision {
            id: review,
            depends_on,
        }, PlanNode::Tool {
            id: save,
            tool,
            arguments,
            depends_on: save_depends,
        }] = nodes.as_slice()
        else {
            return Err(StorageError::CorruptRecord);
        };
        if review.as_str() != "review_feedback"
            || save.as_str() != "save_feedback"
            || !matches!(tool.as_str(),TOOL|REVISION_TOOL)
            || !depends_on.is_empty()
            || save_depends != std::slice::from_ref(review)
            || snapshot.operations.len() != 1
        {
            return Err(StorageError::CorruptRecord);
        }
        let input: Input =
            serde_json::from_value(arguments.clone()).map_err(|_| StorageError::CorruptRecord)?;
        if !input.valid() || tool.as_str()!=input.tool() || *arguments != json!(input) {
            return Err(StorageError::CorruptRecord);
        }
        let (restored, _) = self.report(session, &identity, &input.report_id).await?;
        if restored.archive_digest != input.original_digest {
            return Err(StorageError::CorruptRecord);
        }
        let op = &snapshot.operations[0];
        match snapshot.task.state {
            TaskState::Completed => {
                let saved = self
                    .query(session, &op.operation_id)
                    .await?
                    .ok_or(StorageError::CorruptRecord)?;
                if saved.operation != *op {
                    return Err(StorageError::CorruptRecord);
                }
            }
            TaskState::Running
                if matches!(op.state, OperationState::Draft | OperationState::Ready) => {}
            TaskState::Cancelled
                if op.state == OperationState::Cancelled
                    && snapshot.task.checkpoint == RecoveryPoint::Finished =>
            {
                let row:Option<(String,String)>=sqlx::query_as("SELECT realm::text,nonce::text FROM billing_prepare_requests_v2 WHERE task_id=$1 AND owner_id=$2 AND state='ABANDONED' AND fence_revision=revision")
                    .bind(task.as_str()).bind(identity.user_id.as_str()).fetch_optional(&self.store.repository.pool).await?;
                let Some((_, nonce)) = row else {
                    return Err(StorageError::CorruptRecord);
                };
                let mut tx = self.store.repository.pool.begin().await?;
                self.check_cancelled_recovery(&mut tx, &identity, &nonce, task)
                    .await?;
                tx.commit().await?;
            }
            _ => return Err(StorageError::CorruptRecord),
        }
        self.guard(session, &identity)?;
        Ok(true)
    }
    pub async fn prepare(
        &self,
        session: &SessionRef,
        intent: FeedbackIntent,
    ) -> Result<BillingFeedbackPreparation, StorageError> {
        let identity = self.store.identity(session)?;
        let (restored, scope) = self.report(session, &identity, &intent.report_id).await?;
        let operation_id = new_operation_id();
        prepare_feedback(&scope, &restored.report, operation_id.as_str(), &intent)
            .map_err(|_| StorageError::Invalid("feedback intent rejected"))?;
        let input = Input {
            input_version: 1,
            report_id: intent.report_id,
            original_digest: restored.archive_digest,
            transaction: intent.transaction,
            choice: intent.choice,
            revision: None,
        };
        self.prepare_input(session, input, operation_id, new_task_id()).await
    }
    async fn prepare_input(&self, session:&SessionRef, input:Input, operation_id:OperationId, task_id:TaskId) -> Result<BillingFeedbackPreparation,StorageError> {
        let identity=self.store.identity(session)?;
        let parameters = json!(input);
        let review = NodeId::parse("review_feedback").unwrap();
        let save = NodeId::parse("save_feedback").unwrap();
        let plan = AgentPlan::new(
            task_id,
            1,
            vec![
                PlanNode::UserDecision {
                    id: review.clone(),
                    depends_on: vec![],
                },
                PlanNode::Tool {
                    id: save.clone(),
                    tool: ToolName::parse(input.tool()).unwrap(),
                    arguments: parameters.clone(),
                    depends_on: vec![review.clone()],
                },
            ],
        )
        .map_err(|_| StorageError::PlanConflict)?;
        let persistence = PersistenceService::for_trusted_backend(
            self.store.repository.clone(),
            self.store.sessions.clone(),
        );
        let task = persistence.create_task(session, plan, 1).await?;
        let existing = persistence.recover(session,&task.task_id).await?;
        if let Some(op)=existing.operations.iter().find(|op|op.operation_id==operation_id) {
            if !matches!(op.state,OperationState::Draft|OperationState::Ready) {return Err(StorageError::ExecutionOperationConflict);}
            if op.state==OperationState::Ready {
                let confirmation=self.confirmations.prepare(session,&task.task_id,&operation_id).await?;
                self.guard(session,&identity)?;
                return Ok(BillingFeedbackPreparation{task:existing.task,operation:op.clone(),confirmation,parameters});
            }
        }
        let operation = persistence
            .create_operation(
                session,
                &task.task_id,
                task.revision,
                operation_id,
                save.clone(),
                1,
            )
            .await?;
        let task = persistence.recover(session, &task.task_id).await?.task;
        persistence
            .save_checkpoint(
                session,
                &task.task_id,
                CheckpointChange {
                    expected_revision: task.revision,
                    event: None,
                    checkpoint: RecoveryPoint::AtNode { node_id: save },
                    completed_nodes: vec![review],
                    facts_version: 1,
                },
            )
            .await?;
        persistence
            .record_operation_event(
                session,
                &task.task_id,
                &operation.operation_id,
                OperationChange {
                    expected_revision: operation.revision,
                    event: OperationEvent::GatesSatisfied,
                    facts_version: 1,
                    business_operation_ref: None,
                },
            )
            .await?;
        let confirmation = self
            .confirmations
            .prepare(session, &task.task_id, &operation.operation_id)
            .await?;
        let snapshot = persistence.recover(session, &task.task_id).await?;
        self.guard(session, &identity)?;
        Ok(BillingFeedbackPreparation {
            task: snapshot.task,
            operation: snapshot
                .operations
                .into_iter()
                .find(|op| op.operation_id == operation.operation_id)
                .ok_or(StorageError::CorruptRecord)?,
            confirmation,
            parameters,
        })
    }
    /// Explicit backend user-action handler must show the original parameters.
    pub async fn confirm(
        &self,
        session: &SessionRef,
        prepared: &BillingFeedbackPreparation,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.confirmations
            .record_explicit_confirmation(
                session,
                &prepared.task.task_id,
                &prepared.confirmation.confirmation_ref,
                &prepared.confirmation.binding,
            )
            .await
    }

    /// Explicit HTTP user action over the stored original confirmation. The
    /// caller supplies only server-owned references, never a binding or permit.
    pub async fn confirm_original(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
        reference: &ConfirmationRef,
    ) -> Result<ConfirmationRecord, StorageError> {
        let original = self.confirmations.read(session, task, reference).await?;
        if original.binding.operation_id != *operation
            || !matches!(original.binding.tool_name.as_str(),TOOL|REVISION_TOOL)
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        self.confirmations
            .record_explicit_confirmation(session, task, reference, &original.binding)
            .await
    }

    /// Query only. None is absence/inaccessibility, Err is unavailable/corrupt.
    /// Uses an independent pool, current owner and ALL original accounts, then
    /// rechecks the session. Never consumes a permit or retries a write.
    pub async fn query(
        &self,
        session: &SessionRef,
        operation: &OperationId,
    ) -> Result<Option<BillingFeedbackView>, StorageError> {
        if let Some(view)=self.query_revision(session,operation).await? {return Ok(Some(view));}
        Ok(self
            .checked_query(session, operation)
            .await?
            .map(|checked| BillingFeedbackView {
                operation: checked.operation,
                task_state: checked.task_state,
                saved: json!(checked.saved),
                replayed: true,
                domain_audit: checked.domain_audit,
            }))
    }
    pub(crate) fn matches_navigation(
        &self,
        repo: &PgRepository,
        sessions: &Arc<dyn SessionResolver>,
    ) -> bool {
        self.store.repository.namespace == repo.namespace
            && Arc::ptr_eq(
                &self.store.repository.pool.connect_options(),
                &repo.pool.connect_options(),
            )
            && Arc::ptr_eq(&self.store.sessions, sessions)
    }
    /// The caller obtains this complete ordered account list from a genuine
    /// CheckedBillingFeedback projection. This is a fresh read of the existing
    /// account boundary, not a cached scope, permission revision or lease.
    pub(crate) async fn recheck_report_read_access(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        accounts: &[String],
    ) -> Result<(), StorageError> {
        self.scope(session, identity, accounts).await?;
        self.guard(session, identity)
    }
    pub(crate) async fn checked_query(
        &self,
        session: &SessionRef,
        operation: &OperationId,
    ) -> Result<Option<CheckedBillingFeedback>, StorageError> {
        let identity = self.store.identity(session)?;
        crate::repository::guarded_poll(
            &|| self.guard(session, &identity),
            self.checked_query_inner(session, operation),
        )
        .await
    }
    async fn evidence_stamp(
        &self,
        operation: &OperationId,
        owner: &UserId,
    ) -> Result<String, StorageError> {
        sqlx::query_scalar("SELECT f.xmin::text || ':' || h.xmin::text || ':' || e.xmin::text FROM billing_feedback_v1 f JOIN billing_report_bindings_v1 h ON h.owner_user_id=f.owner_user_id AND h.report_id=f.report_id JOIN billing_feedback_events_v1 e ON e.public_operation_id=f.public_operation_id WHERE f.public_operation_id=$1 AND f.owner_user_id=$2")
            .bind(operation.as_str()).bind(owner.as_str()).fetch_optional(&self.store.repository.pool).await?
            .ok_or(StorageError::CorruptRecord)
    }
    async fn checked_query_inner(
        &self,
        session: &SessionRef,
        operation: &OperationId,
    ) -> Result<Option<CheckedBillingFeedback>, StorageError> {
        let identity = self.store.identity(session)?;
        let row = sqlx::query("SELECT report_id FROM public_billing_feedback_results_v1 WHERE owner_id=$1 AND operation_id=$2")
            .bind(identity.user_id.as_str()).bind(operation.as_str()).fetch_optional(&self.store.repository.pool).await;
        self.guard(session, &identity)?;
        let Some(row) = row? else { return Ok(None) };
        let (restored, scope) = self
            .report(session, &identity, &row.try_get::<String, _>("report_id")?)
            .await?;
        let stamp = self.evidence_stamp(operation, &identity.user_id).await?;
        let saved = load_feedback(&self.store.repository.pool, &scope, operation.as_str()).await;
        self.guard(session, &identity)?;
        let saved = saved
            .map_err(store_error)?
            .ok_or(StorageError::CorruptRecord)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(
            "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND operation_id=$2",
        )
        .bind(identity.user_id.as_str())
        .bind(operation.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let request = crate::execution_request::stored_row(&row)?.request;
        let input = input(&request)?;
        if input.original_digest != restored.archive_digest {
            return Err(StorageError::CorruptRecord);
        }
        // Committed-read consistency only: this pool-loaded observation is not
        // a same-connection final authorization or a current card association.
        let projection = saved.checked_projection(&scope).map_err(store_error)?;
        prepare_feedback(
            &scope,
            &restored.report,
            operation.as_str(),
            &input.intent(),
        )
        .map_err(|_| StorageError::CorruptRecord)?;
        let original = restored
            .report
            .evidence
            .as_ref()
            .and_then(|rows| rows.iter().find(|row| row.reference == input.transaction))
            .ok_or(StorageError::CorruptRecord)?;
        if projection.operation_id() != operation.as_str()
            || request.binding.snapshot.operation_id != *operation
            || projection.owner_user_id() != identity.user_id.as_str()
            || request.binding.snapshot.user_id != identity.user_id
            || projection.report_id() != input.report_id
            || projection.report_id() != restored.report.metadata.report_id
            || projection.report_account_ids() != restored.request.account_ids
            || projection.choice() != &input.choice
            || projection.reference() != &input.transaction
            || projection.reference() != &original.reference
            || projection.reference().source_id != restored.report.read.source_id
            || projection.card_id() != original.transaction.card_id.as_deref()
            || Some(projection.snapshot_id()) != restored.report.read.snapshot_id.as_deref()
            || projection.rule_version() != restored.report.rule_version
            || projection.calculated_at() != restored.report.metadata.calculated_at
        {
            return Err(StorageError::CorruptRecord);
        }
        // Adopt the member's opaque borrowed-connection reader into this closed
        // read path. It does not grant final-card/write/committed capability.
        connection_reader::verify_original_on_connection(
            &mut tx,
            &self.store.repository.namespace,
            &scope,
            &request,
            &restored,
        )
        .await?;
        verify_result(&mut tx, &identity.user_id, &request).await?;
        let row = sqlx::query("SELECT * FROM operations WHERE operation_id=$1")
            .bind(operation.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let op = crate::repository::operation_row(&row)?;
        if op.state != OperationState::Succeeded {
            return Err(StorageError::CorruptRecord);
        }
        let row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2")
            .bind(op.task_id.as_str())
            .bind(identity.user_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let task = crate::repository::task_row(&row)?;
        validate_checkpoint(&task, std::slice::from_ref(&op))?;
        if task.state != TaskState::Completed {
            return Err(StorageError::CorruptRecord);
        }
        let domain_audit = domain_audit::verify_link(
            &mut tx,
            &request,
            &BillingCompletion {
                operation: op.clone(),
                task: task.clone(),
                accounts: restored.request.account_ids.clone(),
                bank: self.bank.clone(),
            },
        )
        .await?;
        tx.commit().await?;
        let current_scope = self
            .scope(session, &identity, &restored.request.account_ids)
            .await?;
        saved
            .checked_projection(&current_scope)
            .map_err(store_error)?;
        self.guard(session, &identity)?;
        if self.evidence_stamp(operation, &identity.user_id).await? != stamp {
            return Err(StorageError::FactsConflict);
        }
        let fingerprint = hash(
            &json!({"original": &saved, "input": &input, "request_digest": request.digest()?,
            "archive_digest": restored.archive_digest}),
        )?;
        self.guard(session, &identity)?;
        Ok(Some(CheckedBillingFeedback {
            operation: op,
            task_state: task.state,
            saved,
            scope: current_scope,
            fingerprint,
            stamp,
            domain_audit,
        }))
    }

    pub async fn execute(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
    ) -> Result<BillingFeedbackView, StorageError> {
        // Even replayed v2 results must use the dedicated read endpoint; an old
        // execute entry is never an admission or recovery authority.
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        crate::repository::lock_task(&mut tx, &identity.user_id, task).await?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        self.execute_checked(session, task, operation, confirmation, None)
            .await
    }
    async fn execute_checked(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        recovery: Option<recovery::Admission>,
    ) -> Result<BillingFeedbackView, StorageError> {
        let identity = self.store.identity(session)?;
        if let Some(view) = self.query(session, operation).await? {
            if view.saved["history_contract"]=="BILLING_FEEDBACK_HISTORY_V1" {
                self.check_revision_replay(session,task,operation,confirmation).await?;
                return Ok(view);
            }
            let row = sqlx::query("SELECT confirmation_ref,session_digest,binding_version FROM public_billing_feedback_results_v1 WHERE owner_id=$1 AND operation_id=$2")
                .bind(identity.user_id.as_str()).bind(operation.as_str()).fetch_one(&self.store.repository.pool).await?;
            if view.operation.task_id != *task
                || row.try_get::<String, _>("confirmation_ref")? != confirmation.as_str()
                || row.try_get::<String, _>("session_digest")?
                    != hash(&("safeflow-confirmation-session:v1", session.as_str()))?.as_str()
                || row.try_get::<i64, _>("binding_version")?
                    != db_version(identity.binding_version)?
            {
                return Err(StorageError::NotFound);
            }
            self.guard(session, &identity)?;
            return Ok(view);
        }
        let mut unit = if let Some(admission) = recovery {
            self.recovery_open_unit(session, task, admission).await?
        } else {
            self.store.open(session, task).await?
        };
        // Recheck after acquiring the owner/task lock: another first call may have committed.
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_billing_feedback_results_v1 WHERE owner_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM public_execution_requests WHERE owner_id=$1 AND operation_id=$2)")
            .bind(identity.user_id.as_str()).bind(operation.as_str()).fetch_one(&mut **unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?).await?;
        if exists {
            unit.rollback().await;
            return Box::pin(self.execute(session, task, operation, confirmation)).await;
        }
        if let Err(error) = self
            .stage_billing_unit(&mut unit, session, &identity, task, operation, confirmation)
            .await
        {
            unit.rollback().await;
            return Err(error);
        }
        unit.finish(true).await?;
        self.guard(session, &identity)?;
        let mut saved = self
            .query(session, operation)
            .await?
            .ok_or(StorageError::ReconcileRequired)?;
        saved.replayed = false;
        Ok(saved)
    }
    async fn stage_billing_unit(
        &self,
        unit: &mut PermitTransaction,
        session: &SessionRef,
        identity: &SessionBinding,
        task: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
    ) -> Result<(), StorageError> {
        domain_audit::writer_lock(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?)
            .await?;
        self.guard(session, identity)?;
        let ops = operations(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            task,
        )
        .await?;
        let binding = current_binding(&unit.task, &ops, operation)?;
        let node = unit
            .task
            .plan
            .nodes
            .iter()
            .find(|node| node.id() == &binding.node_id)
            .ok_or(StorageError::CorruptRecord)?;
        let PlanNode::Tool {
            tool, arguments, ..
        } = node
        else {
            return Err(StorageError::CorruptRecord);
        };
        if !matches!(tool.as_str(),TOOL|REVISION_TOOL) {
            return Err(StorageError::PermitBindingMismatch);
        }
        let value = arguments.clone();
        let input: Input = serde_json::from_value(value.clone())
            .map_err(|_| StorageError::PermitBindingMismatch)?;
        if !input.valid() || tool.as_str()!=input.tool() || value != json!(input) {
            return Err(StorageError::PermitBindingMismatch);
        }
        let (archive, accounts) = load_original(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            &identity.user_id,
            &input.report_id,
        )
        .await?;
        let scope = self.scope(session, &identity, &accounts).await?;
        let restored = restore_original(archive, &scope)?;
        if restored.archive_digest != input.original_digest {
            return Err(StorageError::PermitBindingMismatch);
        }
        let prepared = prepare_feedback(
            &scope,
            &restored.report,
            operation.as_str(),
            &input.intent(),
        )
        .map_err(|_| StorageError::PermitBindingMismatch)?;
        let payload = FingerprintPayloadV1 {
            user_id: identity.user_id.clone(),
            operation_id: operation.clone(),
            tool_name: binding.tool_name.clone(),
            resource_id: ResourceId::parse(&input.report_id)
                .map_err(|_| StorageError::Invalid("report id"))?,
            normalized_arguments: value.clone(),
            plan_version: binding.plan_version,
            schema_version: 1,
        };
        let request = ExecutionRequest {
            idempotency_key: IdempotencyKey::parse(operation.as_str())
                .map_err(|_| StorageError::Invalid("operation id"))?,
            binding: PermitBinding {
                snapshot: binding.clone(),
                fingerprint: compute_operation_fingerprint(&payload)
                    .map_err(|_| StorageError::PermitBindingMismatch)?,
            },
            payload: payload.clone(),
            business_operation_ref: BusinessOperationRef::parse(operation.as_str())
                .map_err(|_| StorageError::Invalid("operation id"))?,
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
            normalized_arguments: value,
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
                if input.revision.is_some() { &self.revision_policy } else { &self.policy },
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
        audit(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?, &unit.task, None, PersistentAuditKind::SecurityDecisionRecorded, json!({"scope":"BILLING_FEEDBACK_LOCAL_V1","decision":decision,"original_digest":input.original_digest})).await?;
        let permit = unit.save(&verified).await?;
        unit.consume(&permit.id, &request.binding).await?;
        // This closed call is the ONLY permitted owner of the member's fixed
        // private SAVEPOINT. Any returned error propagates; Drop/rollback owns
        // cancellation, so no cancelled stage future can be followed by COMMIT.
        let staged = if input.revision.is_some() {
            revisions::stage(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?, &identity.user_id, &input, &request, confirmation, &verified.session_digest, identity.binding_version).await?
        } else { json!(stage_feedback(unit.connection()?, &scope, &prepared).await.map_err(store_error)?) };
        self.scope(session, &identity, &accounts).await?;
        unit.record_submission(&request).await?;
        let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        if input.revision.is_none() { sqlx::query("INSERT INTO public_billing_feedback_results_v1(operation_id,owner_id,report_id,request_digest,staged,confirmation_ref,session_digest,binding_version) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(operation.as_str()).bind(identity.user_id.as_str()).bind(&input.report_id).bind(request_digest(&request)?.as_str()).bind(Json(json!(staged))).bind(confirmation.as_str()).bind(&verified.session_digest).bind(db_version(identity.binding_version)?).execute(&mut **tx).await?; }
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
        let details=if input.revision.is_some() {json!({"event":"EXECUTION_SUCCEEDED","source":"billing-feedback-revision-v1","original_digest":input.original_digest,"public_revision":staged,"lock_authorized":false,"member_domain_mapping_adopted":false})}
            else {json!({"event":"EXECUTION_SUCCEEDED","source":"billing-feedback-local-v1","original_digest":input.original_digest,"member_stage":staged,"lock_authorized":false,"domain_audit_mapping_adopted":false})};
        audit(tx, &task_record, Some(&op), PersistentAuditKind::OperationTransitioned, details).await?;
        verify_result(tx, &identity.user_id, &request).await?;
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
        audit(tx,&resolved,None,PersistentAuditKind::CheckpointSaved,json!({"event":"EXTERNAL_RESOLVED","from":task_record.state,"to":resolved.state,"source":"billing-feedback-local-v1","operation_id":op.operation_id,"checkpoint":resolved.checkpoint,"completed_nodes":resolved.completed_nodes})).await?;
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
        audit(tx,&completed,None,PersistentAuditKind::CheckpointSaved,json!({"event":"COMPLETE","from":resolved.state,"to":completed.state,"source":"billing-feedback-local-v1","operation_id":op.operation_id,"checkpoint":completed.checkpoint,"completed_nodes":completed.completed_nodes})).await?;
        let completion = BillingCompletion {
            operation: op,
            task: completed,
            accounts: accounts.clone(),
            bank: self.bank.clone(),
        };
        if input.revision.is_none() { domain_audit::append(tx, &request, &completion).await?; }
        unit.billing_result = Some(completion);
        self.scope(session, &identity, &accounts).await?;
        Ok(())
    }
}

/// After all mapper/deferred SQL awaits, recheck every original account, then
/// original confirmation, tx/schema, session and deadlines. No new authority.
pub(super) async fn final_scope(unit: &mut PermitTransaction) -> Result<(), StorageError> {
    let proof = unit
        .billing_result
        .clone()
        .ok_or(StorageError::ReconcileRequired)?;
    for account in &proof.accounts {
        let account = ResourceId::parse(account).map_err(|_| StorageError::CorruptRecord)?;
        let result = proof
            .bank
            .get_account_balance(&unit.identity.user_id, &account)
            .await;
        unit.session_guard()?;
        result
            .map_err(|_| StorageError::DependencyUnavailable)?
            .validate_for(&account)
            .map_err(|_| StorageError::CorruptRecord)?;
    }
    let consumed = unit
        .consumed
        .clone()
        .ok_or(StorageError::ReconcileRequired)?;
    let digest = unit.session_digest()?;
    unit.scope_guard().await?;
    verify_stored_confirmation(
        unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
        &consumed.confirmation_ref,
        &consumed.binding.snapshot,
        &digest,
        unit.identity.binding_version,
        unit.store.clock.as_ref(),
    )
    .await?;
    unit.session_guard()?;
    consumed.active_time(unit.now()?)
}

/// A separate closed-domain proof, NOT a new allowed generic callback shape.
/// Reconstruct the exact terminal task from the original locked task and verify
/// member/public result plus first submission before the shared final guards.
pub(super) async fn verify_completion(
    unit: &mut PermitTransaction,
    consumed: &PermitRow,
) -> Result<(), StorageError> {
    let proof = unit
        .billing_result
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
        || !matches!(b.tool_name.as_str(),TOOL|REVISION_TOOL)
        || consumed.execution_scope != Some(VerifiedPermitScope::Common)
        || request.binding != consumed.binding
        || stored.first_submission.as_ref() != Some(receipt)
        || receipt.operation.state != OperationState::Submitted
        || Some(receipt.operation.revision) != b.operation_revision.checked_add(1)
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    verify_result(tx, &unit.identity.user_id, request).await?;
    if input(request)?.revision.is_none() { domain_audit::verify_link(tx, request, &proof).await?; }
    unit.session_guard()
}
fn store_error(error: StoreError) -> StorageError {
    match error {
        StoreError::OperationConflict
        | StoreError::RepeatedFeedbackRuleUnconfirmed
        | StoreError::ReportConflict => StorageError::ExecutionOperationConflict,
        StoreError::ResourceUnavailable => StorageError::NotFound,
        StoreError::OutcomeUnknown => StorageError::ReconcileRequired,
        StoreError::CorruptRecord => StorageError::CorruptRecord,
        StoreError::InvalidInput => StorageError::Invalid("member feedback input"),
        StoreError::TransactionRequired => StorageError::TransactionAborted,
        StoreError::Database => StorageError::Database,
    }
}
fn request_digest(request: &ExecutionRequest) -> Result<ArgumentsHash, StorageError> {
    hash(
        &json!({"binding":request.binding.snapshot,"payload":request.payload,"key":request.idempotency_key,"business":request.business_operation_ref,"version":request.business_version}),
    )
}
fn input(request: &ExecutionRequest) -> Result<Input, StorageError> {
    request.validate()?;
    if !matches!(request.payload.tool_name.as_str(),TOOL|REVISION_TOOL)
        || request.binding.snapshot.operation_id.as_str() != request.business_operation_ref.as_str()
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    serde_json::from_value(request.payload.normalized_arguments.clone())
        .map_err(|_| StorageError::PermitBindingMismatch)
}
pub(super) async fn verify_result(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    request: &ExecutionRequest,
) -> Result<(), StorageError> {
    let expected = input(request)?;
    if expected.revision.is_some() { return revisions::verify_result(tx,owner,request).await; }
    let row = sqlx::query("SELECT r.report_id,r.request_digest, (SELECT count(*) FROM billing_feedback_v1 f WHERE f.public_operation_id=r.operation_id AND f.owner_user_id=r.owner_id AND f.report_id=r.report_id) AS feedback_count, (SELECT count(*) FROM billing_feedback_events_v1 e WHERE e.public_operation_id=r.operation_id) AS event_count FROM public_billing_feedback_results_v1 r WHERE r.operation_id=$1 AND r.owner_id=$2")
        .bind(request.binding.snapshot.operation_id.as_str()).bind(owner.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    if row.try_get::<String, _>("report_id")? != expected.report_id
        || row.try_get::<String, _>("request_digest")? != request_digest(request)?.as_str()
        || row.try_get::<i64, _>("feedback_count")? != 1
        || row.try_get::<i64, _>("event_count")? != 1
    {
        return Err(StorageError::CorruptRecord);
    }
    domain_audit::verify_facts(tx, owner, request).await
}
