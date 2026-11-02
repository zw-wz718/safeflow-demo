//! Public original-request metadata only; no bank receipts or execution authority.
use crate::confirmation::current_binding;
use crate::confirmation_repository::legacy_binding_row;
use crate::models::{db_version, hash, valid_id, validate_checkpoint};
use crate::repository::{audit, decode, guarded, lock_task, operations, version};
use crate::{
    OperationRecord, PermitBinding, PersistentAuditKind, PgRepository, StorageError, TaskRecord,
};
use chrono::{DateTime, Utc};
use safeflow_contracts::{
    compute_operation_fingerprint, BusinessOperationRef, FingerprintPayloadV1, IdempotencyKey,
    OperationState, SessionRef, TaskState,
};
use safeflow_runtime::{SessionBinding, SessionResolver};
use sqlx::{postgres::PgRow, types::Json, Postgres, Row, Transaction};
use std::sync::Arc;

pub const PUBLIC_EXECUTION_STORAGE_VERSION: u16 = 1;

/// Backend comparison command. Resource and business version come from a trusted
/// domain store. Storage verifies the public binding, not that domain provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionRequest {
    pub idempotency_key: IdempotencyKey,
    pub business_operation_ref: BusinessOperationRef,
    pub business_version: u64,
    pub binding: PermitBinding,
    pub payload: FingerprintPayloadV1,
}

/// A frozen public SUBMITTED snapshot, never a bank's success/failure receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicSubmissionReceipt {
    pub operation: OperationRecord,
    pub recorded_at: DateTime<Utc>,
}

/// Backend-only view. The current Operation comes from the existing public table;
/// business result details must be read by the domain adapter along the original ID.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionRequestView {
    pub request: ExecutionRequest,
    pub reserved_at: DateTime<Utc>,
    pub first_submission: Option<PublicSubmissionReceipt>,
    pub current_operation: OperationRecord,
}

#[derive(Clone)]
pub struct ExecutionRequestStore {
    pub(crate) repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
}

impl ExecutionRequestStore {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            repository,
            sessions,
        }
    }

    /// Reserve metadata, without consuming a Permit or invoking any business code.
    /// Identical existing requests return history even after their versions expire.
    pub async fn reserve(
        &self,
        session: &SessionRef,
        request: &ExecutionRequest,
    ) -> Result<ExecutionRequestView, StorageError> {
        request.validate()?;
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(
            &guard,
            crate::daily_usage::protect_owner(&mut tx, &identity.user_id),
        )
        .await?;
        let task = guarded(
            &guard,
            lock_task(
                &mut tx,
                &identity.user_id,
                &request.binding.snapshot.task_id,
            ),
        )
        .await?;
        if request.binding.snapshot.user_id != identity.user_id {
            return Err(StorageError::NotFound);
        }
        let stored = guarded(&guard, reserve_locked(&mut tx, &task, request)).await?;
        let view = guarded(&guard, view_locked(&mut tx, &task, stored)).await?;
        guarded(&guard, tx.commit()).await?;
        Ok(view)
    }

    /// Current read authorization, not the original execution session/Permit.
    pub async fn by_key(
        &self,
        session: &SessionRef,
        key: &IdempotencyKey,
    ) -> Result<ExecutionRequestView, StorageError> {
        valid_id(key.as_str())?;
        self.read(session, key.as_str(), false).await
    }

    pub async fn by_business_operation(
        &self,
        session: &SessionRef,
        original: &BusinessOperationRef,
    ) -> Result<ExecutionRequestView, StorageError> {
        valid_id(original.as_str())?;
        self.read(session, original.as_str(), true).await
    }

    /// Read-only full original-request comparison. None means metadata reserved,
    /// not submitted; it must not be presented as a successful bank receipt.
    pub async fn replay(
        &self,
        session: &SessionRef,
        request: &ExecutionRequest,
    ) -> Result<Option<PublicSubmissionReceipt>, StorageError> {
        request.validate()?;
        let view = self.by_key(session, &request.idempotency_key).await?;
        compare(&view.request, request)?;
        Ok(view.first_submission)
    }

    pub(crate) fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        valid_id(session.as_str())?;
        let identity = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(identity.user_id.as_str())?;
        db_version(identity.binding_version)?;
        Ok(identity)
    }

    pub(crate) fn guard(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
    ) -> Result<(), StorageError> {
        if &self.identity(session)? != identity {
            return Err(StorageError::Session);
        }
        Ok(())
    }

    async fn read(
        &self,
        session: &SessionRef,
        original: &str,
        business: bool,
    ) -> Result<ExecutionRequestView, StorageError> {
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        // Locate only the owner's immutable mapping, then take the common task-first
        // lock before reading the receipt and current Operation as one snapshot.
        let query = if business {
            "SELECT task_id FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2"
        } else {
            "SELECT task_id FROM public_execution_requests WHERE owner_id=$1 AND idempotency_key=$2"
        };
        let row = guarded(
            &guard,
            sqlx::query(query)
                .bind(identity.user_id.as_str())
                .bind(original)
                .fetch_optional(&mut *tx),
        )
        .await?
        .ok_or(StorageError::NotFound)?;
        let task_id = decode(&row, "task_id")?;
        let task = guarded(&guard, lock_task(&mut tx, &identity.user_id, &task_id)).await?;
        let query = if business {
            "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND business_operation_ref=$2"
        } else {
            "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND idempotency_key=$2"
        };
        let row = guarded(
            &guard,
            sqlx::query(query)
                .bind(identity.user_id.as_str())
                .bind(original)
                .fetch_optional(&mut *tx),
        )
        .await?
        .ok_or(StorageError::NotFound)?;
        let view = guarded(&guard, view_locked(&mut tx, &task, stored_row(&row)?)).await?;
        guarded(&guard, tx.commit()).await?;
        Ok(view)
    }
}

