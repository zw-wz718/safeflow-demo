//! Historical comparison adapter, never a preparation-to-Permit conversion.
use super::*;
use crate::repository::{guarded, task_row};
use safeflow_contracts::{IdempotencyKey, OperationFingerprint, ToolResultStatus};
use safeflow_transfer_integration::business_store::BusinessRecord;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaymentExecutionBinding {
    pub preparation_operation_id: OperationId,
    pub preparation_plan_version: u32,
    pub preparation_arguments_hash: ArgumentsHash,
    pub business_operation_id: OperationId,
    pub business_draft_version: u32,
    pub normalized_arguments: MockTransferRequest,
    pub transfer_arguments_hash: ArgumentsHash,
    pub execution_operation_id: Option<OperationId>,
    pub execution_plan_version: Option<u32>,
    pub execution_tool_name: Option<ToolName>,
    pub idempotency_key: Option<IdempotencyKey>,
    pub operation_fingerprint: Option<OperationFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentOriginalState {
    Unsubmitted,
    RegisteredUnsubmitted,
    Processing,
    Succeeded,
    Failed,
    ReconcileRequired,
    SourceConflict,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaymentOriginalResult {
    pub contract_version: u16,
    pub scope: &'static str,
    pub task_id: TaskId,
    pub binding: PaymentExecutionBinding,
    pub state: PaymentOriginalState,
    pub reason: &'static str,
    pub first_public_submission: Option<OperationRecord>,
    pub public_submitted_at: Option<DateTime<Utc>>,
    pub verified_business_result: Option<BusinessRecord>,
    pub funds_execution_enabled: bool,
    pub public_sync_enabled: bool,
    pub execution_link: Option<PaymentExecutionLink>,
}

impl PaymentPreparationService<'_> {
    /// Only task and original preparation selectors are accepted. Current same-owner
    /// login authorizes history; expired original confirmation grants no authority.
    pub async fn original_result(
        &self,
        reference: &SessionRef,
        task_id: &TaskId,
        operation: &OperationId,
    ) -> Result<PaymentOriginalResult, StorageError> {
        PaymentOriginalReader::for_trusted_backend(
            self.repository.clone(),
            self.confirmations.sessions_for_backend(),
        )
        .query(reference, task_id, operation)
        .await
    }
}

/// Historical, authenticated read. No Runtime, provider, permit or confirmation
/// configuration is required to recover an already persisted original.
pub struct PaymentOriginalReader {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
}
impl PaymentOriginalReader {
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
        task_id: &TaskId,
        operation: &OperationId,
    ) -> Result<PaymentOriginalResult, StorageError> {
        valid_id(task_id.as_str())?;
        valid_id(operation.as_str())?;
        let session = self
            .sessions
            .resolve_binding(reference)
            .map_err(|_| StorageError::Session)?;
        let guard = || {
            if self
                .sessions
                .resolve_binding(reference)
                .map_err(|_| StorageError::Session)?
                == session
            {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(
            &guard,
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx),
        )
        .await?;
        let result = guarded(&guard, async {
            let row = sqlx::query("SELECT * FROM tasks WHERE owner_id=$1 AND task_id=$2")
                .bind(session.user_id.as_str())
                .bind(task_id.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StorageError::NotFound)?;
            let task = task_row(&row)?;
            crate::lifecycle::check_mode(&mut tx, task_id, true).await?;
            let ops = operations(&mut tx, task_id).await?;
            crate::lifecycle::project_progress(&mut tx, &task, &ops).await?;
            let saved = saved_payment(&mut tx, &task, operation, false).await?;
            recover_locked(&mut tx, &task, &saved.op, &saved.snapshot).await
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        Ok(result)
    }
}

impl PaymentExecutionBinding {
    fn from_saved(op: &OperationRecord, snapshot: &PaymentSnapshot) -> Result<Self, StorageError> {
        Ok(Self {
            preparation_operation_id: op.operation_id.clone(),
            preparation_plan_version: op.plan_version,
            preparation_arguments_hash: hash(snapshot)?,
            business_operation_id: snapshot.transfer.operation_id.clone(),
            business_draft_version: snapshot.transfer.draft_version,
            normalized_arguments: snapshot.transfer.clone(),
            transfer_arguments_hash: hash(&snapshot.transfer)?,
            execution_operation_id: None,
            execution_plan_version: None,
            execution_tool_name: None,
            idempotency_key: None,
            operation_fingerprint: None,
        })
    }

    fn bind_public(
        &mut self,
        task: &TaskRecord,
        request: &ExecutionRequest,
    ) -> Result<(), StorageError> {
        // bound_request reuses complete payload hashing/fingerprinting, public
        // request validation, resource ID and BUSINESS version comparisons.
        let transfer = crate::transfer_result::bound_request(request)?;
        let binding = &request.binding.snapshot;
        if binding.user_id != task.owner_id
            || binding.task_id != task.task_id
            || binding.operation_id == self.preparation_operation_id
            || binding.tool_name.as_str() == TOOL
            || transfer != self.normalized_arguments
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        self.execution_operation_id = Some(binding.operation_id.clone());
        self.execution_plan_version = Some(binding.plan_version);
        self.execution_tool_name = Some(binding.tool_name.clone());
        self.idempotency_key = Some(request.idempotency_key.clone());
        self.operation_fingerprint = Some(request.binding.fingerprint.clone());
        Ok(())
    }
}

// Called only with a trusted saved preparation, or existing isolated contract tests.
pub(crate) async fn recover_locked(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    op: &OperationRecord,
    snapshot: &PaymentSnapshot,
) -> Result<PaymentOriginalResult, StorageError> {
    let mut result = PaymentOriginalResult {
        contract_version: 1,
        scope: "PAYMENT_ORIGINAL_RESULT_ONLY",
        task_id: task.task_id.clone(),
        binding: PaymentExecutionBinding::from_saved(op, snapshot)?,
        state: PaymentOriginalState::ReconcileRequired,
        reason: "ORIGINAL_SOURCES_INCOMPLETE",
        first_public_submission: None,
        public_submitted_at: None,
        verified_business_result: None,
        funds_execution_enabled: false,
        public_sync_enabled: false,
        execution_link: None,
    };
    let expected = &snapshot.transfer;
    let registered: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM payment_execution_links WHERE preparation_operation_id=$1)",
    )
    .bind(op.operation_id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    if registered {
        let saved = saved_payment(tx, task, &op.operation_id, false).await?;
        result.execution_link = link::read_link(tx, task, &saved).await?;
    }
    let public_row = sqlx::query(
        "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2",
    )
    .bind(task.owner_id.as_str())
    .bind(expected.operation_id.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    let public = match public_row {
        Some(row) => match crate::execution_request::stored_row(&row) {
            Ok(stored) => {
                if result.binding.bind_public(task, &stored.request).is_err() {
                    result.state = PaymentOriginalState::SourceConflict;
                    result.reason = "PUBLIC_PAYMENT_PROJECTION_CONFLICT";
                    return Ok(result);
                }
                if let Some(first) = &stored.first_submission {
                    result.first_public_submission = Some(first.operation.clone());
                    result.public_submitted_at = Some(first.recorded_at);
                }
                match crate::execution_request::view_locked(tx, task, stored).await {
                    Ok(view) => Some(view),
                    Err(StorageError::CorruptRecord | StorageError::NotFound) => {
                        result.state = PaymentOriginalState::SourceConflict;
                        result.reason = "PUBLIC_CURRENT_OPERATION_CONFLICT";
                        return Ok(result);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(StorageError::CorruptRecord) => {
                result.state = PaymentOriginalState::SourceConflict;
                result.reason = "PUBLIC_ORIGINAL_RECORD_CONFLICT";
                return Ok(result);
            }
            Err(error) => return Err(error),
        },
        None => None,
    };
    let original_key: Option<String> = sqlx::query_scalar("SELECT original_key FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2")
        .bind(task.owner_id.as_str()).bind(expected.operation_id.as_str()).fetch_optional(&mut **tx).await?;
    if let Some(key) = original_key {
        let key = IdempotencyKey::parse(key).map_err(|_| StorageError::CorruptRecord)?;
        if result
            .binding
            .idempotency_key
            .as_ref()
            .is_some_and(|k| k != &key)
        {
            result.state = PaymentOriginalState::SourceConflict;
            result.reason = "ORIGINAL_KEY_CONFLICT";
            return Ok(result);
        }
        let record =
            crate::transfer_result::verified_business_record(tx, &task.owner_id, expected, &key)
                .await;
        match record {
            Ok(record) => {
                result.binding.idempotency_key = Some(key);
                result.verified_business_result = Some(record);
            }
            Err(
                StorageError::CorruptRecord
                | StorageError::PermitBindingMismatch
                | StorageError::ExecutionKeyConflict,
            ) => {
                result.state = PaymentOriginalState::SourceConflict;
                result.reason = "BUSINESS_PROJECTION_OR_LEDGER_CONFLICT";
                return Ok(result);
            }
            Err(StorageError::ReconcileRequired | StorageError::NotFound) => return Ok(result),
            Err(error) => return Err(error),
        }
    }
    let (draft_exists, submitted, funds_exist, ledger_exists): (bool, bool, bool, bool) =
        sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2),
        EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 AND submitted),
        EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE owner_id=$1 AND operation_id=$3),
        EXISTS(SELECT 1 FROM transfer_ledger_v1 WHERE business_owner_id=$1 AND operation_id=$3)",
        )
        .bind(task.owner_id.as_str())
        .bind(expected.draft_id.as_str())
        .bind(expected.operation_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
    match (public, &result.verified_business_result) {
        (None, None)
            if result.execution_link.is_some()
                && draft_exists
                && !submitted
                && !funds_exist
                && !ledger_exists =>
        {
            result.state = PaymentOriginalState::RegisteredUnsubmitted;
            result.reason = "METADATA_REGISTERED_WITHOUT_SUBMISSION";
        }
        (None, None) if draft_exists && !submitted && !funds_exist && !ledger_exists => {
            result.state = PaymentOriginalState::Unsubmitted;
            result.reason = "NO_SUBMISSION_IN_CURRENT_SNAPSHOT";
        }
        (Some(public), Some(record)) if public.first_submission.is_some() => {
            if matches!(
                public.current_operation.state,
                OperationState::Succeeded | OperationState::Failed
            ) && !matches!(
                (public.current_operation.state, record.latest_receipt.status),
                (OperationState::Succeeded, ToolResultStatus::Succeeded)
                    | (OperationState::Failed, ToolResultStatus::Failed)
            ) {
                result.state = PaymentOriginalState::SourceConflict;
                result.reason = "PUBLIC_BUSINESS_TERMINAL_CONFLICT";
                return Ok(result);
            }
            result.state = match record.latest_receipt.status {
                ToolResultStatus::Processing => PaymentOriginalState::Processing,
                ToolResultStatus::Succeeded => PaymentOriginalState::Succeeded,
                ToolResultStatus::Failed => PaymentOriginalState::Failed,
            };
            result.reason = "VERIFIED_ORIGINAL_BUSINESS_RESULT";
        }
        (Some(_), None) => result.reason = "PUBLIC_ORIGINAL_WITHOUT_BUSINESS_RESULT",
        (None, Some(_)) => result.reason = "BUSINESS_RESULT_WITHOUT_PUBLIC_ORIGINAL",
        _ => {}
    }
    Ok(result)
}
