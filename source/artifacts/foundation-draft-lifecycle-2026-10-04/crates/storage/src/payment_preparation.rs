//! Persistent user intent and public minimum-level review; no funds Tool or Permit.
mod agent;
mod execution;
mod link;
mod preflight;
use crate::confirmation::ConfirmationSession;
use crate::confirmation_repository::{
    check_active_at, confirmation_details, insert_prepared_confirmation, read_stored_confirmation,
};
use crate::models::{db_version, hash, valid_id, validate_plan};
use crate::repository::{audit, decode, encode, lock_task_for_mode, operation_row, operations};
use crate::*;
pub use agent::PaymentAgentBinding;
use chrono::{DateTime, Utc};
#[cfg(test)]
pub(crate) use execution::recover_locked;
pub use execution::{
    PaymentExecutionBinding, PaymentOriginalReader, PaymentOriginalResult, PaymentOriginalState,
};
pub use link::{PaymentExecutionLink, PaymentRegistrationBinding};
pub use preflight::{PaymentBusinessObservation, PaymentBusinessPreflight};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, ConfirmationRef, ExecutionContext, NodeId, OperationId,
    OperationState, PlanNode, ResourceId, SessionRef, TaskId, TaskState, ToolName,
};
use safeflow_runtime::{Runtime, SessionResolver};
use safeflow_transfer_integration::{funds_store::FundsStore, mock_execution::MockTransferRequest};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{types::Json, Postgres, Row, Transaction};
use std::sync::Arc;

pub const PAYMENT_PREPARATION_SCOPE: &str = "PAYMENT_INTENT_ONLY";
const TOOL: &str = "foundation_payment_intent";

/// All fields are comparisons with the server's current draft/task display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentPreparationRequest {
    pub selection_ticket: String,
    pub task_revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
    pub draft_id: ResourceId,
    pub draft_version: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentSnapshot {
    pub transfer: MockTransferRequest,
    pub display_details: Value,
    pub draft_expires_at: u64,
}

#[derive(Serialize)]
pub struct PaymentPreparationView {
    pub agent_binding: Option<PaymentAgentBinding>,
    pub contract_version: u16,
    pub scope: &'static str,
    pub task_id: TaskId,
    pub operation: OperationRecord,
    pub snapshot: PaymentSnapshot,
    pub snapshot_digest: ArgumentsHash,
    pub display_ticket: String,
    pub confirmation_ref: ConfirmationRef,
    pub confirmation_status: ConfirmationStatus,
    pub prepared_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub state: &'static str,
    pub invalid_reason: Option<String>,
    pub funds_execution_enabled: bool,
    pub execution_conditions: Vec<&'static str>,
    pub replayed: bool,
    pub security_decisions: Vec<StoredSecurityDecision>,
    pub decision_freshness: &'static str,
    pub business_preflight: PaymentBusinessPreflight,
    pub business_recovery_required: bool,
    pub original_result_path: String,
    pub execution_link: Option<PaymentExecutionLink>,
    pub execution_link_path: String,
}

#[derive(Clone)]
pub struct PaymentPreparationService<'a> {
    repository: PgRepository,
    confirmations: ConfirmationService,
    config: ConfirmationConfig,
    runtime: &'a Runtime,
    decision_config: DecisionConfig,
}

