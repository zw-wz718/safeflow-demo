//! Module-local read adapter for a transaction supplied by a trusted ledger.
//! Missing card IDs remain missing. Account-wide lock facts are separate from
//! the card used for a historical transaction, and never authorize spending.

use crate::account_service::{
    AccountCardFacts, AccountDataSource, AccountQueryIssue, AccountQueryService,
};
use crate::CardFacts;

/// Caller must load this reference and identity from trusted services, not an
/// LLM or a tail-number search. This type does not authenticate a transaction.
#[derive(Clone, Copy, Debug)]
pub struct TrustedTransactionReference<'a> {
    pub transaction_id: &'a str,
    pub account_id: Option<&'a str>,
    pub card_id: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionCardFacts {
    pub transaction_id: String,
    /// Current account facts, not the lock state when the transaction occurred.
    pub account: AccountCardFacts,
    /// None means the ledger did not supply a card reference; no card is guessed.
    pub referenced_card: Option<CardFacts>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionReferenceIssue {
    MissingTransactionReference,
    MissingAccountReference,
    InvalidReference,
    AccountQuery(AccountQueryIssue),
    /// Missing, foreign and wrong-account card IDs have the same outward result.
    CardResourceUnavailable,
}

pub struct TransactionCardQueryService<S> {
    accounts: AccountQueryService<S>,
}

impl<S: AccountDataSource> TransactionCardQueryService<S> {
    pub fn new(source: S) -> Self {
        Self {
            accounts: AccountQueryService::new(source),
        }
    }

    pub fn query(
        &self,
        trusted_user_id: &str,
        reference: TrustedTransactionReference<'_>,
    ) -> Result<TransactionCardFacts, TransactionReferenceIssue> {
        use TransactionReferenceIssue as Issue;
        if reference.transaction_id.trim().is_empty() {
            return Err(Issue::MissingTransactionReference);
        }
        let account_id = reference
            .account_id
            .filter(|id| !id.trim().is_empty())
            .ok_or(Issue::MissingAccountReference)?;
        // Do not trim IDs or silently turn a malformed ID into a missing one.
        if !valid_id(reference.transaction_id)
            || !valid_id(account_id)
            || reference.card_id.is_some_and(|id| !valid_id(id))
        {
            return Err(Issue::InvalidReference);
        }
        let account = self
            .accounts
            .query(trusted_user_id, Some(account_id))
            .map_err(Issue::AccountQuery)?;
        let referenced_card = reference
            .card_id
            .map(|id| {
                account
                    .cards
                    .iter()
                    .find(|card| card.card_id == id)
                    .cloned()
                    .ok_or(Issue::CardResourceUnavailable)
            })
            .transpose()?;
        Ok(TransactionCardFacts {
            transaction_id: reference.transaction_id.to_owned(),
            account,
            referenced_card,
        })
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}
