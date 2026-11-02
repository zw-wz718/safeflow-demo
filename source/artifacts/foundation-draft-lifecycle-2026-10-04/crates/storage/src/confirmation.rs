use crate::models::{db_version, hash, valid_id, validate_checkpoint, validate_plan};
use crate::{OperationRecord, PgRepository, StorageError, TaskRecord};
use chrono::{DateTime, Utc};
use safeflow_contracts::{
    ArgumentsHash, ConfirmationRef, NodeId, OperationId, OperationState, PlanNode, SessionRef,
    TaskId, TaskState, ToolName, UserId,
};
use safeflow_runtime::{SessionBinding, SessionResolver};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const SERVER_CONFIRMATION_CONTRACT_VERSION: u16 = 1;

/// Required trusted server configuration. No product TTL default is defined.
#[derive(Clone)]
pub struct ConfirmationConfig {
    pub(crate) ttl: chrono::Duration,
}

impl ConfirmationConfig {
    pub fn new(ttl: Duration) -> Result<Self, StorageError> {
        let ttl = chrono::Duration::from_std(ttl)
            .map_err(|_| StorageError::Invalid("confirmation TTL out of range"))?;
        if ttl <= chrono::Duration::zero() || ttl.num_microseconds().is_none() {
            return Err(StorageError::Invalid("confirmation TTL must be positive"));
        }
        // PostgreSQL timestamps have microsecond precision.
        if ttl.subsec_nanos() % 1_000 != 0 {
            return Err(StorageError::Invalid(
                "confirmation TTL requires microsecond precision",
            ));
        }
        Ok(Self { ttl })
    }
}

/// Trusted backend dependency; never take time from a request or LLM.
pub trait ConfirmationClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

struct ServerClock;
impl ConfirmationClock for ServerClock {
    fn now(&self) -> DateTime<Utc> {
        SystemTime::now().into()
    }
}

/// Comparisons against what was displayed, not independently supplied authority.
/// This is the stored foundation snapshot, NOT OperationFingerprint v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfirmationBinding {
    pub user_id: UserId,
    pub task_id: TaskId,
    pub operation_id: OperationId,
    pub node_id: NodeId,
    pub tool_name: ToolName,
    pub task_revision: u64,
    pub plan_version: u32,
    pub operation_revision: u64,
    pub facts_version: u64,
    pub arguments_hash: ArgumentsHash,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_anchor: Option<ArgumentsHash>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConfirmationStatus {
    Prepared,
    Confirmed,
    Revoked,
}

/// Data only. No session secret/digest, AuthGrant or Permit is returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfirmationRecord {
    pub contract_version: u16,
    pub confirmation_ref: ConfirmationRef,
    pub binding: ConfirmationBinding,
    pub status: ConfirmationStatus,
    pub prepared_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub(crate) struct ConfirmationSession {
    pub binding: SessionBinding,
    pub digest: ArgumentsHash,
}

/// Only wire in a trusted backend explicit-user-action handler. It must establish
/// that the user saw this binding; possession of an ID/echoed binding is not proof.
/// No method is an LLM Tool, HTTP command, auth grant or execution entry point.
#[derive(Clone)]
pub struct ConfirmationService {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
    config: ConfirmationConfig,
    clock: Arc<dyn ConfirmationClock>,
    lifecycle: bool,
}

impl ConfirmationService {
    pub(crate) fn new_locked_record(
        &self,
        binding: ConfirmationBinding,
    ) -> Result<ConfirmationRecord, StorageError> {
        let now = timestamp(self.clock.as_ref())?;
        let expiry = now
            .checked_add_signed(self.config.ttl)
            .ok_or(StorageError::ConfirmationExpired)?;
        Ok(ConfirmationRecord {
            contract_version: SERVER_CONFIRMATION_CONTRACT_VERSION,
            confirmation_ref: ConfirmationRef::parse(format!(
                "confirmation_{}",
                uuid::Uuid::new_v4()
            ))
            .expect("UUID"),
            binding,
            status: ConfirmationStatus::Prepared,
            prepared_at: now,
            expires_at: chrono::DateTime::from_timestamp_micros(expiry.timestamp_micros())
                .ok_or(StorageError::ConfirmationExpired)?,
            confirmed_at: None,
            revoked_at: None,
        })
    }

