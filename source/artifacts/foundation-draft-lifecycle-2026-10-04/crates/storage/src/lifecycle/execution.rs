use super::service::{check_preflight, ensure_active, result_parameters};
use super::*;
use crate::confirmation::{timestamp, ConfirmationSession};
use crate::confirmation_repository::{check_active_at, read_stored_confirmation};
use crate::models::db_version;
use crate::repository::{audit, encode, lock_task_for_mode, operations, update_task};
use safeflow_contracts::{
    ConfirmationRef, IdempotencyKey, OperationEvent, SecurityPhase, TaskEvent, ToolResultStatus,
};

mod audit_source;
pub use audit_source::{LifecycleAuditSource, LifecycleAuditSourceReader, LifecycleAuditSources};
const DRAFT_CLOSE:&str="NONFUNDING_UNKNOWN_DRAFT_CLOSED_V1";

pub(crate) async fn reject_closed_nonfunding_draft(tx:&mut Transaction<'_,Postgres>,task:&TaskId)->Result<(),StorageError>{
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND details->>'schema' IN($2,'UNEXECUTED_DRAFT_TERMINATED_V1'))").bind(task.as_str()).bind(DRAFT_CLOSE).fetch_one(&mut **tx).await?;
    if exists {
        let row=sqlx::query("SELECT owner_id FROM tasks WHERE task_id=$1").bind(task.as_str()).fetch_one(&mut **tx).await?;
        let owner=crate::repository::decode(&row,"owner_id")?;
        // A marker's existence alone is never proof of an unpaid/expired intent.
        // Keep corrupt notes and the old UNKNOWN-close on their original paths.
        if let Some(note)=super::intent::note(tx,&owner,task).await? {
            return Err(super::intent::ended_error(&note));
        }
        return Err(StorageError::ExecutionOperationConflict);
    }Ok(())
}

async fn nonfunding_facts(tx:&mut Transaction<'_,Postgres>,task:&TaskRecord,ops:&[OperationRecord])->Result<Value,StorageError>{
    checked_history(tx,task,ops).await?;
    let claims=checked_claims(tx,task,ops).await?;
    if task.state!=TaskState::Running || ops.len()!=1 || claims.len()!=1 || claims[0].receipt.is_some() || ops[0].state!=OperationState::Draft || ops[0].business_operation_ref.is_some(){return Err(StorageError::ExecutionOperationConflict);}
    let op=&ops[0];
    let row=sqlx::query("SELECT * FROM mock_draft_lifecycle_claims WHERE task_id=$1 AND operation_id=$2 AND status='CLAIMED' AND receipt IS NULL").bind(task.task_id.as_str()).bind(op.operation_id.as_str()).fetch_one(&mut **tx).await?;
    let source=row.try_get::<Json<FrozenSource>,_>("source")?.0;
    let actions:i64=sqlx::query_scalar("SELECT count(*) FROM mock_draft_lifecycle_actions WHERE task_id=$1").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
    let now:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(&mut **tx).await?;
    // The frozen, verified action can only create a simulated draft. It has
    // no payment capability. Absence of financial rows is an additional guard,
    // never a claim that the original Tool did not run.
    let financial:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_preparations WHERE task_id=$1) OR EXISTS(SELECT 1 FROM public_execution_requests WHERE task_id=$1) OR EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE task_id=$1) OR EXISTS(SELECT 1 FROM operations WHERE task_id=$1 AND (state IN('SUBMITTED','SUCCEEDED','PROCESSING') OR business_operation_ref IS NOT NULL))").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
    let reference=ConfirmationRef::parse(source.confirmation["confirmation_ref"].as_str().ok_or(StorageError::CorruptRecord)?).map_err(|_|StorageError::CorruptRecord)?;
    let confirmed=crate::confirmation_repository::read_historical_confirmation(tx,&task.task_id,&reference).await?;
    if confirmed.record.expires_at > now || source.confirmation["status"] != "CONFIRMED" {return Err(StorageError::ExecutionOperationConflict);}
    if actions!=1 || financial || source.action!="CREATE_MOCK_DRAFT" || source.observed_draft.is_some() || source.parameters["replaces"]!=Value::Null ||
        source.expires_at>=now || source.task!=*task || source.operations!=ops ||
        source.task.plan.nodes.iter().any(|n|matches!(n,PlanNode::Tool{tool,..} if tool.as_str()!="create_mock_transfer_draft")) {
        return Err(StorageError::ExecutionOperationConflict);
    }
    Ok(json!({"schema":DRAFT_CLOSE,"task_id":task.task_id,"operation_id":op.operation_id,"task_revision":task.revision,"operation_revision":op.revision,
        "plan_version":task.plan.plan_version,"claim_id":row.try_get::<String,_>("claim_id")?,"source_digest":hash(&source)?,
        "scope":"MOCK_DRAFT_LIFECYCLE_ONLY","original_result":"UNKNOWN","funding_authorized":false,"original_preserved":true}))
}

