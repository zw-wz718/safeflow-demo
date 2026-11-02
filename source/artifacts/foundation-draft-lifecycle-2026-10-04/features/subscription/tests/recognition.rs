use safeflow_subscription_evidence::recognition::*;
use safeflow_subscription_evidence::*;

fn complete() -> ReadState {
    ReadState {
        state: ReadKind::Complete,
        is_complete: true,
        next_cursor: None,
        reason: None,
    }
}

fn case_at(as_of: &str, rows: &[(&str, &str)]) -> Sample {
    Sample {
        sample_id: "LOCAL-RECOGNITION-TEST".into(),
        trusted_context_fixture: MockContext {
            user_id: "U".into(),
            requested_account_ids: vec!["A".into(), "B".into()],
        },
        snapshot: Snapshot {
            window: rolling_four_month_window(as_of).unwrap(),
            accounts: vec![
                Account {
                    account_id: "A".into(),
                    user_id: "U".into(),
                },
                Account {
                    account_id: "B".into(),
                    user_id: "U".into(),
                },
            ],
            transactions_read: complete(),
            managed_subscriptions_read: complete(),
            transactions: rows
                .iter()
                .enumerate()
                .map(|(i, (date, amount))| Transaction {
                    source_id: None,
                    transaction_id: format!("T{i}"),
                    account_id: "A".into(),
                    card_id: None,
                    amount: (*amount).into(),
                    currency: "CNY".into(),
                    direction: Direction::Out,
                    transaction_type: TransactionType::Consumption,
                    status: TransactionStatus::Completed,
                    completed_at: Some(format!("{date}T12:00:00+08:00")),
                    merchant_id: Some("MERCHANT".into()),
                    mock_service_ref: Some("SERVICE".into()),
                    category_code: Some("MOCK".into()),
                })
                .collect(),
        },
    }
}

fn case(rows: &[(&str, &str)]) -> Sample {
    case_at("2026-10-12T12:00:00+08:00", rows)
}

fn run(s: &Sample) -> RecognitionReport {
    recognize_mock(&s.trusted_context_fixture, &s.snapshot)
}

fn cycle(report: &RecognitionReport, cycle: Cycle) -> &CycleAssessment {
    report.groups[0]
        .cycles
        .iter()
        .find(|c| c.cycle == cycle)
        .unwrap()
}

#[test]
fn two_actual_charges_are_enough_for_a_monthly_suspicion() {
    let report = run(&case(&[("2026-07-15", "20.00"), ("2026-08-15", "20.00")]));
    assert_eq!(report.status, RecognitionStatus::Reviewed);
    assert_eq!(report.groups[0].status, GroupStatus::Suspected);
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::Matches
    );
    assert!(!report.production_registered);
    assert!(!report.managed_service_filter_applied);
}

#[test]
fn one_charge_remains_insufficient() {
    let report = run(&case(&[("2026-08-15", "20.00")]));
    assert_eq!(report.groups[0].status, GroupStatus::InsufficientEvidence);
    assert!(report.groups[0].cycles.is_empty());
}

#[test]
fn amount_changes_do_not_split_or_reject_the_same_service() {
    let report = run(&case(&[("2026-07-15", "0.01"), ("2026-08-15", "9000.00")]));
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].status, GroupStatus::Suspected);
    assert_eq!(
        report.groups[0]
            .actual_charges
            .iter()
            .map(|c| c.amount_minor_units)
            .collect::<Vec<_>>(),
        [1, 900_000]
    );
}

#[test]
fn invalid_money_is_still_rejected() {
    for amount in ["0.00", "-2.00", "10.001", "NaN"] {
        let report = run(&case(&[("2026-07-15", amount), ("2026-08-15", "20.00")]));
        assert_eq!(report.status, RecognitionStatus::EvidenceBlocked);
        assert!(report.groups.is_empty());
    }
}

#[test]
fn one_missing_month_is_allowed_without_synthetic_charges() {
    let report = run(&case(&[("2026-06-15", "20.00"), ("2026-08-15", "30.00")]));
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::Matches
    );
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).observed_period_indices,
        [0, 2]
    );
    assert_eq!(cycle(&report, Cycle::CalendarMonth).missing_period_count, 1);
    assert_eq!(report.groups[0].actual_charges.len(), 2);
}

