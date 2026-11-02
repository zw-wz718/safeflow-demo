use crate::confirmation::{current_binding, timestamp};
use crate::confirmation_repository::{legacy_binding_row, verify_stored_confirmation};
use crate::models::{db_version, hash, valid_id, validate_checkpoint, validate_plan};
use crate::repository::{audit, decode, lock_task, operations};
use crate::{
    ConfirmationBinding, ConfirmationClock, ExecutionRequest, PersistentAuditKind, PgRepository,
    PublicSubmissionReceipt, RecoveryPoint, StorageError, TaskRecord,
};
use safeflow_contracts::{
    compute_operation_fingerprint, AuthGrantRef, ConfirmationRef, ContractFuture, DecisionId,
    ExecutionPermit, ExecutionPermitStatus, FingerprintPayloadV1, OperationFingerprint,
    OperationState, PermitId, PermitIssuanceRequest, SecurityGate, SecurityOutcome, SessionRef,
    TaskId, TaskState, EXECUTION_PERMIT_CONTRACT_VERSION,
};
use safeflow_runtime::{SessionBinding, SessionResolver};
use serde_json::json;
use sqlx::{postgres::PgRow, types::Json, PgConnection, Postgres, Row, Transaction};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
mod billing;
pub(crate) mod card_safety;
pub use card_safety::{CardSafetyService, CardSafetyHost, CardSafetyView, CardChallengeDelivery, CardChallengeSecret};
pub(crate) use billing::CheckedBillingFeedback;
mod subscription;
pub(crate) use subscription::subscription_deadline;
pub(crate) use subscription::domain_original_check;
pub use subscription::{SubscriptionA2Candidate, SubscriptionA2CandidateScope, SubscriptionA2Result};
pub use subscription::{SubscriptionPublicScope, SubscriptionPublicRequest, SubscriptionPublicWrites};
pub use subscription::{
    SubscriptionPreparation, SubscriptionRegistrationCapture, SubscriptionRegistrationSelection,
    SubscriptionWriteService,
};
pub(crate) mod payment;
pub(crate) use billing::recovery::installation_lock as billing_installation_lock;
pub(crate) use billing::recovery::reject_generic as reject_billing_recovery_generic;
pub use billing::{BillingDomainAuditInstallation, BillingDomainAuditStatus};
pub use billing::{BillingFeedbackPreparation, BillingFeedbackService, BillingFeedbackView};
pub use billing::BillingFeedbackRevisionAction;
pub use billing::{BillingRecoveryConfig, BillingRecoveryNonce};
pub use payment::{
    PaymentBackgroundDisplay, PaymentBackgroundCode, PaymentBackgroundHost,
    payment_final_registration_for_backend, PaymentExecutionResult, PaymentExecutionService,
    PaymentAcceptedHistoryReader, PaymentAcceptedHistoryReport,
};

pub const PERMIT_STORAGE_CONTRACT_VERSION: u16 = 2;

#[derive(Clone)]
pub struct PermitConfig {
    ttl_ms: i64,
}
impl PermitConfig {
    pub fn new(ttl: Duration) -> Result<Self, StorageError> {
        let ttl_ms = i64::try_from(ttl.as_millis())
            .map_err(|_| StorageError::Invalid("permit TTL out of range"))?;
        if ttl_ms <= 0 || ttl.subsec_nanos() % 1_000_000 != 0 {
            return Err(StorageError::Invalid(
                "permit TTL requires positive whole milliseconds",
            ));
        }
        Ok(Self { ttl_ms })
    }
}
struct ServerClock;
impl ConfirmationClock for ServerClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        SystemTime::now().into()
    }
}

/// Sealed handoff for a FUTURE verified issuer. Production has no constructor:
/// decision/authentication/projection sources are not connected. Not a wire DTO.
///
/// ```compile_fail
/// let _: safeflow_storage::VerifiedPermitSnapshot = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail
/// fn forge(snapshot: &mut safeflow_storage::VerifiedPermitSnapshot) {
///     snapshot.session_version = 1;
/// }
/// ```
pub struct VerifiedPermitSnapshot {
    binding: ConfirmationBinding,
    payload: FingerprintPayloadV1,
    decision_id: DecisionId,
    confirmation_ref: ConfirmationRef,
    required_gates: Vec<SecurityGate>,
    authentication: VerifiedPermitAuthentication,
    execution_scope: VerifiedPermitScope,
    session_digest: String,
    session_version: u64,
}

// Only the sealed verified handoff can select a domain, never a wire argument.
// The future issuer must derive this from the adopted Tool/Profile adapter.
#[derive(Clone, Copy, PartialEq)]
enum VerifiedPermitScope {
    Common,
    Transfer,
    IsolatedCard,
}
impl VerifiedPermitScope {
    fn stored(self) -> &'static str {
        match self {
            Self::Common => "COMMON",
            Self::Transfer => "TRANSFER",
            Self::IsolatedCard => "ISOLATED_CARD",
        }
    }
}

// Only a future trusted issuer can populate the sealed snapshot. A reference alone
// is not authentication evidence, and isolated simulation grants are a different type.
#[derive(Clone, PartialEq)]
enum VerifiedPermitAuthentication {
    NotRequired,
    Verified { grant_ref: AuthGrantRef },
    IsolatedCard { grant_ref: AuthGrantRef },
    IsolatedPayment { grant_ref: AuthGrantRef },
}

#[derive(Clone, PartialEq)]
enum StoredPermitAuthentication {
    LegacyRequired {
        grant_ref: AuthGrantRef,
    },
    Current {
        required_gates: Vec<SecurityGate>,
        authentication: VerifiedPermitAuthentication,
    },
}

