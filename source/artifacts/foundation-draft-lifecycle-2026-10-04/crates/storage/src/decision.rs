//! Trusted Runtime preflight persistence. No formal PRE_COMMIT authority.
pub(crate) mod payment;
mod precommit_sources;
use crate::confirmation::{timestamp, ConfirmationSession};
use crate::confirmation_repository::{
    binding_row, check_active_at, read_stored_confirmation, StoredConfirmation,
};
use crate::models::{db_version, hash, valid_id};
use crate::repository::{audit, operations, version};
use crate::{
    ConfirmationBinding, ConfirmationClock, ConfirmationRecord, ConfirmationStatus,
    PersistentAuditKind, PgRepository, StorageError, TaskRecord,
};
pub use precommit_sources::{
    MissingPreCommitSource, PreCommitPublicSourceDisposition, PreCommitPublicSourceReport,
    PRE_COMMIT_PUBLIC_SOURCE_REPORT_VERSION,
};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, ConfirmationRef, OperationId, OperationSecurityRequest, PlanNode,
    SecurityDecision, SecurityOutcome, SecurityPhase, SessionRef, TaskId,
    SECURITY_CONTRACT_VERSION,
};
use safeflow_runtime::{PreflightInputContract, Runtime, SessionBinding};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgPoolOptions, PgRow},
    types::Json,
    PgPool, Postgres, Row, Transaction,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

