//! Business-parameter checking for an existing operation's history only.
//!
//! This does not execute/replay a freeze, register retry keys, authenticate a
//! principal, check the public confirmation fingerprint or grant a permit.

use crate::operation_records::{
    HistoricalLockReceipt, LockOperationRecordSource, OperationHistoryQueryService,
    OperationReadIssue, StoredLockOperation, StoredReasonCode,
};
use crate::LockSource;

/// Current business fields supplied by the backend, NOT proof of authority.
/// Public task/plan/policy versions and trusted principal binding still belong
/// to the foundation's execution boundary, which is not implemented here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockReplayParameters {
    pub card_id: String,
    pub source: LockSource,
    pub reason: StoredReasonCode,
    pub event_id: Option<String>,
    pub transaction_id: Option<String>,
    pub feedback_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayReadIssue {
    InvalidParameters,
    Read(OperationReadIssue),
    ParameterConflict,
    /// This read never invents a new alias or decides its public adoption.
    RetryKeyNotRecorded,
}

pub struct OperationReplayQueryService<S> {
    history: OperationHistoryQueryService<S>,
}

impl<S: LockOperationRecordSource> OperationReplayQueryService<S> {
    pub fn new(source: S) -> Self {
        Self {
            history: OperationHistoryQueryService::new(source),
        }
    }

    /// Return the original historical receipt only when all compared business
    /// fields match AND this retry key is already associated with that operation.
    /// Exactly one consistent provider read is used for validation/comparison.
    /// Success is not authorization or evidence that a lock remains active.
    pub fn query_existing(
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
            .map_err(ReplayReadIssue::Read)?;
        matching_receipt(original, receipt, retry_key, expected)
    }
}

pub(crate) fn checked_retry_key<'a>(
    retry_key: Option<&'a str>,
    expected: &LockReplayParameters,
) -> Result<&'a str, ReplayReadIssue> {
    let retry_key = retry_key
        .filter(|key| !key.trim().is_empty())
        .ok_or(ReplayReadIssue::InvalidParameters)?;
    if expected.card_id.trim().is_empty()
        || [
            expected.event_id.as_deref(),
            expected.transaction_id.as_deref(),
            expected.feedback_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| id.trim().is_empty())
    {
        return Err(ReplayReadIssue::InvalidParameters);
    }
    Ok(retry_key)
}

pub(crate) fn matching_receipt(
    original: StoredLockOperation,
    receipt: HistoricalLockReceipt,
    retry_key: &str,
    expected: &LockReplayParameters,
) -> Result<HistoricalLockReceipt, ReplayReadIssue> {
    if original.card_id != expected.card_id
        || original.source != expected.source
        || original.reason != expected.reason
        || original.event_id != expected.event_id
        || original.transaction_id != expected.transaction_id
        || original.feedback_id != expected.feedback_id
    {
        return Err(ReplayReadIssue::ParameterConflict);
    }
    if !original.retry_keys.iter().any(|key| key == retry_key) {
        return Err(ReplayReadIssue::RetryKeyNotRecorded);
    }
    Ok(receipt)
}