impl StoredPermitAuthentication {
    fn verified(
        gates: &[SecurityGate],
        authentication: &VerifiedPermitAuthentication,
    ) -> Result<Self, StorageError> {
        match (gates, authentication) {
            ([SecurityGate::ExplicitConfirmation], VerifiedPermitAuthentication::NotRequired) => {}
            (
                [SecurityGate::ExplicitConfirmation, SecurityGate::StrongAuthentication],
                VerifiedPermitAuthentication::Verified { grant_ref },
            ) => {
                valid_id(grant_ref.as_str())?;
                if grant_ref.as_str().starts_with("sim_grant_") || grant_ref.as_str().starts_with("isolated_card_grant_") || grant_ref.as_str().starts_with("isolated_payment_grant_") {
                    return Err(StorageError::PermitBindingMismatch);
                }
            }
            ([SecurityGate::ExplicitConfirmation, SecurityGate::StrongAuthentication],
                VerifiedPermitAuthentication::IsolatedCard { grant_ref })
                if grant_ref.as_str().starts_with("isolated_card_grant_") => {}
            ([SecurityGate::ExplicitConfirmation, SecurityGate::StrongAuthentication],
                VerifiedPermitAuthentication::IsolatedPayment { grant_ref })
                if grant_ref.as_str().starts_with("isolated_payment_grant_") => {}
            _ => return Err(StorageError::PermitBindingMismatch),
        }
        Ok(Self::Current {
            required_gates: gates.to_vec(),
            authentication: authentication.clone(),
        })
    }
    fn storage_version(&self) -> i16 {
        match self {
            Self::LegacyRequired { .. } => 1,
            Self::Current { authentication: VerifiedPermitAuthentication::IsolatedCard { .. }, .. } => 3,
            Self::Current { authentication: VerifiedPermitAuthentication::IsolatedPayment { .. }, .. } => 4,
            Self::Current { .. } => PERMIT_STORAGE_CONTRACT_VERSION as i16,
        }
    }
    fn state(&self) -> &'static str {
        match self {
            Self::LegacyRequired { .. } => "LEGACY_REQUIRED",
            Self::Current {
                authentication: VerifiedPermitAuthentication::NotRequired,
                ..
            } => "NOT_REQUIRED",
            Self::Current { authentication: VerifiedPermitAuthentication::IsolatedCard { .. }, .. } => "ISOLATED_CARD_VERIFIED",
            Self::Current { authentication: VerifiedPermitAuthentication::IsolatedPayment { .. }, .. } => "ISOLATED_PAYMENT_VERIFIED",
            Self::Current {
                authentication: VerifiedPermitAuthentication::Verified { .. },
                ..
            } => "VERIFIED_REQUIRED",
        }
    }
    fn grant_ref(&self) -> Option<&AuthGrantRef> {
        match self {
            Self::LegacyRequired { grant_ref }
            | Self::Current {
                authentication: VerifiedPermitAuthentication::Verified { grant_ref } | VerifiedPermitAuthentication::IsolatedCard { grant_ref } | VerifiedPermitAuthentication::IsolatedPayment { grant_ref },
                ..
            } => Some(grant_ref),
            Self::Current {
                authentication: VerifiedPermitAuthentication::NotRequired,
                ..
            } => None,
        }
    }
    fn gates(&self) -> Option<&[SecurityGate]> {
        match self {
            Self::LegacyRequired { .. } => None,
            Self::Current { required_gates, .. } => Some(required_gates),
        }
    }
}

/// Comparison data, never authority. Includes the full fingerprint, not arguments_hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermitBinding {
    pub snapshot: ConfirmationBinding,
    pub fingerprint: OperationFingerprint,
}

/// Backend-only record. Deliberately not Serialize/Deserialize/Debug.
pub struct StoredExecutionPermit {
    permit: ExecutionPermit,
    binding: PermitBinding,
}
impl StoredExecutionPermit {
    pub fn permit_id(&self) -> &PermitId {
        self.permit.permit_id()
    }
    pub fn binding(&self) -> &PermitBinding {
        &self.binding
    }
    pub fn execution_permit(&self) -> &ExecutionPermit {
        &self.permit
    }
}

#[derive(Clone)]
pub struct PermitStore {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
    clock: Arc<dyn ConfirmationClock>,
    config: PermitConfig,
}
impl PermitStore {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: PermitConfig,
    ) -> Self {
        Self::for_trusted_backend_with_clock(repository, sessions, config, Arc::new(ServerClock))
    }
    pub fn for_trusted_backend_with_clock(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: PermitConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Self {
        Self {
            repository,
            sessions,
            config,
            clock,
        }
    }
    /// Explicitly unavailable, regardless of any caller-supplied READY enum/reference.
    pub async fn request_issuance(
        &self,
        session: &SessionRef,
        _task: &TaskId,
    ) -> Result<StoredExecutionPermit, StorageError> {
        self.identity(session)?;
        Err(StorageError::PermitIssuanceUnavailable)
    }

    /// Storage primitive only. The sealed snapshot cannot currently be obtained by
    /// production callers; isolated crate tests supply public-contract fixtures.
    pub async fn save_verified(
        &self,
        session: &SessionRef,
        verified: &VerifiedPermitSnapshot,
    ) -> Result<StoredExecutionPermit, StorageError> {
        let mut unit = self.open(session, &verified.binding.task_id).await?;
        let result = unit.save(verified).await;
        match result {
            Ok(row) => {
                unit.finish(false).await?;
                row.active_contract()
            }
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }
    pub async fn read(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &PermitId,
        expected: &PermitBinding,
    ) -> Result<StoredExecutionPermit, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = unit.checked_row(id, expected).await;
        match result {
            Ok(row) => {
                unit.finish(false).await?;
                row.active_contract()
            }
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }
    pub async fn revoke(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &PermitId,
    ) -> Result<(), StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = unit.revoke(id).await;
        match result {
            Ok(()) => unit.finish(false).await,
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }

    /// Owns the transaction and final commit. A callback error, poisoned consume,
    /// failed final guard or cancellation rolls back consumption and all SQL.
    pub async fn transaction<T: Send, F>(
        &self,
        session: &SessionRef,
        task: &TaskId,
        work: F,
    ) -> Result<T, StorageError>
    where
        F: for<'a> FnOnce(&'a mut PermitTransaction) -> ContractFuture<'a, Result<T, StorageError>>
            + Send,
    {
        let mut unit = self.open(session, task).await?;
        match work(&mut unit).await {
            Ok(value) => {
                unit.finish(true).await?;
                Ok(value)
            }
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }

    /// Public submission composition with an atomic, read-authorized replay path.
    /// Replay never invokes work and never reads/consumes a fresh Permit. First
    /// submission must use consume + record_submission inside this unit of work.
    /// Domain funding/Audit/Outbox adapters are still required before enabling funds.
    pub async fn submission_transaction<F>(
        &self,
        session: &SessionRef,
        request: &ExecutionRequest,
        work: F,
    ) -> Result<PublicSubmissionReceipt, StorageError>
    where
        F: for<'a> FnOnce(
                &'a mut PermitTransaction,
            ) -> ContractFuture<'a, Result<(), StorageError>>
            + Send,
    {
        request.validate()?;
        let mut unit = self
            .open(session, &request.binding.snapshot.task_id)
            .await?;
        if unit.identity.user_id != request.binding.snapshot.user_id {
            unit.rollback().await;
            return Err(StorageError::NotFound);
        }
        let existing = crate::execution_request::reserve_locked(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            &unit.task,
            request,
        )
        .await;
        match existing {
            Ok(stored) => {
                let view = crate::execution_request::view_locked(
                    unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
                    &unit.task,
                    stored,
                )
                .await?;
                unit.session_guard()?;
                if let Some(receipt) = view.first_submission {
                    let identity = unit.identity.clone();
                    let finished = unit.finish(false).await;
                    if self.identity(session)? != identity {
                        return Err(StorageError::Session);
                    }
                    finished?;
                    return Ok(receipt);
                }
            }
            Err(error) => {
                unit.rollback().await;
                return Err(error);
            }
        }
        let ops = operations(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            &unit.task.task_id,
        )
        .await?;
        if unit.task.state != TaskState::Running
            || !ops.iter().any(|op| {
                op.operation_id == request.binding.snapshot.operation_id
                    && op.state == OperationState::Ready
            })
            || current_binding(&unit.task, &ops, &request.binding.snapshot.operation_id)?
                != request.binding.snapshot
        {
            unit.rollback().await;
            return Err(StorageError::PermitBindingMismatch);
        }
        unit.session_guard()?;
        match work(&mut unit).await {
            Ok(()) => {
                let Some((recorded, receipt)) = unit.submission.clone() else {
                    unit.rollback().await;
                    return Err(StorageError::TransactionAborted);
                };
                if &recorded != request {
                    unit.rollback().await;
                    return Err(StorageError::PermitBindingMismatch);
                }
                let identity = unit.identity.clone();
                let finished = unit.finish(true).await;
                if self.identity(session)? != identity {
                    return Err(StorageError::Session);
                }
                finished?;
                Ok(receipt)
            }
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }
    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        valid_id(session.as_str())?;
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(binding.user_id.as_str())?;
        db_version(binding.binding_version)?;
        Ok(binding)
    }
    async fn open(
        &self,
        session: &SessionRef,
        task: &TaskId,
    ) -> Result<PermitTransaction, StorageError> {
        valid_id(task.as_str())?;
        let identity = self.identity(session)?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT set_config('search_path',$1,true)")
            .bind(&self.repository.namespace)
            .execute(&mut *tx)
            .await?;
        let transaction_id: i64 = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await?;
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        if self.identity(session)? != identity {
            return Err(StorageError::Session);
        }
        let task_record = lock_task(&mut tx, &identity.user_id, task).await?;
        let unit = PermitTransaction {
            tx: Some(tx),
            store: self.clone(),
            session: session.clone(),
            identity,
            task: task_record,
            transaction_id,
            consumed: None,
            final_active: None,
            submission: None,
            transfer_result: None,
            billing_result: None,
            subscription_result: None,
            daily_usage: None,
            last_time: Cell::new(None),
            payment: None,
            recovery: None,
            commit_started: false,
            card: None,
        };
        unit.session_guard()?;
        Ok(unit)
    }
}

