use serde::{Deserialize, Serialize};
use std::fmt;

pub const STATE_MACHINE_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Running,
    WaitingUser,
    WaitingExternal,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskEvent {
    AwaitUser,
    UserResponded,
    AwaitExternal,
    ExternalResolved,
    Complete,
    Fail,
    Cancel,
}

impl TaskState {
    pub fn transition(self, event: TaskEvent) -> Result<Self, StateTransitionError> {
        let next = match (self, event) {
            (Self::Running, TaskEvent::AwaitUser) => Self::WaitingUser,
            (Self::WaitingUser, TaskEvent::UserResponded) => Self::Running,
            (Self::Running, TaskEvent::AwaitExternal) => Self::WaitingExternal,
            (Self::WaitingExternal, TaskEvent::ExternalResolved) => Self::Running,
            (Self::Running, TaskEvent::Complete) => Self::Completed,
            (Self::Running | Self::WaitingUser | Self::WaitingExternal, TaskEvent::Fail) => {
                Self::Failed
            }
            (Self::Running | Self::WaitingUser | Self::WaitingExternal, TaskEvent::Cancel) => {
                Self::Cancelled
            }
            _ => return Err(StateTransitionError::task(self, event)),
        };
        Ok(next)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationState {
    Draft,
    Ready,
    Submitted,
    Processing,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationEvent {
    GatesSatisfied,
    SubmissionRecorded,
    OutcomeUnknown,
    ExecutionSucceeded,
    ExecutionFailed,
    StatusConfirmedSucceeded,
    StatusConfirmedFailed,
    Cancel,
}

impl OperationState {
    pub fn transition(self, event: OperationEvent) -> Result<Self, StateTransitionError> {
        let next = match (self, event) {
            (Self::Draft, OperationEvent::GatesSatisfied) => Self::Ready,
            (Self::Ready, OperationEvent::SubmissionRecorded) => Self::Submitted,
            (Self::Submitted, OperationEvent::OutcomeUnknown) => Self::Processing,
            (Self::Submitted, OperationEvent::ExecutionSucceeded) => Self::Succeeded,
            (Self::Submitted, OperationEvent::ExecutionFailed) => Self::Failed,
            (Self::Processing, OperationEvent::StatusConfirmedSucceeded) => Self::Succeeded,
            (Self::Processing, OperationEvent::StatusConfirmedFailed) => Self::Failed,
            (Self::Draft | Self::Ready, OperationEvent::Cancel) => Self::Cancelled,
            _ => return Err(StateTransitionError::operation(self, event)),
        };
        Ok(next)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub fn may_submit(self) -> bool {
        self == Self::Ready
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SecurityOutcome {
    Allow,
    NeedsInput,
    ReadyToConfirm,
    WaitingAuth,
    Blocked,
    ReadyToExecute,
}

impl SecurityOutcome {
    pub fn allows_write_execution(self) -> bool {
        self == Self::ReadyToExecute
    }

    pub fn requires_user_interaction(self) -> bool {
        matches!(
            self,
            Self::NeedsInput | Self::ReadyToConfirm | Self::WaitingAuth
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransitionError {
    pub machine: &'static str,
    pub from: String,
    pub event: String,
}

impl StateTransitionError {
    fn task(from: TaskState, event: TaskEvent) -> Self {
        Self {
            machine: "task",
            from: format!("{from:?}"),
            event: format!("{event:?}"),
        }
    }

    fn operation(from: OperationState, event: OperationEvent) -> Self {
        Self {
            machine: "operation",
            from: format!("{from:?}"),
            event: format!("{event:?}"),
        }
    }
}

impl fmt::Display for StateTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid {} transition: event {} is not allowed from {}",
            self.machine, self.event, self.from
        )
    }
}

impl std::error::Error for StateTransitionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_waits_and_resumes_without_changing_operation_state() {
        let waiting = TaskState::Running.transition(TaskEvent::AwaitUser).unwrap();
        assert_eq!(waiting, TaskState::WaitingUser);
        assert_eq!(
            waiting.transition(TaskEvent::UserResponded).unwrap(),
            TaskState::Running
        );
    }

    #[test]
    fn terminal_task_states_reject_all_transitions() {
        for state in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            assert!(state.transition(TaskEvent::Cancel).is_err());
            assert!(state.is_terminal());
        }
    }

    #[test]
    fn unknown_outcome_forces_processing_and_forbids_resubmission() {
        let state = OperationState::Ready
            .transition(OperationEvent::SubmissionRecorded)
            .unwrap()
            .transition(OperationEvent::OutcomeUnknown)
            .unwrap();
        assert_eq!(state, OperationState::Processing);
        assert!(!state.may_submit());
        assert!(state
            .transition(OperationEvent::SubmissionRecorded)
            .is_err());
    }

    #[test]
    fn only_ready_to_execute_allows_a_write() {
        for outcome in [
            SecurityOutcome::Allow,
            SecurityOutcome::NeedsInput,
            SecurityOutcome::ReadyToConfirm,
            SecurityOutcome::WaitingAuth,
            SecurityOutcome::Blocked,
        ] {
            assert!(!outcome.allows_write_execution());
        }
        assert!(SecurityOutcome::ReadyToExecute.allows_write_execution());
    }
}
