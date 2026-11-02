//! Opt-in business receipt repository; not a funds execution/authorization entry.
//!
//! Owner identity and receipt provenance MUST come from the trusted backend.
//! Writes borrow the connection of the caller's ALREADY OPEN SQL transaction.
//! The trusted caller owns BEGIN/COMMIT/ROLLBACK and must propagate every error.
//! With foundation contract 28, consume the permit before borrowing connection().
//! Never pass an autocommit connection or treat these methods as authorization.
//! Shared account SQL, draft persistence and public permit/state/audit/Outbox
//! assembly still require the corresponding business and foundation adapters.
use crate::mock_execution::{
    amount_cents, validate_request, MockTransferReceipt, MockTransferRequest,
};
use safeflow_contracts::{IdempotencyKey, OperationId, ToolResultStatus, UserId};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgRow, PgConnection, Row};

pub const PROPOSED_SCHEMA: &str = include_str!("../sql/business_orders_v1.proposed.sql");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessRecord {
    pub request: MockTransferRequest,
    pub first_receipt: MockTransferReceipt,
    pub latest_receipt: MockTransferReceipt,
}

#[derive(Debug)]
pub enum StoreError {
    InvalidRecord,
    IdempotencyConflict,
    OperationConflict,
    ResultConflict,
    NotFound,
    Database(sqlx::Error),
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No connection details or full business parameters in public error text.
        f.write_str(match self {
            Self::InvalidRecord => "INVALID_BUSINESS_RECORD",
            Self::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            Self::OperationConflict => "OPERATION_CONFLICT",
            Self::ResultConflict => "RESULT_CONFLICT",
            Self::NotFound => "BUSINESS_RECORD_NOT_FOUND",
            Self::Database(_) => "BUSINESS_STORAGE_UNAVAILABLE",
        })
    }
}
impl std::error::Error for StoreError {}
impl From<sqlx::Error> for StoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