/// Factory-created, task-locked unit of work. No external transaction/commit API.
pub struct PermitTransaction {
    tx: Option<Transaction<'static, Postgres>>,
    card: Option<card_safety::CardProtection>,
    store: PermitStore,
    session: SessionRef,
    identity: SessionBinding,
    task: TaskRecord,
    transaction_id: i64,
    consumed: Option<PermitRow>,
    final_active: Option<PermitRow>,
    submission: Option<(ExecutionRequest, PublicSubmissionReceipt)>,
    transfer_result: Option<crate::OperationRecord>,
    billing_result: Option<billing::BillingCompletion>,
    subscription_result: Option<subscription::SubscriptionCompletion>,
    daily_usage: Option<crate::DailyUsageSnapshot>,
    last_time: Cell<Option<i64>>,
    payment: Option<payment::PaymentExecutionState>,
    recovery: Option<billing::recovery::Admission>,
    commit_started: bool,
}
impl PermitTransaction {
    /// Snapshot actually consumed before the first write, under the owner lock.
    /// This does not supply missing final funding rules, authentication or card locks.
    pub fn daily_usage_snapshot(&self) -> Option<&crate::DailyUsageSnapshot> {
        self.daily_usage.as_ref()
    }
    /// After same-connection business submission, verify its original SQL record
    /// and ledger and persist the public outcome + audit/outbox before COMMIT.
    /// No caller-selected status or new funds authorization is accepted here.
    pub async fn record_transfer_result(&mut self) -> Result<crate::OperationRecord, StorageError> {
        let result = self.record_transfer_result_inner().await;
        if result.is_err() {
            self.rollback().await;
        }
        result
    }

    async fn record_transfer_result_inner(
        &mut self,
    ) -> Result<crate::OperationRecord, StorageError> {
        if self.payment.is_some() {
            return self.payment_record_result().await;
        }
        self.session_guard()?;
        if self.transfer_result.is_some() {
            return Err(StorageError::TransactionAborted);
        }
        let (request, _) = self
            .submission
            .clone()
            .ok_or(StorageError::TransactionAborted)?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let task = lock_task(tx, &self.identity.user_id, &self.task.task_id).await?;
        let record = crate::transfer_result::verified_record(tx, &request).await?;
        let next = crate::transfer_result::settle(tx, &task, &request, &record).await?;
        self.transfer_result = Some(next.clone());
        self.session_guard()?;
        Ok(next)
    }
    /// Persist the public submission and original-request association on the same
    /// connection as consume. This is not a funding entry or a bank result adapter.
    /// The trusted adapter still owes all domain/Audit/Outbox writes in contract 05.
    pub async fn record_submission(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<PublicSubmissionReceipt, StorageError> {
        let result = self.record_submission_inner(request).await;
        if result.is_err() {
            self.rollback().await;
        }
        result
    }

    async fn record_submission_inner(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<PublicSubmissionReceipt, StorageError> {
        if self.payment.is_some() {
            return self.payment_record_submission(request).await;
        }
        request.validate()?;
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
        self.current(&consumed.binding, &consumed.confirmation_ref)
            .await?;
        self.session_guard()?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let stored = crate::execution_request::reserve_locked(tx, &self.task, request).await?;
        if stored.first_submission.is_some() {
            return Err(StorageError::ExecutionOperationConflict);
        }
        let mut ops = operations(tx, &self.task.task_id).await?;
        let op = ops
            .iter_mut()
            .find(|op| op.operation_id == request.binding.snapshot.operation_id)
            .ok_or(StorageError::NotFound)?;
        op.state = op
            .state
            .transition(safeflow_contracts::OperationEvent::SubmissionRecorded)
            .map_err(|_| StorageError::IllegalTransition)?;
        op.revision = op
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        op.business_operation_ref = Some(request.business_operation_ref.clone());
        let op = op.clone();
        let mut task = self.task.clone();
        if task.checkpoint
            != (RecoveryPoint::AtNode {
                node_id: op.node_id.clone(),
            })
        {
            return Err(StorageError::IllegalTransition);
        }
        task.state = task
            .state
            .transition(safeflow_contracts::TaskEvent::AwaitExternal)
            .map_err(|_| StorageError::IllegalTransition)?;
        task.revision = task
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        task.checkpoint = RecoveryPoint::ReconcileOperation {
            operation_id: op.operation_id.clone(),
        };
        validate_checkpoint(&task, &ops)?;
        let updated = sqlx::query("UPDATE operations SET state='SUBMITTED',revision=$1,business_operation_ref=$2,updated_at=clock_timestamp() WHERE operation_id=$3 AND task_id=$4 AND state='READY' AND revision=$5 AND business_operation_ref IS NULL")
            .bind(db_version(op.revision)?).bind(request.business_operation_ref.as_str()).bind(op.operation_id.as_str()).bind(task.task_id.as_str()).bind(db_version(request.binding.snapshot.operation_revision)?).execute(&mut **tx).await?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::RevisionConflict);
        }
        crate::repository::update_task(tx, &task, self.task.revision).await?;
        let recorded_at = sqlx::query_scalar("UPDATE public_execution_requests SET first_operation=$1,submitted_at=clock_timestamp() WHERE owner_id=$2 AND idempotency_key=$3 AND first_operation IS NULL RETURNING submitted_at")
            .bind(Json(&op)).bind(task.owner_id.as_str()).bind(request.idempotency_key.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ExecutionOperationConflict)?;
        audit(tx, &task, Some(&op), PersistentAuditKind::OperationTransitioned,
            json!({"event":"SUBMISSION_RECORDED","from":"READY","to":"SUBMITTED","facts_version":op.facts_version,"original_reference":request.business_operation_ref})).await?;
        let receipt = PublicSubmissionReceipt {
            operation: op,
            recorded_at,
        };
        self.submission = Some((request.clone(), receipt.clone()));
        self.session_guard()?;
        Ok(receipt)
    }

