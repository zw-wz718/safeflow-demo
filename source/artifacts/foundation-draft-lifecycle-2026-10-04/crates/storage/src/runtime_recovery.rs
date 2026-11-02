use crate::models::{db_version, hash, validate_plan};
use crate::{
    PersistentAuditKind, PgRepository, RecoveryDirective, RecoverySnapshot, StorageError,
    TaskRecord,
};
use safeflow_contracts::{AgentPlan, ArgumentsHash, SessionRef, TaskId, TaskState};
use safeflow_runtime::{Runtime, RuntimeError, READ_ONLY_WAIT_CONTRACT_VERSION};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeWaitEvidence {
    pub contract_version: u16,
    pub bindings_hash: ArgumentsHash,
    pub results_hash: ArgumentsHash,
    pub task_hash: ArgumentsHash,
}

/// Expected versions are comparisons, not authoritative state or execution permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WaitingTaskVersion {
    pub task_revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
}

impl WaitingTaskVersion {
    pub fn of(task: &TaskRecord) -> Self {
        Self {
            task_revision: task.revision,
            plan_version: task.plan.plan_version,
            facts_version: task.facts_version,
        }
    }

    fn check(&self, task: &TaskRecord) -> Result<(), StorageError> {
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeWaitingTask {
    pub task: TaskRecord,
    pub revalidated_facts_version: u64,
}

/// Trusted backend orchestration. The Runtime is the sole source of session identity.
/// No caller can supply owner, completed progress, a ToolResult, state or a Permit.
pub struct RuntimeRecoveryService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
}

impl<'a> RuntimeRecoveryService<'a> {
    pub fn for_trusted_backend(repository: PgRepository, runtime: &'a Runtime) -> Self {
        Self {
            repository,
            runtime,
        }
    }

    pub async fn execute_until_wait(
        &self,
        session: SessionRef,
        plan: AgentPlan,
        facts_version: u64,
    ) -> Result<RuntimeWaitingTask, RuntimeRecoveryError> {
        validate_plan(&plan)?;
        db_version(facts_version)?;
        let review = self
            .runtime
            .review_read_only_plan(plan, session, facts_version)
            .await?;
        let guard = || {
            self.runtime
                .check_read_only_review(&review)
                .map_err(|_| StorageError::Invalid("Runtime review no longer current"))
        };
        let initial = self
            .repository
            .create_runtime_task(review.owner(), review.plan().clone(), facts_version, &guard)
            .await?;
        self.runtime.check_read_only_review(&review)?;
        let verified = self
            .runtime
            .execute_reviewed_read_only_wait(&review)
            .await?;
        let saved = self
            .repository
            .checkpoint_runtime_wait(&initial, &verified, &guard)
            .await?;
        self.runtime.check_read_only_review(&review)?;
        let current = self
            .repository
            .recover(review.owner(), &saved.task_id, &guard)
            .await?;
        self.runtime.check_read_only_review(&review)?;
        if current.task != saved || !current.operations.is_empty() {
            return Err(StorageError::RevisionConflict.into());
        }
        Ok(RuntimeWaitingTask {
            task: saved,
            revalidated_facts_version: review.facts_version(),
        })
    }

    /// Reads and rechecks only. This method never calls execute or verify_execution.
    pub async fn recover_wait(
        &self,
        session: SessionRef,
        task_id: &TaskId,
        expected: WaitingTaskVersion,
    ) -> Result<RuntimeWaitingTask, RuntimeRecoveryError> {
        let binding = self.runtime.session_binding_for_backend(&session)?;
        let owner = binding.user_id.clone();
        let session_guard = || {
            if self
                .runtime
                .session_binding_for_backend(&session)
                .map_err(|_| StorageError::Session)?
                != binding
            {
                return Err(StorageError::Session);
            }
            Ok(())
        };
        let snapshot = self
            .repository
            .recover(&owner, task_id, &session_guard)
            .await?;
        if self.runtime.session_binding_for_backend(&session)? != binding {
            return Err(RuntimeError::SessionChanged.into());
        }
        if snapshot.task.state != TaskState::WaitingUser {
            return Err(RuntimeRecoveryError::NotWaiting {
                directive: snapshot.directive,
            });
        }
        expected.check(&snapshot.task)?;
        if !snapshot.operations.is_empty() {
            return Err(RuntimeRecoveryError::ReviewRequired(
                "operation records need separate reconciliation",
            ));
        }
        validate_plan(&snapshot.task.plan)?;
        let evidence = wait_evidence(&snapshot)?;
        let review = self
            .runtime
            .review_read_only_plan(
                snapshot.task.plan.clone(),
                session.clone(),
                snapshot.task.facts_version,
            )
            .await?;
        if review.owner() != &owner
            || self.runtime.session_binding_for_backend(&session)? != binding
        {
            return Err(RuntimeError::SessionChanged.into());
        }
        if review.bindings_hash() != &evidence.bindings_hash {
            return Err(RuntimeRecoveryError::ReviewRequired(
                "tool/policy/profile registrations changed",
            ));
        }
        self.runtime.revalidate_read_only_wait(&review).await?;
        // Policies may await bank reads. Re-read after them to catch concurrent replanning.
        let current = self
            .repository
            .recover(&owner, task_id, &session_guard)
            .await?;
        self.runtime.check_read_only_review(&review)?;
        if current.task != snapshot.task
            || current.operations != snapshot.operations
            || wait_evidence(&current)? != evidence
        {
            return Err(StorageError::RevisionConflict.into());
        }
        Ok(RuntimeWaitingTask {
            task: snapshot.task,
            revalidated_facts_version: review.facts_version(),
        })
    }
}

fn wait_evidence(snapshot: &RecoverySnapshot) -> Result<RuntimeWaitEvidence, RuntimeRecoveryError> {
    let event = snapshot
        .audit
        .iter()
        .rev()
        .find(|event| {
            event.kind == PersistentAuditKind::CheckpointSaved
                && event.task_revision == snapshot.task.revision
        })
        .ok_or(RuntimeRecoveryError::ReviewRequired(
            "waiting point has no Runtime checkpoint evidence",
        ))?;
    let evidence: RuntimeWaitEvidence =
        serde_json::from_value(event.details["runtime_wait"].clone()).map_err(|_| {
            RuntimeRecoveryError::ReviewRequired("waiting point was not recorded by Runtime")
        })?;
    if evidence.contract_version != READ_ONLY_WAIT_CONTRACT_VERSION
        || evidence.task_hash != hash(&snapshot.task)?
    {
        return Err(RuntimeRecoveryError::ReviewRequired(
            "Runtime checkpoint evidence does not match task",
        ));
    }
    Ok(evidence)
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeRecoveryError {
    Runtime(RuntimeError),
    Storage(StorageError),
    ReviewRequired(&'static str),
    NotWaiting { directive: RecoveryDirective },
}

impl From<RuntimeError> for RuntimeRecoveryError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}
impl From<StorageError> for RuntimeRecoveryError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl fmt::Display for RuntimeRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => error.fmt(f), Self::Storage(error) => error.fmt(f),
            Self::ReviewRequired(reason) => write!(f, "Runtime waiting point requires review: {reason}"),
            Self::NotWaiting { .. } => f.write_str("task is not at a recoverable Runtime user wait; inspect original recovery directive"),
        }
    }
}
impl std::error::Error for RuntimeRecoveryError {}
