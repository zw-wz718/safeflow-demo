//! Session-scoped reads of the received simulated-bank SQL; no funds execution entry.
use crate::models::{db_version, valid_id};
use crate::repository::guarded;
use crate::{PgRepository, StorageError};
use safeflow_contracts::{ExecutionContext, OperationId, ResourceId, SessionRef};
use safeflow_runtime::{SessionBinding, SessionResolver};
use safeflow_transfer_integration::{
    business_store::BusinessRecord,
    funds_store::{FundsError, FundsStore},
    mock_execution::{MockAccountFunds, MockTransferRequest, MockTransferTransaction},
    StoredDraft,
};
use sqlx::{Postgres, Transaction};
use std::sync::Arc;

pub const TRANSFER_SQL_SOURCE_REVISION: &str = "1f0e46db7d08051140d42ac195af5f207f2d7062";

/// A read snapshot, not an authorization, lock retained through execution, or wire DTO.
pub struct TransferDraftProjection {
    pub request: MockTransferRequest,
    pub runtime_operation_id: OperationId,
    pub runtime_plan_version: u32,
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
pub struct TransferSqlReader {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
}

impl TransferSqlReader {
    /// Construct with the same repository and versioned session source as the API.
    /// No sample-owner aliases, account provisioning or plugin registration occur.
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            repository,
            sessions,
        }
    }

    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        valid_id(session.as_str())?;
        let identity = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(identity.user_id.as_str())?;
        db_version(identity.binding_version)?;
        Ok(identity)
    }

    fn guard(&self, session: &SessionRef, identity: &SessionBinding) -> Result<(), StorageError> {
        if self.identity(session)? != *identity {
            return Err(StorageError::Session);
        }
        Ok(())
    }

    async fn begin(
        &self,
        session: &SessionRef,
    ) -> Result<(SessionBinding, Transaction<'static, Postgres>), StorageError> {
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(
            &guard,
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx),
        )
        .await?;
        Ok((identity, tx))
    }

    pub async fn draft(
        &self,
        session: &SessionRef,
        id: &ResourceId,
    ) -> Result<StoredDraft, StorageError> {
        valid_id(id.as_str())?;
        let (identity, mut tx) = self.begin(session).await?;
        let guard = || self.guard(session, &identity);
        let draft = guarded(&guard, async {
            FundsStore::draft(&mut tx, &identity.user_id, id)
                .await
                .map_err(business_error)
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        Ok(draft)
    }

    /// Business operation ID only. Absence is not evidence of no debit, and no
    /// public Operation is advanced or inferred from the business receipt status.
    pub async fn business(
        &self,
        session: &SessionRef,
        id: &OperationId,
    ) -> Result<Option<BusinessRecord>, StorageError> {
        valid_id(id.as_str())?;
        let (identity, mut tx) = self.begin(session).await?;
        let guard = || self.guard(session, &identity);
        let record = guarded(&guard, async {
            FundsStore::query(&mut tx, &identity.user_id, id)
                .await
                .map_err(business_error)
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        Ok(record)
    }

    pub async fn funds(
        &self,
        session: &SessionRef,
        account: &ResourceId,
    ) -> Result<MockAccountFunds, StorageError> {
        valid_id(account.as_str())?;
        let (identity, mut tx) = self.begin(session).await?;
        let guard = || self.guard(session, &identity);
        let funds = guarded(&guard, async {
            FundsStore::funds(&mut tx, &identity.user_id, account)
                .await
                .map_err(business_error)
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        Ok(funds)
    }

    pub async fn transactions(
        &self,
        session: &SessionRef,
    ) -> Result<Vec<MockTransferTransaction>, StorageError> {
        let (identity, mut tx) = self.begin(session).await?;
        let guard = || self.guard(session, &identity);
        let rows = guarded(&guard, async {
            FundsStore::transactions(&mut tx, &identity.user_id)
                .await
                .map_err(business_error)
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        Ok(rows)
    }

    /// Read actual public task/operation and current business draft in one snapshot.
    /// Business version comes from transfer_drafts_v1, not Runtime plan_version.
    /// Actual submission must re-lock/recheck the draft in the permit transaction;
    /// this read does not prove unsubmitted status, final rules, or funding authority.
    pub async fn project_draft(
        &self,
        session: &SessionRef,
        runtime_operation: &OperationId,
        draft_id: &ResourceId,
        expected_business_version: u32,
    ) -> Result<TransferDraftProjection, StorageError> {
        valid_id(runtime_operation.as_str())?;
        valid_id(draft_id.as_str())?;
        let (identity, mut tx) = self.begin(session).await?;
        let guard = || self.guard(session, &identity);
        let context = guarded(&guard, async {
            use sqlx::Row;
            let row = sqlx::query("SELECT t.task_id,t.plan_version,o.node_id FROM operations o JOIN tasks t USING(task_id) WHERE o.operation_id=$1 AND t.owner_id=$2")
                .bind(runtime_operation.as_str()).bind(identity.user_id.as_str())
                .fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
            Ok::<_, StorageError>(ExecutionContext {
                task_id: crate::repository::decode(&row, "task_id")?,
                node_id: crate::repository::decode(&row, "node_id")?,
                operation_id: runtime_operation.clone(),
                authenticated_user_id: identity.user_id.clone(),
                plan_version: u32::try_from(row.try_get::<i64, _>("plan_version")?).map_err(|_| StorageError::CorruptRecord)?,
                idempotency_key: None,
            })
        }).await?;
        let draft = guarded(&guard, async {
            FundsStore::draft(&mut tx, &identity.user_id, draft_id)
                .await
                .map_err(business_error)
        })
        .await?;
        let checked_at: chrono::DateTime<chrono::Utc> = guarded(
            &guard,
            sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(&mut *tx),
        )
        .await?;
        let now = u64::try_from(checked_at.timestamp()).map_err(|_| StorageError::CorruptRecord)?;
        let request = MockTransferRequest::from_trusted_draft(
            &context,
            &draft,
            expected_business_version,
            now,
        )
        .map_err(|_| StorageError::ConfirmationBindingMismatch)?;
        guarded(&guard, tx.commit()).await?;
        Ok(TransferDraftProjection {
            request,
            runtime_operation_id: runtime_operation.clone(),
            runtime_plan_version: context.plan_version,
            checked_at,
        })
    }
}

pub(crate) fn business_error(error: FundsError) -> StorageError {
    match error {
        FundsError::NotFound => StorageError::NotFound,
        FundsError::Database(_)
        | FundsError::Receipt(
            safeflow_transfer_integration::business_store::StoreError::Database(_),
        ) => StorageError::Database,
        _ => StorageError::CorruptRecord,
    }
}
