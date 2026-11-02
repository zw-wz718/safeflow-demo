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

struct Sink {
    calls: usize,
    outcome: DeliveryOutcome,
}
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
        self.calls += 1;
        self.outcome
    }
}
fn scan(
    m: &mut MemoryReminderMock,
    l: &MemoryConfirmationMock,
    day: &str,
    sink: &mut Sink,
) -> ItemState {
    m.scan_mock(l, "user_demo", &format!("{day}T12:00:00+08:00"), sink)
        .unwrap()
        .items
        .last()
        .unwrap()
        .state
}
#[test]
fn daily_schedule_rejects_one_day_lead_instead_of_just_defaulting_to_zero() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    let mut s = schedule();
    s.cycle = Cycle::Day;
    s.anchor_date = date("2026-10-19");
    s.days_before = 1;
    assert_eq!(
        m.prepare_mock(&l, ctx("daily-invalid"), "sub-A", s.clone(), NOW, EXPIRY),
        Err("DAILY_SAME_DAY_REQUIRED")
    );
    s.days_before = 0;
    confirmed(&mut m, &l, s, "daily-valid");
    let mut sink = Sink {
        calls: 0,
        outcome: DeliveryOutcome::SimulatedSuccess,
    };
    assert_eq!(scan(&mut m, &l, "2026-10-19", &mut sink), ItemState::NotDue);
    assert_eq!(sink.calls, 0);
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::SimulatedSuccess
    );
    assert_eq!(sink.calls, 1);
}
#[test]
fn ordinary_second_failure_exhausts_retry_even_on_later_days() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "ordinary");
    let mut sink = Sink {
        calls: 0,
        outcome: DeliveryOutcome::DefiniteFailure,
    };
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::RetryNextDay
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-17", &mut sink),
        ItemState::RetryWaiting
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-18", &mut sink),
        ItemState::RetryExhausted
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-19", &mut sink),
        ItemState::RetryExhausted
    );
    assert_eq!(sink.calls, 2);
    let status = m.plan_status_mock(&l, "user_demo", "sub-A").unwrap();
    assert_eq!(
        status.current_attempt,
        Some(ReminderAttemptStateMock::RetryExhausted)
    );
    assert_eq!(status.current_attempt_count, Some(2));
    assert_eq!(status.retries_remaining, Some(0));
    assert!(status.message.contains("已用完"));
}
#[test]
fn cancellation_check_has_only_one_retry_despite_late_recovery() {
    let l = ledger(LedgerStatus::Cancelling);
    let mut m = MemoryReminderMock::default();
    let mut s = schedule();
    s.enabled = false;
    confirmed(&mut m, &l, s, "cancel");
    let mut sink = Sink {
        calls: 0,
        outcome: DeliveryOutcome::DefiniteFailure,
    };
    assert_eq!(scan(&mut m, &l, "2026-10-19", &mut sink), ItemState::NotDue);
    assert_eq!(
        scan(&mut m, &l, "2026-10-20", &mut sink),
        ItemState::RetryNextDay
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-21", &mut sink),
        ItemState::RetryExhausted
    );
    assert_eq!(
        scan(&mut m, &l, "2026-11-25", &mut sink),
        ItemState::RetryExhausted
    );
    assert_eq!(sink.calls, 2);
}
#[test]
fn uncertain_retry_never_becomes_exhausted_or_automatic_third_attempt() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "unknown");
    let mut sink = Sink {
        calls: 0,
        outcome: DeliveryOutcome::DefiniteFailure,
    };
    scan(&mut m, &l, "2026-10-17", &mut sink);
    sink.outcome = DeliveryOutcome::Unknown;
    assert_eq!(
        scan(&mut m, &l, "2026-10-18", &mut sink),
        ItemState::UnknownNeedsReview
    );
    assert_eq!(
        scan(&mut m, &l, "2026-10-19", &mut sink),
        ItemState::UnknownNeedsReview
    );
    assert_eq!(sink.calls, 2);
    let status = m.plan_status_mock(&l, "user_demo", "sub-A").unwrap();
    assert_eq!(
        status.current_attempt,
        Some(ReminderAttemptStateMock::NeedsReview)
    );
    assert_eq!(status.retries_remaining, Some(0));
    assert!(status.message.contains("结果不明"));
}
#[test]
fn editing_or_toggling_settings_does_not_refill_cycle_retry_budget() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "initial");
    let mut sink = Sink {
        calls: 0,
        outcome: DeliveryOutcome::DefiniteFailure,
    };
    scan(&mut m, &l, "2026-10-17", &mut sink);
    scan(&mut m, &l, "2026-10-18", &mut sink);
    for (n, enabled) in [(0, false), (1, true)] {
        let mut s = schedule();
        s.enabled = enabled;
        s.days_before = 2;
        let b = m
            .prepare_mock(
                &l,
                ctx(&format!("edit-{n}")),
                "sub-A",
                s,
                "2026-10-18T13:00:00+08:00",
                "2026-10-18T13:05:00+08:00",
            )
            .unwrap();
        let mut g = Gate {
            expected: b.clone(),
            outcome: MockGateOutcome::Confirmed,
            calls: 0,
        };
        m.confirm_mock(&l, &b, "2026-10-18T13:00:00+08:00", &mut g)
            .unwrap();
    }
    assert_eq!(
        scan(&mut m, &l, "2026-10-19", &mut sink),
        ItemState::RetryExhausted
    );
    assert_eq!(sink.calls, 2);
}
#[test]
fn new_month_is_independent_but_old_failed_attempt_count_is_preserved() {
    let l = ledger(LedgerStatus::Active);
    let mut m = MemoryReminderMock::default();
    confirmed(&mut m, &l, schedule(), "cycles");
    let mut sink = Sink {
        calls: 0,
        outcome: DeliveryOutcome::DefiniteFailure,
    };
    scan(&mut m, &l, "2026-10-17", &mut sink);
    scan(&mut m, &l, "2026-10-18", &mut sink);
    assert_eq!(
        scan(&mut m, &l, "2026-11-17", &mut sink),
        ItemState::RetryNextDay
    );
    assert_eq!(sink.calls, 3);
    let status = m.plan_status_mock(&l, "user_demo", "sub-A").unwrap();
    assert_eq!(status.current_attempt_count, Some(1));
    assert_eq!(status.retries_remaining, Some(1));
    assert!(status
        .saved_cycle_outcomes
        .iter()
        .any(|a| a.attempt_count == 2 && a.state == ReminderAttemptStateMock::RetryExhausted));
}
