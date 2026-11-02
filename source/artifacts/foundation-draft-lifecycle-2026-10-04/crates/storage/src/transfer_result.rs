//! Internal adapter for the received full transfer request, not a public Tool schema.
//! Reads actual SQL; never accepts a caller-selected outcome or resubmits funds.
use crate::models::{db_version, validate_checkpoint};
use crate::repository::{audit, encode, guarded, lock_task, operations, read_task, update_task};
use crate::{
    ExecutionRequest, ExecutionRequestStore, ExecutionRequestView, OperationRecord,
    PersistentAuditKind, StorageError, TaskRecord,
};
use safeflow_contracts::{
    IdempotencyKey, OperationEvent, OperationState, SessionRef, ToolResultStatus, UserId,
};
use safeflow_transfer_integration::{
    business_store::BusinessRecord, funds_store::FundsStore, mock_execution::MockTransferRequest,
};
use sqlx::{Postgres, Row, Transaction};

#[derive(Clone, Debug, PartialEq)]
pub struct TransferExecutionView {
    pub public: ExecutionRequestView,
    pub business: BusinessRecord,
}

impl ExecutionRequestStore {
    /// Requires the *entire received member DTO* bound as normalized_arguments.
    /// No inference from display fields or a proposed Tool's partial arguments.
    /// This backend representation is not approval/registration of a funds Tool.
    pub async fn original_transfer(
        &self,
        session: &SessionRef,
        key: &IdempotencyKey,
    ) -> Result<TransferExecutionView, StorageError> {
        self.transfer_result(session, key, false).await
    }

    /// Recover only from the original persisted member bank record. Missing or
    /// inconsistent facts are errors; there is no execute/resolve/network callback.
    pub async fn reconcile_transfer(
        &self,
        session: &SessionRef,
        key: &IdempotencyKey,
    ) -> Result<TransferExecutionView, StorageError> {
        self.transfer_result(session, key, true).await
    }

