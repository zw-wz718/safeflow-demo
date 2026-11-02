//! Async original-operation business comparison. No freeze replay, key adoption,
//! public fingerprint validation, authentication, permit or database writes.
use crate::operation_records::HistoricalLockReceipt;
use crate::operation_records_async::{
    AsyncLockOperationRecordSource, AsyncOperationHistoryQueryService,
};
use crate::operation_replay::{
    checked_retry_key, matching_receipt, LockReplayParameters, ReplayReadIssue,
};

pub struct AsyncOperationReplayQueryService<S> {
    history: AsyncOperationHistoryQueryService<S>,
}

impl<S: AsyncLockOperationRecordSource> AsyncOperationReplayQueryService<S> {
    pub fn new(source: S) -> Self {
        Self {
            history: AsyncOperationHistoryQueryService::new(source),
        }
    }

    /// Await one consistent stored bundle, then use the SAME checks as the
    /// synchronous service. Only an already recorded key and identical business
    /// fields return the original historical receipt. Neither input nor success
    /// provides public confirmation, current lock facts, or execution authority.
    pub async fn query_existing(
        &self,
        trusted_user_id: &str,
        operation_id: Option<&str>,
        retry_key: Option<&str>,
        expected: &LockReplayParameters,
    ) -> Result<HistoricalLockReceipt, ReplayReadIssue> {
        let retry_key = checked_retry_key(retry_key, expected)?;
        let (original, receipt) = self
            .history
            .read_checked(trusted_user_id, operation_id)
            .await
            .map_err(ReplayReadIssue::Read)?;
        matching_receipt(original, receipt, retry_key, expected)
    }
}