impl<'a> PaymentPreparationService<'a> {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: ConfirmationConfig,
        runtime: &'a Runtime,
        decision_config: DecisionConfig,
    ) -> Self {
        Self {
            confirmations: ConfirmationService::for_trusted_backend(
                repository.clone(),
                sessions,
                config.clone(),
            ),
            repository,
            config,
            runtime,
            decision_config,
        }
    }

    async fn begin(
        &self,
        reference: &SessionRef,
        id: &TaskId,
    ) -> Result<
        (
            ConfirmationSession,
            Transaction<'static, Postgres>,
            TaskRecord,
        ),
        StorageError,
    > {
        self.begin_mode(reference, id, false).await
    }

    async fn begin_mode(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        consistent: bool,
    ) -> Result<
        (
            ConfirmationSession,
            Transaction<'static, Postgres>,
            TaskRecord,
        ),
        StorageError,
    > {
        valid_id(id.as_str())?;
        let session = self.confirmations.session(reference)?;
        let mut tx = self.repository.pool.begin().await?;
        if consistent {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        crate::daily_usage::protect_owner(&mut tx, &session.binding.user_id).await?;
        let task = lock_task_for_mode(&mut tx, &session.binding.user_id, id, true).await?;
        // A REPEATABLE READ snapshot may predate a close that just released
        // this task lock (close deliberately leaves task/claim unchanged).
        // Use a current independent snapshot WHILE holding the common lock.
        let closed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND details->>'schema' IN('NONFUNDING_UNKNOWN_DRAFT_CLOSED_V1','UNEXECUTED_DRAFT_TERMINATED_V1'))")
            .bind(id.as_str()).fetch_one(&self.repository.subscription_read_pool).await?;
        if closed {return Err(StorageError::ExecutionOperationConflict);}
        self.confirmations.guard(reference, &session)?;
        // Keep the existing strict lifecycle history and claim verification.
        let ops = operations(&mut tx, id).await?;
        crate::lifecycle::project_progress(&mut tx, &task, &ops).await?;
        Ok((session, tx, task))
    }

    async fn begin_read(
        &self,
        reference: &SessionRef,
        id: &TaskId,
    ) -> Result<
        (
            ConfirmationSession,
            Transaction<'static, Postgres>,
            TaskRecord,
        ),
        StorageError,
    > {
        valid_id(id.as_str())?;
        let session = self.confirmations.session(reference)?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT * FROM tasks WHERE owner_id=$1 AND task_id=$2")
            .bind(session.binding.user_id.as_str())
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StorageError::NotFound)?;
        let task = crate::repository::task_row(&row)?;
        let ops = operations(&mut tx, id).await?;
        crate::lifecycle::project_progress(&mut tx, &task, &ops).await?;
        self.confirmations.guard(reference, &session)?;
        Ok((session, tx, task))
    }

    pub async fn prepare(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        input: PaymentPreparationRequest,
    ) -> Result<PaymentPreparationView, StorageError> {
        for attempt in 0..4 {
            match self.prepare_once(reference, id, input.clone(), None).await {
                Err(StorageError::RevisionConflict) if attempt < 3 => continue,
                result => return result,
            }
        }
        unreachable!()
    }

    /// The reviewed locator is resolved only inside the existing preparation transaction.
    pub async fn prepare_for_agent(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        input: PaymentPreparationRequest,
        review: &safeflow_runtime::PlanningPersistenceReview<'_>,
        contracts: Vec<safeflow_runtime::PreflightInputContract>,
    ) -> Result<PaymentPreparationView, StorageError> {
        let intake = safeflow_runtime::PlanningIntakeService::for_registered_backend(
            self.runtime,
            None,
            contracts,
        )
        .map_err(|_| StorageError::PlanConflict)?;
        for attempt in 0..4 {
            intake
                .check_persistence_review(review)
                .map_err(|_| StorageError::PlanConflict)?;
            if review.session_ref() != reference {
                return Err(StorageError::Session);
            }
            match self
                .prepare_once(reference, id, input.clone(), Some(review))
                .await
            {
                Err(StorageError::RevisionConflict) if attempt < 3 => continue,
                result => return result,
            }
        }
        unreachable!()
    }

    async fn prepare_once(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        input: PaymentPreparationRequest,
        review: Option<&safeflow_runtime::PlanningPersistenceReview<'_>>,
    ) -> Result<PaymentPreparationView, StorageError> {
        valid_id(&input.selection_ticket)?;
        valid_id(input.draft_id.as_str())?;
        db_version(input.task_revision)?;
        db_version(input.facts_version)?;
        if input.plan_version == 0 || input.draft_version == 0 {
            return Err(StorageError::Invalid("zero payment version"));
        }
        let (session, mut tx, task) = self.begin_mode(reference, id, true).await?;
        if let Some(review) = review {
            if review.owner() != &task.owner_id || review.session_binding() != &session.binding {
                return Err(StorageError::Session);
            }
        }
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT operation_id FROM payment_preparations WHERE task_id=$1 AND request_ticket=$2",
        )
        .bind(id.as_str())
        .bind(&input.selection_ticket)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(operation) = existing {
            let old: Json<PaymentPreparationRequest> = sqlx::query_scalar(
                "SELECT request FROM payment_preparations WHERE operation_id=$1",
            )
            .bind(&operation)
            .fetch_one(&mut *tx)
            .await?;
            if old.0 != input {
                return Err(StorageError::ExecutionKeyConflict);
            }
            let op = OperationId::parse(operation).map_err(|_| StorageError::CorruptRecord)?;
            if let Some(review) = review {
                agent::bind(&mut tx, &task, &op, review).await?;
            }
            let view = self
                .read_locked(
                    &mut tx, &task, &session, reference, &op, None, true, false, false,
                )
                .await?;
            self.confirmations.guard(reference, &session)?;
            tx.commit().await?;
            return Ok(view);
        }
        let ticket: String = sqlx::query_scalar(
            "SELECT selection_ticket FROM mock_draft_lifecycle_tasks WHERE task_id=$1",
        )
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if task.revision != input.task_revision
            || task.plan.plan_version != input.plan_version
            || task.facts_version != input.facts_version
            || ticket != input.selection_ticket
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let snapshot =
            current_snapshot(&mut tx, &task, &input.draft_id, input.draft_version).await?;
        let op_id = OperationId::parse(format!("payment_{}", uuid::Uuid::new_v4())).unwrap();
        let node_id = NodeId::parse(format!("payment_{}", uuid::Uuid::new_v4())).unwrap();
        let digest = hash(&snapshot)?;
        let operation = OperationRecord {
            operation_id: op_id.clone(),
            task_id: id.clone(),
            node_id: node_id.clone(),
            state: OperationState::Draft,
            revision: 1,
            plan_version: task.plan.plan_version,
            facts_version: task.facts_version,
            arguments_hash: digest.clone(),
            business_operation_ref: None,
        };
        let plan = preparation_plan(&task, &operation, &snapshot)?;
        let base_digest = hash(&task.plan)?;
        let now = server_time(&mut tx).await?;
        let expiry = DateTime::from_timestamp(
            i64::try_from(snapshot.draft_expires_at).map_err(|_| StorageError::CorruptRecord)?,
            0,
        )
        .ok_or(StorageError::CorruptRecord)?;
        let expires_at = now
            .checked_add_signed(self.config.ttl)
            .ok_or(StorageError::Invalid("payment expiry"))?
            .min(expiry);
        let record = ConfirmationRecord {
            contract_version: SERVER_CONFIRMATION_CONTRACT_VERSION,
            confirmation_ref: ConfirmationRef::parse(format!(
                "confirmation_{}",
                uuid::Uuid::new_v4()
            ))
            .unwrap(),
            binding: ConfirmationBinding {
                user_id: task.owner_id.clone(),
                task_id: id.clone(),
                operation_id: op_id.clone(),
                node_id,
                tool_name: ToolName::parse(TOOL).unwrap(),
                task_revision: task.revision,
                plan_version: operation.plan_version,
                operation_revision: 1,
                facts_version: task.facts_version,
                arguments_hash: digest.clone(),
                lifecycle_anchor: Some(hash(&(base_digest.clone(), &plan))?),
            },
            status: ConfirmationStatus::Prepared,
            prepared_at: now,
            expires_at,
            confirmed_at: None,
            revoked_at: None,
        };
        sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'DRAFT',1,$4,$5,$6)")
            .bind(op_id.as_str()).bind(id.as_str()).bind(operation.node_id.as_str())
            .bind(operation.plan_version as i64).bind(db_version(operation.facts_version)?).bind(digest.as_str())
            .execute(&mut *tx).await?;
        insert_prepared_confirmation(&mut tx, &session, &record).await?;
        let display = format!("payment_display_{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO payment_preparations(operation_id,task_id,request,request_ticket,task_revision,draft_id,draft_version,snapshot,snapshot_digest,base_plan_digest,preparation_plan,display_ticket,confirmation_ref) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
            .bind(op_id.as_str()).bind(id.as_str()).bind(Json(&input)).bind(&input.selection_ticket)
            .bind(db_version(task.revision)?).bind(input.draft_id.as_str()).bind(input.draft_version as i64)
            .bind(Json(&snapshot)).bind(digest.as_str()).bind(base_digest.as_str()).bind(Json(&plan))
            .bind(&display).bind(record.confirmation_ref.as_str()).execute(&mut *tx).await
            .map_err(|error| {
                // A concurrent prepare may commit after this RR snapshot was taken.
                if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("23505") {
                    StorageError::RevisionConflict
                } else { error.into() }
            })?;
        audit(&mut tx, &task, Some(&operation), PersistentAuditKind::OperationCreated,
            json!({"payment_preparation":{"scope":PAYMENT_PREPARATION_SCOPE,"plan":plan,"base_plan_digest":base_digest,"snapshot_digest":digest}})).await?;
        audit(
            &mut tx,
            &task,
            Some(&operation),
            PersistentAuditKind::ConfirmationPrepared,
            confirmation_details(&record)?,
        )
        .await?;
        if let Some(review) = review {
            agent::bind(&mut tx, &task, &op_id, review).await?;
        }
        self.confirmations.guard(reference, &session)?;
        // Source expiry can pass while a database trigger or lock is waiting.
        let view = self
            .read_locked(
                &mut tx, &task, &session, reference, &op_id, None, false, false, false,
            )
            .await?;
        if view.invalid_reason.is_some() {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        self.confirmations.guard(reference, &session)?;
        tx.commit().await?;
        Ok(view)
    }

    pub async fn query(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
    ) -> Result<PaymentPreparationView, StorageError> {
        let (session, mut tx, task) = self.begin_read(reference, id).await?;
        let view = self
            .read_locked(
                &mut tx, &task, &session, reference, operation, None, false, false, true,
            )
            .await?;
        self.confirmations.guard(reference, &session)?;
        tx.commit().await?;
        Ok(view)
    }

    /// Recovery needs only the original task ID; no client copy of the payment ID.
    pub async fn query_task(
        &self,
        reference: &SessionRef,
        id: &TaskId,
    ) -> Result<Vec<PaymentPreparationView>, StorageError> {
        let (session, mut tx, task) = self.begin_read(reference, id).await?;
        let ids:Vec<String>=sqlx::query_scalar("SELECT operation_id FROM payment_preparations WHERE task_id=$1 ORDER BY task_revision,operation_id LIMIT 257")
            .bind(id.as_str()).fetch_all(&mut *tx).await?;
        if ids.len() > 256 {
            return Err(StorageError::CorruptRecord);
        }
        let mut views = Vec::new();
        for id in ids {
            let op = OperationId::parse(id).map_err(|_| StorageError::CorruptRecord)?;
            views.push(
                self.read_locked(
                    &mut tx, &task, &session, reference, &op, None, false, false, true,
                )
                .await?,
            );
        }
        self.confirmations.guard(reference, &session)?;
        tx.commit().await?;
        Ok(views)
    }

    pub async fn decide(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        display: &str,
        confirm: bool,
    ) -> Result<PaymentPreparationView, StorageError> {
        self.decide_inner(
            reference,
            id,
            operation,
            confirmation,
            display,
            confirm,
            false,
        )
        .await
    }

    /// Explicit fresh evaluation, distinct from confirmation replay or historical GET.
    pub async fn evaluate(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        display: &str,
    ) -> Result<PaymentPreparationView, StorageError> {
        self.decide_inner(reference, id, operation, confirmation, display, true, true)
            .await
    }

    async fn decide_inner(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        display: &str,
        confirm: bool,
        refresh: bool,
    ) -> Result<PaymentPreparationView, StorageError> {
        // A lock wait can leave a REPEATABLE READ snapshot behind another confirmation.
        // Retry the entire transaction, including session/source validation, on 40001.
        for attempt in 0..4 {
            match self
                .decide_once(
                    reference,
                    id,
                    operation,
                    confirmation,
                    display,
                    confirm,
                    refresh,
                )
                .await
            {
                Err(StorageError::RevisionConflict) if attempt < 3 => continue,
                result => return result,
            }
        }
        unreachable!()
    }

    async fn decide_once(
        &self,
        reference: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        display: &str,
        confirm: bool,
        refresh: bool,
    ) -> Result<PaymentPreparationView, StorageError> {
        let (session, mut tx, task) = self.begin_mode(reference, id, true).await?;
        let view = self
            .read_locked(
                &mut tx,
                &task,
                &session,
                reference,
                operation,
                Some((confirmation, display, confirm)),
                false,
                refresh,
                false,
            )
            .await?;
        self.confirmations.guard(reference, &session)?;
        tx.commit().await?;
        if view
            .invalid_reason
            .as_deref()
            .is_some_and(|r| r != "REJECTED")
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        Ok(view)
    }

    async fn read_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        task: &TaskRecord,
        session: &ConfirmationSession,
        session_ref: &SessionRef,
        operation: &OperationId,
        decision: Option<(&ConfirmationRef, &str, bool)>,
        replayed: bool,
        refresh: bool,
        readonly: bool,
    ) -> Result<PaymentPreparationView, StorageError> {
        let SavedPayment {
            op,
            snapshot,
            digest,
            base_digest,
            display,
            stored,
            mut reason,
        } = saved_payment(tx, task, operation, !readonly).await?;
        let mut record = stored.record.clone();
        let b = &record.binding;
        let reference = record.confirmation_ref.clone();
        if let Some((requested_ref, requested_display, _)) = decision {
            if requested_ref != &reference || requested_display != display {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
        }
        let now = server_time(tx).await?;
        if reason.is_none() {
            reason = if stored.check_session(session).is_err() {
                Some("SESSION_CHANGED".into())
            } else if check_active_at(&record, now).is_err() {
                Some("EXPIRED".into())
            } else if task.revision != b.task_revision
                || task.plan.plan_version != b.plan_version
                || task.facts_version != b.facts_version
                || hash(&task.plan)? != base_digest
            {
                Some("SOURCE_CHANGED".into())
            } else {
                match snapshot_for_mode(
                    tx,
                    task,
                    &snapshot.transfer.draft_id,
                    snapshot.transfer.draft_version,
                    !readonly,
                )
                .await
                {
                    Ok(current) if current == snapshot => None,
                    Ok(_)
                    | Err(
                        StorageError::ConfirmationBindingMismatch
                        | StorageError::ReconcileRequired
                        | StorageError::NotFound,
                    ) => Some("SOURCE_CHANGED".into()),
                    Err(error) => return Err(error),
                }
            };
        }
        let binding = record.binding.clone();
        let old_status = record.status;
        if refresh && record.status != ConfirmationStatus::Confirmed {
            return Err(StorageError::ConfirmationRequired);
        }
        if reason.is_none() {
            if let Some((_, _, confirm)) = decision {
                if confirm && record.status == ConfirmationStatus::Prepared {
                    record.status = ConfirmationStatus::Confirmed;
                    record.confirmed_at = Some(server_time(tx).await?);
                } else if !confirm {
                    if record.status == ConfirmationStatus::Confirmed {
                        return Err(StorageError::DecisionConflict);
                    }
                    reason = Some("REJECTED".into());
                }
            }
        } else if decision.is_some_and(|(_, _, confirm)| confirm)
            && reason.as_deref() == Some("REJECTED")
        {
            return Err(StorageError::ConfirmationRevoked);
        }
        if reason.is_some() && record.status != ConfirmationStatus::Revoked {
            record.status = ConfirmationStatus::Revoked;
            record.revoked_at = Some(server_time(tx).await?);
            if !readonly {
                sqlx::query(
                    "UPDATE payment_preparations SET invalid_reason=$1 WHERE operation_id=$2",
                )
                .bind(&reason)
                .bind(operation.as_str())
                .execute(&mut **tx)
                .await?;
            }
        }
        if record.status != old_status && !readonly {
            sqlx::query("UPDATE server_confirmations SET status=$1,confirmed_at=$2,revoked_at=$3 WHERE confirmation_ref=$4")
                .bind(encode(record.status)?).bind(record.confirmed_at).bind(record.revoked_at).bind(reference.as_str())
                .execute(&mut **tx).await?;
            audit(
                tx,
                task,
                Some(&op),
                if record.status == ConfirmationStatus::Revoked {
                    PersistentAuditKind::ConfirmationRevoked
                } else {
                    PersistentAuditKind::ExplicitConfirmationRecorded
                },
                confirmation_details(&record)?,
            )
            .await?;
        }
        // A later SQL wait must not allow a confirmation past its deadline.
        if record.status != ConfirmationStatus::Revoked {
            check_active_at(&record, server_time(tx).await?)?;
        }
        let business_recovery_required:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 AND submitted) OR EXISTS(SELECT 1 FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$3) OR EXISTS(SELECT 1 FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$3)")
            .bind(task.owner_id.as_str()).bind(snapshot.transfer.draft_id.as_str()).bind(snapshot.transfer.operation_id.as_str()).fetch_one(&mut **tx).await?;
        let (security_decisions, decision_freshness) = DecisionService::payment_locked(
            tx,
            self.runtime,
            &self.decision_config,
            task,
            &op,
            session_ref,
            session,
            &binding,
            &snapshot,
            &record,
            reason.is_none() && !business_recovery_required,
            decision.is_some_and(|(_, _, confirm)| confirm),
            refresh,
            !readonly,
        )
        .await?;
        // The preflight save/audit may wait; repeat the actual draft check before commit.
        if decision_freshness == "CURRENT_MINIMUM_ONLY"
            && snapshot_for_mode(
                tx,
                task,
                &snapshot.transfer.draft_id,
                snapshot.transfer.draft_version,
                !readonly,
            )
            .await?
                != snapshot
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let saved = saved_payment(tx, task, operation, false).await?;
        let business_preflight = preflight::observe(
            tx,
            task,
            session,
            &saved,
            &record,
            reason.is_none(),
            decision.is_some_and(|(_, _, confirm)| confirm) && !readonly,
            refresh,
            self.decision_config.ttl_ms,
        )
        .await?;
        let execution_link = link::read_link(tx, task, &saved).await?;
        let agent_binding = agent::read_binding(tx, task, &saved).await?;
        Ok(PaymentPreparationView {
            agent_binding,
            contract_version: 1,
            scope: PAYMENT_PREPARATION_SCOPE,
            task_id: task.task_id.clone(),
            operation: op,
            snapshot,
            snapshot_digest: digest,
            display_ticket: display,
            confirmation_ref: reference,
            confirmation_status: record.status,
            prepared_at: record.prepared_at,
            expires_at: record.expires_at,
            confirmed_at: record.confirmed_at,
            state: if reason.as_deref() == Some("REJECTED") {
                "REJECTED"
            } else if reason.is_some() {
                "INVALIDATED"
            } else if record.status == ConfirmationStatus::Confirmed {
                "CONFIRMED_AWAITING_EXECUTION_CONDITIONS"
            } else {
                "AWAITING_PAYMENT_CONFIRMATION"
            },
            invalid_reason: reason,
            funds_execution_enabled: false,
            execution_conditions: vec![
                "FINAL_FUNDS_AUTHORITY_NOT_ASSEMBLED",
                "FINAL_RULES_AND_AUTH_NOT_ASSEMBLED",
                "CARD_COMMIT_PROTOCOL_NOT_CONNECTED",
            ],
            replayed,
            security_decisions,
            decision_freshness: if business_recovery_required {
                "QUERY_ORIGINAL_BUSINESS_OPERATION"
            } else {
                decision_freshness
            },
            business_preflight,
            business_recovery_required,
            execution_link,
            execution_link_path: format!(
                "/api/sim/payment-preparations/{}/{}/execution-link",
                task.task_id, operation
            ),
            original_result_path: format!(
                "/api/sim/payment-preparations/{}/{}/original-result",
                task.task_id, operation
            ),
        })
    }
}