    async fn transfer_result(
        &self,
        session: &SessionRef,
        key: &IdempotencyKey,
        reconcile: bool,
    ) -> Result<TransferExecutionView, StorageError> {
        crate::models::valid_id(key.as_str())?;
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        if !reconcile {
            guarded(
                &guard,
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                    .execute(&mut *tx),
            )
            .await?;
        }
        // A historical original read is a genuine READ ONLY snapshot: it must
        // not acquire advisory/row locks. Reconciliation is the explicit
        // write path and keeps the owner protection through COMMIT.
        if reconcile {
            guarded(
                &guard,
                crate::daily_usage::protect_owner(&mut tx, &identity.user_id),
            )
            .await?;
        }
        let task_id = guarded(&guard, async {
            let row = sqlx::query("SELECT task_id FROM public_execution_requests WHERE owner_id=$1 AND idempotency_key=$2")
                .bind(identity.user_id.as_str()).bind(key.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
            crate::repository::decode(&row, "task_id")
        }).await?;
        let task = if reconcile {
            guarded(&guard, lock_task(&mut tx, &identity.user_id, &task_id)).await?
        } else {
            guarded(&guard, read_task(&mut tx, &identity.user_id, &task_id)).await?
        };
        let request = guarded(&guard, async {
            let row = sqlx::query(
                "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND idempotency_key=$2",
            )
            .bind(identity.user_id.as_str())
            .bind(key.as_str())
            .fetch_one(&mut *tx)
            .await?;
            crate::execution_request::stored_row(&row)
        })
        .await?;
        let mut public = guarded(
            &guard,
            crate::execution_request::view_locked(&mut tx, &task, request),
        )
        .await?;
        if public.first_submission.is_none() {
            return Err(StorageError::ReconcileRequired);
        }
        if reconcile {
            guarded(
                &guard,
                crate::daily_usage::protect_operation(
                    &mut tx,
                    identity.user_id.as_str(),
                    public.request.business_operation_ref.as_str(),
                ),
            )
            .await?;
        }
        let business = if reconcile {
            guarded(&guard, verified_record(&mut tx, &public.request)).await?
        } else {
            let expected = bound_request(&public.request)?;
            guarded(
                &guard,
                verified_business_record(
                    &mut tx,
                    &identity.user_id,
                    &expected,
                    &public.request.idempotency_key,
                ),
            )
            .await?
        };
        if reconcile {
            guarded(&guard, settle(&mut tx, &task, &public.request, &business)).await?;
            let stored = crate::execution_request::request_locked(&mut tx, &public.request).await?;
            public = guarded(
                &guard,
                crate::execution_request::view_locked(&mut tx, &task, stored),
            )
            .await?;
        }
        guarded(&guard, tx.commit()).await?;
        Ok(TransferExecutionView { public, business })
    }
}

pub(crate) async fn verified_record(
    tx: &mut Transaction<'_, Postgres>,
    request: &ExecutionRequest,
) -> Result<BusinessRecord, StorageError> {
    let expected = bound_request(request)?;
    let owner = &request.binding.snapshot.user_id;
    // Preserve the write adapter's locking before reusing the read verification.
    sqlx::query("SELECT operation_id FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2 FOR SHARE")
        .bind(owner.as_str()).bind(expected.operation_id.as_str()).fetch_optional(&mut **tx).await?
        .ok_or(StorageError::ReconcileRequired)?;
    sqlx::query("SELECT operation_id FROM transfer_fund_orders_v1 WHERE owner_id=$1 AND operation_id=$2 FOR SHARE")
        .bind(owner.as_str()).bind(expected.operation_id.as_str()).fetch_optional(&mut **tx).await?
        .ok_or(StorageError::ReconcileRequired)?;
    lock_transfer_accounts(tx, owner, &expected).await?;
    verified_business_record(tx, owner, &expected, &request.idempotency_key).await
}

async fn lock_transfer_accounts(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    expected: &MockTransferRequest,
) -> Result<(), StorageError> {
    let recipient: String = sqlx::query_scalar(
        "SELECT recipient_account_id FROM transfer_fund_orders_v1
         WHERE owner_id=$1 AND operation_id=$2",
    )
    .bind(owner.as_str())
    .bind(expected.operation_id.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::ReconcileRequired)?;
    let rows = sqlx::query(
        "SELECT account_id FROM transfer_accounts_v1
         WHERE account_id=$1 OR account_id=$2 ORDER BY account_id FOR SHARE",
    )
    .bind(expected.payer_account_id.as_str())
    .bind(&recipient)
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() != 2 {
        return Err(StorageError::ReconcileRequired);
    }
    Ok(())
}

/// Read verification shared by public settlement and payment recovery. The read
/// caller must hold a repeatable snapshot; this helper never changes bank state.
pub(crate) async fn verified_business_record(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    expected: &MockTransferRequest,
    key: &IdempotencyKey,
) -> Result<BusinessRecord, StorageError> {
    let row = sqlx::query("SELECT original_key FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2")
        .bind(owner.as_str()).bind(expected.operation_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    if row.try_get::<String, _>("original_key")? != key.as_str() {
        return Err(StorageError::ExecutionKeyConflict);
    }
    let record = FundsStore::query(tx, owner, &expected.operation_id)
        .await
        .map_err(crate::transfer_sql::business_error)?
        .ok_or(StorageError::ReconcileRequired)?;
    if &record.request != expected {
        return Err(StorageError::PermitBindingMismatch);
    }
    // Corroborate the receipt against the immutable ledger, including BOTH sides.
    let row = sqlx::query("SELECT f.amount_cents,f.payer_account_id,f.recipient_account_id, (SELECT count(*) FROM transfer_ledger_v1 l WHERE l.business_owner_id=f.owner_id AND l.operation_id=f.operation_id) AS rows, (SELECT count(*) FROM transfer_ledger_v1 l WHERE l.business_owner_id=f.owner_id AND l.operation_id=f.operation_id AND l.amount_cents=f.amount_cents AND ((l.direction='DEBIT' AND l.account_id=f.payer_account_id) OR (l.direction='CREDIT' AND l.account_id=f.recipient_account_id))) AS matching FROM transfer_fund_orders_v1 f WHERE f.owner_id=$1 AND f.operation_id=$2")
        .bind(owner.as_str()).bind(expected.operation_id.as_str()).fetch_one(&mut **tx).await?;
    let count: i64 = row.try_get("rows")?;
    let matching: i64 = row.try_get("matching")?;
    if (record.latest_receipt.status == ToolResultStatus::Succeeded
        && (count != 2 || matching != 2))
        || (record.latest_receipt.status != ToolResultStatus::Succeeded && count != 0)
    {
        return Err(StorageError::CorruptRecord);
    }
    let accounting = FundsStore::check_accounting(&mut **tx, owner, &expected.operation_id)
        .await
        .map_err(crate::transfer_sql::business_error)?
        .ok_or(StorageError::ReconcileRequired)?;
    if accounting.record != record {
        return Err(StorageError::CorruptRecord);
    }
    Ok(record)
}

pub(crate) fn bound_request(
    request: &ExecutionRequest,
) -> Result<MockTransferRequest, StorageError> {
    if request.binding.snapshot.lifecycle_anchor.is_some() {
        return crate::permit::payment::validate_request(request).map(|snapshot| snapshot.transfer);
    }
    request.validate()?;
    let expected: MockTransferRequest =
        serde_json::from_value(request.payload.normalized_arguments.clone())
            .map_err(|_| StorageError::DependencyUnavailable)?;
    if serde_json::to_value(&expected).map_err(|_| StorageError::CorruptRecord)?
        != request.payload.normalized_arguments
        || expected.task_id != request.binding.snapshot.task_id
        || expected.operation_id.as_str() != request.business_operation_ref.as_str()
        || expected.draft_id != request.payload.resource_id
        || u64::from(expected.draft_version) != request.business_version
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    Ok(expected)
}

pub(crate) async fn settle(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    request: &ExecutionRequest,
    record: &BusinessRecord,
) -> Result<OperationRecord, StorageError> {
    let mut ops = operations(tx, &task.task_id).await?;
    let op = ops
        .iter_mut()
        .find(|op| op.operation_id == request.binding.snapshot.operation_id)
        .ok_or(StorageError::NotFound)?;
    let target = match record.latest_receipt.status {
        ToolResultStatus::Succeeded => OperationState::Succeeded,
        ToolResultStatus::Failed => OperationState::Failed,
        ToolResultStatus::Processing => OperationState::Processing,
    };
    if op.state == target {
        return Ok(op.clone());
    }
    let event = match (op.state, target) {
        (OperationState::Submitted, OperationState::Succeeded) => {
            OperationEvent::ExecutionSucceeded
        }
        (OperationState::Submitted, OperationState::Failed) => OperationEvent::ExecutionFailed,
        (OperationState::Submitted, OperationState::Processing) => OperationEvent::OutcomeUnknown,
        (OperationState::Processing, OperationState::Succeeded) => {
            OperationEvent::StatusConfirmedSucceeded
        }
        (OperationState::Processing, OperationState::Failed) => {
            OperationEvent::StatusConfirmedFailed
        }
        _ => return Err(StorageError::IllegalTransition),
    };
    let old = op.clone();
    op.state = old
        .state
        .transition(event)
        .map_err(|_| StorageError::IllegalTransition)?;
    op.revision = old
        .revision
        .checked_add(1)
        .ok_or(StorageError::RevisionConflict)?;
    let next = op.clone();
    // Preserve the recovery point: Runtime must separately review/continue the
    // plan. A bank terminal result does not complete the entire Agent task.
    validate_checkpoint(task, &ops)?;
    let updated = sqlx::query("UPDATE operations SET state=$1,revision=$2,updated_at=clock_timestamp() WHERE operation_id=$3 AND revision=$4")
        .bind(encode(next.state)?).bind(db_version(next.revision)?).bind(next.operation_id.as_str()).bind(db_version(old.revision)?).execute(&mut **tx).await?;
    if updated.rows_affected() != 1 {
        return Err(StorageError::RevisionConflict);
    }
    let mut next_task = task.clone();
    next_task.revision = task
        .revision
        .checked_add(1)
        .ok_or(StorageError::RevisionConflict)?;
    update_task(tx, &next_task, task.revision).await?;
    audit(tx, &next_task, Some(&next), PersistentAuditKind::OperationTransitioned,
        serde_json::json!({"event":event,"from":old.state,"to":next.state,"source":"transfer-funds-v1","business_operation_ref":request.business_operation_ref,"first_receipt":record.first_receipt,"latest_receipt":record.latest_receipt})).await?;
    Ok(next)
}
