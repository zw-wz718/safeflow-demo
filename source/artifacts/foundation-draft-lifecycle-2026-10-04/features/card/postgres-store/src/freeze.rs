//! Backend persistence only. Public owner/task/operation protection, independent
//! confirmation, strong verification, permit consumption and trusted evidence
//! must precede this callback. Rust construction is NOT evidence of authority.
//! No tool registration, public key aliases, online release or bank call here.
use crate::{valid_id, LockedCardAccount, StoreError};
use safeflow_card_facts::{
    operation_records::{
        HistoricalLockReceipt, LockOperationRecordSource, OperationHistoryQueryService,
        OriginalLockEffect, StoredLockIdentity, StoredLockOperation, StoredLockReceipt,
        StoredOperationBundle, StoredReasonCode, StoredReasonHistory,
    },
    operation_replay::LockReplayParameters,
    service::SourceUnavailable,
    LockSource,
};
use sqlx::Row;

/// Backend-generated stable identifiers, never selected from a card tail.
/// On replay these fresh candidate IDs are ignored: original IDs stay intact.
pub struct FreezeRecordIds {
    pub lock_id: String,
    pub operation_record_id: String,
    pub reason_history_id: String,
}

/// Internal command, deliberately not Deserialize and not a user/LLM DTO.
/// canonical_key is the ORIGINAL key supplied by the public operation boundary;
/// this module cannot normalize, adopt or invent a key alias. Evidence IDs and
/// scope must be resolved/revalidated by trusted backend providers in that same
/// controlled execution, not copied from conversation or feedback alone.
pub struct FreezeCommand {
    pub operation_id: String,
    pub canonical_key: String,
    pub parameters: LockReplayParameters,
    pub event_scope: Option<String>,
    pub ids: FreezeRecordIds,
}

/// Transaction-local original result. Even PersistedInCallerTransaction needs
/// caller COMMIT confirmation and a fresh fact lookup before "currently locked".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreezeSaveResult {
    pub historical_receipt: HistoricalLockReceipt,
    pub disposition: FreezeSaveDisposition,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezeSaveDisposition {
    PersistedInCallerTransaction,
    OriginalReplayed,
}

