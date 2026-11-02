use crate::{
    AgentPlan, AuthGrantRef, CheckId, ConfirmationRef, ContractFuture, DecisionId, EventId,
    EvidenceRef, ExecutionObservation, IdempotencyKey, NodeId, OperationId, PlanContractError,
    PolicyVersion, PostExecuteAssessment, ReasonCode, SessionRef, SourceName, TaskId, ToolName,
    UserId,
};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;

pub const SECURITY_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ArgumentsHash(String);

impl ArgumentsHash {
    pub fn parse(value: impl Into<String>) -> Result<Self, SecurityContractError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SecurityContractError::InvalidArgumentsHash);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ArgumentsHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAcceptanceRequest {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub plan_version: u32,
    pub plan: AgentPlan,
}

impl PlanAcceptanceRequest {
    pub fn new(plan: AgentPlan) -> Result<Self, SecurityContractError> {
        plan.validate_envelope()?;
        Ok(Self {
            contract_version: SECURITY_CONTRACT_VERSION,
            task_id: plan.task_id.clone(),
            plan_version: plan.plan_version,
            plan,
        })
    }

    pub fn validate(&self) -> Result<(), SecurityContractError> {
        if self.contract_version != SECURITY_CONTRACT_VERSION {
            return Err(SecurityContractError::UnsupportedContractVersion(
                self.contract_version,
            ));
        }
        self.plan.validate_envelope()?;
        if self.task_id != self.plan.task_id {
            return Err(SecurityContractError::Plan(PlanContractError::TaskMismatch));
        }
        if self.plan_version != self.plan.plan_version {
            return Err(SecurityContractError::Plan(
                PlanContractError::PlanVersionMismatch,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAcceptanceDecision {
    pub contract_version: u16,
    pub decision_id: DecisionId,
    pub task_id: TaskId,
    pub plan_version: u32,
    pub accepted: bool,
    pub policy_version: PolicyVersion,
    pub findings: Vec<SecurityFinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SecurityPhase {
    PreTool,
    PreAuth,
    PreCommit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSecurityRequest {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub operation_id: OperationId,
    pub facts_version: u32,
    pub phase: SecurityPhase,
    pub tool_name: ToolName,
    pub plan_version: u32,
    pub session_ref: SessionRef,
    pub normalized_arguments: Value,
    pub arguments_hash: ArgumentsHash,
    pub idempotency_key: Option<IdempotencyKey>,
    pub confirmation_ref: Option<ConfirmationRef>,
    pub auth_grant_ref: Option<AuthGrantRef>,
}

impl OperationSecurityRequest {
    pub fn validate(&self) -> Result<(), SecurityContractError> {
        if self.contract_version != SECURITY_CONTRACT_VERSION {
            return Err(SecurityContractError::UnsupportedContractVersion(
                self.contract_version,
            ));
        }
        if self.facts_version == 0 {
            return Err(SecurityContractError::ZeroFactsVersion);
        }
        if self.plan_version == 0 {
            return Err(SecurityContractError::ZeroPlanVersion);
        }
        if self.phase == SecurityPhase::PreCommit {
            if self.idempotency_key.is_none() {
                return Err(SecurityContractError::MissingPreCommitReference(
                    "idempotency_key",
                ));
            }
            if self.confirmation_ref.is_none() {
                return Err(SecurityContractError::MissingPreCommitReference(
                    "confirmation_ref",
                ));
            }
            if self.auth_grant_ref.is_none() {
                return Err(SecurityContractError::MissingPreCommitReference(
                    "auth_grant_ref",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityContext {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub operation_id: OperationId,
    pub facts_version: u32,
    pub phase: SecurityPhase,
    pub tool_name: ToolName,
    pub plan_version: u32,
    pub authenticated_user_id: UserId,
    pub normalized_arguments: Value,
    pub arguments_hash: ArgumentsHash,
    pub idempotency_key: Option<IdempotencyKey>,
    pub confirmation_ref: Option<ConfirmationRef>,
    pub auth_grant_ref: Option<AuthGrantRef>,
}

impl SecurityContext {
    pub fn from_request(
        request: OperationSecurityRequest,
        authenticated_user_id: UserId,
    ) -> Result<Self, SecurityContractError> {
        request.validate()?;
        Ok(Self {
            contract_version: SECURITY_CONTRACT_VERSION,
            task_id: request.task_id,
            node_id: request.node_id,
            operation_id: request.operation_id,
            facts_version: request.facts_version,
            phase: request.phase,
            tool_name: request.tool_name,
            plan_version: request.plan_version,
            authenticated_user_id,
            normalized_arguments: request.normalized_arguments,
            arguments_hash: request.arguments_hash,
            idempotency_key: request.idempotency_key,
            confirmation_ref: request.confirmation_ref,
            auth_grant_ref: request.auth_grant_ref,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FindingKind {
    HardDeny,
    NeedsInput,
    RequireGate,
    RiskSignal,
    Informational,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityFinding {
    pub check_id: CheckId,
    pub policy_version: PolicyVersion,
    pub source: SourceName,
    pub kind: FindingKind,
    pub reason_code: ReasonCode,
    pub evidence_refs: Vec<EvidenceRef>,
    pub observed_value: Option<Value>,
    pub event_ids: Vec<EventId>,
    pub dedup_key: Option<String>,
    pub window_start_ms: Option<i64>,
    pub window_end_ms: Option<i64>,
}

impl SecurityFinding {
    pub fn validate(&self) -> Result<(), SecurityContractError> {
        match (self.window_start_ms, self.window_end_ms) {
            (None, None) => Ok(()),
            (Some(start), Some(end)) if start >= 0 && end > start => Ok(()),
            _ => Err(SecurityContractError::InvalidFindingWindow),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatedFinding {
    pub finding: SecurityFinding,
    pub applied_weight: Option<u32>,
    pub score_delta: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SecurityLevel {
    Green,
    Yellow,
    Red,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskTier {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SecurityGate {
    ExplicitConfirmation,
    StrongAuthentication,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityDecision {
    pub contract_version: u16,
    pub decision_id: DecisionId,
    pub operation_id: OperationId,
    pub facts_version: u32,
    pub policy_version: PolicyVersion,
    pub outcome: crate::SecurityOutcome,
    pub minimum_level: SecurityLevel,
    pub risk_score: u32,
    pub risk_tier: RiskTier,
    pub required_gates: Vec<SecurityGate>,
    pub findings: Vec<EvaluatedFinding>,
    pub expires_at_ms: i64,
}

impl SecurityDecision {
    pub fn validate(&self) -> Result<(), SecurityContractError> {
        if self.contract_version != SECURITY_CONTRACT_VERSION {
            return Err(SecurityContractError::UnsupportedContractVersion(
                self.contract_version,
            ));
        }
        if self.facts_version == 0 {
            return Err(SecurityContractError::ZeroFactsVersion);
        }
        if self.risk_score > 100 {
            return Err(SecurityContractError::RiskScoreOutOfRange);
        }
        if self.expires_at_ms < 0 {
            return Err(SecurityContractError::InvalidExpiry);
        }
        for finding in &self.findings {
            finding.finding.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostExecuteRequest {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub operation_id: OperationId,
    pub tool_name: ToolName,
    pub session_ref: SessionRef,
    pub observation: ExecutionObservation,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PostExecuteContext {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub operation_id: OperationId,
    pub tool_name: ToolName,
    pub authenticated_user_id: UserId,
    pub observation: ExecutionObservation,
}

impl PostExecuteContext {
    pub fn from_request(request: PostExecuteRequest, authenticated_user_id: UserId) -> Self {
        Self {
            contract_version: SECURITY_CONTRACT_VERSION,
            task_id: request.task_id,
            node_id: request.node_id,
            operation_id: request.operation_id,
            tool_name: request.tool_name,
            authenticated_user_id,
            observation: request.observation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityContractError {
    UnsupportedContractVersion(u16),
    InvalidArgumentsHash,
    ZeroFactsVersion,
    ZeroPlanVersion,
    MissingPreCommitReference(&'static str),
    InvalidFindingWindow,
    RiskScoreOutOfRange,
    InvalidExpiry,
    Plan(PlanContractError),
}

impl fmt::Display for SecurityContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedContractVersion(version) => {
                write!(
                    formatter,
                    "unsupported security contract version: {version}"
                )
            }
            Self::InvalidArgumentsHash => {
                formatter.write_str("arguments_hash must be 64 lowercase hexadecimal characters")
            }
            Self::ZeroFactsVersion => {
                formatter.write_str("facts_version must be greater than zero")
            }
            Self::ZeroPlanVersion => formatter.write_str("plan_version must be greater than zero"),
            Self::MissingPreCommitReference(field) => {
                write!(formatter, "PRE_COMMIT requires {field}")
            }
            Self::InvalidFindingWindow => formatter.write_str(
                "finding window must be absent or have a non-negative start before its end",
            ),
            Self::RiskScoreOutOfRange => {
                formatter.write_str("risk_score must be in the range 0..=100")
            }
            Self::InvalidExpiry => formatter.write_str("expires_at_ms must be non-negative"),
            Self::Plan(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SecurityContractError {}

impl From<PlanContractError> for SecurityContractError {
    fn from(value: PlanContractError) -> Self {
        Self::Plan(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityError {
    pub reason_code: ReasonCode,
    pub message: String,
}

impl fmt::Display for SecurityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.reason_code, self.message)
    }
}

impl std::error::Error for SecurityError {}

pub trait DomainPolicy: Send + Sync {
    fn tool_name(&self) -> &ToolName;

    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>>;

    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>>;
}

pub trait SecurityKernel: Send + Sync {
    fn validate_plan<'a>(
        &'a self,
        request: PlanAcceptanceRequest,
    ) -> ContractFuture<'a, Result<PlanAcceptanceDecision, SecurityError>>;

    fn evaluate<'a>(
        &'a self,
        request: OperationSecurityRequest,
    ) -> ContractFuture<'a, Result<SecurityDecision, SecurityError>>;

    fn verify_execution<'a>(
        &'a self,
        request: PostExecuteRequest,
    ) -> ContractFuture<'a, Result<PostExecuteAssessment, SecurityError>>;
}