#[test]
fn each_adjacent_pair_can_skip_one_period() {
    let report = run(&case(&[
        ("2026-09-01", "10.00"),
        ("2026-09-03", "30.00"),
        ("2026-09-05", "50.00"),
    ]));
    assert_eq!(cycle(&report, Cycle::Day).status, CycleStatus::Matches);
    assert_eq!(cycle(&report, Cycle::Day).missing_period_count, 2);
    assert_eq!(report.groups[0].actual_charges.len(), 3);
}

#[test]
fn two_missing_months_do_not_pass_as_monthly() {
    let report = run(&case(&[("2026-06-15", "20.00"), ("2026-09-15", "20.00")]));
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::DoesNotMatch
    );
    assert_eq!(report.groups[0].status, GroupStatus::NoSupportedCycle);
}

#[test]
fn month_end_clamping_keeps_the_original_day() {
    for (as_of, rows) in [
        (
            "2027-04-12T12:00:00+08:00",
            [
                ("2027-01-31", "10.00"),
                ("2027-02-28", "11.00"),
                ("2027-03-31", "30.00"),
            ],
        ),
        (
            "2028-04-12T12:00:00+08:00",
            [
                ("2028-01-31", "10.00"),
                ("2028-02-29", "11.00"),
                ("2028-03-31", "30.00"),
            ],
        ),
        (
            "2027-04-12T12:00:00+08:00",
            [
                ("2027-01-30", "10.00"),
                ("2027-02-28", "11.00"),
                ("2027-03-30", "30.00"),
            ],
        ),
    ] {
        let report = run(&case_at(as_of, &rows));
        assert_eq!(
            cycle(&report, Cycle::CalendarMonth).status,
            CycleStatus::Matches
        );
        assert_eq!(
            cycle(&report, Cycle::CalendarMonth).observed_period_indices,
            [0, 1, 2]
        );
    }
}

#[test]
fn monthly_tolerance_includes_both_three_day_edges_but_not_four() {
    for (day, expected) in [
        (12, CycleStatus::Matches),
        (18, CycleStatus::Matches),
        (11, CycleStatus::DoesNotMatch),
        (19, CycleStatus::DoesNotMatch),
    ] {
        let date = format!("2026-08-{day:02}");
        let report = run(&case(&[("2026-07-15", "10.00"), (&date, "10.00")]));
        assert_eq!(cycle(&report, Cycle::CalendarMonth).status, expected);
    }
}

#[test]
fn early_month_charge_can_belong_to_the_previous_planned_month() {
    let report = run(&case_at(
        "2027-04-12T12:00:00+08:00",
        &[
            ("2027-01-31", "10.00"),
            ("2027-03-01", "11.00"),
            ("2027-03-31", "30.00"),
        ],
    ));
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::Matches
    );
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).observed_period_indices,
        [0, 1, 2]
    );
}

#[test]
fn monthly_delays_do_not_accumulate_and_no_subset_is_silently_selected() {
    let report = run(&case(&[
        ("2026-06-15", "10.00"),
        ("2026-07-18", "10.00"),
        ("2026-08-21", "10.00"),
    ]));
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::DoesNotMatch
    );
    assert_eq!(report.groups[0].actual_charges.len(), 3);
    assert_eq!(report.groups[0].status, GroupStatus::NoSupportedCycle);
}

#[test]
fn weekly_one_day_tolerance_is_anchored() {
    let report = run(&case(&[
        ("2026-08-01", "10.00"),
        ("2026-08-08", "10.00"),
        ("2026-08-16", "10.00"),
    ]));
    assert_eq!(cycle(&report, Cycle::Week).status, CycleStatus::Matches);
    let report = run(&case(&[
        ("2026-08-01", "10.00"),
        ("2026-08-09", "10.00"),
        ("2026-08-17", "10.00"),
    ]));
    assert_eq!(
        cycle(&report, Cycle::Week).status,
        CycleStatus::DoesNotMatch
    );
}

