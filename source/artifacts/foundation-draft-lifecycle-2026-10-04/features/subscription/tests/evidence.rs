use safeflow_subscription_evidence::*;
use serde_json::Value;

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json"
));

fn sample(index: usize) -> Sample {
    load_fixture(FIXTURE).unwrap().samples.remove(index - 1)
}
fn run(s: &Sample) -> Report {
    filter_evidence(&s.trusted_context_fixture, &s.snapshot)
}
fn blocked(s: &Sample, outcome: Outcome) {
    let r = run(s);
    assert_eq!(r.outcome, outcome);
    assert!(r.eligible_evidence.is_empty());
    assert!(r.excluded.is_empty());
}

#[test]
fn all_18_real_samples_have_independent_filter_results() {
    use Outcome::*;
    // This table asserts evidence eligibility only, never the fixture's recognition expectations.
    let expectations = [
        (EvidenceReadyRulePending, 4),
        (EvidenceReadyRulePending, 4),
        (EvidenceReadyRulePending, 4),
        (EvidenceReadyRulePending, 8),
        (EvidenceReadyRulePending, 8),
        (EvidenceReadyRulePending, 4),
        (EvidenceReadyRulePending, 8),
        (EvidenceReadyRulePending, 8),
        (EvidenceReadyRulePending, 1),
        (DataIncomplete, 0),
        (EvidenceReadyRulePending, 4),
        (NeedsMerchantEvidence, 0),
        (ResourceUnavailable, 0),
        (EvidenceReadyRulePending, 4),
        (NoEligibleEvidence, 0),
        (LookupUnavailable, 0),
        (EvidenceReadyRulePending, 2),
        (EvidenceReadyRulePending, 4),
    ];
    let set = load_fixture(FIXTURE).unwrap();
    assert_eq!(set.samples.len(), 18);
    assert_eq!(
        set.samples
            .iter()
            .map(|s| s.snapshot.transactions.len())
            .sum::<usize>(),
        79
    );
    for (s, (outcome, count)) in set.samples.iter().zip(expectations) {
        let r = run(s);
        assert_eq!(r.outcome, outcome, "{}", s.sample_id);
        assert_eq!(r.eligible_evidence.len(), count, "{}", s.sample_id);
        for evidence in &r.eligible_evidence {
            let source = s
                .snapshot
                .transactions
                .iter()
                .find(|t| t.transaction_id == evidence.transaction_id)
                .unwrap();
            assert_eq!(source.status, TransactionStatus::Completed);
            assert_eq!(source.transaction_type, TransactionType::Consumption);
            assert_eq!(source.direction, Direction::Out);
            assert_eq!(
                amount_minor_units(&source.amount),
                Some(evidence.amount_minor_units)
            );
        }
    }
}

#[test]
fn six_counterexamples_are_excluded_with_the_right_reasons() {
    let r = run(&sample(11));
    use Exclusion::*;
    let reasons = [
        NotCompleted,
        NotCompleted,
        NotConsumption,
        NotConsumption,
        NotConsumption,
        OutsideWindow,
    ];
    assert_eq!(
        r.excluded.iter().map(|x| x.reason).collect::<Vec<_>>(),
        reasons
    );
    assert_eq!(
        r.eligible_evidence
            .iter()
            .map(|e| e.transaction_id.as_str())
            .collect::<Vec<_>>(),
        ["M11-OK-1", "M11-OK-2", "M11-OK-3", "M11-OK-4"]
    );
}

#[test]
fn pagination_failure_retains_cursor_but_no_partial_evidence() {
    let s = sample(10);
    blocked(&s, Outcome::DataIncomplete);
    assert_eq!(run(&s).next_cursor.as_deref(), Some("MOCK-CURSOR-PAGE-2"));
}

#[test]
fn empty_success_and_unavailable_source_remain_distinct() {
    assert_eq!(run(&sample(15)).outcome, Outcome::NoEligibleEvidence);
    assert_eq!(run(&sample(16)).outcome, Outcome::LookupUnavailable);
    assert!(run(&sample(15)).message.contains("不表示没有订阅"));
}

#[test]
fn forbidden_scope_returns_no_other_users_details_even_if_paging_failed() {
    let mut s = sample(13);
    s.snapshot.transactions_read = sample(10).snapshot.transactions_read;
    let r = run(&s);
    blocked(&s, Outcome::ResourceUnavailable);
    assert!(r.next_cursor.is_none());
    let serialized = serde_json::to_string(&r).unwrap();
    for secret in ["ACCOUNT-B", "U-B", "M13-"] {
        assert!(!serialized.contains(secret));
    }
}

#[test]
fn injected_unrequested_or_unknown_account_fails_closed() {
    for account in ["ACCOUNT-B", "UNKNOWN"] {
        let mut s = sample(1);
        s.snapshot.transactions[0].account_id = account.into();
        blocked(&s, Outcome::ResourceUnavailable);
    }
    let mut s = sample(1);
    s.snapshot.accounts.push(Account {
        account_id: "OWN-OTHER".into(),
        user_id: "U-A".into(),
    });
    s.snapshot.transactions[0].account_id = "OWN-OTHER".into();
    blocked(&s, Outcome::ResourceUnavailable);
}