impl LockedCardAccount<'_> {
    /// Atomically insert lock (if needed), operation, reason and original result
    /// in ONE SQL statement. All errors must propagate to the PUBLIC transaction;
    /// this method never begins/commits/rolls back or retries an unknown outcome.
    pub async fn save_freeze(
        &mut self,
        command: &FreezeCommand,
    ) -> Result<FreezeSaveResult, StoreError> {
        validate_command(&self.owner, command)?;
        self.check_transaction().await?;
        self.revision().await?;
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM card_catalog_v1 WHERE card_id=$1 AND account_id=$2)",
        )
        .bind(&command.parameters.card_id)
        .bind(&self.account)
        .fetch_one(&mut *self.conn)
        .await?;
        if !owned {
            return Err(StoreError::ResourceUnavailable);
        }
        if let Some((bundle, scope)) = self.load_freeze(&command.operation_id).await? {
            check_replay(&bundle, scope.as_deref(), command)?;
            return Ok(FreezeSaveResult {
                historical_receipt: checked_receipt(&self.owner, bundle)?,
                disposition: FreezeSaveDisposition::OriginalReplayed,
            });
        }
        // Public owner protection serializes this owner across accounts. Database
        // owner/key and owner/operation uniqueness also fails closed if violated.
        let collision: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM card_freeze_operations_v1 WHERE owner_id=$1 AND (operation_id=$2 OR original_key=$3))",
        ).bind(&self.owner).bind(&command.operation_id).bind(&command.canonical_key)
            .fetch_one(&mut *self.conn).await?;
        if collision {
            return Err(StoreError::Conflict);
        }
        let p = &command.parameters;
        let matching = sqlx::query(
            "SELECT lock_id,state FROM card_locks_v1 WHERE card_id=$1 AND source=$2 AND event_scope IS NOT DISTINCT FROM $3 AND event_id IS NOT DISTINCT FROM $4 ORDER BY lock_id",
        ).bind(&p.card_id).bind(source_code(p.source)).bind(&command.event_scope)
            .bind(&p.event_id).fetch_all(&mut *self.conn).await?;
        let mut active = None;
        let mut released = false;
        for row in matching {
            match row.try_get::<&str, _>("state")? {
                "ACTIVE" if active.is_none() => active = Some(row.try_get::<String, _>("lock_id")?),
                "RELEASED" => released = true,
                _ => return Err(StoreError::InconsistentRecord),
            }
        }
        if active.is_none() && released {
            // Already selected mock semantics: no implicit reopening. Offline
            // processing/reopening protocol remains outside this increment.
            // Reviewed public maintenance45 permits only a NEW self lock after
            // its recorded organizer release. The original replay above never
            // reaches this branch and remains read-only. No generic reopening.
            let available: bool = sqlx::query_scalar(
                "SELECT to_regprocedure('card_fresh_self_after_release45_v1(text)') IS NOT NULL",
            ).fetch_one(&mut *self.conn).await?;
            let permitted = if available && p.source == LockSource::UserSelf {
                sqlx::query_scalar::<_, bool>("SELECT card_fresh_self_after_release45_v1($1)")
                    .bind(&p.card_id).fetch_one(&mut *self.conn).await?
            } else { false };
            if !permitted { return Err(StoreError::NeedsCoordination); }
        }
        let effect = if active.is_some() {
            OriginalLockEffect::ReusedActive
        } else {
            OriginalLockEffect::Created
        };
        let lock_id = active.as_deref().unwrap_or(&command.ids.lock_id);
        let expected = make_bundle(&self.owner, command, lock_id, effect);
        let receipt = checked_receipt(&self.owner, expected.clone())?;
        // One SQL function call performs ordered inserts in a single statement;
        // any constraint/trigger failure rolls back all of them and versions.
        let inserted: i64 = sqlx::query_scalar(SAVE_FREEZE_SQL)
            .bind(lock_id)
            .bind(&p.card_id)
            .bind(source_code(p.source))
            .bind(&command.event_scope)
            .bind(&p.event_id)
            .bind(effect == OriginalLockEffect::Created)
            .bind(&command.ids.operation_record_id)
            .bind(&self.owner)
            .bind(&self.account)
            .bind(&command.operation_id)
            .bind(reason_code(p.reason))
            .bind(&p.transaction_id)
            .bind(&p.feedback_id)
            .bind(&command.canonical_key)
            .bind(&command.ids.reason_history_id)
            .bind(effect_code(effect))
            .fetch_one(&mut *self.conn)
            .await?;
        if inserted != 1 {
            return Err(StoreError::InconsistentRecord);
        }
        self.check_transaction().await?;
        // Re-read actual stored associations before returning. Caller still
        // must propagate any error and commit the encompassing transaction.
        let (saved, scope) = self
            .load_freeze(&command.operation_id)
            .await?
            .ok_or(StoreError::InconsistentRecord)?;
        if saved != expected || scope != command.event_scope {
            return Err(StoreError::InconsistentRecord);
        }
        Ok(FreezeSaveResult {
            historical_receipt: receipt,
            disposition: FreezeSaveDisposition::PersistedInCallerTransaction,
        })
    }

    /// Scoped ORIGINAL result recovery; no current-state claim and zero writes.
    /// Missing is not proof an uncertain public operation never executed.
    pub async fn read_original_freeze(
        &mut self,
        operation_id: &str,
    ) -> Result<Option<HistoricalLockReceipt>, StoreError> {
        if !valid_id(operation_id) {
            return Err(StoreError::InvalidInput);
        }
        self.check_transaction().await?;
        self.load_freeze(operation_id)
            .await?
            .map(|(bundle, _)| checked_receipt(&self.owner, bundle))
            .transpose()
    }

    async fn load_freeze(
        &mut self,
        operation_id: &str,
    ) -> Result<Option<(StoredOperationBundle, Option<String>)>, StoreError> {
        load_freeze_on_connection(self.conn, &self.owner, &self.account, operation_id).await
    }
}

