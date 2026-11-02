//! Same-id, trusted backend scheduling of persisted, strictly validated read-only plans.
use crate::models::{db_version, hash, valid_id, validate_checkpoint};
use crate::repository::{audit, guarded, lock_task, operations, update_task};
use crate::{PersistentAuditKind, PgRepository, RecoveryPoint, StorageError, TaskRecord};
use safeflow_contracts::{
    ArgumentsHash, OperationId, PlanningResult, SessionRef, TaskEvent, TaskId, TaskState, ToolName,
    ToolResult, ToolResultRef, Understanding, UnderstandingDecision,
};
use safeflow_runtime::{
    PlanningIntakeService, PreflightInputContract, Runtime, RuntimeError, SessionBinding,
    VerifiedReadOnlyExecution, READ_ONLY_EXECUTION_CONTRACT_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{types::Json, Row};
use std::fmt;

type Guard<'a> = &'a (dyn Fn() -> Result<(), StorageError> + Send + Sync);

pub(crate) fn check_result_bodies(
    results: Option<&[ToolResult]>,
    references: &[RuntimeReadResultReference],
    expected_hash: Option<&ArgumentsHash>,
) -> Result<(), StorageError> {
    if let Some(results) = results {
        if expected_hash != Some(&hash(&results)?)
            || results.len() != references.len()
            || results.iter().zip(references).any(|(r, reference)| {
                r.contract_version != safeflow_contracts::AGENT_TOOL_CONTRACT_VERSION
                    || r.status != safeflow_contracts::ToolResultStatus::Succeeded
                    || r.tool_name != reference.tool_name
                    || r.operation_id != reference.operation_id
                    || r.result_ref != reference.result_ref
            })
        {
            return Err(StorageError::CorruptRecord);
        }
    }
    Ok(())
}

/// Comparisons only. The actual plan, owner and progress always come from PostgreSQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RuntimeTaskVersion {
    pub task_revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
}
impl RuntimeTaskVersion {
    pub fn of(task: &TaskRecord) -> Self {
        Self {
            task_revision: task.revision,
            plan_version: task.plan.plan_version,
            facts_version: task.facts_version,
        }
    }
    pub(crate) fn check(self, task: &TaskRecord) -> Result<(), StorageError> {
        if self.plan_version != task.plan.plan_version {
            return Err(StorageError::StalePlan);
        }
        if self.facts_version != task.facts_version {
            return Err(StorageError::FactsConflict);
        }
        if self.task_revision != task.revision {
            return Err(StorageError::RevisionConflict);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RuntimeTaskExecutionStatus {
    /// A claim exists; it may still run or have been interrupted. Never automatically retry.
    RequiresOriginalTaskReview,
    Completed,
    WaitingUser,
    Failed,
}

/// Reference projection only. No new bank-result body or HTTP response contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeReadResultReference {
    pub tool_name: ToolName,
    pub operation_id: OperationId,
    pub result_ref: ToolResultRef,
}

/// Read-only backend view; result bodies are absent for legacy receipts and unknown claims.
/// Cannot be submitted to save execution progress.
/// ```compile_fail
/// let forged: safeflow_storage::RuntimeTaskExecutionView =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeTaskExecutionView {
    pub contract_version: u16,
    pub task: TaskRecord,
    pub execution_status: RuntimeTaskExecutionStatus,
    pub result_references: Vec<RuntimeReadResultReference>,
    pub results: Option<Vec<ToolResult>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanningOrigin {
    session_hash: ArgumentsHash,
    binding_version: u64,
    registration_hash: ArgumentsHash,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBinding {
    user_id: safeflow_contracts::UserId,
    binding_version: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub(crate) contract_version: u16,
    pub(crate) claim_id: String,
    pub(crate) status: RuntimeTaskExecutionStatus,
    pub(crate) task_hash: ArgumentsHash,
    pub(crate) bindings_hash: ArgumentsHash,
    pub(crate) results_hash: Option<ArgumentsHash>,
    pub(crate) result_references: Vec<RuntimeReadResultReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) results: Option<Vec<ToolResult>>,
}
struct Claim {
    id: String,
    initial: TaskRecord,
    session_hash: ArgumentsHash,
    binding: SessionBinding,
    bindings_hash: ArgumentsHash,
}

pub struct RuntimeTaskExecutionService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
    contracts: Vec<PreflightInputContract>,
}
impl<'a> RuntimeTaskExecutionService<'a> {
    pub fn for_trusted_backend(
        repository: PgRepository,
        runtime: &'a Runtime,
        contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, RuntimeTaskExecutionError> {
        PlanningIntakeService::for_registered_backend(runtime, None, contracts.clone())
            .map_err(|error| RuntimeTaskExecutionError::Review(error.to_string()))?;
        Ok(Self {
            repository,
            runtime,
            contracts,
        })
    }

    /// A committed claim is required before the first Tool call. No caller progress is accepted.
    pub async fn schedule(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected: RuntimeTaskVersion,
    ) -> Result<RuntimeTaskExecutionView, RuntimeTaskExecutionError> {
        self.schedule_inner(session, task_id, expected, false).await
    }

    /// Agent result display opts into bodies; legacy callers keep reference-only receipts.
    pub async fn schedule_for_agent_backend(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected: RuntimeTaskVersion,
    ) -> Result<RuntimeTaskExecutionView, RuntimeTaskExecutionError> {
        self.schedule_inner(session, task_id, expected, true).await
    }

    async fn schedule_inner(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected: RuntimeTaskVersion,
        include_result_bodies: bool,
    ) -> Result<RuntimeTaskExecutionView, RuntimeTaskExecutionError> {
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        let binding = self.runtime.session_binding_for_backend(session)?;
        let session_guard = || self.check_binding(session, &binding);
        let snapshot = self
            .repository
            .recover(&binding.user_id, task_id, &session_guard)
            .await?;
        expected.check(&snapshot.task)?;
        if snapshot.task.state != TaskState::Running
            || !snapshot.task.completed_nodes.is_empty()
            || !snapshot.operations.is_empty()
        {
            return Err(StorageError::RuntimeAlreadyClaimed.into());
        }
        let intake = PlanningIntakeService::for_registered_backend(
            self.runtime,
            None,
            self.contracts.clone(),
        )
        .map_err(|error| RuntimeTaskExecutionError::Review(error.to_string()))?;
        let proposal = PlanningResult::for_plan(
            Understanding {
                intent: "READ_ONLY_EXECUTION".into(),
                entities: json!({}),
                missing_fields: vec![],
                context_refs: vec![],
                decision: UnderstandingDecision::Pass,
            },
            snapshot.task.plan.clone(),
        );
        let checked = intake
            .accept_json_for_persistence(
                session,
                task_id,
                snapshot.task.plan.plan_version,
                &serde_json::to_vec(&proposal).map_err(|_| StorageError::CorruptRecord)?,
            )
            .await
            .map_err(|error| RuntimeTaskExecutionError::Review(error.to_string()))?;
        let origin: PlanningOrigin = snapshot
            .audit
            .iter()
            .find(|event| event.kind == PersistentAuditKind::TaskCreated)
            .and_then(|event| serde_json::from_value(event.details["planning_origin"].clone()).ok())
            .ok_or(StorageError::RuntimeIntakeRequired)?;
        if origin.session_hash != hash(session)?
            || origin.binding_version != binding.binding_version
        {
            return Err(StorageError::Session.into());
        }
        if origin.registration_hash != checked.registration_hash() {
            return Err(RuntimeTaskExecutionError::Review(
                "intake registrations changed since creation".into(),
            ));
        }
        let review = self
            .runtime
            .review_read_only_execution(
                snapshot.task.plan.clone(),
                session.clone(),
                snapshot.task.facts_version,
                self.contracts.clone(),
            )
            .await?;
        let guard = || {
            session_guard()?;
            intake
                .check_persistence_review(&checked)
                .map_err(|_| StorageError::Invalid("intake registrations changed"))?;
            self.runtime
                .check_read_only_review(&review)
                .map_err(|_| StorageError::Invalid("Runtime review changed"))
        };
        let claim = self
            .repository
            .claim_runtime_read(
                &snapshot.task,
                session,
                &binding,
                review.bindings_hash(),
                &guard,
            )
            .await?;
        guard()?;
        let execution = self.runtime.execute_reviewed_read_only(&review).await;
        match execution {
            Ok(verified) => {
                let view = self
                    .repository
                    .settle_runtime_read(
                        &claim,
                        Some(&verified),
                        review.bindings_hash(),
                        &guard,
                        include_result_bodies,
                    )
                    .await?;
                guard()?;
                Ok(view)
            }
            Err(error) => {
                self.repository
                    .settle_runtime_read(
                        &claim,
                        None,
                        review.bindings_hash(),
                        &guard,
                        include_result_bodies,
                    )
                    .await?;
                Err(error.into())
            }
        }
    }

    /// New-process recovery is an authenticated read and current-registration check.
    /// It never executes a Tool, resumes after a decision, or retries a claim.
    pub async fn recover(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected: RuntimeTaskVersion,
    ) -> Result<RuntimeTaskExecutionView, RuntimeTaskExecutionError> {
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        let binding = self.runtime.session_binding_for_backend(session)?;
        let guard = || self.check_binding(session, &binding);
        let (view, bindings_hash) = self
            .repository
            .read_runtime_execution(&binding.user_id, task_id, &guard)
            .await?;
        expected.check(&view.task)?;
        if matches!(
            view.execution_status,
            RuntimeTaskExecutionStatus::Completed | RuntimeTaskExecutionStatus::WaitingUser
        ) {
            let review = self
                .runtime
                .review_read_only_execution(
                    view.task.plan.clone(),
                    session.clone(),
                    view.task.facts_version,
                    self.contracts.clone(),
                )
                .await?;
            if review.bindings_hash() != &bindings_hash {
                return Err(RuntimeTaskExecutionError::Review(
                    "registrations changed".into(),
                ));
            }
            if view.execution_status == RuntimeTaskExecutionStatus::WaitingUser {
                self.runtime.revalidate_read_only_wait(&review).await?;
            }
            let current = self
                .repository
                .read_runtime_execution(&binding.user_id, task_id, &guard)
                .await?;
            self.runtime.check_read_only_review(&review)?;
            if current.0 != view {
                return Err(StorageError::RevisionConflict.into());
            }
        }
        guard()?;
        Ok(view)
    }
    fn check_binding(
        &self,
        session: &SessionRef,
        binding: &SessionBinding,
    ) -> Result<(), StorageError> {
        if self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?
            != *binding
        {
            return Err(StorageError::Session);
        }
        Ok(())
    }
}

impl PgRepository {
    async fn claim_runtime_read(
        &self,
        initial: &TaskRecord,
        session: &SessionRef,
        binding: &SessionBinding,
        bindings_hash: &ArgumentsHash,
        guard: Guard<'_>,
    ) -> Result<Claim, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let current = guarded(
            guard,
            lock_task(&mut tx, &initial.owner_id, &initial.task_id),
        )
        .await?;
        if current != *initial
            || !guarded(guard, operations(&mut tx, &initial.task_id))
                .await?
                .is_empty()
        {
            return Err(StorageError::RevisionConflict);
        }
        let id = format!("claim_{}", uuid::Uuid::new_v4());
        let inserted = guarded(guard, sqlx::query("INSERT INTO runtime_read_claims(task_id,claim_id,contract_version,initial_task,session_hash,session_binding,bindings_hash,status) VALUES ($1,$2,1,$3,$4,$5,$6,'CLAIMED') ON CONFLICT(task_id) DO NOTHING")
            .bind(initial.task_id.as_str()).bind(&id).bind(Json(initial)).bind(hash(session)?.as_str()).bind(Json(StoredBinding {user_id: binding.user_id.clone(), binding_version: binding.binding_version})).bind(bindings_hash.as_str()).execute(&mut *tx)).await?;
        if inserted.rows_affected() != 1 {
            return Err(StorageError::RuntimeAlreadyClaimed);
        }
        let claim = Claim {
            id: id.clone(),
            initial: initial.clone(),
            session_hash: hash(session)?,
            binding: binding.clone(),
            bindings_hash: bindings_hash.clone(),
        };
        guarded(guard, audit(&mut tx, initial, None, PersistentAuditKind::CheckpointSaved, json!({"runtime_read_claim":{"claim_id":id,"contract_version":READ_ONLY_EXECUTION_CONTRACT_VERSION}}))).await?;
        self.check_claim_transaction(&mut tx, &claim, guard).await?;
        guarded(guard, tx.commit()).await?;
        // A COMMIT response error never reaches execution. Re-read even after a successful
        // response to catch deferred triggers before handing the claim to the executor.
        let mut tx = guarded(guard, self.pool.begin()).await?;
        self.check_claim_transaction(&mut tx, &claim, guard).await?;
        guarded(guard, tx.commit()).await?;
        Ok(claim)
    }

