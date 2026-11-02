use safeflow_billing_source_reader::{
    duplicates::*, feedback::*, query::*, spending::*, TrustedScope,
};
use serde_json::{json, Value};

fn scope() -> TrustedScope {
    TrustedScope::from_verified_context("user_demo", ["account_001", "account_002"]).unwrap()
}
fn request() -> Request {
    serde_json::from_value(
        json!({"account_ids":["account_001","account_002"],"window":{
        "as_of":"2026-10-18T12:00:00+08:00","start_inclusive":"2026-10-01T00:00:00+08:00",
        "end_exclusive":"2026-10-18T12:00:00+08:00","timezone":"Asia/Shanghai"}}),
    )
    .unwrap()
}
fn row(id: &str, at: &str) -> Transaction {
    serde_json::from_value(json!({"transaction_id":id,"account_id":"account_001",
        "amount":"30.00","currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION",
        "status":"COMPLETED","completed_at":at,"merchant_id":"merchant_demo","card_id":"card_demo"})).unwrap()
}
struct Source(Result<Page, PageError>);
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        "fictional-actions"
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        _: &Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        self.0.clone()
    }
}
fn report(rows: Vec<Transaction>) -> SpendingReport {
    let p = Page {
        snapshot_id: "snapshot_demo".into(),
        owner_user_id: "user_demo".into(),
        account_ids: request().account_ids,
        window: request().window,
        total_unique_rows: Some(rows.len()),
        items: rows,
        has_more: false,
        next_cursor: None,
        range_complete: true,
    };
    analyze_spending(
        &scope(),
        &request(),
        &Source(Ok(p)),
        Limits::default(),
        &ReportMetadata {
            report_id: "report_1".into(),
            calculated_at: request().window.as_of,
        },
    )
    .unwrap()
}
fn ordinary() -> SpendingReport {
    report(vec![row("tx1", "2026-10-10T08:00:00+08:00")])
}
fn intent(choice: &str) -> FeedbackIntent {
    serde_json::from_value(json!({"report_id":"report_1","transaction":{
        "source_id":"fictional-actions","account_id":"account_001","transaction_id":"tx1"},"choice":choice})).unwrap()
}
fn dup(r: &SpendingReport) -> Value {
    serde_json::to_value(detect_duplicates(&scope(), r).unwrap()).unwrap()
}

