use safeflow_subscription_evidence::{billing_query_mock::load_case, managed_ledger_mock::*, *};
use serde_json::{json, Value};
const CAPTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));

#[test]
fn saved_ledger_scenarios_replay_without_changing_expected_results() {
    let cases: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/managed-ledger-cases.v1.json"
    )))
    .unwrap();
    let expected: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/managed-ledger-replay.v1.json"
    )))
    .unwrap();
    let reports: Vec<_> = cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let s = sample(c["consumption_case"].as_str().unwrap());
            let ledger = load_ledger(&c["ledger"].to_string()).unwrap();
            json!({"case_id":c["case_id"],"report":run(&s, &ledger)})
        })
        .collect();
    assert_eq!(reports.len(), 9);
    assert_eq!(json!(reports), expected["reports"]);
}
fn sample(name: &str) -> Sample {
    load_case(
        CAPTURE,
        name,
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap()
}
fn empty() -> LedgerFixture {
    load_ledger(&json!({"schema_version":1,"fictional":true,"production_registered":false,"owner_user_id":"user_demo","snapshot_id":"mock-ledger-1","read":{"state":"COMPLETE","is_complete":true,"next_cursor":null,"reason":null},"entries":[]}).to_string()).unwrap()
}
fn entry(s: &Sample, status: LedgerStatus) -> LedgerEntry {
    let tx = &s.snapshot.transactions[0];
    LedgerEntry {
        subscription_id: "mock-sub-1".into(),
        owner_user_id: "user_demo".into(),
        source_id: tx.source_id.clone(),
        merchant_id: tx.merchant_id.clone(),
        service_ref: tx.mock_service_ref.clone(),
        status,
    }
}
fn run(s: &Sample, l: &LedgerFixture) -> LedgerReview {
    review_mock(&s.trusted_context_fixture, &s.snapshot, l)
}

#[test]
fn complete_empty_ledger_differs_from_unavailable_or_partial() {
    let s = sample("complete");
    let mut l = empty();
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NotInLedger);
    for state in [ReadKind::Unavailable, ReadKind::Incomplete] {
        l.read.state = state;
        l.read.is_complete = false;
        let r = run(&s, &l);
        assert!(!r.matching_executed);
        assert!(r.groups.is_empty());
    }
}
#[test]
fn active_and_cancellation_intent_remain_registered() {
    let s = sample("complete");
    let mut l = empty();
    for state in [LedgerStatus::Active, LedgerStatus::Cancelling] {
        l.entries = vec![entry(&s, state)];
        let r = run(&s, &l);
        assert_eq!(r.groups[0].disposition, Disposition::AlreadyManaged);
        assert_eq!(r.groups[0].matched_subscription_ids, vec!["mock-sub-1"]);
        assert_eq!(r.groups[0].group.actual_charges.len(), 2);
        assert!(!r.business_writes);
        assert!(!r.production_registered);
    }
}
#[test]
fn same_refs_from_another_source_require_review() {
    let s = sample("complete");
    let mut l = empty();
    let mut e = entry(&s, LedgerStatus::Active);
    e.source_id = Some("fictional-other".into());
    l.entries.push(e);
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NeedsReview);
}
#[test]
fn same_merchant_other_service_is_not_filtered() {
    let s = sample("complete");
    let mut l = empty();
    let mut e = entry(&s, LedgerStatus::Active);
    e.service_ref = Some("different-service".into());
    l.entries.push(e);
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NotInLedger);
}
#[test]
fn scoped_error_blocks_only_related_service() {
    let s = sample("same-merchant-different-services");
    let mut l = empty();
    l.entries.push(entry(&s, LedgerStatus::Error));
    let r = run(&s, &l);
    assert!(r.matching_executed);
    assert_eq!(r.groups.len(), 2);
    assert_eq!(
        r.groups
            .iter()
            .filter(|g| g.disposition == Disposition::ServiceBlocked)
            .count(),
        1
    );
    assert_eq!(
        r.groups
            .iter()
            .filter(|g| g.disposition == Disposition::NotInLedger)
            .count(),
        1
    );
}
#[test]
fn error_with_unknown_scope_blocks_entire_batch() {
    let s = sample("complete");
    let mut l = empty();
    let e = entry(&s, LedgerStatus::Error);
    for i in 0..3 {
        let mut bad = e.clone();
        match i {
            0 => bad.source_id = None,
            1 => bad.merchant_id = None,
            _ => bad.service_ref = None,
        }
        l.entries = vec![bad];
        let r = run(&s, &l);
        assert_eq!(r.reason, "LEDGER_ERROR_SCOPE_UNKNOWN");
        assert!(r.groups.is_empty());
    }
}
#[test]
fn foreign_ledger_rows_never_leak_identifiers() {
    let s = sample("complete");
    let mut l = empty();
    let mut e = entry(&s, LedgerStatus::Active);
    e.owner_user_id = "foreign".into();
    e.subscription_id = "secret".into();
    l.entries.push(e);
    let r = run(&s, &l);
    assert_eq!(r.reason, "RESOURCE_UNAVAILABLE");
    assert!(!serde_json::to_string(&r).unwrap().contains("secret"));
    l.entries.clear();
    l.owner_user_id = "foreign".into();
    assert_eq!(run(&s, &l).reason, "RESOURCE_UNAVAILABLE");
}
#[test]
fn partial_match_keeps_entire_multi_source_group_for_review() {
    let mut s = sample("complete");
    let mut l = empty();
    l.entries.push(entry(&s, LedgerStatus::Active));
    s.snapshot
        .transactions
        .iter_mut()
        .find(|t| t.transaction_id == "fictional-consumption-august")
        .unwrap()
        .source_id = Some("fictional-other".into());
    let r = run(&s, &l);
    assert_eq!(r.groups[0].disposition, Disposition::NeedsReview);
    assert_eq!(r.groups[0].group.actual_charges.len(), 2);
}
#[test]
fn unknown_consumption_identity_is_never_exact_match() {
    let s = sample("same-merchant-unknown-service");
    let l = empty();
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NeedsReview);
    let mut s = sample("complete");
    for t in &mut s.snapshot.transactions {
        t.source_id = None;
    }
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NeedsReview);
}
#[test]
fn malformed_and_production_ledgers_are_rejected() {
    let raw = json!({"schema_version":1,"fictional":true,"production_registered":false,"owner_user_id":"user_demo","snapshot_id":"s","read":{"state":"COMPLETE","is_complete":true},"entries":[]});
    for key in ["fictional", "production_registered"] {
        let mut v = raw.clone();
        v[key] = Value::Bool(key == "production_registered");
        assert!(load_ledger(&v.to_string()).is_err());
    }
    let mut v = raw;
    v["entries"] =
        json!([{"subscription_id":"s","owner_user_id":"user_demo","status":"CANCELLED"}]);
    assert!(load_ledger(&v.to_string()).is_err());
}
#[test]
fn inconsistent_complete_metadata_and_duplicate_ids_block() {
    let s = sample("complete");
    let l = empty();
    let mut bad = l.clone();
    bad.read.next_cursor = Some("next".into());
    assert!(!run(&s, &bad).matching_executed);
    let mut bad = l.clone();
    bad.snapshot_id = None;
    assert!(!run(&s, &bad).matching_executed);
    let mut bad = l;
    let e = entry(&s, LedgerStatus::Active);
    bad.entries = vec![e.clone(), e];
    assert!(!run(&s, &bad).matching_executed);
}
#[test]
fn good_ledger_does_not_bypass_consumption_or_owner_checks() {
    let mut s = sample("complete");
    let l = empty();
    s.snapshot.transactions_read.state = ReadKind::Incomplete;
    s.snapshot.transactions_read.is_complete = false;
    assert!(!run(&s, &l).matching_executed);
    let mut s = sample("complete");
    s.snapshot.transactions[0].account_id = "foreign".into();
    assert!(run(&s, &l).groups.is_empty());
}

