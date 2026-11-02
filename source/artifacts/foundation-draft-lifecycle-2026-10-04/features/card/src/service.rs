//! Module-local read service. These types are not a frozen foundation contract.
//!
//! The foundation must supply the authenticated user and selected stable card ID.
//! This service neither authenticates that input nor grants any write permission.

use crate::{query_card, CardFacts, QueryIssue, Snapshot};

/// A transport or storage failure. Details must not become public card data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceUnavailable;

/// Trusted, read-only data provider for the requested card's ownership and locks.
///
/// Return one consistent snapshot containing the card, its owning account and
/// every associated lock. A partial page or failed lookup must be Unavailable,
/// never Complete with an empty or truncated list. Providers must not create or
/// release locks as a side effect of this read.
///
/// Real database/network adapters and snapshot consistency are not implemented
/// here. Returning a Snapshot does not establish ownership: the service checks it.
pub trait CardDataSource {
    fn read_snapshot(
        &self,
        trusted_user_id: &str,
        trusted_card_id: &str,
    ) -> Result<Snapshot, SourceUnavailable>;
}

/// Retrieves data and applies the existing ownership and lock projection rules.
/// The output is card facts, not an account status or a transfer authorization.
pub struct CardQueryService<S> {
    source: S,
}

impl<S: CardDataSource> CardQueryService<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn query(
        &self,
        trusted_user_id: &str,
        trusted_card_id: Option<&str>,
    ) -> Result<CardFacts, QueryIssue> {
        // Reject missing references before touching a data dependency. Do not
        // infer an identity or a card from a tail number or a previous request.
        if trusted_user_id.trim().is_empty() {
            return Err(QueryIssue::ResourceUnavailable);
        }
        let card_id = trusted_card_id
            .filter(|id| !id.trim().is_empty())
            .ok_or(QueryIssue::MissingCardReference)?;
        let snapshot = self
            .source
            .read_snapshot(trusted_user_id, card_id)
            .map_err(|_| QueryIssue::LookupUnavailable)?;
        query_card(trusted_user_id, Some(card_id), &snapshot)
    }
}
