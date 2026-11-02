//! Versioned contracts shared by the SafeFlow runtime, security kernel, and tools.
//!
//! This crate contains data contracts and deterministic validation only. It does
//! not perform I/O, issue credentials, access a database, or execute tools.

mod execution_observation;
mod execution_permit;
mod fingerprint;
mod ids;
mod mock_draft;
mod payment_locator;
mod planning;
mod pre_commit;
mod security;
mod states;
mod tool;

pub type ContractFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub use execution_observation::{
    ExecutionObservation, PostExecuteAssessment, PostExecuteOutcome,
    EXECUTION_OBSERVATION_CONTRACT_VERSION,
};
pub use execution_permit::{
    ExecutionPermit, ExecutionPermitStatus, PermitError, PermitIssuanceRequest, PermitUseRequest,
    EXECUTION_PERMIT_CONTRACT_VERSION,
};
pub use fingerprint::{
    compute_operation_fingerprint, FingerprintError, FingerprintPayloadV1, OperationFingerprint,
    OPERATION_FINGERPRINT_VERSION,
};
pub use ids::{
    AttemptId, AuthGrantRef, BusinessOperationRef, CheckId, ConfirmationRef, DecisionId, EventId,
    EvidenceRef, IdempotencyKey, IdentifierError, NodeId, OperationId, PermitId, PolicyVersion,
    ReasonCode, ResourceId, SessionRef, SourceName, TaskId, ToolName, ToolResultRef, UserId,
};
pub use mock_draft::{
    MockDraftAction, MockDraftPermit, MOCK_DRAFT_AUTHORIZATION_CONTRACT_VERSION,
    MOCK_DRAFT_CANCEL_ACTION, MOCK_DRAFT_CANCEL_TOOL, MOCK_DRAFT_CREATE_ACTION,
    MOCK_DRAFT_CREATE_TOOL, MOCK_DRAFT_LIFECYCLE_CONTRACT_VERSION, MOCK_DRAFT_MODIFY_ACTION,
    MOCK_DRAFT_MODIFY_TOOL, MOCK_DRAFT_TOOL_SCHEMA_VERSION,
};
pub use payment_locator::{
    PaymentDraftLocator, PAYMENT_INTENT_POLICY, PAYMENT_INTENT_TOOL, PAYMENT_LOCATOR_PROJECTION,
};
pub use planning::{
    AgentPlan, Clarification, PlanContractError, PlanNode, PlanningResult, Understanding,
    UnderstandingDecision, AGENT_PLAN_CONTRACT_VERSION, PLANNING_RESULT_CONTRACT_VERSION,
};
pub use pre_commit::{
    PreCommitAuthenticationV2, PreCommitContractError, PreCommitSecurityRequestV2,
    PRE_COMMIT_SECURITY_CONTRACT_VERSION,
};
pub use security::{
    ArgumentsHash, DomainPolicy, EvaluatedFinding, FindingKind, OperationSecurityRequest,
    PlanAcceptanceDecision, PlanAcceptanceRequest, PostExecuteContext, PostExecuteRequest,
    RiskTier, SecurityContext, SecurityContractError, SecurityDecision, SecurityError,
    SecurityFinding, SecurityGate, SecurityKernel, SecurityLevel, SecurityPhase,
    SECURITY_CONTRACT_VERSION,
};
pub use states::{
    OperationEvent, OperationState, SecurityOutcome, StateTransitionError, TaskEvent, TaskState,
    STATE_MACHINE_CONTRACT_VERSION,
};
pub use tool::{
    AgentTool, ExecutionContext, ToolContractError, ToolDefinition, ToolError,
    ToolExecutionAuthorization, ToolResult, ToolResultStatus, AGENT_TOOL_CONTRACT_VERSION,
};
