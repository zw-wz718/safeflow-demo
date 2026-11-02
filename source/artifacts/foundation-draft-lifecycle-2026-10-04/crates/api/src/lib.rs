//! Local project simulation only. Public responses never serialize internal TaskRecord.
mod assembly;
mod audit_worker;
mod billing;
mod card;
mod database_card;
mod http;
mod payment_tool;
#[cfg(test)]
mod plugin_test_support;
mod session;
// Kept under http so native assembly can reuse private identity/bank/Runtime.
pub use assembly::ReadOnlyToolRegistration;
pub use audit_worker::{serve_with_public_audit, AuditWorkerConfig, AuditWorkerFailure};
pub use billing::{
    billing_report_registration_for_backend, LocalBillingSource, BILLING_REPORT_TOOL,
};
pub use card::{card_facts_registration_for_backend, SessionCardFacts};
pub use database_card::database_card_facts_registration_for_backend;
pub use http::{
    router, ApiConfig, ApiStartupError, ApiState, BillingFeedbackConfig, DraftConfirmationConfig,
    PaymentExecutionConfig, PublicDemoConfig, TaskResponse,
};
