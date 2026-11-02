//! Historical replay compatibility and current-only reminder enforcement.
use super::*;
fn initial() -> Journal {
    let mut j = raw_records_tests::actual_fixture_journal();
    j.commands.truncate(4);
    j
}
fn read(j: &Journal) -> SubscriptionReadSnapshotMock {
    SubscriptionReadSnapshotMock::from_stored_journal_mock(
        "user_demo",
        j.commands.len() as i64 + 1,
        value(j).unwrap(),
    )
    .unwrap()
}
fn legacy_failure(j: &mut Journal, now: &str) {
    let s = read(j);
    let mut reserve = ReserveSink::default();
    let mut historical = s.state.reminders.clone();
    historical.set_legacy_replay_mock(true);
    let report = historical
        .scan_mock(&s.state.ledger, "user_demo", now, &mut reserve)
        .unwrap();
    let outcomes = vec![DeliveryOutcome::DefiniteFailure; reserve.messages.len()];
    j.commands.push(Command::ReserveScan {
        reminder_rules: None,
        now: now.into(),
        messages: reserve.messages.iter().map(|m| value(m).unwrap()).collect(),
        report: value(&report).unwrap(),
    });
    let s = read(j);
    let mut base = s.state.scan_base.as_ref().unwrap().1.clone();
    let mut sink = RecordedSink {
        outcomes: outcomes.clone(),
        index: 0,
    };
    let report = base
        .scan_mock(&s.state.ledger, "user_demo", now, &mut sink)
        .unwrap();
    j.commands.push(Command::ResolveScan {
        now: now.into(),
        outcomes,
        report: value(&report).unwrap(),
    });
}
#[test]
fn legacy_three_failures_remain_readable_but_current_scan_has_no_new_budget() {
    let mut j = initial();
    for now in [
        "2026-10-14T12:00:00+08:00",
        "2026-10-15T12:00:00+08:00",
        "2026-10-16T12:00:00+08:00",
    ] {
        legacy_failure(&mut j, now);
    }
    let raw = value(&j).unwrap();
    let s = read(&j);
    let id = s.list_subscriptions_mock().unwrap().items[0]
        .subscription_id
        .clone();
    let plan = s.reminder_plan_mock(&id).unwrap();
    assert_eq!(plan.current_attempt_count, Some(3));
    assert_eq!(plan.retries_remaining, Some(0));
    assert_eq!(
        plan.current_attempt,
        Some(crate::reminder_mock::ReminderAttemptStateMock::RetryExhausted)
    );
    let preview = s.due_reminders_mock("2026-10-17T12:00:00+08:00").unwrap();
    assert!(preview.messages.is_empty());
    assert_eq!(
        preview.report.items[0].state,
        crate::reminder_mock::ItemState::RetryExhausted
    );
    assert_eq!(value(&s.journal).unwrap(), raw);
    // Machine-readable export for database compatibility acceptance; fictional only.
    println!("LEGACY_RETRY_FIXTURE_JSON:{}", raw);
}
#[test]
fn policy_markers_cannot_be_unknown_or_downgraded_after_adoption() {
    let mut j = initial();
    let s = read(&j);
    let mut sink = ReserveSink::default();
    let report = s
        .state
        .reminders
        .with_current_rules_mock()
        .scan_mock(
            &s.state.ledger,
            "user_demo",
            "2026-10-14T12:00:00+08:00",
            &mut sink,
        )
        .unwrap();
    j.commands.push(Command::ReserveScan {
        reminder_rules: Some(1),
        now: "2026-10-14T12:00:00+08:00".into(),
        messages: sink.messages.iter().map(|m| value(m).unwrap()).collect(),
        report: value(&report).unwrap(),
    });
    let _ = read(&j);
    let mut raw = value(&j).unwrap();
    raw["commands"].as_array_mut().unwrap().last_mut().unwrap()["reminder_rules"] =
        serde_json::json!(99);
    assert!(SubscriptionReadSnapshotMock::from_stored_journal_mock(
        "user_demo",
        j.commands.len() as i64 + 1,
        raw
    )
    .is_err());
    j.commands.push(Command::Restart);
    let mut second = j.commands[4].clone();
    if let Command::ReserveScan {
        reminder_rules,
        now,
        ..
    } = &mut second
    {
        *reminder_rules = None;
        *now = "2026-10-15T12:00:00+08:00".into();
    }
    j.commands.push(second);
    assert!(SubscriptionReadSnapshotMock::from_stored_journal_mock(
        "user_demo",
        j.commands.len() as i64 + 1,
        value(&j).unwrap()
    )
    .is_err());
}
#[tokio::test]
async fn old_daily_advance_history_reads_but_old_unconfirmed_settings_cannot_execute() {
    let mut j = initial();
    j.commands.truncate(2);
    let s = read(&j);
    let id = s.list_subscriptions_mock().unwrap().items[0]
        .subscription_id
        .clone();
    let mut reminders = s.state.reminders.clone();
    reminders.set_legacy_replay_mock(true);
    let op = MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "legacy-daily".into(),
        task_id: "legacy".into(),
        node_id: "legacy".into(),
        operation_id: "legacy-daily".into(),
        idempotency_key: "legacy-daily-key".into(),
        plan_version: 1,
    };
    let schedule = Schedule {
        cycle: crate::recognition::Cycle::Day,
        anchor_date: NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
        period_index: 1,
        expected_date: NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        enabled: true,
        days_before: 1,
    };
    let b = reminders
        .prepare_mock(
            &s.state.ledger,
            op.clone(),
            &id,
            schedule.clone(),
            "2026-10-14T12:00:00+08:00",
            "2026-10-14T12:05:00+08:00",
        )
        .unwrap();
    j.commands.push(Command::PrepareSettings {
        reminder_rules: None,
        context: op,
        id,
        schedule: StoredSchedule::from(&schedule),
        now: b.prepared_at.clone(),
        expires: b.expires_at.clone(),
        binding: value(&b).unwrap(),
    });
    let s = read(&j);
    let mut runtime = SubscriptionRuntimeMock {
        pool: None,
        user: s.owner,
        version: s.version,
        journal: s.journal,
        state: s.state,
        staged: true,
    };
    struct RejectGate;
    impl MockScheduleGate for RejectGate {
        fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
            panic!("invalid daily draft must fail before gate")
        }
    }
    assert_eq!(
        runtime
            .confirm_settings_mock(&b, "2026-10-14T12:00:00+08:00", &mut RejectGate)
            .await,
        Err(RuntimeRecoveryIssue::Reminder("DAILY_SAME_DAY_REQUIRED"))
    );
    let r = reminders
        .confirm_mock(
            &runtime.state.ledger,
            &b,
            "2026-10-14T12:00:00+08:00",
            &mut HistoricalGate,
        )
        .unwrap();
    j.commands.push(Command::Settings {
        operation: b.context.operation_id.clone(),
        binding: value(&b).unwrap(),
        now: "2026-10-14T12:00:00+08:00".into(),
        receipt: value(&r).unwrap(),
    });
    assert_eq!(
        read(&j)
            .reminder_plan_mock(&b.target.subscription_id)
            .unwrap()
            .data_state,
        "NEEDS_REVIEW"
    );
}

#[test]
fn public_reminder_clone_from_legacy_snapshot_cannot_resume_legacy_retries() {
    let raw: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/legacy_reminder_three_failures.v1.json"
    )))
    .unwrap();
    let version = raw["commands"].as_array().unwrap().len() as i64 + 1;
    let s =
        SubscriptionReadSnapshotMock::from_stored_journal_mock("user_demo", version, raw).unwrap();
    let mut exposed = s.reminders_mock().clone();
    let mut sink = ReserveSink::default();
    let report = exposed
        .scan_mock(
            s.ledger_mock(),
            "user_demo",
            "2026-10-17T12:00:00+08:00",
            &mut sink,
        )
        .unwrap();
    assert!(sink.messages.is_empty());
    assert_eq!(
        report.items[0].state,
        crate::reminder_mock::ItemState::RetryExhausted
    );
}