pub(crate) async fn load_freeze_on_connection(
    connection: &mut sqlx::PgConnection,
    owner: &str,
    account: &str,
    operation_id: &str,
) -> Result<Option<(StoredOperationBundle, Option<String>)>, StoreError> {
    let row = sqlx::query(READ_FREEZE_SQL)
        .bind(owner)
        .bind(account)
        .bind(operation_id)
        .fetch_optional(&mut *connection)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    // LEFT JOIN preserves a broken operation so it cannot become "not found".
    for name in ["joined_lock_id", "reason_history_id", "receipt_record_id"] {
        if row.try_get::<Option<String>, _>(name)?.is_none() {
            return Err(StoreError::InconsistentRecord);
        }
    }
    let source = parse_source(row.try_get("source")?)?;
    let reason = parse_reason(row.try_get("reason")?)?;
    let scope: Option<String> = row.try_get("event_scope")?;
    let event: Option<String> = row.try_get("event_id")?;
    let lock_scope: Option<String> = row.try_get("lock_event_scope")?;
    if scope != lock_scope
        || (source == LockSource::UserSelf) != scope.is_none()
        || scope.as_deref().is_some_and(|s| !valid_id(s))
    {
        return Err(StoreError::InconsistentRecord);
    }
    let record_id: String = row.try_get("record_id")?;
    let original_key: String = row.try_get("original_key")?;
    let bundle = StoredOperationBundle {
        operation: StoredLockOperation {
            record_id: record_id.clone(),
            owner_id: row.try_get("owner_id")?,
            operation_id: row.try_get("operation_id")?,
            card_id: row.try_get("card_id")?,
            source,
            reason,
            event_id: event,
            transaction_id: row.try_get("transaction_id")?,
            feedback_id: row.try_get("feedback_id")?,
            original_key: original_key.clone(),
            retry_keys: vec![original_key],
        },
        lock: StoredLockIdentity {
            id: row.try_get("joined_lock_id")?,
            card_id: row.try_get("lock_card_id")?,
            source: parse_source(row.try_get("lock_source")?)?,
            event_id: row.try_get("lock_event_id")?,
        },
        reason_history: StoredReasonHistory {
            id: row.try_get("reason_history_id")?,
            lock_id: row.try_get("reason_lock_id")?,
            operation_record_id: row.try_get("reason_record_id")?,
            reason: parse_reason(row.try_get("history_reason")?)?,
        },
        receipt: StoredLockReceipt {
            lock_id: row.try_get("receipt_lock_id")?,
            operation_record_id: row.try_get("receipt_record_id")?,
            reason_history_id: row.try_get("receipt_reason_id")?,
            source,
            original_effect: match row.try_get::<&str, _>("original_effect")? {
                "CREATED" => OriginalLockEffect::Created,
                "REUSED_ACTIVE" => OriginalLockEffect::ReusedActive,
                _ => return Err(StoreError::InconsistentRecord),
            },
        },
    };
    checked_receipt(owner, bundle.clone())?;
    Ok(Some((bundle, scope)))
}

pub(crate) fn validate_command(owner: &str, c: &FreezeCommand) -> Result<(), StoreError> {
    if !valid_id(&c.operation_id)
        || !valid_id(&c.canonical_key)
        || !valid_id(&c.ids.lock_id)
        || !valid_id(&c.ids.operation_record_id)
        || !valid_id(&c.ids.reason_history_id)
        || (c.parameters.source == LockSource::UserSelf) != c.event_scope.is_none()
        || c.event_scope.as_deref().is_some_and(|s| !valid_id(s))
    {
        return Err(StoreError::InvalidInput);
    }
    checked_receipt(
        owner,
        make_bundle(owner, c, &c.ids.lock_id, OriginalLockEffect::Created),
    )
    .map(|_| ())
    .map_err(|_| StoreError::InvalidInput)
}
fn check_replay(
    b: &StoredOperationBundle,
    scope: Option<&str>,
    c: &FreezeCommand,
) -> Result<(), StoreError> {
    check_replay_binding(
        b,
        scope,
        &c.operation_id,
        &c.canonical_key,
        &c.parameters,
        c.event_scope.as_deref(),
    )
}
pub(crate) fn check_replay_binding(
    b: &StoredOperationBundle,
    scope: Option<&str>,
    operation: &str,
    key: &str,
    p: &LockReplayParameters,
    event_scope: Option<&str>,
) -> Result<(), StoreError> {
    let o = &b.operation;
    if o.operation_id != operation
        || o.card_id != p.card_id
        || o.source != p.source
        || o.reason != p.reason
        || o.event_id != p.event_id
        || o.transaction_id != p.transaction_id
        || o.feedback_id != p.feedback_id
        || scope != event_scope
    {
        return Err(StoreError::Conflict);
    }
    if o.original_key != key {
        return Err(StoreError::RetryKeyNeedsCoordination);
    }
    Ok(())
}

