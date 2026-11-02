#[path = "support/transfer_fixture.rs"]
mod transfer_fixture;

use safeflow_card_facts::account_service::AccountQueryIssue;
use safeflow_card_facts::transaction_reference::{
    TransactionCardQueryService, TransactionReferenceIssue, TrustedTransactionReference,
};
use safeflow_contracts::{ResourceId, ToolResultStatus, UserId};
use safeflow_transfer_integration::mock_execution::{MockBehavior, MockResolution};
use serde_json::json;
use transfer_fixture::*;

#[test]
fn actual_success_fixture_is_preserved_with_null_card_and_merchant() {
    let (bank, user, key, request) = bank(MockBehavior::Immediate);
    let receipt = bank.submit(&user, &key, request).unwrap();
    assert_eq!(receipt.status, ToolResultStatus::Succeeded);
    let before = bank.transactions(&user).unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(
        serde_json::to_value(&before[0]).unwrap(),
        fixture()["success_debit_transaction"]
    );
    let facts = review(&before[0], &user, true).unwrap();
    assert!(facts.referenced_card.is_none());
    assert!(facts.account.outgoing_restricted());
    assert_eq!(bank.transactions(&user).unwrap(), before);
    assert_eq!(
        bank.balance(&user, &ResourceId::parse("account_001").unwrap())
            .unwrap()
            .as_str(),
        "1088.00"
    );
}

#[test]
fn failed_or_unsettled_operation_has_no_completed_transaction_to_guess_a_card_for() {
    for behavior in [MockBehavior::Decline, MockBehavior::Pending] {
        let (bank, user, key, request) = bank(behavior);
        bank.submit(&user, &key, request).unwrap();
        assert!(bank.transactions(&user).unwrap().is_empty());
    }
}

#[test]
fn lost_reply_does_not_erase_completed_transaction_or_trigger_a_second_transfer() {
    let (bank, user, key, request) = bank(MockBehavior::LostReplyAfterCommit);
    let submission = bank.submit(&user, &key, request.clone()).unwrap();
    assert_eq!(submission.status, ToolResultStatus::Processing);
    let before = bank.transactions(&user).unwrap();
    assert_eq!(before.len(), 1);
    for _ in 0..3 {
        assert_eq!(
            bank.query(&user, &request.operation_id).unwrap().status,
            ToolResultStatus::Succeeded
        );
        assert!(review(&before[0], &user, true)
            .unwrap()
            .referenced_card
            .is_none());
    }
    assert_eq!(bank.transactions(&user).unwrap(), before);
    assert_eq!(
        bank.balance(&user, &ResourceId::parse("account_001").unwrap())
            .unwrap()
            .as_str(),
        "1088.00"
    );
}

#[test]
fn current_lock_does_not_relabel_a_historical_success_as_failure() {
    let (bank, user, key, request) = bank(MockBehavior::Immediate);
    bank.submit(&user, &key, request).unwrap();
    let transaction = bank.transactions(&user).unwrap().remove(0);
    for locked in [false, true] {
        assert_eq!(
            review(&transaction, &user, locked)
                .unwrap()
                .account
                .outgoing_restricted(),
            locked
        );
        assert_eq!(transaction.status, "COMPLETED");
        assert_eq!(transaction.transaction_type, "TRANSFER");
    }
}

#[test]
fn recipient_entry_is_not_assigned_to_the_payers_cards() {
    let (bank, user, key, request) = bank(MockBehavior::Immediate);
    bank.submit(&user, &key, request).unwrap();
    let recipient = UserId::parse("user_recipient").unwrap();
    let credit = bank.transactions(&recipient).unwrap().remove(0);
    assert_eq!(credit.account_id.as_str(), "account_002");
    assert!(credit.card_id.is_none());
    assert_eq!(
        review(&credit, &user, true),
        Err(TransactionReferenceIssue::AccountQuery(
            AccountQueryIssue::ResourceUnavailable
        ))
    );
}

#[test]
fn account_lookup_failure_does_not_change_existing_ledger_success() {
    let (bank, user, key, request) = bank(MockBehavior::Immediate);
    bank.submit(&user, &key, request).unwrap();
    let before = bank.transactions(&user).unwrap();
    let transaction = &before[0];
    let service = TransactionCardQueryService::new(CurrentAccountSource {
        locked: true,
        unavailable: true,
    });
    assert_eq!(
        service.query(
            user.as_str(),
            TrustedTransactionReference {
                transaction_id: &transaction.transaction_id,
                account_id: Some(transaction.account_id.as_str()),
                card_id: None,
            }
        ),
        Err(TransactionReferenceIssue::AccountQuery(
            AccountQueryIssue::LookupUnavailable
        ))
    );
    assert_eq!(bank.transactions(&user).unwrap(), before);
}

#[test]
fn later_completion_and_repeated_original_queries_keep_one_source_transaction() {
    let (bank, user, key, request) = bank(MockBehavior::Pending);
    bank.submit(&user, &key, request.clone()).unwrap();
    assert!(bank.transactions(&user).unwrap().is_empty());
    bank.resolve_mock(&user, &request.operation_id, MockResolution::Succeed)
        .unwrap();
    let before = bank.transactions(&user).unwrap();
    assert_eq!(before.len(), 1);
    for _ in 0..3 {
        bank.query(&user, &request.operation_id).unwrap();
        let facts = review(&before[0], &user, false).unwrap();
        assert_eq!(facts.transaction_id, before[0].transaction_id);
        assert!(facts.referenced_card.is_none());
    }
    assert_eq!(bank.transactions(&user).unwrap(), before);
}

#[test]
fn demo_verifies_all_four_real_mock_outcomes_with_unmodified_source_values() {
    let report = run_demo();
    assert_eq!(report["cases"].as_array().unwrap().len(), 4);
    assert_eq!(report["runtime_funds_entry_connected"], false);
    for case in report["cases"].as_array().unwrap() {
        for transaction in case["source_transactions_unchanged"].as_array().unwrap() {
            assert_eq!(transaction["card_id"], json!(null));
            assert_eq!(transaction["merchant_id"], json!(null));
            assert_eq!(transaction["status"], "COMPLETED");
        }
    }
}
