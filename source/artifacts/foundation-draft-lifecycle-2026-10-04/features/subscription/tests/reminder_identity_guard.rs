//! Existing ledger rows with incomplete service identity must not acquire a
//! usable reminder profile, including when a fictional user fills the date.
use chrono::NaiveDate;
use safeflow_subscription_evidence::{
    confirmation_mock::*, managed_ledger_mock::*, recognition::Cycle, reminder_mock::*, *,
};

const NOW: &str = "2026-10-15T12:00:00+08:00";
const EXPIRY: &str = "2026-10-15T12:05:00+08:00";
fn entry(id: &str) -> LedgerEntry {
    LedgerEntry {
        subscription_id: id.into(),
        owner_user_id: "user_demo".into(),
        source_id: Some("fictional-source".into()),
        merchant_id: Some("merchant-demo".into()),
        service_ref: Some("service-demo".into()),
        status: LedgerStatus::Active,
    }
}
fn ledger(entries: Vec<LedgerEntry>) -> MemoryConfirmationMock {
    MemoryConfirmationMock::from_fixture(LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("fictional-identity-check".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries,
    })
}
fn operation(id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "fictional-session".into(),
        task_id: "fictional-task".into(),
        node_id: "fictional-settings".into(),
        operation_id: id.into(),
        idempotency_key: format!("fictional-{id}"),
        plan_version: 1,
    }
}
fn schedule() -> Schedule {
    Schedule {
        cycle: Cycle::CalendarMonth,
        anchor_date: NaiveDate::from_ymd_opt(2026, 9, 20).unwrap(),
        period_index: 1,
        expected_date: NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
        enabled: true,
        days_before: 3,
    }
}
struct Gate(ScheduleBinding, usize);
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, binding: &ScheduleBinding) -> MockGateOutcome {
        self.1 += 1;
        if binding == &self.0 && self.1 == 1 {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
#[derive(Default)]
struct Sink(Vec<ReminderMessage>);
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome {
        self.0.push(message.clone());
        DeliveryOutcome::SimulatedSuccess
    }
}

#[test]
fn missing_source_merchant_or_service_cannot_prepare_or_send_a_reminder() {
    for missing in 0..3 {
        let mut row = entry("sub-A");
        match missing {
            0 => row.source_id = None,
            1 => row.merchant_id = None,
            _ => row.service_ref = None,
        }
        let store = ledger(vec![row]);
        let mut reminders = MemoryReminderMock::default();
        assert_eq!(
            reminders.prepare_mock(
                &store,
                operation("settings"),
                "sub-A",
                schedule(),
                NOW,
                EXPIRY
            ),
            Err("SERVICE_NEEDS_REVIEW"),
            "missing field {missing}"
        );
        let mut sink = Sink::default();
        let report = reminders
            .scan_mock(&store, "user_demo", "2026-10-20T12:00:00+08:00", &mut sink)
            .unwrap();
        assert!(report.items.is_empty());
        assert!(sink.0.is_empty());
        assert!(!report.notification_sent);
        assert_eq!(store.list_mock("user_demo").unwrap().len(), 1);
        assert!(store.events_mock("user_demo").unwrap().is_empty());
    }
}

#[test]
fn missing_service_in_cancelling_record_cannot_bypass_review() {
    let mut row = entry("sub-A");
    row.status = LedgerStatus::Cancelling;
    row.service_ref = None;
    let store = ledger(vec![row]);
    let mut reminders = MemoryReminderMock::default();
    assert_eq!(
        reminders.prepare_mock(
            &store,
            operation("settings"),
            "sub-A",
            schedule(),
            NOW,
            EXPIRY
        ),
        Err("SERVICE_NEEDS_REVIEW")
    );
}

#[test]
fn unrelated_incomplete_record_does_not_disable_a_known_owned_service() {
    let mut unknown = entry("sub-unknown");
    unknown.service_ref = None;
    let store = ledger(vec![entry("sub-A"), unknown]);
    let mut reminders = MemoryReminderMock::default();
    let binding = reminders
        .prepare_mock(
            &store,
            operation("settings"),
            "sub-A",
            schedule(),
            NOW,
            EXPIRY,
        )
        .unwrap();
    let mut gate = Gate(binding.clone(), 0);
    reminders
        .confirm_mock(&store, &binding, NOW, &mut gate)
        .unwrap();
    let mut sink = Sink::default();
    let report = reminders
        .scan_mock(&store, "user_demo", "2026-10-17T12:00:00+08:00", &mut sink)
        .unwrap();
    assert_eq!(gate.1, 1);
    assert_eq!(report.items[0].state, ItemState::SimulatedSuccess);
    assert_eq!(sink.0.len(), 1);
    assert_eq!(sink.0[0].subscription_id, "sub-A");
    assert!(!report.notification_sent);
}

#[test]
fn error_and_unknown_ownership_keep_their_existing_failure_precedence() {
    let mut row = entry("sub-A");
    row.status = LedgerStatus::Error;
    row.service_ref = None;
    let store = ledger(vec![row]);
    let mut reminders = MemoryReminderMock::default();
    assert_eq!(
        reminders.prepare_mock(
            &store,
            operation("settings"),
            "sub-A",
            schedule(),
            NOW,
            EXPIRY
        ),
        Err("REGISTRATION_NEEDS_REVIEW")
    );
    let mut foreign = operation("foreign");
    foreign.user_id = "other-user".into();
    assert_eq!(
        reminders.prepare_mock(&store, foreign, "sub-A", schedule(), NOW, EXPIRY),
        Err("RESOURCE_UNAVAILABLE")
    );
}
