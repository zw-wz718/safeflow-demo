//! Typed, read-only handoff of an original saved observation; never a permit.
use super::{permitted, valid_id, FeedbackEvent, SavedFeedback, StoreError};
use crate::{
    feedback::{FeedbackChoice, FeedbackEvidence},
    spending::SourceKey,
    TrustedScope,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardDisposition {
    StopMine,
    StopUnsure,
    StopNoCard,
    /// A reported card reference still requires trusted current association and
    /// the public confirmation/permission/final transaction checks.
    AssociationUnverified,
}

/// Borrowed from a checked SavedFeedback. No public constructor, Deserialize,
/// setters or Serialize of the full projection. Not a live authorization token.
#[derive(Debug)]
pub struct FeedbackProjection<'a> {
    operation_id: &'a str,
    owner_user_id: &'a str,
    choice: &'a FeedbackChoice,
    reference: &'a SourceKey,
    card_id: Option<&'a str>,
    report_id: &'a str,
    report_account_ids: &'a [String],
    snapshot_id: &'a str,
    rule_version: &'a str,
    calculated_at: &'a str,
    local_event: LocalEvent<'a>,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct LocalEvent<'a> {
    namespace: &'static str,
    event_id: i64,
    event_kind: &'a str,
    recorded_at: &'a str,
}

/// Proposed minimal disclosure only, NOT a PendingPublicAudit or adopted
/// public contract. Local event IDs are scoped to the trusted database/schema.
#[derive(Debug, Serialize)]
pub struct BillingAuditCandidate<'a> {
    feedback_operation_id: &'a str,
    choice: &'a FeedbackChoice,
    state: &'static str,
    persisted: bool,
    lock_authorized: bool,
    local_event: LocalEvent<'a>,
}

impl FeedbackProjection<'_> {
    pub fn operation_id(&self) -> &str {
        self.operation_id
    }
    pub fn owner_user_id(&self) -> &str {
        self.owner_user_id
    }
    pub fn choice(&self) -> &FeedbackChoice {
        self.choice
    }
    pub fn reference(&self) -> &SourceKey {
        self.reference
    }
    pub fn card_id(&self) -> Option<&str> {
        self.card_id
    }
    pub fn report_id(&self) -> &str {
        self.report_id
    }
    pub fn report_account_ids(&self) -> &[String] {
        self.report_account_ids
    }
    /// Original source snapshot, NOT a monotonic feedback/card revision.
    pub fn snapshot_id(&self) -> &str {
        self.snapshot_id
    }
    pub fn rule_version(&self) -> &str {
        self.rule_version
    }
    pub fn calculated_at(&self) -> &str {
        self.calculated_at
    }
    pub fn card_disposition(&self) -> CardDisposition {
        match self.choice {
            FeedbackChoice::Mine => CardDisposition::StopMine,
            FeedbackChoice::Unsure => CardDisposition::StopUnsure,
            FeedbackChoice::NotMine if self.card_id.is_none() => CardDisposition::StopNoCard,
            FeedbackChoice::NotMine => CardDisposition::AssociationUnverified,
        }
    }
    pub fn audit_candidate(&self) -> BillingAuditCandidate<'_> {
        BillingAuditCandidate {
            feedback_operation_id: self.operation_id,
            choice: self.choice,
            state: "SAVED",
            persisted: true,
            lock_authorized: false,
            local_event: self.local_event,
        }
    }
}

pub(super) fn validate_evidence(
    scope: &TrustedScope,
    data: &FeedbackEvidence,
    event: &FeedbackEvent,
) -> Result<(), StoreError> {
    if !valid_id(scope.user_id()) {
        return Err(StoreError::InvalidInput);
    }
    if !permitted(scope, data) {
        return Err(StoreError::ResourceUnavailable);
    }
    let accounts: std::collections::BTreeSet<_> = data.request.account_ids.iter().collect();
    if accounts.is_empty()
        || accounts.len() != data.request.account_ids.len()
        || !accounts.contains(&data.reference.account_id)
        || accounts
            .iter()
            .any(|account| crate::validate_reference(account).is_err())
        || [
            &data.public_operation_id,
            &data.owner_user_id,
            &data.report_id,
            &data.reference.source_id,
            &data.reference.account_id,
            &data.reference.transaction_id,
        ]
        .iter()
        .any(|id| !valid_id(id))
        || [&data.snapshot_id, &data.rule_version]
            .iter()
            .any(|id| crate::validate_reference(id).is_err())
        || data
            .transaction
            .card_id
            .as_deref()
            .is_some_and(|id| crate::validate_reference(id).is_err())
        || data.transaction.account_id != data.reference.account_id
        || data.transaction.transaction_id != data.reference.transaction_id
        || chrono::DateTime::parse_from_rfc3339(&data.calculated_at).is_err()
        || chrono::DateTime::parse_from_rfc3339(&event.recorded_at).is_err()
        || event.event_id <= 0
        || event.event_kind != "BILLING_FEEDBACK_RECORDED_LOCAL_V1"
    {
        return Err(StoreError::CorruptRecord);
    }
    Ok(())
}

impl SavedFeedback {
    /// Caller supplies CURRENT trusted scope. This rechecks the original report
    /// accounts, but cannot discover session revocation or current card changes.
    /// Use load_feedback first; do not cache this as a permit or create via JSON.
    pub fn checked_projection(
        &self,
        scope: &TrustedScope,
    ) -> Result<FeedbackProjection<'_>, StoreError> {
        let data = &self.evidence;
        validate_evidence(scope, data, &self.event)?;
        if self.state != "SAVED" || !self.persisted || self.lock_authorized {
            return Err(StoreError::CorruptRecord);
        }
        Ok(FeedbackProjection {
            operation_id: &data.public_operation_id,
            owner_user_id: &data.owner_user_id,
            choice: &data.choice,
            reference: &data.reference,
            card_id: data.transaction.card_id.as_deref(),
            report_id: &data.report_id,
            report_account_ids: &data.request.account_ids,
            snapshot_id: &data.snapshot_id,
            rule_version: &data.rule_version,
            calculated_at: &data.calculated_at,
            local_event: LocalEvent {
                namespace: "billing_feedback_events_v1",
                event_id: self.event.event_id,
                event_kind: &self.event.event_kind,
                recorded_at: &self.event.recorded_at,
            },
        })
    }
}