pub struct BusinessStore;
impl BusinessStore {
    /// Save already-verified business facts in the CALLER'S transaction. This
    /// method neither settles funds nor proves that its receipts came from a bank.
    pub async fn record_submission(
        tx: &mut PgConnection,
        owner: &UserId,
        key: &IdempotencyKey,
        request: &MockTransferRequest,
        first: &MockTransferReceipt,
        latest: &MockTransferReceipt,
    ) -> Result<BusinessRecord, StoreError> {
        validate_request(owner, key, request).map_err(|_| StoreError::InvalidRecord)?;
        // Transaction-scoped locks close the check/insert race; unique indexes
        // are a backstop. Sorted order also covers same-key/different-operation.
        let mut locks = [
            serde_json::json!([
                "transfer-business-operation-v1",
                owner,
                request.operation_id
            ])
            .to_string(),
            serde_json::json!(["transfer-business-key-v1", owner, key]).to_string(),
        ];
        locks.sort();
        for lock in locks {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                .bind(lock)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(row) = sqlx::query("SELECT original_key,request,first_receipt,latest_receipt FROM transfer_business_orders_v1 WHERE owner_id=$1 AND original_key=$2 FOR UPDATE")
            .bind(owner.as_str()).bind(key.as_str()).fetch_optional(&mut *tx).await? {
            let record = decode(&row, owner)?;
            if record.request != *request { return Err(StoreError::IdempotencyConflict); }
            if record.first_receipt != *first { return Err(StoreError::ResultConflict); }
            // Historical submission replay NEVER overwrites the latest result.
            return Ok(record);
        }
        if Self::query(&mut *tx, owner, &request.operation_id)
            .await?
            .is_some()
        {
            return Err(StoreError::OperationConflict);
        }
        validate_pair(request, first, latest)?;
        sqlx::query("INSERT INTO transfer_business_orders_v1 (owner_id,operation_id,original_key,request,first_receipt,latest_receipt) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(owner.as_str()).bind(request.operation_id.as_str()).bind(key.as_str())
            .bind(json(request)?).bind(json(first)?).bind(json(latest)?)
            .execute(&mut *tx).await?;
        Ok(BusinessRecord {
            request: request.clone(),
            first_receipt: first.clone(),
            latest_receipt: latest.clone(),
        })
    }

    /// Scoped historical read. None is not evidence of "no money was deducted".
    /// SQL failure is an error, never a bank FAILED receipt. No writes or bank calls.
    pub async fn query(
        connection: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
    ) -> Result<Option<BusinessRecord>, StoreError> {
        let row = sqlx::query("SELECT original_key,request,first_receipt,latest_receipt FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2")
            .bind(owner.as_str()).bind(operation.as_str()).fetch_optional(connection).await?;
        row.map(|row| {
            let record = decode(&row, owner)?;
            if record.request.operation_id != *operation {
                return Err(StoreError::InvalidRecord);
            }
            Ok(record)
        })
        .transpose()
    }

    /// Apply a trusted later bank fact, not a customer-selected outcome or timer.
    /// Same result is idempotent; a terminal result cannot be rewritten/regressed.
    pub async fn observe_result(
        tx: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
        latest: &MockTransferReceipt,
    ) -> Result<BusinessRecord, StoreError> {
        let row = sqlx::query("SELECT original_key,request,first_receipt,latest_receipt FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2 FOR UPDATE")
            .bind(owner.as_str()).bind(operation.as_str()).fetch_optional(&mut *tx).await?
            .ok_or(StoreError::NotFound)?;
        let mut record = decode(&row, owner)?;
        if record.request.operation_id != *operation {
            return Err(StoreError::InvalidRecord);
        }
        if record.latest_receipt == *latest {
            return Ok(record);
        }
        if record.latest_receipt.status != ToolResultStatus::Processing
            || latest.status == ToolResultStatus::Processing
        {
            return Err(StoreError::ResultConflict);
        }
        validate_pair(&record.request, &record.first_receipt, latest)?;
        sqlx::query("UPDATE transfer_business_orders_v1 SET latest_receipt=$3,latest_recorded_at=clock_timestamp() WHERE owner_id=$1 AND operation_id=$2")
            .bind(owner.as_str()).bind(operation.as_str()).bind(json(latest)?)
            .execute(&mut *tx).await?;
        record.latest_receipt = latest.clone();
        Ok(record)
    }
}

fn json(value: &impl Serialize) -> Result<serde_json::Value, StoreError> {
    serde_json::to_value(value).map_err(|_| StoreError::InvalidRecord)
}
pub(crate) fn decode(row: &PgRow, owner: &UserId) -> Result<BusinessRecord, StoreError> {
    let request =
        serde_json::from_value(row.try_get("request")?).map_err(|_| StoreError::InvalidRecord)?;
    let first_receipt = serde_json::from_value(row.try_get("first_receipt")?)
        .map_err(|_| StoreError::InvalidRecord)?;
    let latest_receipt = serde_json::from_value(row.try_get("latest_receipt")?)
        .map_err(|_| StoreError::InvalidRecord)?;
    let key = IdempotencyKey::parse(row.try_get::<String, _>("original_key")?)
        .map_err(|_| StoreError::InvalidRecord)?;
    validate_request(owner, &key, &request).map_err(|_| StoreError::InvalidRecord)?;
    validate_pair(&request, &first_receipt, &latest_receipt)?;
    Ok(BusinessRecord {
        request,
        first_receipt,
        latest_receipt,
    })
}

fn validate_pair(
    request: &MockTransferRequest,
    first: &MockTransferReceipt,
    latest: &MockTransferReceipt,
) -> Result<(), StoreError> {
    if request.draft_version == 0
        || amount_cents(&request.amount).map_err(|_| StoreError::InvalidRecord)? == 0
    {
        return Err(StoreError::InvalidRecord);
    }
    for receipt in [first, latest] {
        let matches_request = receipt.proposal_version == 1
            && receipt.task_id == request.task_id
            && receipt.operation_id == request.operation_id
            && receipt.draft_id == request.draft_id
            && receipt.draft_version == request.draft_version
            && receipt.payer_account_id == request.payer_account_id
            && receipt.payee_id == request.payee_id
            && receipt.amount == request.amount
            && receipt.currency == request.currency;
        let status_valid = match receipt.status {
            ToolResultStatus::Succeeded => {
                receipt.reason_code.is_none()
                    && receipt
                        .completed_at
                        .as_deref()
                        .is_some_and(|time| chrono::DateTime::parse_from_rfc3339(time).is_ok())
            }
            ToolResultStatus::Failed | ToolResultStatus::Processing => {
                receipt.completed_at.is_none()
                    && receipt.reason_code.as_deref().is_some_and(|reason| {
                        !reason.is_empty()
                            && reason.len() <= 128
                            && !reason.chars().any(char::is_control)
                    })
            }
        };
        if !matches_request || !status_valid {
            return Err(StoreError::InvalidRecord);
        }
    }
    if first.status != ToolResultStatus::Processing && first != latest {
        return Err(StoreError::ResultConflict);
    }
    Ok(())
}
