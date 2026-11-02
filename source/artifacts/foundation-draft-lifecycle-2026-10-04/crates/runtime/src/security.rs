use crate::{DomainPolicyRegistry, ToolRegistry, ToolSecurityCatalog};
use safeflow_contracts::{
    CheckId, ContractFuture, DecisionId, FindingKind, PlanAcceptanceDecision,
    PlanAcceptanceRequest, PlanNode, PolicyVersion, PostExecuteAssessment, PostExecuteContext,
    PostExecuteOutcome, PostExecuteRequest, ReasonCode, SecurityContext, SecurityDecision,
    SecurityError, SecurityFinding, SecurityGate, SecurityKernel, SecurityLevel, SecurityOutcome,
    SecurityPhase, SessionRef, SourceName, UserId, SECURITY_CONTRACT_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const BINDING_POLICY_VERSION: &str = "binding-v1";
mod payment_final;
pub use payment_final::{BackendFinalPolicy, BackendFinalPolicyRegistration};
mod payment_minimum;
pub use payment_minimum::{PaymentMinimumInput, VerifiedPaymentMinimum, PAYMENT_MINIMUM_VERSION};

pub trait SessionResolver: Send + Sync {
    fn resolve(&self, session_ref: &SessionRef) -> Result<UserId, SecurityError>;

    /// Mutable resolvers must advance this version on revoke, renew or rebind,
    /// including a rebind to the same user. Default denies version-bound paths.
    fn resolve_binding(&self, _session_ref: &SessionRef) -> Result<SessionBinding, SecurityError> {
        Err(SecurityError {
            reason_code: ReasonCode::parse("SESSION_BINDING_UNAVAILABLE")
                .expect("static reason code"),
            message: "resolver does not provide versioned session binding snapshots".into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    pub user_id: UserId,
    pub binding_version: u64,
}

#[derive(Default)]
pub struct StaticSessionResolver {
    sessions: HashMap<SessionRef, UserId>,
}

impl StaticSessionResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, session_ref: SessionRef, user_id: UserId) {
        self.sessions.insert(session_ref, user_id);
    }
}

impl SessionResolver for StaticSessionResolver {
    fn resolve_binding(&self, session_ref: &SessionRef) -> Result<SessionBinding, SecurityError> {
        // Static mappings cannot change through a shared resolver reference.
        Ok(SessionBinding {
            user_id: self.resolve(session_ref)?,
            binding_version: 1,
        })
    }

    fn resolve(&self, session_ref: &SessionRef) -> Result<UserId, SecurityError> {
        self.sessions
            .get(session_ref)
            .cloned()
            .ok_or_else(|| SecurityError {
                reason_code: ReasonCode::parse("SESSION_NOT_FOUND").expect("static reason code"),
                message: "session reference did not resolve to an authenticated user".to_owned(),
            })
    }
}

pub struct DeterministicSecurityKernel {
    tools: Arc<ToolRegistry>,
    policies: Arc<DomainPolicyRegistry>,
    catalog: Arc<ToolSecurityCatalog>,
    sessions: Arc<dyn SessionResolver>,
    decision_sequence: AtomicU64,
}

impl DeterministicSecurityKernel {
    pub(crate) fn new(
        tools: Arc<ToolRegistry>,
        policies: Arc<DomainPolicyRegistry>,
        catalog: Arc<ToolSecurityCatalog>,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            tools,
            policies,
            catalog,
            sessions,
            decision_sequence: AtomicU64::new(0),
        }
    }

    fn next_decision_id(&self) -> DecisionId {
        let sequence = self.decision_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        DecisionId::parse(format!("decision_{sequence}"))
            .expect("generated decision IDs always have content")
    }

    fn plan_findings(&self, request: &PlanAcceptanceRequest) -> Vec<SecurityFinding> {
        let mut findings = Vec::new();
        let ids: HashSet<_> = request
            .plan
            .nodes
            .iter()
            .map(PlanNode::id)
            .cloned()
            .collect();

        if ids.len() != request.plan.nodes.len() {
            findings.push(plan_finding("COMMON-03", "DUPLICATE_NODE_ID"));
        }

        for node in &request.plan.nodes {
            for dependency in node.depends_on() {
                if dependency == node.id() || !ids.contains(dependency) {
                    findings.push(plan_finding("COMMON-03", "INVALID_NODE_DEPENDENCY"));
                }
            }

            if let PlanNode::Tool { tool, .. } = node {
                if self.tools.get(tool).is_none() {
                    findings.push(plan_finding("COMMON-01", "TOOL_NOT_REGISTERED"));
                }
                if self.policies.get(tool).is_none() {
                    findings.push(plan_finding("COMMON-01", "POLICY_NOT_REGISTERED"));
                }
            }
        }

        if contains_cycle(&request.plan.nodes) {
            findings.push(plan_finding("COMMON-03", "CYCLIC_PLAN"));
        }

        findings
    }
}

impl SecurityKernel for DeterministicSecurityKernel {
    fn validate_plan<'a>(
        &'a self,
        request: PlanAcceptanceRequest,
    ) -> ContractFuture<'a, Result<PlanAcceptanceDecision, SecurityError>> {
        Box::pin(async move {
            request.validate().map_err(contract_security_error)?;
            let findings = self.plan_findings(&request);
            Ok(PlanAcceptanceDecision {
                contract_version: SECURITY_CONTRACT_VERSION,
                decision_id: self.next_decision_id(),
                task_id: request.task_id,
                plan_version: request.plan_version,
                accepted: findings.is_empty(),
                policy_version: policy_version(),
                findings,
            })
        })
    }

    fn evaluate<'a>(
        &'a self,
        request: safeflow_contracts::OperationSecurityRequest,
    ) -> ContractFuture<'a, Result<SecurityDecision, SecurityError>> {
        Box::pin(async move {
            request.validate().map_err(contract_security_error)?;
            let operation_id = request.operation_id.clone();
            let facts_version = request.facts_version;
            let phase = request.phase;
            let tool_name = request.tool_name.clone();
            let session_ref = request.session_ref.clone();
            let authenticated_user_id = self.sessions.resolve(&request.session_ref)?;
            if phase == SecurityPhase::PreCommit {
                return Err(pre_commit_unavailable());
            }
            let context = SecurityContext::from_request(request, authenticated_user_id)
                .map_err(contract_security_error)?;
            let policy = self.policies.get(&tool_name).ok_or_else(|| SecurityError {
                reason_code: ReasonCode::parse("POLICY_NOT_REGISTERED")
                    .expect("static reason code"),
                message: format!("no domain policy registered for {tool_name}"),
            })?;
            let definition = self
                .tools
                .get(&tool_name)
                .ok_or_else(|| SecurityError {
                    reason_code: ReasonCode::parse("TOOL_NOT_REGISTERED")
                        .expect("static reason code"),
                    message: format!("no tool registered for {tool_name}"),
                })?
                .definition();
            let profile = self
                .catalog
                .get(&tool_name)
                .cloned()
                .ok_or_else(|| SecurityError {
                    reason_code: ReasonCode::parse("SECURITY_PROFILE_NOT_REGISTERED")
                        .expect("static reason code"),
                    message: format!("no security profile registered for {tool_name}"),
                })?;
            let findings =
                crate::read_session::scope(&self.sessions, &session_ref, policy.evaluate(&context))
                    .await??;
            let mut outcome =
                decide_outcome(&findings, definition.has_side_effect, profile.minimum_level);
            let required_gates = required_gates(profile.minimum_level, definition.has_side_effect);
            let (evaluated, risk_score) =
                match self.catalog.risk.evaluate(&context, findings.clone()) {
                    Ok(result) => result,
                    Err(error) => {
                        outcome = SecurityOutcome::Blocked;
                        // Untrusted/malformed risk payloads must not enter persistent audit data.
                        // Keep the safe rejection itself in the same Decision chain.
                        let mut safe: Vec<_> = findings
                            .into_iter()
                            .filter(|f| {
                                f.kind == FindingKind::HardDeny || f.kind == FindingKind::NeedsInput
                            })
                            .take(15)
                            .map(|finding| safeflow_contracts::EvaluatedFinding {
                                finding,
                                applied_weight: None,
                                score_delta: 0,
                            })
                            .collect();
                        let mut rejection = plan_finding("COMMON-17", error.reason_code.as_str());
                        rejection.policy_version =
                            PolicyVersion::parse(crate::CENTRAL_SCORING_VERSION)
                                .expect("static version");
                        safe.push(safeflow_contracts::EvaluatedFinding {
                            finding: rejection,
                            applied_weight: None,
                            score_delta: 0,
                        });
                        (safe, 0)
                    }
                };
            let decision = SecurityDecision {
                contract_version: SECURITY_CONTRACT_VERSION,
                decision_id: self.next_decision_id(),
                operation_id,
                facts_version,
                policy_version: profile.policy_version,
                outcome,
                minimum_level: profile.minimum_level,
                risk_score,
                risk_tier: crate::risk_tier_for_score(risk_score),
                required_gates,
                findings: evaluated,
                expires_at_ms: i64::MAX,
            };
            decision.validate().map_err(contract_security_error)?;
            Ok(decision)
        })
    }

    fn verify_execution<'a>(
        &'a self,
        request: PostExecuteRequest,
    ) -> ContractFuture<'a, Result<PostExecuteAssessment, SecurityError>> {
        Box::pin(async move {
            let authenticated_user_id = self.sessions.resolve(&request.session_ref)?;
            let policy = self
                .policies
                .get(&request.tool_name)
                .ok_or_else(|| SecurityError {
                    reason_code: ReasonCode::parse("POLICY_NOT_REGISTERED")
                        .expect("static reason code"),
                    message: format!("no domain policy registered for {}", request.tool_name),
                })?;
            let observation = request.observation.clone();
            let context = PostExecuteContext::from_request(request, authenticated_user_id);
            let findings = policy.verify_execution(&context).await?;
            let has_denial = findings
                .iter()
                .any(|finding| finding.kind == FindingKind::HardDeny);

            let (outcome, reason) = match observation {
                safeflow_contracts::ExecutionObservation::NotSubmitted { reason_code, .. } => {
                    (PostExecuteOutcome::VerifiedFailed, reason_code)
                }
                safeflow_contracts::ExecutionObservation::ResultReceived { .. } if has_denial => (
                    PostExecuteOutcome::VerifiedFailed,
                    ReasonCode::parse("RESULT_VERIFICATION_FAILED").expect("static reason code"),
                ),
                safeflow_contracts::ExecutionObservation::ResultReceived { .. } => (
                    PostExecuteOutcome::VerifiedSucceeded,
                    ReasonCode::parse("RESULT_VERIFIED").expect("static reason code"),
                ),
                safeflow_contracts::ExecutionObservation::OutcomeUnknown {
                    reason_code, ..
                } => (PostExecuteOutcome::Processing, reason_code),
            };
            Ok(PostExecuteAssessment::new(outcome, reason))
        })
    }
}