pub(crate) async fn closed_nonfunding_draft(tx:&mut Transaction<'_,Postgres>,owner:&safeflow_contracts::UserId,task:&TaskId)->Result<bool,StorageError>{
    let rows=sqlx::query("SELECT e.*,EXISTS(SELECT 1 FROM public_audit_outbox o WHERE o.event_id=e.event_id) AS enqueued FROM audit_events e JOIN tasks t USING(task_id) WHERE e.task_id=$1 AND t.owner_id=$2 AND e.kind='CHECKPOINT_SAVED' AND e.details->>'schema'=$3").bind(task.as_str()).bind(owner.as_str()).bind(DRAFT_CLOSE).fetch_all(&mut **tx).await?;
    if rows.is_empty(){return Ok(false);}if rows.len()!=1{return Err(StorageError::CorruptRecord);}
    let stored=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2").bind(task.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?)?;
    let ops=operations(tx,task).await?;
    let facts=nonfunding_facts(tx,&stored,&ops).await?;
    if rows[0].try_get::<Value,_>("details")?!=facts || !rows[0].try_get::<bool,_>("enqueued")? {return Err(StorageError::CorruptRecord);}Ok(true)
}

impl LifecycleService<'_>{
    /// Explicit closure of an expired, initial, non-funding draft view. The
    /// original task/claim/receipt remain UNKNOWN; this is not cancellation or
    /// a conclusion about whether the historical Tool ran.
    pub async fn close_unknown_nonfunding_draft(&self,session:&SessionRef,id:&TaskId,op:&OperationId,expected:&MockDraftExecutionVersion)->Result<Value,StorageError>{
        if !self.transfer_sql_bridge{return Err(StorageError::DependencyUnavailable);}
        let identity=self.identity(session)?;
        let mut tx=self.repository.pool.begin().await?;
        let task=lock_task_for_mode(&mut tx,&identity.user_id,id,true).await?;
        let ops=operations(&mut tx,id).await?;
        let original=ops.iter().find(|o|o.operation_id==*op).ok_or(StorageError::NotFound)?;
        compare(expected,&task,original).map_err(|_|StorageError::RevisionConflict)?;
        let facts=nonfunding_facts(&mut tx,&task,&ops).await?;
        if !closed_nonfunding_draft(&mut tx,&identity.user_id,id).await? {
            audit(&mut tx,&task,Some(original),PersistentAuditKind::CheckpointSaved,facts).await?;
        }
        self.guard(session,&identity)?;
        tx.commit().await?;
        self.guard(session,&identity)?;
        Ok(json!({"contract_version":1,"task_id":id,"operation_id":op,"closed_nonfunding_draft":true,"original_result":"UNKNOWN","original_preserved":true,"funding_authorized":false,"automatic_retry":false}))
    }
}

#[cfg(test)]
#[path = "execution_replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
#[derive(Default)]
pub(super) struct ExecutionSchedule {
    after_empty_replay: Option<Arc<ExecutionPause>>,
    after_claim_commit: Option<Arc<ExecutionPause>>,
}

