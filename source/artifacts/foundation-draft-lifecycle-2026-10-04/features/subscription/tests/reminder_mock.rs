use chrono::NaiveDate;
use safeflow_subscription_evidence::{
    confirmation_mock::*, managed_ledger_mock::*, recognition::Cycle, reminder_mock::*, *,
};
const NOW: &str = "2026-10-15T12:00:00+08:00";
const EXPIRY: &str = "2026-10-15T12:05:00+08:00";
fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}
fn fixture(status: LedgerStatus) -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("mock-reminder-ledger".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![LedgerEntry {
            subscription_id: "sub-A".into(),
            owner_user_id: "user_demo".into(),
            source_id: Some("fictional-billing-input-v1".into()),
            merchant_id: Some("merchant_demo_video".into()),
            service_ref: Some("service_demo_video_1".into()),
            status,
        }],
    }
}
fn ledger(status: LedgerStatus) -> MemoryConfirmationMock {
    MemoryConfirmationMock::from_fixture(fixture(status))
}
fn ctx(id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "session".into(),
        task_id: "reminder-task".into(),
        node_id: "settings".into(),
        operation_id: id.into(),
        idempotency_key: format!("key-{id}"),
        plan_version: 1,
    }
}
fn schedule() -> Schedule {
    Schedule {
        cycle: Cycle::CalendarMonth,
        anchor_date: date("2026-09-20"),
        period_index: 1,
        expected_date: date("2026-10-20"),
        enabled: true,
        days_before: 3,
    }
}
struct Gate {
    expected: ScheduleBinding,
    outcome: MockGateOutcome,
    calls: usize,
}
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
        self.calls += 1;
        if b == &self.expected && self.calls == 1 {
            self.outcome
        } else {
            MockGateOutcome::Denied
        }
    }
}
fn confirmed(
    m: &mut MemoryReminderMock,
    l: &MemoryConfirmationMock,
    s: Schedule,
    id: &str,
) -> (ScheduleBinding, ScheduleReceipt, Gate) {
    let b = m.prepare_mock(l, ctx(id), "sub-A", s, NOW, EXPIRY).unwrap();
    let mut g = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    let r = m.confirm_mock(l, &b, NOW, &mut g).unwrap();
    (b, r, g)
}

