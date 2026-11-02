//! Fixed fictional reminders. No real sender, credentials, timer or external approval.
use chrono::NaiveDate;
use safeflow_subscription_evidence::{
    confirmation_mock::*, managed_ledger_mock::*, recognition::Cycle, reminder_mock::*, *,
};
use serde_json::{json, Value};
const NOW: &str = "2026-08-16T12:00:00+08:00";
const EXPIRY: &str = "2026-08-16T12:05:00+08:00";
fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}
fn context(id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "fixture-session".into(),
        task_id: "fixture-reminder-task".into(),
        node_id: format!("fixture-{id}"),
        operation_id: id.into(),
        idempotency_key: format!("fixture-key-{id}"),
        plan_version: 1,
    }
}
struct RegisterGate(MockConfirmationBinding);
impl MockGateVerifier for RegisterGate {
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
struct Gate {
    expected: ScheduleBinding,
    outcome: MockGateOutcome,
    used: bool,
}
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
        if b != &self.expected || self.used {
            return MockGateOutcome::Denied;
        }
        if self.outcome == MockGateOutcome::Confirmed {
            self.used = true;
        }
        self.outcome
    }
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
#[derive(Default)]
struct Sink {
    messages: Vec<ReminderMessage>,
    outcomes: Vec<DeliveryOutcome>,
}
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, m: &ReminderMessage) -> DeliveryOutcome {
        self.messages.push(m.clone());
        if self.outcomes.is_empty() {
            DeliveryOutcome::SimulatedSuccess
        } else {
            self.outcomes.remove(0)
        }
    }
}
fn empty() -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("fixture-reminder-ledger".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![],
    }
}
fn confirm(m: &mut MemoryReminderMock, l: &MemoryConfirmationMock, id: &str, s: Schedule) {
    let b = m
        .prepare_mock(l, context("settings"), id, s, NOW, EXPIRY)
        .unwrap();
    let mut gate = Gate {
        expected: b.clone(),
        outcome: MockGateOutcome::Confirmed,
        used: false,
    };
    let first = m.confirm_mock(l, &b, NOW, &mut gate).unwrap();
    assert_eq!(m.confirm_mock(l, &b, NOW, &mut gate).unwrap(), first);
}
fn scan(
    m: &mut MemoryReminderMock,
    l: &MemoryConfirmationMock,
    day: &str,
    s: &mut Sink,
) -> DeliveryReport {
    m.scan_mock(l, "user_demo", &format!("{day}T12:00:00+08:00"), s)
        .unwrap()
}
fn change(l: &mut MemoryConfirmationMock, id: &str, kind: MockIntentKind, day: &str) {
    let now = format!("{day}T12:00:00+08:00");
    let expiry = format!("{day}T12:05:00+08:00");
    let op = if kind == MockIntentKind::Record {
        "cancel"
    } else {
        "revoke"
    };
    let b = l
        .prepare_intent_mock(context(op), id, kind, &now, &expiry)
        .unwrap();
    l.commit_intent_mock(&b.draft_id, &b.context, &now, &mut IntentGate(b.clone()))
        .unwrap();
}
fn replay() -> Value {
    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
    ));
    let mut sample = billing_query_mock::load_case(
        raw,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap();
    // Explicit alternative fixture query clock; saved consumption evidence is unchanged.
    sample.snapshot.window.as_of = NOW.into();
    sample.snapshot.window.end_exclusive = NOW.into();
    sample.snapshot.window.start_inclusive = "2026-04-16T12:00:00+08:00".into();
    let mut ledger = MemoryConfirmationMock::from_fixture(empty());
    let preview = predict_candidate_mock(
        &ledger,
        &sample.trusted_context_fixture,
        &sample.snapshot,
        0,
    )
    .unwrap();
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
    let b = ledger
        .prepare_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            context("register"),
            0,
            EXPIRY,
        )
        .unwrap();
    let registered = ledger
        .commit_mock(&b.draft_id, &b.context, NOW, &mut RegisterGate(b.clone()))
        .unwrap();
    let id = &registered.subscription_id;
    let prediction = predict_mock(
        &ledger,
        &sample.trusted_context_fixture,
        &sample.snapshot,
        id,
    )
    .unwrap();
    assert_eq!(prediction.expected_date, date("2026-09-15"));
    assert_eq!(prediction, preview);
    let mut schedule = Schedule::from_prediction(&prediction);
    schedule.expected_date = date("2026-09-17");
    let mut memory = MemoryReminderMock::default();
    let mut sink = Sink::default();
    let unconfirmed = scan(&mut memory, &ledger, "2026-08-16", &mut sink);
    assert!(unconfirmed.items.is_empty());
    confirm(&mut memory, &ledger, id, schedule.clone());
    let mut cases = vec![
        json!({"name":"date_preview_and_confirmation","prediction":prediction,"confirmed_schedule":schedule,"before_confirmation":unconfirmed}),
    ];
    sink.outcomes = vec![
        DeliveryOutcome::DefiniteFailure,
        DeliveryOutcome::SimulatedSuccess,
    ];
    let failed = scan(&mut memory, &ledger, "2026-09-14", &mut sink);
    let waiting = scan(&mut memory, &ledger, "2026-09-14", &mut sink);
    let retried = scan(&mut memory, &ledger, "2026-09-15", &mut sink);
    assert_eq!(failed.items[0].state, ItemState::RetryNextDay);
    assert_eq!(waiting.items[0].state, ItemState::RetryWaiting);
    assert_eq!(retried.items[0].state, ItemState::SimulatedSuccess);
    assert_eq!(sink.messages.len(), 2);
    assert_eq!(sink.messages[0], sink.messages[1]);
    cases.push(json!({"name":"ordinary_failure_and_next_day_retry","reports":[failed,waiting,retried],"simulated_attempts":2}));
    let retry = scan(&mut memory, &ledger, "2026-09-15", &mut sink);
    assert_eq!(retry.items[0].state, ItemState::AlreadySimulated);
    assert_eq!(sink.messages.len(), 2);
    cases.push(json!({"name":"ordinary_no_duplicate","report":retry,"simulated_attempts_total":2}));
    change(&mut ledger, id, MockIntentKind::Record, "2026-09-15");
    let before = scan(&mut memory, &ledger, "2026-09-16", &mut sink);
    let due = scan(&mut memory, &ledger, "2026-09-17", &mut sink);
    assert_eq!(before.items[0].state, ItemState::NotDue);
    assert_eq!(due.items[0].state, ItemState::SimulatedSuccess);
    assert_eq!(sink.messages[2].kind, ReminderKind::CancellationCheck);
    cases.push(json!({"name":"cancellation_due_uses_same_current_period","reports":[before,due],"message":sink.messages[2]}));
    change(&mut ledger, id, MockIntentKind::Revoke, "2026-09-18");
    let revoked = scan(&mut memory, &ledger, "2026-09-18", &mut sink);
    assert!(revoked
        .items
        .iter()
        .all(|i| i.kind == ReminderKind::ChargeAdvance));
    assert_eq!(sink.messages.len(), 3);
    let next = memory
        .current_schedule_mock(&ledger, "user_demo", id)
        .unwrap();
    assert_eq!(next.expected_date, date("2026-10-15"));
    cases.push(json!({"name":"revoke_stops_cancel_and_restores_original_calendar_anchor","report":revoked,"next_predicted_date":next.expected_date.to_string(),"merchant_service_restored":false}));
    let mut active = empty();
    active.entries = ledger.list_mock("user_demo").unwrap().to_vec();
    let mut cancelling = active.clone();
    cancelling.entries[0].status = LedgerStatus::Cancelling;
    let cancelling = MemoryConfirmationMock::from_fixture(cancelling);
    let mut m = MemoryReminderMock::default();
    confirm(&mut m, &cancelling, id, schedule.clone());
    let mut s = Sink::default();
    let late = scan(&mut m, &cancelling, "2026-10-01", &mut s);
    let repeated = scan(&mut m, &cancelling, "2026-10-01", &mut s);
    assert!(late.items[0].late);
    assert_eq!(s.messages.len(), 1);
    cases.push(
        json!({"name":"late_cancel_review_once","reports":[late,repeated],"simulated_attempts":1}),
    );
    let active = MemoryConfirmationMock::from_fixture(active);
    let mut m = MemoryReminderMock::default();
    confirm(&mut m, &active, id, schedule.clone());
    let mut s = Sink::default();
    let skipped = scan(&mut m, &active, "2026-10-01", &mut s);
    assert_eq!(skipped.items[0].state, ItemState::SkippedExpired);
    assert!(s.messages.is_empty());
    cases.push(json!({"name":"expired_ordinary_not_sent","report":skipped,"simulated_attempts":0}));
    let daily = Schedule {
        cycle: Cycle::Day,
        anchor_date: date("2026-09-16"),
        period_index: 1,
        expected_date: date("2026-09-17"),
        enabled: true,
        days_before: 0,
    };
    let mut m = MemoryReminderMock::default();
    confirm(&mut m, &active, id, daily);
    let mut s = Sink::default();
    let first = scan(&mut m, &active, "2026-09-17", &mut s);
    let next = scan(&mut m, &active, "2026-09-18", &mut s);
    assert_eq!(s.messages.len(), 2);
    assert_ne!(s.messages[0].cycle_ref, s.messages[1].cycle_ref);
    cases.push(json!({"name":"daily_same_day_no_missed_current_cycle","reports":[first,next],"simulated_attempts":2}));
    let mut m = MemoryReminderMock::default();
    confirm(&mut m, &active, id, schedule);
    let mut s = Sink {
        outcomes: vec![DeliveryOutcome::Unknown],
        ..Sink::default()
    };
    let unknown = scan(&mut m, &active, "2026-09-14", &mut s);
    let held = scan(&mut m, &active, "2026-09-15", &mut s);
    assert_eq!(held.items[0].state, ItemState::UnknownNeedsReview);
    assert_eq!(s.messages.len(), 1);
    cases.push(json!({"name":"unknown_result_needs_review","reports":[unknown,held],"simulated_attempts":1}));
    json!({"schema_version":1,"fictional":true,"production_registered":false,"notification_sent":false,"audit_integrated":false,"database_connected":false,"security_kernel_connected":false,"merchant_action_executed":false,"channel":"IN_APP_MOCK_ONLY","clock_source":"FIXED_FICTIONAL_FIXTURES","case_count":cases.len(),"cases":cases})
}
fn main() {
    if std::env::args_os().len() != 1 {
        eprintln!("FIXED_MOCK_ONLY: external confirmation and recipient arguments refused");
        std::process::exit(2);
    }
    println!("{}", serde_json::to_string_pretty(&replay()).unwrap());
}
