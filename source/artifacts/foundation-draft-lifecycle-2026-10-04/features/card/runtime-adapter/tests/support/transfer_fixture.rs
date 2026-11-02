// Test/demo only: read the transfer owner's unchanged fixture and actual mock
// ledger. This is not a funds execution adapter or a trusted production source.
#![allow(dead_code)]
use safeflow_card_facts::account_service::{AccountDataSource, AccountPage};
use safeflow_card_facts::service::SourceUnavailable;
use safeflow_card_facts::transaction_reference::{
    TransactionCardFacts, TransactionCardQueryService, TransactionReferenceIssue,
    TrustedTransactionReference,
};
use safeflow_card_facts::{Account, Card, LockRecord, LockSource, LockState};
use safeflow_contracts::{IdempotencyKey, UserId};
use safeflow_transfer_integration::mock_execution::{
    MockBehavior, MockTransferBank, MockTransferRequest, MockTransferTransaction,
};
use serde_json::{json, Value};
use std::sync::Arc;

struct FixedClock(u64);
impl safeflow_transfer_integration::Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}

pub fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../crates/transfer/fixtures/mock_transfer_200.json"
    ))
    .unwrap()
}

pub fn bank(
    behavior: MockBehavior,
) -> (
    MockTransferBank,
    UserId,
    IdempotencyKey,
    MockTransferRequest,
) {
    let fixture = fixture();
    (
        MockTransferBank::demo_with_behavior(
            Arc::new(FixedClock(fixture["clock_unix_seconds"].as_u64().unwrap())),
            behavior,
        ),
        UserId::parse(fixture["authenticated_user_id"].as_str().unwrap()).unwrap(),
        IdempotencyKey::parse(fixture["idempotency_key"].as_str().unwrap()).unwrap(),
        serde_json::from_value(fixture["request"].clone()).unwrap(),
    )
}

/// Fictional cards are named separately; they are NEVER written into the ledger.
/// These are current lock facts, not a reconstruction of historical lock state.
pub struct CurrentAccountSource {
    pub locked: bool,
    pub unavailable: bool,
}
impl AccountDataSource for CurrentAccountSource {
    fn read_page(
        &self,
        _: &str,
        _: &str,
        cursor: Option<&str>,
        revision: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        assert!(cursor.is_none());
        assert!(revision.is_none());
        if self.unavailable {
            return Err(SourceUnavailable);
        }
        Ok(AccountPage {
            account: Account {
                id: "account_001".into(),
                user_id: "user_demo".into(),
            },
            revision: "fictional-card-revision-1".into(),
            total_cards: 2,
            total_locks: usize::from(self.locked),
            cards: ["fictional-card-1", "fictional-card-2"]
                .into_iter()
                .map(|id| Card {
                    id: id.into(),
                    account_id: "account_001".into(),
                    last_four: "1234".into(),
                })
                .collect(),
            locks: if self.locked {
                vec![LockRecord {
                    id: "fictional-lock-1".into(),
                    card_id: "fictional-card-2".into(),
                    source: LockSource::RiskControl,
                    state: LockState::Active,
                }]
            } else {
                vec![]
            },
            next_cursor: None,
        })
    }
    fn read_current_revision(&self, _: &str, _: &str) -> Result<String, SourceUnavailable> {
        Ok("fictional-card-revision-1".into())
    }
}

pub fn review(
    transaction: &MockTransferTransaction,
    user: &UserId,
    locked: bool,
) -> Result<TransactionCardFacts, TransactionReferenceIssue> {
    TransactionCardQueryService::new(CurrentAccountSource {
        locked,
        unavailable: false,
    })
    .query(
        user.as_str(),
        TrustedTransactionReference {
            transaction_id: &transaction.transaction_id,
            account_id: Some(transaction.account_id.as_str()),
            card_id: transaction.card_id.as_ref().map(|id| id.as_str()),
        },
    )
}

pub fn run_demo() -> Value {
    let mut reports = vec![];
    for (name, behavior) in [
        ("success", MockBehavior::Immediate),
        ("failure", MockBehavior::Decline),
        ("processing", MockBehavior::Pending),
        ("lost_reply", MockBehavior::LostReplyAfterCommit),
    ] {
        let (bank, user, key, request) = bank(behavior);
        let submission = bank.submit(&user, &key, request.clone()).unwrap();
        // Read the original operation, never resubmit to recover a lost reply.
        let original_query = bank.query(&user, &request.operation_id).unwrap();
        let transactions = bank.transactions(&user).unwrap();
        let mut reviews = vec![];
        for transaction in &transactions {
            let facts = review(transaction, &user, true).unwrap();
            assert!(transaction.card_id.is_none());
            assert!(transaction.merchant_id.is_none());
            assert!(facts.referenced_card.is_none());
            assert!(facts.account.outgoing_restricted());
            reviews.push(json!({
                "transaction_id": facts.transaction_id,
                "referenced_card_id": null,
                "card_display": "流水未提供对应卡片",
                "current_account_id": facts.account.account_id,
                "current_account_card_lock_count": facts.account.active_card_lock_ids.len(),
                "current_account_outgoing_restricted": facts.account.outgoing_restricted(),
                "historical_transaction_reclassified": false
            }));
        }
        let expected_count = usize::from(matches!(
            behavior,
            MockBehavior::Immediate | MockBehavior::LostReplyAfterCommit
        ));
        assert_eq!(transactions.len(), expected_count);
        assert_eq!(bank.transactions(&user).unwrap(), transactions);
        reports.push(json!({
            "case": name, "submission": submission, "query_original_operation": original_query,
            "source_transactions_unchanged": transactions, "current_card_reviews": reviews,
            "completed_transaction_count": expected_count
        }));
    }
    json!({
        "scope": "LOCAL_TRANSFER_MOCK_AND_CARD_READ_REVIEW_ONLY",
        "source_revision": "159620b7aab11368c8332cbca8b4b5b7b9d778e3",
        "current_card_source": "EXPLICIT_FICTIONAL_ACCOUNT_SOURCE",
        "runtime_funds_entry_connected": false,
        "real_bank_connected": false,
        "cases": reports
    })
}
