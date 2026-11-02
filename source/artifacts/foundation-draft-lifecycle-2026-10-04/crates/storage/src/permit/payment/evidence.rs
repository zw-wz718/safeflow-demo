//! Historical evidence only. This report never constructs a completion grant.
use super::*;
use crate::repository::{guarded_poll, task_row};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaymentAcceptedHistoryReport {
    pub scope: &'static str,
    pub evidence: Value,
    pub evidence_digest: ArgumentsHash,
}

/// Native current-owner read, not registered as an HTTP/model Tool.
pub struct PaymentAcceptedHistoryReader {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
}
impl PaymentAcceptedHistoryReader {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            repository,
            sessions,
        }
    }
    pub async fn query(
        &self,
        reference: &SessionRef,
        task: &TaskId,
        preparation: &OperationId,
    ) -> Result<PaymentAcceptedHistoryReport, StorageError> {
        valid_id(task.as_str())?;
        valid_id(preparation.as_str())?;
        let identity = self
            .sessions
            .resolve_binding(reference)
            .map_err(|_| StorageError::Session)?;
        let guard = || {
            if self
                .sessions
                .resolve_binding(reference)
                .map_err(|_| StorageError::Session)?
                == identity
            {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let mut tx = guarded_poll(&guard, self.repository.pool.begin()).await?;
        guarded_poll(
            &guard,
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx),
        )
        .await?;
        // Nested SQL awaits must stop on the first resumption after the
        // current binding becomes invalid, before issuing further evidence SQL.
        let evidence = guarded_poll(&guard, async {
            let row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2")
                .bind(task.as_str())
                .bind(identity.user_id.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StorageError::NotFound)?;
            let task = task_row(&row)?;
            crate::lifecycle::check_mode(&mut tx, &task.task_id, true).await?;
            let ops = operations(&mut tx, &task.task_id).await?;
            crate::lifecycle::project_progress(&mut tx, &task, &ops).await?;
            inspect_processing(&mut tx, &task, preparation).await
        })
        .await?;
        let digest = hash(&evidence)?;
        guarded_poll(&guard, tx.commit()).await?;
        Ok(PaymentAcceptedHistoryReport {
            scope: "ACCEPTED_PROCESSING_HISTORY_NOT_COMPLETION_AUTHORITY",
            evidence,
            evidence_digest: digest,
        })
    }
}

pub(crate) async fn inspect_processing(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    preparation: &OperationId,
) -> Result<Value, StorageError> {
    inspect_history(tx, task, preparation, true).await
}

