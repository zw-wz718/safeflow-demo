use safeflow_contracts::{EventId, NodeId, OperationId, TaskId, ToolName};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AuditEventKind {
    PlanAccepted,
    PlanRejected,
    SecurityEvaluated,
    ToolStarted,
    ToolCompleted,
    MockDraftActionVerified,
    TaskCompleted,
    TaskPaused,
    WriteRejected,
    WriteFailed,
    WriteReplayed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEvent {
    pub event_id: EventId,
    pub kind: AuditEventKind,
    pub task_id: TaskId,
    pub node_id: Option<NodeId>,
    pub operation_id: Option<OperationId>,
    pub tool_name: Option<ToolName>,
    pub details: Value,
}

pub trait AuditRecorder: Send + Sync {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError>;
}

#[derive(Default)]
pub struct InMemoryAuditRecorder {
    events: Mutex<Vec<AuditEvent>>,
}

impl InMemoryAuditRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Result<Vec<AuditEvent>, AuditError> {
        self.events
            .lock()
            .map(|events| events.clone())
            .map_err(|_| AuditError::Unavailable)
    }
}

impl AuditRecorder for InMemoryAuditRecorder {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.events
            .lock()
            .map_err(|_| AuditError::Unavailable)?
            .push(event);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditError {
    Unavailable,
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("audit recorder is unavailable")
    }
}

impl std::error::Error for AuditError {}
