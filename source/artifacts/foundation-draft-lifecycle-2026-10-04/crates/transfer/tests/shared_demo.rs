use safeflow_contracts::{ResourceId, UserId};
use safeflow_transfer_integration::shared_demo::{DemoConfigError, DemoRecipient, SharedDemoPlan};

#[test]
fn receiving_route_follows_explicit_ids_not_a_name_or_masked_tail() {
    for id in ["first-explicit-receiver", "second-explicit-receiver"] {
        let plan = SharedDemoPlan::prepare(DemoRecipient {
            owner_id: UserId::parse("fixture-li-owner").unwrap(),
            account_id: ResourceId::parse(id).unwrap(),
        })
        .unwrap();
        assert_eq!(plan.route().recipient_account_id.as_str(), id);
        assert_eq!(plan.payee().payee_id.as_str(), "payee_008");
        assert_eq!(plan.payee().masked_account_no, "**** 1024");
    }
    let oversized = "x".repeat(129);
    assert_eq!(
        SharedDemoPlan::prepare(DemoRecipient {
            owner_id: UserId::parse("fixture-li-owner").unwrap(),
            account_id: ResourceId::parse(oversized).unwrap(),
        })
        .unwrap_err(),
        DemoConfigError::InvalidIdentifier
    );
}
#[test]
fn explicit_recipient_cannot_be_payer_or_have_malformed_configuration_ids() {
    for (owner, account, expected) in [
        (
            "user_001",
            "separate-account",
            DemoConfigError::PayerIsRecipient,
        ),
        (
            "not a trusted owner",
            "separate-account",
            DemoConfigError::InvalidIdentifier,
        ),
        (
            "fixture-li-owner",
            "not an account",
            DemoConfigError::InvalidIdentifier,
        ),
    ] {
        assert_eq!(
            SharedDemoPlan::prepare(DemoRecipient {
                owner_id: UserId::parse(owner).unwrap(),
                account_id: ResourceId::parse(account).unwrap(),
            })
            .unwrap_err(),
            expected
        );
    }
}

#[test]
fn receiving_account_must_not_replace_either_existing_account() {
    for account in ["account_001", "account_002"] {
        assert_eq!(
            SharedDemoPlan::prepare(DemoRecipient {
                owner_id: UserId::parse("fixture-li-owner").unwrap(),
                account_id: ResourceId::parse(account).unwrap(),
            })
            .unwrap_err(),
            DemoConfigError::ExistingAccountCollision
        );
    }
}

#[test]
fn approved_catalog_preserves_existing_payer_and_backup_and_adds_an_explicit_recipient() {
    let plan = SharedDemoPlan::prepare(DemoRecipient {
        owner_id: UserId::parse("fixture-li-owner").unwrap(),
        account_id: ResourceId::parse("fixture-li-account").unwrap(),
    })
    .unwrap();
    let accounts = plan.accounts();
    assert_eq!(accounts.len(), 3);
    assert_eq!(accounts[0].owner_id.as_str(), "user_001");
    assert_eq!(accounts[0].account.account_id.as_str(), "account_001");
    assert_eq!(accounts[0].account.masked_account_no, "**** 6628");
    assert_eq!(accounts[0].opening_balance.as_str(), "1288.00");
    assert_eq!(accounts[1].owner_id.as_str(), "user_002");
    assert_eq!(accounts[1].account.account_id.as_str(), "account_002");
    assert_eq!(accounts[1].account.masked_account_no, "**** 7788");
    assert_eq!(accounts[1].opening_balance.as_str(), "50.00");
    assert_eq!(accounts[2].owner_id.as_str(), "fixture-li-owner");
    assert_eq!(
        accounts[2].account.account_id.as_str(),
        "fixture-li-account"
    );
    assert_eq!(accounts[2].opening_balance.as_str(), "0.00");
    assert_eq!(plan.route().owner_id.as_str(), "user_001");
    assert_eq!(plan.route().payee_id.as_str(), "payee_008");
    assert_eq!(
        plan.route().recipient_account_id.as_str(),
        "fixture-li-account"
    );
}