    async fn check_claim_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: &Claim,
        guard: Guard<'_>,
    ) -> Result<(), StorageError> {
        let task = guarded(
            guard,
            lock_task(tx, &claim.initial.owner_id, &claim.initial.task_id),
        )
        .await?;
        let ops = guarded(guard, operations(tx, &task.task_id)).await?;
        let row = guarded(
            guard,
            sqlx::query("SELECT * FROM runtime_read_claims WHERE task_id=$1 FOR UPDATE")
                .bind(task.task_id.as_str())
                .fetch_one(&mut **tx),
        )
        .await?;
        let stored = row.try_get::<Json<StoredBinding>, _>("session_binding")?.0;
        if task != claim.initial
            || !ops.is_empty()
            || row.try_get::<String, _>("claim_id")? != claim.id
            || row.try_get::<i16, _>("contract_version")?
                != READ_ONLY_EXECUTION_CONTRACT_VERSION as i16
            || row.try_get::<Json<TaskRecord>, _>("initial_task")?.0 != claim.initial
            || row.try_get::<String, _>("status")? != "CLAIMED"
            || row
                .try_get::<Option<Json<Receipt>>, _>("receipt")?
                .is_some()
            || row.try_get::<String, _>("session_hash")? != claim.session_hash.as_str()
            || stored.user_id != claim.binding.user_id
            || stored.binding_version != claim.binding.binding_version
            || row.try_get::<String, _>("bindings_hash")? != claim.bindings_hash.as_str()
        {
            return Err(StorageError::CorruptRecord);
        }
        let audited = guarded(guard, sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND task_revision=$2 AND details ? 'runtime_read_claim' ORDER BY sequence DESC LIMIT 1")
            .bind(task.task_id.as_str()).bind(db_version(task.revision)?).fetch_optional(&mut **tx)).await?.ok_or(StorageError::CorruptRecord)?;
        if audited.try_get::<Json<serde_json::Value>, _>("details")?.0["runtime_read_claim"]
            != json!({"claim_id":claim.id,"contract_version":READ_ONLY_EXECUTION_CONTRACT_VERSION})
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(())
    }

