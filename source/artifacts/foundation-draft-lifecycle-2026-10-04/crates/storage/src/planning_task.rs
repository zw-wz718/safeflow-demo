//! Trusted proposal intake -> initial persistent task only. No execution or business mutation.
use crate::models::db_version;
use crate::{PgRepository, StorageError, TaskRecord};
use safeflow_contracts::{PlanningResult, SessionRef, TaskId, UnderstandingDecision};
use safeflow_runtime::{
    LlmGateway, PlanningIntakeError, PlanningIntakeService, PlanningPersistenceReview,
    PreflightInputContract, Runtime,
};
use std::{fmt, sync::Arc};

/// A saved initial task is not a running Tool, recovery result, security decision or Permit.
pub enum PlanningTaskCreationOutcome {
    TaskCreated(TaskRecord),
    /// Untrusted model text; no task or database progress was created.
    Clarification(PlanningResult),
}

pub struct PlanningTaskCreationService<'a> {
    repository: PgRepository,
    intake: PlanningIntakeService<'a>,
}
impl<'a> PlanningTaskCreationService<'a> {
    pub fn for_trusted_backend(
        repository: PgRepository,
        runtime: &'a Runtime,
        gateway: Option<Arc<dyn LlmGateway>>,
        contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, PlanningTaskCreationError> {
        Ok(Self {
            repository,
            intake: PlanningIntakeService::for_registered_backend(runtime, gateway, contracts)?,
        })
    }

    pub async fn accept_json_and_create(
        &self,
        session: &SessionRef,
        expected_task: &TaskId,
        expected_plan_version: u32,
        facts_version: u64,
        raw: &[u8],
    ) -> Result<PlanningTaskCreationOutcome, PlanningTaskCreationError> {
        db_version(facts_version)?;
        let review = self
            .intake
            .accept_json_for_persistence(session, expected_task, expected_plan_version, raw)
            .await?;
        self.create(review, facts_version).await
    }

    pub async fn plan_and_create(
        &self,
        session: &SessionRef,
        expected_task: &TaskId,
        expected_plan_version: u32,
        facts_version: u64,
        user_input: &str,
    ) -> Result<PlanningTaskCreationOutcome, PlanningTaskCreationError> {
        db_version(facts_version)?;
        let review = self
            .intake
            .plan_for_persistence(session, expected_task, expected_plan_version, user_input)
            .await?;
        self.create(review, facts_version).await
    }

    pub async fn create(
        &self,
        review: PlanningPersistenceReview<'_>,
        facts_version: u64,
    ) -> Result<PlanningTaskCreationOutcome, PlanningTaskCreationError> {
        self.intake.check_persistence_review(&review)?;
        if review.planning_result().understanding.decision == UnderstandingDecision::Clarify {
            let result = review.planning_result().clone();
            self.intake.check_persistence_review(&review)?;
            return Ok(PlanningTaskCreationOutcome::Clarification(result));
        }
        let plan = review
            .planning_result()
            .plan
            .clone()
            .ok_or(PlanningIntakeError::InvalidResult)?;
        let guard = || {
            self.intake
                .check_persistence_review(&review)
                .map_err(|error| match error {
                    PlanningIntakeError::Session => StorageError::Session,
                    _ => StorageError::Invalid("planning intake registration no longer current"),
                })
        };
        let task = self
            .repository
            .create_planning_task(&review, plan, facts_version, &guard)
            .await?;
        self.intake.check_persistence_review(&review)?;
        Ok(PlanningTaskCreationOutcome::TaskCreated(task))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningTaskCreationError {
    Intake(PlanningIntakeError),
    Storage(StorageError),
}
impl From<PlanningIntakeError> for PlanningTaskCreationError {
    fn from(error: PlanningIntakeError) -> Self {
        Self::Intake(error)
    }
}
impl From<StorageError> for PlanningTaskCreationError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl fmt::Display for PlanningTaskCreationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Intake(error) => write!(f, "{error}"),
            Self::Storage(error) => write!(f, "{error}"),
        }
    }
}
impl std::error::Error for PlanningTaskCreationError {}

#[cfg(test)]
#[path = "planning_task_tests.rs"]
mod tests;