#[test]
fn settings_preview_lists_initial_and_changed_values_without_saving() {
    let l = ledger(LedgerStatus::Active);
    let mut model = MemoryReminderMock::default();
    let binding = model
        .prepare_mock(&l, ctx("preview-initial"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    let view = model.settings_preview_mock(&l, &binding, NOW).unwrap();
    assert!(view.before.is_none());
    assert_eq!(view.changed_fields, vec!["initial_plan"]);
    assert!(model
        .current_schedule_mock(&l, "user_demo", "sub-A")
        .is_err());
    let mut gate = Gate {
        expected: binding.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    model.confirm_mock(&l, &binding, NOW, &mut gate).unwrap();
    assert_eq!(gate.calls, 1);
    let mut changed = schedule();
    changed.enabled = false;
    changed.days_before = 0;
    let binding = model
        .prepare_mock(
            &l,
            ctx("preview-edit"),
            "sub-A",
            changed.clone(),
            NOW,
            EXPIRY,
        )
        .unwrap();
    let view = model.settings_preview_mock(&l, &binding, NOW).unwrap();
    assert_eq!(view.before, Some(schedule()));
    assert_eq!(view.after, changed);
    assert_eq!(view.changed_fields, vec!["enabled", "days_before"]);
    assert_eq!(
        model
            .current_schedule_mock(&l, "user_demo", "sub-A")
            .unwrap(),
        &schedule()
    );
}
#[test]
fn changed_settings_preview_and_old_confirmation_both_fail_without_consuming_gate() {
    let l = ledger(LedgerStatus::Active);
    let mut model = MemoryReminderMock::default();
    let first = model
        .prepare_mock(&l, ctx("preview-change"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    let mut operation = first.context.clone();
    operation.plan_version += 1;
    let mut changed = schedule();
    changed.days_before = 0;
    let second = model
        .prepare_mock(&l, operation, "sub-A", changed, NOW, EXPIRY)
        .unwrap();
    assert_eq!(
        model.settings_preview_mock(&l, &first, NOW).unwrap_err(),
        "PLAN_CHANGED"
    );
    assert!(model.settings_preview_mock(&l, &second, EXPIRY).is_err());
    let mut gate = Gate {
        expected: first.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    assert!(model.confirm_mock(&l, &first, NOW, &mut gate).is_err());
    assert_eq!(gate.calls, 0);
    assert!(model.settings_preview_mock(&l, &second, NOW).is_ok());
}
#[derive(Default)]
struct Sink {
    calls: Vec<ReminderMessage>,
    outcomes: Vec<DeliveryOutcome>,
}
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, m: &ReminderMessage) -> DeliveryOutcome {
        self.calls.push(m.clone());
        if self.outcomes.is_empty() {
            DeliveryOutcome::SimulatedSuccess
        } else {
            self.outcomes.remove(0)
        }
    }
}
fn scan(
    m: &mut MemoryReminderMock,
    l: &MemoryConfirmationMock,
    day: &str,
    s: &mut Sink,
) -> ItemState {
    m.scan_mock(l, "user_demo", &format!("{day}T12:00:00+08:00"), s)
        .unwrap()
        .items[0]
        .state
}
struct IntentGate(MockIntentBinding);
impl MockIntentGateVerifier for IntentGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        b: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        if b == &self.0 {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
fn transition(l: &mut MemoryConfirmationMock, kind: MockIntentKind, id: &str) {
    let b = l
        .prepare_intent_mock(ctx(id), "sub-A", kind, NOW, EXPIRY)
        .unwrap();
    l.commit_intent_mock(&b.draft_id, &b.context, NOW, &mut IntentGate(b.clone()))
        .unwrap();
}

#[test]
fn defaults_depend_on_cycle_and_predictions_always_need_confirmation() {
    for (cycle, lead) in [(Cycle::Day, 0), (Cycle::Week, 3), (Cycle::CalendarMonth, 3)] {
        let p = Prediction {
            cycle,
            anchor_date: date("2026-10-01"),
            period_index: 1,
            expected_date: occurrence_date(date("2026-10-01"), cycle, 1).unwrap(),
            actual_charge_count: 2,
            needs_user_confirmation: true,
        };
        let s = Schedule::from_prediction(&p);
        assert_eq!(s.days_before, lead);
        assert!(s.enabled);
        s.validate().unwrap();
    }
}
#[test]
fn short_months_keep_original_anchor_including_leap_years() {
    assert_eq!(
        occurrence_date(date("2026-01-31"), Cycle::CalendarMonth, 1).unwrap(),
        date("2026-02-28")
    );
    assert_eq!(
        occurrence_date(date("2026-01-31"), Cycle::CalendarMonth, 2).unwrap(),
        date("2026-03-31")
    );
    assert_eq!(
        occurrence_date(date("2024-01-31"), Cycle::CalendarMonth, 1).unwrap(),
        date("2024-02-29")
    );
}
#[test]
fn manual_edit_is_only_for_current_period_and_no_missing_charges_are_created() {
    let mut s = schedule();
    s.expected_date = date("2026-10-22");
    s.validate().unwrap();
    let next = s.next_after_mock(date("2026-10-23")).unwrap();
    assert_eq!(next.expected_date, date("2026-11-20"));
    assert_eq!(next.anchor_date, s.anchor_date);
    assert_eq!(next.period_index, 2);
}
#[test]
fn excessive_lead_and_dates_outside_the_period_require_adjustment() {
    for (cycle, anchor, expected, lead) in [
        (Cycle::Day, "2026-10-19", "2026-10-20", 2),
        (Cycle::Week, "2026-10-13", "2026-10-20", 8),
        (Cycle::CalendarMonth, "2026-01-31", "2026-02-28", 30),
    ] {
        let s = Schedule {
            cycle,
            anchor_date: date(anchor),
            period_index: 1,
            expected_date: date(expected),
            enabled: true,
            days_before: lead,
        };
        assert_eq!(s.validate(), Err("LEAD_EXCEEDS_CYCLE"));
    }
    let mut s = schedule();
    s.expected_date = date("2026-11-20");
    assert_eq!(s.validate(), Err("DATE_NEEDS_INPUT"));
    s.expected_date = date("2026-09-20");
    assert_eq!(s.validate(), Err("DATE_NEEDS_INPUT"));
    s.days_before = 31;
    assert_eq!(s.validate(), Err("SETTINGS_NEEDS_INPUT"));
}
#[test]
fn unconfirmed_date_and_all_gate_failures_never_schedule_or_send() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let b = m
        .prepare_mock(&l, ctx("settings"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    for (outcome, error) in [
        (MockGateOutcome::Denied, "CONFIRMATION_REQUIRED"),
        (MockGateOutcome::StrongAuthRequired, "WAITING_AUTH"),
        (MockGateOutcome::Unavailable, "GATE_UNAVAILABLE"),
    ] {
        let mut g = Gate {
            expected: b.clone(),
            outcome,
            calls: 0,
        };
        assert_eq!(m.confirm_mock(&l, &b, NOW, &mut g), Err(error));
    }
    let mut sink = Sink::default();
    assert!(m
        .scan_mock(&l, "user_demo", "2026-10-20T00:00:00+08:00", &mut sink)
        .unwrap()
        .items
        .is_empty());
    assert!(sink.calls.is_empty());
}
#[test]
fn confirmation_binding_changes_expiry_and_backwards_time_stop_before_gate() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let b = m
        .prepare_mock(&l, ctx("settings"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    let mut g = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    let mut changed = b.clone();
    changed.schedule.expected_date = date("2026-10-21");
    assert_eq!(
        m.confirm_mock(&l, &changed, NOW, &mut g),
        Err("PLAN_CHANGED")
    );
    let mut changed = b.clone();
    changed.context.session_ref = "other".into();
    assert_eq!(
        m.confirm_mock(&l, &changed, NOW, &mut g),
        Err("PLAN_CHANGED")
    );
    assert_eq!(m.confirm_mock(&l, &b, EXPIRY, &mut g), Err("DRAFT_EXPIRED"));
    assert_eq!(
        m.confirm_mock(&l, &b, "2026-10-15T11:59:59+08:00", &mut g),
        Err("TIME_INVALID")
    );
    assert_eq!(g.calls, 0);
}
#[test]
fn ledger_aba_invalidates_settings_even_when_status_returns_to_active() {
    let mut l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let b = m
        .prepare_mock(&l, ctx("settings"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    transition(&mut l, MockIntentKind::Record, "record");
    transition(&mut l, MockIntentKind::Revoke, "revoke");
    let mut g = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    assert_eq!(m.confirm_mock(&l, &b, NOW, &mut g), Err("STALE_FACTS"));
    assert_eq!(g.calls, 0);
}
#[test]
fn competing_settings_and_historical_retries_do_not_overwrite_new_preferences() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let (b, receipt, mut g) = confirmed(&mut m, &l, schedule(), "old");
    let mut changed = schedule();
    changed.days_before = 1;
    let a = m
        .prepare_mock(&l, ctx("a"), "sub-A", changed, NOW, EXPIRY)
        .unwrap();
    let c = m
        .prepare_mock(&l, ctx("c"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    let mut ag = Gate {
        expected: a.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    m.confirm_mock(&l, &a, NOW, &mut ag).unwrap();
    let mut cg = Gate {
        expected: c.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    assert_eq!(m.confirm_mock(&l, &c, NOW, &mut cg), Err("STALE_FACTS"));
    assert_eq!(cg.calls, 0);
    assert_eq!(
        m.confirm_mock(&l, &b, "2027-01-01T00:00:00+08:00", &mut g)
            .unwrap(),
        receipt
    );
    assert_eq!(g.calls, 1);
    assert_eq!(
        m.current_schedule_mock(&l, "user_demo", "sub-A")
            .unwrap()
            .days_before,
        1
    );
}
#[test]
fn ordinary_reminder_is_due_three_days_before_once_and_keeps_current_billing_date() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink::default();
    assert_eq!(scan(&mut m, &l, "2026-10-16", &mut sink), ItemState::NotDue);
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-18", &mut sink),
        ItemState::AlreadySimulated
    );
    assert_eq!(sink.calls.len(), 1);
    assert_eq!(
        m.current_schedule_mock(&l, "user_demo", "sub-A")
            .unwrap()
            .expected_date,
        date("2026-10-20")
    );
    assert!(sink.calls[0].content.contains("预计"));
    assert_eq!(sink.calls[0].channel, "IN_APP_MOCK_ONLY");
}
#[test]
fn ordinary_recovery_inside_window_can_remind_but_expired_period_is_skipped() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink::default();
    assert_eq!(
        scan(&mut m, &l, "2026-10-19", &mut sink),
        ItemState::SimulatedSuccess
    );
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    sink.calls.clear();
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::SkippedExpired
    );
    assert!(sink.calls.is_empty());
    assert_eq!(
        m.current_schedule_mock(&l, "user_demo", "sub-A")
            .unwrap()
            .expected_date,
        date("2026-11-20")
    );
}
#[test]
fn daily_default_is_same_day_and_next_cycle_remains_daily() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let s = Schedule {
        cycle: Cycle::Day,
        anchor_date: date("2026-10-19"),
        period_index: 1,
        expected_date: date("2026-10-20"),
        enabled: true,
        days_before: 0,
    };
    confirmed(&mut m, &l, s, "s");
    let mut sink = Sink::default();
    assert_eq!(scan(&mut m, &l, "2026-10-19", &mut sink), ItemState::NotDue);
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::AlreadySimulated
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::AlreadySimulated
    );
    assert_eq!(sink.calls.len(), 2);
    assert_ne!(sink.calls[0].cycle_ref, sink.calls[1].cycle_ref);
}
#[test]
fn definite_failure_retries_next_business_day_but_never_after_expiry() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink {
        outcomes: vec![
            DeliveryOutcome::DefiniteFailure,
            DeliveryOutcome::SimulatedSuccess,
        ],
        ..Sink::default()
    };
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::RetryNextDay
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::RetryWaiting
    );
    assert_eq!(sink.calls.len(), 1);
    assert_eq!(
        scan(&mut m, &l, "2026-10-18", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(sink.calls.len(), 2);
    assert_eq!(sink.calls[0], sink.calls[1]);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink {
        outcomes: vec![DeliveryOutcome::DefiniteFailure],
        ..Sink::default()
    };
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::RetryNextDay
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::SkippedExpired
    );
    assert_eq!(sink.calls.len(), 1);
}
#[test]
fn cancellation_reminder_is_due_on_date_can_be_late_and_is_independent_from_ordinary() {
    let mut l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink::default();
    scan(&mut m, &l, "2026-10-17", &mut sink);
    transition(&mut l, MockIntentKind::Record, "record");
    assert_eq!(scan(&mut m, &l, "2026-10-19", &mut sink), ItemState::NotDue);
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::AlreadySimulated
    );
    assert_eq!(sink.calls.len(), 2);
    assert_ne!(sink.calls[0].kind, sink.calls[1].kind);
    assert_eq!(sink.calls[0].cycle_ref, sink.calls[1].cycle_ref);
    assert!(sink.calls[1].content.contains("未确认商户已取消"));
    let l = ledger(LedgerStatus::Cancelling);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink::default();
    let r = m
        .scan_mock(&l, "user_demo", "2026-11-02T00:00:00+08:00", &mut sink)
        .unwrap();
    assert_eq!(r.items[0].state, ItemState::SimulatedSuccess);
    assert!(r.items[0].late);
    assert!(
        !r.notification_sent
            && !r.audit_integrated
            && !r.database_connected
            && !r.production_registered
    );
}
#[test]
fn revoke_stops_pending_cancellation_retry_without_restoring_merchant_service() {
    let mut l = ledger(LedgerStatus::Cancelling);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink {
        outcomes: vec![DeliveryOutcome::DefiniteFailure],
        ..Sink::default()
    };
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::RetryNextDay
    );
    transition(&mut l, MockIntentKind::Revoke, "revoke");
    assert_eq!(
        scan(&mut m, &l, "2026-10-22", &mut sink),
        ItemState::SkippedExpired
    );
    assert_eq!(sink.calls.len(), 1);
}
#[test]
fn cancellation_failed_or_unknown_outcome_never_counts_as_success() {
    let l = ledger(LedgerStatus::Cancelling);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink {
        outcomes: vec![DeliveryOutcome::DefiniteFailure, DeliveryOutcome::Unknown],
        ..Sink::default()
    };
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::RetryNextDay
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-22", &mut sink),
        ItemState::UnknownNeedsReview
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-23", &mut sink),
        ItemState::UnknownNeedsReview
    );
    assert_eq!(sink.calls.len(), 2);
}
#[test]
fn disabled_ordinary_reminder_does_not_suppress_cancellation_review() {
    let mut l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let mut s = schedule();
    s.enabled = false;
    confirmed(&mut m, &l, s, "s");
    let mut sink = Sink::default();
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::Disabled
    );
    transition(&mut l, MockIntentKind::Record, "record");
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(sink.calls.len(), 1);
}
#[test]
fn foreign_mixed_owner_and_incomplete_ledgers_stop_before_sink_or_confirmation() {
    for case in 0..3 {
        let mut f = fixture(LedgerStatus::Active);
        match case {
            0 => f.owner_user_id = "other".into(),
            1 => f.entries[0].owner_user_id = "other".into(),
            _ => {
                f.read.is_complete = false;
                f.read.state = ReadKind::Incomplete;
            }
        }
        let l = MemoryConfirmationMock::from_fixture(f);
        let mut m = MemoryReminderMock::default();
        assert!(m
            .prepare_mock(&l, ctx("s"), "sub-A", schedule(), NOW, EXPIRY)
            .is_err());
        let mut sink = Sink::default();
        assert!(m.scan_mock(&l, "user_demo", NOW, &mut sink).is_err());
        assert!(sink.calls.is_empty());
    }
}
#[test]
fn business_timezone_and_clock_reversal_are_checked() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink::default();
    let r = m
        .scan_mock(&l, "user_demo", "2026-10-16T16:00:00Z", &mut sink)
        .unwrap();
    assert_eq!(r.business_date, date("2026-10-17"));
    assert_eq!(sink.calls.len(), 1);
    assert!(matches!(
        m.scan_mock(&l, "user_demo", "2026-10-16T15:59:59Z", &mut sink),
        Err("CLOCK_MOVED_BACKWARD")
    ));
    assert_eq!(sink.calls.len(), 1);
}
#[test]
fn short_month_after_roll_requests_lead_adjustment_without_blocking_cancel_review() {
    let mut l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let s = Schedule {
        cycle: Cycle::CalendarMonth,
        anchor_date: date("2026-12-31"),
        period_index: 1,
        expected_date: date("2027-01-31"),
        enabled: true,
        days_before: 30,
    };
    confirmed(&mut m, &l, s, "s");
    let mut sink = Sink::default();
    assert_eq!(
        scan(&mut m, &l, "2027-02-01", &mut sink),
        ItemState::SkippedExpired
    );
    assert_eq!(
        scan(&mut m, &l, "2027-02-02", &mut sink),
        ItemState::SettingsNeedsInput
    );
    transition(&mut l, MockIntentKind::Record, "record");
    assert_eq!(
        scan(&mut m, &l, "2027-02-28", &mut sink),
        ItemState::SimulatedSuccess
    );
}
#[test]
fn rollover_invalidates_an_old_pending_settings_change() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let b = m
        .prepare_mock(&l, ctx("pending"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    let mut sink = Sink::default();
    scan(&mut m, &l, "2026-10-21", &mut sink);
    let mut g = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    assert_eq!(m.confirm_mock(&l, &b, NOW, &mut g), Err("STALE_FACTS"));
    assert_eq!(g.calls, 0);
}

fn consumption() -> Sample {
    billing_query_mock::load_case(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
        )),
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap()
}
struct RegistrationGate(MockConfirmationBinding);
impl MockGateVerifier for RegistrationGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        b: &MockConfirmationBinding,
        _: &str,
    ) -> MockGateOutcome {
        if b == &self.0 {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
#[test]
fn prediction_uses_registered_service_and_keeps_actual_consumption_unchanged() {
    // A separate explicit fixture query date, not a modification of saved inputs.
    let mut s = consumption();
    s.snapshot.window.as_of = "2026-08-16T12:00:00+08:00".into();
    s.snapshot.window.end_exclusive = s.snapshot.window.as_of.clone();
    s.snapshot.window.start_inclusive = "2026-04-16T12:00:00+08:00".into();
    let mut f = fixture(LedgerStatus::Active);
    f.entries.clear();
    let mut l = MemoryConfirmationMock::from_fixture(f);
    let preview = predict_candidate_mock(&l, &s.trusted_context_fixture, &s.snapshot, 0).unwrap();
    assert!(l.list_mock("user_demo").unwrap().is_empty());
    let b = l
        .prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx("register"),
            0,
            EXPIRY,
        )
        .unwrap();
    let r = l
        .commit_mock(
            &b.draft_id,
            &b.context,
            NOW,
            &mut RegistrationGate(b.clone()),
        )
        .unwrap();
    let p = predict_mock(
        &l,
        &s.trusted_context_fixture,
        &s.snapshot,
        &r.subscription_id,
    )
    .unwrap();
    assert!(p.needs_user_confirmation);
    assert_eq!(preview, p);
    assert_eq!(p.cycle, Cycle::CalendarMonth);
    assert_eq!(p.actual_charge_count, 2);
    assert_eq!(
        p.expected_date,
        occurrence_date(p.anchor_date, p.cycle, p.period_index).unwrap()
    );
    // The source also has non-consumption rows; they are not invented charges.
    assert!(s.snapshot.transactions.len() > p.actual_charge_count);
}
#[test]
fn unavailable_or_ambiguous_consumption_never_silently_supplies_a_date() {
    let l = ledger(LedgerStatus::Active);
    let mut s = consumption();
    s.snapshot.transactions_read.is_complete = false;
    assert!(predict_mock(&l, &s.trusted_context_fixture, &s.snapshot, "sub-A").is_err());
    let mut s = consumption();
    for t in &mut s.snapshot.transactions {
        t.mock_service_ref = None;
    }
    assert!(predict_mock(&l, &s.trusted_context_fixture, &s.snapshot, "sub-A").is_err());
    let l = ledger(LedgerStatus::Error);
    let s = consumption();
    assert_eq!(
        predict_mock(&l, &s.trusted_context_fixture, &s.snapshot, "sub-A"),
        Err("REGISTRATION_NEEDS_REVIEW")
    );
}
#[test]
fn overdue_prediction_and_duplicate_ledger_service_require_user_input() {
    let l = ledger(LedgerStatus::Active);
    let s = consumption();
    assert_eq!(
        predict_mock(&l, &s.trusted_context_fixture, &s.snapshot, "sub-A"),
        Err("DATE_NEEDS_INPUT")
    );
    let mut f = fixture(LedgerStatus::Active);
    let mut duplicate = f.entries[0].clone();
    duplicate.subscription_id = "sub-B".into();
    f.entries.push(duplicate);
    let l = MemoryConfirmationMock::from_fixture(f);
    assert_eq!(
        predict_mock(&l, &s.trusted_context_fixture, &s.snapshot, "sub-A"),
        Err("SERVICE_NEEDS_REVIEW")
    );
}
#[test]
fn scan_before_configuration_confirmation_time_is_rejected() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "s");
    let mut sink = Sink::default();
    assert!(matches!(
        m.scan_mock(&l, "user_demo", "2026-10-15T11:59:59+08:00", &mut sink),
        Err("CLOCK_MOVED_BACKWARD")
    ));
    assert!(sink.calls.is_empty());
}
#[test]
fn editing_current_date_and_lead_does_not_reissue_already_simulated_period() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "original");
    let mut sink = Sink::default();
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::SimulatedSuccess
    );
    let mut adjusted = schedule();
    adjusted.expected_date = date("2026-10-21");
    adjusted.days_before = 1;
    let b = m
        .prepare_mock(
            &l,
            ctx("edit"),
            "sub-A",
            adjusted,
            "2026-10-18T12:00:00+08:00",
            "2026-10-18T12:05:00+08:00",
        )
        .unwrap();
    let mut gate = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    m.confirm_mock(&l, &b, "2026-10-18T12:00:00+08:00", &mut gate)
        .unwrap();
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::AlreadySimulated
    );
    assert_eq!(sink.calls.len(), 1);
}
#[test]
fn a_changed_anchor_cycle_or_period_cannot_bypass_existing_period_history() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "original");
    for mutation in 0..3 {
        let mut s = schedule();
        match mutation {
            0 => s.anchor_date = date("2026-09-21"),
            1 => {
                s.cycle = Cycle::Week;
                s.anchor_date = date("2026-10-13");
            }
            _ => {
                s.period_index = 2;
                s.expected_date = date("2026-11-20");
            }
        }
        assert_eq!(
            m.prepare_mock(&l, ctx("edit"), "sub-A", s, NOW, EXPIRY),
            Err("CYCLE_RECHECK_REQUIRED")
        );
    }
}
#[test]
fn replaced_source_for_the_same_subscription_id_stops_before_any_delivery() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "original");
    let mut changed = fixture(LedgerStatus::Active);
    changed.entries[0].source_id = Some("unrelated-source".into());
    let changed = MemoryConfirmationMock::from_fixture(changed);
    assert!(matches!(
        m.current_schedule_mock(&changed, "user_demo", "sub-A"),
        Err("SERVICE_NEEDS_REVIEW")
    ));
    let mut sink = Sink::default();
    assert!(matches!(
        m.scan_mock(
            &changed,
            "user_demo",
            "2026-10-17T00:00:00+08:00",
            &mut sink
        ),
        Err("SERVICE_NEEDS_REVIEW")
    ));
    assert!(sink.calls.is_empty());
}