pub(crate) async fn inspect_history(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    preparation: &OperationId,
    processing: bool,
) -> Result<Value, StorageError> {
    let saved = saved_payment(tx, task, preparation, false).await?;
    let row = sqlx::query(
        "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2",
    )
    .bind(task.owner_id.as_str())
    .bind(saved.snapshot.transfer.operation_id.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::ReconcileRequired)?;
    let stored = crate::execution_request::stored_row(&row)?;
    let public = view_locked(tx, task, stored).await?; // full association, snapshot, fingerprint
    let r = &public.request;
    let b = &r.binding.snapshot;
    let first = public
        .first_submission
        .as_ref()
        .ok_or(StorageError::ReconcileRequired)?;
    let association_row=sqlx::query("SELECT * FROM payment_funds_execution_v1 WHERE execution_operation_id=$1 AND preparation_operation_id=$2")
        .bind(b.operation_id.as_str()).bind(preparation.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    if (processing && public.current_operation.state != OperationState::Processing)
        || (!processing && !matches!(public.current_operation.state, OperationState::Succeeded | OperationState::Failed))
        || public.current_operation.revision != if processing { 3 } else { 4 }
        || first.operation.state != OperationState::Submitted
        || first.operation.revision != 2
        || first.operation.operation_id != b.operation_id
        || first.operation.task_id != task.task_id
        || first.operation.node_id != b.node_id
        || first.operation.plan_version != b.plan_version
        || first.operation.facts_version != b.facts_version
        || first.operation.arguments_hash != b.arguments_hash
        || first.operation.business_operation_ref.as_ref() != Some(&r.business_operation_ref)
        || task.revision != b.task_revision
        || task.plan.plan_version != b.plan_version
        || task.facts_version != b.facts_version
        || hash(&task.plan)? != saved.base_digest
    {
        return Err(StorageError::ReconcileRequired);
    }
    verify_submitted_snapshot(tx, &task.owner_id, &saved.snapshot).await?;
    let row =
        sqlx::query("SELECT * FROM execution_permits WHERE operation_id=$1 AND facts_version=$2")
            .bind(b.operation_id.as_str())
            .bind(db_version(b.facts_version)?)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::ReconcileRequired)?;
    let permit = permit_row(&row)?;
    let c = &saved.stored.record;
    let consumed = DateTime::<Utc>::from_timestamp_millis(
        permit.consumed_at.ok_or(StorageError::ReconcileRequired)?,
    )
    .ok_or(StorageError::CorruptRecord)?;
    let confirmed = c.confirmed_at.ok_or(StorageError::ReconcileRequired)?;
    if permit.status != ExecutionPermitStatus::Consumed
        || permit.binding != r.binding
        || permit.payload != r.payload
        || permit.execution_scope != Some(VerifiedPermitScope::Transfer)
        || permit.confirmation_ref != c.confirmation_ref
        || permit.session_digest != saved.stored.session_digest
        || permit.session_version != saved.stored.session_binding_version
        || confirmed < c.prepared_at
        || consumed.timestamp_millis() < confirmed.timestamp_millis()
        || consumed >= c.expires_at
        || consumed.timestamp_millis() < permit.issued
        || consumed.timestamp_millis() >= permit.expires
        || first.recorded_at < consumed
        || first.recorded_at < confirmed
        || first.recorded_at >= c.expires_at
        || c.revoked_at.is_some_and(|at| at <= first.recorded_at)
    {
        return Err(StorageError::ReconcileRequired);
    }
    // Immutable accepted-event/consume/confirmation trail corroborates the rows.
    // No check that the old session or confirmation remains active NOW.
    let audit_ok:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND kind='PERMIT_CONSUMED' AND details->>'permit_id'=$3 AND details->>'operation_fingerprint'=$4) AND EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='OPERATION_TRANSITIONED' AND details->>'event'='SUBMISSION_RECORDED' AND details->>'original_reference'=$5) AND EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND operation_id=$6 AND kind='EXPLICIT_CONFIRMATION_RECORDED' AND details->>'confirmation_ref'=$7 AND details->>'binding_digest'=$8 AND details->>'status'='CONFIRMED' AND details->'expires_at'=$9)")
        .bind(task.task_id.as_str()).bind(b.operation_id.as_str()).bind(permit.id.as_str())
        .bind(r.binding.fingerprint.as_str()).bind(r.business_operation_ref.as_str()).bind(preparation.as_str())
        .bind(c.confirmation_ref.as_str()).bind(hash(&c.binding)?.as_str()).bind(Json(json!(c.expires_at))).fetch_one(&mut **tx).await?;
    if !audit_ok {
        return Err(StorageError::ReconcileRequired);
    }
    let business = crate::transfer_result::verified_business_record(
        tx,
        &task.owner_id,
        &saved.snapshot.transfer,
        &r.idempotency_key,
    )
    .await?;
    let accounting = FundsStore::check_accounting(
        &mut **tx,
        &task.owner_id,
        &saved.snapshot.transfer.operation_id,
    )
    .await
    .map_err(crate::transfer_sql::business_error)?
    .ok_or(StorageError::ReconcileRequired)?;
    if business.first_receipt.status != ToolResultStatus::Processing
        || (processing && business.latest_receipt.status != ToolResultStatus::Processing)
        || (!processing && business.latest_receipt.status == ToolResultStatus::Processing)
        || public.current_operation.state != match business.latest_receipt.status {
            ToolResultStatus::Processing => OperationState::Processing,
            ToolResultStatus::Succeeded => OperationState::Succeeded,
            ToolResultStatus::Failed => OperationState::Failed,
        }
        || accounting.record != business
        || (processing && accounting.operation_reserved != saved.snapshot.transfer.amount)
        || (!processing && accounting.operation_reserved.as_str() != "0.00")
    {
        return Err(StorageError::ReconcileRequired);
    }
    let fund=sqlx::query("SELECT f.recipient_account_id,f.amount_cents FROM transfer_fund_orders_v1 f JOIN transfer_accounts_v1 a ON a.account_id=f.payer_account_id WHERE f.owner_id=$1 AND f.operation_id=$2 AND a.owner_id=$1 AND f.payer_account_id=$3 AND f.state=$4")
        .bind(task.owner_id.as_str()).bind(saved.snapshot.transfer.operation_id.as_str()).bind(saved.snapshot.transfer.payer_account_id.as_str())
        .bind(match public.current_operation.state {OperationState::Processing=>"PROCESSING",OperationState::Succeeded=>"SUCCEEDED",OperationState::Failed=>"FAILED",_=>return Err(StorageError::ReconcileRequired)})
        .fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    let recipient: String = fund.try_get("recipient_account_id")?;
    if recipient == saved.snapshot.transfer.payer_account_id.as_str() {
        return Err(StorageError::ReconcileRequired);
    }
    Ok(
        json!({"schema":"payment-accepted-history-candidate-v1","mode":"PROJECT_SIMULATION",
        "task":task,"preparation_operation_id":preparation,"execution_operation_id":b.operation_id,
        "business_operation_ref":r.business_operation_ref,"original_key":r.idempotency_key,
        "snapshot":saved.snapshot,"snapshot_digest":saved.digest,"request_binding":{"snapshot":b,"fingerprint":r.binding.fingerprint},
        "request_payload":r.payload,"request_digest":r.digest()?,
        "association_digest":association_row.try_get::<String,_>("association_digest")?,
        "first_submission":{"operation":first.operation,"recorded_at":first.recorded_at},"current_operation":public.current_operation,
        "confirmation":c,"session_digest":permit.session_digest,"session_version":permit.session_version,
        "permit_id":permit.id,"permit_consumed_at":consumed,"permit_issued_ms":permit.issued,"permit_expires_ms":permit.expires,
        "business":business,"original_recipient_account_id":recipient,
        "amount_cents":fund.try_get::<i64,_>("amount_cents")?,"accounting":accounting}),
    )
}
