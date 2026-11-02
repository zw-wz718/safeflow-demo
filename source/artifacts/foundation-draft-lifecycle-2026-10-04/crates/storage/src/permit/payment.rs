//! Closed composition over the existing permit/SQL/result/outbox transaction.
//! No wire DTO, caller-selected success callback, test provider or model entry.
use super::*;
use crate::card_catalog::{read_adopted_card_facts, CardCatalogSourceBinding};
use crate::payment_preparation::{current_snapshot, saved_payment, SavedPayment};
use crate::repository::{lock_task_for_mode, operation_row};
use crate::{ExecutionRequestView, OperationRecord, PaymentSnapshot};
use safeflow_contracts::*;
use safeflow_runtime::{
    BackendFinalPolicy, BackendFinalPolicyRegistration, Runtime, ToolSecurityProfile,
};
use safeflow_transfer_integration::{
    business_store::BusinessRecord,
    funds_policy::{DailyFacts, FundsBusinessPolicy, FundsPolicyFacts, FundsPolicySource},
    funds_store::{FundsError, FundsStore, LockedOutgoingSource},
    mock_execution::{MockBehavior, MockOutgoingState, MockResolution, MockTransferRequest},
};
use serde::Serialize;
use serde_json::Value;
pub(crate) mod evidence;
pub(super) mod background;
pub use background::{PaymentBackgroundDisplay,PaymentBackgroundCode,PaymentBackgroundHost};
pub use evidence::{PaymentAcceptedHistoryReader, PaymentAcceptedHistoryReport};

pub const PAYMENT_EXECUTION_TOOL: &str = "execute_confirmed_payment";
pub const PAYMENT_EXECUTION_POLICY: &str = "transfer-funds-business-2026-10-18-v1";
pub const PAYMENT_FINAL_IMPLEMENTATION: &str = "locked-funds-policy-2026-10-20-v1";

struct RegisteredFundsPolicy;
// Only the closed SQL composer can supply this concrete fact type. Public
// FundsPolicyFacts values or JSON cannot stand in for its protected read.
struct LockedFundsFacts(FundsPolicyFacts);
impl BackendFinalPolicy for RegisteredFundsPolicy {
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
        facts: &'a (dyn std::any::Any + Send + Sync),
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let facts = facts
                .downcast_ref::<LockedFundsFacts>()
                .ok_or_else(|| SecurityError {
                    reason_code: ReasonCode::parse("FINAL_FACTS_TYPE_MISMATCH").unwrap(),
                    message: "final funds facts unavailable".into(),
                })?;
            FundsBusinessPolicy::new(
                ToolName::parse(PAYMENT_EXECUTION_TOOL).unwrap(),
                SnapshotSource(facts.0.clone()),
            )
            .evaluate(context)
            .await
        })
    }
}
/// Adopt the reviewed real funds Policy implementation without inventing a Tool.
pub fn payment_final_registration_for_backend(
    profile: ToolSecurityProfile,
) -> Result<BackendFinalPolicyRegistration, StorageError> {
    if profile.tool_name.as_str() != PAYMENT_EXECUTION_TOOL
        || profile.tool_schema_version != 1
        || profile.policy_version.as_str() != PAYMENT_EXECUTION_POLICY
    {
        return Err(StorageError::DecisionAssemblyChanged);
    }
    BackendFinalPolicyRegistration::for_reviewed_backend(
        profile,
        PAYMENT_FINAL_IMPLEMENTATION,
        Arc::new(RegisteredFundsPolicy),
    )
    .map_err(|_| StorageError::DecisionAssemblyChanged)
}

#[derive(Serialize)]
pub struct PaymentExecutionResult {
    pub scope: &'static str,
    pub operation: OperationRecord,
    pub business: BusinessRecord,
    pub replayed: bool,
}

pub struct PaymentExecutionService<'a> {
    store: PermitStore,
    runtime: &'a Runtime,
    source: Option<CardCatalogSourceBinding>,
    profile: ToolSecurityProfile,
    behavior: MockBehavior,
    background: Option<background::BackgroundConfig>,
}

#[derive(Clone)]
pub(super) struct PaymentExecutionState {
    pub(super) original_binding: ConfirmationBinding,
    request: ExecutionRequest,
    source: CardCatalogSourceBinding,
    card_revision: String,
    snapshot: PaymentSnapshot,
    resolving: bool,
    pub(super) background: Option<background::AcceptedConsent>,
}

struct SnapshotSource(FundsPolicyFacts);
impl FundsPolicySource for SnapshotSource {
    fn read<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<FundsPolicyFacts, SecurityError>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

// Backend-only bridge over the adopted full catalog, on the caller's protected
// connection. A later restriction is an unavailable public execution, never a
// bank rejection: especially on PROCESSING, return an error so the whole outer
// transaction rolls back and the original hold remains. Keep the final guards.
struct PaymentOutgoingSource {
    identity: SessionBinding,
    payer: ResourceId,
    source: CardCatalogSourceBinding,
    revision: String,
    error: Option<StorageError>,
}
impl PaymentOutgoingSource {
    fn new(
        identity: &SessionBinding,
        snapshot: &PaymentSnapshot,
        source: &CardCatalogSourceBinding,
        revision: &str,
    ) -> Self {
        Self {
            identity: identity.clone(),
            payer: snapshot.transfer.payer_account_id.clone(),
            source: source.clone(),
            revision: revision.into(),
            error: None,
        }
    }