fn plan_finding(check_id: &str, reason_code: &str) -> SecurityFinding {
    SecurityFinding {
        check_id: CheckId::parse(check_id).expect("static check ID"),
        policy_version: policy_version(),
        source: SourceName::parse("security_kernel").expect("static source"),
        kind: FindingKind::HardDeny,
        reason_code: ReasonCode::parse(reason_code).expect("static reason code"),
        evidence_refs: vec![],
        observed_value: None,
        event_ids: vec![],
        dedup_key: None,
        window_start_ms: None,
        window_end_ms: None,
    }
}

fn policy_version() -> PolicyVersion {
    PolicyVersion::parse(BINDING_POLICY_VERSION).expect("static policy version")
}

pub(crate) fn contract_security_error(error: impl ToString) -> SecurityError {
    SecurityError {
        reason_code: ReasonCode::parse("INVALID_SECURITY_CONTRACT").expect("static reason code"),
        message: error.to_string(),
    }
}

fn decide_outcome(
    findings: &[SecurityFinding],
    has_side_effect: bool,
    minimum_level: SecurityLevel,
) -> SecurityOutcome {
    if findings
        .iter()
        .any(|finding| finding.kind == FindingKind::HardDeny)
    {
        return SecurityOutcome::Blocked;
    }
    if findings
        .iter()
        .any(|finding| finding.kind == FindingKind::NeedsInput)
    {
        return SecurityOutcome::NeedsInput;
    }
    if minimum_level == SecurityLevel::Critical {
        return SecurityOutcome::Blocked;
    }
    if minimum_level >= SecurityLevel::Red {
        SecurityOutcome::WaitingAuth
    } else if findings
        .iter()
        .any(|finding| finding.kind == FindingKind::RequireGate)
    {
        SecurityOutcome::ReadyToConfirm
    } else if minimum_level == SecurityLevel::Green && !has_side_effect {
        SecurityOutcome::Allow
    } else {
        SecurityOutcome::ReadyToConfirm
    }
}

