//! D-02 account restrictions over complete fictional records only.
//! This read model neither enforces bank payments nor authorizes them. It has
//! no release operation. Released records are supplied bank-outcome fixtures.

use crate::lock_rules_mock::{mock_validate_state, MockIssue, MockLockStatus, MockState};
use crate::account_restrictions::restricts_use;
pub use crate::account_restrictions::{AccountUse as MockAccountUse, UnlockRoute as MockUnlockRoute};

/// Internal simulated facts, not a public payment permit or an actual bank
/// outcome. No card-lock restriction does not imply permission to pay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockAccountRestrictions {
    pub account_id: String,
    pub active_card_lock_ids: Vec<String>,
    pub unlock_route: MockUnlockRoute,
}

impl MockAccountRestrictions {
    pub fn mock_outgoing_restricted(&self) -> bool {
        !self.active_card_lock_ids.is_empty()
    }

    /// D-02 covers every outgoing use, including outgoing uses added later.
    /// Query and incoming funds are unaffected by card locks specifically.
    pub fn mock_restricts(&self, account_use: MockAccountUse) -> bool {
        restricts_use(self.mock_outgoing_restricted(), account_use)
    }
}

/// Aggregate ACTIVE locks on every card associated with the selected account.
/// Never infer an empty lock set from missing, incomplete or inconsistent data.
pub fn simulate_account_restrictions(
    state: &MockState,
    user_id: &str,
    account_id: Option<&str>,
) -> Result<MockAccountRestrictions, MockIssue> {
    mock_validate_state(state)?;
    let account_id = account_id
        .filter(|id| !id.trim().is_empty())
        .ok_or(MockIssue::ResourceUnavailable)?;
    if user_id.trim().is_empty()
        || !state.users.iter().any(|user| user.id == user_id)
        || !state.accounts.iter().any(|account| {
            account.id == account_id && account.user_id == user_id
        })
    {
        return Err(MockIssue::ResourceUnavailable);
    }

    let active_card_lock_ids = state.locks.iter().filter(|lock| {
        lock.status == MockLockStatus::Active
            && state.cards.iter().any(|card| {
                card.id == lock.card_id && card.account_id == account_id
            })
    }).map(|lock| lock.id.clone()).collect();

    Ok(MockAccountRestrictions {
        account_id: account_id.to_owned(),
        active_card_lock_ids,
        unlock_route: MockUnlockRoute::OfflineBank,
    })
}