pub(crate) struct SavedPayment {
    pub(crate) op: OperationRecord,
    pub(crate) snapshot: PaymentSnapshot,
    pub(crate) digest: ArgumentsHash,
    pub(crate) base_digest: ArgumentsHash,
    pub(crate) display: String,
    pub(crate) stored: crate::confirmation_repository::StoredConfirmation,
    pub(crate) reason: Option<String>,
}

// Historical integrity is independent of current confirmation/session validity.
pub(crate) async fn saved_payment(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    operation: &OperationId,
    lock: bool,
) -> Result<SavedPayment, StorageError> {
    valid_id(operation.as_str())?;
    let query = if lock {
        "SELECT * FROM payment_preparations WHERE task_id=$1 AND operation_id=$2 FOR UPDATE"
    } else {
        "SELECT * FROM payment_preparations WHERE task_id=$1 AND operation_id=$2"
    };
    let row = sqlx::query(query)
        .bind(task.task_id.as_str())
        .bind(operation.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::NotFound)?;
    let query = if lock {
        "SELECT * FROM operations WHERE task_id=$1 AND operation_id=$2 FOR UPDATE"
    } else {
        "SELECT * FROM operations WHERE task_id=$1 AND operation_id=$2"
    };
    let op_row = sqlx::query(query)
        .bind(task.task_id.as_str())
        .bind(operation.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let op = operation_row(&op_row)?;
    let snapshot = row.try_get::<Json<PaymentSnapshot>, _>("snapshot")?.0;
    let digest: ArgumentsHash = decode(&row, "snapshot_digest")?;
    let base_digest: ArgumentsHash = decode(&row, "base_plan_digest")?;
    let plan = row.try_get::<Json<AgentPlan>, _>("preparation_plan")?.0;
    let reference: ConfirmationRef = decode(&row, "confirmation_ref")?;
    let stored = if lock {
        read_stored_confirmation(tx, &task.task_id, &reference).await?
    } else {
        crate::confirmation_repository::read_historical_confirmation(tx, &task.task_id, &reference)
            .await?
    };
    let b = &stored.record.binding;
    let input = row
        .try_get::<Json<PaymentPreparationRequest>, _>("request")?
        .0;
    let historical_digest: String = sqlx::query_scalar(
        "SELECT plan_digest FROM mock_draft_lifecycle_plans WHERE task_id=$1 AND plan_version=$2",
    )
    .bind(task.task_id.as_str())
    .bind(op.plan_version as i64)
    .fetch_one(&mut **tx)
    .await?;
    if hash(&snapshot)? != digest
        || op.arguments_hash != digest
        || op.state != OperationState::Draft
        || op.revision != 1
        || op.business_operation_ref.is_some()
        || b.user_id != task.owner_id
        || b.task_id != task.task_id
        || b.operation_id != op.operation_id
        || b.node_id != op.node_id
        || b.operation_revision != op.revision
        || b.plan_version != op.plan_version
        || b.facts_version != op.facts_version
        || b.arguments_hash != digest
        || b.tool_name.as_str() != TOOL
        || b.task_revision != input.task_revision
        || input.plan_version != op.plan_version
        || input.facts_version != op.facts_version
        || input.draft_id != snapshot.transfer.draft_id
        || input.draft_version != snapshot.transfer.draft_version
        || snapshot.transfer.task_id != task.task_id
        || historical_digest != base_digest.as_str()
        || row.try_get::<i64, _>("task_revision")? != db_version(input.task_revision)?
        || row.try_get::<String, _>("request_ticket")? != input.selection_ticket
        || row.try_get::<String, _>("draft_id")? != input.draft_id.as_str()
        || row.try_get::<i64, _>("draft_version")? != input.draft_version as i64
        || plan != preparation_plan(task, &op, &snapshot)?
        || b.lifecycle_anchor != Some(hash(&(base_digest.clone(), &plan))?)
    {
        return Err(StorageError::CorruptRecord);
    }
    let reason: Option<String> = row.try_get("invalid_reason")?;
    if (stored.record.status == ConfirmationStatus::Revoked) != reason.is_some() {
        return Err(StorageError::CorruptRecord);
    }
    Ok(SavedPayment {
        op,
        snapshot,
        digest,
        base_digest,
        display: row.try_get("display_ticket")?,
        stored,
        reason,
    })
}

fn preparation_plan(
    task: &TaskRecord,
    op: &OperationRecord,
    snapshot: &PaymentSnapshot,
) -> Result<AgentPlan, StorageError> {
    let plan = AgentPlan::new(
        task.task_id.clone(),
        op.plan_version,
        vec![
            PlanNode::Tool {
                id: op.node_id.clone(),
                tool: ToolName::parse(TOOL).unwrap(),
                arguments: serde_json::to_value(snapshot)
                    .map_err(|_| StorageError::CorruptRecord)?,
                depends_on: vec![],
            },
            PlanNode::UserDecision {
                id: NodeId::parse(format!("confirm_{}", op.node_id.as_str())).unwrap(),
                depends_on: vec![op.node_id.clone()],
            },
        ],
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    validate_plan(&plan)?;
    Ok(plan)
}

async fn server_time(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, StorageError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}

pub(crate) async fn current_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    id: &ResourceId,
    version: u32,
) -> Result<PaymentSnapshot, StorageError> {
    snapshot_for_mode(tx, task, id, version, true).await
}

async fn snapshot_for_mode(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    id: &ResourceId,
    version: u32,
    lock: bool,
) -> Result<PaymentSnapshot, StorageError> {
    if task.state != TaskState::WaitingUser {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    let draft = FundsStore::draft(tx, &task.owner_id, id)
        .await
        .map_err(crate::transfer_sql::business_error)?;
    if lock {
        crate::daily_usage::protect_operation(
            tx,
            task.owner_id.as_str(),
            draft.operation_id.as_str(),
        )
        .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(json!(["transfer-draft-v1", task.owner_id, id]).to_string())
            .execute(&mut **tx)
            .await?;
    }
    let submitted: bool = sqlx::query_scalar(if lock {
        "SELECT submitted FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 FOR UPDATE"
    } else {
        "SELECT submitted FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2"
    })
    .bind(task.owner_id.as_str())
    .bind(id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    let saved = FundsStore::draft(tx, &task.owner_id, id)
        .await
        .map_err(crate::transfer_sql::business_error)?;
    if draft.operation_id != saved.operation_id {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    let tracked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2)")
        .bind(task.owner_id.as_str()).bind(saved.operation_id.as_str()).fetch_one(&mut **tx).await?;
    let partial_business: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE owner_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM transfer_ledger_v1 WHERE business_owner_id=$1 AND operation_id=$2)")
        .bind(task.owner_id.as_str()).bind(saved.operation_id.as_str()).fetch_one(&mut **tx).await?;
    if submitted
        || tracked
        || partial_business
        || FundsStore::query(tx, &task.owner_id, &saved.operation_id)
            .await
            .map_err(crate::transfer_sql::business_error)?
            .is_some()
    {
        return Err(StorageError::ReconcileRequired);
    }
    let (active, receipt): (String, Json<LifecycleReceipt>) = sqlx::query_as(
        "SELECT t.active_operation,c.receipt FROM mock_draft_lifecycle_tasks t JOIN mock_draft_lifecycle_claims c ON c.operation_id=t.last_success WHERE t.task_id=$1 AND c.status='SUCCEEDED'",
    ).bind(task.task_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ConfirmationBindingMismatch)?;
    if receipt.0.operation_id.as_str() != active
        || receipt.0.draft_id != saved.draft_id
        || receipt.0.draft_version != saved.plan_version
        || receipt.0.business_operation_id.as_str() != saved.operation_id.as_str()
    {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    let now = server_time(tx).await?;
    let context = ExecutionContext {
        task_id: task.task_id.clone(),
        node_id: NodeId::parse("payment_source").unwrap(),
        operation_id: OperationId::parse(active).map_err(|_| StorageError::CorruptRecord)?,
        authenticated_user_id: task.owner_id.clone(),
        plan_version: task.plan.plan_version,
        idempotency_key: None,
    };
    let transfer = MockTransferRequest::from_trusted_draft(
        &context,
        &saved,
        version,
        u64::try_from(now.timestamp()).map_err(|_| StorageError::CorruptRecord)?,
    )
    .map_err(|_| StorageError::ConfirmationBindingMismatch)?;
    Ok(PaymentSnapshot {
        transfer,
        display_details: serde_json::to_value(&saved.details)
            .map_err(|_| StorageError::CorruptRecord)?,
        draft_expires_at: saved.expires_at,
    })
}
