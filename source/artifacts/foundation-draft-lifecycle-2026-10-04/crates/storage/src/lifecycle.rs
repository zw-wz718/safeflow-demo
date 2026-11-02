//! Isolated, checked same-task draft actions. Business state remains in the actual Store.
mod execution;
pub(crate) mod intent;
pub use intent::IntentReplacement;
mod service;
mod sql_bridge;
use crate::confirmation::current_binding;
use crate::models::{hash, validate_checkpoint, validate_plan};
use crate::*;
use execution::checked_claims;
pub(crate) use execution::{closed_nonfunding_draft,reject_closed_nonfunding_draft};
pub use execution::{LifecycleAuditSource, LifecycleAuditSourceReader, LifecycleAuditSources};
use safeflow_contracts::{
    ArgumentsHash, BusinessOperationRef, ContractFuture, ExecutionContext, MockDraftAction, NodeId,
    OperationId, OperationState, PlanNode, ResourceId, SessionRef, TaskId, TaskState, ToolResult,
    ToolResultRef,
};
use safeflow_runtime::{
    PlanningGatewaySource, PlanningIntakeService, PreflightInputContract, Runtime, SessionBinding,
    SessionResolver,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{types::Json, Postgres, Row, Transaction};
use std::sync::Arc;

pub(crate) const SOURCE_MODE: &str = "TRUSTED_RUNTIME_LIFECYCLE_V2";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleDraftObservation {
    pub draft_id: ResourceId,
    pub business_operation_id: BusinessOperationRef,
    pub draft_version: u32,
    pub status: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub parameters: Value,
}

/// Trusted backend adapter over the existing Store. Never a client or model source.
pub trait LifecycleDraftObserver: Send + Sync {
    /// Rebuild business memory only from the current SQL row locked by this service.
    /// This backend hook must not restore authorizations or cached execution results.
    fn restore<'a>(
        &'a self,
        _context: &'a ExecutionContext,
        _recovery: safeflow_transfer_integration::DraftRestore,
    ) -> ContractFuture<'a, Result<safeflow_transfer_integration::StoredDraft, StorageError>> {
        Box::pin(async { Err(StorageError::DependencyUnavailable) })
    }

    fn observe<'a>(
        &'a self,
        context: &'a ExecutionContext,
    ) -> ContractFuture<'a, Result<Option<LifecycleDraftObservation>, StorageError>>;

    /// Actual complete business readback, required only when the SQL bridge is enabled.
    /// Display observations and ToolResult JSON cannot substitute for StoredDraft.
    fn stored<'a>(
        &'a self,
        _context: &'a ExecutionContext,
        _draft_id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<safeflow_transfer_integration::StoredDraft, StorageError>> {
        Box::pin(async { Err(StorageError::DependencyUnavailable) })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleReceipt {
    pub action: String,
    pub operation_id: OperationId,
    pub plan_version: u32,
    pub expected_draft_version: Option<u32>,
    pub draft_id: ResourceId,
    pub business_operation_id: BusinessOperationRef,
    pub draft_version: u32,
    pub tool_result_ref: ToolResultRef,
    pub executed_at: chrono::DateTime<chrono::Utc>,
    pub result: ToolResult,
}

/// Comparisons and checked business input, never an authorization.
pub struct LifecycleSelection {
    pub selection_ticket: String,
    pub task_revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
    pub action: MockDraftAction,
    pub parameters: Value,
}

pub struct LifecycleView {
    pub planning_source: Option<PlanningGatewaySource>,
    pub replayed: bool,
    pub task: TaskRecord,
    pub operations: Vec<OperationRecord>,
    pub operation: OperationRecord,
    /// Current task operation, independent of the operation selected for history.
    pub active_operation_id: OperationId,
    pub action: MockDraftAction,
    pub parameters: Value,
    pub observed_draft: Option<LifecycleDraftObservation>,
    pub selection_ticket: String,
    pub is_current_action: bool,
    pub unknown_operation: Option<OperationId>,
    pub receipt: Option<LifecycleReceipt>,
    pub confirmation: Option<ConfirmationRecord>,
    pub pre_tool: Option<StoredSecurityDecision>,
    pub pre_auth: Option<StoredSecurityDecision>,
    pub display_ticket: Option<String>,
    pub confirmation_recipient: Option<ConfirmationRecipient>,
    pub recipient_reconfirmation_required: bool,
    pub intent_termination: Option<Value>,
    pub intent_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct LifecycleService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
    sessions: Arc<dyn SessionResolver>,
    observer: Arc<dyn LifecycleDraftObserver>,
    intake: PlanningIntakeService<'a>,
    decisions: DecisionService<'a>,
    confirmations: ConfirmationService,
    clock: Arc<dyn ConfirmationClock>,
    planning_source: Option<PlanningGatewaySource>,
    transfer_sql_bridge: bool,
    payee_query: Option<(ResourceId, String)>,
    #[cfg(test)]
    execution_schedule: execution::ExecutionSchedule,
}

struct ServerClock;
impl ConfirmationClock for ServerClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        std::time::SystemTime::now().into()
    }
}

