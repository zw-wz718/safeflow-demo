//! Account-wide, read-only service with a module-local pagination contract.
//! The foundation supplies authenticated identity and a stable account ID.
//! Real adapters must enforce ownership, exact totals, snapshot versioning and
//! pagination completeness. This service cannot detect a lying data source.

use std::collections::HashSet;

use crate::account_restrictions::{restricts_use, AccountUse, UnlockRoute};
use crate::service::SourceUnavailable;
use crate::{query_card, Account, Card, CardFacts, LockRecord, Lookup, QueryIssue, Snapshot};

// Bound even a provider that continually invents fresh cursors.
const MAX_PAGES: usize = 256;

/// All records must belong to exactly the requested account. Counts describe
/// every card and every lock (including released history) at this revision.
/// A final page alone is not proof that all records were returned.
#[derive(Clone, Debug)]
pub struct AccountPage {
    pub account: Account,
    pub revision: String,
    pub total_cards: usize,
    pub total_locks: usize,
    pub cards: Vec<Card>,
    pub locks: Vec<LockRecord>,
    pub next_cursor: Option<String>,
}

/// Implementations are trusted internal dependencies, never model tool inputs.
/// Each page must use the requested revision, stable ordering and exact totals.
/// revision must change on ANY relevant ownership, card or lock change, even if
/// later restored. A real adapter needs a consistent database/snapshot protocol.
pub trait AccountDataSource {
    fn read_page(
        &self,
        trusted_user_id: &str,
        trusted_account_id: &str,
        cursor: Option<&str>,
        expected_revision: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable>;

    /// Recheck ownership and obtain the current revision after collecting pages.
    /// Failure must be an error, including deleted or no-longer-owned accounts.
    fn read_current_revision(
        &self,
        trusted_user_id: &str,
        trusted_account_id: &str,
    ) -> Result<String, SourceUnavailable>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountQueryIssue {
    ResourceUnavailable,
    LookupUnavailable,
    IncompleteData,
    InconsistentData,
    SnapshotChanged,
}

/// Point-in-time card-lock facts, not overall account health or a payment permit.
/// No raw risk evidence or lock-release operation is exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountCardFacts {
    pub account_id: String,
    pub revision: String,
    pub cards: Vec<CardFacts>,
    pub active_card_lock_ids: Vec<String>,
    pub unlock_route: UnlockRoute,
}

impl AccountCardFacts {
    pub fn outgoing_restricted(&self) -> bool {
        !self.active_card_lock_ids.is_empty()
    }

    pub fn restricts(&self, account_use: AccountUse) -> bool {
        restricts_use(self.outgoing_restricted(), account_use)
    }
}

pub struct AccountQueryService<S> {
    source: S,
}

impl<S: AccountDataSource> AccountQueryService<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn query(
        &self,
        trusted_user_id: &str,
        trusted_account_id: Option<&str>,
    ) -> Result<AccountCardFacts, AccountQueryIssue> {
        use AccountQueryIssue as Issue;
        let account_id = trusted_account_id
            .filter(|id| !id.trim().is_empty())
            .ok_or(Issue::ResourceUnavailable)?;
        if trusted_user_id.trim().is_empty() {
            return Err(Issue::ResourceUnavailable);
        }

        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut cards = Vec::new();
        let mut locks = Vec::new();
        let mut card_ids = HashSet::new();
        let mut lock_ids = HashSet::new();
        let mut metadata: Option<(String, usize, usize)> = None;

        for page_index in 0..MAX_PAGES {
            let page = self.source.read_page(
                trusted_user_id,
                account_id,
                cursor.as_deref(),
                metadata.as_ref().map(|m| m.0.as_str()),
            ).map_err(|_| Issue::LookupUnavailable)?;
            // Do not return foreign account metadata or any partial facts.
            if page.account.id != account_id || page.account.user_id != trusted_user_id {
                return Err(Issue::ResourceUnavailable);
            }
            if page.revision.trim().is_empty() {
                return Err(Issue::InconsistentData);
            }
            if let Some((revision, total_cards, total_locks)) = &metadata {
                if *revision != page.revision {
                    return Err(Issue::SnapshotChanged);
                }
                if *total_cards != page.total_cards || *total_locks != page.total_locks {
                    return Err(Issue::InconsistentData);
                }
            } else {
                metadata = Some((page.revision.clone(), page.total_cards, page.total_locks));
            }
            for card in page.cards {
                if card.id.trim().is_empty()
                    || card.account_id != account_id
                    || !card_ids.insert(card.id.clone())
                {
                    return Err(Issue::InconsistentData);
                }
                cards.push(card);
            }
            for lock in page.locks {
                if lock.id.trim().is_empty()
                    || lock.card_id.trim().is_empty()
                    || !lock_ids.insert(lock.id.clone())
                {
                    return Err(Issue::InconsistentData);
                }
                locks.push(lock);
            }
            if cards.len() > page.total_cards || locks.len() > page.total_locks {
                return Err(Issue::InconsistentData);
            }
            match page.next_cursor {
                Some(next) => {
                    if next.trim().is_empty() || !seen_cursors.insert(next.clone()) {
                        return Err(Issue::InconsistentData);
                    }
                    if page_index + 1 == MAX_PAGES {
                        return Err(Issue::IncompleteData);
                    }
                    cursor = Some(next);
                }
                None => {
                    if cards.len() != page.total_cards || locks.len() != page.total_locks {
                        return Err(Issue::IncompleteData);
                    }
                    if locks.iter().any(|lock| !card_ids.contains(&lock.card_id)) {
                        return Err(Issue::InconsistentData);
                    }
                    let current_revision = self.source.read_current_revision(
                        trusted_user_id, account_id,
                    ).map_err(|_| Issue::LookupUnavailable)?;
                    if current_revision != page.revision {
                        return Err(Issue::SnapshotChanged);
                    }
                    let snapshot = Snapshot {
                        accounts: Lookup::Complete(vec![page.account]),
                        cards: Lookup::Complete(cards),
                        locks: Lookup::Complete(locks),
                    };
                    let Lookup::Complete(all_cards) = &snapshot.cards else { unreachable!() };
                    let facts: Vec<CardFacts> = all_cards.iter().map(|card| {
                        query_card(trusted_user_id, Some(&card.id), &snapshot)
                            .map_err(|issue| match issue {
                                QueryIssue::LookupUnavailable => Issue::LookupUnavailable,
                                QueryIssue::InconsistentData => Issue::InconsistentData,
                                _ => Issue::ResourceUnavailable,
                            })
                    }).collect::<Result<_, _>>()?;
                    let active_card_lock_ids = facts.iter()
                        .flat_map(|card| card.active_locks.iter().map(|lock| lock.id.clone()))
                        .collect();
                    return Ok(AccountCardFacts {
                        account_id: account_id.to_owned(),
                        revision: page.revision,
                        cards: facts,
                        active_card_lock_ids,
                        unlock_route: UnlockRoute::OfflineBank,
                    });
                }
            }
        }
        Err(Issue::IncompleteData)
    }
}
