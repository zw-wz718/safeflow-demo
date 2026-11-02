//! Trusted backend recovery storage. Stored states never grant execution permission.
mod audit_outbox;
mod draft_balance_revision;
mod audit_sender;
mod billing_report;
mod card_catalog;
mod card_read;
mod confirmation;
mod confirmation_repository;
mod daily_usage;
mod decision;
mod execution_request;
mod lifecycle;
mod models;
mod payment_completion;
mod payment_background_identity;
pub use payment_background_identity::*;
mod payment_preparation;
pub use payment_completion::*;
mod permit;
mod planning_confirmation;
mod planning_task;
mod repository;
mod chat_request_recovery;
mod runtime_continuation;
mod runtime_execution;
mod runtime_recovery;
mod service;
mod shared_demo;
mod unified_fictional_document;
mod unified_fictional_source;
mod recipient_profile;
pub use recipient_profile::*;
pub use unified_fictional_document::*;
pub use unified_fictional_source::*;
mod simulation_auth;
mod simulation_source;
pub use simulation_source::*;
mod team_consumption;
pub use team_consumption::*;
mod subscription;
mod subscription_local_audit;
mod subscription_reminder;
pub use subscription::*;
pub use subscription_reminder::*;
#[cfg(test)]
#[path = "../../../features/subscription/tools/billing_direct_fixture.rs"]
mod billing_fixture;
mod sql_bank_gateway;
#[path = "../../../features/subscription/adapters/billing_reader_mock.rs"]
pub mod subscription_billing_reader;
mod task_query;
mod transfer_result;
mod transfer_sql;

pub use audit_outbox::*;
pub use audit_sender::*;
pub use billing_report::*;
pub use card_catalog::*;
pub use card_read::*;
pub use confirmation::*;
pub use daily_usage::*;
pub use decision::*;
pub use execution_request::*;
pub use lifecycle::{
    IntentReplacement,
    LifecycleAuditSource, LifecycleAuditSourceReader, LifecycleAuditSources,
    LifecycleDraftObservation, LifecycleDraftObserver, LifecycleReceipt, LifecycleSelection,
    LifecycleService, LifecycleView,
};
pub use models::*;
pub use payment_preparation::*;
pub use permit::*;
pub use planning_confirmation::*;
pub use planning_task::*;
pub use repository::PgRepository;
pub use runtime_continuation::*;
pub use runtime_execution::*;
pub use runtime_recovery::{
    RuntimeRecoveryError, RuntimeRecoveryService, RuntimeWaitingTask, WaitingTaskVersion,
};
pub use service::PersistenceService;
pub use simulation_auth::*;
pub use sql_bank_gateway::*;
pub use task_query::*;
pub use transfer_result::*;
pub use transfer_sql::*;

use std::fmt;

pub const PERSISTENCE_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    Invalid(&'static str),
    Session,
    NotFound,
    RevisionConflict,
    PlanConflict,
    StalePlan,
    FactsConflict,
    IdConflict,
    Terminal,
    DraftIntentExpired,
    DraftIntentReplaced,
    DraftIntentAbandoned,
    IllegalTransition,
    ReconcileRequired,
    Database,
    Migration,
    CorruptRecord,
    ConfirmationExpired,
    ConfirmationRevoked,
    ConfirmationRequired,
    ConfirmationBindingMismatch,
    PermitIssuanceUnavailable,
    PermitNotActive,
    PermitExpired,
    PermitBindingMismatch,
    PermitConflict,
    PermitVersionOverflow,
    TransactionAborted,
    TransactionScope,
    FormalAuthUnavailable,
    SimulationModeRequired,
    AuthChallengeInactive,
    AuthGrantRevoked,
    AuthExpired,
    AuthBindingMismatch,
    AuthChallengeConflict,
    AuthSubmissionConflict,
    DecisionSourcesUnavailable,
    DecisionConflict,
    DecisionInactive,
    DecisionExpired,
    DecisionBindingMismatch,
    DecisionAssemblyChanged,
    DecisionStillCurrent,
    RuntimeAlreadyClaimed,
    RuntimeIntakeRequired,
    RuntimeContinuationRequired,
    RuntimeWaitingNodeMismatch,
    DependencyUnavailable,
    ExecutionKeyConflict,
    ExecutionOperationConflict,
    BillingRecovery(&'static str),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid recovery checkpoint: {message}"),
            _ => write!(f, "recovery storage: {self:?}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<sqlx::Error> for StorageError {
    fn from(error: sqlx::Error) -> Self {
        if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("40001") {
            return Self::RevisionConflict;
        }
        // Do not expose connection strings, SQL parameters, or database diagnostics.
        Self::Database
    }
}

pub use permit::card_safety::final_transaction::{CardFinalDeployment, CardOriginalOutcome};