impl ExecutionRequest {
    pub(crate) fn validate(&self) -> Result<(), StorageError> {
        let b = &self.binding.snapshot;
        for id in [
            self.idempotency_key.as_str(),
            self.business_operation_ref.as_str(),
            b.user_id.as_str(),
            b.task_id.as_str(),
            b.operation_id.as_str(),
            b.node_id.as_str(),
            b.tool_name.as_str(),
            self.payload.resource_id.as_str(),
        ] {
            valid_id(id)?;
        }
        db_version(self.business_version)?;
        db_version(b.task_revision)?;
        db_version(b.operation_revision)?;
        if b.facts_version == 0
            || b.facts_version > u32::MAX as u64
            || b.lifecycle_anchor.is_some()
            || self.payload.user_id != b.user_id
            || self.payload.operation_id != b.operation_id
            || self.payload.tool_name != b.tool_name
            || self.payload.plan_version != b.plan_version
            || hash(&self.payload.normalized_arguments)? != b.arguments_hash
            || compute_operation_fingerprint(&self.payload)
                .map_err(|_| StorageError::PermitBindingMismatch)?
                != self.binding.fingerprint
        {
            return Err(StorageError::PermitBindingMismatch);
        }
        let bytes = serde_json::to_vec(&self.payload)
            .map_err(|_| StorageError::Invalid("execution payload"))?;
        if bytes.len() > 32768 {
            return Err(StorageError::Invalid("execution payload too large"));
        }
        Ok(())
    }

    pub(crate) fn digest(&self) -> Result<String, StorageError> {
        Ok(hash(&(
            "safeflow-public-execution-request:v1",
            &self.idempotency_key,
            &self.business_operation_ref,
            self.business_version,
            &self.binding.snapshot,
            &self.binding.fingerprint,
            &self.payload,
        ))?
        .as_str()
        .to_owned())
    }
}

pub(crate) struct StoredRequest {
    pub request: ExecutionRequest,
    pub(crate) reserved_at: DateTime<Utc>,
    pub first_submission: Option<PublicSubmissionReceipt>,
}

pub(crate) async fn request_locked(
    tx: &mut Transaction<'_, Postgres>,
    request: &ExecutionRequest,
) -> Result<StoredRequest, StorageError> {
    let row = sqlx::query(
        "SELECT * FROM public_execution_requests WHERE owner_id=$1 AND idempotency_key=$2",
    )
    .bind(request.binding.snapshot.user_id.as_str())
    .bind(request.idempotency_key.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::NotFound)?;
    let stored = stored_row(&row)?;
    compare(&stored.request, request)?;
    Ok(stored)
}

