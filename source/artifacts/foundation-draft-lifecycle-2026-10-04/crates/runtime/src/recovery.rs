use crate::runtime::{hash_arguments, security_request};
use crate::{validate_tool_policy_bindings, Runtime, RuntimeError, SessionBinding};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, NodeId, PlanAcceptanceRequest, PlanNode, PlanningResult,
    SecurityOutcome, SecurityPhase, SessionRef, ToolResult, Understanding, UnderstandingDecision,
    UserId, AGENT_TOOL_CONTRACT_VERSION,
};
use serde_json::json;

pub const READ_ONLY_WAIT_CONTRACT_VERSION: u16 = 1;
pub const READ_ONLY_EXECUTION_CONTRACT_VERSION: u16 = 1;

/// Sealed progress from the existing executor, after security and audit succeed.
/// Does not deserialize and is never an external execution authorization.
/// ```compile_fail
/// let forged: safeflow_runtime::VerifiedReadOnlyExecution =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug)]
pub struct VerifiedReadOnlyExecution {
    pub(crate) plan: AgentPlan,
    pub(crate) session_ref: SessionRef,
    pub(crate) owner: UserId,
    pub(crate) facts_version: u64,
    pub(crate) waiting_node: Option<NodeId>,
    pub(crate) completed_nodes: Vec<NodeId>,
    pub(crate) bindings_hash: ArgumentsHash,
    pub(crate) results: Vec<ToolResult>,
}
impl VerifiedReadOnlyExecution {
    pub fn plan(&self) -> &AgentPlan {
        &self.plan
    }
    pub fn session_ref(&self) -> &SessionRef {
        &self.session_ref
    }
    pub fn owner(&self) -> &UserId {
        &self.owner
    }
    pub fn facts_version(&self) -> u64 {
        self.facts_version
    }
    pub fn waiting_node(&self) -> Option<&NodeId> {
        self.waiting_node.as_ref()
    }
    pub fn completed_nodes(&self) -> &[NodeId] {
        &self.completed_nodes
    }
    pub fn bindings_hash(&self) -> &ArgumentsHash {
        &self.bindings_hash
    }
    pub fn results(&self) -> &[ToolResult] {
        &self.results
    }
}

/// A review of current trusted identity and registrations, never execution authority.
#[derive(Debug)]
pub struct ReadOnlyPlanReview {
    pub(crate) plan: AgentPlan,
    pub(crate) session_ref: SessionRef,
    pub(crate) owner: UserId,
    pub(crate) session_binding: SessionBinding,
    pub(crate) facts_version: u64,
    pub(crate) bindings_hash: ArgumentsHash,
}

impl ReadOnlyPlanReview {
    pub fn plan(&self) -> &AgentPlan {
        &self.plan
    }
    pub fn owner(&self) -> &UserId {
        &self.owner
    }
    pub fn facts_version(&self) -> u64 {
        self.facts_version
    }
    pub fn bindings_hash(&self) -> &ArgumentsHash {
        &self.bindings_hash
    }
}

/// Constructed only by Runtime after actual successful ToolResult verification.
/// This process-local value is not a Permit, wire input or resumable execution token.
///
/// ```compile_fail
/// let forged: safeflow_runtime::VerifiedReadOnlyWait =
///     serde_json::from_value(serde_json::json!({"completed_nodes":["read"]})).unwrap();
/// ```
#[derive(Debug)]
pub struct VerifiedReadOnlyWait {
    pub(crate) plan: AgentPlan,
    pub(crate) session_ref: SessionRef,
    pub(crate) owner: UserId,
    pub(crate) facts_version: u64,
    pub(crate) node_id: NodeId,
    pub(crate) completed_nodes: Vec<NodeId>,
    pub(crate) bindings_hash: ArgumentsHash,
    pub(crate) results_hash: ArgumentsHash,
}

impl VerifiedReadOnlyWait {
    pub fn plan(&self) -> &AgentPlan {
        &self.plan
    }
    pub fn session_ref(&self) -> &SessionRef {
        &self.session_ref
    }
    pub fn owner(&self) -> &UserId {
        &self.owner
    }
    pub fn facts_version(&self) -> u64 {
        self.facts_version
    }
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }
    pub fn completed_nodes(&self) -> &[NodeId] {
        &self.completed_nodes
    }
    pub fn bindings_hash(&self) -> &ArgumentsHash {
        &self.bindings_hash
    }
    pub fn results_hash(&self) -> &ArgumentsHash {
        &self.results_hash
    }
}

