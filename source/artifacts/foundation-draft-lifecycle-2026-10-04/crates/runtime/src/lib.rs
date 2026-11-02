//! Deterministic SafeFlow orchestration built around `safeflow-contracts`.
//!
//! This crate implements the step-two in-memory path and step-three validated
//! startup bindings, plus an opt-in process-local mock draft write path. It has
//! no database, funds execution, or production policy engine. An explicitly configured
//! OpenAI-compatible Gateway only proposes plans through strict intake.
//! The optional read-only wait path exposes sealed execution progress for storage
//! orchestration; recovering that wait never executes a Tool or rebuilds a Permit.

mod audit;
mod continuation;
mod decision_input;
mod decision_review;
mod mock_draft;
mod mock_transfer_receipt;
mod openai_gateway;
mod planning;
mod planning_intake;
mod pre_commit;
mod read_session;
pub use read_session::AuthenticatedReadSession;
mod recovery;
mod registry;
mod risk;
mod runtime;
mod security;
mod startup;

pub use audit::{AuditError, AuditEvent, AuditEventKind, AuditRecorder, InMemoryAuditRecorder};
pub use continuation::{ReadOnlyContinuationSource, READ_ONLY_CONTINUATION_CONTRACT_VERSION};
pub use decision_review::{PreflightInputContract, VerifiedPreflightDecision};
pub use mock_draft::{
    MockDraftAuthority, MockDraftClock, MockDraftConfirmation, MockDraftWriteRequest,
    VerifiedMockDraftActionOutcome,
};
pub use mock_transfer_receipt::{
    review_mock_transfer_receipt_proposal, MockTransferReceiptProposalReview,
    ReceiptObservationComparison, ReceiptObservationPhase, ReceiptProposalError,
    ReceiptProposalSummary, MOCK_TRANSFER_PROPOSAL_SOURCE_REVISION,
};
pub use openai_gateway::{OpenAiGateway, OpenAiGatewayConfig};
pub use planning::{
    LlmGateway, MockLlm, PlanningContext, PlanningError, PlanningGatewaySource, PlanningInput,
    PlanningService,
};
pub use planning_intake::{
    parse_bounded_input_json_for_backend, AcceptedPlanningProposal, PlanningIntakeError,
    PlanningIntakeService, PlanningPersistenceReview, PLANNING_INTAKE_CONTRACT_VERSION,
};
pub use recovery::{
    ReadOnlyPlanReview, VerifiedReadOnlyExecution, VerifiedReadOnlyWait,
    READ_ONLY_EXECUTION_CONTRACT_VERSION, READ_ONLY_WAIT_CONTRACT_VERSION,
};
pub use registry::{
    DomainPolicyDefinition, DomainPolicyRegistry, RegistrationError, ToolRegistry,
    ToolSecurityCatalog, ToolSecurityProfile,
};
pub use risk::{
    demo_risk_definitions, risk_tier_for_score, safe_risk_reference, validate_count_observation,
    CentralPolicyCatalog, MissingRiskDataPolicy, RiskCheckDefinition, CENTRAL_SCORING_VERSION,
    DEMO_RISK_VERSION,
};
pub use runtime::{Runtime, RuntimeError, RuntimeOutcome};
pub use security::{
    BackendFinalPolicy, BackendFinalPolicyRegistration, PaymentMinimumInput, SessionBinding,
    SessionResolver, StaticSessionResolver, VerifiedPaymentMinimum, PAYMENT_MINIMUM_VERSION,
};
pub use startup::{
    validate_tool_policy_bindings, RuntimeAssembly, RuntimeBootstrap, StartupValidationError,
};
