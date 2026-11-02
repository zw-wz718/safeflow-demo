//! Reliable handoff of internal public evidence; HTTP adaptation lives in audit_sender.
use crate::models::{hash, valid_id};
use crate::{PgRepository, StorageError};
use chrono::{DateTime, Utc};
use safeflow_contracts::{ArgumentsHash, EventId};
use serde_json::Value;
use sqlx::Row;

#[derive(Clone, Debug, PartialEq)]
pub struct PendingPublicAudit {
    pub event_id: EventId,
    pub envelope: Value,
    pub digest: ArgumentsHash,
    pub historical: bool,
    pub enqueued_at: DateTime<Utc>,
}

/// Privileged backend worker only. Never expose cross-owner evidence to HTTP/LLM.
#[derive(Clone)]
pub struct PublicAuditOutbox {
    repository: PgRepository,
}

impl PublicAuditOutbox {
    pub fn for_trusted_backend(repository: PgRepository) -> Self {
        Self { repository }
    }

    /// An unacknowledged event is returned unchanged after failure or restart.
    /// Concurrent workers may receive the same ID; receivers must deduplicate it.
    pub async fn pending(&self, limit: u16) -> Result<Vec<PendingPublicAudit>, StorageError> {
        if !(1..=1000).contains(&limit) {
            return Err(StorageError::Invalid("outbox batch limit"));
        }
        let rows = sqlx::query("SELECT * FROM public_audit_outbox WHERE acknowledged_at IS NULL ORDER BY sequence LIMIT $1")
            .bind(i64::from(limit)).fetch_all(&self.repository.pool).await?;
        rows.iter()
            .map(|row| {
                let envelope: Value = row.try_get("envelope")?;
                crate::subscription_local_audit::check_envelope(&envelope)?;
                Ok(PendingPublicAudit {
                    event_id: crate::repository::decode(row, "event_id")?,
                    digest: hash(&envelope)?,
                    envelope,
                    historical: row.try_get("historical")?,
                    enqueued_at: row.try_get("enqueued_at")?,
                })
            })
            .collect()
    }

    /// The trusted receiver adapter must verify its ACK before calling this.
    /// This stores local handoff evidence; it does not interpret external ACK DTOs.
    /// Duplicate identical ACKs are idempotent; mismatched payload/receipt refuses.
    pub async fn acknowledge(
        &self,
        event: &EventId,
        expected_digest: &ArgumentsHash,
        receiver: &str,
        receipt_ref: &str,
    ) -> Result<(), StorageError> {
        for id in [event.as_str(), receiver, receipt_ref] {
            valid_id(id)?;
        }
        let mut tx = self.repository.pool.begin().await?;
        let row = sqlx::query("SELECT envelope,receiver,receipt_ref FROM public_audit_outbox WHERE event_id=$1 FOR UPDATE")
            .bind(event.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
        let envelope: Value = row.try_get("envelope")?;
        if hash(&envelope)? != *expected_digest {
            return Err(StorageError::ExecutionKeyConflict);
        }
        match (
            row.try_get::<Option<String>, _>("receiver")?,
            row.try_get::<Option<String>, _>("receipt_ref")?,
        ) {
            (Some(r), Some(a)) if r == receiver && a == receipt_ref => {}
            (None, None) => {
                sqlx::query("UPDATE public_audit_outbox SET receiver=$2,receipt_ref=$3,acknowledged_at=clock_timestamp() WHERE event_id=$1 AND acknowledged_at IS NULL")
                    .bind(event.as_str()).bind(receiver).bind(receipt_ref).execute(&mut *tx).await?;
            }
            _ => return Err(StorageError::ExecutionKeyConflict),
        }
        tx.commit().await?;
        Ok(())
    }
}