// Removing full-duration comparison would include 600.001 seconds incorrectly.
#[test]
fn duplicate_time_boundaries_are_closed_and_preserve_same_second() {
    for (at, count) in [
        ("2026-10-10T00:00:00Z", 1),
        ("2026-10-10T00:10:00Z", 1),
        ("2026-10-10T00:10:00.001Z", 0),
    ] {
        let r = report(vec![row("a", "2026-10-10T08:00:00+08:00"), row("b", at)]);
        assert_eq!(dup(&r)["pairs"].as_array().unwrap().len(), count);
    }
}
#[test]
fn candidates_require_same_merchant_and_exact_amount() {
    for changed in ["merchant", "amount"] {
        let a = row("a", "2026-10-10T00:00:00Z");
        let mut b = a.clone();
        b.transaction_id = "b".into();
        if changed == "merchant" {
            b.merchant_id = Some("other".into());
        } else {
            b.amount = serde_json::from_value(json!("30.01")).unwrap();
        }
        assert!(dup(&report(vec![a, b]))["pairs"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    let a = row("a", "2026-10-10T00:00:00Z");
    let mut b = a.clone();
    b.transaction_id = "b".into();
    b.amount = serde_json::from_value(json!("030.00")).unwrap();
    let invalid = report(vec![a, b]);
    assert_eq!(invalid.read.reason.as_deref(), Some("INVALID_TRANSACTION"));
    assert_eq!(
        detect_duplicates(&scope(), &invalid).unwrap_err(),
        "REPORT_NOT_COMPLETE"
    );
}
#[test]
fn unknown_merchant_is_insufficient_not_a_clean_no_match() {
    let mut missing = row("missing", "2026-10-10T00:00:00Z");
    missing.merchant_id = None;
    let out = dup(&report(vec![
        row("a", "2026-10-10T00:00:00Z"),
        row("b", "2026-10-10T00:01:00Z"),
        missing,
    ]));
    assert_eq!(out["state"], "MERCHANT_EVIDENCE_INCOMPLETE");
    assert_eq!(out["pairs"].as_array().unwrap().len(), 1);
    assert_eq!(out["missing_merchant"][0]["transaction_id"], "missing");
    assert_eq!(out["risk_action_authorized"], false);
}
#[test]
fn cross_account_same_id_survives_and_order_is_deterministic() {
    let a = row("same", "2026-10-10T00:00:00Z");
    let mut b = a.clone();
    b.account_id = "account_002".into();
    let forward = dup(&report(vec![a.clone(), b.clone()]));
    assert_eq!(forward, dup(&report(vec![b, a])));
    assert_eq!(forward["pairs"][0]["first"]["account_id"], "account_001");
    assert_eq!(forward["pairs"][0]["second"]["account_id"], "account_002");
}
#[test]
fn pair_budget_never_claims_a_truncated_list_is_complete() {
    let rows = (0..46)
        .map(|i| row(&format!("tx{i}"), "2026-10-10T00:00:00Z"))
        .collect();
    let out = dup(&report(rows));
    assert_eq!(out["state"], "PAIR_LIMIT_EXCEEDED");
    assert!(out["pairs"].is_null());
}
#[test]
fn incomplete_source_and_changed_scope_block_both_actions() {
    let r = analyze_spending(
        &scope(),
        &request(),
        &Source(Err(PageError::Unavailable)),
        Limits::default(),
        &ReportMetadata {
            report_id: "report_1".into(),
            calculated_at: request().window.as_of,
        },
    )
    .unwrap();
    assert_eq!(
        detect_duplicates(&scope(), &r).unwrap_err(),
        "REPORT_NOT_COMPLETE"
    );
    assert_eq!(
        prepare_feedback(&scope(), &r, "op1", &intent("MINE")).unwrap_err(),
        "REPORT_NOT_COMPLETE"
    );
    let r = ordinary();
    for other in [
        TrustedScope::from_verified_context("other", ["account_001", "account_002"]).unwrap(),
        TrustedScope::from_verified_context("user_demo", ["account_002"]).unwrap(),
    ] {
        assert_eq!(
            detect_duplicates(&other, &r).unwrap_err(),
            "RESOURCE_UNAVAILABLE"
        );
        assert_eq!(
            prepare_feedback(&other, &r, "op1", &intent("MINE")).unwrap_err(),
            "RESOURCE_UNAVAILABLE"
        );
    }
}
#[test]
fn all_choices_prepare_original_evidence_without_acceptance_or_locking() {
    for choice in ["MINE", "NOT_MINE", "UNSURE"] {
        let value = serde_json::to_value(
            prepare_feedback(&scope(), &ordinary(), "op1", &intent(choice)).unwrap(),
        )
        .unwrap();
        assert_eq!(value["choice"], choice);
        assert_eq!(value["state"], "PREPARED_NOT_SAVED");
        assert_eq!(value["persisted"], false);
        assert_eq!(value["lock_authorized"], false);
        assert_eq!(value["transaction"]["card_id"], "card_demo");
        assert_eq!(value["card_association_verified"], false);
        assert_eq!(value["public_operation_id"], "op1");
    }
}
#[test]
fn missing_card_stays_missing() {
    let mut tx = row("tx1", "2026-10-10T00:00:00Z");
    tx.card_id = None;
    let value = serde_json::to_value(
        prepare_feedback(&scope(), &report(vec![tx]), "op1", &intent("NOT_MINE")).unwrap(),
    )
    .unwrap();
    assert!(value["transaction"]["card_id"].is_null());
    assert_eq!(value["lock_authorized"], false);
}
#[test]
fn edited_display_fields_cannot_rebind_original_report_or_transaction() {
    let mut r = ordinary();
    r.metadata.report_id = "new-report".into();
    r.request.window.as_of = "tampered".into();
    r.evidence.as_mut().unwrap()[0].transaction.card_id = Some("wrong-card".into());
    r.evidence.as_mut().unwrap()[0].transaction.amount =
        serde_json::from_value(json!("999.00")).unwrap();
    let value =
        serde_json::to_value(prepare_feedback(&scope(), &r, "op1", &intent("MINE")).unwrap())
            .unwrap();
    assert_eq!(value["report_id"], "report_1");
    assert_eq!(value["transaction"]["amount"], "30.00");
    assert_eq!(value["transaction"]["card_id"], "card_demo");
    assert_eq!(
        value["request"]["window"]["as_of"],
        "2026-10-18T12:00:00+08:00"
    );
    let mut wrong = intent("MINE");
    wrong.report_id = "new-report".into();
    assert!(prepare_feedback(&scope(), &r, "op1", &wrong).is_err());
}
#[test]
fn unrelated_transactions_and_injected_identity_are_rejected() {
    for field in ["transaction_id", "account_id", "source_id"] {
        let mut raw = serde_json::to_value(intent("MINE")).unwrap();
        raw["transaction"][field] = json!("other");
        let wrong = serde_json::from_value(raw).unwrap();
        assert_eq!(
            prepare_feedback(&scope(), &ordinary(), "op1", &wrong).unwrap_err(),
            "TRANSACTION_NOT_IN_REPORT"
        );
    }
    for field in ["user_id", "card_id", "authorized"] {
        let mut raw = serde_json::to_value(intent("MINE")).unwrap();
        raw[field] = json!("injected");
        assert!(serde_json::from_value::<FeedbackIntent>(raw).is_err());
    }
    assert!(prepare_feedback(&scope(), &ordinary(), "bad operation", &intent("MINE")).is_err());
}
#[test]
fn operation_comparison_distinguishes_retry_conflict_and_new_id() {
    let r = ordinary();
    let a = prepare_feedback(&scope(), &r, "op1", &intent("MINE")).unwrap();
    let same = prepare_feedback(&scope(), &r, "op1", &intent("MINE")).unwrap();
    let other = prepare_feedback(&scope(), &r, "op1", &intent("NOT_MINE")).unwrap();
    let new = prepare_feedback(&scope(), &r, "op2", &intent("MINE")).unwrap();
    assert_eq!(
        a.compare_operation(&same),
        OperationComparison::IdenticalRetry
    );
    assert_eq!(a.compare_operation(&other), OperationComparison::Conflict);
    assert_eq!(a.compare_operation(&new), OperationComparison::NewOperation);
}
#[test]
fn refunded_pending_and_transfer_rows_never_become_feedback_targets() {
    for ty in ["refund", "pending", "transfer"] {
        let mut tx = row("tx1", "2026-10-10T00:00:00Z");
        match ty {
            "refund" => {
                tx.transaction_type = TransactionType::Refund;
                tx.direction = Direction::In;
            }
            "transfer" => tx.transaction_type = TransactionType::Transfer,
            _ => {
                tx.status = Status::Processing;
                tx.completed_at = None;
            }
        }
        let r = report(vec![tx]);
        assert!(prepare_feedback(&scope(), &r, "op1", &intent("MINE")).is_err());
        assert!(dup(&r)["pairs"].as_array().unwrap().is_empty());
    }
}
