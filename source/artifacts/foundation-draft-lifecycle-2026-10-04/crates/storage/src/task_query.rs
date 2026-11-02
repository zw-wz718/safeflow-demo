use crate::confirmation::reject_floats;
use crate::models::{directive, hash, valid_id, validate_plan};
use crate::repository::{operation_row, task_row, version};
use crate::{OperationRecord, PgRepository, RecoveryDirective, StorageError, TaskRecord};
use safeflow_contracts::{
    NodeId, OperationId, OperationState, PlanNode, SessionRef, TaskId, TaskState,
};
use safeflow_runtime::{PlanningGatewaySource, Runtime};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashSet;

pub const TASK_QUERY_CONTRACT_VERSION: u16 = 1;
/// One operation per node, matching the existing storage contract's 256-node limit.
pub const TASK_QUERY_MAX_OPERATIONS: usize = 256;
pub const TASK_QUERY_MAX_PLAN_BYTES: usize = 64 * 1024;

/// Internal backend projection, not the team's pending HTTP TaskResponse contract.
///
/// ```compile_fail
/// let claimed: safeflow_storage::TaskProgressView =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskProgressView {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub task_state: TaskState,
    pub revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
    pub operations: Vec<OperationProgressView>,
    pub recovery: TaskRecoveryHint,
    pub security: TaskQuerySecurityScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationProgressView {
    pub operation_id: OperationId,
    pub node_id: NodeId,
    pub operation_state: OperationState,
    pub revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
}

/// No current SecurityOutcome is supplied or inferred by this query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskQuerySecurityScope {
    CurrentDecisionNotProvided,
}

/// A recorded position requiring review, never an instruction/permission to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskRecoveryHint {
    AwaitUser { node_id: NodeId },
    ReviewNode { node_id: NodeId },
    ReconcileOriginal { operation_id: OperationId },
    ReviewRecordedResult { operation_id: OperationId },
    Finished { task_state: TaskState },
}

pub struct TaskQueryService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
}

/// Recorded provenance and current-session comparison, never execution authority.
pub struct PlanningGatewayProvenance {
    source: Option<PlanningGatewaySource>,
    original_session_matches: bool,
    registration_hash: Option<safeflow_contracts::ArgumentsHash>,
}
impl PlanningGatewayProvenance {
    pub fn source(&self) -> Option<PlanningGatewaySource> {
        self.source
    }
    pub fn matches_current_registration(
        &self,
        expected: &safeflow_contracts::ArgumentsHash,
    ) -> bool {
        self.original_session_matches && self.registration_hash.as_ref() == Some(expected)
    }
}

impl<'a> TaskQueryService<'a> {
    pub fn for_trusted_backend(repository: PgRepository, runtime: &'a Runtime) -> Self {
        Self {
            repository,
            runtime,
        }
    }

    /// Resolves identity only. Does not execute, recover, evaluate or authorize anything.
    pub async fn query(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
    ) -> Result<TaskProgressView, StorageError> {
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        let binding = self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(binding.user_id.as_str()).map_err(|_| StorageError::Session)?;
        let guard = || {
            if self
                .runtime
                .session_binding_for_backend(session)
                .map_err(|_| StorageError::Session)?
                != binding
            {
                return Err(StorageError::Session);
            }
            Ok(())
        };
        let view = self
            .repository
            .query_task_progress(&binding.user_id, task_id, &guard)
            .await?;
        guard()?;
        Ok(view)
    }