#[test]
fn settings_result_can_be_read_repeatedly_without_new_confirmation_or_delivery() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let (_, receipt, gate) = confirmed(&mut m, &l, schedule(), "settings-original");
    for _ in 0..3 {
        assert_eq!(
            m.settings_result_mock(&l, "user_demo", "settings-original")
                .unwrap(),
            &receipt
        );
    }
    assert_eq!(gate.calls, 1);
    assert_eq!(
        m.current_schedule_mock(&l, "user_demo", "sub-A").unwrap(),
        &schedule()
    );
    let mut sink = Sink::default();
    assert_eq!(scan(&mut m, &l, "2026-10-16", &mut sink), ItemState::NotDue);
    assert!(sink.calls.is_empty());
    assert!(l.events_mock("user_demo").unwrap().is_empty());
    assert!(!receipt.notification_sent);
}

#[test]
fn settings_result_keeps_old_receipt_without_reverting_new_settings() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let (_, original, _) = confirmed(&mut m, &l, schedule(), "settings-original");
    let mut changed = schedule();
    changed.days_before = 1;
    let (_, newer, _) = confirmed(&mut m, &l, changed.clone(), "settings-newer");
    assert_eq!(
        m.settings_result_mock(&l, "user_demo", "settings-original")
            .unwrap(),
        &original
    );
    assert_eq!(original.days_before, 3);
    assert_eq!(
        m.settings_result_mock(&l, "user_demo", "settings-newer")
            .unwrap(),
        &newer
    );
    assert_eq!(newer.days_before, 1);
    assert_eq!(
        m.current_schedule_mock(&l, "user_demo", "sub-A").unwrap(),
        &changed
    );
}