    pub(crate) fn sessions_for_backend(&self) -> Arc<dyn SessionResolver> {
        self.sessions.clone()
    }
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: ConfirmationConfig,
    ) -> Self {
        Self::for_trusted_backend_with_clock(repository, sessions, config, Arc::new(ServerClock))
    }

    /// Explicit clock injection for isolated foundation tests, not client input.
    pub fn for_trusted_backend_with_clock(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: ConfirmationConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Self {
        Self {
            repository,
            sessions,
            config,
            clock,
            lifecycle: false,
        }
    }

    pub(crate) fn for_lifecycle_backend_with_clock(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: ConfirmationConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Self {
        let mut service = Self::for_trusted_backend_with_clock(repository, sessions, config, clock);
        service.lifecycle = true;
        service
    }

    pub(crate) fn session(
        &self,
        session: &SessionRef,
    ) -> Result<ConfirmationSession, StorageError> {
        valid_id(session.as_str())?;
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(binding.user_id.as_str())?;
        db_version(binding.binding_version)?;
        let digest = hash(&("safeflow-confirmation-session:v1", session.as_str()))?;
        Ok(ConfirmationSession { binding, digest })
    }

    pub(crate) fn guard(
        &self,
        session: &SessionRef,
        original: &ConfirmationSession,
    ) -> Result<(), StorageError> {
        if self.session(session)?.binding != original.binding {
            return Err(StorageError::Session);
        }
        Ok(())
    }

    // Internal absolute bounds supplied only from an immutable member binding.
    pub(crate) async fn prepare_capped(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation_id: &OperationId,
        prepared: DateTime<Utc>,
        expires: DateTime<Utc>,
    ) -> Result<ConfirmationRecord, StorageError> {
        let identity = self.session(session)?;
        self.repository
            .prepare_confirmation(
                &identity,
                task_id,
                operation_id,
                &self.config,
                self.clock.as_ref(),
                &|| self.guard(session, &identity),
                self.lifecycle,
                Some((prepared, expires)),
            )
            .await
    }
    pub async fn prepare(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation_id: &OperationId,
    ) -> Result<ConfirmationRecord, StorageError> {
        let identity = self.session(session)?;
        self.repository
            .prepare_confirmation(
                &identity,
                task_id,
                operation_id,
                &self.config,
                self.clock.as_ref(),
                &|| self.guard(session, &identity),
                self.lifecycle,
                None,
            )
            .await
    }

    pub async fn read(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        reference: &ConfirmationRef,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.access(session, task_id, reference, ConfirmationAccess::Read, None)
            .await
    }

    pub async fn record_explicit_confirmation(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        reference: &ConfirmationRef,
        displayed: &ConfirmationBinding,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.access(
            session,
            task_id,
            reference,
            ConfirmationAccess::Confirm,
            Some(displayed),
        )
        .await
    }

    pub(crate) async fn confirm_on_connection(
        &self, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, session: &SessionRef,
        task: &TaskId, reference: &ConfirmationRef, displayed: &ConfirmationBinding,
    ) -> Result<ConfirmationRecord, StorageError> {
        let identity = self.session(session)?;
        self.repository.access_confirmation_on_connection(tx, &identity, task, reference,
            ConfirmationAccess::Confirm, Some(displayed), self.clock.as_ref(),
            &|| self.guard(session, &identity), self.lifecycle).await
    }
    pub async fn verify(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        reference: &ConfirmationRef,
        expected: &ConfirmationBinding,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.access(
            session,
            task_id,
            reference,
            ConfirmationAccess::Verify,
            Some(expected),
        )
        .await
    }

    /// Cleanup only: original session can revoke an expired/stale/terminal record.
    /// A replay returns the original revoked record without another audit event.
    pub async fn revoke(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        reference: &ConfirmationRef,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.access(
            session,
            task_id,
            reference,
            ConfirmationAccess::Revoke,
            None,
        )
        .await
    }

    /// Public rejection is allowed only before confirmation; cleanup revoke stays separate.
    pub async fn reject_prepared(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        reference: &ConfirmationRef,
        displayed: &ConfirmationBinding,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.access(
            session,
            task_id,
            reference,
            ConfirmationAccess::RejectPrepared,
            Some(displayed),
        )
        .await
    }

    async fn access(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        reference: &ConfirmationRef,
        action: ConfirmationAccess,
        expected: Option<&ConfirmationBinding>,
    ) -> Result<ConfirmationRecord, StorageError> {
        let identity = self.session(session)?;
        self.repository
            .access_confirmation(
                &identity,
                task_id,
                reference,
                action,
                expected,
                self.clock.as_ref(),
                &|| self.guard(session, &identity),
                self.lifecycle,
            )
            .await
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ConfirmationAccess {
    Read,
    Confirm,
    Verify,
    Revoke,
    RejectPrepared,
}

pub(crate) fn current_binding(
    task: &TaskRecord,
    operations: &[OperationRecord],
    operation_id: &OperationId,
) -> Result<ConfirmationBinding, StorageError> {
    validate_plan(&task.plan)?;
    validate_checkpoint(task, operations)?;
    if task.state.is_terminal() {
        return Err(StorageError::Terminal);
    }
    if !matches!(task.state, TaskState::Running | TaskState::WaitingUser)
        || operations.iter().any(|op| {
            matches!(
                op.state,
                OperationState::Submitted | OperationState::Processing
            )
        })
    {
        return Err(StorageError::ReconcileRequired);
    }
    let op = operations
        .iter()
        .find(|op| &op.operation_id == operation_id)
        .ok_or(StorageError::NotFound)?;
    if op.state.is_terminal() {
        return Err(StorageError::Terminal);
    }
    if !matches!(op.state, OperationState::Draft | OperationState::Ready)
        || op.business_operation_ref.is_some()
    {
        return Err(StorageError::ReconcileRequired);
    }
    if op.plan_version != task.plan.plan_version {
        return Err(StorageError::StalePlan);
    }
    if op.facts_version != task.facts_version {
        return Err(StorageError::FactsConflict);
    }
    let node = task
        .plan
        .nodes
        .iter()
        .find(|n| n.id() == &op.node_id)
        .ok_or(StorageError::CorruptRecord)?;
    let PlanNode::Tool {
        tool, arguments, ..
    } = node
    else {
        return Err(StorageError::CorruptRecord);
    };
    valid_id(tool.as_str())?;
    reject_floats(arguments)?;
    if hash(arguments)? != op.arguments_hash {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    Ok(ConfirmationBinding {
        user_id: task.owner_id.clone(),
        task_id: task.task_id.clone(),
        operation_id: op.operation_id.clone(),
        node_id: op.node_id.clone(),
        tool_name: tool.clone(),
        task_revision: task.revision,
        plan_version: task.plan.plan_version,
        operation_revision: op.revision,
        facts_version: task.facts_version,
        arguments_hash: op.arguments_hash.clone(),
        lifecycle_anchor: None,
    })
}

pub(crate) fn reject_floats(value: &serde_json::Value) -> Result<(), StorageError> {
    match value {
        serde_json::Value::Number(n) if !n.is_i64() && !n.is_u64() => Err(StorageError::Invalid(
            "confirmation arguments contain floats",
        )),
        serde_json::Value::Array(v) => v.iter().try_for_each(reject_floats),
        serde_json::Value::Object(v) => v.values().try_for_each(reject_floats),
        _ => Ok(()),
    }
}

pub(crate) fn timestamp(clock: &dyn ConfirmationClock) -> Result<DateTime<Utc>, StorageError> {
    let value = clock.now();
    DateTime::from_timestamp_micros(value.timestamp_micros())
        .ok_or(StorageError::Invalid("server time out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ttl_requires_explicit_positive_microsecond_precision() {
        assert!(ConfirmationConfig::new(Duration::ZERO).is_err());
        assert!(ConfirmationConfig::new(Duration::from_nanos(1)).is_err());
        assert!(ConfirmationConfig::new(Duration::MAX).is_err());
        assert!(ConfirmationConfig::new(Duration::from_secs(60)).is_ok());
    }
    #[test]
    fn nested_floats_are_rejected_without_reinterpreting_money() {
        assert!(reject_floats(&serde_json::json!({"amount":"200.00", "count":1})).is_ok());
        assert!(reject_floats(&serde_json::json!({"nested":[{"amount":200.0}]})).is_err());
    }
}
