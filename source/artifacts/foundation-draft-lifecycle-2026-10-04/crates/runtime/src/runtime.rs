use crate::mock_draft::Admission;
use crate::recovery::{ReadOnlyPlanReview, VerifiedReadOnlyExecution};
use crate::{AuditEvent, AuditEventKind, AuditRecorder, SessionResolver, ToolRegistry};
use crate::{DomainPolicyRegistry, ToolSecurityCatalog};
use crate::{MockDraftAuthority, MockDraftWriteRequest};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, AttemptId, BusinessOperationRef, EventId, ExecutionObservation,
    FindingKind, MockDraftAction, MockDraftPermit, NodeId, OperationId, OperationSecurityRequest,
    PlanAcceptanceRequest, PlanNode, PlanningResult, PostExecuteOutcome, PostExecuteRequest,
    SecurityGate, SecurityKernel, SecurityLevel, SecurityOutcome, SecurityPhase, SessionRef,
    TaskId, TaskState, ToolExecutionAuthorization, ToolResult, ToolResultStatus,
    MOCK_DRAFT_TOOL_SCHEMA_VERSION, SECURITY_CONTRACT_VERSION,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct Runtime {
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) security: Arc<dyn SecurityKernel>,
    audit: Arc<dyn AuditRecorder>,
    pub(crate) sessions: Arc<dyn SessionResolver>,
    pub(crate) policies: Arc<DomainPolicyRegistry>,
    pub(crate) catalog: Arc<ToolSecurityCatalog>,
    pub(crate) final_kernel: Option<Arc<crate::security::DeterministicSecurityKernel>>,
    pub(crate) final_payment: Option<crate::BackendFinalPolicyRegistration>,
    audit_sequence: AtomicU64,
    pub(crate) mock_drafts: Option<Arc<MockDraftAuthority>>,
    pub(crate) backend_registration: Option<serde_json::Value>,
}

impl Runtime {
    pub(crate) fn new(
        tools: Arc<ToolRegistry>,
        security: Arc<dyn SecurityKernel>,
        audit: Arc<dyn AuditRecorder>,
        sessions: Arc<dyn SessionResolver>,
        policies: Arc<DomainPolicyRegistry>,
        catalog: Arc<ToolSecurityCatalog>,
    ) -> Self {
        Self {
            tools,
            security,
            audit,
            sessions,
            policies,
            catalog,
            final_kernel: None,
            final_payment: None,
            audit_sequence: AtomicU64::new(0),
            mock_drafts: None,
            backend_registration: None,
        }
    }

    pub async fn execute(
        &self,
        planning_result: PlanningResult,
        session_ref: SessionRef,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        self.execute_inner(planning_result, session_ref, None, None, None)
            .await
    }

