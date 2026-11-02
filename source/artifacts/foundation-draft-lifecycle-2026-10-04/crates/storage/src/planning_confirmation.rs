//! Checked planning provenance and frozen public confirmation displays. No Tool execution.
mod execution;
use crate::confirmation::{current_binding, timestamp, ConfirmationSession};
use crate::confirmation_repository::{check_active_at, read_stored_confirmation};
use crate::models::{db_version, hash};
use crate::repository::{guarded, lock_task, operations, version};
use crate::*;
pub use execution::*;
use safeflow_contracts::{
    ArgumentsHash, ConfirmationRef, OperationId, OperationState, PlanNode, PlanningResult,
    SecurityGate, SecurityLevel, SecurityOutcome, SecurityPhase, SessionRef, TaskId, TaskState,
};
use safeflow_runtime::{
    PlanningIntakeError, PlanningIntakeService, PlanningPersistenceReview, PreflightInputContract,
    Runtime, SessionBinding, SessionResolver,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{types::Json, Postgres, Row, Transaction};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningConfirmationAction {
    Confirm,
    Reject,
}

/// Backend data only. HTTP must explicitly project the public whitelist.
/// ```compile_fail
/// let forged: safeflow_storage::PlanningConfirmationView =
///     serde_json::from_str("{}").unwrap();
/// ```
pub struct PlanningConfirmationView {
    pub task: TaskRecord,
    pub operation: Option<OperationRecord>,
    pub parameters: Value,
    pub confirmation: Option<ConfirmationRecord>,
    pub pre_tool: Option<StoredSecurityDecision>,
    pub pre_auth: Option<StoredSecurityDecision>,
    pub display_ticket: Option<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RequestOrigin {
    contract_version: u16,
    operation_id: OperationId,
    initial_digest: ArgumentsHash,
    registration_hash: ArgumentsHash,
}
struct CheckedRequest {
    initial: TaskRecord,
    task: TaskRecord,
    operation: Option<OperationRecord>,
    reserved_operation: OperationId,
    parameters: Value,
    snapshot_digest: ArgumentsHash,
}
#[derive(Clone)]
struct Display {
    ticket: String,
    snapshot_digest: String,
    pre_tool_request: DecisionRequestId,
    pre_auth_request: DecisionRequestId,
    pre_tool: Option<StoredDecisionId>,
    pre_auth: Option<StoredDecisionId>,
    confirmation: Option<ConfirmationRef>,
    digest: Option<String>,
}

pub(crate) async fn record_created_request(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    registration_hash: &ArgumentsHash,
) -> Result<Value, StorageError> {
    let origin = RequestOrigin {
        contract_version: 1,
        operation_id: new_operation_id(),
        initial_digest: hash(task)?,
        registration_hash: registration_hash.clone(),
    };
    sqlx::query("INSERT INTO planning_confirmation_requests (task_id,owner_id,operation_id,initial_task,initial_digest,registration_hash) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(task.task_id.as_str()).bind(task.owner_id.as_str()).bind(origin.operation_id.as_str())
        .bind(Json(task)).bind(origin.initial_digest.as_str()).bind(registration_hash.as_str())
        .execute(&mut **tx).await?;
    serde_json::to_value(origin).map_err(|_| StorageError::CorruptRecord)
}

pub struct PlanningConfirmationService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
    sessions: Arc<dyn SessionResolver>,
    intake: PlanningIntakeService<'a>,
    decisions: DecisionService<'a>,
    confirmations: ConfirmationService,
    clock: Arc<dyn ConfirmationClock>,
}
struct ServerClock;
impl ConfirmationClock for ServerClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        std::time::SystemTime::now().into()
    }
}
impl<'a> PlanningConfirmationService<'a> {
    pub fn for_trusted_backend(
        repository: PgRepository,
        runtime: &'a Runtime,
        sessions: Arc<dyn SessionResolver>,
        contracts: Vec<PreflightInputContract>,
        decision_config: DecisionConfig,
        confirmation_config: ConfirmationConfig,
    ) -> Result<Self, StorageError> {
        Self::for_trusted_backend_with_clock(
            repository,
            runtime,
            sessions,
            contracts,
            decision_config,
            confirmation_config,
            Arc::new(ServerClock),
        )
    }
    pub fn for_trusted_backend_with_clock(
        repository: PgRepository,
        runtime: &'a Runtime,
        sessions: Arc<dyn SessionResolver>,
        contracts: Vec<PreflightInputContract>,
        decision_config: DecisionConfig,
        confirmation_config: ConfirmationConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            intake: PlanningIntakeService::for_trusted_backend(runtime, None, contracts)
                .map_err(intake_error)?,
            decisions: DecisionService::for_trusted_backend_with_clock(
                repository.clone(),
                runtime,
                decision_config,
                clock.clone(),
            )?,
            confirmations: ConfirmationService::for_trusted_backend_with_clock(
                repository.clone(),
                sessions.clone(),
                confirmation_config,
                clock.clone(),
            ),
            repository,
            runtime,
            sessions,
            clock,
        })
    }
    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        let identity = self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?;
        db_version(identity.binding_version)?;
        if self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?
            != identity
        {
            return Err(StorageError::Session);
        }
        Ok(identity)
    }
    fn guard(&self, session: &SessionRef, binding: &SessionBinding) -> Result<(), StorageError> {
        if self.identity(session)? != *binding {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    pub async fn create(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        plan_version: u32,
        facts_version: u64,
        proposal: &PlanningResult,
    ) -> Result<TaskRecord, StorageError> {
        u32::try_from(facts_version).map_err(|_| StorageError::Invalid("facts version range"))?;
        db_version(facts_version)?;
        let review = self
            .intake
            .accept_json_for_persistence(
                session,
                task_id,
                plan_version,
                &serde_json::to_vec(proposal).map_err(|_| StorageError::CorruptRecord)?,
            )
            .await
            .map_err(intake_error)?;
        self.create_review(review, facts_version).await
    }
    /// Accepts only a closed review captured by the actual intake service and current Runtime.
    pub async fn create_review(
        &self,
        review: PlanningPersistenceReview<'_>,
        facts_version: u64,
    ) -> Result<TaskRecord, StorageError> {
        u32::try_from(facts_version).map_err(|_| StorageError::Invalid("facts version range"))?;
        db_version(facts_version)?;
        self.intake
            .check_persistence_review(&review)
            .map_err(intake_error)?;
        let plan = review
            .planning_result()
            .plan
            .clone()
            .ok_or(StorageError::RuntimeIntakeRequired)?;
        self.check_plan(&plan)?;
        let guard = || {
            self.intake
                .check_persistence_review(&review)
                .map_err(intake_error)
        };
        self.repository
            .create_planning_confirmation_task(&review, plan, facts_version, &guard)
            .await
    }
    fn check_plan(&self, plan: &safeflow_contracts::AgentPlan) -> Result<(), StorageError> {
        let [PlanNode::Tool {
            tool, depends_on, ..
        }] = plan.nodes.as_slice()
        else {
            return Err(StorageError::DecisionSourcesUnavailable);
        };
        if !depends_on.is_empty() {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        let metadata = self
            .runtime
            .preflight_source_metadata_for_backend()
            .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        let matches = metadata["tools"]
            .as_array()
            .ok_or(StorageError::CorruptRecord)?
            .iter()
            .filter(|d| d["name"] == tool.as_str() && d["has_side_effect"] == true)
            .count();
        if matches != 1 {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        Ok(())
    }
    async fn checked(
        &self,
        session: &SessionRef,
        id: &TaskId,
    ) -> Result<CheckedRequest, StorageError> {
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let snapshot = self
            .repository
            .recover(&identity.user_id, id, &guard)
            .await?;
        let row = guarded(
            &guard,
            sqlx::query(
                "SELECT * FROM planning_confirmation_requests WHERE task_id=$1 AND owner_id=$2",
            )
            .bind(id.as_str())
            .bind(identity.user_id.as_str())
            .fetch_optional(&self.repository.pool),
        )
        .await?
        .ok_or(StorageError::RuntimeIntakeRequired)?;
        let initial = row.try_get::<Json<TaskRecord>, _>("initial_task")?.0;
        let origin = RequestOrigin {
            contract_version: 1,
            operation_id: OperationId::parse(row.try_get::<String, _>("operation_id")?)
                .map_err(|_| StorageError::CorruptRecord)?,
            initial_digest: ArgumentsHash::parse(row.try_get::<String, _>("initial_digest")?)
                .map_err(|_| StorageError::CorruptRecord)?,
            registration_hash: ArgumentsHash::parse(row.try_get::<String, _>("registration_hash")?)
                .map_err(|_| StorageError::CorruptRecord)?,
        };
        let created: Vec<_> = snapshot
            .audit
            .iter()
            .filter(|a| a.kind == PersistentAuditKind::TaskCreated)
            .collect();
        let [created] = created.as_slice() else {
            return Err(StorageError::RuntimeIntakeRequired);
        };
        let audited: RequestOrigin =
            serde_json::from_value(created.details["planning_confirmation_origin"].clone())
                .map_err(|_| StorageError::RuntimeIntakeRequired)?;
        let historical = &created.details["planning_origin"];
        if audited != origin
            || initial.task_id != *id
            || initial.owner_id != identity.user_id
            || initial.revision != 1
            || initial.state != TaskState::Running
            || !initial.completed_nodes.is_empty()
            || hash(&initial)? != origin.initial_digest
            || created.task_revision != 1
            || created.operation_id.is_some()
            || created.details["creation_digest"]
                != json!(hash(&(initial.plan.clone(), initial.facts_version))?)
            || historical["registration_hash"] != json!(origin.registration_hash)
            || historical["binding_version"]
                .as_u64()
                .filter(|v| *v > 0)
                .is_none()
            || historical["session_hash"]
                .as_str()
                .filter(|s| {
                    s.len() == 64
                        && s.bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                })
                .is_none()
        {
            return Err(StorageError::RuntimeIntakeRequired);
        }
        if self
            .intake
            .registration_hash_for_backend()
            .map_err(intake_error)?
            != origin.registration_hash
        {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        self.check_plan(&initial.plan)?;
        let mut expected = initial.clone();
        let operation = match snapshot.operations.as_slice() {
            [] => None,
            [op] if op.operation_id == origin.operation_id
                && op.state == OperationState::Draft
                && op.revision == 1
                && op.plan_version == initial.plan.plan_version
                && op.facts_version == initial.facts_version
                && op.business_operation_ref.is_none()
                && op.task_id == *id
                && op.node_id == *initial.plan.nodes[0].id() =>
            {
                expected.revision = 2;
                Some(op.clone())
            }
            _ => return Err(StorageError::ConfirmationBindingMismatch),
        };
        if snapshot.task != expected {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        if let Some(op) = &operation {
            current_binding(&snapshot.task, &snapshot.operations, &op.operation_id)?;
        }
        let PlanNode::Tool { arguments, .. } = &initial.plan.nodes[0] else {
            unreachable!()
        };
        let parameters = arguments.clone();
        let snapshot_digest = hash(&(snapshot.task.clone(), snapshot.operations))?;
        guard()?;
        Ok(CheckedRequest {
            initial,
            task: snapshot.task,
            operation,
            reserved_operation: origin.operation_id,
            parameters,
            snapshot_digest,
        })
    }
    fn session_digest(session: &SessionRef) -> Result<ArgumentsHash, StorageError> {
        hash(&(
            "safeflow-planning-confirmation-display:v1",
            session.as_str(),
        ))
    }
    async fn display(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        id: &TaskId,
    ) -> Result<Option<Display>, StorageError> {
        let row = sqlx::query("SELECT * FROM planning_confirmation_displays WHERE task_id=$1 AND session_digest=$2 AND binding_version=$3")
            .bind(id.as_str()).bind(Self::session_digest(session)?.as_str())
            .bind(db_version(identity.binding_version)?).fetch_optional(&self.repository.pool).await?;
        self.guard(session, identity)?;
        row.as_ref().map(display_row).transpose()
    }
    pub async fn query(
        &self,
        session: &SessionRef,
        id: &TaskId,
    ) -> Result<PlanningConfirmationView, StorageError> {
        let identity = self.identity(session)?;
        let checked = self.checked(session, id).await?;
        let display = self.display(session, &identity, id).await?;
        self.project(session, checked, display).await
    }
    async fn project(
        &self,
        session: &SessionRef,
        checked: CheckedRequest,
        display: Option<Display>,
    ) -> Result<PlanningConfirmationView, StorageError> {
        let mut view = PlanningConfirmationView {
            task: checked.task.clone(),
            operation: checked.operation.clone(),
            parameters: checked.parameters.clone(),
            confirmation: None,
            pre_tool: None,
            pre_auth: None,
            display_ticket: None,
        };
        let Some(display) = display else {
            return Ok(view);
        };
        if display.snapshot_digest != checked.snapshot_digest.as_str() {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let (Some(pre_tool), Some(pre_auth), Some(reference), Some(digest)) = (
            &display.pre_tool,
            &display.pre_auth,
            &display.confirmation,
            &display.digest,
        ) else {
            return Ok(view);
        };
        let identity = self.identity(session)?;
        let mut tx = self.repository.pool.begin().await?;
        let current = lock_task(&mut tx, &identity.user_id, &checked.task.task_id).await?;
        let ops = operations(&mut tx, &checked.task.task_id).await?;
        if current != checked.task
            || hash(&(current.clone(), ops.clone()))? != checked.snapshot_digest
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let sources = self
            .decisions
            .read_display_sources(
                &mut tx,
                session,
                &current,
                &ops,
                &[pre_tool.clone(), pre_auth.clone()],
            )
            .await?;
        let [a, b] = sources.as_slice() else {
            return Err(StorageError::CorruptRecord);
        };
        check_preflight(a, SecurityPhase::PreTool)?;
        check_preflight(b, SecurityPhase::PreAuth)?;
        if a.binding != b.binding
            || a.request_id != display.pre_tool_request
            || b.request_id != display.pre_auth_request
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let stored = read_stored_confirmation(&mut tx, &checked.task.task_id, reference).await?;
        stored.check_session(&ConfirmationSession {
            binding: identity.clone(),
            digest: hash(&("safeflow-confirmation-session:v1", session.as_str()))?,
        })?;
        let confirmation = stored.record;
        if confirmation.binding != a.binding
            || current_binding(&current, &ops, &a.binding.operation_id)? != a.binding
            || display_digest(&checked.parameters, a, b, &confirmation)?.as_str() != digest
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let now = timestamp(self.clock.as_ref())?;
        if confirmation.expires_at <= confirmation.prepared_at
            || (confirmation.status == ConfirmationStatus::Confirmed)
                != confirmation.confirmed_at.is_some()
                && confirmation.status != ConfirmationStatus::Revoked
            || (confirmation.status == ConfirmationStatus::Revoked)
                != confirmation.revoked_at.is_some()
            || now < confirmation.prepared_at
            || confirmation.confirmed_at.is_some_and(|v| {
                v < confirmation.prepared_at || v >= confirmation.expires_at || v > now
            })
            || confirmation
                .revoked_at
                .is_some_and(|v| v < confirmation.prepared_at || v > now)
        {
            return Err(StorageError::CorruptRecord);
        }
        if confirmation.status != ConfirmationStatus::Revoked {
            check_active_at(&confirmation, now)?;
        }
        if self
            .decisions
            .read_display_sources(
                &mut tx,
                session,
                &current,
                &ops,
                &[pre_tool.clone(), pre_auth.clone()],
            )
            .await?
            != sources
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        self.guard(session, &identity)?;
        let final_now = timestamp(self.clock.as_ref())?;
        crate::decision::check_time(a, final_now.timestamp_millis())?;
        crate::decision::check_time(b, final_now.timestamp_millis())?;
        if confirmation.status != ConfirmationStatus::Revoked {
            check_active_at(&confirmation, final_now)?;
        }
        tx.commit().await?;
        view.display_ticket =
            (confirmation.status == ConfirmationStatus::Prepared).then_some(display.ticket);
        view.confirmation = Some(confirmation);
        view.pre_tool = Some(a.clone());
        view.pre_auth = Some(b.clone());
        Ok(view)
    }
    pub async fn prepare(
        &self,
        session: &SessionRef,
        id: &TaskId,
    ) -> Result<PlanningConfirmationView, StorageError> {
        let identity = self.identity(session)?;
        let mut checked = self.checked(session, id).await?;
        if checked.operation.is_none() {
            self.repository
                .create_operation(
                    &identity.user_id,
                    id,
                    checked.task.revision,
                    checked.reserved_operation.clone(),
                    checked.initial.plan.nodes[0].id().clone(),
                    checked.initial.plan.plan_version,
                    &|| self.guard(session, &identity),
                )
                .await?;
            checked = self.checked(session, id).await?;
        }
        let display = self.reserve_display(session, &identity, &checked).await?;
        if display.confirmation.is_some() {
            // Never renew a revoked or expired confirmation in the same session.
            let view = self.project(session, checked, Some(display)).await?;
            if view
                .confirmation
                .as_ref()
                .is_some_and(|r| r.status == ConfirmationStatus::Revoked)
            {
                return Err(StorageError::ConfirmationRevoked);
            }
            return Ok(view);
        }
        let operation = checked.operation.as_ref().ok_or(StorageError::NotFound)?;
        let a = self
            .decisions
            .evaluate_and_save(
                session,
                id,
                &operation.operation_id,
                &display.pre_tool_request,
                SecurityPhase::PreTool,
            )
            .await?;
        check_preflight(&a, SecurityPhase::PreTool)?;
        let b = self
            .decisions
            .evaluate_and_save(
                session,
                id,
                &operation.operation_id,
                &display.pre_auth_request,
                SecurityPhase::PreAuth,
            )
            .await?;
        check_preflight(&b, SecurityPhase::PreAuth)?;
        if a.binding != b.binding {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let confirmation = self
            .confirmations
            .prepare(session, id, &operation.operation_id)
            .await?;
        if confirmation.binding != a.binding {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let digest = display_digest(&checked.parameters, &a, &b, &confirmation)?;
        let mut tx = self.repository.pool.begin().await?;
        let current = lock_task(&mut tx, &identity.user_id, id).await?;
        let ops = operations(&mut tx, id).await?;
        if current != checked.task || hash(&(current, ops))? != checked.snapshot_digest {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        sqlx::query("UPDATE planning_confirmation_displays SET pre_tool_decision=$1,pre_auth_decision=$2,confirmation_ref=$3,display_digest=$4 WHERE display_ticket=$5 AND pre_tool_decision IS NULL")
            .bind(a.stored_decision_id.as_str()).bind(b.stored_decision_id.as_str())
            .bind(confirmation.confirmation_ref.as_str()).bind(digest.as_str()).bind(&display.ticket)
            .execute(&mut *tx).await?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        self.query(session, id).await
    }
    async fn reserve_display(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        checked: &CheckedRequest,
    ) -> Result<Display, StorageError> {
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task(&mut tx, &identity.user_id, &checked.task.task_id).await?;
        let ops = operations(&mut tx, &checked.task.task_id).await?;
        if task != checked.task || hash(&(task, ops))? != checked.snapshot_digest {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        sqlx::query("INSERT INTO planning_confirmation_displays (display_ticket,task_id,session_digest,binding_version,snapshot_digest,pre_tool_request,pre_auth_request) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (task_id,session_digest,binding_version) DO NOTHING")
            .bind(format!("display_{}", uuid::Uuid::new_v4().simple())).bind(checked.task.task_id.as_str())
            .bind(Self::session_digest(session)?.as_str()).bind(db_version(identity.binding_version)?)
            .bind(checked.snapshot_digest.as_str()).bind(DecisionRequestId::new_server_id().as_str())
            .bind(DecisionRequestId::new_server_id().as_str()).execute(&mut *tx).await?;
        self.guard(session, identity)?;
        tx.commit().await?;
        let display = self
            .display(session, identity, &checked.task.task_id)
            .await?
            .ok_or(StorageError::CorruptRecord)?;
        if display.snapshot_digest != checked.snapshot_digest.as_str() {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        Ok(display)
    }
    pub async fn act(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        operation_id: &OperationId,
        reference: &ConfirmationRef,
        ticket: &str,
        action: PlanningConfirmationAction,
    ) -> Result<PlanningConfirmationView, StorageError> {
        let identity = self.identity(session)?;
        let checked = self.checked(session, task_id).await?;
        let display = self
            .display(session, &identity, task_id)
            .await?
            .ok_or(StorageError::NotFound)?;
        if display.ticket != ticket
            || display.confirmation.as_ref() != Some(reference)
            || checked.operation.as_ref().map(|op| &op.operation_id) != Some(operation_id)
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let view = self.project(session, checked, Some(display)).await?;
        let confirmation = view
            .confirmation
            .as_ref()
            .ok_or(StorageError::ConfirmationRequired)?;
        match action {
            PlanningConfirmationAction::Confirm => {
                let pre_tool = view
                    .pre_tool
                    .as_ref()
                    .ok_or(StorageError::DecisionSourcesUnavailable)?;
                let pre_auth = view
                    .pre_auth
                    .as_ref()
                    .ok_or(StorageError::DecisionSourcesUnavailable)?;
                self.decisions
                    .verify(
                        session,
                        task_id,
                        &pre_tool.stored_decision_id,
                        &confirmation.binding,
                    )
                    .await?;
                self.decisions
                    .verify(
                        session,
                        task_id,
                        &pre_auth.stored_decision_id,
                        &confirmation.binding,
                    )
                    .await?;
                self.confirmations
                    .record_explicit_confirmation(
                        session,
                        task_id,
                        reference,
                        &confirmation.binding,
                    )
                    .await?;
                self.decisions
                    .verify_with_confirmation(
                        session,
                        task_id,
                        &pre_tool.stored_decision_id,
                        reference,
                        &confirmation.binding,
                    )
                    .await?;
                self.decisions
                    .verify_with_confirmation(
                        session,
                        task_id,
                        &pre_auth.stored_decision_id,
                        reference,
                        &confirmation.binding,
                    )
                    .await?;
            }
            PlanningConfirmationAction::Reject => {
                self.confirmations
                    .reject_prepared(session, task_id, reference, &confirmation.binding)
                    .await?;
            }
        }
        self.query(session, task_id).await
    }
}
fn intake_error(error: PlanningIntakeError) -> StorageError {
    match error {
        PlanningIntakeError::Session => StorageError::Session,
        _ => StorageError::RuntimeIntakeRequired,
    }
}
fn display_row(row: &sqlx::postgres::PgRow) -> Result<Display, StorageError> {
    let pre_tool = row
        .try_get::<Option<String>, _>("pre_tool_decision")?
        .map(StoredDecisionId::parse)
        .transpose()?;
    let pre_auth = row
        .try_get::<Option<String>, _>("pre_auth_decision")?
        .map(StoredDecisionId::parse)
        .transpose()?;
    let confirmation = row
        .try_get::<Option<String>, _>("confirmation_ref")?
        .map(ConfirmationRef::parse)
        .transpose()
        .map_err(|_| StorageError::CorruptRecord)?;
    let digest = row.try_get::<Option<String>, _>("display_digest")?;
    if [
        pre_tool.is_some(),
        pre_auth.is_some(),
        confirmation.is_some(),
        digest.is_some(),
    ]
    .iter()
    .any(|v| *v != pre_tool.is_some())
    {
        return Err(StorageError::CorruptRecord);
    }
    version(row, "binding_version")?;
    Ok(Display {
        ticket: row.try_get("display_ticket")?,
        snapshot_digest: row.try_get("snapshot_digest")?,
        pre_tool_request: DecisionRequestId::parse(row.try_get::<String, _>("pre_tool_request")?)?,
        pre_auth_request: DecisionRequestId::parse(row.try_get::<String, _>("pre_auth_request")?)?,
        pre_tool,
        pre_auth,
        confirmation,
        digest,
    })
}
fn check_preflight(
    record: &StoredSecurityDecision,
    phase: SecurityPhase,
) -> Result<(), StorageError> {
    if record.phase != phase
        || record.evidence_kind != "PREFLIGHT_ONLY"
        || record.decision.outcome != SecurityOutcome::ReadyToConfirm
        || record.decision.required_gates != [SecurityGate::ExplicitConfirmation]
        || record.decision.minimum_level != SecurityLevel::Green
    {
        return Err(StorageError::DecisionSourcesUnavailable);
    }
    Ok(())
}
fn display_digest(
    parameters: &Value,
    a: &StoredSecurityDecision,
    b: &StoredSecurityDecision,
    confirmation: &ConfirmationRecord,
) -> Result<ArgumentsHash, StorageError> {
    hash(&(
        parameters,
        &a.binding,
        &a.stored_decision_id,
        &b.stored_decision_id,
        &confirmation.confirmation_ref,
        confirmation.prepared_at,
        confirmation.expires_at,
    ))
}