pub(crate) fn pre_commit_unavailable() -> SecurityError {
    SecurityError {
        reason_code: ReasonCode::parse("TRUSTED_PRE_COMMIT_VERIFICATION_UNAVAILABLE")
            .expect("static reason code"),
        message: "trusted current decision, confirmation and conditional authentication verification are not assembled".into(),
    }
}

#[cfg(test)]
#[path = "pre_commit_tests.rs"]
mod pre_commit_tests;

fn required_gates(minimum_level: SecurityLevel, has_side_effect: bool) -> Vec<SecurityGate> {
    match minimum_level {
        SecurityLevel::Green if has_side_effect => vec![SecurityGate::ExplicitConfirmation],
        SecurityLevel::Green => vec![],
        SecurityLevel::Yellow => vec![SecurityGate::ExplicitConfirmation],
        SecurityLevel::Red | SecurityLevel::Critical => vec![
            SecurityGate::ExplicitConfirmation,
            SecurityGate::StrongAuthentication,
        ],
    }
}

fn contains_cycle(nodes: &[PlanNode]) -> bool {
    fn visit(
        id: &safeflow_contracts::NodeId,
        dependencies: &HashMap<safeflow_contracts::NodeId, Vec<safeflow_contracts::NodeId>>,
        visiting: &mut HashSet<safeflow_contracts::NodeId>,
        visited: &mut HashSet<safeflow_contracts::NodeId>,
    ) -> bool {
        if visited.contains(id) {
            return false;
        }
        if !visiting.insert(id.clone()) {
            return true;
        }
        if let Some(node_dependencies) = dependencies.get(id) {
            for dependency in node_dependencies {
                if dependencies.contains_key(dependency)
                    && visit(dependency, dependencies, visiting, visited)
                {
                    return true;
                }
            }
        }
        visiting.remove(id);
        visited.insert(id.clone());
        false
    }

    let dependencies: HashMap<_, _> = nodes
        .iter()
        .map(|node| (node.id().clone(), node.depends_on().to_vec()))
        .collect();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    dependencies
        .keys()
        .any(|id| visit(id, &dependencies, &mut visiting, &mut visited))
}