#[test]
fn daily_series_within_a_month_is_not_labelled_duplicate_monthly_charging() {
    let report = run(&case(&[("2026-09-01", "10.00"), ("2026-09-02", "10.00")]));
    assert_eq!(cycle(&report, Cycle::Day).status, CycleStatus::Matches);
    assert_eq!(report.groups[0].status, GroupStatus::Suspected);
}

#[test]
fn multiple_charges_in_one_month_are_kept_for_review() {
    let report = run(&case(&[
        ("2026-06-15", "10.00"),
        ("2026-07-15", "10.00"),
        ("2026-07-16", "10.00"),
    ]));
    assert_eq!(report.groups[0].status, GroupStatus::NeedsReview);
    assert!(report.groups[0].message.contains("有多笔扣款"));
    assert_eq!(report.groups[0].actual_charges.len(), 3);
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::MultipleChargesInPeriod
    );
}

#[test]
fn same_day_different_transaction_ids_are_not_silently_deduplicated() {
    let report = run(&case(&[("2026-09-01", "10.00"), ("2026-09-01", "10.00")]));
    assert_eq!(report.groups[0].status, GroupStatus::NeedsReview);
    assert_eq!(report.groups[0].actual_charges.len(), 2);
}

#[test]
fn same_day_charges_keep_review_warning_even_with_irregular_later_charge() {
    let mut sample = case(&[
        ("2026-07-15", "20.00"),
        ("2026-07-15", "30.00"),
        ("2026-08-19", "20.00"),
    ]);
    // Different timestamps, same Asia/Shanghai business day.
    sample.snapshot.transactions[1].completed_at = Some("2026-07-15T15:00:00Z".into());
    for reverse in [false, true] {
        if reverse {
            sample.snapshot.transactions.reverse();
        }
        let report = run(&sample);
        let group = &report.groups[0];
        assert_eq!(group.status, GroupStatus::NeedsReview);
        assert!(group.message.contains("有多笔扣款，请核对"));
        assert_eq!(group.actual_charges.len(), 3);
        assert_eq!(
            group
                .actual_charges
                .iter()
                .map(|c| c.transaction_id.as_str())
                .collect::<Vec<_>>(),
            vec!["T0", "T1", "T2"]
        );
        assert!(group
            .cycles
            .iter()
            .all(|c| c.status == CycleStatus::DoesNotMatch));
        assert!(!report.production_registered);
    }
}

#[test]
fn irregular_charges_on_distinct_business_days_do_not_claim_duplicate_charges() {
    let report = run(&case(&[("2026-07-15", "20.00"), ("2026-08-19", "20.00")]));
    assert_eq!(report.groups[0].status, GroupStatus::NoSupportedCycle);
    assert!(!report.groups[0].message.contains("有多笔扣款"));
}

#[test]
fn trusted_mock_same_service_merges_across_owned_accounts() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "50.00")]);
    sample.snapshot.transactions[1].account_id = "B".into();
    let report = run(&sample);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].account_ids, ["A", "B"]);
    assert_eq!(report.groups[0].status, GroupStatus::Suspected);
}

#[test]
fn different_services_at_same_merchant_remain_separate_even_at_same_price() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    sample.snapshot.transactions[1].mock_service_ref = Some("OTHER-SERVICE".into());
    let report = run(&sample);
    assert_eq!(report.groups.len(), 2);
    assert!(report
        .groups
        .iter()
        .all(|g| g.status == GroupStatus::InsufficientEvidence));
}

#[test]
fn service_reference_is_scoped_by_merchant() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    sample.snapshot.transactions[1].merchant_id = Some("OTHER-MERCHANT".into());
    let report = run(&sample);
    assert_eq!(report.groups.len(), 2);
}

#[test]
fn unknown_service_does_not_merge_accounts_into_a_determined_subscription() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    sample.snapshot.transactions[1].account_id = "B".into();
    sample.snapshot.transactions[1].mock_service_ref = None;
    let report = run(&sample);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].status, GroupStatus::NeedsServiceEvidence);
    assert!(report.groups[0].mock_service_ref.is_none());
    assert!(report.groups[0].cycles.is_empty());
    assert_eq!(report.groups[0].actual_charges.len(), 2);
}