    /// Recorded planner provenance only. This does not authorize execution or recapture
    /// the original session. Scheduling independently verifies that original binding.
    pub async fn planning_gateway_provenance(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
    ) -> Result<PlanningGatewayProvenance, StorageError> {
        use crate::repository::guarded;
        use sqlx::types::Json;
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        let binding = self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?;
        let guard = || {
            if self
                .runtime
                .session_binding_for_backend(session)
                .map_err(|_| StorageError::Session)?
                != binding
            {
                return Err(StorageError::Session);
            }
            Ok(())
        };
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(
            &guard,
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx),
        )
        .await?;
        let row = guarded(&guard, sqlx::query("SELECT creation_digest,plan_version,facts_version,CASE WHEN octet_length(plan::text)<=$3 THEN plan END AS plan FROM tasks WHERE task_id=$1 AND owner_id=$2")
            .bind(task_id.as_str()).bind(binding.user_id.as_str()).bind(TASK_QUERY_MAX_PLAN_BYTES as i32).fetch_optional(&mut *tx)).await?
            .ok_or(StorageError::NotFound)?;
        let rows = guarded(&guard, sqlx::query("SELECT task_revision,CASE WHEN octet_length(details::text)<=4096 THEN details END AS details FROM audit_events WHERE task_id=$1 AND kind='TASK_CREATED' ORDER BY sequence LIMIT 2")
            .bind(task_id.as_str()).fetch_all(&mut *tx)).await?;
        if rows.len() != 1 || version(&rows[0], "task_revision")? != 1 {
            return Err(StorageError::CorruptRecord);
        }
        let details = rows[0]
            .try_get::<Json<serde_json::Value>, _>("details")
            .map_err(|_| StorageError::CorruptRecord)?
            .0;
        let provenance = match details.get("planning_gateway_source") {
            None => PlanningGatewayProvenance {
                source: None,
                original_session_matches: false,
                registration_hash: None,
            },
            Some(value) => {
                let source: PlanningGatewaySource = serde_json::from_value(value.clone())
                    .map_err(|_| StorageError::CorruptRecord)?;
                let origin: PlanningSourceOrigin =
                    serde_json::from_value(details["planning_origin"].clone())
                        .map_err(|_| StorageError::CorruptRecord)?;
                let plan = row
                    .try_get::<Json<safeflow_contracts::AgentPlan>, _>("plan")
                    .map_err(|_| StorageError::CorruptRecord)?
                    .0;
                let facts = version(&row, "facts_version")?;
                let digest = row.try_get::<String, _>("creation_digest")?;
                crate::models::validate_plan(&plan).map_err(|_| StorageError::CorruptRecord)?;
                if origin.binding_version == 0
                    || origin.session_hash.as_str().len() != 64
                    || origin.registration_hash.as_str().len() != 64
                    || plan.task_id != *task_id
                    || u64::from(plan.plan_version) != version(&row, "plan_version")?
                    || details["plan_version"].as_u64() != Some(u64::from(plan.plan_version))
                    || details["creation_digest"].as_str() != Some(digest.as_str())
                    || hash(&(plan, facts))?.as_str() != digest
                {
                    return Err(StorageError::CorruptRecord);
                }
                PlanningGatewayProvenance {
                    source: Some(source),
                    original_session_matches: origin.session_hash == hash(session)?
                        && origin.binding_version == binding.binding_version,
                    registration_hash: Some(origin.registration_hash),
                }
            }
        };
        guarded(&guard, tx.commit()).await?;
        Ok(provenance)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanningSourceOrigin {
    session_hash: safeflow_contracts::ArgumentsHash,
    binding_version: u64,
    registration_hash: safeflow_contracts::ArgumentsHash,
}

impl PgRepository {
    async fn query_task_progress(
        &self,
        owner: &safeflow_contracts::UserId,
        task_id: &TaskId,
        guard: &(dyn Fn() -> Result<(), StorageError> + Send + Sync),
    ) -> Result<TaskProgressView, StorageError> {
        let acquired = self.pool.begin().await;
        guard()?;
        let mut tx = acquired?;
        let isolation = sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await;
        guard()?;
        isolation?;
        // Bound JSON before transfer/decoding. An oversized value becomes NULL and fails closed.
        let fetched = sqlx::query("SELECT task_id,owner_id,contract_version,state,revision,plan_version,facts_version, CASE WHEN octet_length(plan::text)<=$3 THEN plan END AS plan, CASE WHEN octet_length(checkpoint::text)<=1024 THEN checkpoint END AS checkpoint, CASE WHEN octet_length(completed_nodes::text)<=$3 THEN completed_nodes END AS completed_nodes FROM tasks WHERE task_id=$1 AND owner_id=$2")
            .bind(task_id.as_str()).bind(owner.as_str()).bind(TASK_QUERY_MAX_PLAN_BYTES as i32).fetch_optional(&mut *tx).await;
        guard()?;
        let row = fetched?.ok_or(StorageError::NotFound)?;
        let task = task_row(&row).map_err(|_| StorageError::CorruptRecord)?;
        if task.task_id != *task_id
            || task.owner_id != *owner
            || u64::from(task.plan.plan_version) != version(&row, "plan_version")?
        {
            return Err(StorageError::CorruptRecord);
        }
        let preparations: bool =
            sqlx::query_scalar("SELECT to_regclass('payment_preparations') IS NOT NULL")
                .fetch_one(&mut *tx)
                .await?;
        let preparation_filter = if preparations {
            " AND NOT EXISTS (SELECT 1 FROM payment_preparations p WHERE p.operation_id=o.operation_id)"
        } else {
            ""
        };
        let statement=format!("SELECT operation_id,task_id,node_id,state,revision,plan_version,facts_version, CASE WHEN octet_length(arguments_hash)<=64 THEN arguments_hash END AS arguments_hash, CASE WHEN business_operation_ref IS NULL OR octet_length(business_operation_ref)<=200 THEN business_operation_ref END AS business_operation_ref, COALESCE(octet_length(business_operation_ref),0)>200 AS oversized_reference FROM operations o WHERE task_id=$1{preparation_filter} ORDER BY operation_id COLLATE \"C\" LIMIT $2");
        let fetched = sqlx::query(&statement)
            .bind(task_id.as_str())
            .bind((TASK_QUERY_MAX_OPERATIONS + 1) as i64)
            .fetch_all(&mut *tx)
            .await;
        guard()?;
        let rows = fetched?;
        if rows.len() > TASK_QUERY_MAX_OPERATIONS {
            return Err(StorageError::CorruptRecord);
        }
        let operations = rows
            .iter()
            .map(|row| {
                if row
                    .try_get::<bool, _>("oversized_reference")
                    .map_err(|_| StorageError::CorruptRecord)?
                {
                    return Err(StorageError::CorruptRecord);
                }
                operation_row(row).map_err(|_| StorageError::CorruptRecord)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let lifecycle = crate::lifecycle::is_lifecycle_task(&mut tx, task_id).await?;
        guard()?;
        let view = if lifecycle {
            crate::lifecycle::project_progress(&mut tx, &task, &operations).await?
        } else {
            project(&task, &operations).map_err(|_| StorageError::CorruptRecord)?
        };
        let committed = tx.commit().await;
        guard()?;
        committed?;
        Ok(view)
    }
}

fn project(
    task: &TaskRecord,
    operations: &[OperationRecord],
) -> Result<TaskProgressView, StorageError> {
    validate_plan(&task.plan)?;
    valid_id(task.task_id.as_str())?;
    valid_id(task.owner_id.as_str())?;
    if task.plan.task_id != task.task_id
        || task.revision == 0
        || task.facts_version == 0
        || operations.len() > TASK_QUERY_MAX_OPERATIONS
    {
        return Err(StorageError::CorruptRecord);
    }
    let mut ids = HashSet::new();
    let mut nodes = HashSet::new();
    if operations
        .iter()
        .filter(|op| {
            matches!(
                op.state,
                OperationState::Submitted | OperationState::Processing
            )
        })
        .count()
        > 1
    {
        return Err(StorageError::CorruptRecord);
    }
    for node in &task.plan.nodes {
        if let PlanNode::Tool {
            tool, arguments, ..
        } = node
        {
            valid_id(tool.as_str())?;
            reject_floats(arguments)?;
        }
    }
    for op in operations {
        valid_id(op.operation_id.as_str())?;
        valid_id(op.node_id.as_str())?;
        if op.task_id != task.task_id
            || op.plan_version != task.plan.plan_version
            || op.revision == 0
            || op.facts_version == 0
            || op.facts_version > task.facts_version
            || !ids.insert(&op.operation_id)
            || !nodes.insert(&op.node_id)
        {
            return Err(StorageError::CorruptRecord);
        }
        let Some(PlanNode::Tool { arguments, .. }) =
            task.plan.nodes.iter().find(|node| node.id() == &op.node_id)
        else {
            return Err(StorageError::CorruptRecord);
        };
        if hash(arguments)? != op.arguments_hash {
            return Err(StorageError::CorruptRecord);
        }
        if let Some(reference) = &op.business_operation_ref {
            valid_id(reference.as_str())?;
            if matches!(
                op.state,
                OperationState::Draft | OperationState::Ready | OperationState::Cancelled
            ) {
                return Err(StorageError::CorruptRecord);
            }
        } else if matches!(
            op.state,
            OperationState::Submitted
                | OperationState::Processing
                | OperationState::Succeeded
                | OperationState::Failed
        ) {
            return Err(StorageError::CorruptRecord);
        }
    }
    let recovery = match directive(task, operations)? {
        RecoveryDirective::AwaitUser { node_id } => TaskRecoveryHint::AwaitUser { node_id },
        RecoveryDirective::ReviewNode { node_id } => TaskRecoveryHint::ReviewNode { node_id },
        RecoveryDirective::ReconcileOriginal { operation_id, .. } => {
            TaskRecoveryHint::ReconcileOriginal { operation_id }
        }
        RecoveryDirective::ReviewRecordedResult { operation_id } => {
            TaskRecoveryHint::ReviewRecordedResult { operation_id }
        }
        RecoveryDirective::Finished { state } => TaskRecoveryHint::Finished { task_state: state },
    };
    Ok(TaskProgressView {
        contract_version: TASK_QUERY_CONTRACT_VERSION,
        task_id: task.task_id.clone(),
        task_state: task.state,
        revision: task.revision,
        plan_version: task.plan.plan_version,
        facts_version: task.facts_version,
        operations: operations
            .iter()
            .map(|op| OperationProgressView {
                operation_id: op.operation_id.clone(),
                node_id: op.node_id.clone(),
                operation_state: op.state,
                revision: op.revision,
                plan_version: op.plan_version,
                facts_version: op.facts_version,
            })
            .collect(),
        recovery,
        security: TaskQuerySecurityScope::CurrentDecisionNotProvided,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_task_id, RecoveryPoint};
    use safeflow_contracts::{AgentPlan, ToolName, UserId};
    use serde_json::json;

    fn task() -> TaskRecord {
        let plan = AgentPlan::new(
            new_task_id(),
            1,
            vec![PlanNode::Tool {
                id: NodeId::parse("stored").unwrap(),
                tool: ToolName::parse("stored_metadata").unwrap(),
                arguments: json!({"amount":"200.00"}),
                depends_on: vec![],
            }],
        )
        .unwrap();
        TaskRecord {
            task_id: plan.task_id.clone(),
            owner_id: UserId::parse("owner").unwrap(),
            state: TaskState::Running,
            revision: 1,
            facts_version: 7,
            plan,
            checkpoint: RecoveryPoint::AtNode {
                node_id: NodeId::parse("stored").unwrap(),
            },
            completed_nodes: vec![],
        }
    }
    #[test]
    fn view_has_no_implicit_current_security_outcome() {
        let view = project(&task(), &[]).unwrap();
        assert_eq!(
            serde_json::to_value(view).unwrap()["security"],
            "CURRENT_DECISION_NOT_PROVIDED"
        );
    }
    #[test]
    fn invalid_plan_and_completed_node_are_rejected() {
        let mut t = task();
        t.plan.nodes.push(t.plan.nodes[0].clone());
        assert!(project(&t, &[]).is_err());
        let mut t = task();
        t.completed_nodes.push(NodeId::parse("missing").unwrap());
        assert!(project(&t, &[]).is_err());
    }
    #[test]
    fn mismatched_task_identity_and_zero_versions_are_rejected() {
        let mut t = task();
        t.plan.task_id = new_task_id();
        assert!(project(&t, &[]).is_err());
        let mut t = task();
        t.revision = 0;
        assert!(project(&t, &[]).is_err());
        let mut t = task();
        t.facts_version = 0;
        assert!(project(&t, &[]).is_err());
    }
    #[test]
    fn corrupt_completed_task_is_not_reported_as_finished() {
        let mut t = task();
        t.state = TaskState::Completed;
        t.checkpoint = RecoveryPoint::Finished;
        assert!(project(&t, &[]).is_err());
        t.completed_nodes = vec![NodeId::parse("stored").unwrap()];
        assert!(project(&t, &[]).is_ok());
    }
}
