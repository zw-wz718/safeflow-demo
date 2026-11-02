use crate::confirmation::{timestamp, ConfirmationAccess, ConfirmationSession};
use crate::lifecycle::binding_for_mode;
use crate::models::{db_version, hash, valid_id};
use crate::repository::{audit, decode, encode, lock_task_for_mode, operations, version};
use crate::{
    ConfirmationBinding, ConfirmationClock, ConfirmationConfig, ConfirmationRecord,
    ConfirmationStatus, PersistentAuditKind, PgRepository, StorageError,
    SERVER_CONFIRMATION_CONTRACT_VERSION,
};
use safeflow_contracts::{ConfirmationRef, OperationId, TaskId};
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::Row;

type Guard<'a> = &'a (dyn Fn() -> Result<(), StorageError> + Send + Sync);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredConfirmation {
    pub(crate) record: ConfirmationRecord,
    pub(crate) session_digest: String,
    pub(crate) session_binding_version: u64,
}

impl StoredConfirmation {
    pub(crate) fn check_session(&self, session: &ConfirmationSession) -> Result<(), StorageError> {
        if self.record.binding.user_id != session.binding.user_id
            || self.session_digest != session.digest.as_str()
            || self.session_binding_version != session.binding.binding_version
        {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }
}

impl PgRepository {
    pub(crate) async fn prepare_confirmation(
        &self,
        session: &ConfirmationSession,
        task_id: &TaskId,
        operation_id: &OperationId,
        config: &ConfirmationConfig,
        clock: &dyn ConfirmationClock,
        guard: Guard<'_>,
        lifecycle: bool,
        bounds: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<ConfirmationRecord, StorageError> {
        valid_id(task_id.as_str())?;
        valid_id(operation_id.as_str())?;
        let mut tx = self.pool.begin().await?;
        // The same task-first lock is used by all persistence state changes.
        let task =
            lock_task_for_mode(&mut tx, &session.binding.user_id, task_id, lifecycle).await?;
        guard()?;
        let ops = operations(&mut tx, task_id).await?;
        let binding = binding_for_mode(&mut tx, &task, &ops, operation_id, lifecycle).await?;
        // Every public-confirmation entry point observes the immutable domain
        // cutoff, including a trusted generic prepare after a lost response.
        let bounds = if binding.tool_name.as_str() == "subscription_local_change" {
            let row = sqlx::query("SELECT member_binding,public_parameters,session_digest,binding_version FROM subscription_public_bindings_v1 WHERE operation_id=$1 AND owner_id=$2")
                .bind(operation_id.as_str()).bind(session.binding.user_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
            if row.try_get::<String, _>("session_digest")? != session.digest.as_str()
                || row.try_get::<i64, _>("binding_version")?
                    != db_version(session.binding.binding_version)?
            {
                return Err(StorageError::NotFound);
            }
            let member: serde_json::Value = row.try_get("member_binding")?;
            let input: serde_json::Value = row.try_get("public_parameters")?;
            if hash(&input)? != binding.arguments_hash
                || serde_json::json!(hash(&member)?) != input["member_digest"]
                || input["expires_at"] != member["expires_at"]
            {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            let original = crate::permit::subscription_deadline(&input, &member)?;
            if original.1 <= original.0 || bounds.is_some_and(|v| v != original) {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            Some(original)
        } else {
            bounds
        };
        if binding.tool_name.as_str() == "card_freeze_isolated" { return Err(StorageError::FormalAuthUnavailable); }
        let digest = hash(&binding)?;
        if let Some((prepared, expires)) = bounds {
            let now = timestamp(clock)?;
            if now < prepared {
                return Err(StorageError::Invalid("server clock moved backwards"));
            }
            if now >= expires {
                return Err(StorageError::ConfirmationExpired);
            }
        }
        let row = sqlx::query("SELECT * FROM server_confirmations WHERE task_id=$1 AND operation_id=$2 AND session_digest=$3 AND session_binding_version=$4 AND binding_digest=$5 FOR UPDATE")
            .bind(task_id.as_str()).bind(operation_id.as_str()).bind(session.digest.as_str())
            .bind(db_version(session.binding.binding_version)?).bind(digest.as_str())
            .fetch_optional(&mut *tx).await?;
        if let Some(row) = row {
            let stored = confirmation_row(&row)?;
            stored.check_session(session)?;
            if stored.record.binding != binding {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            if bounds.is_some_and(|(_, expires)| stored.record.expires_at > expires) {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            check_active(&stored.record, clock)?;
            guard()?;
            tx.commit().await?;
            return Ok(stored.record);
        }
        let now = timestamp(clock)?;
        let expires_at = now
            .checked_add_signed(config.ttl)
            .ok_or(StorageError::Invalid("confirmation expiry out of range"))?;
        let expires_at = bounds.map_or(expires_at, |(_, cap)| expires_at.min(cap));
        // PostgreSQL timestamptz stores microseconds. Round down before both
        // insertion AND response so concurrent/recovered preparations are exact.
        let expires_at = chrono::DateTime::from_timestamp_micros(expires_at.timestamp_micros())
            .ok_or(StorageError::ConfirmationExpired)?;
        let record = ConfirmationRecord {
            contract_version: SERVER_CONFIRMATION_CONTRACT_VERSION,
            confirmation_ref: ConfirmationRef::parse(format!(
                "confirmation_{}",
                uuid::Uuid::new_v4()
            ))
            .expect("UUID has content"),
            binding,
            status: ConfirmationStatus::Prepared,
            prepared_at: now,
            expires_at,
            confirmed_at: None,
            revoked_at: None,
        };
        guard()?;
        insert_prepared_confirmation(&mut tx, session, &record).await?;
        audit(
            &mut tx,
            &task,
            ops.iter().find(|op| &op.operation_id == operation_id),
            PersistentAuditKind::ConfirmationPrepared,
            confirmation_details(&record)?,
        )
        .await?;
        guard()?;
        check_active(&record, clock)?;
        tx.commit().await?;
        Ok(record)
    }

    pub(crate) async fn access_confirmation(
        &self,
        session: &ConfirmationSession,
        task_id: &TaskId,
        reference: &ConfirmationRef,
        action: ConfirmationAccess,
        expected: Option<&ConfirmationBinding>,
        clock: &dyn ConfirmationClock,
        guard: Guard<'_>,
        lifecycle: bool,
    ) -> Result<ConfirmationRecord, StorageError> {
        valid_id(task_id.as_str())?;
        valid_id(reference.as_str())?;
        let mut tx = self.pool.begin().await?;
        let result = self.access_confirmation_on_connection(&mut tx, session, task_id, reference, action, expected, clock, guard, lifecycle).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub(crate) async fn access_confirmation_on_connection(
        &self, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session: &ConfirmationSession, task_id: &TaskId, reference: &ConfirmationRef,
        action: ConfirmationAccess, expected: Option<&ConfirmationBinding>,
        clock: &dyn ConfirmationClock, guard: Guard<'_>, lifecycle: bool,
    ) -> Result<ConfirmationRecord, StorageError> {
        let task = lock_task_for_mode(tx, &session.binding.user_id, task_id, lifecycle).await?;
        guard()?;
        let row = sqlx::query("SELECT * FROM server_confirmations WHERE task_id=$1 AND confirmation_ref=$2 FOR UPDATE")
            .bind(task_id.as_str()).bind(reference.as_str()).fetch_optional(&mut **tx).await?
            .ok_or(StorageError::NotFound)?;
        let stored = confirmation_row(&row)?;
        stored.check_session(session)?;
        let mut record = stored.record;
        if record.binding.tool_name.as_str() == "card_freeze_isolated" { return Err(StorageError::FormalAuthUnavailable); }
        let ops = operations(tx, task_id).await?;
        let mut kind = None;
        if matches!(action, ConfirmationAccess::RejectPrepared) {
            if record.status == ConfirmationStatus::Confirmed {
                return Err(StorageError::DecisionConflict);
            }
            if expected.is_some_and(|e| e != &record.binding)
                || binding_for_mode(
                    tx,
                    &task,
                    &ops,
                    &record.binding.operation_id,
                    lifecycle,
                )
                .await?
                    != record.binding
            {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            if record.status != ConfirmationStatus::Revoked {
                check_active(&record, clock)?;
            }
        }
        if matches!(
            action,
            ConfirmationAccess::Revoke | ConfirmationAccess::RejectPrepared
        ) {
            if record.status != ConfirmationStatus::Revoked {
                record.status = ConfirmationStatus::Revoked;
                let now = timestamp(clock)?;
                if now < record.prepared_at
                    || record.confirmed_at.is_some_and(|confirmed| now < confirmed)
                {
                    return Err(StorageError::Invalid("server clock moved backwards"));
                }
                record.revoked_at = Some(now);
                kind = Some(PersistentAuditKind::ConfirmationRevoked);
            }
        } else {
            check_active(&record, clock)?;
            if expected.is_some_and(|e| e != &record.binding) {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            if binding_for_mode(
                tx,
                &task,
                &ops,
                &record.binding.operation_id,
                lifecycle,
            )
            .await?
                != record.binding
            {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
            match action {
                ConfirmationAccess::Confirm if record.status == ConfirmationStatus::Prepared => {
                    record.status = ConfirmationStatus::Confirmed;
                    record.confirmed_at = Some(timestamp(clock)?);
                    kind = Some(PersistentAuditKind::ExplicitConfirmationRecorded);
                }
                ConfirmationAccess::Verify if record.status != ConfirmationStatus::Confirmed => {
                    return Err(StorageError::ConfirmationRequired);
                }
                _ => {}
            }
        }
        guard()?;
        if let Some(kind) = kind {
            sqlx::query("UPDATE server_confirmations SET status=$1, confirmed_at=$2, revoked_at=$3 WHERE confirmation_ref=$4 AND task_id=$5")
                .bind(encode(record.status)?).bind(record.confirmed_at).bind(record.revoked_at)
                .bind(reference.as_str()).bind(task_id.as_str()).execute(&mut **tx).await?;
            audit(
                tx,
                &task,
                ops.iter()
                    .find(|op| op.operation_id == record.binding.operation_id),
                kind,
                confirmation_details(&record)?,
            )
            .await?;
        }
        guard()?;
        if !matches!(
            action,
            ConfirmationAccess::Revoke | ConfirmationAccess::RejectPrepared
        ) {
            check_active(&record, clock)?;
        }
        Ok(record)
    }
}

pub(crate) fn check_active(
    record: &ConfirmationRecord,
    clock: &dyn ConfirmationClock,
) -> Result<(), StorageError> {
    if record.status == ConfirmationStatus::Revoked {
        return Err(StorageError::ConfirmationRevoked);
    }
    check_active_at(record, timestamp(clock)?)
}

pub(crate) fn check_active_at(
    record: &ConfirmationRecord,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StorageError> {
    if record.status == ConfirmationStatus::Revoked {
        return Err(StorageError::ConfirmationRevoked);
    }
    if now >= record.expires_at {
        return Err(StorageError::ConfirmationExpired);
    }
    if now < record.prepared_at || record.confirmed_at.is_some_and(|confirmed| now < confirmed) {
        return Err(StorageError::Invalid("server clock moved backwards"));
    }
    Ok(())
}

pub(crate) fn confirmation_details(
    record: &ConfirmationRecord,
) -> Result<serde_json::Value, StorageError> {
    Ok(
        json!({"confirmation_ref":record.confirmation_ref, "contract_version":record.contract_version,
        "binding_digest":hash(&record.binding)?, "status":record.status, "expires_at":record.expires_at}),
    )
}

// Shared connection-level write: callers retain source locks and journal in the
// same transaction. This helper does not validate sources or grant permission.
pub(crate) async fn insert_prepared_confirmation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: &ConfirmationSession,
    record: &ConfirmationRecord,
) -> Result<(), StorageError> {
    let b = &record.binding;
    sqlx::query("INSERT INTO server_confirmations (confirmation_ref,contract_version,owner_id,task_id,operation_id,node_id,tool_name,session_digest,session_binding_version,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,status,prepared_at,expires_at,lifecycle_anchor) VALUES ($1,1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'PREPARED',$15,$16,$17)")
        .bind(record.confirmation_ref.as_str()).bind(b.user_id.as_str()).bind(b.task_id.as_str())
        .bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str())
        .bind(session.digest.as_str()).bind(db_version(session.binding.binding_version)?)
        .bind(db_version(b.task_revision)?).bind(b.plan_version as i64)
        .bind(db_version(b.operation_revision)?).bind(db_version(b.facts_version)?)
        .bind(b.arguments_hash.as_str()).bind(hash(b)?.as_str()).bind(record.prepared_at).bind(record.expires_at)
        .bind(b.lifecycle_anchor.as_ref().map(|value| value.as_str()))
        .execute(&mut **tx).await?;
    Ok(())
}

pub(crate) fn confirmation_row(row: &PgRow) -> Result<StoredConfirmation, StorageError> {
    if row.try_get::<i16, _>("contract_version")? != SERVER_CONFIRMATION_CONTRACT_VERSION as i16 {
        return Err(StorageError::CorruptRecord);
    }
    let binding = binding_row(row)?;
    if hash(&binding)?.as_str() != row.try_get::<String, _>("binding_digest")? {
        return Err(StorageError::CorruptRecord);
    }
    Ok(StoredConfirmation {
        record: ConfirmationRecord {
            contract_version: SERVER_CONFIRMATION_CONTRACT_VERSION,
            confirmation_ref: decode(row, "confirmation_ref")?,
            binding,
            status: decode(row, "status")?,
            prepared_at: row.try_get("prepared_at")?,
            expires_at: row.try_get("expires_at")?,
            confirmed_at: row.try_get("confirmed_at")?,
            revoked_at: row.try_get("revoked_at")?,
        },
        session_digest: row.try_get("session_digest")?,
        session_binding_version: version(row, "session_binding_version")?,
    })
}

pub(crate) fn binding_row(row: &PgRow) -> Result<ConfirmationBinding, StorageError> {
    decode_binding_row(row, true)
}

// Permit and isolated auth rows retain their existing schema without lifecycle anchors.
pub(crate) fn legacy_binding_row(row: &PgRow) -> Result<ConfirmationBinding, StorageError> {
    decode_binding_row(row, false)
}

fn decode_binding_row(
    row: &PgRow,
    has_lifecycle_column: bool,
) -> Result<ConfirmationBinding, StorageError> {
    Ok(ConfirmationBinding {
        user_id: decode(row, "owner_id")?,
        task_id: decode(row, "task_id")?,
        operation_id: decode(row, "operation_id")?,
        node_id: decode(row, "node_id")?,
        tool_name: decode(row, "tool_name")?,
        task_revision: version(row, "task_revision")?,
        plan_version: u32::try_from(version(row, "plan_version")?)
            .map_err(|_| StorageError::CorruptRecord)?,
        operation_revision: version(row, "operation_revision")?,
        facts_version: version(row, "facts_version")?,
        arguments_hash: decode(row, "arguments_hash")?,
        lifecycle_anchor: if has_lifecycle_column {
            row.try_get::<Option<String>, _>("lifecycle_anchor")?
                .map(safeflow_contracts::ArgumentsHash::parse)
                .transpose()
                .map_err(|_| StorageError::CorruptRecord)?
        } else {
            None
        },
    })
}

pub(crate) async fn verify_stored_confirmation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    reference: &ConfirmationRef,
    binding: &ConfirmationBinding,
    session_digest: &str,
    session_version: u64,
    clock: &dyn ConfirmationClock,
) -> Result<chrono::DateTime<chrono::Utc>, StorageError> {
    let stored = read_stored_confirmation(tx, &binding.task_id, reference).await?;
    if stored.record.binding != *binding
        || stored.session_digest != session_digest
        || stored.session_binding_version != session_version
    {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    check_active(&stored.record, clock)?;
    if stored.record.status != ConfirmationStatus::Confirmed {
        return Err(StorageError::ConfirmationRequired);
    }
    Ok(stored.record.expires_at)
}

pub(crate) async fn read_stored_confirmation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: &TaskId,
    reference: &ConfirmationRef,
) -> Result<StoredConfirmation, StorageError> {
    let row = sqlx::query(
        "SELECT * FROM server_confirmations WHERE task_id=$1 AND confirmation_ref=$2 FOR UPDATE",
    )
    .bind(task.as_str())
    .bind(reference.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::NotFound)?;
    confirmation_row(&row)
}

pub(crate) async fn read_historical_confirmation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: &TaskId,
    reference: &ConfirmationRef,
) -> Result<StoredConfirmation, StorageError> {
    let row =
        sqlx::query("SELECT * FROM server_confirmations WHERE task_id=$1 AND confirmation_ref=$2")
            .bind(task.as_str())
            .bind(reference.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::CorruptRecord)?;
    confirmation_row(&row)
}
