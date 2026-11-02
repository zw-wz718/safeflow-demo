//! Read-only source seam for fictional subscription ledgers, not authentication
//! or persistence. Loading creates fresh memory state; it does not restore
//! historical confirmations, events, reminder settings or delivery attempts.
use crate::{
    confirmation_mock::{MemoryConfirmationMock, MockIssue},
    managed_ledger_mock::LedgerFixture,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockLedgerSourceError {
    Unavailable,
}

/// A source must return a complete, explicitly fictional snapshot or an error.
/// Caller-supplied user IDs remain mock context, never production credentials.
pub trait MockLedgerSource {
    fn read_owned_mock(&self, user: &str) -> Result<LedgerFixture, MockLedgerSourceError>;
}

/// Validate source metadata and ownership using the existing ledger rules.
/// A failed read is never replaced with an empty ledger. No source write occurs.
pub fn load_owned_ledger_mock(
    source: &impl MockLedgerSource,
    user: &str,
) -> Result<MemoryConfirmationMock, MockIssue> {
    if user.trim().is_empty() {
        return Err(MockIssue::ResourceUnavailable);
    }
    let fixture = source
        .read_owned_mock(user)
        .map_err(|_| MockIssue::LedgerUnavailable)?;
    let ledger = MemoryConfirmationMock::from_fixture(fixture);
    ledger.list_mock(user)?;
    Ok(ledger)
}