    // The member seam returns FundsError. Preserve the actual storage failure
    // instead of converting dependency/revision/DB errors to CorruptRecord.
    fn result<T>(&mut self, result: Result<T, FundsError>) -> Result<T, StorageError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        result.map_err(|error| match error {
            FundsError::TransactionRequired => StorageError::TransactionScope,
            error => crate::transfer_sql::business_error(error),
        })
    }
}
impl LockedOutgoingSource for PaymentOutgoingSource {
    fn read<'a>(
        &'a mut self,
        connection: &'a mut PgConnection,
        owner: &'a UserId,
        payer: &'a ResourceId,
    ) -> ContractFuture<'a, Result<MockOutgoingState, FundsError>> {
        Box::pin(async move {
            let result = async {
                if owner != &self.identity.user_id || payer != &self.payer {
                    return Err(StorageError::PermitBindingMismatch);
                }
                let cards =
                    read_adopted_card_facts(connection, owner, payer, Some(&self.source)).await?;
                if cards.revision != self.revision || cards.outgoing_restricted() {
                    return Err(StorageError::FactsConflict);
                }
                self.source
                    .verify_current(connection, &self.identity, payer)
                    .await?;
                Ok(MockOutgoingState::Allowed)
            }
            .await;
            result.map_err(|error| {
                self.error = Some(error);
                FundsError::OutgoingUnknown
            })
        })
    }
}

