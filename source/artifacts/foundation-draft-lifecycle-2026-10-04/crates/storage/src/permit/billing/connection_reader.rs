//! Narrow adoption into the existing original-result READ ONLY query.
//! This returns no witness, permit, committed/saved assertion or card capability.
use super::*;
use safeflow_billing_source_reader::feedback_store::load_feedback_on_connection;

// Only SELECTs: check the trusted caller's original transaction/schema on both
// sides of the member await. This detects connection replacement; it is not an
// authority fence against external changes or a guarantee against a malicious DBA.
async fn observation_scope(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &str,
) -> Result<i64, StorageError> {
    let (txid, schema, schemas, read_only): (i64, String, Vec<String>, String) =
        sqlx::query_as("SELECT txid_current(),current_schema()::text,current_schemas(false)::text[],current_setting('transaction_read_only')")
            .fetch_one(&mut **tx).await?;
    if schema != namespace || schemas != [namespace] || read_only != "on" {
        return Err(StorageError::TransactionScope);
    }
    Ok(txid)
}

pub(super) async fn verify_original_on_connection(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &str,
    scope: &TrustedScope,
    request: &ExecutionRequest,
    restored: &RestoredReport,
) -> Result<(), StorageError> {
    let before = observation_scope(tx, namespace).await?;
    let expected = input(request)?;
    let operation = &request.binding.snapshot.operation_id;
    let owned = load_feedback_on_connection(&mut **tx, scope, operation.as_str())
        .await
        .map_err(store_error)?
        .ok_or(StorageError::CorruptRecord)?;
    let view = owned.checked_projection(scope).map_err(store_error)?;
    let original = restored
        .report
        .evidence
        .as_ref()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.reference == expected.transaction)
        })
        .ok_or(StorageError::CorruptRecord)?;
    if request.binding.snapshot.user_id.as_str() != scope.user_id()
        || expected.original_digest != restored.archive_digest
        || view.operation_id() != operation.as_str()
        || view.owner_user_id() != scope.user_id()
        || view.report_id() != expected.report_id
        || view.report_id() != restored.report.metadata.report_id
        || view.choice() != &expected.choice
        || view.reference() != &expected.transaction
        || view.reference() != &original.reference
        || view.reference().source_id != restored.report.read.source_id
        || view.card_id() != original.transaction.card_id.as_deref()
        || view.request() != &restored.request
        || view.report_account_ids() != restored.request.account_ids
        || Some(view.snapshot_id()) != restored.report.read.snapshot_id.as_deref()
        || view.rule_version() != restored.report.rule_version
        || view.calculated_at() != restored.report.metadata.calculated_at
        || view.local_event_namespace() != "billing_feedback_events_v1"
        || view.local_event_id() <= 0
        || view.local_event_kind() != "BILLING_FEEDBACK_RECORDED_LOCAL_V1"
    {
        return Err(StorageError::CorruptRecord);
    }
    if observation_scope(tx, namespace).await? != before {
        return Err(StorageError::TransactionScope);
    }
    // The caller still verifies the public result/request, full archived member
    // payload/event and terminal task/op/link/outbox in THIS transaction, then
    // rechecks current session and every original account. Never export `owned`.
    Ok(())
}
