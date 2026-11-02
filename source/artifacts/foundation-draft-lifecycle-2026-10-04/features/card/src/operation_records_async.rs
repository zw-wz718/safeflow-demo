//! Module-local async seam for reading an already stored original operation.
//! It shares the existing validator; it does not authenticate the owner, choose
//! SQL tables, open connections, grant permission, retry, or perform writes.
use std::{future::Future, pin::Pin};

use crate::operation_records::{
    checked_operation_id, validate_bundle, HistoricalLockReceipt, OperationReadIssue,
    StoredLockOperation, StoredOperationBundle,
};
use crate::service::SourceUnavailable;

pub type OperationRecordReadFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<StoredOperationBundle>, SourceUnavailable>> + Send + 'a>,
>;

/// Trusted backend provider; enforce owner scope and return ONE consistent
/// joined bundle. None means a complete scoped lookup found no stored record;
/// SQL errors, missing joined rows and uncertain/partial reads must return Err.
/// Borrowed futures allow async providers without forcing a pool or executor
/// into the business module. This is not a frozen public storage/HTTP contract.
pub trait AsyncLockOperationRecordSource {
    fn read_operation_record<'a>(
        &'a self,
        trusted_user_id: &'a str,
        original_operation_id: &'a str,
    ) -> OperationRecordReadFuture<'a>;
}

pub struct AsyncOperationHistoryQueryService<S> {
    source: S,
}

impl<S: AsyncLockOperationRecordSource> AsyncOperationHistoryQueryService<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    /// Identity must come from the authenticated backend. One awaited read,
    /// no cache/fallback/retry. The receipt describes the ORIGINAL operation;
    /// a missing record does not prove the lock was never applied, and a saved
    /// receipt does not prove the lock is active now or authorize a new action.
    pub async fn query(
        &self,
        trusted_user_id: &str,
        operation_id: Option<&str>,
    ) -> Result<HistoricalLockReceipt, OperationReadIssue> {
        self.read_checked(trusted_user_id, operation_id)
            .await
            .map(|(_, receipt)| receipt)
    }

    /// Share ONE validated lookup with the async business-parameter comparator.
    /// Original private fields remain internal to the crate.
    pub(crate) async fn read_checked(
        &self,
        trusted_user_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(StoredLockOperation, HistoricalLockReceipt), OperationReadIssue> {
        let operation_id = checked_operation_id(trusted_user_id, operation_id)?;
        let bundle = self
            .source
            .read_operation_record(trusted_user_id, operation_id)
            .await
            .map_err(|_| OperationReadIssue::LookupUnavailable)?
            .ok_or(OperationReadIssue::ResourceUnavailable)?;
        let original = bundle.operation.clone();
        let receipt = validate_bundle(trusted_user_id, operation_id, bundle)?;
        Ok((original, receipt))
    }
}