impl<'a> PaymentExecutionService<'a> {
    /// Explicit trusted PROJECT SIMULATION assembly. Startup currently has no
    /// adopted full catalog and therefore does not install this service.
    /// Profile/behavior are deployment decisions, never customer parameters.
    pub fn for_project_simulation(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        runtime: &'a Runtime,
        source: Option<CardCatalogSourceBinding>,
        profile: ToolSecurityProfile,
        config: PermitConfig,
        behavior: MockBehavior,
    ) -> Result<Self, StorageError> {
        if profile.tool_name.as_str() != PAYMENT_EXECUTION_TOOL
            || profile.tool_schema_version != 1
            || profile.policy_version.as_str() != PAYMENT_EXECUTION_POLICY
        {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        runtime
            .validate_final_payment_binding_for_backend(&profile, PAYMENT_FINAL_IMPLEMENTATION)
            .map_err(|_| StorageError::DecisionAssemblyChanged)?;
        if !runtime.final_payment_uses_session_resolver_for_backend(&sessions) {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        Ok(Self {
            store: PermitStore::for_trusted_backend(repository, sessions, config),
            runtime,
            source,
            profile,
            behavior,
            background: None,
        })
    }

    /// Only an explicit user execution command may invoke this method. GET,
    /// confirmation and Agent continue keep their existing observation behavior.
    pub async fn execute(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        preparation: &OperationId,
        confirmation: &ConfirmationRef,
        display_ticket: &str,
    ) -> Result<PaymentExecutionResult, StorageError> {
        self.execute_command(session,task_id,preparation,confirmation,display_ticket,None).await
    }
    /// New native contract: explicit consent ID, never a bool on the old DTO.
    pub async fn execute_with_background_delegation(&self,session:&SessionRef,task_id:&TaskId,preparation:&OperationId,confirmation:&ConfirmationRef,display_ticket:&str,consent_id:&str)->Result<PaymentExecutionResult,StorageError>{
        self.execute_command(session,task_id,preparation,confirmation,display_ticket,Some(consent_id)).await
    }
    async fn execute_command(&self,session:&SessionRef,task_id:&TaskId,preparation:&OperationId,confirmation:&ConfirmationRef,display_ticket:&str,consent:Option<&str>)->Result<PaymentExecutionResult,StorageError>{
        let identity=self.store.identity(session)?;
        let guard=||if self.store.identity(session)?==identity {Ok(())}else{Err(StorageError::Session)};
        let pending=crate::repository::guarded_poll(&guard,Box::pin(self.prepare_execution(session,task_id,preparation,confirmation,display_ticket,consent))).await?;
        match pending {
            PendingPayment::Replay(tx,result)=>{guard()?;tx.commit().await.map_err(|_|StorageError::ReconcileRequired)?;guard()?;Ok(result)}
            PendingPayment::New(unit,result)=>{unit.finish(true).await?;guard()?;Ok(result)}
        }
    }
    async fn prepare_execution(&self,session:&SessionRef,task_id:&TaskId,preparation:&OperationId,confirmation:&ConfirmationRef,display_ticket:&str,consent:Option<&str>)->Result<PendingPayment,StorageError>{
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        let transaction_id = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await?;
        if consent.is_some() {
            let accepted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests r JOIN payment_funds_execution_v1 a ON a.execution_operation_id=r.operation_id WHERE r.owner_id=$1 AND a.preparation_operation_id=$2 AND r.first_operation IS NOT NULL)").bind(identity.user_id.as_str()).bind(preparation.as_str()).fetch_one(&mut *tx).await?;
            if !accepted {self.acceptance_fences(&mut tx,session,&identity,task_id,preparation).await?;}
        }
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, task_id, true).await?;
        let guard = || {
            if self.store.identity(session)? != identity {
                Err(StorageError::Session)
            } else {
                Ok(())
            }
        };
        guard()?;
        let ops = operations(&mut tx, task_id).await?;
        crate::lifecycle::project_progress(&mut tx, &task, &ops).await?;
        crate::lifecycle::reject_closed_nonfunding_draft(&mut tx, task_id).await?;
        let saved = saved_payment(&mut tx, &task, preparation, true).await?;
        // Original business identity always precedes issuing or consuming again.
        if let Some(result) = replay(&mut tx, &task, &saved).await? {
            guard()?;
            return Ok(PendingPayment::Replay(tx,result));
        }
        if &saved.stored.record.confirmation_ref != confirmation || saved.display != display_ticket
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        saved
            .stored
            .check_session(&crate::confirmation::ConfirmationSession {
                binding: identity.clone(),
                digest: hash(&("safeflow-confirmation-session:v1", session.as_str()))?,
            })?;
        let delegated=if let Some(id)=consent {Some(self.acceptance_consent(&mut tx,session,&identity,&saved,id).await?)}else{None};
        let original_binding = saved.stored.record.binding.clone();
        let session_digest = hash(&("safeflow-confirmation-session:v1", session.as_str()))?
            .as_str()
            .to_owned();
        verify_stored_confirmation(
            &mut tx,
            confirmation,
            &original_binding,
            &session_digest,
            identity.binding_version,
            self.store.clock.as_ref(),
        )
        .await?;
        if saved.reason.is_some()
            || original_binding.task_revision != task.revision
            || original_binding.plan_version != task.plan.plan_version
            || original_binding.facts_version != task.facts_version
            || saved.base_digest != hash(&task.plan)?
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let source = self
            .source
            .clone()
            .ok_or(StorageError::DependencyUnavailable)?;
        let daily = crate::daily_usage::read_locked(&mut tx, &identity.user_id).await?;
        let key = IdempotencyKey::parse(format!("payment_{}", uuid::Uuid::new_v4())).unwrap();
        protect_current(&mut tx, &identity.user_id, &key, &saved.snapshot.transfer).await?;
        if current_snapshot(
            &mut tx,
            &task,
            &saved.snapshot.transfer.draft_id,
            saved.snapshot.transfer.draft_version,
        )
        .await?
            != saved.snapshot
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        lock_route_and_accounts(&mut tx, &identity.user_id, &saved.snapshot.transfer, false)
            .await?;
        let cards = read_adopted_card_facts(
            &mut tx,
            &identity.user_id,
            &saved.snapshot.transfer.payer_account_id,
            Some(&source),
        )
        .await?;
        source
            .verify_current(
                &mut tx,
                &identity,
                &saved.snapshot.transfer.payer_account_id,
            )
            .await?;
        guard()?;
        let request = create_execution(&mut tx, &task, &saved, key).await?;
        let context = SecurityContext {
            contract_version: SECURITY_CONTRACT_VERSION,
            task_id: task_id.clone(),
            node_id: request.binding.snapshot.node_id.clone(),
            operation_id: request.binding.snapshot.operation_id.clone(),
            facts_version: facts_u32(task.facts_version)?,
            phase: SecurityPhase::PreCommit,
            tool_name: self.profile.tool_name.clone(),
            plan_version: task.plan.plan_version,
            authenticated_user_id: identity.user_id.clone(),
            normalized_arguments: serde_json::to_value(&saved.snapshot.transfer)
                .map_err(|_| StorageError::CorruptRecord)?,
            arguments_hash: hash(
                &serde_json::to_value(&saved.snapshot.transfer)
                    .map_err(|_| StorageError::CorruptRecord)?,
            )?,
            idempotency_key: Some(request.idempotency_key.clone()),
            confirmation_ref: Some(confirmation.clone()),
            auth_grant_ref: None,
        };
        let outgoing = if cards.outgoing_restricted() {
            MockOutgoingState::Blocked
        } else {
            MockOutgoingState::Allowed
        };
        let stored = FundsStore::policy_facts(
            &mut tx,
            &identity.user_id,
            &request.idempotency_key,
            &saved.snapshot.transfer,
        )
        .await
        .map_err(crate::transfer_sql::business_error)?;
        let facts = FundsPolicyFacts {
            binding: context.clone(),
            checked_at: daily.as_of.clone(),
            stored,
            outgoing,
            daily: Some(DailyFacts {
                complete: true,
                submitted: daily.submitted.clone(),
            }),
        };
        let decision = self
            .runtime
            .final_payment_policy_for_backend(
                session,
                &context,
                &self.profile,
                PAYMENT_FINAL_IMPLEMENTATION,
                &LockedFundsFacts(facts),
                daily.usage.used_cents,
                cents(&saved.snapshot)?,
            )
            .await
            .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        // Only this sealed issuer has verified the persisted, current original
        // confirmation. Higher levels await an adopted per-payment credential.
        if matches!(
            decision.outcome,
            SecurityOutcome::Blocked | SecurityOutcome::NeedsInput
        ) {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        if delegated.is_none() && (decision.outcome == SecurityOutcome::WaitingAuth
            || decision
                .required_gates
                .contains(&SecurityGate::StrongAuthentication))
        {
            return Err(StorageError::FormalAuthUnavailable);
        }
        let ready=if delegated.is_some(){decision.outcome==SecurityOutcome::WaitingAuth && decision.minimum_level==SecurityLevel::Red && decision.required_gates==vec![SecurityGate::ExplicitConfirmation,SecurityGate::StrongAuthentication] && decision.findings.iter().all(|f|f.finding.kind==FindingKind::Informational)}else{decision.outcome==SecurityOutcome::ReadyToExecute && decision.required_gates==vec![SecurityGate::ExplicitConfirmation]};
        if !ready {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        audit(&mut tx,&task,None,PersistentAuditKind::SecurityDecisionRecorded,json!({"scope":"LOCKED_FINAL_PAYMENT_POLICY","preparation_operation_id":preparation,
            "original_binding":original_binding,"execution_binding":request.binding.snapshot,"fingerprint":request.binding.fingerprint,
            "decision":decision,"card_revision":cards.revision,"daily_usage":daily.usage,"gates_completed_from_persisted_confirmation":true})).await?;
        reserve(&mut tx, &request).await?;
        let mut outgoing_source =
            PaymentOutgoingSource::new(&identity, &saved.snapshot, &source, &cards.revision);
        let mut unit = PermitTransaction {
            tx: Some(tx),
            store: self.store.clone(),
            session: session.clone(),
            identity,
            task,
            transaction_id,
            consumed: None,
            final_active: None,
            submission: None,
            transfer_result: None,
            billing_result: None,
            subscription_result: None,
            daily_usage: None,
            last_time: Cell::new(None),
            recovery: None,
            commit_started: false,
            card: None,
            payment: Some(PaymentExecutionState {
                original_binding: original_binding.clone(),
                request: request.clone(),
                source,
                card_revision: cards.revision,
                snapshot: saved.snapshot.clone(),
                resolving: false,
                background: delegated.clone(),
            }),
        };
        let verified = VerifiedPermitSnapshot {
            binding: request.binding.snapshot.clone(),
            payload: request.payload.clone(),
            decision_id: decision.decision_id,
            confirmation_ref: confirmation.clone(),
            required_gates: decision.required_gates,
            authentication: delegated.as_ref().map(background::authentication).unwrap_or(VerifiedPermitAuthentication::NotRequired),
            execution_scope: VerifiedPermitScope::Transfer,
            session_digest,
            session_version: unit.identity.binding_version,
        };
        let permit = unit.save(&verified).await?;
        unit.consume(&permit.id, &request.binding).await?;
        let owner = unit.identity.user_id.clone();
        let submitted = FundsStore::submit_checked(
            unit.connection()?,
            &owner,
            &request.idempotency_key,
            &saved.snapshot.transfer,
            self.behavior,
            &mut outgoing_source,
        )
        .await;
        outgoing_source.result(submitted)?;
        unit.record_submission(&request).await?;
        let operation = unit.record_transfer_result().await?;
        let business = crate::transfer_result::verified_record(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            &request,
        )
        .await?;
        if delegated.is_some() && operation.state==OperationState::Processing { background::build_acceptance(&mut unit,preparation).await?; }
        Ok(PendingPayment::New(unit,PaymentExecutionResult {
            scope: "PROJECT_SIMULATION_PAYMENT",
            operation,
            business,
            replayed: false,
        }))
    }

    /// Trusted simulated-bank later completion, never an HTTP/model-selected
    /// outcome. Existing originals alone may resolve; this never submits again.
    pub async fn resolve_simulated_success(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        preparation: &OperationId,
    ) -> Result<PaymentExecutionResult, StorageError> {
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        let transaction_id = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await?;
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, task_id, true).await?;
        let saved = saved_payment(&mut tx, &task, preparation, true).await?;
        let prior = replay(&mut tx, &task, &saved)
            .await?
            .ok_or(StorageError::ReconcileRequired)?;
        let delegated:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_completion_candidate_grants WHERE preparation_operation_id=$1)").bind(preparation.as_str()).fetch_one(&mut *tx).await?;
        if delegated {return Err(StorageError::FormalAuthUnavailable);}
        if prior.business.latest_receipt.status != ToolResultStatus::Processing {
            if self.store.identity(session)? != identity {
                return Err(StorageError::Session);
            }
            tx.commit().await?;
            if self.store.identity(session)? != identity {
                return Err(StorageError::Session);
            }
            return Ok(prior);
        }
        let row=sqlx::query("SELECT * FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2")
            .bind(identity.user_id.as_str()).bind(saved.snapshot.transfer.operation_id.as_str()).fetch_one(&mut *tx).await?;
        let request = crate::execution_request::stored_row(&row)?.request;
        association(&mut tx, &task, &request).await?;
        let binding = &request.binding.snapshot;
        if binding.task_revision != task.revision
            || binding.plan_version != task.plan.plan_version
            || binding.facts_version != task.facts_version
            || saved.base_digest != hash(&task.plan)?
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        crate::daily_usage::read_locked(&mut tx, &identity.user_id).await?;
        protect_current(
            &mut tx,
            &identity.user_id,
            &request.idempotency_key,
            &saved.snapshot.transfer,
        )
        .await?;
        lock_route_and_accounts(&mut tx, &identity.user_id, &saved.snapshot.transfer, true).await?;
        let source = self
            .source
            .clone()
            .ok_or(StorageError::DependencyUnavailable)?;
        let cards = read_adopted_card_facts(
            &mut tx,
            &identity.user_id,
            &saved.snapshot.transfer.payer_account_id,
            Some(&source),
        )
        .await?;
        if cards.outgoing_restricted() {
            return Err(StorageError::FactsConflict);
        }
        source
            .verify_current(
                &mut tx,
                &identity,
                &saved.snapshot.transfer.payer_account_id,
            )
            .await?;
        let original_binding = saved.stored.record.binding.clone();
        let digest = hash(&("safeflow-confirmation-session:v1", session.as_str()))?;
        verify_stored_confirmation(
            &mut tx,
            &saved.stored.record.confirmation_ref,
            &original_binding,
            digest.as_str(),
            identity.binding_version,
            self.store.clock.as_ref(),
        )
        .await?;
        if self.store.identity(session)? != identity {
            return Err(StorageError::Session);
        }
        let mut outgoing_source =
            PaymentOutgoingSource::new(&identity, &saved.snapshot, &source, &cards.revision);
        let resolved = FundsStore::resolve_checked(
            &mut tx,
            &identity.user_id,
            &saved.snapshot.transfer.operation_id,
            MockResolution::Succeed,
            &mut outgoing_source,
        )
        .await;
        let business = outgoing_source.result(resolved)?;
        let row=sqlx::query("UPDATE operations SET state='SUCCEEDED',revision=revision+1 WHERE operation_id=$1 AND state='PROCESSING' AND revision=3 RETURNING *")
            .bind(request.binding.snapshot.operation_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::IllegalTransition)?;
        let operation = operation_row(&row)?;
        audit(&mut tx,&task,Some(&operation),PersistentAuditKind::OperationTransitioned,json!({"source":"transfer-funds-v1","latest_receipt":business.latest_receipt,"card_revision":cards.revision})).await?;
        let unit = PermitTransaction {
            tx: Some(tx),
            store: self.store.clone(),
            session: session.clone(),
            identity: identity.clone(),
            task,
            transaction_id,
            consumed: None,
            final_active: None,
            submission: None,
            transfer_result: Some(operation.clone()),
            billing_result: None,
            subscription_result: None,
            daily_usage: None,
            last_time: Cell::new(None),
            recovery: None,
            commit_started: false,
            card: None,
            payment: Some(PaymentExecutionState {
                original_binding,
                request,
                source,
                card_revision: cards.revision,
                snapshot: saved.snapshot,
                resolving: true,
                background: None,
            }),
        };
        unit.finish(false).await?;
        if self.store.identity(session)? != identity {
            return Err(StorageError::Session);
        }
        Ok(PaymentExecutionResult {
            scope: "PROJECT_SIMULATION_PAYMENT",
            operation,
            business,
            replayed: false,
        })
    }
}

