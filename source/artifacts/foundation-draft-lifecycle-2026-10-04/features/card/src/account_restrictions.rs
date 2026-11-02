//! Shared D-02 read rules. These facts do not grant permission to spend.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountUse {
    Transfer,
    CardSpend,
    AutomaticDebit,
    OtherOutgoing,
    Query,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlockRoute {
    OfflineBank,
}

pub(crate) fn restricts_use(has_active_card_locks: bool, account_use: AccountUse) -> bool {
    match account_use {
        AccountUse::Transfer
        | AccountUse::CardSpend
        | AccountUse::AutomaticDebit
        | AccountUse::OtherOutgoing => has_active_card_locks,
        AccountUse::Query | AccountUse::Receive => false,
    }
}
