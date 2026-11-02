//! Read-only association of a saved feedback, original transaction, historical
//! lock operation and separately versioned current account/card facts.
//! No confirmation, freeze execution, result synthesis or retry is performed.

use crate::account_service::AccountDataSource;
use crate::operation_records::{
    HistoricalLockReceipt, LockOperationRecordSource, OperationHistoryQueryService,
    OperationReadIssue, StoredLockOperation, StoredReasonCode,
};
use crate::saved_feedback::{
    SavedFeedbackCardFacts, SavedFeedbackCardQueryService, SavedFeedbackDataSource,
    SavedFeedbackQueryIssue,
};
use crate::LockSource;

/// Historical action and current read facts intentionally remain separate.
/// This does not mean the original lock is still active or a public operation
/// has a verified SUCCEEDED state. Source versions are not a global transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackOperationFacts {
    pub original_operation: HistoricalLockReceipt,
    pub current: SavedFeedbackCardFacts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackOperationIssue {
    Feedback(SavedFeedbackQueryIssue),
    Operation(OperationReadIssue),
    AssociationMismatch,
    SnapshotChanged,
}

pub struct SavedFeedbackOperationQueryService<F, S, O> {
    feedback: SavedFeedbackCardQueryService<F, S>,
    operations: OperationHistoryQueryService<O>,
}

impl<F: SavedFeedbackDataSource, S: AccountDataSource, O: LockOperationRecordSource>
    SavedFeedbackOperationQueryService<F, S, O>
{
    pub fn new(feedback: F, cards: S, operations: O) -> Self {
        Self {
            feedback: SavedFeedbackCardQueryService::new(feedback, cards),
            operations: OperationHistoryQueryService::new(operations),
        }
    }

    /// The authenticated backend supplies the owner and selected references.
    /// These strings do not authenticate the caller or approve a freeze.
    pub fn query(
        &self,
        trusted_user_id: &str,
        feedback_id: Option<&str>,
        original_operation_id: Option<&str>,
    ) -> Result<FeedbackOperationFacts, FeedbackOperationIssue> {
        use FeedbackOperationIssue as Issue;
        // Do not search for a "latest" operation when the original ID is absent.
        check_original_reference(original_operation_id)?;
        let before = self
            .feedback
            .query(trusted_user_id, feedback_id)
            .map_err(Issue::Feedback)?;
        let (operation, receipt) = self
            .operations
            .read_checked(trusted_user_id, original_operation_id)
            .map_err(Issue::Operation)?;
        // Matching a card alone is insufficient: a receipt for another saved
        // feedback or transaction must never become this feedback's result.
        check_feedback_association(&operation, &before)?;
        // Reuse the established complete-read, ownership and revision checks
        // after fetching history. Never combine an old feedback/card snapshot
        // with history fetched while those facts changed.
        let after = self
            .feedback
            .query(trusted_user_id, feedback_id)
            .map_err(Issue::Feedback)?;
        checked_feedback_operation(before, after, receipt)
    }
}

pub(crate) fn check_original_reference(
    original_operation_id: Option<&str>,
) -> Result<(), FeedbackOperationIssue> {
    if !original_operation_id.is_some_and(|id| !id.trim().is_empty()) {
        return Err(FeedbackOperationIssue::Operation(
            OperationReadIssue::InvalidReference,
        ));
    }
    Ok(())
}

pub(crate) fn check_feedback_association(
    operation: &StoredLockOperation,
    before: &SavedFeedbackCardFacts,
) -> Result<(), FeedbackOperationIssue> {
    if operation.source != LockSource::AnomalyDispute
        || operation.reason != StoredReasonCode::NonOwnerTransaction
        || operation.feedback_id.as_deref() != Some(before.feedback_id.as_str())
        || operation.transaction_id.as_deref() != Some(before.transaction_id.as_str())
        || operation.card_id != before.card.card_id
    {
        return Err(FeedbackOperationIssue::AssociationMismatch);
    }
    Ok(())
}

pub(crate) fn checked_feedback_operation(
    before: SavedFeedbackCardFacts,
    after: SavedFeedbackCardFacts,
    receipt: HistoricalLockReceipt,
) -> Result<FeedbackOperationFacts, FeedbackOperationIssue> {
    if before != after {
        return Err(FeedbackOperationIssue::SnapshotChanged);
    }
    // An active lock with the SAME stable ID must still agree with the
    // historical lock's card and fixed public source reason. Different IDs
    // may share a reason; a released lock need not appear in active facts.
    // These public projections cannot verify hidden events/released rows.
    if after.account.cards.iter().any(|card| {
        card.active_locks.iter().any(|lock| {
            lock.id == receipt.lock_id
                && (card.card_id != receipt.card_id || lock.reason != receipt.public_reason)
        })
    }) {
        return Err(FeedbackOperationIssue::AssociationMismatch);
    }
    Ok(FeedbackOperationFacts {
        original_operation: receipt,
        current: after,
    })
}