fn make_bundle(
    owner: &str,
    c: &FreezeCommand,
    lock_id: &str,
    effect: OriginalLockEffect,
) -> StoredOperationBundle {
    let p = &c.parameters;
    StoredOperationBundle {
        operation: StoredLockOperation {
            record_id: c.ids.operation_record_id.clone(),
            owner_id: owner.into(),
            operation_id: c.operation_id.clone(),
            card_id: p.card_id.clone(),
            source: p.source,
            reason: p.reason,
            event_id: p.event_id.clone(),
            transaction_id: p.transaction_id.clone(),
            feedback_id: p.feedback_id.clone(),
            original_key: c.canonical_key.clone(),
            retry_keys: vec![c.canonical_key.clone()],
        },
        lock: StoredLockIdentity {
            id: lock_id.into(),
            card_id: p.card_id.clone(),
            source: p.source,
            event_id: p.event_id.clone(),
        },
        reason_history: StoredReasonHistory {
            id: c.ids.reason_history_id.clone(),
            lock_id: lock_id.into(),
            operation_record_id: c.ids.operation_record_id.clone(),
            reason: p.reason,
        },
        receipt: StoredLockReceipt {
            lock_id: lock_id.into(),
            operation_record_id: c.ids.operation_record_id.clone(),
            reason_history_id: c.ids.reason_history_id.clone(),
            source: p.source,
            original_effect: effect,
        },
    }
}
struct BundleSource(StoredOperationBundle);
impl LockOperationRecordSource for BundleSource {
    fn read_operation_record(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<StoredOperationBundle>, SourceUnavailable> {
        Ok(Some(self.0.clone()))
    }
}
pub(crate) fn checked_receipt(
    owner: &str,
    b: StoredOperationBundle,
) -> Result<HistoricalLockReceipt, StoreError> {
    let id = b.operation.operation_id.clone();
    OperationHistoryQueryService::new(BundleSource(b))
        .query(owner, Some(&id))
        .map_err(|_| StoreError::InconsistentRecord)
}
fn source_code(source: LockSource) -> &'static str {
    match source {
        LockSource::UserSelf => "USER_SELF",
        LockSource::AnomalyDispute => "ANOMALY_DISPUTE",
        LockSource::RiskControl => "RISK_CONTROL",
    }
}
fn parse_source(s: &str) -> Result<LockSource, StoreError> {
    match s {
        "USER_SELF" => Ok(LockSource::UserSelf),
        "ANOMALY_DISPUTE" => Ok(LockSource::AnomalyDispute),
        "RISK_CONTROL" => Ok(LockSource::RiskControl),
        _ => Err(StoreError::InconsistentRecord),
    }
}
fn reason_code(reason: StoredReasonCode) -> &'static str {
    match reason {
        StoredReasonCode::UserReportLost => "USER_REPORT_LOST",
        StoredReasonCode::UserSecurityConcern => "USER_SECURITY_CONCERN",
        StoredReasonCode::NonOwnerTransaction => "NON_OWNER_TRANSACTION",
        StoredReasonCode::RiskControlFlag => "RISK_CONTROL_FLAG",
    }
}
fn parse_reason(s: &str) -> Result<StoredReasonCode, StoreError> {
    match s {
        "USER_REPORT_LOST" => Ok(StoredReasonCode::UserReportLost),
        "USER_SECURITY_CONCERN" => Ok(StoredReasonCode::UserSecurityConcern),
        "NON_OWNER_TRANSACTION" => Ok(StoredReasonCode::NonOwnerTransaction),
        "RISK_CONTROL_FLAG" => Ok(StoredReasonCode::RiskControlFlag),
        _ => Err(StoreError::InconsistentRecord),
    }
}
fn effect_code(effect: OriginalLockEffect) -> &'static str {
    match effect {
        OriginalLockEffect::Created => "CREATED",
        OriginalLockEffect::ReusedActive => "REUSED_ACTIVE",
    }
}

const SAVE_FREEZE_SQL: &str =
    "SELECT card_save_freeze_v1($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)";
const READ_FREEZE_SQL: &str = r#"
SELECT o.*, l.lock_id AS joined_lock_id, l.card_id AS lock_card_id,
    l.source AS lock_source,l.event_scope AS lock_event_scope,l.event_id AS lock_event_id,
    r.reason_history_id,r.lock_id AS reason_lock_id,r.operation_record_id AS reason_record_id,
    r.reason AS history_reason,p.operation_record_id AS receipt_record_id,
    p.lock_id AS receipt_lock_id,p.reason_history_id AS receipt_reason_id,p.original_effect
FROM card_freeze_operations_v1 o
LEFT JOIN card_locks_v1 l ON l.lock_id=o.lock_id
LEFT JOIN card_freeze_reasons_v1 r ON r.operation_record_id=o.record_id
LEFT JOIN card_freeze_receipts_v1 p ON p.operation_record_id=o.record_id
WHERE o.owner_id=$1 AND o.account_id=$2 AND o.operation_id=$3
"#;

