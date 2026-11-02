use safeflow_billing_source_reader::*;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/billing/upstream/mock_transfer_200.2a72b5d.json"
));

fn fixture() -> TransferFixture {
    TransferFixture::from_json(FIXTURE).unwrap()
}

fn scope() -> TrustedScope {
    TrustedScope::from_verified_context("user_demo", ["account_001"]).unwrap()
}

// Mutations below are adversarial test inputs, not new upstream or demo data.
fn modified_transaction(patch: Value) -> SourceTransaction {
    let mut value: Value = serde_json::from_str(FIXTURE).unwrap();
    let transaction = &mut value["success_debit_transaction"];
    for (key, value) in patch.as_object().unwrap() {
        transaction[key] = value.clone();
    }
    serde_json::from_value(transaction.clone()).unwrap()
}

#[derive(Clone)]
struct ReadFixture {
    source_id: String,
    rows: Result<Vec<OwnedTransaction>, SourceError>,
    query: Result<ObservedOperation, SourceError>,
}

impl ReadOnlySource for ReadFixture {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn read_completed_transactions(
        &self,
        _scope: &TrustedScope,
    ) -> Result<Vec<OwnedTransaction>, SourceError> {
        self.rows.clone()
    }

    fn query_operation(
        &self,
        trusted: &TrustedScope,
        business_operation_id: &str,
    ) -> Result<ObservedOperation, SourceError> {
        // A wrong user or a newly invented operation must fail the real refresh.
        assert_eq!(trusted.user_id(), "user_demo");
        assert_eq!(business_operation_id, "business-transfer-001");
        self.query.clone()
    }
}

fn read_fixture(rows: Vec<OwnedTransaction>) -> ReadFixture {
    ReadFixture {
        source_id: "test-source".into(),
        rows: Ok(rows),
        query: Err(SourceError::Unavailable),
    }
}

fn owned(transaction: SourceTransaction) -> OwnedTransaction {
    OwnedTransaction {
        user_id: "user_demo".into(),
        transaction,
    }
}

fn observed(receipt: OperationReceipt) -> ObservedOperation {
    ObservedOperation {
        source_id: "test-source".into(),
        user_id: "user_demo".into(),
        receipt,
    }
}

#[test]
fn real_fixture_preserves_source_values_and_null_references() {
    let source = OfflineFixtureSource::new(fixture(), DemoCase::Success);
    let bill = BillingReader::default().read(&scope(), &source).unwrap();
    let value = serde_json::to_value(&bill.entries[0]).unwrap();
    assert_eq!(
        value["transaction"]["transaction_id"],
        "business-transfer-001:debit"
    );
    assert_eq!(
        value["transaction"]["operation_id"],
        "business-transfer-001"
    );
    assert_eq!(value["transaction"]["direction"], "DEBIT");
    assert_eq!(value["transaction"]["status"], "COMPLETED");
    assert_eq!(value["transaction"]["transaction_type"], "TRANSFER");
    assert_eq!(value["transaction"]["amount"], "200.00");
    assert_eq!(value["transaction"]["currency"], "CNY");
    assert_eq!(value["transaction"]["completed_at"], "2026-10-07T00:00:00Z");
    assert!(value["transaction"].get("card_id").unwrap().is_null());
    assert!(value["transaction"].get("merchant_id").unwrap().is_null());
    assert!(value["transaction"].get("category_code").is_none());
    assert!(value["transaction"].get("original_tx_id").is_none());
    assert_eq!(
        bill.entries[0].consumption,
        ConsumptionAssessment::TransferExcluded
    );
    assert_eq!(
        bill.entries[0].card_note.as_deref(),
        Some("流水未提供对应卡片")
    );
    assert_eq!(
        bill.subscription_evidence,
        SubscriptionEvidence::Unavailable
    );
}

#[test]
fn four_cases_keep_operation_and_ledger_states_separate() {
    for (case, status, count, query_status) in [
        (
            DemoCase::Success,
            OperationStatus::Succeeded,
            1,
            OperationStatus::Succeeded,
        ),
        (
            DemoCase::Failure,
            OperationStatus::Failed,
            0,
            OperationStatus::Failed,
        ),
        (
            DemoCase::Processing,
            OperationStatus::Processing,
            0,
            OperationStatus::Processing,
        ),
        (
            DemoCase::LostReply,
            OperationStatus::Processing,
            1,
            OperationStatus::Succeeded,
        ),
    ] {
        let source = OfflineFixtureSource::new(fixture(), case);
        let initial = source.initial_operation();
        assert_eq!(initial.receipt.status, status, "{case:?}");
        let bill = BillingReader::default().read(&scope(), &source).unwrap();
        assert_eq!(bill.entries.len(), count, "{case:?}");
        let queried = refresh_operation(&scope(), &source, &initial).unwrap();
        assert_eq!(queried.last_known.receipt.status, query_status, "{case:?}");
        if case == DemoCase::LostReply {
            assert_eq!(queried.last_known.receipt, fixture().success);
            assert_eq!(
                bill.entries[0].transaction,
                fixture().success_debit_transaction
            );
        }
    }
}