enum PendingPayment { Replay(Transaction<'static,Postgres>,PaymentExecutionResult), New(PermitTransaction,PaymentExecutionResult) }
fn cents(snapshot: &PaymentSnapshot) -> Result<u64, StorageError> {
    let (whole, fraction) = snapshot
        .transfer
        .amount
        .as_str()
        .split_once('.')
        .ok_or(StorageError::FactsConflict)?;
    whole
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(100))
        .and_then(|n| n.checked_add(fraction.parse().ok()?))
        .filter(|n| *n > 0)
        .ok_or(StorageError::FactsConflict)
}

pub(crate) fn validate_request(
    request: &ExecutionRequest,
) -> Result<PaymentSnapshot, StorageError> {
    let b = &request.binding.snapshot;
    if b.tool_name.as_str() != PAYMENT_EXECUTION_TOOL
        || b.lifecycle_anchor.is_none()
        || b.operation_revision != 1
        || request.payload.schema_version != 1
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    check_payload(&request.payload, b)?;
    if serde_json::to_vec(&request.payload)
        .map_err(|_| StorageError::CorruptRecord)?
        .len()
        > 32768
    {
        return Err(StorageError::Invalid("execution payload too large"));
    }
    db_version(request.business_version)?;
    db_version(b.task_revision)?;
    let snapshot: PaymentSnapshot =
        serde_json::from_value(request.payload.normalized_arguments.clone())
            .map_err(|_| StorageError::CorruptRecord)?;
    if serde_json::to_value(&snapshot).map_err(|_| StorageError::CorruptRecord)?
        != request.payload.normalized_arguments
        || snapshot.transfer.task_id != b.task_id
        || snapshot.transfer.operation_id.as_str() != request.business_operation_ref.as_str()
        || snapshot.transfer.draft_id != request.payload.resource_id
        || u64::from(snapshot.transfer.draft_version) != request.business_version
        || compute_operation_fingerprint(&request.payload)
            .map_err(|_| StorageError::CorruptRecord)?
            != request.binding.fingerprint
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    Ok(snapshot)
}

async fn protect_current(
    tx: &mut PgConnection,
    owner: &UserId,
    key: &IdempotencyKey,
    request: &MockTransferRequest,
) -> Result<(), StorageError> {
    let mut keys = [
        json!([
            "transfer-business-operation-v1",
            owner,
            request.operation_id
        ])
        .to_string(),
        json!(["transfer-business-key-v1", owner, key]).to_string(),
    ];
    keys.sort();
    for key in keys {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

async fn lock_route_and_accounts(
    tx: &mut PgConnection,
    owner: &UserId,
    r: &MockTransferRequest,
    original: bool,
) -> Result<(), StorageError> {
    // Submitted operations use their immutable saved recipient; new submissions
    // protect the live owner-scoped route until commit.
    let recipient: String = if original {
        sqlx::query_scalar("SELECT recipient_account_id FROM transfer_fund_orders_v1 WHERE owner_id=$1 AND operation_id=$2 FOR UPDATE").bind(owner.as_str()).bind(r.operation_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::ReconcileRequired)?
    } else {
        sqlx::query(
            "SELECT draft_id FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 FOR UPDATE",
        )
        .bind(owner.as_str())
        .bind(r.draft_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StorageError::NotFound)?;
        sqlx::query_scalar("SELECT recipient_account_id FROM transfer_payee_routes_v1 WHERE owner_id=$1 AND payee_id=$2 FOR SHARE").bind(owner.as_str()).bind(r.payee_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::DependencyUnavailable)?
    };
    let rows:Vec<String>=sqlx::query_scalar("SELECT account_id FROM transfer_accounts_v1 WHERE account_id=$1 OR account_id=$2 ORDER BY account_id FOR UPDATE")
        .bind(r.payer_account_id.as_str()).bind(recipient).fetch_all(&mut *tx).await?;
    if rows.len() != 2 {
        return Err(StorageError::NotFound);
    }
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM transfer_accounts_v1 WHERE account_id=$1 AND owner_id=$2)",
    )
    .bind(r.payer_account_id.as_str())
    .bind(owner.as_str())
    .fetch_one(tx)
    .await?;
    if !owned {
        return Err(StorageError::NotFound);
    }
    Ok(())
}

async fn create_execution(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    saved: &SavedPayment,
    key: IdempotencyKey,
) -> Result<ExecutionRequest, StorageError> {
    let id = crate::new_operation_id();
    let node = NodeId::parse(format!("payment_execute_{}", uuid::Uuid::new_v4())).unwrap();
    let b = ConfirmationBinding {
        user_id: task.owner_id.clone(),
        task_id: task.task_id.clone(),
        operation_id: id.clone(),
        node_id: node.clone(),
        tool_name: ToolName::parse(PAYMENT_EXECUTION_TOOL).unwrap(),
        task_revision: task.revision,
        plan_version: task.plan.plan_version,
        operation_revision: 1,
        facts_version: task.facts_version,
        arguments_hash: hash(
            &serde_json::to_value(&saved.snapshot).map_err(|_| StorageError::CorruptRecord)?,
        )?,
        lifecycle_anchor: saved.stored.record.binding.lifecycle_anchor.clone(),
    };
    let payload = FingerprintPayloadV1 {
        user_id: task.owner_id.clone(),
        operation_id: id.clone(),
        tool_name: b.tool_name.clone(),
        resource_id: saved.snapshot.transfer.draft_id.clone(),
        normalized_arguments: serde_json::to_value(&saved.snapshot)
            .map_err(|_| StorageError::CorruptRecord)?,
        plan_version: b.plan_version,
        schema_version: 1,
    };
    let plan = AgentPlan::new(
        task.task_id.clone(),
        b.plan_version,
        vec![PlanNode::Tool {
            id: node.clone(),
            tool: b.tool_name.clone(),
            arguments: payload.normalized_arguments.clone(),
            depends_on: vec![],
        }],
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    let request = ExecutionRequest {
        idempotency_key: key,
        business_operation_ref: BusinessOperationRef::parse(
            saved.snapshot.transfer.operation_id.as_str(),
        )
        .unwrap(),
        business_version: u64::from(saved.snapshot.transfer.draft_version),
        binding: PermitBinding {
            snapshot: b.clone(),
            fingerprint: compute_operation_fingerprint(&payload)
                .map_err(|_| StorageError::CorruptRecord)?,
        },
        payload,
    };
    validate_request(&request)?;
    sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'READY',1,$4,$5,$6)")
        .bind(id.as_str()).bind(task.task_id.as_str()).bind(node.as_str()).bind(b.plan_version as i64).bind(db_version(b.facts_version)?).bind(b.arguments_hash.as_str()).execute(&mut **tx).await?;
    let digest = hash(&(
        &saved.op.operation_id,
        &id,
        &task.task_id,
        &saved.stored.record.binding,
        &b,
        &plan,
        &saved.digest,
    ))?;
    sqlx::query("INSERT INTO payment_funds_execution_v1(preparation_operation_id,execution_operation_id,task_id,original_binding,execution_binding,execution_plan,snapshot_digest,association_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(saved.op.operation_id.as_str()).bind(id.as_str()).bind(task.task_id.as_str()).bind(Json(&saved.stored.record.binding)).bind(Json(&b)).bind(Json(&plan)).bind(saved.digest.as_str()).bind(digest.as_str()).execute(&mut **tx).await?;
    audit(tx,task,None,PersistentAuditKind::OperationCreated,json!({"payment_execution":{"preparation":saved.op.operation_id,"execution":id,"association_digest":digest}})).await?;
    Ok(request)
}

async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    r: &ExecutionRequest,
) -> Result<(), StorageError> {
    validate_request(r)?;
    let b = &r.binding.snapshot;
    sqlx::query("INSERT INTO public_execution_requests(contract_version,owner_id,idempotency_key,business_operation_ref,business_version,task_id,operation_id,node_id,tool_name,task_revision,operation_revision,plan_version,facts_version,arguments_hash,operation_fingerprint,fingerprint_payload,request_digest,lifecycle_anchor) VALUES(1,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
        .bind(b.user_id.as_str()).bind(r.idempotency_key.as_str()).bind(r.business_operation_ref.as_str()).bind(db_version(r.business_version)?)
        .bind(b.task_id.as_str()).bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str()).bind(db_version(b.task_revision)?).bind(db_version(b.operation_revision)?).bind(b.plan_version as i64).bind(db_version(b.facts_version)?)
        .bind(b.arguments_hash.as_str()).bind(r.binding.fingerprint.as_str()).bind(Json(&r.payload)).bind(r.digest()?).bind(b.lifecycle_anchor.as_ref().map(|a|a.as_str())).execute(&mut **tx).await?;
    Ok(())
}

async fn association(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    r: &ExecutionRequest,
) -> Result<SavedPayment, StorageError> {
    let snapshot = validate_request(r)?;
    let b = &r.binding.snapshot;
    let row = sqlx::query(
        "SELECT * FROM payment_funds_execution_v1 WHERE execution_operation_id=$1 AND task_id=$2",
    )
    .bind(b.operation_id.as_str())
    .bind(task.task_id.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::CorruptRecord)?;
    let preparation: OperationId = decode(&row, "preparation_operation_id")?;
    let saved = saved_payment(tx, task, &preparation, false).await?;
    let plan = row.try_get::<Json<AgentPlan>, _>("execution_plan")?.0;
    let digest = hash(&(
        &preparation,
        &b.operation_id,
        &task.task_id,
        &saved.stored.record.binding,
        b,
        &plan,
        &saved.digest,
    ))?;
    if row.try_get::<Json<Value>, _>("original_binding")?.0 != json!(saved.stored.record.binding)
        || row.try_get::<Json<Value>, _>("execution_binding")?.0 != json!(b)
        || row.try_get::<String, _>("snapshot_digest")? != saved.digest.as_str()
        || row.try_get::<String, _>("association_digest")? != digest.as_str()
        || snapshot != saved.snapshot
        || b.lifecycle_anchor != saved.stored.record.binding.lifecycle_anchor
        || b.arguments_hash != hash(&r.payload.normalized_arguments)?
        || b.user_id != task.owner_id
        || b.task_id != task.task_id
        || plan
            != AgentPlan::new(
                task.task_id.clone(),
                b.plan_version,
                vec![PlanNode::Tool {
                    id: b.node_id.clone(),
                    tool: b.tool_name.clone(),
                    arguments: r.payload.normalized_arguments.clone(),
                    depends_on: vec![],
                }],
            )
            .map_err(|_| StorageError::CorruptRecord)?
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(saved)
}

pub(crate) async fn view_locked(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    stored: crate::execution_request::StoredRequest,
) -> Result<ExecutionRequestView, StorageError> {
    association(tx, task, &stored.request).await?;
    let row = sqlx::query("SELECT * FROM operations WHERE operation_id=$1 AND task_id=$2")
        .bind(stored.request.binding.snapshot.operation_id.as_str())
        .bind(task.task_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let op = operation_row(&row)?;
    let b = &stored.request.binding.snapshot;
    if op.node_id != b.node_id
        || op.arguments_hash != b.arguments_hash
        || op.plan_version != b.plan_version
        || op.facts_version != b.facts_version
        || (stored.first_submission.is_some()
            && (op.business_operation_ref.as_ref() != Some(&stored.request.business_operation_ref)
                || op.revision < 2))
        || (stored.first_submission.is_none()
            && (op.state != OperationState::Ready || op.revision != 1))
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(ExecutionRequestView {
        request: stored.request,
        reserved_at: stored.reserved_at,
        first_submission: stored.first_submission,
        current_operation: op,
    })
}

async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    saved: &SavedPayment,
) -> Result<Option<PaymentExecutionResult>, StorageError> {
    let row = sqlx::query(
        "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2",
    )
    .bind(task.owner_id.as_str())
    .bind(saved.snapshot.transfer.operation_id.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = row {
        let stored = crate::execution_request::stored_row(&row)?;
        if crate::transfer_result::bound_request(&stored.request)? != saved.snapshot.transfer {
            return Err(StorageError::ExecutionKeyConflict);
        }
        if stored.first_submission.is_none() {
            return Err(StorageError::ReconcileRequired);
        }
        let business = crate::transfer_result::verified_business_record(
            tx,
            &task.owner_id,
            &saved.snapshot.transfer,
            &stored.request.idempotency_key,
        )
        .await?;
        let public = crate::execution_request::view_locked(tx, task, stored).await?;
        let expected = match business.latest_receipt.status {
            ToolResultStatus::Succeeded => OperationState::Succeeded,
            ToolResultStatus::Failed => OperationState::Failed,
            ToolResultStatus::Processing => OperationState::Processing,
        };
        if public.current_operation.state != expected {
            return Err(StorageError::ReconcileRequired);
        }
        return Ok(Some(PaymentExecutionResult {
            scope: "PROJECT_SIMULATION_PAYMENT",
            operation: public.current_operation,
            business,
            replayed: true,
        }));
    }
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$3 AND submitted)")
        .bind(task.owner_id.as_str()).bind(saved.snapshot.transfer.operation_id.as_str()).bind(saved.snapshot.transfer.draft_id.as_str()).fetch_one(&mut **tx).await?;
    if exists {
        return Err(StorageError::ReconcileRequired);
    }
    Ok(None)
}

impl PermitTransaction {
    pub(super) async fn payment_current(
        &mut self,
        binding: &PermitBinding,
        confirmation: &ConfirmationRef,
    ) -> Result<i64, StorageError> {
        self.scope_guard().await?;
        self.session_guard()?;
        let digest = self.session_digest()?;
        let p = self
            .payment
            .clone()
            .ok_or(StorageError::TransactionAborted)?;
        if binding != &p.request.binding {
            return Err(StorageError::PermitBindingMismatch);
        }
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let task = lock_task_for_mode(tx, &self.identity.user_id, &self.task.task_id, true).await?;
        if task != self.task {
            return Err(StorageError::PermitBindingMismatch);
        }
        let saved = association(tx, &task, &p.request).await?;
        if saved.stored.record.binding != p.original_binding
            || saved.reason.is_some()
            || &saved.stored.record.confirmation_ref != confirmation
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let state: String =
            sqlx::query_scalar("SELECT state FROM operations WHERE operation_id=$1")
                .bind(binding.snapshot.operation_id.as_str())
                .fetch_one(&mut **tx)
                .await?;
        if state != "READY" {
            return Err(StorageError::IllegalTransition);
        }
        let expires = verify_stored_confirmation(
            tx,
            confirmation,
            &p.original_binding,
            &digest,
            self.identity.binding_version,
            self.store.clock.as_ref(),
        )
        .await?;
        self.session_guard()?;
        Ok(expires.timestamp_millis())
    }
    pub(super) async fn payment_record_submission(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<PublicSubmissionReceipt, StorageError> {
        let consumed = self
            .consumed
            .clone()
            .ok_or(StorageError::TransactionAborted)?;
        if self.submission.is_some()
            || request.binding != consumed.binding
            || request.payload != consumed.payload
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        self.payment_current(&request.binding, &consumed.confirmation_ref)
            .await?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let row=sqlx::query("UPDATE operations SET state='SUBMITTED',revision=2,business_operation_ref=$1 WHERE operation_id=$2 AND state='READY' AND revision=1 RETURNING *")
            .bind(request.business_operation_ref.as_str()).bind(request.binding.snapshot.operation_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::RevisionConflict)?;
        let op = operation_row(&row)?;
        let recorded_at=sqlx::query_scalar("UPDATE public_execution_requests SET first_operation=$1,submitted_at=clock_timestamp() WHERE owner_id=$2 AND idempotency_key=$3 AND first_operation IS NULL RETURNING submitted_at")
            .bind(Json(&op)).bind(self.identity.user_id.as_str()).bind(request.idempotency_key.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ExecutionOperationConflict)?;
        audit(tx,&self.task,Some(&op),PersistentAuditKind::OperationTransitioned,json!({"event":"SUBMISSION_RECORDED","original_reference":request.business_operation_ref})).await?;
        let receipt = PublicSubmissionReceipt {
            operation: op,
            recorded_at,
        };
        self.submission = Some((request.clone(), receipt.clone()));
        Ok(receipt)
    }
    pub(super) async fn payment_record_result(&mut self) -> Result<OperationRecord, StorageError> {
        let (request, _) = self
            .submission
            .clone()
            .ok_or(StorageError::TransactionAborted)?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let business = crate::transfer_result::verified_record(tx, &request).await?;
        let state = match business.latest_receipt.status {
            ToolResultStatus::Succeeded => "SUCCEEDED",
            ToolResultStatus::Failed => "FAILED",
            ToolResultStatus::Processing => "PROCESSING",
        };
        let row=sqlx::query("UPDATE operations SET state=$1,revision=revision+1 WHERE operation_id=$2 AND state='SUBMITTED' RETURNING *").bind(state).bind(request.binding.snapshot.operation_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::IllegalTransition)?;
        let op = operation_row(&row)?;
        audit(tx,&self.task,Some(&op),PersistentAuditKind::OperationTransitioned,json!({"source":"transfer-funds-v1","first_receipt":business.first_receipt,"latest_receipt":business.latest_receipt})).await?;
        self.transfer_result = Some(op.clone());
        self.session_guard()?;
        Ok(op)
    }
    pub(super) async fn payment_final_snapshot(
        &mut self,
        consumed: &PermitRow,
    ) -> Result<(), StorageError> {
        let p = self
            .payment
            .clone()
            .ok_or(StorageError::TransactionAborted)?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let current =
            lock_task_for_mode(tx, &self.identity.user_id, &self.task.task_id, true).await?;
        if current != self.task || consumed.binding != p.request.binding {
            return Err(StorageError::PermitBindingMismatch);
        }
        association(tx, &current, &p.request).await?;
        verify_submitted_snapshot(tx, &self.identity.user_id, &p.snapshot).await?;
        let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut **tx)
            .await?;
        if now.timestamp() >= p.snapshot.draft_expires_at as i64 {
            return Err(StorageError::ConfirmationExpired);
        }
        let row = sqlx::query("SELECT * FROM operations WHERE operation_id=$1")
            .bind(p.request.binding.snapshot.operation_id.as_str())
            .fetch_one(&mut **tx)
            .await?;
        let op = operation_row(&row)?;
        if self.transfer_result.as_ref() != Some(&op) || op.revision != 3 {
            return Err(StorageError::PermitBindingMismatch);
        }
        let cards = read_adopted_card_facts(
            tx,
            &self.identity.user_id,
            &p.snapshot.transfer.payer_account_id,
            Some(&p.source),
        )
        .await?;
        if cards.revision != p.card_revision || cards.outgoing_restricted() {
            return Err(StorageError::FactsConflict);
        }
        p.source
            .verify_current(tx, &self.identity, &p.snapshot.transfer.payer_account_id)
            .await?;
        let business = crate::transfer_result::verified_record(tx, &p.request).await?;
        let expected = match business.latest_receipt.status {
            ToolResultStatus::Succeeded => OperationState::Succeeded,
            ToolResultStatus::Failed => OperationState::Failed,
            ToolResultStatus::Processing => OperationState::Processing,
        };
        if op.state != expected {
            return Err(StorageError::CorruptRecord);
        }
        self.session_guard()?;
        Ok(())
    }

