//! Actual confirmed Create execution. PostgreSQL claim is durable; business storage is memory.
use super::*;
use crate::models::{valid_id, validate_checkpoint};
use crate::repository::{audit, encode, update_task};
use safeflow_contracts::{
    BusinessOperationRef, ExecutionContext, IdempotencyKey, OperationEvent, ResourceId, TaskEvent,
    ToolResultRef, MOCK_DRAFT_CREATE_TOOL,
};
use safeflow_runtime::RuntimeOutcome;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockDraftExecutionVersion {
    pub task_revision: u64,
    pub operation_revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalMockDraftReceipt {
    pub draft_id: ResourceId,
    pub business_operation_id: BusinessOperationRef,
    pub draft_version: u32,
    pub tool_result_ref: ToolResultRef,
    pub executed_at: chrono::DateTime<chrono::Utc>,
}

/// Backend historical projection, never a current business draft or an execution permission.
pub struct MockDraftExecutionView {
    pub task: TaskRecord,
    pub operation: OperationRecord,
    pub receipt: Option<HistoricalMockDraftReceipt>,
    pub replayed: bool,
}

#[derive(Clone, Serialize, PartialEq)]
struct Source {
    task: TaskRecord,
    operation: OperationRecord,
    parameters: Value,
    confirmation: ConfirmationRecord,
    pre_tool: StoredSecurityDecision,
    pre_auth: StoredSecurityDecision,
    session_digest: ArgumentsHash,
    session_binding: StoredBinding,
    expires_at: chrono::DateTime<chrono::Utc>,
    checked_at: chrono::DateTime<chrono::Utc>,
}
// Historical database data, never promoted back to actual verified source types.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSource {
    task: TaskRecord,
    operation: OperationRecord,
    parameters: Value,
    confirmation: Value,
    pre_tool: Value,
    pre_auth: Value,
    session_digest: ArgumentsHash,
    session_binding: StoredBinding,
    expires_at: chrono::DateTime<chrono::Utc>,
    checked_at: chrono::DateTime<chrono::Utc>,
}
fn source_digest(source: &Source) -> Result<ArgumentsHash, StorageError> {
    hash(&serde_json::to_value(source).map_err(|_| StorageError::CorruptRecord)?)
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StoredBinding {
    user_id: safeflow_contracts::UserId,
    binding_version: u64,
}
struct Claim {
    id: String,
    source: Source,
}

impl MockDraftExecutionVersion {
    fn check(&self, task: &TaskRecord, operation: &OperationRecord) -> Result<(), StorageError> {
        if self.task_revision != task.revision || self.operation_revision != operation.revision {
            return Err(StorageError::RevisionConflict);
        }
        if self.plan_version != task.plan.plan_version {
            return Err(StorageError::StalePlan);
        }
        if self.facts_version != task.facts_version {
            return Err(StorageError::FactsConflict);
        }
        Ok(())
    }
}

impl PlanningConfirmationService<'_> {
    /// All permission sources are loaded internally; callers supply comparisons only.
    pub async fn execute_confirmed_mock_draft(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        expected: &MockDraftExecutionVersion,
    ) -> Result<MockDraftExecutionView, StorageError> {
        for id in [task.as_str(), operation.as_str(), confirmation.as_str()] {
            valid_id(id)?;
        }
        let identity = self.identity(session)?;
        // Repeated commands only read the original claim. They never regenerate a Permit.
        if let Some(view) = self
            .read_mock_execution(session, task, operation, Some((confirmation, expected)))
            .await?
        {
            return Ok(view);
        }
        let source = match self.query(session, task).await.and_then(|view| {
            self.execution_source(session, &identity, view, operation, confirmation, expected)
        }) {
            Ok(source) => source,
            Err(error) => {
                return self
                    .read_mock_execution(session, task, operation, Some((confirmation, expected)))
                    .await?
                    .ok_or(error);
            }
        };
        let claim = Claim {
            id: format!("mock_draft_claim_{}", uuid::Uuid::new_v4()),
            source,
        };
        let mut tx = self.repository.pool.begin().await?;
        if let Err(error) = self
            .lock_execution_sources(&mut tx, session, &claim.source)
            .await
        {
            tx.rollback().await?;
            return self
                .read_mock_execution(session, task, operation, Some((confirmation, expected)))
                .await?
                .ok_or(error);
        }
        let inserted = sqlx::query("INSERT INTO mock_draft_execution_claims(task_id,operation_id,claim_id,owner_id,source,source_digest,status) VALUES($1,$2,$3,$4,$5,$6,'CLAIMED') ON CONFLICT(task_id) DO NOTHING")
            .bind(task.as_str()).bind(operation.as_str()).bind(&claim.id).bind(identity.user_id.as_str())
            .bind(Json(&claim.source)).bind(source_digest(&claim.source)?.as_str()).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            tx.rollback().await?;
            return self
                .read_mock_execution(session, task, operation, Some((confirmation, expected)))
                .await?
                .ok_or(StorageError::CorruptRecord);
        }
        audit(&mut tx, &claim.source.task, Some(&claim.source.operation), PersistentAuditKind::CheckpointSaved,
            json!({"mock_draft_execution_claim":{"claim_id":claim.id,"source_digest":source_digest(&claim.source)?}})).await?;
        self.lock_execution_sources(&mut tx, session, &claim.source)
            .await?;
        // Claim COMMIT uncertainty must never reach a Tool call.
        tx.commit().await?;

        let mut tx = self.repository.pool.begin().await?;
        self.lock_execution_sources(&mut tx, session, &claim.source)
            .await?;
        self.check_execution_claim(&mut tx, &claim).await?;
        let context = ExecutionContext {
            task_id: task.clone(),
            node_id: claim.source.operation.node_id.clone(),
            operation_id: operation.clone(),
            authenticated_user_id: identity.user_id,
            plan_version: claim.source.task.plan.plan_version,
            idempotency_key: Some(
                IdempotencyKey::parse(claim.id.clone()).map_err(|_| StorageError::CorruptRecord)?,
            ),
        };
        // Task-first and actual source row locks stay held only across this local Mock call.
        let result = self
            .runtime
            .execute_persistent_mock_draft_for_backend(
                context,
                session.clone(),
                claim.source.session_binding.binding_version,
                u32::try_from(claim.source.task.facts_version)
                    .map_err(|_| StorageError::FactsConflict)?,
                claim.source.expires_at.timestamp_millis(),
                claim.source.checked_at.timestamp_millis(),
                claim.source.parameters.clone(),
            )
            .await;
        let receipt = match result {
            Ok(outcome) => self.receipt_from_runtime(&claim.source, outcome)?,
            Err(_) => {
                tx.rollback().await?;
                return self
                    .query_mock_draft_execution(session, task, operation)
                    .await;
            }
        };
        self.lock_execution_sources(&mut tx, session, &claim.source)
            .await?;
        self.check_execution_claim(&mut tx, &claim).await?;
        let (next_task, next_operation) = self
            .finish_mock_execution(&mut tx, &claim, &receipt)
            .await?;
        self.guard(
            session,
            &SessionBinding {
                user_id: claim.source.session_binding.user_id.clone(),
                binding_version: claim.source.session_binding.binding_version,
            },
        )?;
        tx.commit().await?;
        Ok(MockDraftExecutionView {
            task: next_task,
            operation: next_operation,
            receipt: Some(receipt),
            replayed: false,
        })
    }

    fn execution_source(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        view: PlanningConfirmationView,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        expected: &MockDraftExecutionVersion,
    ) -> Result<Source, StorageError> {
        let op = view.operation.ok_or(StorageError::ConfirmationRequired)?;
        expected.check(&view.task, &op)?;
        if &op.operation_id != operation
            || op.state != OperationState::Draft
            || view.task.state != TaskState::Running
            || !view.task.completed_nodes.is_empty()
            || view.task.plan.nodes.len() != 1
            || identity.binding_version == 0
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let [PlanNode::Tool {
            tool,
            arguments,
            depends_on,
            ..
        }] = view.task.plan.nodes.as_slice()
        else {
            return Err(StorageError::DecisionSourcesUnavailable);
        };
        if tool.as_str() != MOCK_DRAFT_CREATE_TOOL
            || !depends_on.is_empty()
            || arguments != &view.parameters
        {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        let c = view
            .confirmation
            .ok_or(StorageError::ConfirmationRequired)?;
        if &c.confirmation_ref != confirmation || c.status != ConfirmationStatus::Confirmed {
            return Err(StorageError::ConfirmationRequired);
        }
        let a = view
            .pre_tool
            .ok_or(StorageError::DecisionSourcesUnavailable)?;
        let b = view
            .pre_auth
            .ok_or(StorageError::DecisionSourcesUnavailable)?;
        check_preflight(&a, SecurityPhase::PreTool)?;
        check_preflight(&b, SecurityPhase::PreAuth)?;
        if a.binding != b.binding || c.binding != a.binding {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let expires_at = c
            .expires_at
            .min(
                chrono::DateTime::from_timestamp_millis(a.expires_at_ms)
                    .ok_or(StorageError::CorruptRecord)?,
            )
            .min(
                chrono::DateTime::from_timestamp_millis(b.expires_at_ms)
                    .ok_or(StorageError::CorruptRecord)?,
            );
        Ok(Source {
            task: view.task,
            operation: op,
            parameters: view.parameters,
            confirmation: c,
            pre_tool: a,
            pre_auth: b,
            session_digest: hash(session)?,
            session_binding: StoredBinding {
                user_id: identity.user_id.clone(),
                binding_version: identity.binding_version,
            },
            expires_at,
            checked_at: timestamp(self.clock.as_ref())?,
        })
    }

    async fn lock_execution_sources(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        source: &Source,
    ) -> Result<(), StorageError> {
        self.guard(
            session,
            &SessionBinding {
                user_id: source.session_binding.user_id.clone(),
                binding_version: source.session_binding.binding_version,
            },
        )?;
        if hash(session)? != source.session_digest {
            return Err(StorageError::Session);
        }
        let task = lock_task(tx, &source.task.owner_id, &source.task.task_id).await?;
        let ops = operations(tx, &source.task.task_id).await?;
        if task != source.task || ops != [source.operation.clone()] {
            return Err(StorageError::RevisionConflict);
        }
        let records = self
            .decisions
            .read_display_sources(
                tx,
                session,
                &task,
                &ops,
                &[
                    source.pre_tool.stored_decision_id.clone(),
                    source.pre_auth.stored_decision_id.clone(),
                ],
            )
            .await?;
        if records != [source.pre_tool.clone(), source.pre_auth.clone()] {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let stored =
            read_stored_confirmation(tx, &task.task_id, &source.confirmation.confirmation_ref)
                .await?;
        stored.check_session(&ConfirmationSession {
            binding: self.identity(session)?,
            digest: hash(&("safeflow-confirmation-session:v1", session.as_str()))?,
        })?;
        if stored.record != source.confirmation
            || stored.record.status != ConfirmationStatus::Confirmed
            || current_binding(&task, &ops, &source.operation.operation_id)?
                != stored.record.binding
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let now = timestamp(self.clock.as_ref())?;
        check_active_at(&stored.record, now)?;
        crate::decision::check_time(&source.pre_tool, now.timestamp_millis())?;
        crate::decision::check_time(&source.pre_auth, now.timestamp_millis())?;
        if now >= source.expires_at {
            return Err(StorageError::DecisionExpired);
        }
        if now < source.checked_at {
            return Err(StorageError::Invalid("execution clock moved backwards"));
        }
        self.guard(
            session,
            &SessionBinding {
                user_id: source.session_binding.user_id.clone(),
                binding_version: source.session_binding.binding_version,
            },
        )
    }

    async fn check_execution_claim(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claim: &Claim,
    ) -> Result<(), StorageError> {
        let row =
            sqlx::query("SELECT * FROM mock_draft_execution_claims WHERE task_id=$1 FOR UPDATE")
                .bind(claim.source.task.task_id.as_str())
                .fetch_one(&mut **tx)
                .await?;
        let source = row.try_get::<Json<Value>, _>("source")?.0;
        if row.try_get::<String, _>("claim_id")? != claim.id
            || source
                != serde_json::to_value(&claim.source).map_err(|_| StorageError::CorruptRecord)?
            || row.try_get::<String, _>("source_digest")? != hash(&source)?.as_str()
            || row.try_get::<String, _>("owner_id")? != claim.source.task.owner_id.as_str()
            || row.try_get::<String, _>("operation_id")?
                != claim.source.operation.operation_id.as_str()
            || row.try_get::<String, _>("status")? != "CLAIMED"
            || row.try_get::<Option<Json<Value>>, _>("receipt")?.is_some()
        {
            return Err(StorageError::CorruptRecord);
        }
        let recorded = sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND details ? 'mock_draft_execution_claim' ORDER BY sequence DESC LIMIT 1")
            .bind(claim.source.task.task_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::CorruptRecord)?;
        if recorded.try_get::<Json<Value>, _>("details")?.0["mock_draft_execution_claim"]
            != json!({"claim_id":claim.id,"source_digest":hash(&source)?})
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(())
    }

    fn receipt_from_runtime(
        &self,
        source: &Source,
        outcome: RuntimeOutcome,
    ) -> Result<HistoricalMockDraftReceipt, StorageError> {
        let RuntimeOutcome::Completed {
            task_id,
            results,
            state: TaskState::Completed,
        } = outcome
        else {
            return Err(StorageError::CorruptRecord);
        };
        let [result] = results.as_slice() else {
            return Err(StorageError::CorruptRecord);
        };
        if task_id != source.task.task_id
            || result.operation_id != source.operation.operation_id
            || result.tool_name.as_str() != MOCK_DRAFT_CREATE_TOOL
            || result.data["task_id"] != json!(task_id)
            || result.data["runtime_plan_version"] != json!(source.task.plan.plan_version)
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
            .and_then(|n| u32::try_from(n).ok())
            .filter(|v| *v > 0)
            .ok_or(StorageError::CorruptRecord)?;
        Ok(HistoricalMockDraftReceipt {
            draft_id,
            business_operation_id,
            draft_version,
            tool_result_ref: result.result_ref.clone(),
            executed_at: timestamp(self.clock.as_ref())?,
        })
    }

    async fn finish_mock_execution(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claim: &Claim,
        receipt: &HistoricalMockDraftReceipt,
    ) -> Result<(TaskRecord, OperationRecord), StorageError> {
        let mut operation = claim.source.operation.clone();
        // These are recorded historical transitions supported by the actual verified receipt.
        for event in [
            OperationEvent::GatesSatisfied,
            OperationEvent::SubmissionRecorded,
            OperationEvent::ExecutionSucceeded,
        ] {
            let old_revision = operation.revision;
            operation.state = operation
                .state
                .transition(event)
                .map_err(|_| StorageError::IllegalTransition)?;
            operation.revision = operation
                .revision
                .checked_add(1)
                .ok_or(StorageError::RevisionConflict)?;
            if event == OperationEvent::SubmissionRecorded {
                operation.business_operation_ref = Some(receipt.business_operation_id.clone());
            }
            let changed = sqlx::query("UPDATE operations SET state=$1,revision=$2,business_operation_ref=$3,updated_at=clock_timestamp() WHERE task_id=$4 AND operation_id=$5 AND revision=$6")
                .bind(encode(operation.state)?).bind(db_version(operation.revision)?).bind(operation.business_operation_ref.as_ref().map(|r| r.as_str()))
                .bind(operation.task_id.as_str()).bind(operation.operation_id.as_str()).bind(db_version(old_revision)?).execute(&mut **tx).await?;
            if changed.rows_affected() != 1 {
                return Err(StorageError::RevisionConflict);
            }
            audit(
                tx,
                &claim.source.task,
                Some(&operation),
                PersistentAuditKind::OperationTransitioned,
                json!({"event":event,"mock_draft_claim":claim.id}),
            )
            .await?;
        }
        let mut task = claim.source.task.clone();
        task.state = task
            .state
            .transition(TaskEvent::Complete)
            .map_err(|_| StorageError::IllegalTransition)?;
        task.revision = task
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        task.completed_nodes = vec![operation.node_id.clone()];
        task.checkpoint = RecoveryPoint::Finished;
        validate_checkpoint(&task, &[operation.clone()])?;
        update_task(tx, &task, claim.source.task.revision).await?;
        audit(
            tx,
            &task,
            Some(&operation),
            PersistentAuditKind::CheckpointSaved,
            json!({"mock_draft_execution_completed":{"claim_id":claim.id,"receipt":receipt}}),
        )
        .await?;
        sqlx::query("UPDATE mock_draft_execution_claims SET status='COMPLETED',receipt=$1 WHERE task_id=$2 AND claim_id=$3 AND status='CLAIMED'")
            .bind(Json(receipt)).bind(task.task_id.as_str()).bind(&claim.id).execute(&mut **tx).await?;
        Ok((task, operation))
    }

    pub async fn query_mock_draft_execution(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
    ) -> Result<MockDraftExecutionView, StorageError> {
        self.read_mock_execution(session, task, operation, None)
            .await?
            .ok_or(StorageError::NotFound)
    }

    async fn read_mock_execution(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation_id: &OperationId,
        expected: Option<(&ConfirmationRef, &MockDraftExecutionVersion)>,
    ) -> Result<Option<MockDraftExecutionView>, StorageError> {
        let identity = self.identity(session)?;
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task(&mut tx, &identity.user_id, task_id).await?;
        let ops = operations(&mut tx, task_id).await?;
        let row = sqlx::query("SELECT * FROM mock_draft_execution_claims WHERE task_id=$1 AND operation_id=$2 AND owner_id=$3 FOR UPDATE")
            .bind(task_id.as_str()).bind(operation_id.as_str()).bind(identity.user_id.as_str()).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let source_json = row.try_get::<Json<Value>, _>("source")?.0;
        let source: StoredSource =
            serde_json::from_value(source_json.clone()).map_err(|_| StorageError::CorruptRecord)?;
        if source.task.task_id != *task_id
            || source.operation.operation_id != *operation_id
            || source.task.owner_id != identity.user_id
            || row.try_get::<String, _>("source_digest")? != hash(&source_json)?.as_str()
            || source.operation.task_id != *task_id
            || source.session_binding.user_id != identity.user_id
            || source.session_binding.binding_version == 0
            || source.expires_at <= source.checked_at
            || !matches!(source.task.plan.nodes.as_slice(), [PlanNode::Tool { arguments, tool, id, depends_on }]
                if *arguments == source.parameters && tool.as_str() == MOCK_DRAFT_CREATE_TOOL
                && *id == source.operation.node_id && depends_on.is_empty())
            || source.pre_tool.is_null()
            || source.pre_auth.is_null()
            || source.session_digest.as_str().is_empty()
        {
            return Err(StorageError::CorruptRecord);
        }
        if let Some((confirmation, versions)) = expected {
            if source.confirmation["confirmation_ref"] != json!(confirmation) {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            versions.check(&source.task, &source.operation)?;
        }
        let [operation] = ops.as_slice() else {
            return Err(StorageError::CorruptRecord);
        };
        let original = sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND details ? 'mock_draft_execution_claim' ORDER BY sequence DESC LIMIT 1")
            .bind(task_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::CorruptRecord)?;
        if original.try_get::<Json<Value>, _>("details")?.0["mock_draft_execution_claim"]
            != json!({"claim_id":row.try_get::<String,_>("claim_id")?,"source_digest":hash(&source_json)?})
        {
            return Err(StorageError::CorruptRecord);
        }
        let receipt = row
            .try_get::<Option<Json<HistoricalMockDraftReceipt>>, _>("receipt")?
            .map(|r| r.0);
        match (row.try_get::<String, _>("status")?.as_str(), &receipt) {
            ("CLAIMED", None) if task == source.task && *operation == source.operation => {
                let recorded = sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND details ? 'mock_draft_execution_claim' ORDER BY sequence DESC LIMIT 1")
                    .bind(task_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::CorruptRecord)?;
                if recorded.try_get::<Json<Value>, _>("details")?.0["mock_draft_execution_claim"]
                    != json!({"claim_id":row.try_get::<String,_>("claim_id")?,"source_digest":hash(&source_json)?})
                {
                    return Err(StorageError::CorruptRecord);
                }
            }
            ("COMPLETED", Some(receipt)) => {
                let mut expected_task = source.task.clone();
                let mut expected_operation = source.operation.clone();
                for event in [
                    OperationEvent::GatesSatisfied,
                    OperationEvent::SubmissionRecorded,
                    OperationEvent::ExecutionSucceeded,
                ] {
                    expected_operation.state = expected_operation
                        .state
                        .transition(event)
                        .map_err(|_| StorageError::CorruptRecord)?;
                    expected_operation.revision = expected_operation
                        .revision
                        .checked_add(1)
                        .ok_or(StorageError::CorruptRecord)?;
                }
                expected_operation.business_operation_ref =
                    Some(receipt.business_operation_id.clone());
                expected_task.state = expected_task
                    .state
                    .transition(TaskEvent::Complete)
                    .map_err(|_| StorageError::CorruptRecord)?;
                expected_task.revision = expected_task
                    .revision
                    .checked_add(1)
                    .ok_or(StorageError::CorruptRecord)?;
                expected_task.checkpoint = RecoveryPoint::Finished;
                expected_task.completed_nodes = vec![expected_operation.node_id.clone()];
                if expected_task != task
                    || expected_operation != *operation
                    || receipt.draft_version == 0
                    || receipt.executed_at < source.checked_at
                    || receipt.executed_at >= source.expires_at
                {
                    return Err(StorageError::CorruptRecord);
                }
                for id in [
                    receipt.draft_id.as_str(),
                    receipt.business_operation_id.as_str(),
                    receipt.tool_result_ref.as_str(),
                ] {
                    valid_id(id)?;
                }
                validate_checkpoint(&task, &ops)?;
                let recorded = sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND details ? 'mock_draft_execution_completed' ORDER BY sequence DESC LIMIT 1")
                    .bind(task_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::CorruptRecord)?;
                if recorded.try_get::<Json<Value>, _>("details")?.0
                    ["mock_draft_execution_completed"]
                    != json!({"claim_id":row.try_get::<String,_>("claim_id")?,"receipt":receipt})
                {
                    return Err(StorageError::CorruptRecord);
                }
            }
            _ => return Err(StorageError::CorruptRecord),
        }
        self.guard(session, &identity)?;
        tx.commit().await?;
        Ok(Some(MockDraftExecutionView {
            task,
            operation: operation.clone(),
            receipt,
            replayed: true,
        }))
    }
}