#[test]
fn rereading_identical_transaction_adds_nothing() {
    let source = OfflineFixtureSource::new(fixture(), DemoCase::Success);
    let mut reader = BillingReader::default();
    assert_eq!(reader.read(&scope(), &source).unwrap().added, 1);
    let second = reader.read(&scope(), &source).unwrap();
    assert_eq!(second.added, 0);
    assert_eq!(second.unchanged, 1);
    assert_eq!(second.entries.len(), 1);
}

#[test]
fn user_and_account_filters_prevent_cross_scope_leaks() {
    let mut wrong_user = owned(fixture().success_debit_transaction);
    wrong_user.user_id = "other-user".into();
    let wrong_account = owned(modified_transaction(json!({"account_id":"account_002"})));
    let source = read_fixture(vec![
        wrong_user,
        wrong_account,
        owned(fixture().success_debit_transaction),
    ]);
    let mut reader = BillingReader::default();
    let bill = reader.read(&scope(), &source).unwrap();
    assert_eq!(bill.entries.len(), 1);
    assert_eq!(bill.excluded_out_of_scope, 2);
    let other = TrustedScope::from_verified_context("unrelated-user", ["account_001"]).unwrap();
    assert!(reader.read(&other, &source).unwrap().entries.is_empty());
}

#[test]
fn same_operation_is_not_a_deduplication_key() {
    let credit = modified_transaction(json!({
        "transaction_id":"business-transfer-001:credit",
        "account_id":"second-owned-test-account", "direction":"CREDIT"
    }));
    let source = read_fixture(vec![
        owned(fixture().success_debit_transaction),
        owned(credit),
    ]);
    let trusted = TrustedScope::from_verified_context(
        "user_demo",
        ["account_001", "second-owned-test-account"],
    )
    .unwrap();
    let bill = BillingReader::default().read(&trusted, &source).unwrap();
    assert_eq!(bill.entries.len(), 2);
    assert!(bill
        .entries
        .iter()
        .all(|row| row.consumption == ConsumptionAssessment::TransferExcluded));
    assert!(bill
        .entries
        .iter()
        .any(|row| row.transaction.direction == Direction::Credit));
}

#[test]
fn deduplication_includes_source_user_and_account() {
    let transaction = fixture().success_debit_transaction;
    let mut source = read_fixture(vec![owned(transaction.clone())]);
    let mut reader = BillingReader::default();
    assert_eq!(reader.read(&scope(), &source).unwrap().added, 1);
    source.source_id = "another-source".into();
    assert_eq!(reader.read(&scope(), &source).unwrap().added, 1);
    source.rows = Ok(vec![OwnedTransaction {
        user_id: "other-user".into(),
        transaction,
    }]);
    let other = TrustedScope::from_verified_context("other-user", ["account_001"]).unwrap();
    assert_eq!(reader.read(&other, &source).unwrap().added, 1);
    let trusted = TrustedScope::from_verified_context(
        "user_demo",
        ["account_001", "second-owned-test-account"],
    )
    .unwrap();
    source.rows = Ok(vec![owned(modified_transaction(
        json!({"account_id":"second-owned-test-account"}),
    ))]);
    let bill = reader.read(&trusted, &source).unwrap();
    assert_eq!(bill.added, 1);
    assert_eq!(bill.entries.len(), 2);
}

#[test]
fn conflicting_id_fails_without_overwriting_or_partially_adding() {
    let mut source = read_fixture(vec![owned(fixture().success_debit_transaction)]);
    let mut reader = BillingReader::default();
    reader.read(&scope(), &source).unwrap();
    source.rows = Ok(vec![
        owned(modified_transaction(
            json!({"transaction_id":"new-test-id"}),
        )),
        owned(modified_transaction(json!({"amount":"201.00"}))),
    ]);
    assert!(matches!(
        reader.read(&scope(), &source),
        Err(BillingError::ConflictingTransaction { .. })
    ));
    source.rows = Ok(vec![owned(fixture().success_debit_transaction)]);
    let after = reader.read(&scope(), &source).unwrap();
    assert_eq!(after.entries.len(), 1);
    assert_eq!(after.entries[0].transaction.amount.as_str(), "200.00");
    assert_eq!(after.added, 0);
}

#[test]
fn ledger_source_errors_are_explicit_failures() {
    let mut source = read_fixture(vec![]);
    source.rows = Err(SourceError::Unavailable);
    assert!(matches!(
        BillingReader::default().read(&scope(), &source),
        Err(BillingError::Source(SourceError::Unavailable))
    ));
}

#[test]
fn query_errors_keep_last_known_state_and_original_business_id() {
    for receipt in [fixture().lost_reply, fixture().success, fixture().failure] {
        for error in [SourceError::Unavailable, SourceError::OperationNotFound] {
            let mut source = read_fixture(vec![]);
            source.query = Err(error.clone());
            let last = observed(receipt.clone());
            let result = refresh_operation(&scope(), &source, &last).unwrap();
            assert_eq!(result.last_known, last);
            assert_eq!(result.query_error, Some(error));
            let advice = result.next_read.unwrap();
            assert_eq!(advice.business_operation_id, "business-transfer-001");
            assert_eq!(advice.action, "QUERY_ORIGINAL_BUSINESS_OPERATION");
        }
    }
}