impl<'a> LifecycleService<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn for_trusted_backend(
        repository: PgRepository,
        runtime: &'a Runtime,
        sessions: Arc<dyn SessionResolver>,
        observer: Arc<dyn LifecycleDraftObserver>,
        contracts: Vec<PreflightInputContract>,
        decision_config: DecisionConfig,
        confirmation_config: ConfirmationConfig,
    ) -> Result<Self, StorageError> {
        Self::for_trusted_backend_with_clock(
            repository,
            runtime,
            sessions,
            observer,
            contracts,
            decision_config,
            confirmation_config,
            Arc::new(ServerClock),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn for_trusted_backend_with_clock(
        repository: PgRepository,
        runtime: &'a Runtime,
        sessions: Arc<dyn SessionResolver>,
        observer: Arc<dyn LifecycleDraftObserver>,
        contracts: Vec<PreflightInputContract>,
        decision_config: DecisionConfig,
        confirmation_config: ConfirmationConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            intake: PlanningIntakeService::for_lifecycle_backend(runtime, contracts)
                .map_err(|_| StorageError::RuntimeIntakeRequired)?,
            decisions: DecisionService::for_lifecycle_backend_with_clock(
                repository.clone(),
                runtime,
                decision_config,
                clock.clone(),
            )?,
            confirmations: ConfirmationService::for_lifecycle_backend_with_clock(
                repository.clone(),
                sessions.clone(),
                confirmation_config,
                clock.clone(),
            ),
            repository,
            runtime,
            sessions,
            observer,
            clock,
            planning_source: None,
            transfer_sql_bridge: false,
            payee_query: None,
            #[cfg(test)]
            execution_schedule: execution::ExecutionSchedule::default(),
        })
    }
    pub fn with_planning_source_for_backend(mut self, source: PlanningGatewaySource) -> Self {
        self.planning_source = Some(source);
        self
    }
    /// Persist checked actual draft readback in the public settlement transaction.
    /// Legacy constructors alone retain their explicitly memory-only business scope.
    /// Missing complete readback or SQL history is an error; never backfill history.
    pub fn with_transfer_sql_bridge_for_backend(mut self) -> Self {
        self.transfer_sql_bridge = true;
        self
    }
    /// Actual retained/new explicit search verified by the trusted calling entry.
    /// Persisted with action provenance, never inferred from a display name.
    pub fn with_payee_query_for_backend(
        mut self,
        payee: ResourceId,
        query: String,
    ) -> Result<Self, StorageError> {
        safeflow_bank_gateway::FindPayeesInput {
            query: query.clone(),
        }
        .validate()
        .map_err(|_| StorageError::Invalid("payee query"))?;
        self.payee_query = Some((payee, query));
        Ok(self)
    }
    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        let binding = self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?;
        crate::models::db_version(binding.binding_version)?;
        if self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?
            != binding
        {
            return Err(StorageError::Session);
        }
        Ok(binding)
    }
    fn guard(&self, session: &SessionRef, binding: &SessionBinding) -> Result<(), StorageError> {
        if self.identity(session)? != *binding {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    fn context(task: &TaskRecord, op: &OperationRecord) -> ExecutionContext {
        ExecutionContext {
            task_id: task.task_id.clone(),
            node_id: op.node_id.clone(),
            operation_id: op.operation_id.clone(),
            authenticated_user_id: task.owner_id.clone(),
            plan_version: task.plan.plan_version,
            idempotency_key: None,
        }
    }
}

fn action_for(value: &str) -> Result<MockDraftAction, StorageError> {
    [
        MockDraftAction::Create,
        MockDraftAction::Modify,
        MockDraftAction::Cancel,
    ]
    .into_iter()
    .find(|action| action.action() == value)
    .ok_or(StorageError::CorruptRecord)
}
fn value<T: Serialize>(input: &T) -> Result<Value, StorageError> {
    serde_json::to_value(input).map_err(|_| StorageError::CorruptRecord)
}

struct ClaimState {
    operation: OperationId,
    receipt: Option<LifecycleReceipt>,
}

pub(crate) async fn project_progress(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    ops: &[OperationRecord],
) -> Result<TaskProgressView, StorageError> {
    checked_history(tx, task, ops).await?;
    let claims = checked_claims(tx, task, ops).await?;
    let recovery = if let Some(unknown) = claims.iter().find(|claim| claim.receipt.is_none()) {
        TaskRecoveryHint::ReconcileOriginal {
            operation_id: unknown.operation.clone(),
        }
    } else {
        match &task.checkpoint {
            RecoveryPoint::Finished => TaskRecoveryHint::Finished {
                task_state: task.state,
            },
            RecoveryPoint::AtNode { node_id } if task.state == TaskState::WaitingUser => {
                TaskRecoveryHint::AwaitUser {
                    node_id: node_id.clone(),
                }
            }
            RecoveryPoint::AtNode { node_id } => TaskRecoveryHint::ReviewNode {
                node_id: node_id.clone(),
            },
            _ => return Err(StorageError::CorruptRecord),
        }
    };
    Ok(TaskProgressView {
        contract_version: TASK_QUERY_CONTRACT_VERSION,
        task_id: task.task_id.clone(),
        task_state: task.state,
        revision: task.revision,
        plan_version: task.plan.plan_version,
        facts_version: task.facts_version,
        operations: ops
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

pub(crate) async fn check_mode(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskId,
    lifecycle: bool,
) -> Result<(), StorageError> {
    if is_lifecycle_task(tx, id).await? != lifecycle {
        return Err(StorageError::RuntimeContinuationRequired);
    }
    Ok(())
}

pub(crate) async fn is_lifecycle_task(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskId,
) -> Result<bool, StorageError> {
    // Only a recorded pre-v13 schema may omit the mode table.
    let (has_table, migration_recorded, migrated): (bool, bool, bool) = sqlx::query_as(
        "SELECT to_regclass('mock_draft_lifecycle_tasks') IS NOT NULL,
         EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version>=13),
         EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=13 AND success)",
    )
    .fetch_one(&mut **tx)
    .await?;
    if has_table != migrated || (migration_recorded && !migrated) {
        return Err(StorageError::CorruptRecord);
    }
    if !has_table {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mock_draft_lifecycle_tasks WHERE task_id=$1)",
    )
    .bind(id.as_str())
    .fetch_one(&mut **tx)
    .await?)
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanHistory {
    pub(crate) plan: safeflow_contracts::AgentPlan,
    pub(crate) plan_digest: ArgumentsHash,
    pub(crate) registration_hash: ArgumentsHash,
    pub(crate) completed_prefix: Vec<safeflow_contracts::NodeId>,
    pub(crate) previous_digest: Option<ArgumentsHash>,
}

pub(crate) async fn checked_history(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    ops: &[OperationRecord],
) -> Result<Vec<PlanHistory>, StorageError> {
    check_mode(tx, &task.task_id, true).await?;
    validate_plan(&task.plan)?;
    validate_checkpoint(task, ops)?;
    let rows = sqlx::query(
        "SELECT * FROM mock_draft_lifecycle_plans WHERE task_id=$1 ORDER BY plan_version",
    )
    .bind(task.task_id.as_str())
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() != task.plan.plan_version as usize || rows.len() > 256 {
        return Err(StorageError::CorruptRecord);
    }
    let mut history: Vec<PlanHistory> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let value = PlanHistory {
            plan: row
                .try_get::<Json<safeflow_contracts::AgentPlan>, _>("plan")?
                .0,
            plan_digest: crate::repository::decode(row, "plan_digest")?,
            registration_hash: crate::repository::decode(row, "registration_hash")?,
            completed_prefix: row
                .try_get::<Json<Vec<safeflow_contracts::NodeId>>, _>("completed_prefix")?
                .0,
            previous_digest: row
                .try_get::<Option<String>, _>("previous_digest")?
                .map(ArgumentsHash::parse)
                .transpose()
                .map_err(|_| StorageError::CorruptRecord)?,
        };
        validate_plan(&value.plan)?;
        if value.plan.task_id != task.task_id
            || value.plan.plan_version as usize != index + 1
            || value.plan_digest != hash(&value.plan)?
            || crate::repository::version(row, "plan_version")? != (index + 1) as u64
            || serde_json::to_vec(&value.plan)
                .map_err(|_| StorageError::CorruptRecord)?
                .len()
                > 65536
            || value.previous_digest != history.last().map(|previous| previous.plan_digest.clone())
        {
            return Err(StorageError::CorruptRecord);
        }
        if let Some(previous) = history.last() {
            for node in &value.completed_prefix {
                let old = previous
                    .plan
                    .nodes
                    .iter()
                    .find(|candidate| candidate.id() == node)
                    .ok_or(StorageError::CorruptRecord)?;
                if !value.plan.nodes.iter().any(|candidate| candidate == old) {
                    return Err(StorageError::CorruptRecord);
                }
            }
        } else if !value.completed_prefix.is_empty() {
            return Err(StorageError::CorruptRecord);
        }
        history.push(value);
    }
    if history.last().map(|value| &value.plan) != Some(&task.plan) {
        return Err(StorageError::CorruptRecord);
    }
    let actions = sqlx::query(
        "SELECT * FROM mock_draft_lifecycle_actions WHERE task_id=$1 ORDER BY plan_version",
    )
    .bind(task.task_id.as_str())
    .fetch_all(&mut **tx)
    .await?;
    if actions.len() != history.len() || ops.len() != actions.len() {
        return Err(StorageError::CorruptRecord);
    }
    for (action, plan) in actions.iter().zip(&history) {
        let op_id: OperationId = crate::repository::decode(action, "operation_id")?;
        let op = ops
            .iter()
            .find(|op| op.operation_id == op_id)
            .ok_or(StorageError::CorruptRecord)?;
        let node = plan
            .plan
            .nodes
            .iter()
            .find(|node| node.id() == &op.node_id)
            .ok_or(StorageError::CorruptRecord)?;
        let PlanNode::Tool {
            tool, arguments, ..
        } = node
        else {
            return Err(StorageError::CorruptRecord);
        };
        let request = action.try_get::<Json<Value>, _>("request")?.0;
        let parameters = action.try_get::<Json<Value>, _>("parameters")?.0;
        let kind = action_for(&action.try_get::<String, _>("action")?)?;
        let observed = action
            .try_get::<Option<Json<LifecycleDraftObservation>>, _>("observed_draft")?
            .map(|v| v.0);
        let request_ticket: String = action.try_get("request_ticket")?;
        if op.task_id != task.task_id
            || op.plan_version != plan.plan.plan_version
            || op.facts_version != task.facts_version
            || op.arguments_hash != hash(arguments)?
            || parameters != *arguments
            || hash(&request)?.as_str() != action.try_get::<String, _>("request_digest")?
            || op.node_id.as_str() != action.try_get::<String, _>("node_id")?
            || safeflow_contracts::MockDraftAction::for_tool(tool.as_str())
                .map(|value| value.action())
                != Some(action.try_get::<String, _>("action")?.as_str())
            || (action.try_get::<bool, _>("archived")? && op.state != OperationState::Cancelled)
            || crate::repository::version(action, "plan_version")? != plan.plan.plan_version as u64
            || request["action"] != kind.action()
            || request["parameters"] != parameters
            || (kind == MockDraftAction::Create) != observed.is_none()
        {
            return Err(StorageError::CorruptRecord);
        }
        let plan_audits:Vec<Json<Value>>=sqlx::query_scalar("SELECT details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='PLAN_REPLACED' AND details ? 'lifecycle_plan'")
            .bind(task.task_id.as_str()).bind(op_id.as_str()).fetch_all(&mut **tx).await?;
        let action_audits:Vec<Json<Value>>=sqlx::query_scalar("SELECT details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='OPERATION_CREATED' AND details ? 'lifecycle_action'")
            .bind(task.task_id.as_str()).bind(op_id.as_str()).fetch_all(&mut **tx).await?;
        if plan_audits.len() != 1
            || plan_audits[0].0["lifecycle_plan"] != value(plan)?
            || action_audits.len() != 1
            || action_audits[0].0["lifecycle_action"]
                != service::action_origin(
                    op,
                    kind,
                    &parameters,
                    &request_ticket,
                    &request,
                    observed.as_ref(),
                )?
        {
            return Err(StorageError::CorruptRecord);
        }
        if action.try_get::<bool, _>("archived")? {
            let claimed: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mock_draft_lifecycle_claims WHERE operation_id=$1)",
            )
            .bind(op_id.as_str())
            .fetch_one(&mut **tx)
            .await?;
            let archived:Vec<Json<Value>>=sqlx::query_scalar("SELECT details FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='OPERATION_TRANSITIONED'").bind(task.task_id.as_str()).bind(op_id.as_str()).fetch_all(&mut **tx).await?;
            if claimed
                || op.revision != 2
                || archived.len() != 1
                || archived[0].0 != json!({"event":"CANCEL","lifecycle_archived":true})
                || op.business_operation_ref.is_some()
            {
                return Err(StorageError::CorruptRecord);
            }
        } else if !matches!(op.state, OperationState::Draft | OperationState::Succeeded) {
            return Err(StorageError::CorruptRecord);
        }
    }
    let row = sqlx::query("SELECT * FROM mock_draft_lifecycle_tasks WHERE task_id=$1")
        .bind(task.task_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let initial = row.try_get::<Json<TaskRecord>, _>("initial_task")?.0;
    if initial.task_id != task.task_id
        || initial.owner_id != task.owner_id
        || initial.plan != history[0].plan
        || initial.revision != 1
        || initial.state != TaskState::Running
        || hash(&initial)?.as_str() != row.try_get::<String, _>("initial_digest")?
        || initial.facts_version != task.facts_version
        || history
            .iter()
            .any(|plan| plan.registration_hash != history[0].registration_hash)
        || row.try_get::<String, _>("registration_hash")? != history[0].registration_hash.as_str()
        || row.try_get::<i16, _>("lifecycle_version")? != 2
        || row.try_get::<String, _>("scope")? != "MOCK_DRAFT_LIFECYCLE_ONLY"
        || row.try_get::<String, _>("active_operation")?
            != actions
                .last()
                .ok_or(StorageError::CorruptRecord)?
                .try_get::<String, _>("operation_id")?
    {
        return Err(StorageError::CorruptRecord);
    }
    let genesis: Vec<Json<Value>> = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE task_id=$1 AND kind='TASK_CREATED'",
    )
    .bind(task.task_id.as_str())
    .fetch_all(&mut **tx)
    .await?;
    if genesis.len() != 1
        || genesis[0].0
            != json!({"lifecycle_origin":{"initial_digest":hash(&initial)?,"registration_hash":history[0].registration_hash,"initial_task":initial}})
    {
        return Err(StorageError::CorruptRecord);
    }
    let succeeded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mock_draft_lifecycle_claims WHERE task_id=$1 AND status='SUCCEEDED'",
    )
    .bind(task.task_id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    if task.revision != task.plan.plan_version as u64 + succeeded as u64
        || !matches!(
            task.state,
            TaskState::Running | TaskState::WaitingUser | TaskState::Cancelled
        )
    {
        return Err(StorageError::CorruptRecord);
    }
    let last = history.last().ok_or(StorageError::CorruptRecord)?;
    let active = ops
        .iter()
        .find(|op| {
            op.operation_id.as_str()
                == row
                    .try_get::<String, _>("active_operation")
                    .unwrap_or_default()
        })
        .ok_or(StorageError::CorruptRecord)?;
    let expected_completed = last
        .completed_prefix
        .iter()
        .cloned()
        .chain(if task.state == TaskState::Running {
            None
        } else {
            Some(active.node_id.clone())
        })
        .collect::<Vec<_>>();
    if task.completed_nodes != expected_completed
        || (task.state == TaskState::Running && active.state != OperationState::Draft)
        || (task.state != TaskState::Running && active.state != OperationState::Succeeded)
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(history)
}