    pub(super) async fn payment_resolution_guard(&mut self) -> Result<(), StorageError> {
        let p = self
            .payment
            .clone()
            .ok_or(StorageError::TransactionAborted)?;
        if !p.resolving {
            return Ok(());
        }
        self.scope_guard().await?;
        self.session_guard()?;
        let digest = self.session_digest()?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let current =
            lock_task_for_mode(tx, &self.identity.user_id, &self.task.task_id, true).await?;
        if current != self.task {
            return Err(StorageError::PermitBindingMismatch);
        }
        let saved = association(tx, &current, &p.request).await?;
        verify_submitted_snapshot(tx, &self.identity.user_id, &p.snapshot).await?;
        verify_stored_confirmation(
            tx,
            &saved.stored.record.confirmation_ref,
            &p.original_binding,
            &digest,
            self.identity.binding_version,
            self.store.clock.as_ref(),
        )
        .await?;
        let row = sqlx::query(
            "SELECT * FROM execution_permits WHERE operation_id=$1 AND facts_version=$2",
        )
        .bind(p.request.binding.snapshot.operation_id.as_str())
        .bind(db_version(p.request.binding.snapshot.facts_version)?)
        .fetch_one(&mut **tx)
        .await?;
        let permit = permit_row(&row)?;
        if permit.status != ExecutionPermitStatus::Consumed
            || permit.binding != p.request.binding
            || permit.payload != p.request.payload
            || permit.execution_scope != Some(VerifiedPermitScope::Transfer)
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        let cards = read_adopted_card_facts(
            tx,
            &self.identity.user_id,
            &p.snapshot.transfer.payer_account_id,
            Some(&p.source),
        )
        .await?;
        if cards.revision != p.card_revision || cards.outgoing_restricted() {
            return Err(StorageError::FactsConflict);
        }
        p.source
            .verify_current(tx, &self.identity, &p.snapshot.transfer.payer_account_id)
            .await?;
        let business = crate::transfer_result::verified_record(tx, &p.request).await?;
        let row = sqlx::query("SELECT * FROM operations WHERE operation_id=$1")
            .bind(p.request.binding.snapshot.operation_id.as_str())
            .fetch_one(&mut **tx)
            .await?;
        let op = operation_row(&row)?;
        if self.transfer_result.as_ref() != Some(&op)
            || op.revision != 4
            || op.state != OperationState::Succeeded
            || business.latest_receipt.status != ToolResultStatus::Succeeded
        {
            return Err(StorageError::CorruptRecord);
        }
        crate::daily_usage::read_locked(tx, &self.identity.user_id).await?;
        self.session_guard()?;
        Ok(())
    }
}

async fn verify_submitted_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    snapshot: &PaymentSnapshot,
) -> Result<(), StorageError> {
    let draft = FundsStore::draft(tx, owner, &snapshot.transfer.draft_id)
        .await
        .map_err(crate::transfer_sql::business_error)?;
    let r = &snapshot.transfer;
    let details = draft.details.as_ref().ok_or(StorageError::CorruptRecord)?;
    let submitted: bool = sqlx::query_scalar(
        "SELECT submitted FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2",
    )
    .bind(owner.as_str())
    .bind(r.draft_id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    if !submitted
        || &draft.user_id != owner
        || draft.task_id != r.task_id
        || draft.operation_id != r.operation_id
        || draft.plan_version != r.draft_version
        || draft.expires_at != snapshot.draft_expires_at
        || serde_json::to_value(&draft.details).map_err(|_| StorageError::CorruptRecord)?
            != snapshot.display_details
        || details.account.account_id != r.payer_account_id
        || details.payee.payee_id != r.payee_id
        || details.amount != r.amount
        || details.currency != r.currency
        || details.remark != r.remark
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    Ok(())
}
