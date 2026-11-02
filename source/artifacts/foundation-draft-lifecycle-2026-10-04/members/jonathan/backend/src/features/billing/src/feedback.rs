//! Immutable feedback preparation; optional storage lives in feedback_store.
use crate::{
    query::{Request, Transaction},
    spending::{SourceKey, SpendingReport},
    TrustedScope,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeedbackChoice {
    Mine,
    NotMine,
    Unsure,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackIntent {
    pub report_id: String,
    pub transaction: SourceKey,
    pub choice: FeedbackChoice,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreparedFeedback {
    state: &'static str,
    persisted: bool,
    lock_authorized: bool,
    card_association_verified: bool,
    #[serde(flatten)]
    pub(crate) evidence: FeedbackEvidence,
}
/// Internal stored representation, not a model input or write credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeedbackEvidence {
    pub public_operation_id: String,
    pub owner_user_id: String,
    pub report_id: String,
    pub rule_version: String,
    pub snapshot_id: String,
    pub calculated_at: String,
    pub request: Request,
    pub reference: SourceKey,
    pub transaction: Transaction,
    pub choice: FeedbackChoice,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationComparison {
    IdenticalRetry,
    Conflict,
    NewOperation,
}
impl PreparedFeedback {
    /// Pure comparison only. NewOperation does NOT approve another feedback on
    /// the transaction; D-20 editing semantics still require team adoption.
    pub fn compare_operation(&self, other: &Self) -> OperationComparison {
        if self.evidence.public_operation_id != other.evidence.public_operation_id {
            OperationComparison::NewOperation
        } else if self == other {
            OperationComparison::IdenticalRetry
        } else {
            OperationComparison::Conflict
        }
    }
}
/// operation_id must come from the trusted public executor, not model input.
/// Neither TrustedScope nor this value authenticates a caller or grants a write.
pub fn prepare_feedback(
    scope: &TrustedScope,
    report: &SpendingReport,
    operation_id: &str,
    intent: &FeedbackIntent,
) -> Result<PreparedFeedback, &'static str> {
    let original = report.verified_for(scope)?;
    crate::validate_reference(operation_id).map_err(|_| "INVALID_OPERATION_ID")?;
    if original.metadata.report_id != intent.report_id {
        return Err("REPORT_MISMATCH");
    }
    let evidence = original
        .rows
        .iter()
        .find(|row| row.reference == intent.transaction)
        .ok_or("TRANSACTION_NOT_IN_REPORT")?;
    Ok(PreparedFeedback {
        state: "PREPARED_NOT_SAVED",
        persisted: false,
        lock_authorized: false,
        card_association_verified: false,
        evidence: FeedbackEvidence {
            public_operation_id: operation_id.into(),
            owner_user_id: original.owner_user_id.clone(),
            report_id: original.metadata.report_id.clone(),
            rule_version: original.rule_version.clone(),
            snapshot_id: original.snapshot_id.clone(),
            calculated_at: original.metadata.calculated_at.clone(),
            request: original.request.clone(),
            reference: evidence.reference.clone(),
            transaction: evidence.transaction.clone(),
            choice: intent.choice.clone(),
        },
    })
}
