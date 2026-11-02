//! Read-only facts and isolated pure simulation rules. No authentication,
//! authorization, transfer permission or bank operation.
//! The caller must obtain identity and complete snapshots from trusted services.

use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub user_id: String,
}

#[derive(Clone, Debug)]
pub struct Card {
    pub id: String,
    pub account_id: String,
    pub last_four: String,
}

/// Complete means the entire requested result was retrieved, including every page.
#[derive(Clone, Debug)]
pub enum Lookup<T> {
    Complete(Vec<T>),
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockSource {
    UserSelf,
    AnomalyDispute,
    RiskControl,
}

impl LockSource {
    pub fn public_reason(self) -> &'static str {
        match self {
            Self::UserSelf => "卡片丢失／安全顾虑",
            Self::AnomalyDispute => "针对您反馈的交易采取保护",
            Self::RiskControl => "另有安全限制，请联系人工客服",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    Active,
    Released,
}

/// Public projection of a trusted lock record; internal risk evidence is excluded.
#[derive(Clone, Debug)]
pub struct LockRecord {
    pub id: String,
    pub card_id: String,
    pub source: LockSource,
    pub state: LockState,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub accounts: Lookup<Account>,
    pub cards: Lookup<Card>,
    pub locks: Lookup<LockRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleLock {
    pub id: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardFacts {
    pub card_id: String,
    pub account_id: String,
    pub last_four: String,
    pub active_locks: Vec<VisibleLock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryIssue {
    MissingCardReference,
    LookupUnavailable,
    /// Missing and another user's card have the same outward result.
    ResourceUnavailable,
    InconsistentData,
}

/// Resolve by trusted ID, never by last four digits. An empty successful lock
/// query is different from an unavailable or incomplete query.
/// Returned facts cannot authorize a transfer or a freeze.
pub fn query_card(
    trusted_user_id: &str,
    trusted_card_id: Option<&str>,
    snapshot: &Snapshot,
) -> Result<CardFacts, QueryIssue> {
    if trusted_user_id.trim().is_empty() {
        return Err(QueryIssue::ResourceUnavailable);
    }
    let card_id = trusted_card_id
        .filter(|id| !id.trim().is_empty())
        .ok_or(QueryIssue::MissingCardReference)?;
    let cards = complete(&snapshot.cards)?;
    let matching_cards: Vec<_> = cards.iter().filter(|c| c.id == card_id).collect();
    let card = match matching_cards.as_slice() {
        [] => return Err(QueryIssue::ResourceUnavailable),
        [card] => *card,
        _ => return Err(QueryIssue::InconsistentData),
    };
    let accounts = complete(&snapshot.accounts)?;
    let matching_accounts: Vec<_> = accounts
        .iter()
        .filter(|a| a.id == card.account_id)
        .collect();
    let account = match matching_accounts.as_slice() {
        [] => return Err(QueryIssue::ResourceUnavailable),
        [account] => *account,
        _ => return Err(QueryIssue::InconsistentData),
    };
    if account.user_id != trusted_user_id {
        return Err(QueryIssue::ResourceUnavailable);
    }
    // Matching two blank IDs does not establish a real card-account mapping.
    // Check after ownership so malformed foreign records disclose no details.
    if account.id.trim().is_empty() {
        return Err(QueryIssue::InconsistentData);
    }
    if card.last_four.len() != 4 || !card.last_four.bytes().all(|b| b.is_ascii_digit()) {
        return Err(QueryIssue::InconsistentData);
    }
    let locks = complete(&snapshot.locks)?;
    let mut ids = HashSet::new();
    let mut active_self = 0;
    let mut active_locks = Vec::new();
    for lock in locks.iter().filter(|lock| lock.card_id == card.id) {
        if lock.id.trim().is_empty() || !ids.insert(&lock.id) {
            return Err(QueryIssue::InconsistentData);
        }
        if lock.state == LockState::Active {
            if lock.source == LockSource::UserSelf {
                active_self += 1;
                if active_self > 1 {
                    return Err(QueryIssue::InconsistentData);
                }
            }
            active_locks.push(VisibleLock {
                id: lock.id.clone(),
                reason: lock.source.public_reason(),
            });
        }
    }
    Ok(CardFacts {
        card_id: card.id.clone(),
        account_id: account.id.clone(),
        last_four: card.last_four.clone(),
        active_locks,
    })
}

fn complete<T>(lookup: &Lookup<T>) -> Result<&[T], QueryIssue> {
    match lookup {
        Lookup::Complete(items) => Ok(items),
        Lookup::Unavailable => Err(QueryIssue::LookupUnavailable),
    }
}

pub mod fixtures;
pub mod mock;
pub mod service;
pub mod lock_rules_mock;
pub mod account_restrictions_mock;
pub mod account_restrictions;
pub mod account_service;
pub mod transaction_reference;
pub mod operation_records;
pub mod operation_records_async;
pub mod operation_replay;
pub mod operation_replay_async;
pub mod saved_feedback;
pub mod saved_feedback_async;
pub mod feedback_operation;
pub mod feedback_operation_async;
