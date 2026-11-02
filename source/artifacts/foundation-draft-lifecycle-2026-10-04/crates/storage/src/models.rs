use crate::StorageError;
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, BusinessOperationRef, EventId, NodeId, OperationEvent, OperationId,
    OperationState, PlanNode, TaskEvent, TaskId, TaskState, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum RecoveryPoint {
    AtNode { node_id: NodeId },
    ReconcileOperation { operation_id: OperationId },
    Finished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub owner_id: UserId,
    pub state: TaskState,
    /// Database revision, separate from the task's plan_version.
    pub revision: u64,
    pub facts_version: u64,
    pub plan: AgentPlan,
    pub checkpoint: RecoveryPoint,
    pub completed_nodes: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub operation_id: OperationId,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub state: OperationState,
    pub revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
    pub arguments_hash: ArgumentsHash,
    pub business_operation_ref: Option<BusinessOperationRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PersistentAuditKind {
    TaskCreated,
    CheckpointSaved,
    PlanReplaced,
    OperationCreated,
    OperationTransitioned,
    ChangeRejected,
    ConfirmationPrepared,
    ExplicitConfirmationRecorded,
    ConfirmationRevoked,
    PermitSaved,
    PermitConsumed,
    PermitRevoked,
    SimAuthChallengeCreated,
    SimAuthFailed,
    SimAuthVerified,
    SimAuthGrantIssued,
    SimAuthChallengeRevoked,
    SimAuthGrantRevoked,
    SecurityDecisionRecorded,
    SecurityDecisionRevoked,
    SecurityDecisionInvalidated,
    ExecutionRequestReserved,
    BillingFeedbackSaved,
    SubscriptionLocalChangeRecorded,
    CardFreezeOperationRecorded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistentAuditEvent {
    pub sequence: i64,
    pub event_id: EventId,
    pub task_id: TaskId,
    pub operation_id: Option<OperationId>,
    pub kind: PersistentAuditKind,
    pub task_revision: u64,
    pub operation_revision: Option<u64>,
    pub details: Value,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

/// Commands are intentionally not deserializable client/LLM wire contracts.
#[derive(Debug, Clone)]
pub struct CheckpointChange {
    pub expected_revision: u64,
    pub event: Option<TaskEvent>,
    pub checkpoint: RecoveryPoint,
    pub completed_nodes: Vec<NodeId>,
    pub facts_version: u64,
}

#[derive(Debug, Clone)]
pub struct OperationChange {
    pub expected_revision: u64,
    pub event: OperationEvent,
    pub facts_version: u64,
    /// An original external reference is data for reconciliation, not a Permit.
    pub business_operation_ref: Option<BusinessOperationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RecoveryDirective {
    AwaitUser {
        node_id: NodeId,
    },
    ReviewNode {
        node_id: NodeId,
    },
    ReconcileOriginal {
        operation_id: OperationId,
        business_operation_ref: BusinessOperationRef,
    },
    ReviewRecordedResult {
        operation_id: OperationId,
    },
    Finished {
        state: TaskState,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoverySnapshot {
    pub task: TaskRecord,
    pub operations: Vec<OperationRecord>,
    pub audit: Vec<PersistentAuditEvent>,
    pub directive: RecoveryDirective,
}

pub fn new_task_id() -> TaskId {
    TaskId::parse(format!("task_{}", uuid::Uuid::new_v4())).expect("UUID has content")
}

pub fn new_operation_id() -> OperationId {
    OperationId::parse(format!("operation_{}", uuid::Uuid::new_v4())).expect("UUID has content")
}

pub(crate) fn hash(value: &impl Serialize) -> Result<ArgumentsHash, StorageError> {
    let bytes = serde_json::to_vec(value).map_err(|_| StorageError::CorruptRecord)?;
    ArgumentsHash::parse(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|_| StorageError::CorruptRecord)
}

pub(crate) fn valid_id(id: &str) -> Result<(), StorageError> {
    if id.is_empty() || id.len() > 200 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(StorageError::Invalid("identifier length or characters"));
    }
    Ok(())
}

pub(crate) fn db_version(version: u64) -> Result<i64, StorageError> {
    if version == 0 || version >= i64::MAX as u64 {
        return Err(StorageError::Invalid("version out of database range"));
    }
    Ok(version as i64)
}

pub(crate) fn validate_plan(plan: &AgentPlan) -> Result<(), StorageError> {
    plan.validate_envelope()
        .map_err(|_| StorageError::Invalid("plan envelope"))?;
    valid_id(plan.task_id.as_str())?;
    if plan.nodes.is_empty() || plan.nodes.len() > 256 {
        return Err(StorageError::Invalid("plan must contain 1..256 nodes"));
    }
    let mut seen = HashSet::new();
    for node in &plan.nodes {
        valid_id(node.id().as_str())?;
        if !seen.insert(node.id()) {
            return Err(StorageError::Invalid("duplicate node"));
        }
    }
    let mut resolved = HashSet::new();
    while resolved.len() < plan.nodes.len() {
        let next = plan.nodes.iter().find(|n| {
            !resolved.contains(n.id()) && n.depends_on().iter().all(|d| resolved.contains(d))
        });
        let Some(next) = next else {
            return Err(StorageError::Invalid("missing or cyclic dependency"));
        };
        let dependencies: HashSet<_> = next.depends_on().iter().collect();
        if dependencies.len() != next.depends_on().len() {
            return Err(StorageError::Invalid("duplicate dependency"));
        }
        resolved.insert(next.id());
    }
    // This is structural validation only. Runtime/SecurityKernel still owns Tool acceptance.
    Ok(())
}

pub(crate) fn validate_checkpoint(
    task: &TaskRecord,
    operations: &[OperationRecord],
) -> Result<(), StorageError> {
    let completed: HashSet<_> = task.completed_nodes.iter().collect();
    if completed.len() != task.completed_nodes.len() {
        return Err(StorageError::Invalid("duplicate completed node"));
    }
    for id in &task.completed_nodes {
        let node = task
            .plan
            .nodes
            .iter()
            .find(|n| n.id() == id)
            .ok_or(StorageError::Invalid("unknown completed node"))?;
        if !node.depends_on().iter().all(|d| completed.contains(d)) {
            return Err(StorageError::Invalid("incomplete dependencies"));
        }
        if operations
            .iter()
            .any(|op| &op.node_id == id && op.state != OperationState::Succeeded)
        {
            return Err(StorageError::Invalid("operation node is not succeeded"));
        }
    }
    if let Some(pending) = operations.iter().find(|op| {
        matches!(
            op.state,
            OperationState::Submitted | OperationState::Processing
        )
    }) {
        if task.state != TaskState::WaitingExternal
            || task.checkpoint
                != (RecoveryPoint::ReconcileOperation {
                    operation_id: pending.operation_id.clone(),
                })
        {
            return Err(StorageError::ReconcileRequired);
        }
    }
    match &task.checkpoint {
        RecoveryPoint::AtNode { node_id } => {
            let node = task
                .plan
                .nodes
                .iter()
                .find(|n| n.id() == node_id)
                .ok_or(StorageError::Invalid("unknown recovery node"))?;
            if task.state.is_terminal()
                || completed.contains(node_id)
                || !node.depends_on().iter().all(|d| completed.contains(d))
            {
                return Err(StorageError::Invalid("inconsistent recovery node"));
            }
            if task.state == TaskState::WaitingUser
                && !matches!(node, PlanNode::UserDecision { .. })
            {
                return Err(StorageError::Invalid(
                    "waiting user requires a decision node",
                ));
            }
        }
        RecoveryPoint::ReconcileOperation { operation_id } => {
            if task.state != TaskState::WaitingExternal
                || !operations.iter().any(|op| {
                    &op.operation_id == operation_id && op.business_operation_ref.is_some()
                })
            {
                return Err(StorageError::Invalid("missing original operation"));
            }
        }
        RecoveryPoint::Finished => {
            if !task.state.is_terminal()
                || (task.state == TaskState::Completed && completed.len() != task.plan.nodes.len())
            {
                return Err(StorageError::Invalid("inconsistent terminal checkpoint"));
            }
        }
    }
    Ok(())
}

pub(crate) fn directive(
    task: &TaskRecord,
    operations: &[OperationRecord],
) -> Result<RecoveryDirective, StorageError> {
    validate_checkpoint(task, operations)?;
    Ok(match &task.checkpoint {
        RecoveryPoint::AtNode { node_id } if task.state == TaskState::WaitingUser => {
            RecoveryDirective::AwaitUser {
                node_id: node_id.clone(),
            }
        }
        RecoveryPoint::AtNode { node_id } => RecoveryDirective::ReviewNode {
            node_id: node_id.clone(),
        },
        RecoveryPoint::ReconcileOperation { operation_id } => {
            let op = operations
                .iter()
                .find(|op| &op.operation_id == operation_id)
                .ok_or(StorageError::CorruptRecord)?;
            if matches!(
                op.state,
                OperationState::Submitted | OperationState::Processing
            ) {
                RecoveryDirective::ReconcileOriginal {
                    operation_id: operation_id.clone(),
                    business_operation_ref: op
                        .business_operation_ref
                        .clone()
                        .ok_or(StorageError::CorruptRecord)?,
                }
            } else {
                RecoveryDirective::ReviewRecordedResult {
                    operation_id: operation_id.clone(),
                }
            }
        }
        RecoveryPoint::Finished => RecoveryDirective::Finished { state: task.state },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> AgentPlan {
        AgentPlan::new(
            new_task_id(),
            1,
            vec![
                PlanNode::UserDecision {
                    id: NodeId::parse("choose").unwrap(),
                    depends_on: vec![],
                },
                PlanNode::Tool {
                    id: NodeId::parse("save").unwrap(),
                    tool: safeflow_contracts::ToolName::parse("create_mock_transfer_draft")
                        .unwrap(),
                    arguments: serde_json::json!({"amount": "200.00"}),
                    depends_on: vec![NodeId::parse("choose").unwrap()],
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn structural_validation_rejects_duplicate_missing_and_cyclic_nodes() {
        let mut p = plan();
        validate_plan(&p).unwrap();
        p.nodes.push(p.nodes[0].clone());
        assert!(validate_plan(&p).is_err());
        let mut p = plan();
        let PlanNode::UserDecision { depends_on, .. } = &mut p.nodes[0] else {
            unreachable!()
        };
        *depends_on = vec![NodeId::parse("save").unwrap()];
        assert!(validate_plan(&p).is_err());
        let mut p = plan();
        let PlanNode::Tool { depends_on, .. } = &mut p.nodes[1] else {
            unreachable!()
        };
        *depends_on = vec![NodeId::parse("missing").unwrap()];
        assert!(validate_plan(&p).is_err());
    }

    #[test]
    fn checkpoints_require_consistent_completed_dependencies() {
        let p = plan();
        let mut t = TaskRecord {
            task_id: p.task_id.clone(),
            owner_id: UserId::parse("user_001").unwrap(),
            state: TaskState::WaitingUser,
            revision: 1,
            facts_version: 1,
            plan: p,
            checkpoint: RecoveryPoint::AtNode {
                node_id: NodeId::parse("choose").unwrap(),
            },
            completed_nodes: vec![],
        };
        assert!(matches!(
            directive(&t, &[]).unwrap(),
            RecoveryDirective::AwaitUser { .. }
        ));
        t.completed_nodes = vec![NodeId::parse("save").unwrap()];
        assert!(validate_checkpoint(&t, &[]).is_err());
        t.completed_nodes = vec![NodeId::parse("choose").unwrap()];
        assert!(validate_checkpoint(&t, &[]).is_err());
    }

    #[test]
    fn database_ids_and_versions_are_bounded() {
        assert!(valid_id("node\ncontrol").is_err());
        assert!(valid_id(&"x".repeat(201)).is_err());
        assert!(db_version(0).is_err());
        assert!(db_version(i64::MAX as u64).is_err());
        assert!(db_version(1).is_ok());
        assert_ne!(new_task_id(), new_task_id());
    }

    #[test]
    fn argument_digest_preserves_amount_string_and_object_order() {
        assert_eq!(
            hash(&serde_json::json!({"amount":"200.00", "currency":"CNY"})).unwrap(),
            hash(&serde_json::json!({"currency":"CNY", "amount":"200.00"})).unwrap()
        );
        assert_ne!(
            hash(&serde_json::json!({"amount":"200.00"})).unwrap(),
            hash(&serde_json::json!({"amount":200})).unwrap()
        );
    }
}
