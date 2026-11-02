//! Actual business readback and member SQL share the public settlement connection.
use super::*;
use safeflow_transfer_integration::{
    funds_store::{FundsError, FundsStore},
    DraftStatus, StoredDraft,
};

impl LifecycleService<'_> {
    pub(super) fn retain_payee_query(
        &self,
        request: &mut Value,
        action: MockDraftAction,
        parameters: &Value,
    ) {
        let selected = if action == MockDraftAction::Create {
            &parameters["payee_id"]
        } else {
            &parameters["changes"]["payee_id"]
        };
        if let Some((payee, query)) = &self.payee_query {
            if selected.is_null() || *selected == json!(payee) {
                request["draft_restore_query"] = json!({"payee_id":payee,"query":query});
            }
        }
    }

    pub(super) async fn restore_sql_draft(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        identity: &SessionBinding,
        task: &TaskRecord,
        op: &OperationRecord,
        receipt: &LifecycleReceipt,
    ) -> Result<LifecycleDraftObservation, StorageError> {
        // checked_history/checked_claims and the task lock are held by callers.
        // Only a successful action's retained query, or new explicit input, is a source.
        let payee = &receipt.result.data["payee_id"];
        let query = if let Some((id, query)) = &self.payee_query {
            if json!(id) == *payee {
                Some(query.clone())
            } else {
                None
            }
        } else {
            None
        };
        let query = if let Some(query) = query {
            query
        } else {
            let rows: Vec<Json<Value>> = sqlx::query_scalar("SELECT a.request FROM mock_draft_lifecycle_actions a JOIN mock_draft_lifecycle_claims c USING(operation_id,task_id) WHERE a.task_id=$1 AND c.status='SUCCEEDED' ORDER BY a.plan_version DESC")
                .bind(task.task_id.as_str()).fetch_all(&mut **tx).await?;
            rows.iter()
                .find_map(|row| {
                    let source = &row.0["draft_restore_query"];
                    (source["payee_id"] == *payee)
                        .then(|| source["query"].as_str().map(str::to_owned))
                        .flatten()
                })
                .ok_or(StorageError::DependencyUnavailable)?
        };
        let recovery = FundsStore::draft_for_restore(
            tx,
            &identity.user_id,
            &receipt.draft_id,
            receipt.draft_version,
            &query,
        )
        .await
        .map_err(storage_error)?;
        self.guard(session, identity)?;
        let context = Self::context(task, op);
        check_owner(&context, &recovery.draft)?;
        let expected = observation(&recovery.draft)?;
        if expected.business_operation_id != receipt.business_operation_id
            || expected.draft_id != receipt.draft_id
            || expected.draft_version != receipt.draft_version
            || expected.status != "READY_TO_CONFIRM"
            || expected.parameters != super::service::result_parameters(&receipt.result.data)?
            || json!(expected.expires_at) != receipt.result.data["expires_at"]
            || expected.expires_at <= crate::confirmation::timestamp(self.clock.as_ref())?
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let original = recovery.draft.clone();
        let restored = self.observer.restore(&context, recovery).await?;
        self.guard(session, identity)?;
        if restored != original {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        // No SQL/public writes or claim changes here. The caller retains this
        // connection's locks through its original save/settlement transaction.
        Ok(expected)
    }

    pub(super) async fn check_sql_draft_before(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        context: &ExecutionContext,
        observed: &LifecycleDraftObservation,
    ) -> Result<(), StorageError> {
        let actual = self.observer.stored(context, &observed.draft_id).await?;
        check_owner(context, &actual)?;
        if observation(&actual)? != *observed {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        // Same advisory key/order as the received FundsStore::save_draft, then
        // the row lock, retained until settlement COMMIT/rollback.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(
                json!([
                    "transfer-draft-v1",
                    context.authenticated_user_id,
                    observed.draft_id
                ])
                .to_string(),
            )
            .execute(&mut **tx)
            .await?;
        let submitted: bool = sqlx::query_scalar(
            "SELECT submitted FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 FOR UPDATE",
        )
        .bind(context.authenticated_user_id.as_str())
        .bind(observed.draft_id.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::DependencyUnavailable)?;
        let saved = FundsStore::draft(tx, &context.authenticated_user_id, &observed.draft_id)
            .await
            .map_err(storage_error)?;
        if submitted || saved != actual {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        Ok(())
    }

    pub(super) async fn save_sql_draft(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        identity: &SessionBinding,
        context: &ExecutionContext,
        receipt: &LifecycleReceipt,
    ) -> Result<(), StorageError> {
        let actual = self.observer.stored(context, &receipt.draft_id).await?;
        self.guard(session, identity)?;
        check_owner(context, &actual)?;
        let observed = observation(&actual)?;
        let expires: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(receipt.result.data["expires_at"].clone())
                .map_err(|_| StorageError::CorruptRecord)?;
        let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut **tx)
            .await?;
        if actual.draft_id != receipt.draft_id
            || actual.operation_id.as_str() != receipt.business_operation_id.as_str()
            || actual.plan_version != receipt.draft_version
            || observed.status != receipt.result.data["status"]
            || observed.parameters != super::service::result_parameters(&receipt.result.data)?
            || observed.expires_at != expires
            || now >= expires
            || context.operation_id != receipt.operation_id
            || context.plan_version != receipt.plan_version
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        FundsStore::save_draft(
            tx,
            &identity.user_id,
            receipt.expected_draft_version,
            &actual,
        )
        .await
        .map_err(storage_error)?;
        let saved = FundsStore::draft(tx, &identity.user_id, &actual.draft_id)
            .await
            .map_err(storage_error)?;
        if saved != actual {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        self.guard(session, identity)?;
        Ok(())
    }
}

fn check_owner(context: &ExecutionContext, draft: &StoredDraft) -> Result<(), StorageError> {
    if draft.user_id != context.authenticated_user_id || draft.task_id != context.task_id {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    Ok(())
}

fn observation(draft: &StoredDraft) -> Result<LifecycleDraftObservation, StorageError> {
    let details = draft
        .details
        .as_ref()
        .ok_or(StorageError::DependencyUnavailable)?;
    Ok(LifecycleDraftObservation {
        draft_id: draft.draft_id.clone(),
        business_operation_id: BusinessOperationRef::parse(draft.operation_id.as_str())
            .map_err(|_| StorageError::CorruptRecord)?,
        draft_version: draft.plan_version,
        status: match draft.status {
            DraftStatus::ReadyToConfirm => "READY_TO_CONFIRM",
            DraftStatus::Cancelled => "CANCELLED",
            DraftStatus::Expired => "EXPIRED",
            DraftStatus::NeedsInformation => "NEEDS_INFORMATION",
        }
        .into(),
        expires_at: chrono::DateTime::from_timestamp(
            i64::try_from(draft.expires_at).map_err(|_| StorageError::CorruptRecord)?,
            0,
        )
        .ok_or(StorageError::CorruptRecord)?,
        parameters: json!({"payer_account_id":details.account.account_id,"payee_id":details.payee.payee_id,"amount":details.amount,"currency":details.currency,"remark":details.remark}),
    })
}

fn storage_error(error: FundsError) -> StorageError {
    match error {
        FundsError::Database(_) => StorageError::Database,
        FundsError::VersionConflict => StorageError::ConfirmationBindingMismatch,
        FundsError::NotFound => StorageError::DependencyUnavailable,
        _ => StorageError::CorruptRecord,
    }
}