#[test]
fn duplicate_or_contradictory_account_map_is_invalid() {
    let mut s = sample(1);
    s.snapshot.accounts.push(Account {
        account_id: "ACCOUNT-A".into(),
        user_id: "U-B".into(),
    });
    blocked(&s, Outcome::InvalidInput);
}

#[test]
fn empty_identity_and_empty_or_duplicate_scope_are_invalid() {
    let mut s = sample(1);
    s.trusted_context_fixture.user_id = " ".into();
    blocked(&s, Outcome::InvalidInput);
    let mut s = sample(1);
    s.trusted_context_fixture.requested_account_ids.clear();
    blocked(&s, Outcome::InvalidInput);
    let mut s = sample(1);
    s.trusted_context_fixture
        .requested_account_ids
        .push("ACCOUNT-A".into());
    blocked(&s, Outcome::InvalidInput);
}

#[test]
fn duplicate_transactions_never_inflate_evidence() {
    let mut s = sample(1);
    s.snapshot
        .transactions
        .push(s.snapshot.transactions[0].clone());
    blocked(&s, Outcome::InvalidInput);
}

#[test]
fn invalid_last_transaction_discards_already_collected_evidence() {
    let mut s = sample(1);
    s.snapshot.transactions.last_mut().unwrap().amount = "NaN".into();
    blocked(&s, Outcome::InvalidInput);
    assert!(run(&s).next_cursor.is_none());
}

#[test]
fn input_order_changes_neither_eligibility_nor_exclusion_reasons() {
    // Source ordering is not a trusted indication of subscription frequency.
    let mut s = sample(11);
    let mut original = run(&s);
    s.snapshot.transactions.reverse();
    let mut reordered = run(&s);
    original
        .eligible_evidence
        .sort_by(|a, b| a.transaction_id.cmp(&b.transaction_id));
    reordered
        .eligible_evidence
        .sort_by(|a, b| a.transaction_id.cmp(&b.transaction_id));
    original
        .excluded
        .sort_by(|a, b| a.transaction_id.cmp(&b.transaction_id));
    reordered
        .excluded
        .sort_by(|a, b| a.transaction_id.cmp(&b.transaction_id));
    assert_eq!(original, reordered);
}

#[test]
fn money_is_positive_exact_and_overflow_checked() {
    assert_eq!(amount_minor_units("20.00"), Some(2000));
    assert_eq!(amount_minor_units("0.01"), Some(1));
    assert_eq!(amount_minor_units("184467440737095516.15"), Some(u64::MAX));
    for value in [
        "0.00",
        "-1.00",
        "+1.00",
        "1e2",
        "NaN",
        "1",
        "1.0",
        "1.001",
        "01.00",
        " 1.00",
        "１.00",
        "184467440737095516.16",
    ] {
        assert_eq!(amount_minor_units(value), None, "{value}");
        let mut s = sample(1);
        s.snapshot.transactions[0].amount = value.into();
        blocked(&s, Outcome::InvalidInput);
    }
}

#[test]
fn mixed_currency_is_not_silently_combined() {
    let mut s = sample(1);
    s.snapshot.transactions[0].currency = "USD".into();
    blocked(&s, Outcome::InvalidInput);
}

#[test]
fn missing_card_and_service_reference_do_not_invalidate_consumption() {
    let r = run(&sample(14));
    assert_eq!(r.eligible_evidence.len(), 4);
    let r = run(&sample(5));
    assert_eq!(r.eligible_evidence.len(), 8);
    assert!(r
        .eligible_evidence
        .iter()
        .all(|e| e.mock_service_ref.is_none()));
}

#[test]
fn missing_merchant_is_explicit_even_with_other_valid_evidence() {
    let mut s = sample(1);
    s.snapshot.transactions[0].merchant_id = None;
    let r = run(&s);
    assert_eq!(r.outcome, Outcome::NeedsMerchantEvidence);
    assert_eq!(r.eligible_evidence.len(), 3);
    assert_eq!(r.excluded[0].reason, Exclusion::MissingMerchant);
}

#[test]
fn blank_identifiers_are_invalid_instead_of_treated_as_null() {
    for field in 0..5 {
        let mut s = sample(1);
        let t = &mut s.snapshot.transactions[0];
        match field {
            0 => t.transaction_id = " ".into(),
            1 => t.card_id = Some(" ".into()),
            2 => t.merchant_id = Some(" ".into()),
            3 => t.mock_service_ref = Some(" ".into()),
            _ => t.category_code = Some(" ".into()),
        }
        blocked(&s, Outcome::InvalidInput);
    }
}

#[test]
fn cancelled_or_incoming_consumption_is_not_evidence() {
    let mut s = sample(1);
    s.snapshot.transactions[0].status = TransactionStatus::Cancelled;
    assert_eq!(run(&s).excluded[0].reason, Exclusion::NotCompleted);
    let mut s = sample(1);
    s.snapshot.transactions[0].direction = Direction::In;
    assert_eq!(run(&s).excluded[0].reason, Exclusion::NotOutgoing);
}

