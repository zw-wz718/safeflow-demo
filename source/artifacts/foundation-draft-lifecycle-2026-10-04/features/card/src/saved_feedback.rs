//! Read-only CARD-03 boundary over feedback already saved by a trusted provider.
//! This neither saves feedback nor authorizes, executes or retries a freeze.
use crate::account_service::{AccountCardFacts, AccountDataSource};
use crate::service::SourceUnavailable;
use crate::transaction_reference::{
    TransactionCardFacts, TransactionCardQueryService, TransactionReferenceIssue,
    TrustedTransactionReference,
};
use crate::CardFacts;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedFeedbackKind {
    NonOwnerTransaction,
    Other,
}

/// Provider-owned stored record, not a model/client confirmation form.
#[derive(Clone, Debug)]
pub struct SavedFeedback {
    pub feedback_id: String,
    pub user_id: String,
    pub transaction_id: String,
    pub kind: SavedFeedbackKind,
}

#[derive(Clone, Debug)]
pub struct FeedbackTransaction {
    pub transaction_id: String,
    pub user_id: String,
    pub account_id: Option<String>,
    pub card_id: Option<String>,
}

/// One complete, consistently versioned feedback/original-transaction snapshot.
#[derive(Clone, Debug)]
pub struct SavedFeedbackSnapshot {
    pub revision: String,
    pub feedback: SavedFeedback,
    pub transaction: FeedbackTransaction,
}

pub trait SavedFeedbackDataSource {
    /// Read an already saved feedback and its original transaction together.
    /// Missing or foreign feedback is None; failed/incomplete reads are errors.
    /// A real provider must supply an authenticated scope and a complete snapshot.
    /// Never manufacture a stored record from chat text or a saved=true flag.
    fn read_saved_snapshot(
        &self,
        trusted_user_id: &str,
        feedback_id: &str,
    ) -> Result<Option<SavedFeedbackSnapshot>, SourceUnavailable>;

    /// Recheck ownership/existence after reading card facts. Revision must change
    /// on any relevant feedback/transaction change, including change-and-restore.
    fn read_current_revision(
        &self,
        trusted_user_id: &str,
        feedback_id: &str,
    ) -> Result<String, SourceUnavailable>;
}

/// Facts for later server-side review; never an approval or execution permit.
/// Fields intentionally exclude raw feedback text and internal risk evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFeedbackCardFacts {
    pub feedback_id: String,
    pub transaction_id: String,
    pub feedback_revision: String,
    pub account: AccountCardFacts,
    pub card: CardFacts,
}

/// Module-local failures, not a frozen C-CARD/C-BILL or HTTP error contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedFeedbackQueryIssue {
    ResourceUnavailable,
    LookupUnavailable,
    InconsistentData,
    FeedbackNotApplicable,
    MissingCardReference,
    SnapshotChanged,
    TransactionReference(TransactionReferenceIssue),
}

pub struct SavedFeedbackCardQueryService<F, S> {
    feedback: F,
    cards: TransactionCardQueryService<S>,
}

impl<F: SavedFeedbackDataSource, S: AccountDataSource> SavedFeedbackCardQueryService<F, S> {
    pub fn new(feedback: F, cards: S) -> Self {
        Self {
            feedback,
            cards: TransactionCardQueryService::new(cards),
        }
    }

    pub fn query(
        &self,
        trusted_user_id: &str,
        feedback_id: Option<&str>,
    ) -> Result<SavedFeedbackCardFacts, SavedFeedbackQueryIssue> {
        use SavedFeedbackQueryIssue as Issue;
        let id = checked_feedback_reference(trusted_user_id, feedback_id)?;
        let snapshot = self
            .feedback
            .read_saved_snapshot(trusted_user_id, id)
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
            .map_err(|_| Issue::LookupUnavailable)?;
        checked_feedback_card_facts(snapshot, facts, revision)
    }
}

pub(crate) fn checked_feedback_reference<'a>(
    trusted_user_id: &str,
    feedback_id: Option<&'a str>,
) -> Result<&'a str, SavedFeedbackQueryIssue> {
    let id = feedback_id
        .filter(|id| valid_id(id))
        .ok_or(SavedFeedbackQueryIssue::ResourceUnavailable)?;
    if !valid_id(trusted_user_id) {
        return Err(SavedFeedbackQueryIssue::ResourceUnavailable);
    }
    Ok(id)
}

pub(crate) fn check_saved_snapshot(
    trusted_user_id: &str,
    feedback_id: &str,
    snapshot: &SavedFeedbackSnapshot,
) -> Result<(), SavedFeedbackQueryIssue> {
    use SavedFeedbackQueryIssue as Issue;
    // Ownership precedes malformed-record diagnostics in both read paths.
    if snapshot.feedback.user_id != trusted_user_id
        || snapshot.transaction.user_id != trusted_user_id
    {
        return Err(Issue::ResourceUnavailable);
    }
    if snapshot.revision.trim().is_empty()
        || snapshot.feedback.feedback_id != feedback_id
        || !valid_id(&snapshot.feedback.transaction_id)
        || snapshot.feedback.transaction_id != snapshot.transaction.transaction_id
    {
        return Err(Issue::InconsistentData);
    }
    if snapshot.feedback.kind != SavedFeedbackKind::NonOwnerTransaction {
        return Err(Issue::FeedbackNotApplicable);
    }
    // Missing card remains missing; never guess by account/card count or tail.
    if snapshot.transaction.card_id.is_none() {
        return Err(Issue::MissingCardReference);
    }
    Ok(())
}

pub(crate) fn checked_feedback_card_facts(
    snapshot: SavedFeedbackSnapshot,
    facts: TransactionCardFacts,
    revision: String,
) -> Result<SavedFeedbackCardFacts, SavedFeedbackQueryIssue> {
    if revision != snapshot.revision {
        return Err(SavedFeedbackQueryIssue::SnapshotChanged);
    }
    Ok(SavedFeedbackCardFacts {
        feedback_id: snapshot.feedback.feedback_id,
        transaction_id: snapshot.transaction.transaction_id,
        feedback_revision: snapshot.revision,
        account: facts.account,
        card: facts
            .referenced_card
            .ok_or(SavedFeedbackQueryIssue::MissingCardReference)?,
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}