pub const STORED_SECURITY_DECISION_CONTRACT_VERSION: u16 = 1;
const MODE: &str = "TRUSTED_RUNTIME_V1";
macro_rules! id {
    ($name:ident,$prefix:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new_server_id() -> Self {
                Self(format!("{}{}", $prefix, uuid::Uuid::new_v4()))
            }
            pub fn parse(value: impl Into<String>) -> Result<Self, StorageError> {
                let value = value.into();
                let suffix = value
                    .strip_prefix($prefix)
                    .ok_or(StorageError::Invalid("decision ID prefix"))?;
                if suffix.len() != 36
                    || uuid::Uuid::parse_str(suffix)
                        .map_err(|_| StorageError::Invalid("decision UUID"))?
                        .to_string()
                        != suffix
                {
                    return Err(StorageError::Invalid("noncanonical decision ID"));
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
id!(DecisionRequestId, "decision_request_");
id!(StoredDecisionId, "stored_decision_");
#[derive(Clone)]
pub struct DecisionConfig {
    pub(crate) ttl_ms: i64,
    deployment_revision: String,
    input_contracts: Vec<PreflightInputContract>,
}
impl DecisionConfig {
    /// All settings are trusted backend inputs, not product defaults or client permissions.
    pub fn new(
        ttl: Duration,
        deployment_revision: impl Into<String>,
        mut input_contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, StorageError> {
        let ttl_ms = i64::try_from(ttl.as_millis())
            .map_err(|_| StorageError::Invalid("decision TTL range"))?;
        let deployment_revision = deployment_revision.into();
        if ttl_ms <= 0
            || ttl.subsec_nanos() % 1_000_000 != 0
            || deployment_revision.is_empty()
            || deployment_revision.len() > 80
            || !deployment_revision
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err(StorageError::Invalid(
                "explicit bounded decision configuration required",
            ));
        }
        if input_contracts.is_empty() || input_contracts.len() > 256 {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        input_contracts.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        for (i, contract) in input_contracts.iter().enumerate() {
            valid_id(contract.tool_name.as_str())?;
            valid_id(contract.policy_version.as_str())?;
            if contract.tool_schema_version == 0
                || contract.projection_revision.is_empty()
                || contract.projection_revision.len() > 80
                || !contract
                    .projection_revision
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                || i > 0 && input_contracts[i - 1].tool_name == contract.tool_name
            {
                return Err(StorageError::DecisionSourcesUnavailable);
            }
        }
        Ok(Self {
            ttl_ms,
            deployment_revision,
            input_contracts,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StoredDecisionStatus {
    Active,
    Revoked,
    Invalidated,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionInvalidationReason {
    SnapshotChanged,
    AssemblyChanged,
    Expired,
}
/// Safe data only. A deserialized/constructed SecurityDecision cannot enter the save API.
/// ```compile_fail
/// fn forge(service: &safeflow_storage::DecisionService<'_>, value: safeflow_contracts::SecurityDecision) {
///     service.save_decision(value);
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StoredSecurityDecision {
    pub contract_version: u16,
    pub stored_decision_id: StoredDecisionId,
    pub request_id: DecisionRequestId,
    pub evidence_kind: &'static str,
    pub source_mode: &'static str,
    pub source_instance_id: String,
    pub binding: ConfirmationBinding,
    pub phase: SecurityPhase,
    pub snapshot_digest: ArgumentsHash,
    pub assembly_digest: ArgumentsHash,
    pub assembly_metadata: Value,
    pub decision: SecurityDecision,
    pub decision_digest: ArgumentsHash,
    pub plan_review_digest: ArgumentsHash,
    pub evaluated_at_ms: i64,
    pub expires_at_ms: i64,
    pub status: StoredDecisionStatus,
    pub ended_at_ms: Option<i64>,
    pub invalidation_reason: Option<DecisionInvalidationReason>,
}

pub const PREFLIGHT_CONFIRMATION_CHECK_VERSION: u16 = 1;

/// Transaction-local source check receipt, not PRE_COMMIT evidence or execution authority.
/// Recheck database sources on every use; this receipt does not retain any database locks.
/// ```compile_fail
/// let forged: safeflow_storage::VerifiedPreflightConfirmation =
///     serde_json::from_value(serde_json::json!({"verified":true})).unwrap();
/// ```
/// ```compile_fail
/// async fn promote(
///     store: &safeflow_storage::PermitStore,
///     session: &safeflow_contracts::SessionRef,
///     checked: &safeflow_storage::VerifiedPreflightConfirmation,
/// ) {
///     store.save_verified(session, checked).await;
/// }
/// ```
pub struct VerifiedPreflightConfirmation {
    decision: StoredSecurityDecision,
    confirmation: ConfirmationRecord,
    checked_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl VerifiedPreflightConfirmation {
    pub fn contract_version(&self) -> u16 {
        PREFLIGHT_CONFIRMATION_CHECK_VERSION
    }
    pub fn preflight_decision(&self) -> &StoredSecurityDecision {
        &self.decision
    }
    pub fn confirmation(&self) -> &ConfirmationRecord {
        &self.confirmation
    }
    pub fn checked_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.checked_at
    }
    pub fn expires_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.expires_at
    }
}
struct ServerClock;
impl ConfirmationClock for ServerClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        SystemTime::now().into()
    }
}
#[derive(Clone)]
pub struct DecisionService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
    clock: Arc<dyn ConfirmationClock>,
    config: DecisionConfig,
    metadata: Value,
    assembly_digest: ArgumentsHash,
    source_instance_id: String,
    time_pool: PgPool,
    lifecycle: bool,
}
impl<'a> DecisionService<'a> {
    fn current_metadata(&self) -> Result<Value, StorageError> {
        let runtime = if self.lifecycle {
            self.runtime.lifecycle_source_metadata_for_backend()
        } else {
            self.runtime.preflight_source_metadata_for_backend()
        }
        .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        let mut metadata = json!({"contract_version":1,"source_mode":self.source_mode(),
            "runtime":runtime,
            "deployment_revision_digest":hash(&self.config.deployment_revision)?,"ttl_ms":self.config.ttl_ms,
            "input_contracts":self.config.input_contracts});
        if self.lifecycle {
            metadata["lifecycle_storage_source_digest"] = json!(hash(&[
                include_str!("lifecycle.rs").replace("\r\n", "\n"),
                include_str!("lifecycle/service.rs").replace("\r\n", "\n"),
                include_str!("lifecycle/execution.rs").replace("\r\n", "\n"),
            ])?);
        }
        Ok(metadata)
    }
    fn source_mode(&self) -> &'static str {
        if self.lifecycle {
            crate::lifecycle::SOURCE_MODE
        } else {
            MODE
        }
    }
    pub(crate) fn for_lifecycle_backend_with_clock(
        repository: PgRepository,
        runtime: &'a Runtime,
        config: DecisionConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Result<Self, StorageError> {
        if config
            .input_contracts
            .iter()
            .any(|c| c.projection_revision != "bounded-direct-input-v2")
        {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        let mut service = Self::for_trusted_backend_with_clock(repository, runtime, config, clock)?;
        service.lifecycle = true;
        service.time_pool = service.repository.lifecycle_time_pool.clone();
        service.metadata = service.current_metadata()?;
        bounded(&service.metadata, 16384)?;
        service.assembly_digest = hash(&service.metadata)?;
        Ok(service)
    }
    pub fn for_trusted_backend(
        repository: PgRepository,
        runtime: &'a Runtime,
        config: DecisionConfig,
    ) -> Result<Self, StorageError> {
        Self::for_trusted_backend_with_clock(repository, runtime, config, Arc::new(ServerClock))
    }
    pub fn for_trusted_backend_with_clock(
        repository: PgRepository,
        runtime: &'a Runtime,
        config: DecisionConfig,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Result<Self, StorageError> {
        let metadata = json!({"contract_version":1,"source_mode":MODE,
            "runtime":runtime.preflight_source_metadata_for_backend().map_err(|_|StorageError::DecisionSourcesUnavailable)?,
            "deployment_revision_digest":hash(&config.deployment_revision)?,"ttl_ms":config.ttl_ms,
            "input_contracts":config.input_contracts});
        bounded(&metadata, 16384)?;
        let assembly_digest = hash(&metadata)?;
        let time_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(repository.pool.connect_options().as_ref().clone());
        Ok(Self {
            repository,
            runtime,
            clock,
            config,
            metadata,
            assembly_digest,
            source_instance_id: uuid::Uuid::new_v4().to_string(),
            time_pool,
            lifecycle: false,
        })
    }
    pub async fn evaluate_and_save(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
        request: &DecisionRequestId,
        phase: SecurityPhase,
    ) -> Result<StoredSecurityDecision, StorageError> {
        self.identity(session)?;
        if phase == SecurityPhase::PreCommit {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        let mut unit = self.open(session, task).await?;
        let result = unit.evaluate(operation, request, phase).await;
        unit.complete(result, false).await
    }
    pub async fn read(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
    ) -> Result<StoredSecurityDecision, StorageError> {
        self.access(session, task, id, None).await
    }
    /// Caller holds the task-first lock. Reads current display sources without advancing time.
    pub(crate) async fn read_display_sources(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        task: &TaskRecord,
        ops: &[crate::OperationRecord],
        ids: &[StoredDecisionId],
    ) -> Result<Vec<StoredSecurityDecision>, StorageError> {
        let identity = self.identity(session)?;
        if task.owner_id != identity.user_id || self.current_metadata()? != self.metadata {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        let digest = hash(&("safeflow-decision-session:v1", session.as_str()))?;
        let mut records = Vec::new();
        for id in ids {
            let row = sqlx::query("SELECT * FROM security_decisions WHERE task_id=$1 AND stored_decision_id=$2 FOR UPDATE")
                .bind(task.task_id.as_str()).bind(id.as_str()).fetch_optional(&mut **tx).await?
                .ok_or(StorageError::NotFound)?;
            let row = decode_row(&row)?;
            if row.session_digest != digest.as_str()
                || row.session_version != identity.binding_version
                || row.record.binding.user_id != identity.user_id
            {
                return Err(StorageError::NotFound);
            }
            if row.record.status != StoredDecisionStatus::Active {
                return Err(StorageError::DecisionInactive);
            }
            if row.record.assembly_digest != self.assembly_digest
                || row.record.assembly_metadata != self.metadata
            {
                return Err(StorageError::DecisionAssemblyChanged);
            }
            let binding = crate::lifecycle::binding_for_mode(
                tx,
                task,
                ops,
                &row.record.binding.operation_id,
                self.lifecycle,
            )
            .await?;
            if row.record.source_mode != self.source_mode()
                || row.record.snapshot_digest
                    != crate::lifecycle::snapshot_digest(task, ops, &binding)?
                || binding != row.record.binding
            {
                return Err(StorageError::DecisionBindingMismatch);
            }
            direct_arguments(self.runtime, &self.config, &task.plan, &row.record.binding)?;
            records.push(row.record);
        }
        let now = self.display_time(tx).await?.timestamp_millis();
        for record in &records {
            check_time(record, now)?;
        }
        if self.identity(session)? != identity {
            return Err(StorageError::Session);
        }
        if self.current_metadata()? != self.metadata {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        Ok(records)
    }
    pub async fn verify(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
        expected: &ConfirmationBinding,
    ) -> Result<StoredSecurityDecision, StorageError> {
        self.access(session, task, id, Some(expected)).await
    }

    /// Read both persisted sources in one task-first transaction. No Policy or Tool runs.
    /// `expected` is comparison data; sources and identity always come from trusted storage.
    pub async fn verify_with_confirmation(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
        confirmation: &ConfirmationRef,
        expected: &ConfirmationBinding,
    ) -> Result<VerifiedPreflightConfirmation, StorageError> {
        valid_id(confirmation.as_str())?;
        let mut unit = self.open(session, task).await?;
        let result = unit
            .load_confirmation_sources(id, confirmation, Some(expected))
            .await;
        match result {
            Ok((decision, confirmation, expires_at)) => {
                if let Err(error) = unit.final_guard(false).await {
                    unit.rollback().await;
                    return Err(error);
                }
                let checked_at = unit.last_precise_time.ok_or(StorageError::CorruptRecord)?;
                unit.tx
                    .take()
                    .ok_or(StorageError::TransactionAborted)?
                    .commit()
                    .await?;
                Ok(VerifiedPreflightConfirmation {
                    decision,
                    confirmation,
                    checked_at,
                    expires_at,
                })
            }
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }
    async fn access(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
        expected: Option<&ConfirmationBinding>,
    ) -> Result<StoredSecurityDecision, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = async {
            let row = unit.row(id).await?;
            unit.validate(&row).await?;
            if expected.is_some_and(|binding| binding != &row.record.binding) {
                return Err(StorageError::DecisionBindingMismatch);
            }
            unit.expected = Some(row.clone());
            Ok(row.record)
        }
        .await;
        unit.complete(result, false).await
    }
    pub async fn revoke(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
    ) -> Result<StoredSecurityDecision, StorageError> {
        self.end(session, task, id, false).await
    }
    /// Only current server facts/config/time can establish an invalidation reason.
    pub async fn invalidate_if_stale(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
    ) -> Result<StoredSecurityDecision, StorageError> {
        self.end(session, task, id, true).await
    }
    async fn end(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &StoredDecisionId,
        invalidate: bool,
    ) -> Result<StoredSecurityDecision, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = unit.end(id, invalidate).await;
        unit.complete(result, true).await
    }
    /// No complete projection, formal auth or decision source exists for Permit issuance here.
    pub async fn request_precommit_evidence(
        &self,
        session: &SessionRef,
        _task: &TaskId,
    ) -> Result<(), StorageError> {
        self.identity(session)?;
        Err(StorageError::DecisionSourcesUnavailable)
    }
    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        valid_id(session.as_str())?;
        let identity = self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(identity.user_id.as_str())?;
        db_version(identity.binding_version)?;
        Ok(identity)
    }
    async fn observe_time(&self) -> Result<i64, StorageError> {
        Ok(self.observe_precise_time().await?.timestamp_millis())
    }
    pub(crate) async fn observe_precise_time(
        &self,
    ) -> Result<chrono::DateTime<chrono::Utc>, StorageError> {
        // This separate, task-lock-free transaction stores time only, including failed requests.
        let table = format!(
            "\"{}\".security_decision_time_fence",
            self.repository.namespace
        );
        let mut tx = self.time_pool.begin().await?;
        sqlx::query(&format!("INSERT INTO {table} (scope,last_observed_ms) VALUES (1,0) ON CONFLICT (scope) DO NOTHING")).execute(&mut *tx).await?;
        let (previous_ms, previous_us): (i64, i64) = sqlx::query_as(&format!(
            "SELECT last_observed_ms,last_observed_us FROM {table} WHERE scope=1 FOR UPDATE"
        ))
        .fetch_one(&mut *tx)
        .await?;
        if previous_ms < 0 || previous_us < 0 {
            return Err(StorageError::CorruptRecord);
        }
        // If an older writer advanced only milliseconds, reject its whole unknown bucket.
        let previous = if previous_ms > previous_us / 1000 {
            previous_ms
                .checked_mul(1000)
                .and_then(|value| value.checked_add(999))
                .ok_or(StorageError::Invalid("decision clock fence out of range"))?
        } else {
            previous_us
        };
        let now = timestamp(self.clock.as_ref())?;
        if now.timestamp_micros() < 0 || now.timestamp_micros() < previous {
            return Err(StorageError::Invalid("decision clock moved backwards"));
        }
        sqlx::query(&format!(
            "UPDATE {table} SET last_observed_ms=$1,last_observed_us=$2 WHERE scope=1"
        ))
        .bind(now.timestamp_millis())
        .bind(now.timestamp_micros())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(now)
    }
    async fn open(
        &self,
        session: &SessionRef,
        task: &TaskId,
    ) -> Result<DecisionUnit<'a>, StorageError> {
        valid_id(task.as_str())?;
        let identity = self.identity(session)?;
        let observed = self.observe_time().await?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SELECT set_config('search_path',$1,true)")
            .bind(&self.repository.namespace)
            .execute(&mut *tx)
            .await?;
        let task =
            crate::repository::lock_task_for_mode(&mut tx, &identity.user_id, task, self.lifecycle)
                .await?;
        let unit = DecisionUnit {
            tx: Some(tx),
            service: self.clone(),
            session: session.clone(),
            identity,
            task,
            expected: None,
            expected_confirmation: None,
            expected_precommit: None,
            cleanup_snapshot: None,
            last_time: observed,
            last_precise_time: None,
        };
        unit.session_guard()?;
        Ok(unit)
    }
    async fn display_time(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<chrono::DateTime<chrono::Utc>, StorageError> {
        let fence: Option<(i64, i64)> = sqlx::query_as(
            "SELECT last_observed_ms,last_observed_us FROM security_decision_time_fence WHERE scope=1"
        ).fetch_optional(&mut **tx).await?;
        let now = timestamp(self.clock.as_ref())?;
        if let Some((ms, us)) = fence {
            let previous = if ms > us / 1000 {
                ms.checked_mul(1000)
                    .and_then(|v| v.checked_add(999))
                    .ok_or(StorageError::CorruptRecord)?
            } else {
                us
            };
            if ms < 0 || us < 0 || now.timestamp_micros() < previous {
                return Err(StorageError::Invalid("decision clock moved backwards"));
            }
        }
        Ok(now)
    }
}
fn bounded(value: &impl Serialize, limit: usize) -> Result<(), StorageError> {
    if serde_json::to_vec(value)
        .map_err(|_| StorageError::CorruptRecord)?
        .len()
        > limit
    {
        return Err(StorageError::Invalid("decision JSON size"));
    }
    Ok(())
}
fn direct_arguments(
    runtime: &Runtime,
    config: &DecisionConfig,
    plan: &AgentPlan,
    operation: &ConfirmationBinding,
) -> Result<Value, StorageError> {
    bounded(plan, 65536)?;
    if plan.nodes.len() > 256 {
        return Err(StorageError::Invalid("decision plan size"));
    }
    let node = plan
        .nodes
        .iter()
        .find(|n| n.id() == &operation.node_id)
        .ok_or(StorageError::NotFound)?;
    let PlanNode::Tool {
        tool, arguments, ..
    } = node
    else {
        return Err(StorageError::DecisionSourcesUnavailable);
    };
    bounded(arguments, 4096)?;
    let contract = config
        .input_contracts
        .iter()
        .find(|c| c.tool_name == *tool)
        .ok_or(StorageError::DecisionSourcesUnavailable)?;
    let normalized = if operation.lifecycle_anchor.is_some() {
        runtime.lifecycle_arguments_for_backend(tool, arguments, contract)
    } else {
        runtime.direct_preflight_arguments_for_backend(tool, arguments, contract)
    }
    .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
    if normalized != *arguments || hash(&normalized)? != operation.arguments_hash {
        return Err(StorageError::DecisionBindingMismatch);
    }
    Ok(normalized)
}
fn safe_decision(decision: &SecurityDecision) -> Result<(), StorageError> {
    decision
        .validate()
        .map_err(|_| StorageError::CorruptRecord)?;
    bounded(decision, 16384)?;
    valid_id(decision.decision_id.as_str())?;
    valid_id(decision.policy_version.as_str())?;
    if decision.outcome == SecurityOutcome::ReadyToExecute
        || decision.findings.len() > 16
        || decision.required_gates.len() > 2
    {
        return Err(StorageError::DecisionSourcesUnavailable);
    }
    for evaluated in &decision.findings {
        let f = &evaluated.finding;
        // Preserve exact safe output; reject unapproved free-form evidence rather than silently redact.
        valid_id(f.check_id.as_str())?;
        valid_id(f.source.as_str())?;
        valid_id(f.policy_version.as_str())?;
        valid_id(f.reason_code.as_str())?;
        if f.kind == safeflow_contracts::FindingKind::RiskSignal {
            safeflow_runtime::validate_count_observation(
                f,
                &decision.operation_id,
                decision.facts_version,
            )
            .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
            let rule = safeflow_runtime::demo_risk_definitions()
                .into_iter()
                .find(|r| r.check_id == f.check_id.as_str())
                .ok_or(StorageError::DecisionSourcesUnavailable)?;
            let expected = match f.observed_value.as_ref().and_then(Value::as_u64) {
                Some(n) if n >= rule.distinct_count_threshold.unwrap_or(u64::MAX) => rule.weight,
                Some(_) => 0,
                None if rule.missing_data_policy
                    == safeflow_runtime::MissingRiskDataPolicy::SkipWithAudit =>
                {
                    0
                }
                _ => return Err(StorageError::DecisionSourcesUnavailable),
            };
            if evaluated.applied_weight != Some(rule.weight) || evaluated.score_delta != expected {
                return Err(StorageError::DecisionSourcesUnavailable);
            }
            continue;
        }
        if evaluated.applied_weight.is_some() || evaluated.score_delta != 0 {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
        if f.observed_value.is_some()
            || !f.evidence_refs.is_empty()
            || !f.event_ids.is_empty()
            || f.dedup_key.is_some()
            || f.window_start_ms.is_some()
            || f.window_end_ms.is_some()
        {
            return Err(StorageError::DecisionSourcesUnavailable);
        }
    }
    let score = decision
        .findings
        .iter()
        .fold(0u32, |sum, f| sum.saturating_add(f.score_delta).min(100));
    if decision.risk_score != score
        || decision.risk_tier != safeflow_runtime::risk_tier_for_score(score)
    {
        return Err(StorageError::DecisionSourcesUnavailable);
    }
    Ok(())
}
#[derive(Clone, PartialEq)]
struct DecisionRow {
    record: StoredSecurityDecision,
    session_digest: String,
    session_version: u64,
}
struct DecisionUnit<'a> {
    tx: Option<Transaction<'static, Postgres>>,
    service: DecisionService<'a>,
    session: SessionRef,
    identity: SessionBinding,
    task: TaskRecord,
    expected: Option<DecisionRow>,
    expected_confirmation: Option<StoredConfirmation>,
    expected_precommit: Option<safeflow_contracts::PreCommitSecurityRequestV2>,
    cleanup_snapshot: Option<ArgumentsHash>,
    last_time: i64,
    last_precise_time: Option<chrono::DateTime<chrono::Utc>>,
}
impl DecisionUnit<'_> {
    async fn load_confirmation_sources(
        &mut self,
        id: &StoredDecisionId,
        confirmation: &ConfirmationRef,
        expected: Option<&ConfirmationBinding>,
    ) -> Result<
        (
            StoredSecurityDecision,
            ConfirmationRecord,
            chrono::DateTime<chrono::Utc>,
        ),
        StorageError,
    > {
        valid_id(confirmation.as_str())?;
        let row = self.row(id).await?;
        self.validate(&row).await?;
        if expected.is_some_and(|binding| binding != &row.record.binding) {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let task = self.task.task_id.clone();
        let stored = read_stored_confirmation(self.tx()?, &task, confirmation).await?;
        let now = self.now_precise().await?;
        self.check_confirmation(&stored, &row.record.binding, now)?;
        let expires_at = chrono::DateTime::from_timestamp_millis(row.record.expires_at_ms)
            .ok_or(StorageError::CorruptRecord)?
            .min(stored.record.expires_at);
        self.expected = Some(row.clone());
        self.expected_confirmation = Some(stored.clone());
        Ok((row.record, stored.record, expires_at))
    }
    fn tx(&mut self) -> Result<&mut Transaction<'static, Postgres>, StorageError> {
        self.tx.as_mut().ok_or(StorageError::TransactionAborted)
    }
    fn session_guard(&self) -> Result<(), StorageError> {
        if self.service.identity(&self.session)? != self.identity {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    fn session_digest(&self) -> Result<String, StorageError> {
        Ok(
            hash(&("safeflow-decision-session:v1", self.session.as_str()))?
                .as_str()
                .into(),
        )
    }
    async fn now(&mut self) -> Result<i64, StorageError> {
        Ok(self.now_precise().await?.timestamp_millis())
    }
    async fn now_precise(&mut self) -> Result<chrono::DateTime<chrono::Utc>, StorageError> {
        let now = self.service.observe_precise_time().await?;
        if now.timestamp_millis() < self.last_time
            || self
                .last_precise_time
                .is_some_and(|previous| now < previous)
        {
            return Err(StorageError::Invalid("decision clock moved backwards"));
        }
        self.last_time = now.timestamp_millis();
        self.last_precise_time = Some(now);
        self.session_guard()?;
        Ok(now)
    }

    fn check_confirmation(
        &self,
        stored: &StoredConfirmation,
        binding: &ConfirmationBinding,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StorageError> {
        stored.check_session(&ConfirmationSession {
            binding: self.identity.clone(),
            digest: hash(&("safeflow-confirmation-session:v1", self.session.as_str()))?,
        })?;
        if stored.record.binding != *binding {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        check_active_at(&stored.record, now)?;
        if stored.record.status != ConfirmationStatus::Confirmed {
            return Err(StorageError::ConfirmationRequired);
        }
        if stored.record.confirmed_at.is_none()
            || stored.record.revoked_at.is_some()
            || stored.record.expires_at <= stored.record.prepared_at
            || stored.record.confirmed_at.is_some_and(|confirmed| {
                confirmed < stored.record.prepared_at || confirmed >= stored.record.expires_at
            })
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(())
    }
    async fn snapshot(
        &mut self,
        operation: &OperationId,
    ) -> Result<(ConfirmationBinding, ArgumentsHash, Value), StorageError> {
        valid_id(operation.as_str())?;
        let owner = self.identity.user_id.clone();
        let id = self.task.task_id.clone();
        let lifecycle = self.service.lifecycle;
        let task =
            crate::repository::lock_task_for_mode(self.tx()?, &owner, &id, lifecycle).await?;
        let ops = operations(self.tx()?, &id).await?;
        let binding =
            crate::lifecycle::binding_for_mode(self.tx()?, &task, &ops, operation, lifecycle)
                .await?;
        u32::try_from(binding.facts_version)
            .map_err(|_| StorageError::Invalid("decision facts version exceeds contract"))?;
        let arguments = direct_arguments(
            self.service.runtime,
            &self.service.config,
            &task.plan,
            &binding,
        )?;
        let digest = crate::lifecycle::snapshot_digest(&task, &ops, &binding)?;
        self.session_guard()?;
        Ok((binding, digest, arguments))
    }
    async fn row(&mut self, id: &StoredDecisionId) -> Result<DecisionRow, StorageError> {
        let task = self.task.task_id.clone();
        let row=sqlx::query("SELECT * FROM security_decisions WHERE task_id=$1 AND stored_decision_id=$2 FOR UPDATE")
            .bind(task.as_str()).bind(id.as_str()).fetch_optional(&mut **self.tx()?).await?.ok_or(StorageError::NotFound)?;
        let row = decode_row(&row)?;
        self.owner(&row)?;
        Ok(row)
    }
    fn owner(&self, row: &DecisionRow) -> Result<(), StorageError> {
        if row.record.binding.user_id != self.identity.user_id
            || row.record.binding.task_id != self.task.task_id
            || row.session_digest != self.session_digest()?
            || row.session_version != self.identity.binding_version
        {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }
    async fn validate(&mut self, row: &DecisionRow) -> Result<(), StorageError> {
        if row.record.status != StoredDecisionStatus::Active {
            return Err(StorageError::DecisionInactive);
        }
        check_time(&row.record, self.now().await?)?;
        if row.record.assembly_digest != self.service.assembly_digest
            || row.record.assembly_metadata != self.service.metadata
        {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        let (binding, digest, _) = self.snapshot(&row.record.binding.operation_id).await?;
        if binding != row.record.binding || digest != row.record.snapshot_digest {
            return Err(StorageError::DecisionBindingMismatch);
        }
        Ok(())
    }
    async fn emit(
        &mut self,
        row: &DecisionRow,
        kind: PersistentAuditKind,
    ) -> Result<(), StorageError> {
        let task = self.task.clone();
        let ops = operations(self.tx()?, &task.task_id).await?;
        let details = json!({"stored_decision_id":row.record.stored_decision_id,"request_id":row.record.request_id,
            "evidence_kind":"PREFLIGHT_ONLY","source_mode":row.record.source_mode,"phase":row.record.phase,
            "outcome":row.record.decision.outcome,"assembly_digest":row.record.assembly_digest,"decision_digest":row.record.decision_digest});
        audit(
            self.tx()?,
            &task,
            ops.iter()
                .find(|op| op.operation_id == row.record.binding.operation_id),
            kind,
            details,
        )
        .await?;
        Ok(())
    }
    async fn evaluate(
        &mut self,
        operation: &OperationId,
        request: &DecisionRequestId,
        phase: SecurityPhase,
    ) -> Result<StoredSecurityDecision, StorageError> {
        let (binding, snapshot_digest, arguments) = self.snapshot(operation).await?;
        let existing =
            sqlx::query("SELECT * FROM security_decisions WHERE request_id=$1 FOR UPDATE")
                .bind(request.as_str())
                .fetch_optional(&mut **self.tx()?)
                .await?;
        if let Some(existing) = existing {
            let row = decode_row(&existing)?;
            self.owner(&row)?;
            if row.record.binding != binding
                || row.record.phase != phase
                || row.record.snapshot_digest != snapshot_digest
                || row.record.assembly_digest != self.service.assembly_digest
            {
                return Err(StorageError::DecisionConflict);
            }
            self.validate(&row).await?;
            let record = row.record.clone();
            self.expected = Some(row);
            return Ok(record);
        }
        // open() holds the owning task row lock through commit. Reuse the original scoring
        // snapshot even when a caller creates a new transport request ID. No second store.
        if phase == SecurityPhase::PreAuth {
            let original = sqlx::query("SELECT * FROM security_decisions WHERE task_id=$1 AND operation_id=$2 AND facts_version=$3 AND phase='PRE_AUTH' ORDER BY evaluated_at_ms,stored_decision_id LIMIT 1 FOR UPDATE")
                .bind(binding.task_id.as_str()).bind(operation.as_str()).bind(db_version(binding.facts_version)?)
                .fetch_optional(&mut **self.tx()?).await?;
            if let Some(original) = original {
                let row = decode_row(&original)?;
                let scored_before = row.record.assembly_metadata["runtime"]["central_scoring"]
                    ["bindings"]
                    .as_array()
                    .is_some_and(|bindings| {
                        bindings
                            .iter()
                            .any(|b| b["tool"] == binding.tool_name.as_str())
                    });
                if scored_before
                    || self
                        .service
                        .runtime
                        .has_central_scoring_for_backend(&binding.tool_name)
                {
                    self.owner(&row)?;
                    self.validate(&row).await?;
                    let record = row.record.clone();
                    self.expected = Some(row);
                    return Ok(record);
                }
            }
        }
        let evaluated_at_ms = self.now().await?;
        let expires_at_ms = evaluated_at_ms
            .checked_add(self.service.config.ttl_ms)
            .ok_or(StorageError::Invalid("decision expiry overflow"))?;
        let runtime = self.service.runtime;
        let contract = self
            .service
            .config
            .input_contracts
            .iter()
            .find(|c| c.tool_name == binding.tool_name)
            .ok_or(StorageError::DecisionSourcesUnavailable)?;
        let request_input = OperationSecurityRequest {
            contract_version: SECURITY_CONTRACT_VERSION,
            task_id: binding.task_id.clone(),
            node_id: binding.node_id.clone(),
            operation_id: binding.operation_id.clone(),
            facts_version: u32::try_from(binding.facts_version)
                .map_err(|_| StorageError::Invalid("decision facts range"))?,
            phase,
            tool_name: binding.tool_name.clone(),
            plan_version: binding.plan_version,
            session_ref: self.session.clone(),
            normalized_arguments: arguments,
            arguments_hash: binding.arguments_hash.clone(),
            idempotency_key: None,
            confirmation_ref: None,
            auth_grant_ref: None,
        };
        let input_digest =
            hash(&serde_json::to_value(&request_input).map_err(|_| StorageError::CorruptRecord)?)?;
        let result = if self.service.lifecycle {
            runtime
                .review_lifecycle_security_for_backend(&self.task.plan, request_input, contract)
                .await
        } else {
            runtime
                .review_preflight_security_for_backend(&self.task.plan, request_input, contract)
                .await
        }
        .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        self.session_guard()?;
        self.now().await?;
        if result.session_binding() != &self.identity
            || result.request_digest() != &input_digest
            || result.decision().operation_id != binding.operation_id
            || result.decision().facts_version as u64 != binding.facts_version
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        let decision = result.decision().clone();
        safe_decision(&decision)?;
        let metadata = self.service.metadata.clone();
        let original_expiry = decision.expires_at_ms;
        let row = DecisionRow {
            record: StoredSecurityDecision {
                contract_version: 1,
                stored_decision_id: StoredDecisionId::new_server_id(),
                request_id: request.clone(),
                evidence_kind: "PREFLIGHT_ONLY",
                source_mode: self.service.source_mode(),
                source_instance_id: self.service.source_instance_id.clone(),
                binding: binding.clone(),
                phase,
                snapshot_digest,
                assembly_digest: self.service.assembly_digest.clone(),
                assembly_metadata: metadata,
                decision_digest: hash(&decision)?,
                decision,
                plan_review_digest: hash(result.plan_review())?,
                evaluated_at_ms,
                expires_at_ms: expires_at_ms.min(original_expiry),
                status: StoredDecisionStatus::Active,
                ended_at_ms: None,
                invalidation_reason: None,
            },
            session_digest: self.session_digest()?,
            session_version: self.identity.binding_version,
        };
        check_time(&row.record, self.now().await?)?;
        insert_decision(self.tx()?, &row).await?;
        self.emit(&row, PersistentAuditKind::SecurityDecisionRecorded)
            .await?;
        let record = row.record.clone();
        self.expected = Some(row);
        Ok(record)
    }
    async fn end(
        &mut self,
        id: &StoredDecisionId,
        invalidate: bool,
    ) -> Result<StoredSecurityDecision, StorageError> {
        let task = self.task.clone();
        let ops = operations(self.tx()?, &task.task_id).await?;
        self.cleanup_snapshot = Some(hash(&(task, ops))?);
        let mut row = self.row(id).await?;
        let now = self.now().await?;
        if now < row.record.evaluated_at_ms || row.record.ended_at_ms.is_some_and(|t| now < t) {
            return Err(StorageError::Invalid("decision clock precedes record"));
        }
        if row.record.status != StoredDecisionStatus::Active {
            let record = row.record.clone();
            self.expected = Some(row);
            return Ok(record);
        }
        let reason = if invalidate {
            if row.record.assembly_digest != self.service.assembly_digest {
                Some(DecisionInvalidationReason::AssemblyChanged)
            } else if now >= row.record.expires_at_ms {
                Some(DecisionInvalidationReason::Expired)
            } else {
                if self.cleanup_snapshot.as_ref() == Some(&row.record.snapshot_digest) {
                    return Err(StorageError::DecisionStillCurrent);
                }
                Some(DecisionInvalidationReason::SnapshotChanged)
            }
        } else {
            None
        };
        row.record.status = if invalidate {
            StoredDecisionStatus::Invalidated
        } else {
            StoredDecisionStatus::Revoked
        };
        row.record.ended_at_ms = Some(now);
        row.record.invalidation_reason = reason;
        let changed=sqlx::query("UPDATE security_decisions SET status=$1,ended_at_ms=$2,invalidation_reason=$3 WHERE stored_decision_id=$4 AND status='ACTIVE'")
            .bind(if invalidate {"INVALIDATED"}else{"REVOKED"}).bind(now).bind(reason.map(reason_text)).bind(id.as_str()).execute(&mut **self.tx()?).await?;
        if changed.rows_affected() != 1 {
            return Err(StorageError::DecisionConflict);
        }
        self.emit(
            &row,
            if invalidate {
                PersistentAuditKind::SecurityDecisionInvalidated
            } else {
                PersistentAuditKind::SecurityDecisionRevoked
            },
        )
        .await?;
        let record = row.record.clone();
        self.expected = Some(row);
        Ok(record)
    }
    async fn complete<T>(
        mut self,
        result: Result<T, StorageError>,
        cleanup: bool,
    ) -> Result<T, StorageError> {
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                self.rollback().await;
                return Err(e);
            }
        };
        if let Err(e) = self.final_guard(cleanup).await {
            self.rollback().await;
            return Err(e);
        }
        self.tx
            .take()
            .ok_or(StorageError::TransactionAborted)?
            .commit()
            .await?;
        Ok(value)
    }
    async fn rollback(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.rollback().await;
        }
    }
    async fn final_guard(&mut self, cleanup: bool) -> Result<(), StorageError> {
        self.session_guard()?;
        let expected = self.expected.clone().ok_or(StorageError::CorruptRecord)?;
        if self.service.current_metadata()? != self.service.metadata {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        let owner = self.identity.user_id.clone();
        let task = self.task.task_id.clone();
        let lifecycle = self.service.lifecycle;
        if crate::repository::lock_task_for_mode(self.tx()?, &owner, &task, lifecycle).await?
            != self.task
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        sqlx::query(
            "SELECT operation_id FROM operations WHERE task_id=$1 ORDER BY operation_id FOR UPDATE",
        )
        .bind(task.as_str())
        .fetch_all(&mut **self.tx()?)
        .await?;
        let row = self.row(&expected.record.stored_decision_id).await?;
        if row != expected {
            return Err(StorageError::DecisionBindingMismatch);
        }
        if let Some(expected_confirmation) = self.expected_confirmation.clone() {
            let current = read_stored_confirmation(
                self.tx()?,
                &task,
                &expected_confirmation.record.confirmation_ref,
            )
            .await?;
            if current != expected_confirmation {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
        }
        if !cleanup {
            self.validate(&row).await?;
            if let Some(check) = self.expected_precommit.clone() {
                self.check_precommit_request(&check, &row.record).await?;
            }
        } else {
            let task = self.task.clone();
            let ops = operations(self.tx()?, &task.task_id).await?;
            if self.cleanup_snapshot.as_ref() != Some(&hash(&(task, ops))?) {
                return Err(StorageError::DecisionBindingMismatch);
            }
        }
        let precise_now = self.now_precise().await?;
        let now = precise_now.timestamp_millis();
        if self.service.current_metadata()? != self.service.metadata {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        self.session_guard()?;
        if !cleanup {
            check_time(&row.record, now)?;
            if let Some(stored) = &self.expected_confirmation {
                self.check_confirmation(stored, &row.record.binding, precise_now)?;
            }
        } else if now < row.record.ended_at_ms.unwrap_or(row.record.evaluated_at_ms) {
            return Err(StorageError::Invalid("decision clock precedes end"));
        }
        Ok(())
    }
}
fn phase_text(phase: SecurityPhase) -> &'static str {
    match phase {
        SecurityPhase::PreTool => "PRE_TOOL",
        SecurityPhase::PreAuth => "PRE_AUTH",
        SecurityPhase::PreCommit => "PRE_COMMIT",
    }
}
fn reason_text(reason: DecisionInvalidationReason) -> &'static str {
    match reason {
        DecisionInvalidationReason::SnapshotChanged => "SNAPSHOT_CHANGED",
        DecisionInvalidationReason::AssemblyChanged => "ASSEMBLY_CHANGED",
        DecisionInvalidationReason::Expired => "EXPIRED",
    }
}
pub(crate) fn check_time(record: &StoredSecurityDecision, now: i64) -> Result<(), StorageError> {
    if now < record.evaluated_at_ms {
        return Err(StorageError::Invalid("decision clock precedes evaluation"));
    }
    if now >= record.expires_at_ms {
        return Err(StorageError::DecisionExpired);
    }
    Ok(())
}
fn decode_row(row: &PgRow) -> Result<DecisionRow, StorageError> {
    let mode = row.try_get::<String, _>("source_mode")?;
    let mode = match mode.as_str() {
        MODE => MODE,
        crate::lifecycle::SOURCE_MODE => crate::lifecycle::SOURCE_MODE,
        payment::MODE => payment::MODE,
        _ => return Err(StorageError::CorruptRecord),
    };
    if row.try_get::<i16, _>("contract_version")? != 1
        || row.try_get::<String, _>("evidence_kind")? != "PREFLIGHT_ONLY"
    {
        return Err(StorageError::CorruptRecord);
    }
    let binding = binding_row(row)?;
    if binding.lifecycle_anchor.is_some() != (mode != MODE) {
        return Err(StorageError::CorruptRecord);
    }
    let decision = row.try_get::<Json<SecurityDecision>, _>("decision")?.0;
    safe_decision(&decision)?;
    let metadata = row.try_get::<Json<Value>, _>("assembly_metadata")?.0;
    bounded(&metadata, 16384)?;
    let assembly_digest = ArgumentsHash::parse(row.try_get::<String, _>("assembly_digest")?)
        .map_err(|_| StorageError::CorruptRecord)?;
    let decision_digest = ArgumentsHash::parse(row.try_get::<String, _>("decision_digest")?)
        .map_err(|_| StorageError::CorruptRecord)?;
    if hash(&binding)?.as_str() != row.try_get::<String, _>("binding_digest")?
        || hash(&decision)? != decision_digest
        || hash(&metadata)? != assembly_digest
        || decision.operation_id != binding.operation_id
        || decision.facts_version as u64 != binding.facts_version
        || decision.decision_id.as_str() != row.try_get::<String, _>("source_decision_id")?
    {
        return Err(StorageError::CorruptRecord);
    }
    let parse = |name| {
        serde_json::from_value(Value::String(row.try_get::<String, _>(name)?))
            .map_err(|_| StorageError::CorruptRecord)
    };
    let phase: SecurityPhase = parse("phase")?;
    if phase == SecurityPhase::PreCommit {
        return Err(StorageError::DecisionSourcesUnavailable);
    }
    let status: StoredDecisionStatus =
        serde_json::from_value(Value::String(row.try_get("status")?))
            .map_err(|_| StorageError::CorruptRecord)?;
    let invalidation_reason = row
        .try_get::<Option<String>, _>("invalidation_reason")?
        .map(|s| serde_json::from_value(Value::String(s)).map_err(|_| StorageError::CorruptRecord))
        .transpose()?;
    let evaluated_at_ms: i64 = row.try_get("evaluated_at_ms")?;
    let expires_at_ms: i64 = row.try_get("expires_at_ms")?;
    let ended_at_ms: Option<i64> = row.try_get("ended_at_ms")?;
    let instance: String = row.try_get("source_instance_id")?;
    if uuid::Uuid::parse_str(&instance)
        .map_err(|_| StorageError::CorruptRecord)?
        .to_string()
        != instance
        || evaluated_at_ms < 0
        || expires_at_ms <= evaluated_at_ms
        || expires_at_ms > decision.expires_at_ms
        || (status == StoredDecisionStatus::Active) != ended_at_ms.is_none()
        || (status == StoredDecisionStatus::Invalidated) != invalidation_reason.is_some()
        || ended_at_ms.is_some_and(|t| t < evaluated_at_ms)
    {
        return Err(StorageError::CorruptRecord);
    }
    if mode == payment::MODE {
        payment::validate_historical(&binding, &metadata, &decision, phase)?;
    } else {
        let profiles = metadata["runtime"]["profiles"]
            .as_array()
            .ok_or(StorageError::CorruptRecord)?;
        let profile = profiles
            .iter()
            .find(|p| p["tool_name"] == binding.tool_name.as_str())
            .ok_or(StorageError::CorruptRecord)?;
        if profile["policy_version"] != decision.policy_version.as_str()
            || profile["minimum_level"]
                != serde_json::to_value(decision.minimum_level)
                    .map_err(|_| StorageError::CorruptRecord)?
        {
            return Err(StorageError::CorruptRecord);
        }
    }
    Ok(DecisionRow {
        record: StoredSecurityDecision {
            contract_version: 1,
            stored_decision_id: StoredDecisionId::parse(
                row.try_get::<String, _>("stored_decision_id")?,
            )?,
            request_id: DecisionRequestId::parse(row.try_get::<String, _>("request_id")?)?,
            evidence_kind: "PREFLIGHT_ONLY",
            source_mode: mode,
            source_instance_id: instance,
            binding,
            phase,
            snapshot_digest: ArgumentsHash::parse(row.try_get::<String, _>("snapshot_digest")?)
                .map_err(|_| StorageError::CorruptRecord)?,
            assembly_digest,
            assembly_metadata: metadata,
            decision,
            decision_digest,
            plan_review_digest: ArgumentsHash::parse(
                row.try_get::<String, _>("plan_review_digest")?,
            )
            .map_err(|_| StorageError::CorruptRecord)?,
            evaluated_at_ms,
            expires_at_ms,
            status,
            ended_at_ms,
            invalidation_reason,
        },
        session_digest: row.try_get("session_digest")?,
        session_version: version(row, "session_binding_version")?,
    })
}

async fn insert_decision(
    tx: &mut Transaction<'_, Postgres>,
    row: &DecisionRow,
) -> Result<(), StorageError> {
    let r = &row.record;
    let b = &r.binding;
    safe_decision(&r.decision)?;
    bounded(&r.assembly_metadata, 16384)?;
    let inserted=sqlx::query("INSERT INTO security_decisions (stored_decision_id,request_id,contract_version,evidence_kind,source_mode,source_instance_id,source_decision_id,phase,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,snapshot_digest,session_digest,session_binding_version,assembly_digest,assembly_metadata,decision_digest,decision,plan_review_digest,evaluated_at_ms,expires_at_ms,status,lifecycle_anchor) VALUES ($1,$2,1,'PREFLIGHT_ONLY',$27,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,'ACTIVE',$28) ON CONFLICT (request_id) DO NOTHING")
        .bind(r.stored_decision_id.as_str()).bind(r.request_id.as_str()).bind(&r.source_instance_id).bind(r.decision.decision_id.as_str()).bind(phase_text(r.phase))
        .bind(b.user_id.as_str()).bind(b.task_id.as_str()).bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str())
        .bind(db_version(b.task_revision)?).bind(b.plan_version as i64).bind(db_version(b.operation_revision)?).bind(db_version(b.facts_version)?)
        .bind(b.arguments_hash.as_str()).bind(hash(b)?.as_str()).bind(r.snapshot_digest.as_str()).bind(&row.session_digest).bind(db_version(row.session_version)?)
        .bind(r.assembly_digest.as_str()).bind(Json(&r.assembly_metadata)).bind(r.decision_digest.as_str()).bind(Json(&r.decision)).bind(r.plan_review_digest.as_str())
        .bind(r.evaluated_at_ms).bind(r.expires_at_ms).bind(r.source_mode)
        .bind(b.lifecycle_anchor.as_ref().map(|value| value.as_str())).execute(&mut **tx).await?;
    if inserted.rows_affected() != 1 {
        return Err(StorageError::DecisionConflict);
    }
    Ok(())
}

pub(crate) async fn read_historical_decision(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskId,
    id: &StoredDecisionId,
) -> Result<StoredSecurityDecision, StorageError> {
    let row =
        sqlx::query("SELECT * FROM security_decisions WHERE task_id=$1 AND stored_decision_id=$2")
            .bind(task.as_str())
            .bind(id.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::CorruptRecord)?;
    Ok(decode_row(&row)?.record)
}
