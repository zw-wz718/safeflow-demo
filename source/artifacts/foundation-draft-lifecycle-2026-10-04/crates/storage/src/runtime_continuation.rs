//! Explicit continuation of a settled, original read-only plan. Claims are never retried.
use crate::models::{db_version, hash, valid_id, validate_checkpoint};
use crate::repository::{audit, guarded, lock_task, operations, update_task};
use crate::runtime_execution::{check_result_bodies, Receipt};
use crate::{
    PersistentAuditKind, PgRepository, RecoveryPoint, RuntimeReadResultReference,
    RuntimeTaskExecutionError, RuntimeTaskExecutionStatus, RuntimeTaskVersion, StorageError,
    TaskRecord,
};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, NodeId, PlanNode, SessionRef, TaskEvent, TaskId, TaskState,
    ToolResult, UserId,
};
use safeflow_runtime::{
    PreflightInputContract, ReadOnlyContinuationSource, Runtime, SessionBinding,
    VerifiedReadOnlyExecution, READ_ONLY_CONTINUATION_CONTRACT_VERSION,
    READ_ONLY_EXECUTION_CONTRACT_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{types::Json, Postgres, Row, Transaction};
use std::collections::HashSet;

type Guard<'a> = &'a (dyn Fn() -> Result<(), StorageError> + Send + Sync);

/// Backend reference view, never an HTTP DTO or a progress command. Replayed receipts
/// describe their historical segment; current_task_revision identifies the current head.
/// ```compile_fail
/// let forged: safeflow_storage::RuntimeTaskContinuationView = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeTaskContinuationView {
    pub contract_version: u16,
    pub task: TaskRecord,
    pub execution_status: RuntimeTaskExecutionStatus,
    pub result_references: Vec<RuntimeReadResultReference>,
    pub results: Option<Vec<ToolResult>>,
    pub segment_claim_id: Option<String>,
    pub replayed: bool,
    pub current_task_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    user_id: UserId,
    binding_version: u64,
}
/// Private persistence evidence. The service never accepts a caller-supplied source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    contract_version: u16,
    id: String,
    source_task: TaskRecord,
    claimed_task: TaskRecord,
    waiting_node: NodeId,
    prior_receipt_hash: ArgumentsHash,
    session_hash: ArgumentsHash,
    binding: Binding,
    bindings_hash: ArgumentsHash,
}
impl ReadOnlyContinuationSource for Claim {
    fn plan(&self) -> &AgentPlan {
        &self.source_task.plan
    }
    fn owner(&self) -> &UserId {
        &self.source_task.owner_id
    }
    fn facts_version(&self) -> u64 {
        self.source_task.facts_version
    }
    fn completed_nodes(&self) -> &[NodeId] {
        &self.source_task.completed_nodes
    }
    fn accepted_waiting_node(&self) -> &NodeId {
        &self.waiting_node
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentReceipt {
    contract_version: u16,
    claim_id: String,
    prior_receipt_hash: ArgumentsHash,
    task: TaskRecord,
    status: RuntimeTaskExecutionStatus,
    bindings_hash: ArgumentsHash,
    segment_results_hash: Option<ArgumentsHash>,
    result_references: Vec<RuntimeReadResultReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    results: Option<Vec<ToolResult>>,
}
#[derive(Debug, Clone, PartialEq)]
struct Chain {
    view: RuntimeTaskContinuationView,
    tip_hash: ArgumentsHash,
    bindings_hash: ArgumentsHash,
    segments: Vec<(Claim, Option<SegmentReceipt>)>,
}

/// Trusted backend entry; the persisted Runtime source remains private.
/// ```compile_fail
/// use safeflow_storage::runtime_continuation::Claim;
/// ```
pub struct RuntimeTaskContinuationService<'a> {
    repository: PgRepository,
    runtime: &'a Runtime,
    contracts: Vec<PreflightInputContract>,
}
impl<'a> RuntimeTaskContinuationService<'a> {
    pub fn for_trusted_backend(
        repository: PgRepository,
        runtime: &'a Runtime,
        contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, RuntimeTaskExecutionError> {
        // Reuse initial execution's assembly validation; this does not execute or claim.
        crate::RuntimeTaskExecutionService::for_trusted_backend(
            repository.clone(),
            runtime,
            contracts.clone(),
        )?;
        Ok(Self {
            repository,
            runtime,
            contracts,
        })
    }

    /// Only the explicit continue command is accepted. No answer, plan edits, completed
    /// nodes or ToolResult bodies are accepted, and no financial confirmation is implied.
    pub async fn continue_task(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        waiting_node: &NodeId,
        expected: RuntimeTaskVersion,
    ) -> Result<RuntimeTaskContinuationView, RuntimeTaskExecutionError> {
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        valid_id(waiting_node.as_str())?;
        let binding = self.runtime.session_binding_for_backend(session)?;
        let session_guard = || self.check_binding(session, &binding);
        let source = self
            .repository
            .continuation_chain(&binding.user_id, task_id, &session_guard)
            .await?;
        // Historical replay requires exact source versions and node plus a valid current chain.
        let replay = source
            .segments
            .iter()
            .find(|(claim, _)| claim.source_task.revision == expected.task_revision);
        if let Some((claim, _)) = replay {
            expected.check(&claim.source_task)?;
            if claim.waiting_node != *waiting_node {
                return Err(StorageError::RuntimeWaitingNodeMismatch.into());
            }
        } else {
            expected.check(&source.view.task)?;
            if source.view.execution_status != RuntimeTaskExecutionStatus::WaitingUser {
                return Err(StorageError::RuntimeContinuationRequired.into());
            }
            if source.view.task.checkpoint
                != (RecoveryPoint::AtNode {
                    node_id: waiting_node.clone(),
                })
            {
                return Err(StorageError::RuntimeWaitingNodeMismatch.into());
            }
        }
        let review = self
            .runtime
            .review_read_only_execution(
                source.view.task.plan.clone(),
                session.clone(),
                source.view.task.facts_version,
                self.contracts.clone(),
            )
            .await?;
        if review.bindings_hash() != &source.bindings_hash {
            return Err(RuntimeTaskExecutionError::Review(
                "registrations changed".into(),
            ));
        }
        self.runtime.revalidate_read_only_wait(&review).await?;
        let guard = || {
            session_guard()?;
            self.runtime
                .check_read_only_review(&review)
                .map_err(|_| StorageError::Invalid("Runtime continuation review changed"))
        };
        let current = self
            .repository
            .continuation_chain(&binding.user_id, task_id, &guard)
            .await?;
        if current != source {
            return Err(StorageError::RevisionConflict.into());
        }
        if let Some((claim, receipt)) = replay {
            let receipt = receipt
                .as_ref()
                .ok_or(StorageError::RuntimeAlreadyClaimed)?;
            return Ok(receipt_view(
                receipt,
                &claim.id,
                source.view.task.revision,
                true,
            ));
        }
        let claim = self
            .repository
            .claim_continuation(&source, session, &binding, waiting_node, &guard)
            .await?;
        guard()?;
        match self
            .runtime
            .continue_reviewed_read_only(&review, &claim)
            .await
        {
            Ok(verified) => Ok(self
                .repository
                .settle_continuation(&claim, Some(&verified), &guard)
                .await?),
            Err(error) => {
                // Drift or DB uncertainty retains the permanent pending claim. A Runtime
                // failure is settled only while the original context is still current.
                self.repository
                    .settle_continuation(&claim, None, &guard)
                    .await?;
                Err(error.into())
            }
        }
    }

    /// Authenticated projection of already saved results, even after a tool upgrade
    /// or removal. Validates the stored chain, owner, session and expected versions;
    /// never rebuilds evidence, evaluates policies or grants continuation authority.
    pub async fn query_saved_history(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected: RuntimeTaskVersion,
    ) -> Result<RuntimeTaskContinuationView, RuntimeTaskExecutionError> {
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        let binding = self.runtime.session_binding_for_backend(session)?;
        let guard = || self.check_binding(session, &binding);
        let chain = self
            .repository
            .continuation_chain(&binding.user_id, task_id, &guard)
            .await?;
        expected.check(&chain.view.task)?;
        guard()?;
        Ok(chain.view)
    }

    /// Authenticated, zero-Tool recovery. Pending segments require original-ID review;
    /// a new process can explicitly continue only a settled waiting segment.
    pub async fn recover(
        &self,
        session: &SessionRef,
        task_id: &TaskId,
        expected: RuntimeTaskVersion,
    ) -> Result<RuntimeTaskContinuationView, RuntimeTaskExecutionError> {
        valid_id(session.as_str())?;
        valid_id(task_id.as_str())?;
        let binding = self.runtime.session_binding_for_backend(session)?;
        let session_guard = || self.check_binding(session, &binding);
        let chain = self
            .repository
            .continuation_chain(&binding.user_id, task_id, &session_guard)
            .await?;
        expected.check(&chain.view.task)?;
        let review = self
            .runtime
            .review_read_only_execution(
                chain.view.task.plan.clone(),
                session.clone(),
                chain.view.task.facts_version,
                self.contracts.clone(),
            )
            .await?;
        if review.bindings_hash() != &chain.bindings_hash {
            return Err(RuntimeTaskExecutionError::Review(
                "registrations changed".into(),
            ));
        }
        if chain.view.execution_status == RuntimeTaskExecutionStatus::WaitingUser {
            self.runtime.revalidate_read_only_wait(&review).await?;
        }
        let guard = || {
            session_guard()?;
            self.runtime
                .check_read_only_review(&review)
                .map_err(|_| StorageError::Invalid("Runtime continuation review changed"))
        };
        let current = self
            .repository
            .continuation_chain(&binding.user_id, task_id, &guard)
            .await?;
        if current != chain {
            return Err(StorageError::RevisionConflict.into());
        }
        guard()?;
        Ok(current.view)
    }
    fn check_binding(
        &self,
        session: &SessionRef,
        binding: &SessionBinding,
    ) -> Result<(), StorageError> {
        if self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| StorageError::Session)?
            != *binding
        {
            return Err(StorageError::Session);
        }
        Ok(())
    }
}

impl PgRepository {
    async fn continuation_chain(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        guard: Guard<'_>,
    ) -> Result<Chain, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let chain = self
            .read_continuation_chain(&mut tx, owner, task_id, guard)
            .await?;
        guarded(guard, tx.commit()).await?;
        Ok(chain)
    }
    async fn read_continuation_chain(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        owner: &UserId,
        task_id: &TaskId,
        guard: Guard<'_>,
    ) -> Result<Chain, StorageError> {
        let task = guarded(guard, lock_task(tx, owner, task_id)).await?;
        if !guarded(guard, operations(tx, task_id)).await?.is_empty() {
            return Err(StorageError::CorruptRecord);
        }
        validate_checkpoint(&task, &[])?;
        let root = guarded(
            guard,
            sqlx::query("SELECT * FROM runtime_read_claims WHERE task_id=$1 FOR UPDATE")
                .bind(task_id.as_str())
                .fetch_optional(&mut **tx),
        )
        .await?
        .ok_or(StorageError::RuntimeIntakeRequired)?;
        let initial = root.try_get::<Json<TaskRecord>, _>("initial_task")?.0;
        validate_checkpoint(&initial, &[])?;
        let root_id: String = root.try_get("claim_id")?;
        valid_id(&root_id)?;
        let root_binding = root.try_get::<Json<Binding>, _>("session_binding")?.0;
        let bindings_hash = ArgumentsHash::parse(root.try_get::<String, _>("bindings_hash")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        ArgumentsHash::parse(root.try_get::<String, _>("session_hash")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        if root.try_get::<i16, _>("contract_version")?
            != READ_ONLY_EXECUTION_CONTRACT_VERSION as i16
            || root_binding.user_id != *owner
            || root_binding.binding_version == 0
            || initial.task_id != *task_id
            || initial.owner_id != *owner
            || initial.state != TaskState::Running
            || !initial.completed_nodes.is_empty()
        {
            return Err(StorageError::CorruptRecord);
        }
        check_audit(
            tx,
            task_id,
            initial.revision,
            "runtime_read_claim",
            &json!({"claim_id":root_id,"contract_version":READ_ONLY_EXECUTION_CONTRACT_VERSION}),
            guard,
        )
        .await?;
        let rows = guarded(guard, sqlx::query("SELECT * FROM runtime_read_continuations WHERE task_id=$1 ORDER BY source_revision LIMIT 257 FOR UPDATE")
            .bind(task_id.as_str()).fetch_all(&mut **tx)).await?;
        if rows.len()
            > initial
                .plan
                .nodes
                .iter()
                .filter(|node| matches!(node, PlanNode::UserDecision { .. }))
                .count()
        {
            return Err(StorageError::CorruptRecord);
        }
        let mut segments = Vec::new();
        for row in rows {
            let claim = row.try_get::<Json<Claim>, _>("claim")?.0;
            let receipt = row
                .try_get::<Option<Json<SegmentReceipt>>, _>("receipt")?
                .map(|value| value.0);
            if row.try_get::<i64, _>("source_revision")? != db_version(claim.source_task.revision)?
                || row.try_get::<String, _>("claim_id")? != claim.id
                || row.try_get::<i16, _>("contract_version")?
                    != READ_ONLY_CONTINUATION_CONTRACT_VERSION as i16
                || row.try_get::<String, _>("status")?
                    != receipt.as_ref().map_or("CLAIMED", |r| {
                        if r.status == RuntimeTaskExecutionStatus::Failed {
                            "FAILED"
                        } else {
                            "SETTLED"
                        }
                    })
            {
                return Err(StorageError::CorruptRecord);
            }
            segments.push((claim, receipt));
        }
        let root_receipt = root
            .try_get::<Option<Json<Receipt>>, _>("receipt")?
            .map(|value| value.0);
        let Some(root_receipt) = root_receipt else {
            if !segments.is_empty()
                || root.try_get::<String, _>("status")? != "CLAIMED"
                || task != initial
            {
                return Err(StorageError::CorruptRecord);
            }
            return Ok(Chain {
                view: RuntimeTaskContinuationView {
                    contract_version: READ_ONLY_CONTINUATION_CONTRACT_VERSION,
                    current_task_revision: task.revision,
                    task,
                    execution_status: RuntimeTaskExecutionStatus::RequiresOriginalTaskReview,
                    result_references: vec![],
                    results: None,
                    segment_claim_id: None,
                    replayed: false,
                },
                tip_hash: hash(&initial)?,
                bindings_hash,
                segments,
            });
        };
        let first_task = segments
            .first()
            .map_or(&task, |(claim, _)| &claim.source_task);
        validate_settled_task(
            first_task,
            &root_receipt.result_references,
            root_receipt.status,
        )?;
        if root_receipt.contract_version != READ_ONLY_EXECUTION_CONTRACT_VERSION
            || root_receipt.claim_id != root_id
            || root_receipt.bindings_hash != bindings_hash
            || root_receipt.task_hash != hash(first_task)?
            || first_task.revision != increment(initial.revision)?
            || first_task.task_id != initial.task_id
            || first_task.owner_id != initial.owner_id
            || first_task.plan != initial.plan
            || first_task.facts_version != initial.facts_version
            || first_task.completed_nodes.iter().any(|id| {
                initial
                    .plan
                    .nodes
                    .iter()
                    .any(|node| node.id() == id && matches!(node, PlanNode::UserDecision { .. }))
            })
            || root.try_get::<String, _>("status")?
                != if root_receipt.status == RuntimeTaskExecutionStatus::Failed {
                    "FAILED"
                } else {
                    "SETTLED"
                }
            || root_receipt.results_hash.is_some()
                != (root_receipt.status != RuntimeTaskExecutionStatus::Failed)
        {
            return Err(StorageError::CorruptRecord);
        }
        check_audit(
            tx,
            task_id,
            first_task.revision,
            "runtime_read_execution",
            &to_json(&root_receipt)?,
            guard,
        )
        .await?;
        let mut prior_task = first_task.clone();
        let mut references = root_receipt.result_references.clone();
        check_result_bodies(
            root_receipt.results.as_deref(),
            &references,
            root_receipt.results_hash.as_ref(),
        )?;
        let mut results = root_receipt.results.clone();
        let mut tip_hash = hash(&root_receipt)?;
        let mut status = root_receipt.status;
        let mut segment_id = None;
        for (index, (claim, receipt)) in segments.iter().enumerate() {
            validate_claim(claim)?;
            if status != RuntimeTaskExecutionStatus::WaitingUser
                || claim.source_task != prior_task
                || claim.prior_receipt_hash != tip_hash
                || claim.bindings_hash != bindings_hash
            {
                return Err(StorageError::CorruptRecord);
            }
            check_audit(
                tx,
                task_id,
                claim.claimed_task.revision,
                "runtime_read_continuation_claim",
                &to_json(claim)?,
                guard,
            )
            .await?;
            segment_id = Some(claim.id.clone());
            if let Some(receipt) = receipt {
                validate_settled_task(&receipt.task, &receipt.result_references, receipt.status)?;
                let accepted = accepted_nodes(claim);
                if receipt.contract_version != READ_ONLY_CONTINUATION_CONTRACT_VERSION
                    || receipt.claim_id != claim.id
                    || receipt.prior_receipt_hash != tip_hash
                    || receipt.bindings_hash != bindings_hash
                    || receipt.task.revision != increment(claim.claimed_task.revision)?
                    || receipt.task.plan != claim.source_task.plan
                    || receipt.task.task_id != *task_id
                    || receipt.task.owner_id != *owner
                    || receipt.task.facts_version != claim.source_task.facts_version
                    || !accepted
                        .iter()
                        .all(|node| receipt.task.completed_nodes.contains(node))
                    || receipt.task.completed_nodes.iter().any(|id| {
                        !accepted.contains(id)
                            && claim.source_task.plan.nodes.iter().any(|node| {
                                node.id() == id && matches!(node, PlanNode::UserDecision { .. })
                            })
                    })
                    || !receipt.result_references.starts_with(&references)
                    || receipt.segment_results_hash.is_some()
                        != (receipt.status != RuntimeTaskExecutionStatus::Failed)
                    || (receipt.status == RuntimeTaskExecutionStatus::Failed
                        && (receipt.task.completed_nodes != accepted
                            || receipt.result_references != references))
                {
                    return Err(StorageError::CorruptRecord);
                }
                check_audit(
                    tx,
                    task_id,
                    receipt.task.revision,
                    "runtime_read_continuation_execution",
                    &to_json(receipt)?,
                    guard,
                )
                .await?;
                tip_hash = hash(receipt)?;
                prior_task = receipt.task.clone();
                if let Some(all) = &receipt.results {
                    let prefix = results.as_ref().ok_or(StorageError::CorruptRecord)?;
                    if !all.starts_with(prefix)
                        || prefix.len() != references.len()
                        || all.len() != receipt.result_references.len()
                    {
                        return Err(StorageError::CorruptRecord);
                    }
                    check_result_bodies(
                        Some(&all[prefix.len()..]),
                        &receipt.result_references[references.len()..],
                        receipt.segment_results_hash.as_ref(),
                    )?;
                }
                results = receipt.results.clone();
                references = receipt.result_references.clone();
                status = receipt.status;
            } else {
                if index + 1 != segments.len() {
                    return Err(StorageError::CorruptRecord);
                }
                prior_task = claim.claimed_task.clone();
                status = RuntimeTaskExecutionStatus::RequiresOriginalTaskReview;
            }
        }
        if prior_task != task {
            return Err(StorageError::CorruptRecord);
        }
        Ok(Chain {
            view: RuntimeTaskContinuationView {
                contract_version: READ_ONLY_CONTINUATION_CONTRACT_VERSION,
                current_task_revision: task.revision,
                task,
                execution_status: status,
                result_references: references,
                results,
                segment_claim_id: segment_id,
                replayed: false,
            },
            tip_hash,
            bindings_hash,
            segments,
        })
    }

    async fn claim_continuation(
        &self,
        source: &Chain,
        session: &SessionRef,
        binding: &SessionBinding,
        waiting_node: &NodeId,
        guard: Guard<'_>,
    ) -> Result<Claim, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let current = self
            .read_continuation_chain(&mut tx, &binding.user_id, &source.view.task.task_id, guard)
            .await?;
        if current != *source {
            return Err(StorageError::RevisionConflict);
        }
        let mut claimed = source.view.task.clone();
        claimed.state = claimed
            .state
            .transition(TaskEvent::UserResponded)
            .map_err(|_| StorageError::IllegalTransition)?;
        claimed.revision = increment(claimed.revision)?;
        let claim = Claim {
            contract_version: READ_ONLY_CONTINUATION_CONTRACT_VERSION,
            id: format!("continuation_{}", uuid::Uuid::new_v4()),
            source_task: source.view.task.clone(),
            claimed_task: claimed,
            waiting_node: waiting_node.clone(),
            prior_receipt_hash: source.tip_hash.clone(),
            session_hash: hash(session)?,
            binding: Binding {
                user_id: binding.user_id.clone(),
                binding_version: binding.binding_version,
            },
            bindings_hash: source.bindings_hash.clone(),
        };
        validate_claim(&claim)?;
        let inserted = guarded(guard, sqlx::query("INSERT INTO runtime_read_continuations(task_id,source_revision,claim_id,contract_version,claim,status) VALUES ($1,$2,$3,1,$4,'CLAIMED') ON CONFLICT(task_id,source_revision) DO NOTHING")
            .bind(claim.source_task.task_id.as_str()).bind(db_version(claim.source_task.revision)?).bind(&claim.id).bind(Json(&claim)).execute(&mut *tx)).await?;
        if inserted.rows_affected() != 1 {
            return Err(StorageError::RuntimeAlreadyClaimed);
        }
        guarded(
            guard,
            update_task(&mut tx, &claim.claimed_task, claim.source_task.revision),
        )
        .await?;
        guarded(
            guard,
            audit(
                &mut tx,
                &claim.claimed_task,
                None,
                PersistentAuditKind::CheckpointSaved,
                json!({"runtime_read_continuation_claim":claim}),
            ),
        )
        .await?;
        self.check_pending_continuation(&mut tx, &claim, guard)
            .await?;
        guarded(guard, tx.commit()).await?;
        // Deferred DB mutations are checked after COMMIT and before the executor receives
        // the private source. A lost commit response never reaches a Tool call.
        let mut tx = guarded(guard, self.pool.begin()).await?;
        self.check_pending_continuation(&mut tx, &claim, guard)
            .await?;
        guarded(guard, tx.commit()).await?;
        Ok(claim)
    }
    async fn check_pending_continuation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claim: &Claim,
        guard: Guard<'_>,
    ) -> Result<Chain, StorageError> {
        let chain = self
            .read_continuation_chain(
                tx,
                &claim.binding.user_id,
                &claim.source_task.task_id,
                guard,
            )
            .await?;
        if chain.view.task != claim.claimed_task
            || chain.segments.last() != Some(&(claim.clone(), None))
            || chain.view.execution_status != RuntimeTaskExecutionStatus::RequiresOriginalTaskReview
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(chain)
    }
    async fn settle_continuation(
        &self,
        claim: &Claim,
        verified: Option<&VerifiedReadOnlyExecution>,
        guard: Guard<'_>,
    ) -> Result<RuntimeTaskContinuationView, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let source = self
            .check_pending_continuation(&mut tx, claim, guard)
            .await?;
        let mut next = claim.claimed_task.clone();
        next.revision = increment(next.revision)?;
        let mut references = source.view.result_references;
        let mut results = source.view.results;
        let (status, segment_results_hash) =
            if let Some(verified) = verified {
                if verified.plan() != &next.plan
                    || verified.owner() != &next.owner_id
                    || verified.facts_version() != next.facts_version
                    || verified.bindings_hash() != &claim.bindings_hash
                    || hash(verified.session_ref())? != claim.session_hash
                {
                    return Err(StorageError::PlanConflict);
                }
                next.completed_nodes = verified.completed_nodes().to_vec();
                if !accepted_nodes(claim)
                    .iter()
                    .all(|node| next.completed_nodes.contains(node))
                {
                    return Err(StorageError::CorruptRecord);
                }
                let status = if let Some(waiting) = verified.waiting_node() {
                    next.state = next
                        .state
                        .transition(TaskEvent::AwaitUser)
                        .map_err(|_| StorageError::IllegalTransition)?;
                    next.checkpoint = RecoveryPoint::AtNode {
                        node_id: waiting.clone(),
                    };
                    RuntimeTaskExecutionStatus::WaitingUser
                } else {
                    next.state = next
                        .state
                        .transition(TaskEvent::Complete)
                        .map_err(|_| StorageError::IllegalTransition)?;
                    next.checkpoint = RecoveryPoint::Finished;
                    RuntimeTaskExecutionStatus::Completed
                };
                references.extend(verified.results().iter().map(|result| {
                    RuntimeReadResultReference {
                        tool_name: result.tool_name.clone(),
                        operation_id: result.operation_id.clone(),
                        result_ref: result.result_ref.clone(),
                    }
                }));
                if let Some(results) = &mut results {
                    results.extend(crate::billing_report::visible_results(verified.results()));
                }
                (status, Some(hash(&verified.results())?))
            } else {
                results = None;
                next.state = next
                    .state
                    .transition(TaskEvent::Fail)
                    .map_err(|_| StorageError::IllegalTransition)?;
                next.checkpoint = RecoveryPoint::Finished;
                next.completed_nodes = accepted_nodes(claim);
                (RuntimeTaskExecutionStatus::Failed, None)
            };
        validate_settled_task(&next, &references, status)?;
        let receipt = SegmentReceipt {
            contract_version: READ_ONLY_CONTINUATION_CONTRACT_VERSION,
            claim_id: claim.id.clone(),
            prior_receipt_hash: claim.prior_receipt_hash.clone(),
            task: next.clone(),
            status,
            bindings_hash: claim.bindings_hash.clone(),
            segment_results_hash,
            result_references: references,
            results,
        };
        guarded(
            guard,
            update_task(&mut tx, &next, claim.claimed_task.revision),
        )
        .await?;
        let updated = guarded(guard, sqlx::query("UPDATE runtime_read_continuations SET status=$1,receipt=$2,updated_at=clock_timestamp() WHERE task_id=$3 AND source_revision=$4 AND claim_id=$5 AND status='CLAIMED'")
            .bind(if status == RuntimeTaskExecutionStatus::Failed { "FAILED" } else { "SETTLED" }).bind(Json(&receipt))
            .bind(next.task_id.as_str()).bind(db_version(claim.source_task.revision)?).bind(&claim.id).execute(&mut *tx)).await?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::RuntimeAlreadyClaimed);
        }
        if let Some(verified) = verified.filter(|_| receipt.results.is_some()) {
            guarded(
                guard,
                crate::billing_report::archive_settled_reports(
                    &mut tx,
                    &next,
                    &claim.id,
                    "CONTINUATION",
                    &json!(receipt),
                    verified.results(),
                ),
            )
            .await?;
        }
        guarded(
            guard,
            audit(
                &mut tx,
                &next,
                None,
                PersistentAuditKind::CheckpointSaved,
                json!({"runtime_read_continuation_execution":receipt}),
            ),
        )
        .await?;
        let expected = self
            .read_continuation_chain(&mut tx, &next.owner_id, &next.task_id, guard)
            .await?;
        if expected.segments.last() != Some(&(claim.clone(), Some(receipt.clone()))) {
            return Err(StorageError::CorruptRecord);
        }
        guarded(guard, tx.commit()).await?;
        let current = self
            .continuation_chain(&next.owner_id, &next.task_id, guard)
            .await?;
        if current != expected {
            return Err(StorageError::RevisionConflict);
        }
        Ok(current.view)
    }
}