#[test]
fn foreign_or_unrequested_accounts_never_expose_group_ids() {
    for foreign_owner in [true, false] {
        let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
        sample.snapshot.transactions[1].account_id = "B".into();
        if foreign_owner {
            sample.snapshot.accounts[1].user_id = "FOREIGN".into();
        } else {
            sample.trusted_context_fixture.requested_account_ids = vec!["A".into()];
        }
        let report = run(&sample);
        assert_eq!(report.status, RecognitionStatus::EvidenceBlocked);
        assert!(report.groups.is_empty());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("MERCHANT"));
        assert!(!json.contains("SERVICE"));
    }
}

#[test]
fn read_failures_do_not_become_skipped_periods() {
    for state in [ReadKind::Incomplete, ReadKind::Unavailable] {
        let mut sample = case(&[("2026-06-15", "10.00"), ("2026-08-15", "10.00")]);
        sample.snapshot.transactions_read.state = state;
        sample.snapshot.transactions_read.is_complete = false;
        let report = run(&sample);
        assert_eq!(report.status, RecognitionStatus::EvidenceBlocked);
        assert!(report.groups.is_empty());
    }
}

#[test]
fn duplicate_source_ids_do_not_count_twice() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    sample.snapshot.transactions[1].transaction_id = "T0".into();
    let report = run(&sample);
    assert_eq!(report.filter_report.outcome, Outcome::InvalidInput);
    assert!(report.groups.is_empty());
}

#[test]
fn completed_transfer_is_never_counted_as_subscription_evidence() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    for tx in &mut sample.snapshot.transactions {
        tx.transaction_type = TransactionType::Transfer;
    }
    let report = run(&sample);
    assert_eq!(report.status, RecognitionStatus::Reviewed);
    assert!(report.groups.is_empty());
    assert_eq!(report.filter_report.excluded.len(), 2);
}

#[test]
fn null_card_does_not_invalidate_consumption_and_missing_merchant_stops_recognition() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    assert_eq!(run(&sample).groups[0].status, GroupStatus::Suspected);
    sample.snapshot.transactions[1].merchant_id = None;
    let report = run(&sample);
    assert_eq!(report.status, RecognitionStatus::EvidenceBlocked);
    assert_eq!(report.filter_report.outcome, Outcome::NeedsMerchantEvidence);
    assert!(report.groups.is_empty());
}

#[test]
fn rolling_window_preserves_time_clamps_short_months_and_normalizes_offsets() {
    let window = rolling_four_month_window("2027-06-30T04:20:30.123Z").unwrap();
    assert_eq!(window.start_inclusive, "2027-02-28T12:20:30.123+08:00");
    assert_eq!(window.end_exclusive, "2027-06-30T12:20:30.123+08:00");
    assert_eq!(
        rolling_four_month_window("2028-06-30T12:00:00+08:00")
            .unwrap()
            .start_inclusive,
        "2028-02-29T12:00:00+08:00"
    );
    assert!(rolling_four_month_window("not-a-time").is_none());
}

#[test]
fn window_contains_start_but_not_query_instant() {
    let report = run(&case(&[
        ("2026-06-12", "10.00"),
        ("2026-07-12", "10.00"),
        ("2026-10-12", "10.00"),
    ]));
    assert_eq!(report.groups[0].actual_charges.len(), 2);
    assert_eq!(report.groups[0].actual_charges[0].transaction_id, "T0");
    assert_eq!(report.filter_report.excluded[0].transaction_id, "T2");
    assert_eq!(
        report.filter_report.excluded[0].reason,
        Exclusion::OutsideWindow
    );
}

#[test]
fn business_dates_use_shanghai_instead_of_timestamp_display_offset() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    sample.snapshot.transactions[1].completed_at = Some("2026-08-14T16:00:00Z".into());
    let report = run(&sample);
    assert_eq!(
        cycle(&report, Cycle::CalendarMonth).status,
        CycleStatus::Matches
    );
}

#[test]
fn shorter_complete_window_is_not_pretended_to_be_four_months() {
    let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "10.00")]);
    sample.snapshot.window.start_inclusive = "2026-07-01T00:00:00+08:00".into();
    let report = run(&sample);
    assert_eq!(report.status, RecognitionStatus::InvalidRollingWindow);
    assert!(report.groups.is_empty());
}