#[test]
fn mismatched_or_regressive_query_does_not_replace_known_receipt() {
    let mut source = read_fixture(vec![]);
    let last = observed(fixture().success);
    let mut wrong_id = observed(fixture().success);
    wrong_id.receipt.operation_id = "unrelated-operation".into();
    let mut wrong_user = observed(fixture().success);
    wrong_user.user_id = "other-user".into();
    let mut wrong_source = observed(fixture().success);
    wrong_source.source_id = "other-source".into();
    let mut wrong_amount = observed(fixture().success);
    wrong_amount.receipt.amount = serde_json::from_value(json!("201.00")).unwrap();
    for candidate in [
        wrong_id,
        wrong_user,
        wrong_source,
        wrong_amount,
        observed(fixture().processing),
        observed(fixture().failure),
    ] {
        source.query = Ok(candidate);
        let result = refresh_operation(&scope(), &source, &last).unwrap();
        assert_eq!(result.last_known, last);
        assert_eq!(result.query_error, Some(SourceError::InvalidResponse));
    }
}

#[test]
fn invalid_amount_currency_or_missing_required_fields_fail_parsing() {
    let base: Value = serde_json::from_str(FIXTURE).unwrap();
    for amount in [
        json!(200.0),
        json!("-200.00"),
        json!("0.00"),
        json!("200"),
        json!("200.0"),
        json!(" 200.00"),
        json!("1e2"),
        json!("18446744073709551616.00"),
    ] {
        let mut broken = base.clone();
        broken["success_debit_transaction"]["amount"] = amount;
        assert!(TransferFixture::from_json(&broken.to_string()).is_err());
    }
    for field in [
        "transaction_id",
        "operation_id",
        "amount",
        "currency",
        "completed_at",
        "account_id",
        "card_id",
        "merchant_id",
        "status",
    ] {
        let mut broken = base.clone();
        broken["success_debit_transaction"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            TransferFixture::from_json(&broken.to_string()).is_err(),
            "{field}"
        );
    }
    for (field, invalid) in [
        ("currency", json!("USD")),
        ("status", json!("SUCCEEDED")),
        ("completed_at", json!("")),
        ("direction", json!("OUT")),
    ] {
        let mut broken = base.clone();
        broken["success_debit_transaction"][field] = invalid;
        assert!(
            TransferFixture::from_json(&broken.to_string()).is_err(),
            "{field}"
        );
    }
    assert!(TransferFixture::from_json("{broken").is_err());
}

#[test]
fn fixture_user_metadata_cannot_select_authenticated_scope() {
    let mut value: Value = serde_json::from_str(FIXTURE).unwrap();
    value["authenticated_user_id"] = json!("pretend-admin");
    let source = OfflineFixtureSource::new(
        TransferFixture::from_json(&value.to_string()).unwrap(),
        DemoCase::Success,
    );
    let pretend = TrustedScope::from_verified_context("pretend-admin", ["account_001"]).unwrap();
    assert!(matches!(
        BillingReader::default().read(&pretend, &source),
        Err(BillingError::Source(SourceError::ResourceNotAccessible))
    ));
}

#[test]
fn inconsistent_fixture_receipts_or_ledger_are_rejected() {
    let base: Value = serde_json::from_str(FIXTURE).unwrap();
    for (section, field, invalid) in [
        ("failure", "operation_id", json!("another-operation")),
        ("processing", "payer_account_id", json!("another-account")),
        ("lost_reply", "amount", json!("201.00")),
        ("failure", "status", json!("PROCESSING")),
        ("success_debit_transaction", "amount", json!("201.00")),
        (
            "success_debit_transaction",
            "completed_at",
            json!("2026-10-08T00:00:00Z"),
        ),
        ("success_debit_transaction", "direction", json!("CREDIT")),
    ] {
        let mut broken = base.clone();
        broken[section][field] = invalid;
        assert!(
            TransferFixture::from_json(&broken.to_string()).is_err(),
            "{section}.{field}"
        );
    }
}

#[test]
fn unknown_transaction_types_are_preserved_without_claiming_consumption() {
    let source = read_fixture(vec![owned(modified_transaction(
        json!({"transaction_type":"UNCONFIRMED_KIND"}),
    ))]);
    let bill = BillingReader::default().read(&scope(), &source).unwrap();
    assert_eq!(
        bill.entries[0].transaction.transaction_type,
        "UNCONFIRMED_KIND"
    );
    assert_eq!(
        bill.entries[0].consumption,
        ConsumptionAssessment::UnassessedSourceType
    );
    assert_eq!(
        bill.subscription_evidence,
        SubscriptionEvidence::Unavailable
    );
}
