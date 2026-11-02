//! Trusted SQL binding of member facts to immutable public first submissions.
//! A statistic is not an authorization decision or an execution permit.
use crate::{ExecutionRequestStore, StorageError};
use safeflow_contracts::{ResourceId, SessionRef, UserId};
use safeflow_transfer_integration::{
    daily_usage::{calculate_daily_usage, DailyUsage, DailyUsageInput, SubmittedTransferFact},
    funds_store::FundsStore,
    mock_execution::MockTransferRequest,
};
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct DailyUsageSnapshot {
    pub owner_id: UserId,
    pub payer_accounts: Vec<ResourceId>,
    pub as_of: String,
    pub submitted: Vec<SubmittedTransferFact>,
    pub usage: DailyUsage,
}

// Always before task/member/account locks, held until COMMIT/ROLLBACK. READ
// COMMITTED deliberately takes a fresh snapshot AFTER an advisory-lock wait.
pub(crate) async fn protect_owner(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
) -> Result<(), StorageError> {
    crate::permit::billing_installation_lock(tx, false).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || $1,0))")
        .bind(serde_json::json!(["public-owner-execution-v1", owner]).to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn protect_operation(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    operation: &str,
) -> Result<(), StorageError> {
    // Exact existing member key: also excludes its later resolve/failure-release
    // operation, including callers using the received FundsStore directly.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(serde_json::json!(["transfer-business-operation-v1", owner, operation]).to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) fn funding_shape(arguments: &serde_json::Value) -> bool {
    arguments.get("payer_account_id").is_some()
        || serde_json::from_value::<MockTransferRequest>(arguments.clone()).is_ok()
}

pub(crate) async fn read_locked(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
) -> Result<DailyUsageSnapshot, StorageError> {
    // The caller already holds the public owner lock. Keep operation locks in
    // the stable owner/operation order, but do not lock every historical
    // payer/recipient account: daily usage is a fact snapshot, and those
    // account locks would be reacquired in a different order by submit.
    read_for_mode(tx, owner, true).await
}

pub(crate) async fn read_for_mode(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    lock: bool,
) -> Result<DailyUsageSnapshot, StorageError> {
    // Identity enumeration only, not a replacement for member business facts.
    // Lock member operation keys in stable order BEFORE any account/row locks.
    let identities: Vec<(String, String)> = sqlx::query_as(
        "SELECT owner_id,operation_id FROM transfer_business_orders_v1 WHERE owner_id=$1
         UNION SELECT f.owner_id,f.operation_id FROM transfer_fund_orders_v1 f
         LEFT JOIN transfer_accounts_v1 a ON a.account_id=f.payer_account_id
         WHERE f.owner_id=$1 OR a.owner_id=$1 ORDER BY 1,2",
    )
    .bind(owner.as_str())
    .fetch_all(&mut **tx)
    .await?;
    for (owner, operation) in &identities {
        if lock {
            protect_operation(tx, owner, operation).await?;
        }
    }
    let accounts: Vec<String> = sqlx::query_scalar(
        "SELECT account_id FROM transfer_accounts_v1 WHERE owner_id=$1 ORDER BY account_id",
    )
    .bind(owner.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let payer_accounts: Vec<ResourceId> = accounts
        .into_iter()
        .map(|id| ResourceId::parse(id).map_err(|_| StorageError::CorruptRecord))
        .collect::<Result<_, _>>()?;
    let mut facts = FundsStore::daily_usage_facts(tx, owner)
        .await
        .map_err(crate::transfer_sql::business_error)?;
    let business_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transfer_business_orders_v1 WHERE owner_id=$1")
            .bind(owner.as_str())
            .fetch_one(&mut **tx)
            .await?;
    if business_count != facts.len() as i64 {
        return Err(StorageError::ReconcileRequired);
    }
    let mut unmatched: BTreeMap<String, usize> = facts
        .iter()
        .enumerate()
        .map(|(index, fact)| (fact.operation_id.as_str().to_owned(), index))
        .collect();
    if unmatched.len() != facts.len() {
        return Err(StorageError::CorruptRecord);
    }
    let rows = sqlx::query(
        "SELECT * FROM public_execution_requests WHERE owner_id=$1 ORDER BY business_operation_ref",
    )
    .bind(owner.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut seen = BTreeSet::new();
    for row in rows {
        let stored = crate::execution_request::stored_row(&row)?;
        let Some(first) = stored.first_submission else {
            continue; // Reservations/draft actions are not formal fund submissions.
        };
        let request = &stored.request;
        let operation = request.business_operation_ref.as_str();
        if !funding_shape(&request.payload.normalized_arguments)
            && !unmatched.contains_key(operation)
        {
            continue;
        }
        let index = unmatched
            .remove(operation)
            .ok_or(StorageError::ReconcileRequired)?;
        let reserved_at: chrono::DateTime<chrono::Utc> = row.try_get("reserved_at")?;
        if first.recorded_at < reserved_at || !seen.insert(operation.to_owned()) {
            return Err(StorageError::CorruptRecord);
        }
        let expected = crate::transfer_result::bound_request(request)?;
        // Both modes use the same immutable business/ledger corroboration.
        // Account row locks belong to the single-operation submit/reconcile
        // paths, never to a batch historical usage scan.
        let record = crate::transfer_result::verified_business_record(
            tx,
            owner,
            &expected,
            &request.idempotency_key,
        )
        .await?;
        let fact = &mut facts[index];
        if fact.owner_id != *owner
            || fact.operation_id != record.request.operation_id
            || fact.payer_account_id != record.request.payer_account_id
            || fact.amount != record.request.amount
            || fact.status != record.latest_receipt.status
            || !payer_accounts.contains(&fact.payer_account_id)
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        fact.submitted_at = Some(first.recorded_at.to_rfc3339());
    }
    if !unmatched.is_empty() {
        return Err(StorageError::ReconcileRequired);
    }
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    let as_of = now.to_rfc3339();
    let usage = calculate_daily_usage(DailyUsageInput {
        owner_id: owner,
        as_of: &as_of,
        complete: true,
        submitted: &facts,
    })
    .map_err(|_| StorageError::FactsConflict)?;
    Ok(DailyUsageSnapshot {
        owner_id: owner.clone(),
        payer_accounts,
        as_of,
        submitted: facts,
        usage,
    })
}

impl ExecutionRequestStore {
    /// Backend-only, complete owner scope from the trusted session. Not a permit.
    pub async fn daily_usage(
        &self,
        session: &SessionRef,
    ) -> Result<DailyUsageSnapshot, StorageError> {
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let mut tx = crate::repository::guarded(&guard, self.repository.pool.begin()).await?;
        crate::repository::guarded(
            &guard,
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED").execute(&mut *tx),
        )
        .await?;
        crate::repository::guarded(&guard, protect_owner(&mut tx, &identity.user_id)).await?;
        let snapshot =
            crate::repository::guarded(&guard, read_locked(&mut tx, &identity.user_id)).await?;
        crate::repository::guarded(&guard, tx.commit()).await?;
        Ok(snapshot)
    }
}
