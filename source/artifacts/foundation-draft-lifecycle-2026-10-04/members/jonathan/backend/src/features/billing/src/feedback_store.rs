//! Borrowed-connection storage, NOT an authenticated public write route.
//! Public executor owns permit/session rechecks, BEGIN/COMMIT, result and Outbox.
//! Cancellation requires the caller to roll back, never commit the transaction.
use crate::{
    feedback::{FeedbackEvidence, PreparedFeedback},
    TrustedScope,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, types::Json, PgConnection, PgPool, Row};
mod connection;
mod projection;
pub use connection::{
    load_feedback_on_connection, CheckedFeedbackOnConnection, ConnectionFeedbackProjection,
};
pub use projection::{BillingAuditCandidate, CardDisposition, FeedbackProjection};

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Database,
    TransactionRequired,
    OperationConflict,
    RepeatedFeedbackRuleUnconfirmed,
    ReportConflict,
    ResourceUnavailable,
    InvalidInput,
    CorruptRecord,
    OutcomeUnknown,
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for StoreError {}
impl From<sqlx::Error> for StoreError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}
#[derive(Debug, Serialize)]
pub struct FeedbackEvent {
    event_id: i64,
    event_kind: String,
    recorded_at: String,
}
#[derive(Debug, Serialize)]
pub struct StagedFeedback {
    state: &'static str,
    public_operation_id: String,
    replayed: bool,
    /// Same-transaction event; not committed, delivered or acknowledged.
    event: FeedbackEvent,
}
#[derive(Debug, Serialize)]
pub struct SavedFeedback {
    state: &'static str,
    persisted: bool,
    lock_authorized: bool,
    card_handoff: &'static str,
    evidence: FeedbackEvidence,
    event: FeedbackEvent,
}
fn permitted(scope: &TrustedScope, data: &FeedbackEvidence) -> bool {
    data.owner_user_id == scope.user_id()
        && data
            .request
            .account_ids
            .iter()
            .all(|a| scope.contains_account(a))
}
fn valid_id(id: &str) -> bool {
    id.len() <= 200 && crate::validate_reference(id).is_ok()
}
fn header(data: &FeedbackEvidence) -> Value {
    json!({"owner_user_id":data.owner_user_id,"report_id":data.report_id,"request":data.request,
        "source_id":data.reference.source_id,"snapshot_id":data.snapshot_id,
        "rule_version":data.rule_version,"calculated_at":data.calculated_at})
}
const READ: &str = r#"SELECT f.public_operation_id, f.owner_user_id, f.report_id, f.source_id,
    f.account_id, f.transaction_id, f.payload, h.header, e.event_id, e.event_kind,
    to_char(e.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS recorded_at
    FROM billing_feedback_v1 f
    LEFT JOIN billing_report_bindings_v1 h ON h.owner_user_id=f.owner_user_id AND h.report_id=f.report_id
    LEFT JOIN billing_feedback_events_v1 e ON e.public_operation_id=f.public_operation_id
    WHERE f.public_operation_id=$1 AND ($2::text IS NULL OR f.owner_user_id=$2)"#;
fn decode(row: PgRow) -> Result<(FeedbackEvidence, FeedbackEvent), StoreError> {
    let Json(data): Json<FeedbackEvidence> = row
        .try_get("payload")
        .map_err(|_| StoreError::CorruptRecord)?;
    for (column, expected) in [
        ("public_operation_id", &data.public_operation_id),
        ("owner_user_id", &data.owner_user_id),
        ("report_id", &data.report_id),
        ("source_id", &data.reference.source_id),
        ("account_id", &data.reference.account_id),
        ("transaction_id", &data.reference.transaction_id),
    ] {
        if row
            .try_get::<String, _>(column)
            .map_err(|_| StoreError::CorruptRecord)?
            != *expected
        {
            return Err(StoreError::CorruptRecord);
        }
    }
    let Json(bound): Json<Value> = row
        .try_get("header")
        .map_err(|_| StoreError::CorruptRecord)?;
    if bound != header(&data)
        || data.transaction.account_id != data.reference.account_id
        || data.transaction.transaction_id != data.reference.transaction_id
    {
        return Err(StoreError::CorruptRecord);
    }
    let event = FeedbackEvent {
        event_id: row
            .try_get("event_id")
            .map_err(|_| StoreError::CorruptRecord)?,
        event_kind: row
            .try_get("event_kind")
            .map_err(|_| StoreError::CorruptRecord)?,
        recorded_at: row
            .try_get("recorded_at")
            .map_err(|_| StoreError::CorruptRecord)?,
    };
    if event.event_kind != "BILLING_FEEDBACK_RECORDED_LOCAL_V1" || event.event_id <= 0 {
        return Err(StoreError::CorruptRecord);
    }
    Ok((data, event))
}
async fn original(
    conn: &mut PgConnection,
    op: &str,
) -> Result<Option<(FeedbackEvidence, FeedbackEvent)>, StoreError> {
    sqlx::query(READ)
        .bind(op)
        .bind(Option::<&str>::None)
        .fetch_optional(conn)
        .await?
        .map(decode)
        .transpose()
}
fn staged(data: &FeedbackEvidence, event: FeedbackEvent, replayed: bool) -> StagedFeedback {
    StagedFeedback {
        state: "STAGED_AWAITING_PUBLIC_COMMIT",
        public_operation_id: data.public_operation_id.clone(),
        replayed,
        event,
    }
}
fn replay(
    data: &FeedbackEvidence,
    stored: (FeedbackEvidence, FeedbackEvent),
) -> Result<StagedFeedback, StoreError> {
    if data != &stored.0 {
        return Err(StoreError::OperationConflict);
    }
    Ok(staged(data, stored.1, true))
}

/// Requires ACTIVE outer transaction and original prepared binding. Caller owns
/// COMMIT and MUST roll back on cancellation/OutcomeUnknown. Scope is not auth.
pub async fn stage_feedback(
    conn: &mut PgConnection,
    scope: &TrustedScope,
    prepared: &PreparedFeedback,
) -> Result<StagedFeedback, StoreError> {
    let data = &prepared.evidence;
    if !permitted(scope, data) {
        return Err(StoreError::ResourceUnavailable);
    }
    if [
        &data.public_operation_id,
        &data.owner_user_id,
        &data.report_id,
        &data.reference.source_id,
        &data.reference.account_id,
        &data.reference.transaction_id,
    ]
    .iter()
    .any(|id| !valid_id(id))
        || serde_json::to_vec(data)
            .map_err(|_| StoreError::InvalidInput)?
            .len()
            > 65536
    {
        return Err(StoreError::InvalidInput);
    }
    // PostgreSQL rejects SAVEPOINT outside BEGIN. This private savepoint never
    // takes ownership of the caller's transaction or commits it.
    sqlx::query("SAVEPOINT billing_feedback_stage_v1")
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("25P01") {
                StoreError::TransactionRequired
            } else {
                StoreError::Database
            }
        })?;
    let result = stage_inside(conn, data).await;
    if result.is_err() {
        sqlx::query("ROLLBACK TO SAVEPOINT billing_feedback_stage_v1")
            .execute(&mut *conn)
            .await
            .map_err(|_| StoreError::OutcomeUnknown)?;
    }
    sqlx::query("RELEASE SAVEPOINT billing_feedback_stage_v1")
        .execute(conn)
        .await
        .map_err(|_| StoreError::OutcomeUnknown)?;
    result
}
async fn stage_inside(
    conn: &mut PgConnection,
    data: &FeedbackEvidence,
) -> Result<StagedFeedback, StoreError> {
    if let Some(stored) = original(conn, &data.public_operation_id).await? {
        return replay(data, stored);
    }
    let bound = header(data);
    sqlx::query("INSERT INTO billing_report_bindings_v1(owner_user_id,report_id,header) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(&data.owner_user_id).bind(&data.report_id).bind(Json(&bound)).execute(&mut *conn).await?;
    let Json(existing): Json<Value> = sqlx::query_scalar(
        "SELECT header FROM billing_report_bindings_v1 WHERE owner_user_id=$1 AND report_id=$2",
    )
    .bind(&data.owner_user_id)
    .bind(&data.report_id)
    .fetch_one(&mut *conn)
    .await?;
    if existing != bound {
        return Err(StoreError::ReportConflict);
    }
    let inserted=sqlx::query("INSERT INTO billing_feedback_v1(public_operation_id,owner_user_id,report_id,source_id,account_id,transaction_id,payload) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
        .bind(&data.public_operation_id).bind(&data.owner_user_id).bind(&data.report_id)
        .bind(&data.reference.source_id).bind(&data.reference.account_id).bind(&data.reference.transaction_id)
        .bind(Json(data)).execute(&mut *conn).await?.rows_affected();
    if inserted == 0 {
        return match original(conn, &data.public_operation_id).await? {
            Some(stored) => replay(data, stored),
            None => Err(StoreError::RepeatedFeedbackRuleUnconfirmed),
        };
    }
    sqlx::query("INSERT INTO billing_feedback_events_v1(public_operation_id,event_kind) VALUES ($1,'BILLING_FEEDBACK_RECORDED_LOCAL_V1')")
        .bind(&data.public_operation_id).execute(&mut *conn).await?;
    let (_, event) = original(conn, &data.public_operation_id)
        .await?
        .ok_or(StoreError::CorruptRecord)?;
    Ok(staged(data, event, false))
}

/// Independent committed read on a pool, not on the writer's connection. Errors
/// remain errors; unreadable owner/accounts return None. Public caller must
/// recheck current session before returning. Pool targets trusted primary/schema.
pub async fn load_feedback(
    pool: &PgPool,
    scope: &TrustedScope,
    operation_id: &str,
) -> Result<Option<SavedFeedback>, StoreError> {
    if !valid_id(operation_id) || !valid_id(scope.user_id()) {
        return Err(StoreError::InvalidInput);
    }
    let row = sqlx::query(READ)
        .bind(operation_id)
        .bind(scope.user_id())
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let (evidence, event) = decode(row)?;
    if !permitted(scope, &evidence) {
        return Ok(None);
    }
    let card_handoff = if evidence.transaction.card_id.is_none() {
        "BLOCKED_NO_CARD"
    } else {
        "BLOCKED_CARD_ASSOCIATION_UNVERIFIED"
    };
    Ok(Some(SavedFeedback {
        state: "SAVED",
        persisted: true,
        lock_authorized: false,
        card_handoff,
        evidence,
        event,
    }))
}