fn increment(revision: u64) -> Result<u64, StorageError> {
    let next = revision
        .checked_add(1)
        .ok_or(StorageError::RevisionConflict)?;
    db_version(next)?;
    Ok(next)
}
fn to_json(value: &impl Serialize) -> Result<Value, StorageError> {
    serde_json::to_value(value).map_err(|_| StorageError::CorruptRecord)
}
async fn check_audit(
    tx: &mut Transaction<'_, Postgres>,
    task_id: &TaskId,
    revision: u64,
    key: &str,
    expected: &Value,
    guard: Guard<'_>,
) -> Result<(), StorageError> {
    let row = guarded(guard, sqlx::query("SELECT details FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED' AND task_revision=$2 AND details ? $3 ORDER BY sequence DESC LIMIT 1")
        .bind(task_id.as_str()).bind(db_version(revision)?).bind(key).fetch_optional(&mut **tx)).await?.ok_or(StorageError::CorruptRecord)?;
    if row.try_get::<Json<Value>, _>("details")?.0[key] != *expected {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}
fn validate_claim(claim: &Claim) -> Result<(), StorageError> {
    valid_id(&claim.id)?;
    validate_checkpoint(&claim.source_task, &[])?;
    validate_checkpoint(&claim.claimed_task, &[])?;
    let mut expected = claim.source_task.clone();
    expected.state = expected
        .state
        .transition(TaskEvent::UserResponded)
        .map_err(|_| StorageError::CorruptRecord)?;
    expected.revision = increment(expected.revision)?;
    if claim.contract_version != READ_ONLY_CONTINUATION_CONTRACT_VERSION
        || claim.source_task.state != TaskState::WaitingUser
        || claim.source_task.checkpoint
            != (RecoveryPoint::AtNode {
                node_id: claim.waiting_node.clone(),
            })
        || claim.claimed_task != expected
        || claim.binding.user_id != expected.owner_id
        || claim.binding.binding_version == 0
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}
fn accepted_nodes(claim: &Claim) -> Vec<NodeId> {
    claim
        .source_task
        .plan
        .nodes
        .iter()
        .filter(|node| {
            claim.source_task.completed_nodes.contains(node.id())
                || node.id() == &claim.waiting_node
        })
        .map(|node| node.id().clone())
        .collect()
}
fn validate_settled_task(
    task: &TaskRecord,
    references: &[RuntimeReadResultReference],
    status: RuntimeTaskExecutionStatus,
) -> Result<(), StorageError> {
    validate_checkpoint(task, &[])?;
    let expected_state = match status {
        RuntimeTaskExecutionStatus::Completed => TaskState::Completed,
        RuntimeTaskExecutionStatus::WaitingUser => TaskState::WaitingUser,
        RuntimeTaskExecutionStatus::Failed => TaskState::Failed,
        RuntimeTaskExecutionStatus::RequiresOriginalTaskReview => {
            return Err(StorageError::CorruptRecord)
        }
    };
    if task.state != expected_state {
        return Err(StorageError::CorruptRecord);
    }
    let completed: HashSet<_> = task.completed_nodes.iter().collect();
    let mut scheduled = HashSet::new();
    let mut expected_tools = Vec::new();
    while scheduled.len() < completed.len() {
        let node = task
            .plan
            .nodes
            .iter()
            .find(|node| {
                !scheduled.contains(node.id())
                    && node
                        .depends_on()
                        .iter()
                        .all(|dependency| scheduled.contains(dependency))
            })
            .ok_or(StorageError::CorruptRecord)?;
        if !completed.contains(node.id()) {
            return Err(StorageError::CorruptRecord);
        }
        if let PlanNode::Tool { tool, .. } = node {
            expected_tools.push((node.id(), tool));
        }
        scheduled.insert(node.id().clone());
    }
    if status == RuntimeTaskExecutionStatus::WaitingUser {
        let next = task
            .plan
            .nodes
            .iter()
            .find(|node| {
                !scheduled.contains(node.id())
                    && node
                        .depends_on()
                        .iter()
                        .all(|dependency| scheduled.contains(dependency))
            })
            .ok_or(StorageError::CorruptRecord)?;
        if task.checkpoint
            != (RecoveryPoint::AtNode {
                node_id: next.id().clone(),
            })
            || !matches!(next, PlanNode::UserDecision { .. })
        {
            return Err(StorageError::CorruptRecord);
        }
    }
    if references.len() != expected_tools.len() {
        return Err(StorageError::CorruptRecord);
    }
    for (reference, (node, tool)) in references.iter().zip(expected_tools) {
        if reference.tool_name != *tool
            || reference.operation_id.as_str()
                != format!("operation_{}_{}", task.task_id.as_str(), node.as_str())
        {
            return Err(StorageError::CorruptRecord);
        }
        safeflow_contracts::ToolResultRef::parse(reference.result_ref.as_str())
            .map_err(|_| StorageError::CorruptRecord)?;
    }
    Ok(())
}
fn receipt_view(
    receipt: &SegmentReceipt,
    id: &str,
    current_task_revision: u64,
    replayed: bool,
) -> RuntimeTaskContinuationView {
    RuntimeTaskContinuationView {
        contract_version: READ_ONLY_CONTINUATION_CONTRACT_VERSION,
        task: receipt.task.clone(),
        execution_status: receipt.status,
        result_references: receipt.result_references.clone(),
        results: receipt.results.clone(),
        segment_claim_id: Some(id.into()),
        replayed,
        current_task_revision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use safeflow_contracts::{OperationId, ToolName, ToolResultRef};
    #[test]
    fn existing_reference_contract_accepts_shared_references_and_long_derived_ids() {
        for long in [false, true] {
            let task_id =
                TaskId::parse(if long { "t".repeat(200) } else { "task".into() }).unwrap();
            let ids = [
                NodeId::parse(if long { "a".repeat(200) } else { "a".into() }).unwrap(),
                NodeId::parse(if long { "b".repeat(200) } else { "b".into() }).unwrap(),
            ];
            let tool = ToolName::parse("get_account_balance").unwrap();
            let plan = AgentPlan::new(
                task_id.clone(),
                1,
                ids.iter()
                    .enumerate()
                    .map(|(i, id)| PlanNode::Tool {
                        id: id.clone(),
                        tool: tool.clone(),
                        arguments: json!({"account_id":"account_001"}),
                        depends_on: if i == 0 { vec![] } else { vec![ids[0].clone()] },
                    })
                    .collect(),
            )
            .unwrap();
            let task = TaskRecord {
                task_id: task_id.clone(),
                owner_id: UserId::parse("user").unwrap(),
                state: TaskState::Completed,
                revision: 2,
                facts_version: 7,
                plan,
                checkpoint: RecoveryPoint::Finished,
                completed_nodes: ids.to_vec(),
            };
            let shared = ToolResultRef::parse(if long {
                format!(
                    "read_result_operation_{}_{}",
                    task_id.as_str(),
                    ids[0].as_str()
                )
            } else {
                "shared_snapshot".into()
            })
            .unwrap();
            let references: Vec<_> = ids
                .iter()
                .map(|id| RuntimeReadResultReference {
                    tool_name: tool.clone(),
                    operation_id: OperationId::parse(format!(
                        "operation_{}_{}",
                        task_id.as_str(),
                        id.as_str()
                    ))
                    .unwrap(),
                    result_ref: shared.clone(),
                })
                .collect();
            validate_settled_task(&task, &references, RuntimeTaskExecutionStatus::Completed)
                .unwrap();
        }
    }
}