#[test]
fn completion_time_requires_real_date_and_timezone_and_must_not_be_future() {
    for time in [
        None,
        Some("2026-06-31T10:00:00+08:00"),
        Some("2026-06-15T10:00:00"),
        Some("2026-10-02T10:00:00+08:00"),
    ] {
        let mut s = sample(1);
        s.snapshot.transactions[0].completed_at = time.map(str::to_owned);
        blocked(&s, Outcome::InvalidInput);
    }
}

#[test]
fn supplied_window_includes_start_and_excludes_end_in_absolute_time() {
    let mut s = sample(1);
    s.snapshot.transactions[0].completed_at = Some("2026-05-31T16:00:00Z".into());
    s.snapshot.transactions[1].completed_at = Some("2026-09-30T16:00:00Z".into());
    let r = run(&s);
    assert_eq!(r.eligible_evidence.len(), 3);
    assert_eq!(r.excluded[0].transaction_id, "M01-2");
    assert_eq!(r.excluded[0].reason, Exclusion::OutsideWindow);
}

#[test]
fn invalid_or_reversed_window_and_unsupported_timezone_stop_filtering() {
    for field in 0..4 {
        let mut s = sample(1);
        match field {
            0 => s.snapshot.window.start_inclusive = "bad".into(),
            1 => s.snapshot.window.start_inclusive = s.snapshot.window.end_exclusive.clone(),
            2 => s.snapshot.window.as_of = "2026-09-01T00:00:00+08:00".into(),
            _ => s.snapshot.window.timezone = "UTC".into(),
        }
        blocked(&s, Outcome::InvalidInput);
    }
}

#[test]
fn contradictory_completeness_flags_are_invalid() {
    for field in 0..3 {
        let mut s = sample(1);
        let r = &mut s.snapshot.transactions_read;
        match field {
            0 => r.is_complete = false,
            1 => r.next_cursor = Some("more".into()),
            _ => r.reason = Some("PAGE_FAILED".into()),
        }
        blocked(&s, Outcome::InvalidInput);
    }
}

#[test]
fn failed_or_partial_managed_ledger_is_not_treated_as_empty() {
    for state in [ReadKind::Incomplete, ReadKind::Unavailable] {
        let mut s = sample(1);
        s.snapshot.managed_subscriptions_read.state = state;
        s.snapshot.managed_subscriptions_read.is_complete = false;
        blocked(&s, Outcome::LedgerUnavailable);
    }
}

#[test]
fn expected_recognition_text_does_not_drive_execution() {
    let mut value: Value = serde_json::from_str(FIXTURE).unwrap();
    for s in value["samples"].as_array_mut().unwrap() {
        s["expected"] = serde_json::json!({"result_kind":"SUCCESS","candidate_expectations":[],"user_message_proposal":"untrusted"});
    }
    let original = load_fixture(FIXTURE).unwrap();
    let modified = load_fixture(&value.to_string()).unwrap();
    for (a, b) in original.samples.iter().zip(&modified.samples) {
        assert_eq!(run(a), run(b));
    }
}

#[test]
fn existing_ledger_and_similar_services_are_not_filtered_or_merged() {
    for index in [4, 6, 7, 8] {
        let s = sample(index);
        let r = run(&s);
        assert_eq!(r.eligible_evidence.len(), s.snapshot.transactions.len());
        assert_eq!(r.outcome, Outcome::EvidenceReadyRulePending);
    }
}

#[test]
fn annual_and_two_record_cases_do_not_choose_identification_thresholds() {
    for (index, count) in [(9, 1), (17, 2)] {
        let r = run(&sample(index));
        assert_eq!(r.eligible_evidence.len(), count);
        assert_eq!(r.outcome, Outcome::EvidenceReadyRulePending);
        assert!(r.message.contains("尚未识别订阅"));
    }
}

#[test]
fn loader_rejects_broken_json_unknown_enums_and_inconsistent_metadata() {
    assert!(load_fixture("{").is_err());
    for field in ["status", "transaction_type", "direction"] {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v["samples"][0]["snapshot"]["transactions"][0][field] = Value::String("UNKNOWN".into());
        assert!(load_fixture(&v.to_string()).is_err());
    }
    for field in [
        "fictional",
        "production_registration",
        "sample_count",
        "schema_version",
        "rule_version",
    ] {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v[field] = match field {
            "fictional" => false.into(),
            "production_registration" => true.into(),
            "rule_version" => "approved".into(),
            _ => 999.into(),
        };
        assert!(load_fixture(&v.to_string()).is_err());
    }
    let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
    v["samples"][1]["sample_id"] = v["samples"][0]["sample_id"].clone();
    assert!(load_fixture(&v.to_string()).is_err());
}

#[test]
fn cli_replays_original_fixture_and_advertises_mock_only() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_safeflow-subscription-evidence"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["sample_count"], 18);
    assert_eq!(v["recognition_executed"], false);
    assert_eq!(v["reports"].as_array().unwrap().len(), 18);
    assert!(v["rule_version"].is_null());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_safeflow-subscription-evidence"))
        .arg("nonexistent-fixture.json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "FIXTURE_READ_FAILED"
    );
}
