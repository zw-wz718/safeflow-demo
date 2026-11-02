//! Checked backend projection of existing settlement evidence; no event writer or sender.
use super::*;
use crate::models::valid_id;
use crate::repository::guarded;
use safeflow_contracts::{EventId, ToolName, UserId};

/// Internal evidence, deliberately not Serialize/Deserialize/Debug. Public adapters
/// must select approved, redacted fields; this is not the audit receiver's DTO.
///
/// ```compile_fail
/// fn expose(source: &safeflow_storage::LifecycleAuditSource) {
///     serde_json::to_string(source).unwrap();
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct LifecycleAuditSource {
    pub event_id: EventId,
    pub sequence: i64,
    pub task_id: TaskId,
    pub operation_id: OperationId,
    pub node_id: NodeId,
    pub tool_name: ToolName,
    pub user_id: UserId,
    pub source_mode: &'static str,
    pub scope: &'static str,
    pub identity_source: &'static str,
    pub source_digest: ArgumentsHash,
    pub receipt_digest: ArgumentsHash,
    /// LifecycleReceipt.executed_at: capture after Runtime result verification.
    pub result_captured_at: chrono::DateTime<chrono::Utc>,
    /// audit_events.occurred_at: database insertion time, not business occurrence.
    pub source_recorded_at: chrono::DateTime<chrono::Utc>,
    pub business_occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    receipt: LifecycleReceipt,
}

impl LifecycleAuditSource {
    /// Verified original body for trusted backend adaptation only, never a wire response.
    pub fn receipt_for_backend(&self) -> &LifecycleReceipt {
        &self.receipt
    }
}

pub struct LifecycleAuditSources {
    pub task_id: TaskId,
    pub records: Vec<LifecycleAuditSource>,
    /// Durable claim with no verified settlement; never retry it to produce an event.
    pub unresolved_operations: Vec<OperationId>,
    /// No settled success evidence. Includes unexecuted/rejected actions, not bank failures.
    pub operations_without_source: Vec<OperationId>,
}

#[derive(Clone)]
pub struct LifecycleAuditSourceReader {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
}

impl LifecycleAuditSourceReader {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            repository,
            sessions,
        }
    }

    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if binding.binding_version == 0 || valid_id(binding.user_id.as_str()).is_err() {
            return Err(StorageError::Session);
        }
        Ok(binding)
    }

    /// Read persisted evidence without a Runtime, draft observer or in-memory business Store.
    /// Current read authority is checked independently of the original execution session.
    pub async fn read_task(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
    ) -> Result<LifecycleAuditSources, StorageError> {
        valid_id(task_id.as_str())?;
        let identity = self.identity(session)?;
        let guard = || {
            if self.identity(session)? != identity {
                return Err(StorageError::Session);
            }
            Ok(())
        };
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        let task = guarded(
            &guard,
            lock_task_for_mode(&mut tx, &identity.user_id, task_id, true),
        )
        .await?;
        let ops = guarded(&guard, operations(&mut tx, task_id)).await?;
        guarded(&guard, checked_history(&mut tx, &task, &ops)).await?;
        let claims = guarded(&guard, checked_claims(&mut tx, &task, &ops)).await?;
        let rows = guarded(&guard, sqlx::query(
            "SELECT e.*,c.source FROM audit_events e JOIN mock_draft_lifecycle_claims c ON c.task_id=e.task_id AND c.operation_id=e.operation_id WHERE e.task_id=$1 AND e.kind='CHECKPOINT_SAVED' AND e.details ? 'lifecycle_settled' ORDER BY e.sequence"
        ).bind(task_id.as_str()).fetch_all(&mut *tx)).await?;
        let mut records = Vec::new();
        for row in rows {
            let source = row.try_get::<Json<FrozenSource>, _>("source")?.0;
            let receipt = claims
                .iter()
                .find(|claim| claim.operation == source.operation_id)
                .and_then(|claim| claim.receipt.as_ref())
                .ok_or(StorageError::CorruptRecord)?;
            let original = source.operation()?;
            let sequence: i64 = row.try_get("sequence")?;
            let recorded_at: chrono::DateTime<chrono::Utc> = row.try_get("occurred_at")?;
            if sequence <= 0
                || recorded_at < chrono::DateTime::UNIX_EPOCH
                || row.try_get::<i64, _>("task_revision")?
                    != db_version(
                        source
                            .task
                            .revision
                            .checked_add(1)
                            .ok_or(StorageError::CorruptRecord)?,
                    )?
                || row.try_get::<Option<i64>, _>("operation_revision")?
                    != Some(db_version(
                        original
                            .revision
                            .checked_add(3)
                            .ok_or(StorageError::CorruptRecord)?,
                    )?)
            {
                return Err(StorageError::CorruptRecord);
            }
            records.push(LifecycleAuditSource {
                event_id: crate::repository::decode(&row, "event_id")?,
                sequence,
                task_id: task_id.clone(),
                operation_id: source.operation_id.clone(),
                node_id: original.node_id.clone(),
                tool_name: receipt.result.tool_name.clone(),
                user_id: source.session_user.clone(),
                source_mode: SOURCE_MODE,
                scope: "MOCK_DRAFT_LIFECYCLE_ONLY",
                identity_source: "VERSIONED_SERVER_SESSION_IN_SIMULATION_SCOPE",
                source_digest: hash(&source)?,
                receipt_digest: hash(receipt)?,
                result_captured_at: receipt.executed_at,
                source_recorded_at: recorded_at,
                business_occurred_at: None,
                receipt: receipt.clone(),
            });
        }
        if records.len()
            != claims
                .iter()
                .filter(|claim| claim.receipt.is_some())
                .count()
        {
            return Err(StorageError::CorruptRecord);
        }
        let unresolved_operations = claims
            .iter()
            .filter(|claim| claim.receipt.is_none())
            .map(|claim| claim.operation.clone())
            .collect();
        let operations_without_source = ops
            .iter()
            .filter(|op| {
                !claims
                    .iter()
                    .any(|claim| claim.operation == op.operation_id)
            })
            .map(|op| op.operation_id.clone())
            .collect();
        guarded(&guard, tx.commit()).await?;
        Ok(LifecycleAuditSources {
            task_id: task_id.clone(),
            records,
            unresolved_operations,
            operations_without_source,
        })
    }
}