#[test]
fn settings_original_receipt_survives_later_simulated_reminder_without_replaying_it() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let (_, original, gate) = confirmed(&mut m, &l, schedule(), "settings-original");
    let mut sink = Sink::default();
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(sink.calls.len(), 1);

    // This is the saved settings action, not a lifetime notification summary.
    for _ in 0..3 {
        let historical = m
            .settings_result_mock(&l, "user_demo", "settings-original")
            .unwrap();
        assert_eq!(historical, &original);
        assert!(!historical.notification_sent);
    }
    assert_eq!(gate.calls, 1);
    assert_eq!(sink.calls.len(), 1);
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::AlreadySimulated
    );
    assert_eq!(sink.calls.len(), 1);
}

#[test]
fn pending_denied_and_unknown_settings_operations_have_no_success_receipt() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let b = m
        .prepare_mock(&l, ctx("pending"), "sub-A", schedule(), NOW, EXPIRY)
        .unwrap();
    assert_eq!(
        m.settings_result_mock(&l, "user_demo", "pending"),
        Err("RESOURCE_UNAVAILABLE")
    );
    let mut g = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Denied,
        calls: 0,
    };
    assert_eq!(
        m.confirm_mock(&l, &b, NOW, &mut g),
        Err("CONFIRMATION_REQUIRED")
    );
    for op in ["pending", "unknown", ""] {
        assert_eq!(
            m.settings_result_mock(&l, "user_demo", op),
            Err("RESOURCE_UNAVAILABLE")
        );
    }
    assert_eq!(g.calls, 1);
    assert_eq!(
        m.current_schedule_mock(&l, "user_demo", "sub-A"),
        Err("DATE_CONFIRMATION_REQUIRED")
    );
}