fn compare(stored: &ExecutionRequest, expected: &ExecutionRequest) -> Result<(), StorageError> {
    if stored != expected {
        return Err(StorageError::ExecutionKeyConflict);
    }
    Ok(())
}

pub(crate) async fn reserve_locked(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    request: &ExecutionRequest,
) -> Result<StoredRequest, StorageError> {
    request.validate()?;
    if task.owner_id != request.binding.snapshot.user_id
        || task.task_id != request.binding.snapshot.task_id
    {
        return Err(StorageError::NotFound);
    }
    let ops = operations(tx, &task.task_id).await?;
    if !ops
        .iter()
        .any(|op| op.operation_id == request.binding.snapshot.operation_id)
    {
        return Err(StorageError::NotFound);
    }
    let conflicts = sqlx::query("SELECT * FROM public_execution_requests WHERE (owner_id=$1 AND (idempotency_key=$2 OR business_operation_ref=$3)) OR operation_id=$4")
        .bind(task.owner_id.as_str()).bind(request.idempotency_key.as_str()).bind(request.business_operation_ref.as_str()).bind(request.binding.snapshot.operation_id.as_str()).fetch_all(&mut **tx).await?;
    if !conflicts.is_empty() {
        return existing(&conflicts, request);
    }
    if task.state != TaskState::Running
        || !ops.iter().any(|op| {
            op.operation_id == request.binding.snapshot.operation_id
                && op.state == OperationState::Ready
        })
        || current_binding(task, &ops, &request.binding.snapshot.operation_id)?
            != request.binding.snapshot
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    let b = &request.binding.snapshot;
    let inserted = sqlx::query("INSERT INTO public_execution_requests (contract_version,owner_id,idempotency_key,business_operation_ref,business_version,task_id,operation_id,node_id,tool_name,task_revision,operation_revision,plan_version,facts_version,arguments_hash,operation_fingerprint,fingerprint_payload,request_digest) VALUES (1,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) ON CONFLICT DO NOTHING")
        .bind(b.user_id.as_str()).bind(request.idempotency_key.as_str()).bind(request.business_operation_ref.as_str()).bind(db_version(request.business_version)?)
        .bind(b.task_id.as_str()).bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str()).bind(db_version(b.task_revision)?)
        .bind(db_version(b.operation_revision)?).bind(b.plan_version as i64).bind(b.facts_version as i64).bind(b.arguments_hash.as_str()).bind(request.binding.fingerprint.as_str()).bind(Json(&request.payload)).bind(request.digest()?).execute(&mut **tx).await?;
    if inserted.rows_affected() != 1 {
        let rows = sqlx::query("SELECT * FROM public_execution_requests WHERE (owner_id=$1 AND (idempotency_key=$2 OR business_operation_ref=$3)) OR operation_id=$4")
            .bind(b.user_id.as_str()).bind(request.idempotency_key.as_str()).bind(request.business_operation_ref.as_str()).bind(b.operation_id.as_str()).fetch_all(&mut **tx).await?;
        return existing(&rows, request);
    }
    audit(tx, task, ops.iter().find(|op| op.operation_id == b.operation_id), PersistentAuditKind::ExecutionRequestReserved,
        serde_json::json!({"request_digest":request.digest()?,"operation_fingerprint":request.binding.fingerprint})).await?;
    request_locked(tx, request).await
}

fn existing(rows: &[PgRow], request: &ExecutionRequest) -> Result<StoredRequest, StorageError> {
    for row in rows {
        let stored = stored_row(row)?;
        if stored.request.binding.snapshot.user_id == request.binding.snapshot.user_id
            && stored.request.idempotency_key == request.idempotency_key
        {
            compare(&stored.request, request)?;
            return Ok(stored);
        }
    }
    Err(StorageError::ExecutionOperationConflict)
}

