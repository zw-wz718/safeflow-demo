//! Module-local storage read models and original-operation history lookup.
//!
//! No database driver, migration, write entry, authorization or bank call is
//! implemented here. Providers supply already stored, consistent records. The
//! checks detect broken associations; they cannot prove a provider is truthful.
//! A historical receipt is NOT current card status or execution permission.

use std::collections::HashSet;

use crate::service::SourceUnavailable;
use crate::LockSource;

/// Existing reason codes, separate from fictional principal/evidence types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredReasonCode {
    UserReportLost,
    UserSecurityConcern,
    NonOwnerTransaction,
    RiskControlFlag,
}

impl StoredReasonCode {
    fn matches(self, source: LockSource) -> bool {
        matches!(
            (source, self),
            (
                LockSource::UserSelf,
                Self::UserReportLost | Self::UserSecurityConcern
            ) | (LockSource::AnomalyDispute, Self::NonOwnerTransaction)
                | (LockSource::RiskControl, Self::RiskControlFlag)
        )
    }
}

/// What the original operation recorded, not the lock's effectiveness now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginalLockEffect {
    Created,
    ReusedActive,
}

/// Internal storage projection. No client deserialization is provided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLockOperation {
    pub record_id: String,
    pub owner_id: String,
    pub operation_id: String,
    pub card_id: String,
    pub source: LockSource,
    pub reason: StoredReasonCode,
    pub event_id: Option<String>,
    pub transaction_id: Option<String>,
    pub feedback_id: Option<String>,
    pub original_key: String,
    /// Includes the original key. Alias adoption remains a public-contract gap.
    pub retry_keys: Vec<String>,
}

/// Identity of the referenced lock. Current state is read through the existing
/// card/account fact service, never inferred from an old operation receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLockIdentity {
    pub id: String,
    pub card_id: String,
    pub source: LockSource,
    pub event_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReasonHistory {
    pub id: String,
    pub lock_id: String,
    pub operation_record_id: String,
    pub reason: StoredReasonCode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLockReceipt {
    pub lock_id: String,
    pub operation_record_id: String,
    pub reason_history_id: String,
    pub source: LockSource,
    pub original_effect: OriginalLockEffect,
}

/// A consistent joined read of the ONE requested operation, its original
/// receipt, its reason history and the referenced lock identity. This is not a
/// proposed table layout, nor a complete account lock collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOperationBundle {
    pub operation: StoredLockOperation,
    pub lock: StoredLockIdentity,
    pub reason_history: StoredReasonHistory,
    pub receipt: StoredLockReceipt,
}

/// Trusted backend dependency. Enforce the requested owner's scope in storage.
/// None requires a successful, complete lookup proving no such scoped record;
/// missing joined rows, SQL failure, partial reads and unknown results are Err.
/// Reads must not create locks, retry aliases, reasons or public operations.
/// This local synchronous boundary follows existing card read providers; it is
/// not a frozen SQLx/public Runtime interface. An async database adapter is TBD.
pub trait LockOperationRecordSource {
    fn read_operation_record(
        &self,
        trusted_user_id: &str,
        operation_id: &str,
    ) -> Result<Option<StoredOperationBundle>, SourceUnavailable>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationReadIssue {
    InvalidReference,
    /// Missing and foreign-owner records share the outward result.
    ResourceUnavailable,
    LookupUnavailable,
    InconsistentData,
}

/// Checked ORIGINAL operation fact only. Does not expose evidence IDs, retry
/// keys, raw reasons or risk internals. It does not say "currently frozen".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoricalLockReceipt {
    pub operation_id: String,
    pub card_id: String,
    pub operation_record_id: String,
    pub lock_id: String,
    pub reason_history_id: String,
    pub source: LockSource,
    pub public_reason: &'static str,
    pub original_effect: OriginalLockEffect,
}

pub struct OperationHistoryQueryService<S> {
    source: S,
}

