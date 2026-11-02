//! Independent, fictional snapshots. These are not real banking responses.
use crate::{Account, Card, LockRecord, LockSource, LockState, Lookup, Snapshot};

pub fn normal() -> Snapshot {
    Snapshot {
        accounts: Lookup::Complete(vec![
            Account {
                id: "ACCOUNT-A".into(),
                user_id: "U-A".into(),
            },
            Account {
                id: "ACCOUNT-B".into(),
                user_id: "U-B".into(),
            },
        ]),
        cards: Lookup::Complete(vec![
            Card {
                id: "CARD-K".into(),
                account_id: "ACCOUNT-A".into(),
                last_four: "1024".into(),
            },
            Card {
                id: "CARD-L".into(),
                account_id: "ACCOUNT-B".into(),
                last_four: "5678".into(),
            },
        ]),
        locks: Lookup::Complete(vec![]),
    }
}

pub fn restricted() -> Snapshot {
    let mut snapshot = normal();
    snapshot.locks = Lookup::Complete(vec![
        LockRecord {
            id: "LOCK-SELF-1".into(),
            card_id: "CARD-K".into(),
            source: LockSource::UserSelf,
            state: LockState::Active,
        },
        LockRecord {
            id: "LOCK-RISK-1".into(),
            card_id: "CARD-K".into(),
            source: LockSource::RiskControl,
            state: LockState::Active,
        },
    ]);
    snapshot
}

pub fn unknown() -> Snapshot {
    let mut snapshot = normal();
    snapshot.locks = Lookup::Unavailable;
    snapshot
}