pub(crate) fn stored_row(row: &PgRow) -> Result<StoredRequest, StorageError> {
    if row.try_get::<i16, _>("contract_version")? != PUBLIC_EXECUTION_STORAGE_VERSION as i16 {
        return Err(StorageError::CorruptRecord);
    }
    let mut request = ExecutionRequest {
        idempotency_key: decode(row, "idempotency_key")?,
        business_operation_ref: decode(row, "business_operation_ref")?,
        business_version: version(row, "business_version")?,
        binding: PermitBinding {
            snapshot: legacy_binding_row(row)?,
            fingerprint: decode(row, "operation_fingerprint")?,
        },
        payload: row
            .try_get::<Json<FingerprintPayloadV1>, _>("fingerprint_payload")?
            .0,
    };
    request.binding.snapshot.lifecycle_anchor = row
        .try_get::<Option<String>, _>("lifecycle_anchor")
        .unwrap_or(None)
        .map(safeflow_contracts::ArgumentsHash::parse)
        .transpose()
        .map_err(|_| StorageError::CorruptRecord)?;
    if request.binding.snapshot.lifecycle_anchor.is_some() {
        crate::permit::payment::validate_request(&request)
            .map_err(|_| StorageError::CorruptRecord)?;
    } else {
        request
            .validate()
            .map_err(|_| StorageError::CorruptRecord)?;
    }
    if request.digest()? != row.try_get::<String, _>("request_digest")? {
        return Err(StorageError::CorruptRecord);
    }
    let first = row
        .try_get::<Option<Json<OperationRecord>>, _>("first_operation")
        .map_err(|_| StorageError::CorruptRecord)?;
    let submitted_at = row.try_get::<Option<DateTime<Utc>>, _>("submitted_at")?;
    let first_submission = match (first, submitted_at) {
        (None, None) => None,
        (Some(first), Some(recorded_at)) => {
            let op = first.0;
            check_original(&request, &op)?;
            if op.state != OperationState::Submitted
                || Some(op.revision) != request.binding.snapshot.operation_revision.checked_add(1)
                || op.facts_version != request.binding.snapshot.facts_version
                || op.business_operation_ref.as_ref() != Some(&request.business_operation_ref)
            {
                return Err(StorageError::CorruptRecord);
            }
            Some(PublicSubmissionReceipt {
                operation: op,
                recorded_at,
            })
        }
        _ => return Err(StorageError::CorruptRecord),
    };
    Ok(StoredRequest {
        request,
        reserved_at: row.try_get("reserved_at")?,
        first_submission,
    })
}

fn check_original(request: &ExecutionRequest, op: &OperationRecord) -> Result<(), StorageError> {
    let b = &request.binding.snapshot;
    if op.operation_id != b.operation_id
        || op.task_id != b.task_id
        || op.node_id != b.node_id
        || op.plan_version != b.plan_version
        || op.arguments_hash != b.arguments_hash
        || op.facts_version < b.facts_version
        || op.revision < b.operation_revision
        || op
            .business_operation_ref
            .as_ref()
            .is_some_and(|r| r != &request.business_operation_ref)
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}

pub(crate) async fn view_locked(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    stored: StoredRequest,
) -> Result<ExecutionRequestView, StorageError> {
    if stored.request.binding.snapshot.lifecycle_anchor.is_some() {
        return crate::permit::payment::view_locked(tx, task, stored).await;
    }
    if task.owner_id != stored.request.binding.snapshot.user_id
        || task.task_id != stored.request.binding.snapshot.task_id
    {
        return Err(StorageError::NotFound);
    }
    let ops = operations(tx, &task.task_id).await?;
    validate_checkpoint(task, &ops)?;
    let op = ops
        .into_iter()
        .find(|op| op.operation_id == stored.request.binding.snapshot.operation_id)
        .ok_or(StorageError::CorruptRecord)?;
    check_original(&stored.request, &op)?;
    if let Some(first) = &stored.first_submission {
        if op.revision < first.operation.revision
            || !matches!(
                op.state,
                OperationState::Submitted
                    | OperationState::Processing
                    | OperationState::Succeeded
                    | OperationState::Failed
            )
            || op.business_operation_ref.as_ref() != Some(&stored.request.business_operation_ref)
        {
            return Err(StorageError::CorruptRecord);
        }
    } else if matches!(
        op.state,
        OperationState::Submitted
            | OperationState::Processing
            | OperationState::Succeeded
            | OperationState::Failed
    ) {
        return Err(StorageError::CorruptRecord);
    }
    Ok(ExecutionRequestView {
        request: stored.request,
        reserved_at: stored.reserved_at,
        first_submission: stored.first_submission,
        current_operation: op,
    })
}
