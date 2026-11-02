//! Connection-local evidence observations, never a commit or authorization proof.
use super::{
    decode, permitted, projection::validate_evidence, valid_id, FeedbackEvent, StoreError, READ,
};
use crate::{
    feedback::{FeedbackChoice, FeedbackEvidence},
    query::Request,
    spending::SourceKey,
    TrustedScope,
};
use sqlx::PgConnection;

/// Owned evidence observed on the caller's connection, possibly uncommitted.
/// Not an authorization/commit token. Intentionally no Debug/Serialize/Deserialize.
///
/// ```compile_fail
/// use safeflow_billing_source_reader::feedback_store::CheckedFeedbackOnConnection;
/// fn export(value: &CheckedFeedbackOnConnection) { serde_json::to_value(value).unwrap(); }
/// ```
pub struct CheckedFeedbackOnConnection {
    evidence: FeedbackEvidence,
    event: FeedbackEvent,
}
/// Backend-only borrowed original facts. No saved-state or audit export API.
/// Scope is only as current as the trusted caller supplies; this view is not a lease.
///
/// ```compile_fail
/// use safeflow_billing_source_reader::feedback_store::ConnectionFeedbackProjection;
/// fn export(value: &ConnectionFeedbackProjection<'_>) { serde_json::to_value(value).unwrap(); }
/// ```
/// ```compile_fail
/// use safeflow_billing_source_reader::feedback_store::ConnectionFeedbackProjection;
/// fn export(value: &ConnectionFeedbackProjection<'_>) { value.audit_candidate(); }
/// ```
pub struct ConnectionFeedbackProjection<'a> {
    evidence: &'a FeedbackEvidence,
    event: &'a FeedbackEvent,
}
/// Executes one SELECT on the supplied connection. Does not own transactions,
/// acquire a pool connection, set schema/isolation, or assert prior COMMIT.
/// Caller owns trusted source/schema, current authority and public completion checks.
pub async fn load_feedback_on_connection(
    connection: &mut PgConnection,
    scope: &TrustedScope,
    operation_id: &str,
) -> Result<Option<CheckedFeedbackOnConnection>, StoreError> {
    if !valid_id(operation_id) || !valid_id(scope.user_id()) {
        return Err(StoreError::InvalidInput);
    }
    let row = sqlx::query(READ)
        .bind(operation_id)
        .bind(scope.user_id())
        .fetch_optional(connection)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let (evidence, event) = decode(row)?;
    if !permitted(scope, &evidence) {
        return Ok(None);
    }
    validate_evidence(scope, &evidence, &event)?;
    Ok(Some(CheckedFeedbackOnConnection { evidence, event }))
}
impl CheckedFeedbackOnConnection {
    pub fn checked_projection(
        &self,
        scope: &TrustedScope,
    ) -> Result<ConnectionFeedbackProjection<'_>, StoreError> {
        validate_evidence(scope, &self.evidence, &self.event)?;
        Ok(ConnectionFeedbackProjection {
            evidence: &self.evidence,
            event: &self.event,
        })
    }
}
impl ConnectionFeedbackProjection<'_> {
    pub fn operation_id(&self) -> &str {
        &self.evidence.public_operation_id
    }
    pub fn owner_user_id(&self) -> &str {
        &self.evidence.owner_user_id
    }
    pub fn choice(&self) -> &FeedbackChoice {
        &self.evidence.choice
    }
    pub fn reference(&self) -> &SourceKey {
        &self.evidence.reference
    }
    pub fn card_id(&self) -> Option<&str> {
        self.evidence.transaction.card_id.as_deref()
    }
    pub fn report_id(&self) -> &str {
        &self.evidence.report_id
    }
    pub fn request(&self) -> &Request {
        &self.evidence.request
    }
    pub fn report_account_ids(&self) -> &[String] {
        &self.evidence.request.account_ids
    }
    pub fn snapshot_id(&self) -> &str {
        &self.evidence.snapshot_id
    }
    pub fn rule_version(&self) -> &str {
        &self.evidence.rule_version
    }
    pub fn calculated_at(&self) -> &str {
        &self.evidence.calculated_at
    }
    pub fn local_event_namespace(&self) -> &'static str {
        "billing_feedback_events_v1"
    }
    pub fn local_event_id(&self) -> i64 {
        self.event.event_id
    }
    pub fn local_event_kind(&self) -> &str {
        &self.event.event_kind
    }
    pub fn local_event_recorded_at(&self) -> &str {
        &self.event.recorded_at
    }
}