#[cfg(test)]
mod tests {
    use super::*;
    fn command() -> FreezeCommand {
        FreezeCommand {
            operation_id: "OP1".into(),
            canonical_key: "KEY1".into(),
            event_scope: None,
            ids: FreezeRecordIds {
                lock_id: "L1".into(),
                operation_record_id: "R1".into(),
                reason_history_id: "H1".into(),
            },
            parameters: LockReplayParameters {
                card_id: "K".into(),
                source: LockSource::UserSelf,
                reason: StoredReasonCode::UserReportLost,
                event_id: None,
                transaction_id: None,
                feedback_id: None,
            },
        }
    }
    #[test]
    fn source_reason_and_evidence_shapes_use_existing_business_rules() {
        let mut c = command();
        assert!(validate_command("U1", &c).is_ok());
        c.parameters.source = LockSource::AnomalyDispute;
        c.parameters.reason = StoredReasonCode::NonOwnerTransaction;
        c.event_scope = Some("billing-feedback-v1".into());
        c.parameters.event_id = Some("E1".into());
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
        c.parameters.transaction_id = Some("T1".into());
        c.parameters.feedback_id = Some("F1".into());
        assert!(validate_command("U1", &c).is_ok());
        c.parameters.source = LockSource::RiskControl;
        c.parameters.reason = StoredReasonCode::RiskControlFlag;
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
        c.parameters.transaction_id = None;
        c.parameters.feedback_id = None;
        assert!(validate_command("U1", &c).is_ok());
        c.parameters.reason = StoredReasonCode::UserReportLost;
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
    }
    #[test]
    fn scope_is_required_for_event_and_forbidden_for_self() {
        let mut c = command();
        c.event_scope = Some("risk-v1".into());
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
        c.parameters.source = LockSource::RiskControl;
        c.parameters.reason = StoredReasonCode::RiskControlFlag;
        c.parameters.event_id = Some("E1".into());
        c.event_scope = None;
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
        c.event_scope = Some("  ".into());
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
    }
    #[test]
    fn exact_replay_keeps_original_ids_instead_of_fresh_candidates() {
        let mut c = command();
        let b = make_bundle("U1", &c, "L1", OriginalLockEffect::Created);
        c.ids.lock_id = "fresh-L".into();
        c.ids.operation_record_id = "fresh-R".into();
        c.ids.reason_history_id = "fresh-H".into();
        assert!(check_replay(&b, None, &c).is_ok());
        let r = checked_receipt("U1", b).unwrap();
        assert_eq!(
            (
                r.lock_id.as_str(),
                r.operation_record_id.as_str(),
                r.reason_history_id.as_str()
            ),
            ("L1", "R1", "H1")
        );
    }
    #[test]
    fn changed_parameters_conflict_before_unknown_key() {
        let mut c = command();
        let b = make_bundle("U1", &c, "L1", OriginalLockEffect::Created);
        c.canonical_key = "unknown-alias".into();
        assert!(matches!(
            check_replay(&b, None, &c),
            Err(StoreError::RetryKeyNeedsCoordination)
        ));
        c.parameters.reason = StoredReasonCode::UserSecurityConcern;
        assert!(matches!(
            check_replay(&b, None, &c),
            Err(StoreError::Conflict)
        ));
    }
    #[test]
    fn event_namespace_cannot_change_during_replay() {
        let mut c = command();
        c.parameters.source = LockSource::RiskControl;
        c.parameters.reason = StoredReasonCode::RiskControlFlag;
        c.parameters.event_id = Some("E1".into());
        c.event_scope = Some("risk-v1".into());
        let b = make_bundle("U1", &c, "L1", OriginalLockEffect::Created);
        c.event_scope = Some("other-v1".into());
        assert!(matches!(
            check_replay(&b, Some("risk-v1"), &c),
            Err(StoreError::Conflict)
        ));
    }
    #[test]
    fn historical_projection_hides_keys_and_evidence_and_rejects_broken_links() {
        let c = command();
        let mut b = make_bundle("U1", &c, "L1", OriginalLockEffect::ReusedActive);
        let receipt = checked_receipt("U1", b.clone()).unwrap();
        let printed = format!("{receipt:?}");
        assert!(!printed.contains("KEY1"));
        assert_eq!(receipt.original_effect, OriginalLockEffect::ReusedActive);
        b.receipt.lock_id = "wrong".into();
        assert!(matches!(
            checked_receipt("U1", b),
            Err(StoreError::InconsistentRecord)
        ));
    }
    #[test]
    fn blank_ids_and_foreign_history_are_rejected() {
        let mut c = command();
        c.ids.reason_history_id = " ".into();
        assert!(matches!(
            validate_command("U1", &c),
            Err(StoreError::InvalidInput)
        ));
        let c = command();
        let b = make_bundle("U2", &c, "L1", OriginalLockEffect::Created);
        assert!(matches!(
            checked_receipt("U1", b),
            Err(StoreError::InconsistentRecord)
        ));
    }
}