impl<S: LockOperationRecordSource> OperationHistoryQueryService<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    /// Identity is injected by the authenticated backend. This method does not
    /// authenticate it. A missing record cannot prove the freeze never ran;
    /// reconcile the ORIGINAL public operation, without creating a new one.
    pub fn query(
        &self,
        trusted_user_id: &str,
        operation_id: Option<&str>,
    ) -> Result<HistoricalLockReceipt, OperationReadIssue> {
        self.read_checked(trusted_user_id, operation_id)
            .map(|(_, receipt)| receipt)
    }

    /// Share one checked storage read with the business-parameter comparator.
    /// The original operation stays internal and is never a public response.
    pub(crate) fn read_checked(
        &self,
        trusted_user_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(StoredLockOperation, HistoricalLockReceipt), OperationReadIssue> {
        let operation_id = checked_operation_id(trusted_user_id, operation_id)?;
        let bundle = self
            .source
            .read_operation_record(trusted_user_id, operation_id)
            .map_err(|_| OperationReadIssue::LookupUnavailable)?
            .ok_or(OperationReadIssue::ResourceUnavailable)?;
        let original = bundle.operation.clone();
        let receipt = validate_bundle(trusted_user_id, operation_id, bundle)?;
        Ok((original, receipt))
    }
}

pub(crate) fn checked_operation_id<'a>(
    trusted_user_id: &str,
    operation_id: Option<&'a str>,
) -> Result<&'a str, OperationReadIssue> {
    let operation_id = operation_id
        .filter(|id| nonempty(id))
        .ok_or(OperationReadIssue::InvalidReference)?;
    if !nonempty(trusted_user_id) {
        return Err(OperationReadIssue::InvalidReference);
    }
    Ok(operation_id)
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(nonempty)
}

pub(crate) fn validate_bundle(
    trusted_user_id: &str,
    requested_operation_id: &str,
    bundle: StoredOperationBundle,
) -> Result<HistoricalLockReceipt, OperationReadIssue> {
    use OperationReadIssue as Issue;
    let StoredOperationBundle {
        operation,
        lock,
        reason_history,
        receipt,
    } = bundle;
    // Reject a foreign owner before checking/reporting their record details.
    if operation.owner_id != trusted_user_id {
        return Err(Issue::ResourceUnavailable);
    }
    if operation.operation_id != requested_operation_id
        || [
            operation.record_id.as_str(),
            operation.card_id.as_str(),
            operation.original_key.as_str(),
            lock.id.as_str(),
            reason_history.id.as_str(),
        ]
        .iter()
        .any(|id| !nonempty(id))
        || !operation.reason.matches(operation.source)
    {
        return Err(Issue::InconsistentData);
    }
    let evidence_shape_matches = match operation.source {
        LockSource::UserSelf => {
            operation.event_id.is_none()
                && operation.transaction_id.is_none()
                && operation.feedback_id.is_none()
        }
        LockSource::AnomalyDispute => {
            present(&operation.event_id)
                && present(&operation.transaction_id)
                && present(&operation.feedback_id)
        }
        LockSource::RiskControl => {
            present(&operation.event_id)
                && operation.transaction_id.is_none()
                && operation.feedback_id.is_none()
        }
    };
    let mut keys = HashSet::new();
    if !evidence_shape_matches
        || !operation
            .retry_keys
            .iter()
            .all(|key| nonempty(key) && keys.insert(key.as_str()))
        || !keys.contains(operation.original_key.as_str())
        || lock.card_id != operation.card_id
        || lock.source != operation.source
        || lock.event_id != operation.event_id
        || receipt.lock_id != lock.id
        || receipt.operation_record_id != operation.record_id
        || receipt.reason_history_id != reason_history.id
        || receipt.source != operation.source
        || reason_history.lock_id != lock.id
        || reason_history.operation_record_id != operation.record_id
        || reason_history.reason != operation.reason
    {
        return Err(Issue::InconsistentData);
    }
    Ok(HistoricalLockReceipt {
        operation_id: operation.operation_id,
        card_id: operation.card_id,
        operation_record_id: operation.record_id,
        lock_id: lock.id,
        reason_history_id: reason_history.id,
        source: operation.source,
        public_reason: operation.source.public_reason(),
        original_effect: receipt.original_effect,
    })
}
