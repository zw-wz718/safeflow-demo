//! Await already-saved feedback/transaction and its current revision; card
//! reads still use the existing synchronous interface. No save/auth/freeze.
use crate::account_service::{AccountDataSource, AccountPage, AccountQueryIssue};
use crate::saved_feedback::{
    check_saved_snapshot, checked_feedback_card_facts, checked_feedback_reference,
    SavedFeedbackCardFacts, SavedFeedbackQueryIssue, SavedFeedbackSnapshot,
};
use crate::service::SourceUnavailable;
use crate::transaction_reference::{
    TransactionCardQueryService, TransactionReferenceIssue, TrustedTransactionReference,
};
use std::{future::Future, pin::Pin, sync::Arc};

pub type FeedbackReadFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SourceUnavailable>> + Send + 'a>>;

/// Trusted provider of one complete saved feedback/original transaction snapshot.
/// None is a successful complete scoped lookup finding no visible saved record;
/// incomplete rows, SQL failures and uncertainty are Err. Revision rechecks
/// must verify owner/existence and change on relevant changes, including ABA.
/// No method saves feedback from chat or accepts a client saved=true claim.
pub trait AsyncSavedFeedbackDataSource {
    fn read_saved_snapshot<'a>(
        &'a self,
        trusted_user_id: &'a str,
        feedback_id: &'a str,
    ) -> FeedbackReadFuture<'a, Option<SavedFeedbackSnapshot>>;
    fn read_current_revision<'a>(
        &'a self,
        trusted_user_id: &'a str,
        feedback_id: &'a str,
    ) -> FeedbackReadFuture<'a, String>;
}

struct SharedCards<S>(Arc<S>);
impl<S: AccountDataSource> AccountDataSource for SharedCards<S> {
    fn read_page(
        &self,
        user: &str,
        account: &str,
        cursor: Option<&str>,
        revision: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        self.0.read_page(user, account, cursor, revision)
    }
    fn read_current_revision(
        &self,
        user: &str,
        account: &str,
    ) -> Result<String, SourceUnavailable> {
        self.0.read_current_revision(user, account)
    }
}

pub struct AsyncSavedFeedbackCardQueryService<F, S> {
    feedback: F,
    cards: TransactionCardQueryService<SharedCards<S>>,
    card_source: Arc<S>,
}
impl<F: AsyncSavedFeedbackDataSource, S: AccountDataSource>
    AsyncSavedFeedbackCardQueryService<F, S>
{
    pub fn new(feedback: F, cards: S) -> Self {
        let card_source = Arc::new(cards);
        Self {
            feedback,
            cards: TransactionCardQueryService::new(SharedCards(card_source.clone())),
            card_source,
        }
    }

    /// Reuse synchronous feedback validation, with awaited snapshot/revision.
    /// Recheck the same card source's ownership/version after the final await.
    /// This closes that wait window but does not prove a global transaction.
    /// No cache, fallback, retry, pool, database or public registration is added.
    pub async fn query(
        &self,
        trusted_user_id: &str,
        feedback_id: Option<&str>,
    ) -> Result<SavedFeedbackCardFacts, SavedFeedbackQueryIssue> {
        use SavedFeedbackQueryIssue as Issue;
        let id = checked_feedback_reference(trusted_user_id, feedback_id)?;
        let snapshot = self
            .feedback
            .read_saved_snapshot(trusted_user_id, id)
            .await
            .map_err(|_| Issue::LookupUnavailable)?
            .ok_or(Issue::ResourceUnavailable)?;
        check_saved_snapshot(trusted_user_id, id, &snapshot)?;
        let facts = self
            .cards
            .query(
                trusted_user_id,
                TrustedTransactionReference {
                    transaction_id: &snapshot.transaction.transaction_id,
                    account_id: snapshot.transaction.account_id.as_deref(),
                    card_id: snapshot.transaction.card_id.as_deref(),
                },
            )
            .map_err(Issue::TransactionReference)?;
        let revision = self
            .feedback
            .read_current_revision(trusted_user_id, id)
            .await
            .map_err(|_| Issue::LookupUnavailable)?;
        let facts = checked_feedback_card_facts(snapshot, facts, revision)?;
        // The feedback revision await must not hide a card/ownership change
        // after the prior card read. Use the SAME provider, not a cached copy.
        let current_card_revision = self
            .card_source
            .read_current_revision(trusted_user_id, &facts.account.account_id)
            .map_err(|_| card_read_issue(AccountQueryIssue::LookupUnavailable))?;
        if current_card_revision != facts.account.revision {
            return Err(card_read_issue(AccountQueryIssue::SnapshotChanged));
        }
        Ok(facts)
    }
}

fn card_read_issue(issue: AccountQueryIssue) -> SavedFeedbackQueryIssue {
    SavedFeedbackQueryIssue::TransactionReference(TransactionReferenceIssue::AccountQuery(issue))
}