    async fn settle_runtime_read(
        &self,
        claim: &Claim,
        verified: Option<&VerifiedReadOnlyExecution>,
        bindings_hash: &ArgumentsHash,
        guard: Guard<'_>,
        include_result_bodies: bool,
    ) -> Result<RuntimeTaskExecutionView, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let old = guarded(
            guard,
            lock_task(&mut tx, &claim.initial.owner_id, &claim.initial.task_id),
        )
        .await?;
        if old != claim.initial
            || !guarded(guard, operations(&mut tx, &old.task_id))
                .await?
                .is_empty()
        {
            return Err(StorageError::RevisionConflict);
        }
        let row = guarded(
            guard,
            sqlx::query(
                "SELECT * FROM runtime_read_claims WHERE task_id=$1 AND claim_id=$2 FOR UPDATE",
            )
            .bind(old.task_id.as_str())
            .bind(&claim.id)
            .fetch_optional(&mut *tx),
        )
        .await?
        .ok_or(StorageError::CorruptRecord)?;
        if row.try_get::<String, _>("status")? != "CLAIMED"
            || row.try_get::<String, _>("bindings_hash")? != bindings_hash.as_str()
        {
            return Err(StorageError::RuntimeAlreadyClaimed);
        }
        let mut next = old.clone();
        next.revision = old
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        db_version(next.revision)?;
        let results = verified
            .filter(|_| include_result_bodies)
            .map(|v| crate::billing_report::visible_results(v.results()));
        let (status, results_hash, references) = if let Some(verified) = verified {
            if verified.plan() != &old.plan
                || verified.owner() != &old.owner_id
                || verified.facts_version() != old.facts_version
                || verified.bindings_hash() != bindings_hash
                || hash(verified.session_ref())?.as_str()
                    != row.try_get::<String, _>("session_hash")?
            {
                return Err(StorageError::PlanConflict);
            }
            next.completed_nodes = verified.completed_nodes().to_vec();
            let status = if let Some(node_id) = verified.waiting_node() {
                next.state = old
                    .state
                    .transition(TaskEvent::AwaitUser)
                    .map_err(|_| StorageError::IllegalTransition)?;
                next.checkpoint = RecoveryPoint::AtNode {
                    node_id: node_id.clone(),
                };
                RuntimeTaskExecutionStatus::WaitingUser
            } else {
                next.state = old
                    .state
                    .transition(TaskEvent::Complete)
                    .map_err(|_| StorageError::IllegalTransition)?;
                next.checkpoint = RecoveryPoint::Finished;
                RuntimeTaskExecutionStatus::Completed
            };
            (
                status,
                Some(hash(&verified.results())?),
                verified
                    .results()
                    .iter()
                    .map(|result| RuntimeReadResultReference {
                        tool_name: result.tool_name.clone(),
                        operation_id: result.operation_id.clone(),
                        result_ref: result.result_ref.clone(),
                    })
                    .collect(),
            )
        } else {
            next.state = old
                .state
                .transition(TaskEvent::Fail)
                .map_err(|_| StorageError::IllegalTransition)?;
            next.checkpoint = RecoveryPoint::Finished;
            (RuntimeTaskExecutionStatus::Failed, None, vec![])
        };
        validate_checkpoint(&next, &[])?;
        let receipt = Receipt {
            contract_version: READ_ONLY_EXECUTION_CONTRACT_VERSION,
            claim_id: claim.id.clone(),
            status,
            task_hash: hash(&next)?,
            bindings_hash: bindings_hash.clone(),
            results_hash,
            result_references: references.clone(),
            results: results.clone(),
        };
        guarded(guard, update_task(&mut tx, &next, old.revision)).await?;
        let updated = guarded(guard, sqlx::query("UPDATE runtime_read_claims SET status=$1,receipt=$2,updated_at=clock_timestamp() WHERE task_id=$3 AND claim_id=$4 AND status='CLAIMED'")
            .bind(if status == RuntimeTaskExecutionStatus::Failed { "FAILED" } else { "SETTLED" }).bind(Json(&receipt)).bind(old.task_id.as_str()).bind(&claim.id).execute(&mut *tx)).await?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::RuntimeAlreadyClaimed);
        }
        if let Some(verified) = verified.filter(|_| include_result_bodies) {
            guarded(
                guard,
                crate::billing_report::archive_settled_reports(
                    &mut tx,
                    &next,
                    &claim.id,
                    "ROOT",
                    &json!(receipt),
                    verified.results(),
                ),
            )
            .await?;
        }
        guarded(
            guard,
            audit(
                &mut tx,
                &next,
                None,
                PersistentAuditKind::CheckpointSaved,
                json!({"runtime_read_execution":receipt}),
            ),
        )
        .await?;
        let reread = guarded(guard, lock_task(&mut tx, &next.owner_id, &next.task_id)).await?;
        let ops = guarded(guard, operations(&mut tx, &next.task_id)).await?;
        let row = guarded(
            guard,
            sqlx::query("SELECT * FROM runtime_read_claims WHERE task_id=$1 FOR UPDATE")
                .bind(next.task_id.as_str())
                .fetch_one(&mut *tx),
        )
        .await?;
        let audited = guarded(guard, sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND task_revision=$2 AND details ? 'runtime_read_execution' ORDER BY sequence DESC LIMIT 1").bind(next.task_id.as_str()).bind(db_version(next.revision)?).fetch_one(&mut *tx)).await?;
        let stored_binding = row.try_get::<Json<StoredBinding>, _>("session_binding")?.0;
        if reread != next
            || !ops.is_empty()
            || row.try_get::<String, _>("claim_id")? != claim.id
            || row.try_get::<Json<TaskRecord>, _>("initial_task")?.0 != claim.initial
            || row.try_get::<String, _>("session_hash")? != claim.session_hash.as_str()
            || stored_binding.user_id != claim.binding.user_id
            || stored_binding.binding_version != claim.binding.binding_version
            || row.try_get::<String, _>("bindings_hash")? != bindings_hash.as_str()
            || row.try_get::<String, _>("status")?
                != if status == RuntimeTaskExecutionStatus::Failed {
                    "FAILED"
                } else {
                    "SETTLED"
                }
            || row.try_get::<Json<Receipt>, _>("receipt")?.0 != receipt
            || audited.try_get::<Json<serde_json::Value>, _>("details")?.0["runtime_read_execution"]
                != serde_json::to_value(&receipt).map_err(|_| StorageError::CorruptRecord)?
        {
            return Err(StorageError::CorruptRecord);
        }
        guarded(guard, tx.commit()).await?;
        let expected = RuntimeTaskExecutionView {
            contract_version: READ_ONLY_EXECUTION_CONTRACT_VERSION,
            task: next,
            execution_status: status,
            result_references: references,
            results,
        };
        let (current, _) = self
            .read_runtime_execution(&expected.task.owner_id, &expected.task.task_id, guard)
            .await?;
        if current != expected {
            return Err(StorageError::RevisionConflict);
        }
        Ok(current)
    }

    async fn read_runtime_execution(
        &self,
        owner: &safeflow_contracts::UserId,
        task_id: &TaskId,
        guard: Guard<'_>,
    ) -> Result<(RuntimeTaskExecutionView, ArgumentsHash), StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let row = guarded(
            guard,
            sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR SHARE")
                .bind(task_id.as_str())
                .bind(owner.as_str())
                .fetch_optional(&mut *tx),
        )
        .await?
        .ok_or(StorageError::NotFound)?;
        let task = crate::repository::task_row(&row)?;
        validate_checkpoint(&task, &guarded(guard, operations(&mut tx, task_id)).await?)?;
        let row = guarded(
            guard,
            sqlx::query("SELECT * FROM runtime_read_claims WHERE task_id=$1")
                .bind(task_id.as_str())
                .fetch_optional(&mut *tx),
        )
        .await?
        .ok_or(StorageError::RuntimeIntakeRequired)?;
        let stored = row.try_get::<Json<StoredBinding>, _>("session_binding")?.0;
        if stored.user_id != *owner
            || stored.binding_version == 0
            || row.try_get::<i16, _>("contract_version")?
                != READ_ONLY_EXECUTION_CONTRACT_VERSION as i16
        {
            return Err(StorageError::CorruptRecord);
        }
        ArgumentsHash::parse(row.try_get::<String, _>("session_hash")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        let bindings_hash = ArgumentsHash::parse(row.try_get::<String, _>("bindings_hash")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        let receipt = row
            .try_get::<Option<Json<Receipt>>, _>("receipt")?
            .map(|receipt| receipt.0);
        let view = if let Some(receipt) = receipt {
            check_result_bodies(
                receipt.results.as_deref(),
                &receipt.result_references,
                receipt.results_hash.as_ref(),
            )?;
            let event = guarded(guard, sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND task_revision=$2 AND details ? 'runtime_read_execution' ORDER BY sequence DESC LIMIT 1").bind(task_id.as_str()).bind(db_version(task.revision)?).fetch_optional(&mut *tx)).await?.ok_or(StorageError::CorruptRecord)?;
            let audited: Receipt = serde_json::from_value(
                event.try_get::<Json<serde_json::Value>, _>("details")?.0["runtime_read_execution"]
                    .clone(),
            )
            .map_err(|_| StorageError::CorruptRecord)?;
            let expected_state = match receipt.status {
                RuntimeTaskExecutionStatus::Completed => TaskState::Completed,
                RuntimeTaskExecutionStatus::WaitingUser => TaskState::WaitingUser,
                RuntimeTaskExecutionStatus::Failed => TaskState::Failed,
                RuntimeTaskExecutionStatus::RequiresOriginalTaskReview => {
                    return Err(StorageError::CorruptRecord)
                }
            };
            if receipt != audited
                || receipt.task_hash != hash(&task)?
                || receipt.contract_version != READ_ONLY_EXECUTION_CONTRACT_VERSION
                || receipt.bindings_hash != bindings_hash
                || receipt.claim_id != row.try_get::<String, _>("claim_id")?
                || task.state != expected_state
                || row.try_get::<String, _>("status")?
                    != if receipt.status == RuntimeTaskExecutionStatus::Failed {
                        "FAILED"
                    } else {
                        "SETTLED"
                    }
            {
                return Err(StorageError::CorruptRecord);
            }
            RuntimeTaskExecutionView {
                contract_version: READ_ONLY_EXECUTION_CONTRACT_VERSION,
                task,
                execution_status: receipt.status,
                result_references: receipt.result_references,
                results: receipt.results,
            }
        } else {
            if row.try_get::<String, _>("status")? != "CLAIMED" {
                return Err(StorageError::CorruptRecord);
            }
            RuntimeTaskExecutionView {
                contract_version: READ_ONLY_EXECUTION_CONTRACT_VERSION,
                task,
                execution_status: RuntimeTaskExecutionStatus::RequiresOriginalTaskReview,
                result_references: vec![],
                results: None,
            }
        };
        guarded(guard, tx.commit()).await?;
        Ok((view, bindings_hash))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeTaskExecutionError {
    Runtime(RuntimeError),
    Storage(StorageError),
    Review(String),
}
impl From<RuntimeError> for RuntimeTaskExecutionError {
    fn from(value: RuntimeError) -> Self {
        Self::Runtime(value)
    }
}
impl From<StorageError> for RuntimeTaskExecutionError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}
impl fmt::Display for RuntimeTaskExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => error.fmt(f),
            Self::Storage(error) => error.fmt(f),
            Self::Review(reason) => write!(f, "Runtime task review: {reason}"),
        }
    }
}
impl std::error::Error for RuntimeTaskExecutionError {}