#[test]
fn input_order_does_not_change_groups_or_lose_transaction_references() {
    let mut sample = case(&[
        ("2026-06-15", "10.00"),
        ("2026-07-15", "200.00"),
        ("2026-08-15", "1.00"),
    ]);
    let first = run(&sample).groups;
    sample.snapshot.transactions.reverse();
    assert_eq!(run(&sample).groups, first);
}

#[test]
fn existing_18_fixtures_are_reused_without_treating_expected_as_results() {
    let fixtures = load_fixture(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json"
    )))
    .unwrap();
    for sample in fixtures.samples {
        let result = run(&sample);
        assert!(!result.production_registered);
        assert!(!result.managed_service_filter_applied);
        for group in result.groups {
            for charge in group.actual_charges {
                let tx = sample
                    .snapshot
                    .transactions
                    .iter()
                    .find(|tx| tx.transaction_id == charge.transaction_id)
                    .unwrap();
                assert_eq!(tx.status, TransactionStatus::Completed);
                assert_eq!(tx.transaction_type, TransactionType::Consumption);
                assert_eq!(tx.direction, Direction::Out);
                assert_eq!(
                    charge.amount_minor_units,
                    amount_minor_units(&tx.amount).unwrap()
                );
            }
        }
    }
}

#[test]
fn reports_distinguish_local_execution_from_production_and_annual_support() {
    let sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "9000.00")]);
    let report = run(&sample);
    assert!(report.recognition_executed);
    assert!(!report.team_contract_approved);
    assert!(!report.annual_recognition_supported);
    assert!(!report.managed_service_filter_applied);
    assert_eq!(report.rules.minimum_actual_charges, 2);
    assert!(!report.rules.amount_changes_gate_recognition);
    assert_eq!(report.groups[0].status, GroupStatus::Suspected);
    let mut incomplete = sample;
    incomplete.snapshot.transactions_read.state = ReadKind::Incomplete;
    incomplete.snapshot.transactions_read.is_complete = false;
    assert!(!run(&incomplete).recognition_executed);
}

#[test]
fn incomplete_or_unavailable_managed_ledger_prevents_determined_results() {
    for state in [ReadKind::Incomplete, ReadKind::Unavailable] {
        let mut sample = case(&[("2026-07-15", "10.00"), ("2026-08-15", "20.00")]);
        sample.snapshot.managed_subscriptions_read.state = state;
        sample.snapshot.managed_subscriptions_read.is_complete = false;
        let report = run(&sample);
        assert_eq!(report.filter_report.outcome, Outcome::LedgerUnavailable);
        assert!(!report.recognition_executed);
        assert!(report.groups.is_empty());
    }
}

#[test]
fn weekly_tolerance_applies_to_skipped_periods_and_both_edges() {
    for (day, status) in [
        (13, CycleStatus::DoesNotMatch),
        (14, CycleStatus::Matches),
        (16, CycleStatus::Matches),
        (17, CycleStatus::DoesNotMatch),
        (22, CycleStatus::DoesNotMatch),
    ] {
        let date = format!("2026-08-{day:02}");
        let report = run(&case(&[("2026-08-01", "10.00"), (&date, "100.00")]));
        let assessment = cycle(&report, Cycle::Week);
        assert_eq!(assessment.status, status, "{date}");
        if status == CycleStatus::Matches {
            assert_eq!(assessment.observed_period_indices, [0, 2]);
            assert_eq!(assessment.missing_period_count, 1);
        }
    }
}

#[test]
fn missing_service_never_uses_price_to_synthesize_separate_subscriptions() {
    let mut sample = case(&[
        ("2026-07-15", "10.00"),
        ("2026-07-15", "300.00"),
        ("2026-08-15", "10.00"),
        ("2026-08-15", "300.00"),
    ]);
    for tx in &mut sample.snapshot.transactions {
        tx.mock_service_ref = None;
    }
    let report = run(&sample);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].status, GroupStatus::NeedsServiceEvidence);
    assert_eq!(report.groups[0].actual_charges.len(), 4);
    assert!(report.groups[0].cycles.is_empty());
}