async fn historical_anchor(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    ops: &[OperationRecord],
) -> Result<ArgumentsHash, StorageError> {
    let rows=sqlx::query("SELECT * FROM mock_draft_lifecycle_plans WHERE task_id=$1 AND plan_version<=$2 ORDER BY plan_version").bind(task.task_id.as_str()).bind(task.plan.plan_version as i64).fetch_all(&mut **tx).await?;
    let history = rows
        .iter()
        .map(|row| {
            Ok(PlanHistory {
                plan: row
                    .try_get::<Json<safeflow_contracts::AgentPlan>, _>("plan")?
                    .0,
                plan_digest: crate::repository::decode(row, "plan_digest")?,
                registration_hash: crate::repository::decode(row, "registration_hash")?,
                completed_prefix: row.try_get::<Json<Vec<NodeId>>, _>("completed_prefix")?.0,
                previous_digest: row
                    .try_get::<Option<String>, _>("previous_digest")?
                    .map(ArgumentsHash::parse)
                    .transpose()
                    .map_err(|_| StorageError::CorruptRecord)?,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    let actions: Json<Value>=sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY plan_version),'[]'::jsonb) FROM mock_draft_lifecycle_actions a WHERE task_id=$1 AND plan_version<=$2").bind(task.task_id.as_str()).bind(task.plan.plan_version as i64).fetch_one(&mut **tx).await?;
    hash(&(
        "safeflow-mock-draft-lifecycle:v2",
        SOURCE_MODE,
        task,
        ops,
        history,
        actions.0,
    ))
}

pub(crate) async fn binding_for_mode(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    ops: &[OperationRecord],
    operation: &OperationId,
    lifecycle: bool,
) -> Result<ConfirmationBinding, StorageError> {
    check_mode(tx, &task.task_id, lifecycle).await?;
    let mut binding = current_binding(task, ops, operation)?;
    if lifecycle {
        execution::reject_closed_nonfunding_draft(tx,&task.task_id).await?;
        checked_history(tx, task, ops).await?;
        let active: String = sqlx::query_scalar(
            "SELECT active_operation FROM mock_draft_lifecycle_tasks WHERE task_id=$1",
        )
        .bind(task.task_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        if active != operation.as_str() {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        binding.lifecycle_anchor = Some(historical_anchor(tx, task, ops).await?);
    }
    Ok(binding)
}

pub(crate) fn snapshot_digest(
    task: &TaskRecord,
    ops: &[OperationRecord],
    binding: &ConfirmationBinding,
) -> Result<ArgumentsHash, StorageError> {
    match &binding.lifecycle_anchor {
        Some(anchor) => hash(&(
            "safeflow-mock-draft-lifecycle-snapshot:v2",
            task,
            ops,
            anchor,
        )),
        None => hash(&(task, ops)),
    }
}