#[cfg(test)]
#[derive(Default)]
struct ExecutionPause {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
impl ExecutionPause {
    async fn wait(&self) {
        self.reached.notify_one();
        self.release.notified().await;
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenSource {
    task: TaskRecord,
    operations: Vec<OperationRecord>,
    operation_id: OperationId,
    action: String,
    parameters: Value,
    observed_draft: Option<LifecycleDraftObservation>,
    binding: Value,
    confirmation: Value,
    pre_tool: Value,
    pre_auth: Value,
    session_digest: ArgumentsHash,
    session_user: safeflow_contracts::UserId,
    session_binding_version: u64,
    expires_at: chrono::DateTime<chrono::Utc>,
    checked_at: chrono::DateTime<chrono::Utc>,
}
impl FrozenSource {
    fn operation(&self) -> Result<&OperationRecord, StorageError> {
        self.operations
            .iter()
            .find(|op| op.operation_id == self.operation_id)
            .ok_or(StorageError::CorruptRecord)
    }
}

impl LifecycleService<'_> {
    pub async fn execute(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        expected: &MockDraftExecutionVersion,
    ) -> Result<LifecycleView, StorageError> {
        if let Some(view) = self
            .repeat_execution(session, task_id, operation, confirmation, expected)
            .await?
        {
            return Ok(view);
        }
        #[cfg(test)]
        if let Some(pause) = &self.execution_schedule.after_empty_replay {
            pause.wait().await;
        }
        let frozen = async {
            let identity = self.identity(session)?;
            let view = self.query(session, task_id, Some(operation)).await?;
            ensure_active(&view)?;
            compare(expected, &view.task, &view.operation)?;
            let c = view
                .confirmation
                .as_ref()
                .ok_or(StorageError::ConfirmationRequired)?;
            if &c.confirmation_ref != confirmation || c.status != ConfirmationStatus::Confirmed {
                return Err(StorageError::ConfirmationRequired);
            }
            let a = view
                .pre_tool
                .as_ref()
                .ok_or(StorageError::DecisionSourcesUnavailable)?;
            let b = view
                .pre_auth
                .as_ref()
                .ok_or(StorageError::DecisionSourcesUnavailable)?;
            check_preflight(a, SecurityPhase::PreTool)?;
            check_preflight(b, SecurityPhase::PreAuth)?;
            let now = self.decisions.observe_precise_time().await?;
            let mut expires_at = c
                .expires_at
                .min(
                    chrono::DateTime::from_timestamp_millis(a.expires_at_ms)
                        .ok_or(StorageError::CorruptRecord)?,
                )
                .min(
                    chrono::DateTime::from_timestamp_millis(b.expires_at_ms)
                        .ok_or(StorageError::CorruptRecord)?,
                );
            if let Some(draft) = &view.observed_draft {
                expires_at = expires_at.min(draft.expires_at);
            }
            let source = FrozenSource {
                task: view.task.clone(),
                operations: view.operations.clone(),
                operation_id: operation.clone(),
                action: view.action.action().into(),
                parameters: view.parameters.clone(),
                observed_draft: view.observed_draft.clone(),
                binding: value(&c.binding)?,
                confirmation: value(c)?,
                pre_tool: value(a)?,
                pre_auth: value(b)?,
                session_digest: hash(&("safeflow-confirmation-session:v1", session.as_str()))?,
                session_user: identity.user_id.clone(),
                session_binding_version: identity.binding_version,
                expires_at,
                checked_at: now,
            };
            Ok::<_, StorageError>((identity, view.action, source))
        }
        .await;
        let (identity, action, source) = match frozen {
            Ok(source) => source,
            Err(error) => {
                return self
                    .repeat_execution(session, task_id, operation, confirmation, expected)
                    .await?
                    .ok_or(error);
            }
        };
        let claim_id = format!("lifecycle_claim_{}", uuid::Uuid::new_v4());
        let mut tx = self.repository.pool.begin().await?;
        if let Err(error) = self.lock_sources(&mut tx, session, &source, None).await {
            tx.rollback().await?;
            return self
                .repeat_execution(session, task_id, operation, confirmation, expected)
                .await?
                .ok_or(error);
        }
        let observed = self
            .observe_for_action(
                &mut tx,
                session,
                &identity,
                &source.task,
                source.operation()?,
                action,
            )
            .await?;
        if observed != source.observed_draft {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let inserted = sqlx::query("INSERT INTO mock_draft_lifecycle_claims(operation_id,task_id,node_id,plan_version,action,claim_id,source,source_digest,status) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'CLAIMED') ON CONFLICT(operation_id) DO NOTHING")
            .bind(operation.as_str()).bind(task_id.as_str()).bind(source.operation()?.node_id.as_str()).bind(source.task.plan.plan_version as i64).bind(&source.action).bind(&claim_id).bind(Json(&source)).bind(hash(&source)?.as_str()).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            tx.rollback().await?;
            return self
                .repeat_execution(session, task_id, operation, confirmation, expected)
                .await?
                .ok_or(StorageError::CorruptRecord);
        }
        audit(
            &mut tx,
            &source.task,
            Some(source.operation()?),
            PersistentAuditKind::CheckpointSaved,
            json!({"lifecycle_claim":{"claim_id":claim_id,"source_digest":hash(&source)?}}),
        )
        .await?;
        self.lock_sources(&mut tx, session, &source, Some(&claim_id))
            .await?;
        // Only a clearly successful claim COMMIT permits the original call to reach a Tool.
        tx.commit().await?;

        #[cfg(test)]
        if let Some(pause) = &self.execution_schedule.after_claim_commit {
            pause.wait().await;
        }

        let mut tx = self.repository.pool.begin().await?;
        self.lock_sources(&mut tx, session, &source, Some(&claim_id))
            .await?;
        checked_claims(&mut tx, &source.task, &source.operations).await?;
        let mut context = Self::context(&source.task, source.operation()?);
        context.idempotency_key =
            Some(IdempotencyKey::parse(claim_id.clone()).map_err(|_| StorageError::CorruptRecord)?);
        let action = action_for(&source.action)?;
        // Recheck and lock SQL/actual memory agreement after the durable claim,
        // before a Tool may change the in-memory business state.
        if self.transfer_sql_bridge && action != MockDraftAction::Create {
            let observed = self
                .observe_for_action(
                    &mut tx,
                    session,
                    &identity,
                    &source.task,
                    source.operation()?,
                    action,
                )
                .await?;
            if observed != source.observed_draft {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
        }
        let result = self
            .runtime
            .execute_persistent_mock_draft_action_for_backend(
                context.clone(),
                session.clone(),
                source.session_binding_version,
                u32::try_from(source.task.facts_version)
                    .map_err(|_| StorageError::FactsConflict)?,
                source.expires_at.timestamp_millis(),
                source.checked_at.timestamp_millis(),
                source.parameters.clone(),
                action,
            )
            .await;
        let verified = match result {
            Ok(result) => result,
            Err(_) => {
                tx.rollback().await?;
                return self.query(session, task_id, Some(operation)).await;
            }
        };
        if verified.context() != &context || verified.action() != action {
            return Err(StorageError::CorruptRecord);
        }
        let receipt = make_receipt(
            &source,
            verified.result().clone(),
            timestamp(self.clock.as_ref())?,
        )?;
        self.lock_sources(&mut tx, session, &source, Some(&claim_id))
            .await?;
        checked_claims(&mut tx, &source.task, &source.operations).await?;
        if self.transfer_sql_bridge {
            self.save_sql_draft(&mut tx, session, &identity, &context, &receipt)
                .await?;
            self.lock_sources(&mut tx, session, &source, Some(&claim_id))
                .await?;
        }
        self.settle(&mut tx, &source, &claim_id, &receipt).await?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        let mut view = self.query(session, task_id, Some(operation)).await?;
        view.replayed = false;
        Ok(view)
    }

    async fn repeat_execution(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        expected: &MockDraftExecutionVersion,
    ) -> Result<Option<LifecycleView>, StorageError> {
        let identity = self.identity(session)?;
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, task_id, true).await?;
        let ops = operations(&mut tx, task_id).await?;
        checked_history(&mut tx, &task, &ops).await?;
        checked_claims(&mut tx, &task, &ops).await?;
        let row = sqlx::query(
            "SELECT source FROM mock_draft_lifecycle_claims WHERE task_id=$1 AND operation_id=$2",
        )
        .bind(task_id.as_str())
        .bind(operation.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            self.guard(session, &identity)?;
            tx.commit().await?;
            return Ok(None);
        };
        let source = row.try_get::<Json<FrozenSource>, _>("source")?.0;
        compare(expected, &source.task, source.operation()?)
            .map_err(|_| StorageError::DecisionConflict)?;
        if source.confirmation["confirmation_ref"] != json!(confirmation) {
            return Err(StorageError::DecisionConflict);
        }
        self.guard(session, &identity)?;
        tx.commit().await?;
        Ok(Some(self.query(session, task_id, Some(operation)).await?))
    }

    async fn lock_sources(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        source: &FrozenSource,
        own_claim: Option<&str>,
    ) -> Result<(), StorageError> {
        let identity = self.identity(session)?;
        if identity.user_id != source.session_user
            || identity.binding_version != source.session_binding_version
            || hash(&("safeflow-confirmation-session:v1", session.as_str()))?
                != source.session_digest
        {
            return Err(StorageError::Session);
        }
        let task = lock_task_for_mode(tx, &identity.user_id, &source.task.task_id, true).await?;
        reject_closed_nonfunding_draft(tx,&task.task_id).await?;
        let ops = operations(tx, &task.task_id).await?;
        if task != source.task || ops != source.operations {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let claims = sqlx::query("SELECT claim_id FROM mock_draft_lifecycle_claims WHERE task_id=$1 AND status='CLAIMED'").bind(task.task_id.as_str()).fetch_all(&mut **tx).await?;
        if claims
            .iter()
            .any(|row| row.try_get::<String, _>("claim_id").ok().as_deref() != own_claim)
        {
            return Err(StorageError::RuntimeAlreadyClaimed);
        }
        if own_claim.is_some() && claims.len() != 1 {
            return Err(StorageError::CorruptRecord);
        }
        // Hold the adopted-profile read lock through Tool execution/settlement.
        // A maintenance adoption cannot silently change a confirmed old action.
        if action_for(&source.action)? != MockDraftAction::Cancel {
            let request: Json<Value> = sqlx::query_scalar("SELECT request FROM mock_draft_lifecycle_actions WHERE operation_id=$1 AND task_id=$2")
                .bind(source.operation_id.as_str()).bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
            let payee = if action_for(&source.action)? == MockDraftAction::Create {&source.parameters["payee_id"]}
                else {source.parameters["changes"].get("payee_id").unwrap_or(&source.observed_draft.as_ref().ok_or(StorageError::CorruptRecord)?.parameters["payee_id"])};
            let payee = ResourceId::parse(payee.as_str().ok_or(StorageError::CorruptRecord)?).map_err(|_|StorageError::CorruptRecord)?;
            let profile = PgRepository::recipient_profile_on(&mut **tx, &identity.user_id, &payee).await?;
            if profile.as_ref().map(|p| json!(&p.revision)).as_ref() != request.0.get("recipient_profile_revision") {
                return Err(StorageError::FactsConflict);
            }
        }
        let binding = binding_for_mode(tx, &task, &ops, &source.operation_id, true).await?;
        if value(&binding)? != source.binding {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let a_id = StoredDecisionId::parse(
            source.pre_tool["stored_decision_id"]
                .as_str()
                .ok_or(StorageError::CorruptRecord)?,
        )?;
        let b_id = StoredDecisionId::parse(
            source.pre_auth["stored_decision_id"]
                .as_str()
                .ok_or(StorageError::CorruptRecord)?,
        )?;
        let records = self
            .decisions
            .read_display_sources(tx, session, &task, &ops, &[a_id, b_id])
            .await?;
        let [a, b] = records.as_slice() else {
            return Err(StorageError::CorruptRecord);
        };
        if value(a)? != source.pre_tool || value(b)? != source.pre_auth {
            return Err(StorageError::DecisionBindingMismatch);
        }
        check_preflight(a, SecurityPhase::PreTool)?;
        check_preflight(b, SecurityPhase::PreAuth)?;
        let reference = ConfirmationRef::parse(
            source.confirmation["confirmation_ref"]
                .as_str()
                .ok_or(StorageError::CorruptRecord)?,
        )
        .map_err(|_| StorageError::CorruptRecord)?;
        let stored = read_stored_confirmation(tx, &task.task_id, &reference).await?;
        stored.check_session(&ConfirmationSession {
            binding: identity.clone(),
            digest: source.session_digest.clone(),
        })?;
        if stored.record.status != ConfirmationStatus::Confirmed
            || value(&stored.record)? != source.confirmation
            || stored.record.binding != binding
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let now = self.decisions.observe_precise_time().await?;
        check_active_at(&stored.record, now)?;
        if now < source.checked_at || now >= source.expires_at {
            return Err(StorageError::DecisionExpired);
        }
        check_historical_sources(tx, source).await?;
        self.guard(session, &identity)?;
        Ok(())
    }

    async fn settle(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        source: &FrozenSource,
        claim_id: &str,
        receipt: &LifecycleReceipt,
    ) -> Result<(), StorageError> {
        let mut op = source.operation()?.clone();
        for event in [
            OperationEvent::GatesSatisfied,
            OperationEvent::SubmissionRecorded,
            OperationEvent::ExecutionSucceeded,
        ] {
            let old = op.revision;
            op.state = op
                .state
                .transition(event)
                .map_err(|_| StorageError::IllegalTransition)?;
            op.revision = op
                .revision
                .checked_add(1)
                .ok_or(StorageError::RevisionConflict)?;
            if event == OperationEvent::SubmissionRecorded {
                op.business_operation_ref = Some(receipt.business_operation_id.clone());
            }
            let changed = sqlx::query("UPDATE operations SET state=$1,revision=$2,business_operation_ref=$3,updated_at=clock_timestamp() WHERE operation_id=$4 AND task_id=$5 AND revision=$6")
                .bind(encode(op.state)?).bind(db_version(op.revision)?).bind(op.business_operation_ref.as_ref().map(|v|v.as_str())).bind(op.operation_id.as_str()).bind(op.task_id.as_str()).bind(db_version(old)?).execute(&mut **tx).await?;
            if changed.rows_affected() != 1 {
                return Err(StorageError::RevisionConflict);
            }
            audit(
                tx,
                &source.task,
                Some(&op),
                PersistentAuditKind::OperationTransitioned,
                json!({"event":event,"lifecycle_claim":claim_id}),
            )
            .await?;
        }
        let mut task = source.task.clone();
        task.completed_nodes.push(op.node_id.clone());
        let action = action_for(&source.action)?;
        if action == MockDraftAction::Cancel {
            task.state = task
                .state
                .transition(TaskEvent::Cancel)
                .map_err(|_| StorageError::IllegalTransition)?;
            task.checkpoint = RecoveryPoint::Finished;
        } else {
            task.state = task
                .state
                .transition(TaskEvent::AwaitUser)
                .map_err(|_| StorageError::IllegalTransition)?;
            let decision = task.plan.nodes.iter().find(|n|matches!(n,PlanNode::UserDecision{depends_on,..} if depends_on == &vec![op.node_id.clone()])).ok_or(StorageError::CorruptRecord)?;
            task.checkpoint = RecoveryPoint::AtNode {
                node_id: decision.id().clone(),
            };
        }
        task.revision = task
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        let mut ops = source.operations.clone();
        *ops.iter_mut()
            .find(|candidate| candidate.operation_id == op.operation_id)
            .ok_or(StorageError::CorruptRecord)? = op.clone();
        validate_checkpoint(&task, &ops)?;
        update_task(tx, &task, source.task.revision).await?;
        sqlx::query("UPDATE mock_draft_lifecycle_tasks SET last_success=$1,selection_ticket=$2 WHERE task_id=$3")
            .bind(op.operation_id.as_str()).bind(format!("lifecycle_{}",uuid::Uuid::new_v4().simple())).bind(task.task_id.as_str()).execute(&mut **tx).await?;
        let updated = sqlx::query("UPDATE mock_draft_lifecycle_claims SET status='SUCCEEDED',receipt=$1,receipt_digest=$2 WHERE operation_id=$3 AND claim_id=$4 AND status='CLAIMED'")
            .bind(Json(receipt)).bind(hash(receipt)?.as_str()).bind(op.operation_id.as_str()).bind(claim_id).execute(&mut **tx).await?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::CorruptRecord);
        }
        audit(tx,&task,Some(&op),PersistentAuditKind::CheckpointSaved,json!({"lifecycle_settled":{"claim_id":claim_id,"source_digest":hash(source)?,"receipt_digest":hash(receipt)?,"task":task,"operation":op}})).await?;
        checked_history(tx, &task, &ops).await?;
        checked_claims(tx, &task, &ops).await?;
        Ok(())
    }
}

pub(super) async fn checked_claims(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    ops: &[OperationRecord],
) -> Result<Vec<ClaimState>, StorageError> {
    let rows = sqlx::query(
        "SELECT * FROM mock_draft_lifecycle_claims WHERE task_id=$1 ORDER BY plan_version",
    )
    .bind(task.task_id.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut result = Vec::new();
    let mut last_success = None;
    for row in rows {
        let source = row.try_get::<Json<FrozenSource>, _>("source")?.0;
        let op = ops
            .iter()
            .find(|op| op.operation_id == source.operation_id)
            .ok_or(StorageError::CorruptRecord)?;
        let original = source.operation()?;
        let id: String = row.try_get("claim_id")?;
        if hash(&source)?.as_str() != row.try_get::<String, _>("source_digest")?
            || source.task.task_id != task.task_id
            || source.task.owner_id != task.owner_id
            || source.session_user != task.owner_id
            || source.session_binding_version == 0
            || source.task.state != TaskState::Running
            || original.state != OperationState::Draft
            || original.revision != 1
            || original.business_operation_ref.is_some()
            || source.operation_id.as_str() != row.try_get::<String, _>("operation_id")?
            || original.node_id.as_str() != row.try_get::<String, _>("node_id")?
            || source.task.plan.plan_version as i64 != row.try_get::<i64, _>("plan_version")?
            || source.action != row.try_get::<String, _>("action")?
            || original.plan_version != source.task.plan.plan_version
            || original.facts_version != source.task.facts_version
            || source.task.revision == 0
            || source.checked_at < chrono::DateTime::UNIX_EPOCH
            || source.expires_at <= source.checked_at
        {
            return Err(StorageError::CorruptRecord);
        }
        let plan: Json<safeflow_contracts::AgentPlan> = sqlx::query_scalar(
            "SELECT plan FROM mock_draft_lifecycle_plans WHERE task_id=$1 AND plan_version=$2",
        )
        .bind(task.task_id.as_str())
        .bind(source.task.plan.plan_version as i64)
        .fetch_one(&mut **tx)
        .await?;
        if plan.0 != source.task.plan {
            return Err(StorageError::CorruptRecord);
        }
        let actions = sqlx::query("SELECT * FROM mock_draft_lifecycle_actions WHERE task_id=$1 AND plan_version<=$2 ORDER BY operation_id")
                .bind(task.task_id.as_str()).bind(source.task.plan.plan_version as i64).fetch_all(&mut **tx).await?;
        if actions.len() != source.operations.len() {
            return Err(StorageError::CorruptRecord);
        }
        for previous in &source.operations {
            let actual = ops
                .iter()
                .find(|candidate| candidate.operation_id == previous.operation_id)
                .ok_or(StorageError::CorruptRecord)?;
            if previous.operation_id != source.operation_id && previous != actual {
                return Err(StorageError::CorruptRecord);
            }
        }
        validate_checkpoint(&source.task, &source.operations)
            .map_err(|_| StorageError::CorruptRecord)?;
        let current_binding =
            current_binding(&source.task, &source.operations, &source.operation_id)?;
        let mut expected_binding = value(&current_binding)?;
        expected_binding["lifecycle_anchor"] = source
            .binding
            .get("lifecycle_anchor")
            .ok_or(StorageError::CorruptRecord)?
            .clone();
        if expected_binding != source.binding
            || source.confirmation["binding"] != source.binding
            || source.pre_tool["binding"] != source.binding
            || source.pre_auth["binding"] != source.binding
            || source.confirmation["status"] != "CONFIRMED"
        {
            return Err(StorageError::CorruptRecord);
        }
        let action_row = actions
            .iter()
            .find(|candidate| {
                candidate
                    .try_get::<String, _>("operation_id")
                    .ok()
                    .as_deref()
                    == Some(source.operation_id.as_str())
            })
            .ok_or(StorageError::CorruptRecord)?;
        if action_row.try_get::<Json<Value>, _>("parameters")?.0 != source.parameters
            || action_row
                .try_get::<Option<Json<LifecycleDraftObservation>>, _>("observed_draft")?
                .map(|v| v.0)
                != source.observed_draft
        {
            return Err(StorageError::CorruptRecord);
        }
        let anchor = historical_anchor(tx, &source.task, &source.operations).await?;
        if source.binding["lifecycle_anchor"] != json!(anchor) {
            return Err(StorageError::CorruptRecord);
        }
        check_historical_sources(tx, &source).await?;
        let claim_audits: Vec<Json<Value>> = sqlx::query_scalar("SELECT details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='CHECKPOINT_SAVED' AND details ? 'lifecycle_claim'")
                .bind(task.task_id.as_str()).bind(op.operation_id.as_str()).fetch_all(&mut **tx).await?;
        if claim_audits.len() != 1
            || claim_audits[0].0["lifecycle_claim"]
                != json!({"claim_id":id,"source_digest":hash(&source)?})
        {
            return Err(StorageError::CorruptRecord);
        }
        let receipt = row
            .try_get::<Option<Json<LifecycleReceipt>>, _>("receipt")?
            .map(|v| v.0);
        match (row.try_get::<String, _>("status")?.as_str(), &receipt) {
            ("CLAIMED", None)
                if row
                    .try_get::<Option<String>, _>("receipt_digest")?
                    .is_none()
                    && op == original
                    && source.task == *task
                    && source.operations == ops => {}
            ("SUCCEEDED", Some(receipt)) => {
                if hash(receipt)?.as_str() != row.try_get::<String, _>("receipt_digest")?
                    || make_receipt(&source, receipt.result.clone(), receipt.executed_at)?
                        != *receipt
                    || op.state != OperationState::Succeeded
                    || op.revision != original.revision + 3
                    || op.business_operation_ref.as_ref() != Some(&receipt.business_operation_id)
                    || op.node_id != original.node_id
                    || op.plan_version != original.plan_version
                    || op.arguments_hash != original.arguments_hash
                {
                    return Err(StorageError::CorruptRecord);
                }
                let settled: Vec<Json<Value>> = sqlx::query_scalar("SELECT details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='CHECKPOINT_SAVED' AND details ? 'lifecycle_settled'")
                        .bind(task.task_id.as_str()).bind(op.operation_id.as_str()).fetch_all(&mut **tx).await?;
                if settled.len() != 1
                    || settled[0].0["lifecycle_settled"]["claim_id"] != id
                    || settled[0].0["lifecycle_settled"]["source_digest"] != json!(hash(&source)?)
                    || settled[0].0["lifecycle_settled"]["receipt_digest"] != json!(hash(receipt)?)
                    || settled[0].0["lifecycle_settled"]["operation"] != value(op)?
                {
                    return Err(StorageError::CorruptRecord);
                }
                let settlement_task: TaskRecord =
                    serde_json::from_value(settled[0].0["lifecycle_settled"]["task"].clone())
                        .map_err(|_| StorageError::CorruptRecord)?;
                if settlement_task.task_id != task.task_id
                    || settlement_task.owner_id != task.owner_id
                    || settlement_task.plan != source.task.plan
                    || settlement_task.revision != source.task.revision + 1
                    || settlement_task.facts_version != source.task.facts_version
                    || settlement_task.completed_nodes
                        != source
                            .task
                            .completed_nodes
                            .iter()
                            .cloned()
                            .chain(std::iter::once(op.node_id.clone()))
                            .collect::<Vec<_>>()
                    || settlement_task.state
                        != if source.action == MockDraftAction::Cancel.action() {
                            TaskState::Cancelled
                        } else {
                            TaskState::WaitingUser
                        }
                {
                    return Err(StorageError::CorruptRecord);
                }
                let mut settlement_ops = source.operations.clone();
                *settlement_ops
                    .iter_mut()
                    .find(|candidate| candidate.operation_id == op.operation_id)
                    .ok_or(StorageError::CorruptRecord)? = op.clone();
                validate_checkpoint(&settlement_task, &settlement_ops)
                    .map_err(|_| StorageError::CorruptRecord)?;
                let transitions: Vec<(i64,Json<Value>)> = sqlx::query_as("SELECT operation_revision,details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='OPERATION_TRANSITIONED' ORDER BY sequence")
                        .bind(task.task_id.as_str()).bind(op.operation_id.as_str()).fetch_all(&mut **tx).await?;
                if transitions.len() != 3 {
                    return Err(StorageError::CorruptRecord);
                }
                for ((revision, details), event) in transitions.iter().zip([
                    OperationEvent::GatesSatisfied,
                    OperationEvent::SubmissionRecorded,
                    OperationEvent::ExecutionSucceeded,
                ]) {
                    if *revision
                        != match event {
                            OperationEvent::GatesSatisfied => 2,
                            OperationEvent::SubmissionRecorded => 3,
                            _ => 4,
                        }
                        || details.0 != json!({"event":event,"lifecycle_claim":id})
                    {
                        return Err(StorageError::CorruptRecord);
                    }
                }
                last_success = Some(op.operation_id.clone());
            }
            _ => return Err(StorageError::CorruptRecord),
        }
        result.push(ClaimState {
            operation: op.operation_id.clone(),
            receipt,
        });
    }
    let marker: Option<String> =
        sqlx::query_scalar("SELECT last_success FROM mock_draft_lifecycle_tasks WHERE task_id=$1")
            .bind(task.task_id.as_str())
            .fetch_one(&mut **tx)
            .await?;
    if marker.as_deref() != last_success.as_ref().map(|v| v.as_str()) {
        return Err(StorageError::CorruptRecord);
    }
    for op in ops {
        if op.state == OperationState::Succeeded
            && !result
                .iter()
                .any(|claim| claim.operation == op.operation_id && claim.receipt.is_some())
        {
            return Err(StorageError::CorruptRecord);
        }
    }
    Ok(result)
}
pub(super) fn compare(
    expected: &MockDraftExecutionVersion,
    task: &TaskRecord,
    op: &OperationRecord,
) -> Result<(), StorageError> {
    if expected.task_revision != task.revision || expected.operation_revision != op.revision {
        return Err(StorageError::RevisionConflict);
    }
    if expected.plan_version != task.plan.plan_version {
        return Err(StorageError::StalePlan);
    }
    if expected.facts_version != task.facts_version {
        return Err(StorageError::FactsConflict);
    }
    Ok(())
}

fn make_receipt(
    source: &FrozenSource,
    result: ToolResult,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<LifecycleReceipt, StorageError> {
    let action = action_for(&source.action)?;
    if result.status != ToolResultStatus::Succeeded
        || result.operation_id != source.operation_id
        || result.tool_name.as_str() != action.tool_name()
        || result.data["task_id"] != json!(source.task.task_id)
        || result.data["runtime_plan_version"] != json!(source.task.plan.plan_version)
        || now < source.checked_at
        || now >= source.expires_at
    {
        return Err(StorageError::CorruptRecord);
    }
    let draft_id = ResourceId::parse(
        result.data["draft_id"]
            .as_str()
            .ok_or(StorageError::CorruptRecord)?,
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    let business_operation_id = BusinessOperationRef::parse(
        result.data["operation_id"]
            .as_str()
            .ok_or(StorageError::CorruptRecord)?,
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    let draft_version = result.data["plan_version"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or(StorageError::CorruptRecord)?;
    let expected_draft_version = if action == MockDraftAction::Create {
        None
    } else {
        Some(
            source.parameters["expected_version"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(StorageError::CorruptRecord)?,
        )
    };
    if action == MockDraftAction::Create {
        if source.observed_draft.is_some()
            || draft_version != 1
            || result.data["status"] != "READY_TO_CONFIRM"
            || result_parameters(&result.data)? != {
                let mut parameters = source.parameters.clone();
                let object = parameters.as_object_mut().ok_or(StorageError::CorruptRecord)?;
                // Match the registered CreateMockDraftInput defaults, without
                // altering the approved input/hash or accepting unknown fields.
                object.remove("replaces");
                object.entry("remark").or_insert_with(|| json!(""));
                parameters
            }
        {
            return Err(StorageError::CorruptRecord);
        }
    } else {
        let observed = source
            .observed_draft
            .as_ref()
            .ok_or(StorageError::CorruptRecord)?;
        let expected = expected_draft_version.ok_or(StorageError::CorruptRecord)?;
        if draft_id != observed.draft_id
            || business_operation_id != observed.business_operation_id
            || expected != observed.draft_version
            || result.data["expires_at"] != json!(observed.expires_at)
            || result.data["action"] != source.action
            || result.data["expected_version"] != json!(expected)
            || draft_version
                != if action == MockDraftAction::Modify {
                    expected.checked_add(1).ok_or(StorageError::CorruptRecord)?
                } else {
                    expected
                }
            || result.data["status"]
                != if action == MockDraftAction::Cancel {
                    "CANCELLED"
                } else {
                    "READY_TO_CONFIRM"
                }
        {
            return Err(StorageError::CorruptRecord);
        }
        let mut target = observed.parameters.clone();
        if action == MockDraftAction::Modify {
            for (key, value) in source.parameters["changes"]
                .as_object()
                .ok_or(StorageError::CorruptRecord)?
            {
                target
                    .as_object_mut()
                    .ok_or(StorageError::CorruptRecord)?
                    .insert(key.clone(), value.clone());
            }
        }
        if result_parameters(&result.data)? != target {
            return Err(StorageError::CorruptRecord);
        }
    }
    Ok(LifecycleReceipt {
        action: source.action.clone(),
        operation_id: source.operation_id.clone(),
        plan_version: source.task.plan.plan_version,
        expected_draft_version,
        draft_id,
        business_operation_id,
        draft_version,
        tool_result_ref: result.result_ref.clone(),
        executed_at: now,
        result,
    })
}

async fn check_historical_sources(
    tx: &mut Transaction<'_, Postgres>,
    source: &FrozenSource,
) -> Result<(), StorageError> {
    let reference = ConfirmationRef::parse(
        source.confirmation["confirmation_ref"]
            .as_str()
            .ok_or(StorageError::CorruptRecord)?,
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    let stored = crate::confirmation_repository::read_historical_confirmation(
        tx,
        &source.task.task_id,
        &reference,
    )
    .await?;
    if stored.session_digest != source.session_digest.as_str()
        || stored.session_binding_version != source.session_binding_version
    {
        return Err(StorageError::CorruptRecord);
    }
    let mut actual = value(&stored.record)?;
    let mut frozen = source.confirmation.clone();
    for key in ["status", "revoked_at"] {
        actual
            .as_object_mut()
            .ok_or(StorageError::CorruptRecord)?
            .remove(key);
        frozen
            .as_object_mut()
            .ok_or(StorageError::CorruptRecord)?
            .remove(key);
    }
    if actual != frozen {
        return Err(StorageError::CorruptRecord);
    }
    for (kind, status) in [
        ("CONFIRMATION_PREPARED", "PREPARED"),
        ("EXPLICIT_CONFIRMATION_RECORDED", "CONFIRMED"),
    ] {
        let expected = json!({"confirmation_ref":reference,"contract_version":1,
            "binding_digest":hash(&stored.record.binding)?,"status":status,"expires_at":stored.record.expires_at});
        check_source_audit(
            tx,
            source,
            kind,
            "confirmation_ref",
            reference.as_str(),
            &expected,
        )
        .await?;
    }
    for (frozen, phase) in [
        (&source.pre_tool, SecurityPhase::PreTool),
        (&source.pre_auth, SecurityPhase::PreAuth),
    ] {
        let id = StoredDecisionId::parse(
            frozen["stored_decision_id"]
                .as_str()
                .ok_or(StorageError::CorruptRecord)?,
        )?;
        let record =
            crate::decision::read_historical_decision(tx, &source.task.task_id, &id).await?;
        check_preflight(&record, phase)?;
        let mut actual = value(&record)?;
        let mut frozen = frozen.clone();
        for key in ["status", "ended_at_ms", "invalidation_reason"] {
            actual
                .as_object_mut()
                .ok_or(StorageError::CorruptRecord)?
                .remove(key);
            frozen
                .as_object_mut()
                .ok_or(StorageError::CorruptRecord)?
                .remove(key);
        }
        if actual != frozen {
            return Err(StorageError::CorruptRecord);
        }
        let expected = json!({"stored_decision_id":record.stored_decision_id,"request_id":record.request_id,
            "evidence_kind":"PREFLIGHT_ONLY","source_mode":record.source_mode,"phase":record.phase,
            "outcome":record.decision.outcome,"assembly_digest":record.assembly_digest,"decision_digest":record.decision_digest});
        check_source_audit(
            tx,
            source,
            "SECURITY_DECISION_RECORDED",
            "stored_decision_id",
            id.as_str(),
            &expected,
        )
        .await?;
    }
    Ok(())
}

async fn check_source_audit(
    tx: &mut Transaction<'_, Postgres>,
    source: &FrozenSource,
    kind: &str,
    key: &str,
    reference: &str,
    expected: &Value,
) -> Result<(), StorageError> {
    let rows: Vec<(i64, Option<i64>, Json<Value>)> = sqlx::query_as("SELECT task_revision,operation_revision,details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind=$3 AND details->>$4=$5")
        .bind(source.task.task_id.as_str()).bind(source.operation_id.as_str()).bind(kind).bind(key).bind(reference).fetch_all(&mut **tx).await?;
    if rows.len() != 1
        || rows[0].0 != db_version(source.task.revision)?
        || rows[0].1 != Some(db_version(source.operation()?.revision)?)
        || rows[0].2 .0 != *expected
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}