#[test]
fn changing_historical_expectations_cannot_change_recognition() {
    let input = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json"
    ));
    let expected = replay_mock(input).unwrap();
    let mut modified: serde_json::Value = serde_json::from_str(input).unwrap();
    for sample in modified["samples"].as_array_mut().unwrap() {
        sample["expected"] = serde_json::json!({
            "recognition_executed": true,
            "subscription_id": "UNTRUSTED-EXPECTED-ID",
            "message": "UNTRUSTED-SUCCESS",
        });
    }
    assert_eq!(replay_mock(&modified.to_string()).unwrap(), expected);
    assert!(!expected.to_string().contains("UNTRUSTED-EXPECTED-ID"));
}

#[test]
fn replay_rejects_production_metadata_and_late_invalid_samples() {
    let original: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json"
    )))
    .unwrap();
    for (field, value) in [
        ("fictional", serde_json::json!(false)),
        ("production_registration", serde_json::json!(true)),
        ("rule_version", serde_json::json!("UNTRUSTED-RULE-OVERRIDE")),
    ] {
        let mut invalid = original.clone();
        invalid[field] = value;
        assert_eq!(
            replay_mock(&invalid.to_string()),
            Err("FIXTURE_METADATA_INVALID")
        );
    }
    let mut invalid = original;
    invalid["samples"][17]["snapshot"]["transactions"][0]["status"] =
        serde_json::json!("UNKNOWN-STATUS");
    assert_eq!(
        replay_mock(&invalid.to_string()),
        Err("FIXTURE_PARSE_FAILED")
    );
}

#[test]
fn same_number_in_two_owned_accounts_keeps_correct_evidence() {
    let mut s = case(&[("2026-07-15", "20.00"), ("2026-08-15", "30.00")]);
    for tx in &mut s.snapshot.transactions {
        tx.transaction_id = "001".into();
        tx.source_id = Some("fictional-bank".into());
        tx.category_code = None;
    }
    s.snapshot.transactions[1].account_id = "B".into();
    let r = run(&s);
    assert_eq!(r.groups[0].status, GroupStatus::Suspected);
    assert_eq!(r.groups[0].account_ids, vec!["A", "B"]);
    for (i, account) in ["A", "B"].iter().enumerate() {
        let charge = &r.groups[0].actual_charges[i];
        assert_eq!(&charge.account_id, account);
        assert_eq!(charge.transaction_id, "001");
        assert_eq!(&charge.source_key.as_ref().unwrap().account_id, account);
    }
}

#[test]
fn repeated_full_source_key_is_rejected_but_other_source_is_distinct() {
    let mut s = case(&[("2026-07-15", "20.00"), ("2026-08-15", "30.00")]);
    for tx in &mut s.snapshot.transactions {
        tx.transaction_id = "001".into();
        tx.source_id = Some("fictional-bank".into());
    }
    assert_eq!(run(&s).filter_report.outcome, Outcome::InvalidInput);
    s.snapshot.transactions[1].source_id = Some("fictional-other".into());
    let r = run(&s);
    assert_eq!(r.groups[0].actual_charges.len(), 2);
    assert_ne!(
        r.groups[0].actual_charges[0].source_key,
        r.groups[0].actual_charges[1].source_key
    );
}

#[test]
fn absent_category_does_not_turn_transfer_into_consumption() {
    let mut s = case(&[("2026-07-15", "20.00"), ("2026-08-15", "30.00")]);
    for tx in &mut s.snapshot.transactions {
        tx.category_code = None;
    }
    s.snapshot.transactions[1].transaction_type = TransactionType::Transfer;
    let r = run(&s);
    assert_eq!(r.groups[0].actual_charges.len(), 1);
    assert_eq!(r.groups[0].status, GroupStatus::InsufficientEvidence);
    assert_eq!(
        r.filter_report.excluded[0].reason,
        Exclusion::NotConsumption
    );
}

#[test]
fn source_and_category_if_supplied_cannot_be_blank() {
    let mut s = case(&[("2026-07-15", "20.00")]);
    s.snapshot.transactions[0].source_id = Some(" ".into());
    assert_eq!(run(&s).filter_report.outcome, Outcome::InvalidInput);
    s.snapshot.transactions[0].source_id = None;
    s.snapshot.transactions[0].category_code = Some(" ".into());
    assert_eq!(run(&s).filter_report.outcome, Outcome::InvalidInput);
}
