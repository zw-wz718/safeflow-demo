//! CARD-03 association with staged or async feedback/original-operation reads.
//! Current-card providers retain the existing synchronous interface.
//! No database, public registration, freeze, permission or retry is introduced.
use crate::account_service::AccountDataSource;
use crate::feedback_operation::{
    check_feedback_association, check_original_reference, checked_feedback_operation,
    FeedbackOperationFacts, FeedbackOperationIssue,
};
use crate::operation_records_async::{
    AsyncLockOperationRecordSource, AsyncOperationHistoryQueryService,
};
use crate::saved_feedback::{SavedFeedbackCardQueryService, SavedFeedbackDataSource};
use crate::saved_feedback_async::{
    AsyncSavedFeedbackCardQueryService, AsyncSavedFeedbackDataSource,
};

pub struct AsyncSavedFeedbackOperationQueryService<F, S, O> {
    feedback: SavedFeedbackCardQueryService<F, S>,
    operations: AsyncOperationHistoryQueryService<O>,
}

/// Composed async feedback + history path. The earlier staged entry remains
/// available; no provider conversion, public registration or default is added.
pub struct AsyncFeedbackAndOperationQueryService<F, S, O> {
    feedback: AsyncSavedFeedbackCardQueryService<F, S>,
    operations: AsyncOperationHistoryQueryService<O>,
}

impl<F: AsyncSavedFeedbackDataSource, S: AccountDataSource, O: AsyncLockOperationRecordSource>
    AsyncFeedbackAndOperationQueryService<F, S, O>
{
    pub fn new(feedback: F, cards: S, operations: O) -> Self {
        Self {
            feedback: AsyncSavedFeedbackCardQueryService::new(feedback, cards),
            operations: AsyncOperationHistoryQueryService::new(operations),
        }
    }

    /// Await feedback snapshot/version, then ONE original record, then feedback
    /// snapshot/version again. Shared rules keep history separate from current
    /// facts and reject changed or mismatched associations. Card reads remain
    /// synchronous and separately versioned; this is not a global transaction.
    /// Backend identity is required; this does not authorize or execute a lock.
    pub async fn query(
        &self,
        trusted_user_id: &str,
        feedback_id: Option<&str>,
        original_operation_id: Option<&str>,
    ) -> Result<FeedbackOperationFacts, FeedbackOperationIssue> {
        use FeedbackOperationIssue as Issue;
        check_original_reference(original_operation_id)?;
        let before = self
            .feedback
            .query(trusted_user_id, feedback_id)
            .await
            .map_err(Issue::Feedback)?;
        let (operation, receipt) = self
            .operations
            .read_checked(trusted_user_id, original_operation_id)
            .await
            .map_err(Issue::Operation)?;
        check_feedback_association(&operation, &before)?;
        let after = self
            .feedback
            .query(trusted_user_id, feedback_id)
            .await
            .map_err(Issue::Feedback)?;
        checked_feedback_operation(before, after, receipt)
    }
}

impl<F: SavedFeedbackDataSource, S: AccountDataSource, O: AsyncLockOperationRecordSource>
    AsyncSavedFeedbackOperationQueryService<F, S, O>
{
    pub fn new(feedback: F, cards: S, operations: O) -> Self {
        Self {
            feedback: SavedFeedbackCardQueryService::new(feedback, cards),
            operations: AsyncOperationHistoryQueryService::new(operations),
        }
    }

    /// Await ONE validated original record, then re-read complete current
    /// feedback/card facts. Shared synchronous rules reject changed snapshots
    /// and conflicting visible lock references. Versions do not prove a global
    /// transaction; authenticated identity must still come from the backend.
    pub async fn query(
        &self,
        trusted_user_id: &str,
        feedback_id: Option<&str>,
        original_operation_id: Option<&str>,
    ) -> Result<FeedbackOperationFacts, FeedbackOperationIssue> {
        use FeedbackOperationIssue as Issue;
        check_original_reference(original_operation_id)?;
        let before = self
            .feedback
            .query(trusted_user_id, feedback_id)
            .map_err(Issue::Feedback)?;
        let (operation, receipt) = self
            .operations
            .read_checked(trusted_user_id, original_operation_id)
            .await
            .map_err(Issue::Operation)?;
        check_feedback_association(&operation, &before)?;
        let after = self
            .feedback
            .query(trusted_user_id, feedback_id)
            .map_err(Issue::Feedback)?;
        checked_feedback_operation(before, after, receipt)
    }
}
