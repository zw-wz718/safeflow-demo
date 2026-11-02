use safeflow_subscription_evidence::{billing_query_mock::load_case, recognition::*, *};
use serde_json::{json, Value};

const CAPTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));
fn context() -> MockContext {
    MockContext {
        user_id: "user_demo".into(),
        requested_account_ids: vec!["account_001".into()],
    }
}
fn run(s: &Sample) -> RecognitionReport {
    recognize_mock(&s.trusted_context_fixture, &s.snapshot)
}
// Explicit test-only ledger assertion. The adapter itself always returns UNAVAILABLE.
fn test_ledger(s: &mut Sample) {
    s.snapshot.managed_subscriptions_read = ReadState {
        state: ReadKind::Complete,
        is_complete: true,
        next_cursor: None,
        reason: None,
    };
}
fn capture() -> Value {
    serde_json::from_str(CAPTURE).unwrap()
}

#[test]
fn all_twelve_captured_cases_gate_on_source_completion() {
    let raw = capture();
    assert_eq!(raw["cases"].as_array().unwrap().len(), 12);
    for c in raw["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let loaded = load_case(CAPTURE, name, context());
        if c["read"]["state"] == "COMPLETE" {
            let s = loaded.unwrap();
            assert_eq!(run(&s).filter_report.outcome, Outcome::LedgerUnavailable);
            assert!(!run(&s).recognition_executed);
        } else {
            assert!(loaded.is_err(), "{name}");
        }
    }
}

#[test]
fn actual_complete_capture_filters_non_consumption_and_retains_source() {
    let mut s = load_case(CAPTURE, "complete", context()).unwrap();
    test_ledger(&mut s);
    let r = run(&s);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].status, GroupStatus::Suspected);
    assert_eq!(r.groups[0].actual_charges.len(), 2);
    assert!(!r.filter_report.excluded.is_empty());
    assert!(r.groups[0]
        .actual_charges
        .iter()
        .all(|c| c.source_key.is_some()));
}

#[test]
fn different_and_missing_service_references_are_not_guessed() {
    let mut different = load_case(CAPTURE, "same-merchant-different-services", context()).unwrap();
    test_ledger(&mut different);
    let r = run(&different);
    assert_eq!(r.groups.len(), 2);
    assert!(r
        .groups
        .iter()
        .all(|g| g.status == GroupStatus::InsufficientEvidence));
    let mut unknown = load_case(CAPTURE, "same-merchant-unknown-service", context()).unwrap();
    test_ledger(&mut unknown);
    assert_eq!(
        run(&unknown).groups[0].status,
        GroupStatus::NeedsServiceEvidence
    );
}

#[test]
fn local_variant_missing_category_and_cross_account_same_number() {
    let mut raw = capture();
    let c = &mut raw["cases"][0];
    c["request"]["account_ids"] = json!(["account_001", "account_002"]);
    let rows = c["read"]["observed_transactions"].as_array_mut().unwrap();
    let mut n = 0;
    for row in rows {
        row.as_object_mut().unwrap().remove("category_code");
        if row["transaction_type"] == "CONSUMPTION" && row["status"] == "COMPLETED" {
            row["transaction_id"] = json!("001");
            row["account_id"] = json!(if n == 0 { "account_001" } else { "account_002" });
            n += 1;
        }
    }
    let mut ctx = context();
    ctx.requested_account_ids.push("account_002".into());
    let mut s = load_case(&raw.to_string(), "complete", ctx).unwrap();
    assert!(s
        .snapshot
        .transactions
        .iter()
        .all(|t| t.category_code.is_none()));
    assert_eq!(run(&s).filter_report.outcome, Outcome::LedgerUnavailable);
    test_ledger(&mut s);
    let r = run(&s);
    assert_eq!(r.groups[0].account_ids, vec!["account_001", "account_002"]);
    assert!(r.groups[0]
        .actual_charges
        .iter()
        .all(|c| c.transaction_id == "001"));
}

#[test]
fn mock_adapter_rejects_scope_changes_and_real_input() {
    let mut raw = capture();
    raw["real_consumption_source_connected"] = json!(true);
    assert!(load_case(&raw.to_string(), "complete", context()).is_err());
    let mut raw = capture();
    raw["cases"][0]["read"]["observed_transactions"][0]["account_id"] = json!("foreign");
    assert!(load_case(&raw.to_string(), "complete", context()).is_err());
    let mut ctx = context();
    ctx.requested_account_ids.push("unexpected".into());
    assert!(load_case(CAPTURE, "complete", ctx).is_err());
}

