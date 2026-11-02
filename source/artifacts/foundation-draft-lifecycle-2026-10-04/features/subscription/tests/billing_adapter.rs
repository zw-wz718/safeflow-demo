use safeflow_subscription_evidence::billing_adapter::{
    normalize_bill_read, ConsumptionCheck, SourceKey,
};
use serde_json::{json, Value};

fn input() -> Value {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../fixtures/subscription/peer_sources/transfer_mock_200_2a72b5d.json"
    ))
    .unwrap();
    json!({"source_id":"transfer-mock:2a72b5d", "entries":[{
        "transaction":fixture["success_debit_transaction"],
        "consumption":"TRANSFER_EXCLUDED", "card_note":"流水未提供对应卡片"
    }], "added":1, "unchanged":0, "excluded_out_of_scope":0, "subscription_evidence":"UNAVAILABLE"})
}
fn normalize(
    v: &Value,
) -> Result<safeflow_subscription_evidence::billing_adapter::NormalizedRead, &'static str> {
    normalize_bill_read(&v.to_string())
}

#[test]
fn real_transfer_fields_survive_without_inventing_category_or_service() {
    let v = input();
    let out = normalize(&v).unwrap();
    let row = &out.entries[0];
    assert_eq!(
        row.key.transaction_id,
        v["entries"][0]["transaction"]["transaction_id"]
    );
    assert_eq!(
        row.operation_id,
        v["entries"][0]["transaction"]["operation_id"]
    );
    assert_eq!(row.category_code, None);
    assert_eq!(row.card_id, None);
    assert_eq!(row.original_amount, "200.00");
    assert_eq!(row.canonical_amount, "200.00");
    assert_eq!(row.consumption_check, ConsumptionCheck::TransferExcluded);
    assert_eq!(out.subscription_evidence, "UNAVAILABLE");
    assert!(!out.recognition_executed);
    assert!(!out.production_registered);
}

#[test]
fn duplicate_ids_across_accounts_are_distinct_but_same_key_is_rejected() {
    let mut v = input();
    let mut second = v["entries"][0].clone();
    second["transaction"]["account_id"] = json!("another-account");
    v["entries"].as_array_mut().unwrap().push(second);
    let out = normalize(&v).unwrap();
    assert_ne!(out.entries[0].key, out.entries[1].key);
    assert_eq!(
        out.entries[0].key.transaction_id,
        out.entries[1].key.transaction_id
    );
    v["entries"][1] = v["entries"][0].clone();
    assert_eq!(normalize(&v).unwrap_err(), "DUPLICATE_SOURCE_KEY");
}

#[test]
fn key_keeps_source_and_does_not_join_components_with_ambiguous_separators() {
    let a = SourceKey {
        source_id: "s:a".into(),
        account_id: "b".into(),
        transaction_id: "c".into(),
    };
    let b = SourceKey {
        source_id: "s".into(),
        account_id: "a:b".into(),
        transaction_id: "c".into(),
    };
    assert_ne!(a, b);
    let mut v = input();
    let first = normalize(&v).unwrap();
    v["source_id"] = json!("other-source");
    assert_ne!(first.entries[0].key, normalize(&v).unwrap().entries[0].key);
}

#[test]
fn canonical_amount_retains_original_and_checks_exact_positive_cents() {
    for (raw, canonical, cents) in [
        ("0200.00", "200.00", 20000),
        ("000.01", "0.01", 1),
        ("184467440737095516.15", "184467440737095516.15", u64::MAX),
    ] {
        let mut v = input();
        v["entries"][0]["transaction"]["amount"] = json!(raw);
        let out = normalize(&v).unwrap();
        let row = &out.entries[0];
        assert_eq!(row.original_amount, raw);
        assert_eq!(row.canonical_amount, canonical);
        assert_eq!(row.amount_minor_units, cents);
    }
    for raw in [
        "0.00",
        "-1.00",
        " 2.00",
        "2.0",
        "2.000",
        "1e2.00",
        "184467440737095516.16",
        ".01",
    ] {
        let mut v = input();
        v["entries"][0]["transaction"]["amount"] = json!(raw);
        assert!(normalize(&v).is_err(), "{raw}");
    }
}