    /// Opt-in, explicitly confirmed, single-node mock draft storage only.
    /// Ordinary execute() keeps rejecting every side-effecting Tool.
    pub async fn execute_mock_draft(
        &self,
        request: MockDraftWriteRequest,
        permit: &MockDraftPermit,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        let admission = match &self.mock_drafts {
            Some(authority) => authority.begin(&request, permit),
            None => Err(RuntimeError::MockDraftAuthorization(
                "MOCK_DRAFT_PATH_NOT_CONFIGURED".into(),
            )),
        };
        let admission = match admission {
            Ok(admission) => admission,
            Err(error) => {
                self.record_write(
                    AuditEventKind::WriteRejected,
                    &request,
                    json!({"reason": error.to_string()}),
                )?;
                return Err(error);
            }
        };
        match admission {
            Admission::Replay(result) => {
                self.record_write(
                    AuditEventKind::WriteReplayed,
                    &request,
                    json!({"cached": true}),
                )?;
                result
            }
            Admission::Wait => {
                let result = self
                    .mock_drafts
                    .as_ref()
                    .expect("configured authority")
                    .wait(&request, permit)
                    .await;
                self.record_write(
                    AuditEventKind::WriteReplayed,
                    &request,
                    json!({"joined": true}),
                )?;
                result
            }
            Admission::Execute(mut reservation) => {
                reservation.audit = Some(self.audit.clone());
                let result = match request.planning_result() {
                    Ok(plan) => {
                        self.execute_inner(
                            plan,
                            request.session_ref.clone(),
                            Some((&request, permit)),
                            None,
                            None,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                let result = if let Err(error) = &result {
                    match self.record_write(
                        AuditEventKind::WriteFailed,
                        &request,
                        json!({"reason": error.to_string()}),
                    ) {
                        Ok(()) => result,
                        Err(error) => Err(error),
                    }
                } else {
                    result
                };
                reservation.finish(result.clone());
                result
            }
        }
    }

    fn record_write(
        &self,
        kind: AuditEventKind,
        request: &MockDraftWriteRequest,
        details: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        self.record(
            kind,
            request.task_id.clone(),
            Some(request.node_id.clone()),
            Some(request.operation_id()),
            Some(request.tool_name.clone()),
            details,
        )
    }

    pub(crate) async fn execute_inner(
        &self,
        planning_result: PlanningResult,
        session_ref: SessionRef,
        mock_write: Option<(&MockDraftWriteRequest, &MockDraftPermit)>,
        mut read_only: Option<(&ReadOnlyPlanReview, &mut Option<VerifiedReadOnlyExecution>)>,
        completed_seed: Option<&[NodeId]>,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        planning_result
            .validate()
            .map_err(|error| RuntimeError::InvalidPlan(error.to_string()))?;
        let plan = match planning_result.plan {
            Some(plan) => plan,
            None => {
                let question = planning_result
                    .clarification
                    .map(|clarification| clarification.question)
                    .ok_or_else(|| RuntimeError::InvalidPlan("missing plan".to_owned()))?;
                return Ok(RuntimeOutcome::NeedsClarification { question });
            }
        };
        let task_id = plan.task_id.clone();
        let plan_version = plan.plan_version;
        let review = read_only.as_ref().map(|(review, _)| *review);
        let facts_version = review.map_or_else(
            || {
                mock_write
                    .and_then(|(request, _)| request.persistent.as_ref())
                    .map_or(1, |binding| u64::from(binding.facts_version))
            },
            |review| review.facts_version,
        );
        if let Some(review) = review {
            self.check_read_only_review(review)?;
            if plan != review.plan {
                return Err(RuntimeError::RecoveryReview(
                    "execution plan changed".into(),
                ));
            }
        }
        let acceptance = self
            .security
            .validate_plan(
                PlanAcceptanceRequest::new(plan.clone())
                    .map_err(|error| RuntimeError::InvalidPlan(error.to_string()))?,
            )
            .await
            .map_err(|error| RuntimeError::Security(error.to_string()))?;

        if !acceptance.accepted {
            self.record(
                AuditEventKind::PlanRejected,
                task_id.clone(),
                None,
                None,
                None,
                json!({"decision_id": acceptance.decision_id.as_str()}),
            )?;
            return Err(RuntimeError::PlanRejected);
        }
        self.record(
            AuditEventKind::PlanAccepted,
            task_id.clone(),
            None,
            None,
            None,
            json!({"decision_id": acceptance.decision_id.as_str()}),
        )?;
        let authenticated_user_id = self
            .sessions
            .resolve(&session_ref)
            .map_err(|error| RuntimeError::Security(error.to_string()))?;
        if let Some(review) = review {
            if authenticated_user_id != review.owner {
                return Err(RuntimeError::SessionChanged);
            }
            self.check_read_only_review(review)?;
        }

        if completed_seed.is_some() && (read_only.is_none() || mock_write.is_some()) {
            return Err(RuntimeError::RecoveryReview(
                "continuation requires a read-only review".into(),
            ));
        }
        let mut completed: HashSet<NodeId> =
            completed_seed.unwrap_or(&[]).iter().cloned().collect();
        let mut results = Vec::new();
        while completed.len() < plan.nodes.len() {
            let Some(node) = plan.nodes.iter().find(|node| {
                !completed.contains(node.id())
                    && node
                        .depends_on()
                        .iter()
                        .all(|dependency| completed.contains(dependency))
            }) else {
                return Err(RuntimeError::InvalidPlan(
                    "plan cannot make progress because dependencies are unresolved".to_owned(),
                ));
            };

            match node {
                PlanNode::UserDecision { id, .. } => {
                    if let Some(review) = review {
                        self.check_read_only_review(review)?;
                    }
                    self.record(
                        AuditEventKind::TaskPaused,
                        task_id.clone(),
                        Some(id.clone()),
                        None,
                        None,
                        json!({"state": "WAITING_USER"}),
                    )?;
                    if let Some((review, capture)) = read_only.as_mut() {
                        self.check_read_only_review(review)?;
                        **capture = Some(VerifiedReadOnlyExecution {
                            plan: plan.clone(),
                            session_ref: session_ref.clone(),
                            owner: authenticated_user_id.clone(),
                            facts_version,
                            waiting_node: Some(id.clone()),
                            completed_nodes: plan
                                .nodes
                                .iter()
                                .filter(|node| completed.contains(node.id()))
                                .map(|node| node.id().clone())
                                .collect(),
                            bindings_hash: review.bindings_hash.clone(),
                            results: results.clone(),
                        });
                    }
                    return Ok(RuntimeOutcome::WaitingUser {
                        task_id,
                        node_id: id.clone(),
                    });
                }
                PlanNode::Tool {
                    id,
                    tool,
                    arguments,
                    ..
                } => {
                    if let Some(review) = review {
                        self.check_read_only_review(review)?;
                    }
                    let definition = self
                        .tools
                        .get(tool)
                        .ok_or_else(|| RuntimeError::ToolNotFound(tool.to_string()))?
                        .definition();
                    if definition.has_side_effect {
                        let Some((request, _)) = mock_write else {
                            self.record(
                                AuditEventKind::WriteRejected,
                                task_id.clone(),
                                Some(id.clone()),
                                None,
                                Some(tool.clone()),
                                json!({"reason": "WRITE_TOOL_REQUIRES_LATER_STAGE"}),
                            )?;
                            return Err(RuntimeError::WriteToolRequiresLaterStage(
                                tool.to_string(),
                            ));
                        };
                        if MockDraftAction::for_tool(tool.as_str()).map_or(true, |action| {
                            definition.action != action.action()
                                || request.contract_version != action.contract_version()
                        }) || tool != &request.tool_name
                            || definition.schema_version != MOCK_DRAFT_TOOL_SCHEMA_VERSION
                            || definition.contract_version
                                != safeflow_contracts::AGENT_TOOL_CONTRACT_VERSION
                            || id != &request.node_id
                            || arguments != &request.arguments
                        {
                            return Err(RuntimeError::MockDraftAuthorization(
                                "DRAFT_TOOL_DEFINITION_MISMATCH".into(),
                            ));
                        }
                    } else if mock_write.is_some() {
                        return Err(RuntimeError::MockDraftAuthorization(
                            "DRAFT_WRITE_MUST_DECLARE_SIDE_EFFECT".into(),
                        ));
                    }
                    let operation_id = mock_write
                        .map(|(request, _)| request.operation_id())
                        .unwrap_or_else(|| {
                            OperationId::parse(format!(
                                "operation_{}_{}",
                                task_id.as_str(),
                                id.as_str()
                            ))
                            .expect("task and node IDs are already non-empty")
                        });
                    for phase in [SecurityPhase::PreTool, SecurityPhase::PreAuth] {
                        let mut request =
                            security_request(&plan, node, &session_ref, facts_version, phase)?;
                        request.operation_id = operation_id.clone();
                        let decision = self
                            .security
                            .evaluate(request)
                            .await
                            .map_err(|error| RuntimeError::Security(error.to_string()))?;
                        if let Some(review) = review {
                            self.check_read_only_review(review)?;
                            if u64::from(decision.facts_version) != facts_version {
                                return Err(RuntimeError::RecoveryReview(
                                    "security facts version mismatch".into(),
                                ));
                            }
                        }
                        if mock_write.is_some_and(|(request, _)| request.persistent.is_some())
                            && (decision.operation_id != operation_id
                                || u64::from(decision.facts_version) != facts_version)
                        {
                            return Err(RuntimeError::MockDraftAuthorization(
                                "PERSISTENT_DECISION_BINDING_MISMATCH".into(),
                            ));
                        }
                        self.record(
                            AuditEventKind::SecurityEvaluated,
                            task_id.clone(),
                            Some(id.clone()),
                            Some(operation_id.clone()),
                            Some(tool.clone()),
                            json!({
                                "phase": phase,
                                "outcome": decision.outcome,
                                "risk_score": decision.risk_score,
                                "risk_tier": decision.risk_tier,
                                "scoring_mode": "COMPUTE_AUDIT_ONLY",
                                "decision_id": decision.decision_id.as_str()
                            }),
                        )?;
                        let allowed = if mock_write.is_some() {
                            decision.outcome == SecurityOutcome::ReadyToConfirm
                                && decision.minimum_level == SecurityLevel::Green
                                && decision.required_gates
                                    == vec![SecurityGate::ExplicitConfirmation]
                                && decision.findings.iter().all(|finding| {
                                    finding.finding.kind == FindingKind::Informational
                                        || (finding.finding.kind == FindingKind::RiskSignal
                                            && finding.applied_weight.is_some())
                                })
                        } else {
                            decision.outcome == SecurityOutcome::Allow
                        };
                        if !allowed {
                            return Err(RuntimeError::SecurityGate(decision.outcome));
                        }
                    }

                    self.record(
                        AuditEventKind::ToolStarted,
                        task_id.clone(),
                        Some(id.clone()),
                        Some(operation_id.clone()),
                        Some(tool.clone()),
                        json!({}),
                    )?;
                    let execution_context = safeflow_contracts::ExecutionContext {
                        task_id: task_id.clone(),
                        node_id: id.clone(),
                        operation_id: operation_id.clone(),
                        authenticated_user_id: authenticated_user_id.clone(),
                        plan_version,
                        idempotency_key: mock_write.map(|(request, _)| request.request_id.clone()),
                    };
                    if let Some(review) = review {
                        self.check_read_only_review(review)?;
                    }
                    let executable = self
                        .tools
                        .get(tool)
                        .expect("tool was resolved before security evaluation");
                    let result = crate::read_session::scope(
                        &self.sessions,
                        &session_ref,
                        executable.execute(
                            &execution_context,
                            match mock_write {
                                Some((_, permit)) => {
                                    ToolExecutionAuthorization::MockDraftWriteV1(permit)
                                }
                                None => ToolExecutionAuthorization::ReadOnly,
                            },
                            arguments.clone(),
                        ),
                    )
                    .await
                    .map_err(|error| RuntimeError::Security(error.to_string()))?
                    .map_err(|error| RuntimeError::Tool(error.to_string()))?;
                    if let Some(review) = review {
                        self.check_read_only_review(review)?;
                    }
                    if result.tool_name != *tool
                        || result.operation_id != operation_id
                        || result.status != ToolResultStatus::Succeeded
                        || result.contract_version
                            != safeflow_contracts::AGENT_TOOL_CONTRACT_VERSION
                    {
                        return Err(RuntimeError::Tool(
                            "Tool returned a failed, pending, or mismatched result".into(),
                        ));
                    }
                    if let Some((request, permit)) = mock_write {
                        self.mock_drafts
                            .as_ref()
                            .expect("configured authority")
                            .verify_tool_result(request, permit, &result)?;
                    }
                    let assessment = self
                        .security
                        .verify_execution(PostExecuteRequest {
                            contract_version: SECURITY_CONTRACT_VERSION,
                            task_id: task_id.clone(),
                            node_id: id.clone(),
                            operation_id: operation_id.clone(),
                            tool_name: tool.clone(),
                            session_ref: session_ref.clone(),
                            observation: ExecutionObservation::ResultReceived {
                                attempt_id: AttemptId::parse(format!(
                                    "attempt_{}_{}",
                                    task_id.as_str(),
                                    id.as_str()
                                ))
                                .expect("task and node IDs are already non-empty"),
                                business_operation_ref: BusinessOperationRef::parse(format!(
                                    "business_{}",
                                    operation_id.as_str()
                                ))
                                .expect("operation IDs are already non-empty"),
                                tool_result_ref: result.result_ref.clone(),
                            },
                        })
                        .await
                        .map_err(|error| RuntimeError::Security(error.to_string()))?;
                    if let Some(review) = review {
                        self.check_read_only_review(review)?;
                    }
                    if assessment.outcome != PostExecuteOutcome::VerifiedSucceeded {
                        return Err(RuntimeError::PostExecute(assessment.outcome));
                    }
                    self.record(
                        AuditEventKind::ToolCompleted,
                        task_id.clone(),
                        Some(id.clone()),
                        Some(operation_id),
                        Some(tool.clone()),
                        json!({
                            "result_ref": result.result_ref.as_str(),
                            "status": result.status,
                            "assessment": assessment.outcome
                        }),
                    )?;
                    results.push(result);
                    completed.insert(id.clone());
                }
            }
        }

        if let Some((request, _)) = mock_write.filter(|(request, _)| {
            request
                .persistent
                .as_ref()
                .is_some_and(|binding| binding.lifecycle)
        }) {
            let [result] = results.as_slice() else {
                return Err(RuntimeError::MockDraftAuthorization(
                    "SINGLE_ACTION_RESULT_REQUIRED".into(),
                ));
            };
            self.record_write(AuditEventKind::MockDraftActionVerified, request,
                json!({"result_ref":result.result_ref,"action":MockDraftAction::for_tool(request.tool_name.as_str()).map(MockDraftAction::action)}))?;
            return Ok(RuntimeOutcome::MockDraftActionVerified {
                task_id,
                operation_id: request.operation_id(),
                result: result.clone(),
            });
        }
        self.record(
            AuditEventKind::TaskCompleted,
            task_id.clone(),
            None,
            None,
            None,
            json!({"state": "COMPLETED"}),
        )?;
        if let Some((review, capture)) = read_only.as_mut() {
            self.check_read_only_review(review)?;
            **capture = Some(VerifiedReadOnlyExecution {
                plan: plan.clone(),
                session_ref: session_ref.clone(),
                owner: authenticated_user_id,
                facts_version,
                waiting_node: None,
                completed_nodes: plan.nodes.iter().map(|node| node.id().clone()).collect(),
                bindings_hash: review.bindings_hash.clone(),
                results: results.clone(),
            });
        }
        Ok(RuntimeOutcome::Completed {
            task_id,
            state: TaskState::Completed,
            results,
        })
    }

    fn record(
        &self,
        kind: AuditEventKind,
        task_id: TaskId,
        node_id: Option<NodeId>,
        operation_id: Option<OperationId>,
        tool_name: Option<safeflow_contracts::ToolName>,
        details: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        let sequence = self.audit_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        self.audit
            .record(AuditEvent {
                event_id: EventId::parse(format!("audit_{sequence}"))
                    .expect("generated audit IDs always have content"),
                kind,
                task_id,
                node_id,
                operation_id,
                tool_name,
                details,
            })
            .map_err(|error| RuntimeError::Audit(error.to_string()))
    }
}

pub(crate) fn hash_arguments(arguments: &serde_json::Value) -> ArgumentsHash {
    let serialized = serde_json::to_vec(arguments).expect("JSON values always serialize");
    let digest = Sha256::digest(serialized);
    let value = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("");
    ArgumentsHash::parse(value).expect("SHA-256 is always a lowercase 64-character digest")
}

pub(crate) fn security_request(
    plan: &AgentPlan,
    node: &PlanNode,
    session_ref: &SessionRef,
    facts_version: u64,
    phase: SecurityPhase,
) -> Result<OperationSecurityRequest, RuntimeError> {
    let PlanNode::Tool {
        id,
        tool,
        arguments,
        ..
    } = node
    else {
        return Err(RuntimeError::InvalidPlan(
            "security check requires tool node".into(),
        ));
    };
    Ok(OperationSecurityRequest {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: plan.task_id.clone(),
        node_id: id.clone(),
        operation_id: OperationId::parse(format!(
            "operation_{}_{}",
            plan.task_id.as_str(),
            id.as_str()
        ))
        .expect("validated IDs"),
        facts_version: u32::try_from(facts_version).map_err(|_| {
            RuntimeError::RecoveryReview("facts version exceeds security contract".into())
        })?,
        phase,
        tool_name: tool.clone(),
        plan_version: plan.plan_version,
        session_ref: session_ref.clone(),
        normalized_arguments: arguments.clone(),
        arguments_hash: hash_arguments(arguments),
        idempotency_key: None,
        confirmation_ref: None,
        auth_grant_ref: None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeOutcome {
    MockDraftActionVerified {
        task_id: TaskId,
        operation_id: OperationId,
        result: ToolResult,
    },
    Completed {
        task_id: TaskId,
        state: TaskState,
        results: Vec<ToolResult>,
    },
    WaitingUser {
        task_id: TaskId,
        node_id: NodeId,
    },
    NeedsClarification {
        question: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidPlan(String),
    PlanRejected,
    ToolNotFound(String),
    WriteToolRequiresLaterStage(String),
    Security(String),
    SecurityGate(SecurityOutcome),
    PostExecute(PostExecuteOutcome),
    Tool(String),
    Audit(String),
    MockDraftAuthorization(String),
    RecoveryReview(String),
    SessionChanged,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPlan(message) => write!(formatter, "invalid plan: {message}"),
            Self::PlanRejected => formatter.write_str("plan was rejected by SecurityKernel"),
            Self::ToolNotFound(name) => write!(formatter, "tool not found: {name}"),
            Self::WriteToolRequiresLaterStage(name) => {
                write!(
                    formatter,
                    "write tool requires a later implementation stage: {name}"
                )
            }
            Self::Security(message) => write!(formatter, "security evaluation failed: {message}"),
            Self::SecurityGate(outcome) => {
                write!(formatter, "security gate stopped execution: {outcome:?}")
            }
            Self::PostExecute(outcome) => {
                write!(
                    formatter,
                    "post-execution verification stopped completion: {outcome:?}"
                )
            }
            Self::Tool(message) => write!(formatter, "tool execution failed: {message}"),
            Self::Audit(message) => write!(formatter, "audit recording failed: {message}"),
            Self::MockDraftAuthorization(message) => {
                write!(formatter, "mock draft authorization failed: {message}")
            }
            Self::RecoveryReview(message) => {
                write!(formatter, "read-only wait requires review: {message}")
            }
            Self::SessionChanged => {
                formatter.write_str("session identity changed during read-only wait")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}