#[test]
fn mock_completion_must_have_terminal_metadata() {
    for (key, value) in [
        ("next_cursor", json!("next")),
        ("reason", json!("error")),
        ("snapshot_id", json!("")),
        ("pages_read", json!(0)),
        ("is_complete", json!(false)),
    ] {
        let mut raw = capture();
        raw["cases"][0]["read"][key] = value;
        assert!(
            load_case(&raw.to_string(), "complete", context()).is_err(),
            "{key}"
        );
    }
    let mut raw = capture();
    raw["cases"][0]["read"]
        .as_object_mut()
        .unwrap()
        .remove("next_cursor");
    assert!(load_case(&raw.to_string(), "complete", context()).is_err());
}

#[test]
fn supplied_export_or_fake_ledger_cannot_enable_recognition() {
    let mut raw = capture();
    raw["cases"][0]["subscription_snapshot"]["managed_subscriptions_read"] =
        json!({"state":"COMPLETE","is_complete":true,"next_cursor":null,"reason":null});
    raw["cases"][0]["read"]["eligible_consumption"] = json!([]);
    let s = load_case(&raw.to_string(), "complete", context()).unwrap();
    assert!(!s.snapshot.transactions.is_empty());
    assert_eq!(run(&s).filter_report.outcome, Outcome::LedgerUnavailable);
}

#[test]
fn conflicting_row_source_is_rejected_instead_of_silently_relabelled() {
    for source in [json!("fictional-other-source"), Value::Null, json!(42)] {
        let mut raw = capture();
        let rows = raw["cases"][0]["read"]["observed_transactions"]
            .as_array_mut()
            .unwrap();
        // A conflict after valid rows must reject the entire handoff.
        rows.last_mut().unwrap()["source_id"] = source;
        assert_eq!(
            load_case(&raw.to_string(), "complete", context()).unwrap_err(),
            "TRANSACTION_REFERENCE_CONFLICT"
        );
    }
}

#[test]
fn conflicting_service_alias_is_rejected_instead_of_silently_replaced() {
    for service in [json!("fictional-other-service"), Value::Null, json!(42)] {
        let mut raw = capture();
        raw["cases"][0]["read"]["observed_transactions"][0]["mock_service_ref"] = service;
        assert_eq!(
            load_case(&raw.to_string(), "complete", context()).unwrap_err(),
            "TRANSACTION_REFERENCE_CONFLICT"
        );
    }
    let mut raw = capture();
    let row = &mut raw["cases"][0]["read"]["observed_transactions"][0];
    row.as_object_mut().unwrap().remove("service_ref");
    row["mock_service_ref"] = json!("fictional-injected-service");
    assert_eq!(
        load_case(&raw.to_string(), "complete", context()).unwrap_err(),
        "TRANSACTION_REFERENCE_CONFLICT"
    );
}

#[test]
fn consistent_redundant_references_preserve_the_original_handoff() {
    let expected = load_case(CAPTURE, "complete", context()).unwrap();
    let mut raw = capture();
    let source = raw["cases"][0]["read"]["source_id"].clone();
    for row in raw["cases"][0]["read"]["observed_transactions"]
        .as_array_mut()
        .unwrap()
    {
        row["source_id"] = source.clone();
        row["mock_service_ref"] = row.get("service_ref").cloned().unwrap_or(Value::Null);
    }
    let actual = load_case(&raw.to_string(), "complete", context()).unwrap();
    assert_eq!(run(&actual).filter_report, run(&expected).filter_report);
    assert_eq!(
        actual.snapshot.transactions.len(),
        expected.snapshot.transactions.len()
    );
    for (actual, expected) in actual
        .snapshot
        .transactions
        .iter()
        .zip(&expected.snapshot.transactions)
    {
        assert_eq!(actual.source_id, expected.source_id);
        assert_eq!(actual.mock_service_ref, expected.mock_service_ref);
    }
    assert_eq!(
        run(&actual).filter_report.outcome,
        Outcome::LedgerUnavailable
    );
}