#[test]
fn category_is_optional_but_does_not_make_unknown_types_consumption() {
    let mut v = input();
    v["entries"][0]["transaction"]["category_code"] = Value::Null;
    assert!(normalize(&v).is_ok());
    v["entries"][0]["transaction"]["transaction_type"] = json!("FUTURE_TYPE");
    v["entries"][0]["consumption"] = json!("UNASSESSED_SOURCE_TYPE");
    assert_eq!(
        normalize(&v).unwrap().entries[0].consumption_check,
        ConsumptionCheck::UnassessedSourceType
    );
    v["entries"][0]["transaction"]["transaction_type"] = json!("CONSUMPTION");
    assert_eq!(
        normalize(&v).unwrap().entries[0].consumption_check,
        ConsumptionCheck::ConsumptionOutgoingNeedsSourceVerification
    );
    v["entries"][0]["transaction"]["direction"] = json!("CREDIT");
    let out = normalize(&v).unwrap();
    assert_eq!(out.entries[0].direction, "IN");
    assert_eq!(
        out.entries[0].consumption_check,
        ConsumptionCheck::NotOutgoingConsumption
    );
}

#[test]
fn errors_and_scope_counts_cannot_be_upgraded_to_complete_evidence() {
    let mut v = input();
    v["excluded_out_of_scope"] = json!(2);
    let out = normalize(&v).unwrap();
    assert_eq!(out.excluded_out_of_scope, 2);
    assert_eq!(out.subscription_evidence, "UNAVAILABLE");
    v["subscription_evidence"] = json!("COMPLETE");
    assert!(normalize(&v).is_err());
    let mut v = input();
    v["entries"] = json!([]);
    v["added"] = json!(0);
    assert_eq!(normalize(&v).unwrap().subscription_evidence, "UNAVAILABLE");
}

#[test]
fn bad_last_row_and_invalid_dates_fail_entire_conversion() {
    let mut v = input();
    let mut bad = v["entries"][0].clone();
    bad["transaction"]["transaction_id"] = json!("last");
    bad["transaction"]["completed_at"] = json!("2026-02-30T00:00:00+08:00");
    v["entries"].as_array_mut().unwrap().push(bad);
    assert_eq!(normalize(&v).unwrap_err(), "INVALID_COMPLETION_TIME");
    for stamp in ["not-a-date", "2026-10-13T00:00:00"] {
        v["entries"][1]["transaction"]["completed_at"] = json!(stamp);
        assert!(normalize(&v).is_err());
    }
}

#[test]
fn contradictory_assessment_unknown_direction_and_status_are_rejected() {
    for (field, value) in [
        ("direction", "OUT"),
        ("currency", "USD"),
        ("status", "PROCESSING"),
    ] {
        let mut v = input();
        v["entries"][0]["transaction"][field] = json!(value);
        assert!(normalize(&v).is_err());
    }
    let mut v = input();
    v["entries"][0]["consumption"] = json!("UNASSESSED_SOURCE_TYPE");
    assert_eq!(
        normalize(&v).unwrap_err(),
        "CONTRADICTORY_CONSUMPTION_ASSESSMENT"
    );
}

#[test]
fn missing_source_references_are_not_silently_filled_with_null() {
    for field in ["card_id", "merchant_id"] {
        let mut v = input();
        v["entries"][0]["transaction"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert_eq!(normalize(&v).unwrap_err(), "BILL_READ_PARSE_FAILED");
    }
}

#[test]
fn captured_actual_billing_outputs_replay_without_claiming_consumption_evidence() {
    let artifact: Value = serde_json::from_str(include_str!(
        "../../../../../fixtures/subscription/SafeFlow_SUB-01_账单输入转换实测_v0.1.json"
    ))
    .unwrap();
    assert_eq!(
        artifact["billing_commit"],
        "3beed995bd8ed93517899144c3957d7928da45de"
    );
    let cases = artifact["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    for (case, count) in cases.iter().zip([1, 0, 0, 1]) {
        let report = normalize(&case["input"]).unwrap();
        assert_eq!(report.entries.len(), count);
        assert_eq!(report.subscription_evidence, "UNAVAILABLE");
        assert!(!report.recognition_executed);
        assert!(report
            .entries
            .iter()
            .all(|row| row.consumption_check == ConsumptionCheck::TransferExcluded));
        assert_eq!(serde_json::to_value(report).unwrap(), case["normalized"]);
    }
}