impl Runtime {
    /// Trusted backend identity lookup. Storage uses this exact Runtime resolver.
    pub fn authenticated_user(&self, session: &SessionRef) -> Result<UserId, RuntimeError> {
        Ok(self.session_binding_for_backend(session)?.user_id)
    }

    pub fn session_binding_for_backend(
        &self,
        session: &SessionRef,
    ) -> Result<SessionBinding, RuntimeError> {
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|error| RuntimeError::Security(error.to_string()))?;
        if binding.binding_version == 0 {
            return Err(RuntimeError::SessionChanged);
        }
        Ok(binding)
    }

    pub async fn review_read_only_plan(
        &self,
        plan: AgentPlan,
        session_ref: SessionRef,
        facts_version: u64,
    ) -> Result<ReadOnlyPlanReview, RuntimeError> {
        if !plan
            .nodes
            .iter()
            .any(|node| matches!(node, PlanNode::UserDecision { .. }))
        {
            return Err(RuntimeError::RecoveryReview(
                "expected read-only user decision plan".into(),
            ));
        }
        self.review_read_only_execution_plan(plan, session_ref, facts_version)
            .await
    }

    /// Scheduling review; input schemas are checked again against approved contracts.
    pub async fn review_read_only_execution(
        &self,
        plan: AgentPlan,
        session_ref: SessionRef,
        facts_version: u64,
        contracts: Vec<crate::PreflightInputContract>,
    ) -> Result<ReadOnlyPlanReview, RuntimeError> {
        let intake = crate::PlanningIntakeService::for_trusted_backend(self, None, contracts)
            .map_err(|error| RuntimeError::RecoveryReview(error.to_string()))?;
        let proposal = PlanningResult::for_plan(
            Understanding {
                intent: "READ_ONLY_EXECUTION".into(),
                entities: json!({}),
                missing_fields: vec![],
                context_refs: vec![],
                decision: UnderstandingDecision::Pass,
            },
            plan.clone(),
        );
        let raw = serde_json::to_vec(&proposal)
            .map_err(|_| RuntimeError::InvalidPlan("serialization".into()))?;
        let checked = intake
            .accept_json_for_persistence(&session_ref, &plan.task_id, plan.plan_version, &raw)
            .await
            .map_err(|error| RuntimeError::RecoveryReview(error.to_string()))?;
        let review = self
            .review_read_only_execution_plan(plan, session_ref, facts_version)
            .await?;
        intake
            .check_persistence_review(&checked)
            .map_err(|error| RuntimeError::RecoveryReview(error.to_string()))?;
        Ok(review)
    }

    async fn review_read_only_execution_plan(
        &self,
        plan: AgentPlan,
        session_ref: SessionRef,
        facts_version: u64,
    ) -> Result<ReadOnlyPlanReview, RuntimeError> {
        if facts_version == 0
            || facts_version > u32::MAX as u64
            || plan.nodes.is_empty()
            || plan.nodes.len() > 256
        {
            return Err(RuntimeError::RecoveryReview(
                "expected bounded read-only plan and positive facts version".into(),
            ));
        }
        let session_binding = self.session_binding_for_backend(&session_ref)?;
        let owner = session_binding.user_id.clone();
        let decision = self
            .security
            .validate_plan(
                PlanAcceptanceRequest::new(plan.clone())
                    .map_err(|error| RuntimeError::InvalidPlan(error.to_string()))?,
            )
            .await
            .map_err(|error| RuntimeError::Security(error.to_string()))?;
        if !decision.accepted {
            return Err(RuntimeError::PlanRejected);
        }
        let review = ReadOnlyPlanReview {
            bindings_hash: self.read_only_bindings_hash(&plan)?,
            plan,
            session_ref,
            owner,
            session_binding,
            facts_version,
        };
        self.check_read_only_review(&review)?;
        Ok(review)
    }

    pub fn check_read_only_review(&self, review: &ReadOnlyPlanReview) -> Result<(), RuntimeError> {
        if self.session_binding_for_backend(&review.session_ref)? != review.session_binding {
            return Err(RuntimeError::SessionChanged);
        }
        if self.read_only_bindings_hash(&review.plan)? != review.bindings_hash {
            return Err(RuntimeError::RecoveryReview(
                "tool/policy/profile registrations changed".into(),
            ));
        }
        Ok(())
    }

    pub async fn execute_reviewed_read_only_wait(
        &self,
        review: &ReadOnlyPlanReview,
    ) -> Result<VerifiedReadOnlyWait, RuntimeError> {
        if !review
            .plan
            .nodes
            .iter()
            .any(|node| matches!(node, PlanNode::UserDecision { .. }))
        {
            return Err(RuntimeError::RecoveryReview(
                "expected read-only user decision plan".into(),
            ));
        }
        let verified = self.execute_reviewed_read_only(review).await?;
        Ok(VerifiedReadOnlyWait {
            results_hash: hash_arguments(
                &serde_json::to_value(&verified.results).expect("results serialize"),
            ),
            plan: verified.plan,
            session_ref: verified.session_ref,
            owner: verified.owner,
            facts_version: verified.facts_version,
            node_id: verified.waiting_node.ok_or_else(|| {
                RuntimeError::RecoveryReview("execution did not reach a user decision".into())
            })?,
            completed_nodes: verified.completed_nodes,
            bindings_hash: verified.bindings_hash,
        })
    }

    pub async fn execute_reviewed_read_only(
        &self,
        review: &ReadOnlyPlanReview,
    ) -> Result<VerifiedReadOnlyExecution, RuntimeError> {
        self.execute_reviewed_read_only_seeded(review, None).await
    }

    pub(crate) async fn execute_reviewed_read_only_seeded(
        &self,
        review: &ReadOnlyPlanReview,
        completed_seed: Option<&[NodeId]>,
    ) -> Result<VerifiedReadOnlyExecution, RuntimeError> {
        self.check_read_only_review(review)?;
        let planning = PlanningResult::for_plan(
            Understanding {
                intent: "READ_ONLY_WAIT".into(),
                entities: json!({}),
                missing_fields: vec![],
                context_refs: vec![],
                decision: UnderstandingDecision::Pass,
            },
            review.plan.clone(),
        );
        let mut captured = None;
        self.execute_inner(
            planning,
            review.session_ref.clone(),
            None,
            Some((review, &mut captured)),
            completed_seed,
        )
        .await?;
        captured.ok_or_else(|| {
            RuntimeError::RecoveryReview("execution produced no verified progress".into())
        })
    }

    /// Re-evaluates current policies without executing any AgentTool or post-result callback.
    pub async fn revalidate_read_only_wait(
        &self,
        review: &ReadOnlyPlanReview,
    ) -> Result<(), RuntimeError> {
        for node in &review.plan.nodes {
            if !matches!(node, PlanNode::Tool { .. }) {
                continue;
            }
            for phase in [SecurityPhase::PreTool, SecurityPhase::PreAuth] {
                self.check_read_only_review(review)?;
                let decision = self
                    .security
                    .evaluate(security_request(
                        &review.plan,
                        node,
                        &review.session_ref,
                        review.facts_version,
                        phase,
                    )?)
                    .await
                    .map_err(|error| RuntimeError::Security(error.to_string()))?;
                self.check_read_only_review(review)?;
                if u64::from(decision.facts_version) != review.facts_version
                    || decision.outcome != SecurityOutcome::Allow
                {
                    return Err(RuntimeError::SecurityGate(decision.outcome));
                }
            }
        }
        self.check_read_only_review(review)
    }

    fn read_only_bindings_hash(&self, plan: &AgentPlan) -> Result<ArgumentsHash, RuntimeError> {
        validate_tool_policy_bindings(&self.tools, &self.policies, &self.catalog)
            .map_err(|error| RuntimeError::RecoveryReview(error.to_string()))?;
        let mut bindings = Vec::new();
        for node in &plan.nodes {
            let PlanNode::Tool { tool, .. } = node else {
                continue;
            };
            let definition = self
                .tools
                .get(tool)
                .ok_or_else(|| RuntimeError::ToolNotFound(tool.to_string()))?
                .definition();
            if definition.name != *tool
                || definition.contract_version != AGENT_TOOL_CONTRACT_VERSION
            {
                return Err(RuntimeError::RecoveryReview(
                    "tool identity/contract changed".into(),
                ));
            }
            if definition.has_side_effect {
                return Err(RuntimeError::WriteToolRequiresLaterStage(tool.to_string()));
            }
            bindings.push((
                definition,
                self.policies
                    .definition(tool)
                    .ok_or_else(|| RuntimeError::RecoveryReview("missing policy".into()))?,
                self.catalog
                    .get(tool)
                    .ok_or_else(|| RuntimeError::RecoveryReview("missing profile".into()))?,
            ));
        }
        let bindings = serde_json::to_value(bindings).expect("bindings serialize");
        // Startup approvals must survive every recovery/continuation entry, including relogin.
        // Keep the original hash format for runtimes without backend registration metadata.
        Ok(hash_arguments(&match &self.backend_registration {
            Some(_) => {
                json!({"bindings": bindings, "backend_source": self.preflight_source_metadata_for_backend()?})
            }
            None => bindings,
        }))
    }
}