    /// Borrow only AFTER consume succeeds. Trusted SQL may compose domain changes
    /// in this same transaction. No transaction-control/schema SQL is allowed,
    /// except the closed billing composer owns the reviewed member's sole
    /// billing_feedback_stage_v1 SAVEPOINT. It never permits BEGIN/COMMIT or
    /// public nested callbacks; cancellation/error discards the WHOLE outer tx.
    pub fn connection(&mut self) -> Result<&mut PgConnection, StorageError> {
        if self.consumed.is_none() {
            return Err(StorageError::TransactionAborted);
        }
        Ok(&mut *self.tx.as_mut().ok_or(StorageError::TransactionAborted)?)
    }
    pub async fn consume(
        &mut self,
        id: &PermitId,
        expected: &PermitBinding,
    ) -> Result<(), StorageError> {
        let result = self.consume_inner(id, expected).await;
        if result.is_err() {
            self.rollback().await;
        }
        result
    }
    async fn consume_inner(
        &mut self,
        id: &PermitId,
        expected: &PermitBinding,
    ) -> Result<(), StorageError> {
        if self.consumed.is_some() { return Err(StorageError::TransactionAborted); }
        let mut row = self.checked_row(id, expected).await?;
        if row.execution_scope == Some(VerifiedPermitScope::IsolatedCard) {
            if self.card.is_none() { return Err(StorageError::FormalAuthUnavailable); }
            card_safety::guard_unit(self).await?;
        }
        self.session_guard()?;
        if row.execution_scope == Some(VerifiedPermitScope::Transfer) {
            // Require a full trusted public funding projection even if arguments
            // omit payer_account_id. Scope comes from the immutable permit row.
            let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
            let stored = sqlx::query(
                "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND operation_id=$2",
            )
            .bind(self.identity.user_id.as_str())
            .bind(row.binding.snapshot.operation_id.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::DependencyUnavailable)?;
            let public = crate::execution_request::stored_row(&stored)?;
            if public.request.binding != row.binding || public.request.payload != row.payload {
                return Err(StorageError::PermitBindingMismatch);
            }
            let request = crate::transfer_result::bound_request(&public.request)?;
            let snapshot = crate::daily_usage::read_locked(tx, &self.identity.user_id).await?;
            if !snapshot.payer_accounts.contains(&request.payer_account_id) {
                return Err(StorageError::NotFound);
            }
            self.daily_usage = Some(snapshot);
        } else if crate::daily_usage::funding_shape(&row.payload.normalized_arguments) {
            // Legacy/unscoped funding must be re-verified by the future issuer,
            // never downgraded to a common permit on a generic backend path.
            return Err(StorageError::DependencyUnavailable);
        }
        self.session_guard()?;
        let now = self.now()?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let result = sqlx::query("UPDATE execution_permits SET status='CONSUMED',consumed_at_ms=$1 WHERE permit_id=$2 AND task_id=$3 AND owner_id=$4 AND status='ACTIVE' AND issued_at_ms<=$1 AND expires_at_ms>$1 AND operation_fingerprint=$5 AND facts_version=$6 AND EXISTS (SELECT 1 FROM operations o WHERE o.task_id=execution_permits.task_id AND o.operation_id=execution_permits.operation_id AND o.state='READY' AND o.revision=execution_permits.operation_revision)")
            .bind(now).bind(id.as_str()).bind(self.task.task_id.as_str()).bind(self.identity.user_id.as_str())
            .bind(row.binding.fingerprint.as_str()).bind(row.binding.snapshot.facts_version as i64).execute(&mut **tx).await?;
        if result.rows_affected() != 1 {
            return Err(StorageError::PermitNotActive);
        }
        let ops = operations(tx, &self.task.task_id).await?;
        audit(
            tx,
            &self.task,
            ops.iter()
                .find(|op| op.operation_id == row.binding.snapshot.operation_id),
            PersistentAuditKind::PermitConsumed,
            json!({"permit_id":row.id,"operation_fingerprint":row.binding.fingerprint}),
        )
        .await?;
        self.session_guard()?;
        row.active_time(self.now()?)?;
        row.consumed_at = Some(now);
        self.consumed = Some(row);
        Ok(())
    }
    fn session_digest(&self) -> Result<String, StorageError> {
        Ok(
            hash(&("safeflow-confirmation-session:v1", self.session.as_str()))?
                .as_str()
                .to_owned(),
        )
    }
    fn session_guard(&self) -> Result<(), StorageError> {
        if self.store.identity(&self.session)? != self.identity {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    fn now(&self) -> Result<i64, StorageError> {
        let now = timestamp(self.store.clock.as_ref())?.timestamp_millis();
        if now < 0 {
            return Err(StorageError::Invalid("permit server time is negative"));
        }
        if self.last_time.get().is_some_and(|previous| now < previous) {
            return Err(StorageError::Invalid("permit clock moved backwards"));
        }
        self.last_time.set(Some(now));
        Ok(now)
    }
    async fn scope_guard(&mut self) -> Result<(), StorageError> {
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let scope: bool = sqlx::query_scalar("SELECT current_schema()::text=$1 AND current_schemas(false)::text[]=ARRAY[$1] AND txid_current()=$2")
            .bind(&self.store.repository.namespace).bind(self.transaction_id).fetch_one(&mut **tx).await?;
        if !scope {
            return Err(StorageError::TransactionScope);
        }
        Ok(())
    }
    async fn row(&mut self, id: &PermitId) -> Result<PermitRow, StorageError> {
        valid_id(id.as_str())?;
        self.scope_guard().await?;
        let row = sqlx::query(
            "SELECT * FROM execution_permits WHERE task_id=$1 AND permit_id=$2 FOR UPDATE",
        )
        .bind(self.task.task_id.as_str())
        .bind(id.as_str())
        .fetch_optional(&mut **self.tx.as_mut().ok_or(StorageError::TransactionAborted)?)
        .await?
        .ok_or(StorageError::NotFound)?;
        let row = permit_row(&row)?;
        if matches!(row.authentication,StoredPermitAuthentication::Current { authentication:VerifiedPermitAuthentication::IsolatedPayment {..},..}) && self.payment.as_ref().is_none_or(|p|p.background.is_none()) {return Err(StorageError::FormalAuthUnavailable);}
        if row.execution_scope == Some(VerifiedPermitScope::IsolatedCard) && self.card.is_none() {
            return Err(StorageError::FormalAuthUnavailable);
        }
        if row.binding.snapshot.user_id != self.identity.user_id
            || row.session_digest != self.session_digest()?
            || row.session_version != self.identity.binding_version
        {
            return Err(StorageError::NotFound);
        }
        Ok(row)
    }
    async fn current(
        &mut self,
        binding: &PermitBinding,
        confirmation: &ConfirmationRef,
    ) -> Result<i64, StorageError> {
        if self.payment.is_some() {
            return self.payment_current(binding, confirmation).await;
        }
        self.scope_guard().await?;
        let digest = self.session_digest()?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let task = lock_task(tx, &self.identity.user_id, &self.task.task_id).await?;
        let ops = operations(tx, &self.task.task_id).await?;
        let current = current_binding(&task, &ops, &binding.snapshot.operation_id)?;
        facts_u32(current.facts_version)?;
        if task.state != TaskState::Running
            || !ops.iter().any(|op| {
                op.operation_id == current.operation_id && op.state == OperationState::Ready
            })
        {
            return Err(StorageError::IllegalTransition);
        }
        if current != binding.snapshot {
            return Err(StorageError::PermitBindingMismatch);
        }
        let expires = verify_stored_confirmation(
            tx,
            confirmation,
            &current,
            &digest,
            self.identity.binding_version,
            self.store.clock.as_ref(),
        )
        .await?;
        self.session_guard()?;
        Ok(expires.timestamp_millis())
    }
    async fn checked_row(
        &mut self,
        id: &PermitId,
        expected: &PermitBinding,
    ) -> Result<PermitRow, StorageError> {
        let row = self.row(id).await?;
        row.active_time(self.now()?)?;
        if &row.binding != expected {
            return Err(StorageError::PermitBindingMismatch);
        }
        self.current(&row.binding, &row.confirmation_ref).await?;
        self.session_guard()?;
        self.final_active = Some(row.clone());
        Ok(row)
    }
    async fn save(&mut self, verified: &VerifiedPermitSnapshot) -> Result<PermitRow, StorageError> {
        if verified.session_digest != self.session_digest()?
            || verified.session_version != self.identity.binding_version
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        let fingerprint = compute_operation_fingerprint(&verified.payload)
            .map_err(|_| StorageError::PermitBindingMismatch)?;
        check_payload(&verified.payload, &verified.binding)?;
        valid_id(verified.decision_id.as_str())?;
        if (verified.execution_scope == VerifiedPermitScope::IsolatedCard) != matches!(verified.authentication, VerifiedPermitAuthentication::IsolatedCard { .. }) { return Err(StorageError::PermitBindingMismatch); }
        if matches!(verified.authentication,VerifiedPermitAuthentication::IsolatedPayment {..}) && (verified.execution_scope!=VerifiedPermitScope::Transfer || self.payment.as_ref().is_none_or(|p|p.background.is_none())) {return Err(StorageError::FormalAuthUnavailable);}
        let authentication = StoredPermitAuthentication::verified(
            &verified.required_gates,
            &verified.authentication,
        )?;
        let binding = PermitBinding {
            snapshot: verified.binding.clone(),
            fingerprint,
        };
        let confirmation_expiry = self.current(&binding, &verified.confirmation_ref).await?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let card_attempt:i64=if self.card.is_some() {
            sqlx::query_scalar("SELECT COALESCE(MAX(attempt_no),0) FROM public_card_attempts_v1 WHERE operation_id=$1 AND state='ROLLBACK_VERIFIED'").bind(binding.snapshot.operation_id.as_str()).fetch_one(&mut **tx).await?
        } else {0};
        let mut lookup=sqlx::query(if self.card.is_some() {
            "SELECT * FROM execution_permits WHERE operation_id=$1 AND facts_version=$2 AND card_attempt_no=$3 FOR UPDATE"
        } else {"SELECT * FROM execution_permits WHERE operation_id=$1 AND facts_version=$2 FOR UPDATE"})
        .bind(binding.snapshot.operation_id.as_str()).bind(binding.snapshot.facts_version as i64);
        if self.card.is_some(){lookup=lookup.bind(card_attempt);}
        let existing=lookup.fetch_optional(&mut **tx).await?;
        if let Some(existing) = existing {
            let existing = permit_row(&existing)?;
            if existing.binding != binding
                || existing.payload != verified.payload
                || existing.decision_id != verified.decision_id
                || existing.confirmation_ref != verified.confirmation_ref
                || existing.authentication != authentication
                || existing.execution_scope != Some(verified.execution_scope)
                || existing.session_digest != verified.session_digest
                || existing.session_version != verified.session_version
            {
                return Err(StorageError::PermitConflict);
            }
            existing.active_time(self.now()?)?;
            self.final_active = Some(existing.clone());
            return Ok(existing);
        }
        let issued = self.now()?;
        let expires = issued
            .checked_add(self.store.config.ttl_ms)
            .ok_or(StorageError::Invalid("permit expiry overflow"))?
            .min(confirmation_expiry);
        if expires <= issued {
            return Err(StorageError::PermitExpired);
        }
        let row = PermitRow {
            id: PermitId::parse(format!("permit_{}", uuid::Uuid::new_v4())).expect("UUID"),
            binding,
            payload: verified.payload.clone(),
            decision_id: verified.decision_id.clone(),
            confirmation_ref: verified.confirmation_ref.clone(),
            authentication,
            execution_scope: Some(verified.execution_scope),
            session_digest: verified.session_digest.clone(),
            session_version: verified.session_version,
            issued,
            expires,
            status: ExecutionPermitStatus::Active,
            consumed_at: None,
        };
        let b = &row.binding.snapshot;
        self.session_guard()?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let mut insert=sqlx::query(if self.card.is_some() {"INSERT INTO execution_permits (permit_id,contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,operation_fingerprint,fingerprint_payload,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,issued_at_ms,expires_at_ms,status,storage_version,authentication_state,required_gates,execution_scope,lifecycle_anchor,card_attempt_no) VALUES ($1,1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,'ACTIVE',$22,$23,$24,$25,$26,$27)"} else {"INSERT INTO execution_permits (permit_id,contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,operation_fingerprint,fingerprint_payload,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,issued_at_ms,expires_at_ms,status,storage_version,authentication_state,required_gates,execution_scope,lifecycle_anchor) VALUES ($1,1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,'ACTIVE',$22,$23,$24,$25,$26)"})
            .bind(row.id.as_str()).bind(row.decision_id.as_str()).bind(b.user_id.as_str()).bind(b.task_id.as_str())
            .bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str()).bind(db_version(b.task_revision)?)
            .bind(b.plan_version as i64).bind(db_version(b.operation_revision)?).bind(b.facts_version as i64).bind(b.arguments_hash.as_str())
            .bind(hash(b)?.as_str().to_owned()).bind(row.binding.fingerprint.as_str()).bind(Json(&row.payload)).bind(&row.session_digest)
            .bind(db_version(row.session_version)?).bind(row.confirmation_ref.as_str()).bind(row.authentication.grant_ref().map(AuthGrantRef::as_str)).bind(row.issued).bind(row.expires)
            .bind(row.authentication.storage_version()).bind(row.authentication.state()).bind(row.authentication.gates().map(Json))
            .bind(verified.execution_scope.stored())
            .bind(b.lifecycle_anchor.as_ref().map(|a|a.as_str()))
            ;
        if self.card.is_some(){insert=insert.bind(card_attempt);}
        insert.execute(&mut **tx).await?;
        let ops = operations(tx, &self.task.task_id).await?;
        audit(tx,&self.task,ops.iter().find(|op|op.operation_id==b.operation_id),PersistentAuditKind::PermitSaved,
            json!({"permit_id":row.id,"decision_id":row.decision_id,"operation_fingerprint":row.binding.fingerprint})).await?;
        self.final_active = Some(row.clone());
        Ok(row)
    }
    async fn revoke(&mut self, id: &PermitId) -> Result<(), StorageError> {
        let row = self.row(id).await?;
        match row.status {
            ExecutionPermitStatus::Revoked => return Ok(()),
            ExecutionPermitStatus::Active => {}
            _ => return Err(StorageError::PermitNotActive),
        }
        if self.now()? < row.issued {
            return Err(StorageError::Invalid("permit clock moved backwards"));
        }
        self.session_guard()?;
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let result = sqlx::query(
            "UPDATE execution_permits SET status='REVOKED' WHERE permit_id=$1 AND status='ACTIVE'",
        )
        .bind(id.as_str())
        .execute(&mut **tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StorageError::PermitNotActive);
        }
        let ops = operations(tx, &self.task.task_id).await?;
        audit(
            tx,
            &self.task,
            ops.iter()
                .find(|op| op.operation_id == row.binding.snapshot.operation_id),
            PersistentAuditKind::PermitRevoked,
            json!({"permit_id":id}),
        )
        .await?;
        Ok(())
    }
    async fn rollback(&mut self) {
        if let Some(tx) = self.tx.take() {
            let confirmed = tx.rollback().await.is_ok();
            if confirmed && !self.commit_started {
                card_safety::final_transaction::seal_unit_rollback(self).await;
                if let Some(admission) = self.recovery.take() {
                    let proof =
                        billing::recovery::VerifiedRollback::new(admission, self.transaction_id);
                    let _ = billing::recovery::seal_rollback(
                        &self.store,
                        &self.session,
                        &self.identity,
                        &self.task.task_id,
                        proof,
                    )
                    .await;
                }
            }
        }
    }
    async fn finish(mut self, require_consumed: bool) -> Result<(), StorageError> {
        if self.recovery.is_some() {
            if let Err(error) = billing::recovery::stage_saved(&mut self).await {
                self.rollback().await;
                return Err(error);
            }
        }
        let result = self.final_guard(require_consumed).await;
        if let Err(error) = result {
            self.rollback().await;
            return Err(error);
        }
        if self.recovery.is_some() {
            if let Err(error) = billing::recovery::final_scope(&mut self).await {
                self.rollback().await;
                return Err(error);
            }
        }
        if self.billing_result.is_some() {
            let checks = async {
                sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
                    .execute(&mut **self.tx.as_mut().ok_or(StorageError::TransactionAborted)?)
                    .await?;
                billing::final_scope(&mut self).await
            }
            .await;
            if let Err(error) = checks {
                self.rollback().await;
                return Err(error);
            }
        }
        if self.subscription_result.is_some() {
            let checks=async { sqlx::query("SET CONSTRAINTS ALL IMMEDIATE").execute(&mut **self.tx.as_mut().ok_or(StorageError::TransactionAborted)?).await?;subscription::final_scope(&mut self).await }.await;
            if let Err(e)=checks { self.rollback().await;return Err(e); }
        }
        // Step 05 has no final card completion verifier. Even trusted generic
        // callbacks cannot commit a bare card consume or label it frozen.
        if require_consumed && self.card.is_some() {
            let checked=card_safety::final_transaction::finish_checks(&mut self).await;
            if let Err(e)=checked {self.rollback().await;return Err(e);}
        }
        let billing_commit = self.billing_result.is_some() || self.subscription_result.is_some() || self.card.as_ref().is_some_and(|p|p.completion.is_some());
        self.commit_started = true;
        self.tx
            .take()
            .ok_or(StorageError::TransactionAborted)?
            .commit()
            .await
            .map_err(|error| {
                if billing_commit {
                    StorageError::ReconcileRequired
                } else {
                    error.into()
                }
            })?;
        Ok(())
    }
    async fn final_snapshot(&mut self, consumed: &PermitRow) -> Result<(), StorageError> {
        if self.card.as_ref().is_some_and(|p|p.completion.is_some()) {
            return card_safety::final_transaction::verify_completion(self, consumed).await;
        }
        if self.payment.is_some() {
            return self.payment_final_snapshot(consumed).await;
        }
        if self.billing_result.is_some() {
            return billing::verify_completion(self, consumed).await;
        }
        if self.subscription_result.is_some() {
            return subscription::verify_completion(self, consumed).await;
        }
        let tx = self.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        // Row locks exclude other transactions, but not changes made by our callback.
        let current = lock_task(tx, &self.identity.user_id, &self.task.task_id).await?;
        let ops = operations(tx, &self.task.task_id).await?;
        validate_plan(&current.plan)?;
        validate_checkpoint(&current, &ops)?;
        if current.plan != self.task.plan
            || current.facts_version != self.task.facts_version
            || current.completed_nodes != self.task.completed_nodes
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        let b = &consumed.binding.snapshot;
        let op = ops
            .iter()
            .find(|op| op.operation_id == b.operation_id)
            .ok_or(StorageError::PermitBindingMismatch)?;
        if op.task_id != b.task_id
            || op.node_id != b.node_id
            || op.plan_version != b.plan_version
            || op.facts_version != b.facts_version
            || op.arguments_hash != b.arguments_hash
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        let unchanged = current == self.task
            && op.state == OperationState::Ready
            && op.revision == b.operation_revision
            && op.business_operation_ref.is_none();
        // Only the existing SubmissionRecorded shape is allowed to change progress.
        // Other domain transaction/result shapes require a separately agreed adapter.
        let submitted = op.state == OperationState::Submitted
            && op.business_operation_ref.is_some()
            && Some(op.revision) == b.operation_revision.checked_add(1)
            && Some(current.revision) == self.task.revision.checked_add(1)
            && current.state == TaskState::WaitingExternal
            && current.checkpoint
                == (RecoveryPoint::ReconcileOperation {
                    operation_id: b.operation_id.clone(),
                });
        let verified_transfer = self.transfer_result.as_ref().is_some_and(|expected| {
            expected == op
                && Some(op.revision) == b.operation_revision.checked_add(2)
                && Some(current.revision) == self.task.revision.checked_add(2)
                && current.state == TaskState::WaitingExternal
                && current.checkpoint
                    == (RecoveryPoint::ReconcileOperation {
                        operation_id: b.operation_id.clone(),
                    })
        });
        if !unchanged && !submitted && !verified_transfer {
            return Err(StorageError::PermitBindingMismatch);
        }
        if let Some((request, receipt)) = &self.submission {
            let stored = crate::execution_request::request_locked(tx, request).await?;
            if (!submitted && !verified_transfer)
                || stored.first_submission.as_ref() != Some(receipt)
                || (submitted && &receipt.operation != op)
            {
                return Err(StorageError::PermitBindingMismatch);
            }
            if verified_transfer {
                let record = crate::transfer_result::verified_record(tx, request).await?;
                let expected = match record.latest_receipt.status {
                    safeflow_contracts::ToolResultStatus::Succeeded => OperationState::Succeeded,
                    safeflow_contracts::ToolResultStatus::Failed => OperationState::Failed,
                    safeflow_contracts::ToolResultStatus::Processing => OperationState::Processing,
                };
                if op.state != expected {
                    return Err(StorageError::PermitBindingMismatch);
                }
            }
        }
        Ok(())
    }
    async fn final_guard(&mut self, require_consumed: bool) -> Result<(), StorageError> {
        if self.card.is_some() || self.payment.is_some() {
            return card_safety::guard_final_unit(self, require_consumed).await;
        }
        Box::pin(self.final_guard_checked(require_consumed)).await
    }
    async fn final_guard_checked(&mut self, require_consumed: bool) -> Result<(), StorageError> {
        if !require_consumed && self.payment.is_some() && self.consumed.is_none() {
            return self.payment_resolution_guard().await;
        }
        self.scope_guard().await?;
        let member_deadline = subscription::deadline_for_permit(self).await?;
        self.session_guard()?;
        if require_consumed && self.consumed.is_none() {
            return Err(StorageError::TransactionAborted);
        }
        if let Some(consumed) = self.consumed.clone() {
            consumed.active_time(self.now()?)?;
            let mut current = self.row(&consumed.id).await?;
            if current.status != ExecutionPermitStatus::Consumed {
                return Err(StorageError::PermitBindingMismatch);
            }
            current.status = ExecutionPermitStatus::Active;
            if current != consumed {
                return Err(StorageError::PermitBindingMismatch);
            }
            self.final_snapshot(&consumed).await?;
            // Include the new submission and reject orphan/partial SQL even when
            // a callback swallowed a domain error. The same owner lock is held.
            if consumed.execution_scope == Some(VerifiedPermitScope::Transfer) {
                let final_usage = crate::daily_usage::read_locked(
                    self.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
                    &self.identity.user_id,
                )
                .await?;
                if self.daily_usage.as_ref().map(|s| &s.usage.statistical_day)
                    != Some(&final_usage.usage.statistical_day)
                {
                    return Err(StorageError::FactsConflict);
                }
            }
            let digest = self.session_digest()?;
            let confirmation_binding = self
                .payment
                .as_ref()
                .map(|p| &p.original_binding)
                .unwrap_or(&consumed.binding.snapshot);
            verify_stored_confirmation(
                self.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
                &consumed.confirmation_ref,
                confirmation_binding,
                &digest,
                self.identity.binding_version,
                self.store.clock.as_ref(),
            )
            .await?;
        } else if let Some(active) = self.final_active.clone() {
            active.active_time(self.now()?)?;
            let current = self.checked_row(&active.id, &active.binding).await?;
            if current != active {
                return Err(StorageError::PermitBindingMismatch);
            }
        }
        self.session_guard()?;
        if let Some(row) = self.consumed.as_ref().or(self.final_active.as_ref()) {
            row.active_time(self.now()?)?;
        }
        if self.card.is_some() { card_safety::guard_unit(self).await?; }
        if self.payment.as_ref().is_some_and(|p|p.background.is_some()) { payment::background::guard_payment_consent(self).await?; }
        if let Some(deadline) = member_deadline {
            deadline.check(self.now()?)?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq)]
struct PermitRow {
    id: PermitId,
    binding: PermitBinding,
    payload: FingerprintPayloadV1,
    decision_id: DecisionId,
    confirmation_ref: ConfirmationRef,
    authentication: StoredPermitAuthentication,
    execution_scope: Option<VerifiedPermitScope>,
    session_digest: String,
    session_version: u64,
    issued: i64,
    expires: i64,
    status: ExecutionPermitStatus,
    consumed_at: Option<i64>,
}
impl PermitRow {
    fn active_time(&self, now: i64) -> Result<(), StorageError> {
        if self.status != ExecutionPermitStatus::Active {
            return Err(StorageError::PermitNotActive);
        }
        if now < self.issued {
            return Err(StorageError::Invalid("permit clock moved backwards"));
        }
        if now >= self.expires {
            return Err(StorageError::PermitExpired);
        }
        Ok(())
    }
    fn active_contract(self) -> Result<StoredExecutionPermit, StorageError> {
        if self.status != ExecutionPermitStatus::Active {
            return Err(StorageError::PermitNotActive);
        }
        // Rehydrate the frozen ACTIVE representation, not evidence for issuing authority.
        let permit = ExecutionPermit::issue(PermitIssuanceRequest {
            permit_id: self.id,
            operation_id: self.binding.snapshot.operation_id.clone(),
            decision_id: self.decision_id,
            user_id: self.binding.snapshot.user_id.clone(),
            tool_name: self.binding.snapshot.tool_name.clone(),
            fingerprint: self.binding.fingerprint.clone(),
            facts_version: facts_u32(self.binding.snapshot.facts_version)?,
            issued_at_ms: self.issued,
            expires_at_ms: self.expires,
            security_outcome: SecurityOutcome::ReadyToExecute,
        })
        .map_err(|_| StorageError::CorruptRecord)?;
        Ok(StoredExecutionPermit {
            permit,
            binding: self.binding,
        })
    }
}
fn facts_u32(version: u64) -> Result<u32, StorageError> {
    let value = u32::try_from(version).map_err(|_| StorageError::PermitVersionOverflow)?;
    if value == 0 {
        return Err(StorageError::PermitVersionOverflow);
    }
    Ok(value)
}
fn check_payload(
    payload: &FingerprintPayloadV1,
    binding: &ConfirmationBinding,
) -> Result<(), StorageError> {
    valid_id(payload.resource_id.as_str())?;
    if payload.user_id != binding.user_id
        || payload.operation_id != binding.operation_id
        || payload.tool_name != binding.tool_name
        || payload.plan_version != binding.plan_version
        || hash(&payload.normalized_arguments)? != binding.arguments_hash
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    facts_u32(binding.facts_version)?;
    Ok(())
}
fn permit_row(row: &PgRow) -> Result<PermitRow, StorageError> {
    if row.try_get::<i16, _>("contract_version")? != EXECUTION_PERMIT_CONTRACT_VERSION as i16 {
        return Err(StorageError::CorruptRecord);
    }
    let mut snapshot = legacy_binding_row(row)?;
    snapshot.lifecycle_anchor = row
        .try_get::<Option<String>, _>("lifecycle_anchor")
        .unwrap_or(None)
        .map(safeflow_contracts::ArgumentsHash::parse)
        .transpose()
        .map_err(|_| StorageError::CorruptRecord)?;
    facts_u32(snapshot.facts_version)?;
    if hash(&snapshot)?.as_str() != row.try_get::<String, _>("binding_digest")? {
        return Err(StorageError::CorruptRecord);
    }
    let payload = row
        .try_get::<Json<FingerprintPayloadV1>, _>("fingerprint_payload")?
        .0;
    check_payload(&payload, &snapshot)?;
    let fingerprint: OperationFingerprint = decode(row, "operation_fingerprint")?;
    if compute_operation_fingerprint(&payload).map_err(|_| StorageError::CorruptRecord)?
        != fingerprint
    {
        return Err(StorageError::CorruptRecord);
    }
    let status = match row.try_get::<String, _>("status")?.as_str() {
        "ACTIVE" => ExecutionPermitStatus::Active,
        "CONSUMED" => ExecutionPermitStatus::Consumed,
        "REVOKED" => ExecutionPermitStatus::Revoked,
        "EXPIRED" => ExecutionPermitStatus::Expired,
        _ => return Err(StorageError::CorruptRecord),
    };
    Ok(PermitRow {
        id: decode(row, "permit_id")?,
        binding: PermitBinding {
            snapshot,
            fingerprint,
        },
        payload,
        decision_id: decode(row, "decision_id")?,
        confirmation_ref: decode(row, "confirmation_ref")?,
        authentication: authentication_row(row)?,
        execution_scope: match row.try_get::<&str, _>("execution_scope")? {
            "LEGACY" => None,
            "COMMON" => Some(VerifiedPermitScope::Common),
            "TRANSFER" => Some(VerifiedPermitScope::Transfer),
            "ISOLATED_CARD" => Some(VerifiedPermitScope::IsolatedCard),
            _ => return Err(StorageError::CorruptRecord),
        },
        session_digest: row.try_get("session_digest")?,
        session_version: crate::repository::version(row, "session_binding_version")?,
        issued: row.try_get("issued_at_ms")?,
        expires: row.try_get("expires_at_ms")?,
        status,
        consumed_at: row.try_get("consumed_at_ms")?,
    })
}

fn authentication_row(row: &PgRow) -> Result<StoredPermitAuthentication, StorageError> {
    let version: i16 = row.try_get("storage_version")?;
    let state: String = row.try_get("authentication_state")?;
    let gates: Option<Json<Vec<SecurityGate>>> = row
        .try_get("required_gates")
        .map_err(|_| StorageError::CorruptRecord)?;
    let grant: Option<String> = row.try_get("auth_grant_ref")?;
    let grant = grant
        .map(AuthGrantRef::parse)
        .transpose()
        .map_err(|_| StorageError::CorruptRecord)?;
    match (version, state.as_str(), gates, grant) {
        (1, "LEGACY_REQUIRED", None, Some(grant_ref)) => {
            Ok(StoredPermitAuthentication::LegacyRequired { grant_ref })
        }
        (2, "NOT_REQUIRED", Some(gates), None) => StoredPermitAuthentication::verified(
            &gates.0,
            &VerifiedPermitAuthentication::NotRequired,
        )
        .map_err(|_| StorageError::CorruptRecord),
        (2, "VERIFIED_REQUIRED", Some(gates), Some(grant_ref)) => {
            StoredPermitAuthentication::verified(
                &gates.0,
                &VerifiedPermitAuthentication::Verified { grant_ref },
            )
            .map_err(|_| StorageError::CorruptRecord)
        }
        (3, "ISOLATED_CARD_VERIFIED", Some(gates), Some(grant_ref)) => StoredPermitAuthentication::verified(
            &gates.0, &VerifiedPermitAuthentication::IsolatedCard { grant_ref }).map_err(|_| StorageError::CorruptRecord),
        (4, "ISOLATED_PAYMENT_VERIFIED", Some(gates), Some(grant_ref)) => StoredPermitAuthentication::verified(
            &gates.0, &VerifiedPermitAuthentication::IsolatedPayment { grant_ref }).map_err(|_| StorageError::CorruptRecord),
        _ => Err(StorageError::CorruptRecord),
    }
}

#[cfg(test)]
#[path = "permit_tests.rs"]
mod tests;
