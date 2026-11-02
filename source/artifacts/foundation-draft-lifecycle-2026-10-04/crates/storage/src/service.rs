use crate::{
    CheckpointChange, OperationChange, OperationRecord, PgRepository, RecoverySnapshot,
    StorageError, TaskRecord,
};
use safeflow_contracts::{AgentPlan, NodeId, OperationId, SessionRef, TaskId};
use safeflow_runtime::{SessionBinding, SessionResolver};
use std::sync::Arc;

/// Trusted backend assembly only, never an HTTP/LLM command contract.
/// Versioned identity is rechecked inside transactions. Owner/state are not input fields.
#[derive(Clone)]
pub struct PersistenceService {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
}

impl PersistenceService {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            repository,
            sessions,
        }
    }

    fn binding(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if binding.binding_version == 0 {
            return Err(StorageError::Session);
        }
        Ok(binding)
    }

    fn check_binding(
        &self,
        session: &SessionRef,
        expected: &SessionBinding,
    ) -> Result<(), StorageError> {
        if self.binding(session)? != *expected {
            return Err(StorageError::Session);
        }
        Ok(())
    }

    pub async fn create_task(
        &self,
        session: &SessionRef,
        plan: AgentPlan,
        facts_version: u64,
    ) -> Result<TaskRecord, StorageError> {
        let binding = self.binding(session)?;
        let guard = || self.check_binding(session, &binding);
        self.repository
            .create_task(&binding.user_id, plan, facts_version, &guard)
            .await
    }

    pub async fn recover(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
    ) -> Result<RecoverySnapshot, StorageError> {
        let binding = self.binding(session)?;
        let guard = || self.check_binding(session, &binding);
        self.repository
            .recover(&binding.user_id, task_id, &guard)
            .await
    }

    pub async fn save_checkpoint(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        change: CheckpointChange,
    ) -> Result<TaskRecord, StorageError> {
        let binding = self.binding(session)?;
        let guard = || self.check_binding(session, &binding);
        self.repository
            .checkpoint(&binding.user_id, task_id, change, &guard)
            .await
    }

    pub async fn replace_plan(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected_revision: u64,
        plan: AgentPlan,
        facts_version: u64,
    ) -> Result<TaskRecord, StorageError> {
        let binding = self.binding(session)?;
        let guard = || self.check_binding(session, &binding);
        self.repository
            .replace_plan(
                &binding.user_id,
                task_id,
                expected_revision,
                plan,
                facts_version,
                &guard,
            )
            .await
    }

    pub async fn create_operation(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected_task_revision: u64,
        operation_id: OperationId,
        node_id: NodeId,
        plan_version: u32,
    ) -> Result<OperationRecord, StorageError> {
        let binding = self.binding(session)?;
        let guard = || self.check_binding(session, &binding);
        self.repository
            .create_operation(
                &binding.user_id,
                task_id,
                expected_task_revision,
                operation_id,
                node_id,
                plan_version,
                &guard,
            )
            .await
    }

    /// Records trusted backend observations only; does not satisfy/consume any gates or Permit.
    pub async fn record_operation_event(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation_id: &OperationId,
        change: OperationChange,
    ) -> Result<OperationRecord, StorageError> {
        let binding = self.binding(session)?;
        let guard = || self.check_binding(session, &binding);
        self.repository
            .operation_event(&binding.user_id, task_id, operation_id, change, &guard)
            .await
    }
}