#[test]
fn settings_result_requires_owned_complete_current_ledger_and_present_target() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "original");
    assert_eq!(
        m.settings_result_mock(&l, "foreign", "original"),
        Err("RESOURCE_UNAVAILABLE")
    );
    for state in [ReadKind::Unavailable, ReadKind::Incomplete] {
        let mut f = fixture(LedgerStatus::Active);
        f.read.state = state;
        f.read.is_complete = false;
        let unavailable = MemoryConfirmationMock::from_fixture(f);
        assert_eq!(
            m.settings_result_mock(&unavailable, "user_demo", "original"),
            Err("LEDGER_NOT_READY")
        );
    }
    let mut f = fixture(LedgerStatus::Active);
    f.entries.clear();
    let missing = MemoryConfirmationMock::from_fixture(f);
    assert_eq!(
        m.settings_result_mock(&missing, "user_demo", "original"),
        Err("RESOURCE_UNAVAILABLE")
    );
}

#[test]
fn same_operation_id_for_different_mock_users_returns_only_own_receipt() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let (_, own, _) = confirmed(&mut m, &l, schedule(), "same-operation");
    let mut f = fixture(LedgerStatus::Active);
    f.owner_user_id = "other-user".into();
    f.entries[0].owner_user_id = "other-user".into();
    let other_ledger = MemoryConfirmationMock::from_fixture(f);
    let mut other_context = ctx("same-operation");
    other_context.user_id = "other-user".into();
    let mut other_schedule = schedule();
    other_schedule.days_before = 1;
    let b = m
        .prepare_mock(
            &other_ledger,
            other_context,
            "sub-A",
            other_schedule,
            NOW,
            EXPIRY,
        )
        .unwrap();
    let mut g = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        calls: 0,
    };
    let other = m.confirm_mock(&other_ledger, &b, NOW, &mut g).unwrap();
    assert_eq!(
        m.settings_result_mock(&l, "user_demo", "same-operation")
            .unwrap(),
        &own
    );
    assert_eq!(
        m.settings_result_mock(&other_ledger, "other-user", "same-operation")
            .unwrap(),
        &other
    );
    assert_eq!(own.days_before, 3);
    assert_eq!(other.days_before, 1);
}