#[test]
fn scoped_error_with_uncertain_consumption_is_not_recommended() {
    let mut s = sample("complete");
    let mut l = empty();
    l.entries.push(entry(&s, LedgerStatus::Error));
    for t in &mut s.snapshot.transactions {
        t.mock_service_ref = None;
    }
    assert_eq!(
        run(&s, &l).groups[0].disposition,
        Disposition::ServiceBlocked
    );
}
#[test]
fn cross_source_error_never_counts_as_unregistered_certainty() {
    let s = sample("complete");
    let mut l = empty();
    let mut e = entry(&s, LedgerStatus::Error);
    e.source_id = Some("fictional-other".into());
    l.entries.push(e);
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NeedsReview);
}
#[test]
fn incomplete_ledger_service_key_cannot_hide_candidate() {
    let s = sample("complete");
    let mut l = empty();
    let mut e = entry(&s, LedgerStatus::Active);
    e.service_ref = None;
    l.entries.push(e);
    assert_eq!(run(&s, &l).groups[0].disposition, Disposition::NeedsReview);
}

#[test]
fn same_service_error_overrides_normal_match_independent_of_row_order() {
    let s = sample("complete");
    let mut ledger = empty();
    let normal = entry(&s, LedgerStatus::Active);
    let mut error = entry(&s, LedgerStatus::Error);
    error.subscription_id = "mock-error-record".into();
    ledger.entries = vec![normal, error];
    for _ in 0..2 {
        let result = run(&s, &ledger);
        assert_eq!(result.groups[0].disposition, Disposition::ServiceBlocked);
        assert_eq!(result.groups[0].group.actual_charges.len(), 2);
        ledger.entries.reverse();
    }
}

#[test]
fn late_foreign_record_discards_previous_own_matches() {
    let s = sample("complete");
    let mut ledger = empty();
    let normal = entry(&s, LedgerStatus::Active);
    let mut foreign = normal.clone();
    foreign.subscription_id = "foreign-private-subscription".into();
    foreign.owner_user_id = "another-user".into();
    ledger.entries = vec![normal, foreign];
    let result = run(&s, &ledger);
    assert_eq!(result.reason, "RESOURCE_UNAVAILABLE");
    assert!(!result.matching_executed);
    assert!(result.groups.is_empty());
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains("mock-sub-1"));
    assert!(!serialized.contains("foreign-private-subscription"));
}

#[test]
fn unrelated_merchant_error_does_not_suspend_an_exact_managed_service() {
    let s = sample("complete");
    let mut ledger = empty();
    let normal = entry(&s, LedgerStatus::Cancelling);
    let mut unrelated = entry(&s, LedgerStatus::Error);
    unrelated.subscription_id = "mock-other-error".into();
    unrelated.merchant_id = Some("different-merchant".into());
    ledger.entries = vec![normal, unrelated];
    let result = run(&s, &ledger);
    assert!(result.matching_executed);
    assert_eq!(result.groups[0].disposition, Disposition::AlreadyManaged);
    assert_eq!(
        result.groups[0].matched_subscription_ids,
        vec!["mock-sub-1"]
    );
}
