use crate::{AttemptId, BusinessOperationRef, ReasonCode, ToolResultRef};
use serde::{Deserialize, Serialize};

pub const EXECUTION_OBSERVATION_CONTRACT_VERSION: u16 = 1;

/// What the tool adapter can prove about a submission attempt.
///
/// `OutcomeUnknown` is deliberately distinct from failure: the external side
/// effect may have happened, so Runtime must reconcile instead of resubmitting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ExecutionObservation {
    NotSubmitted {
        attempt_id: AttemptId,
        reason_code: ReasonCode,
    },
    ResultReceived {
        attempt_id: AttemptId,
        business_operation_ref: BusinessOperationRef,
        tool_result_ref: ToolResultRef,
    },
    OutcomeUnknown {
        attempt_id: AttemptId,
        business_operation_ref: BusinessOperationRef,
        reason_code: ReasonCode,
    },
}

impl ExecutionObservation {
    /// Proves only that this attempt did not leave the process.
    /// Runtime must still re-evaluate gates before scheduling another attempt.
    pub fn submission_is_proven_absent(&self) -> bool {
        matches!(self, Self::NotSubmitted { .. })
    }

    pub fn requires_reconciliation(&self) -> bool {
        matches!(self, Self::OutcomeUnknown { .. })
    }

    pub fn business_operation_ref(&self) -> Option<&BusinessOperationRef> {
        match self {
            Self::NotSubmitted { .. } => None,
            Self::ResultReceived {
                business_operation_ref,
                ..
            }
            | Self::OutcomeUnknown {
                business_operation_ref,
                ..
            } => Some(business_operation_ref),
        }
    }
}

/// The result of comparing an execution observation with trusted business state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PostExecuteOutcome {
    VerifiedSucceeded,
    VerifiedFailed,
    Processing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostExecuteAssessment {
    pub contract_version: u16,
    pub outcome: PostExecuteOutcome,
    pub reason_code: ReasonCode,
}

impl PostExecuteAssessment {
    pub fn new(outcome: PostExecuteOutcome, reason_code: ReasonCode) -> Self {
        Self {
            contract_version: EXECUTION_OBSERVATION_CONTRACT_VERSION,
            outcome,
            reason_code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_outcome_requires_reconciliation_and_forbids_resubmission() {
        let observation = ExecutionObservation::OutcomeUnknown {
            attempt_id: AttemptId::parse("attempt_1").unwrap(),
            business_operation_ref: BusinessOperationRef::parse("bank_op_1").unwrap(),
            reason_code: ReasonCode::parse("UPSTREAM_TIMEOUT").unwrap(),
        };
        assert!(observation.requires_reconciliation());
        assert!(!observation.submission_is_proven_absent());
        assert!(observation.business_operation_ref().is_some());
    }

    #[test]
    fn only_proven_not_submitted_attempts_may_be_resubmitted() {
        let observation = ExecutionObservation::NotSubmitted {
            attempt_id: AttemptId::parse("attempt_1").unwrap(),
            reason_code: ReasonCode::parse("LOCAL_VALIDATION_FAILED").unwrap(),
        };
        assert!(observation.submission_is_proven_absent());
        assert!(!observation.requires_reconciliation());
    }
}
