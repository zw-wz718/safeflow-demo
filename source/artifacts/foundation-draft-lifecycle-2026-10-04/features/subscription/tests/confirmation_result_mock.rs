use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::*, *};
const NOW: &str = "2026-10-13T12:01:00+08:00";
const EXPIRY: &str = "2026-10-13T12:05:00+08:00";
const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));
fn ledger() -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("mock-result-ledger".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![],
    }
}
fn context(op: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "mock-session".into(),
        task_id: "mock-task".into(),
        node_id: format!("mock-node-{op}"),
        operation_id: op.into(),
        idempotency_key: format!("mock-key-{op}"),
        plan_version: 1,
    }
}
fn prepare(store: &mut MemoryConfirmationMock) -> MockConfirmationBinding {
    let sample = billing_query_mock::load_case(
        INPUT,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap();
    store
        .prepare_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            context("register"),
            0,
            EXPIRY,
        )
        .unwrap()
}
struct RegistrationGate {
    expected: MockConfirmationBinding,
    calls: usize,
}
impl MockGateVerifier for RegistrationGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
        _now: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if self.calls == 1 && binding == &self.expected {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
struct IntentGate(MockIntentBinding);
impl MockIntentGateVerifier for IntentGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
        _now: &str,
    ) -> MockGateOutcome {
        if binding == &self.0 {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}

#[test]
fn repeated_queries_return_original_receipt_without_new_events_or_confirmation() {
    let mut store = MemoryConfirmationMock::from_fixture(ledger());
    let draft = prepare(&mut store);
    let mut gate = RegistrationGate {
        expected: draft.clone(),
        calls: 0,
    };
    let original = store
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            store
                .confirmation_result_mock("user_demo", "register")
                .unwrap(),
            &original
        );
    }
    assert_eq!(gate.calls, 1);
    assert_eq!(store.events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(store.list_mock("user_demo").unwrap().len(), 1);
    assert!(store.intent_events_mock("user_demo").unwrap().is_empty());
}

#[test]
fn registration_history_stays_active_after_current_state_changes_to_cancelling() {
    let mut store = MemoryConfirmationMock::from_fixture(ledger());
    let draft = prepare(&mut store);
    let mut gate = RegistrationGate {
        expected: draft.clone(),
        calls: 0,
    };
    let original = store
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    let intent = store
        .prepare_intent_mock(
            context("cancel-intent"),
            &original.subscription_id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .unwrap();
    store
        .commit_intent_mock(
            &intent.draft_id,
            &intent.context,
            NOW,
            &mut IntentGate(intent.clone()),
        )
        .unwrap();
    assert_eq!(
        store
            .confirmation_result_mock("user_demo", "register")
            .unwrap(),
        &original
    );
    assert_eq!(original.state, "ACTIVE");
    assert_eq!(
        store.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Cancelling
    );
    assert_eq!(store.intent_events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(
        store
            .intent_result_mock("user_demo", "cancel-intent")
            .unwrap()
            .ledger_state_at_operation,
        "CANCELLING"
    );
    assert_eq!(
        store.confirmation_result_mock("user_demo", "cancel-intent"),
        Err(MockIssue::ResourceUnavailable)
    );
    assert_eq!(
        store.intent_result_mock("user_demo", "register"),
        Err(MockIssue::ResourceUnavailable)
    );
}

#[test]
fn pending_unknown_and_foreign_queries_return_no_receipt() {
    let mut store = MemoryConfirmationMock::from_fixture(ledger());
    let draft = prepare(&mut store);
    assert_eq!(
        store.confirmation_result_mock("user_demo", "register"),
        Err(MockIssue::ResourceUnavailable)
    );
    let mut gate = RegistrationGate {
        expected: draft.clone(),
        calls: 0,
    };
    store
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    assert_eq!(
        store.confirmation_result_mock("foreign", "register"),
        Err(MockIssue::ResourceUnavailable)
    );
    for op in ["unknown", ""] {
        assert_eq!(
            store.confirmation_result_mock("user_demo", op),
            Err(MockIssue::ResourceUnavailable)
        );
    }
    assert_eq!(store.events_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn historical_lookup_still_requires_an_available_owned_ledger() {
    for state in [ReadKind::Unavailable, ReadKind::Incomplete] {
        let mut fixture = ledger();
        fixture.read.state = state;
        fixture.read.is_complete = false;
        let store = MemoryConfirmationMock::from_fixture(fixture);
        assert_eq!(
            store.confirmation_result_mock("user_demo", "register"),
            Err(MockIssue::LedgerUnavailable)
        );
    }
    let mut fixture = ledger();
    fixture.owner_user_id = "foreign".into();
    let store = MemoryConfirmationMock::from_fixture(fixture);
    assert_eq!(
        store.confirmation_result_mock("user_demo", "register"),
        Err(MockIssue::ResourceUnavailable)
    );
}

#[test]
fn unified_reminder_flow_preserves_registration_history_and_deduplicates_late_cancel_check() {
    use chrono::NaiveDate;
    use safeflow_subscription_evidence::{recognition::Cycle, reminder_mock::*};
    struct ScheduleGate(ScheduleBinding);
    impl MockScheduleGate for ScheduleGate {
        fn verify_and_consume_mock(&mut self, binding: &ScheduleBinding) -> MockGateOutcome {
            if binding == &self.0 {
                MockGateOutcome::Confirmed
            } else {
                MockGateOutcome::Denied
            }
        }
    }
    #[derive(Default)]
    struct Sink(Vec<ReminderKind>);
    impl MockInAppSink for Sink {
        fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome {
            self.0.push(message.kind);
            DeliveryOutcome::SimulatedSuccess
        }
    }
    let mut store = MemoryConfirmationMock::from_fixture(ledger());
    let draft = prepare(&mut store);
    let mut gate = RegistrationGate {
        expected: draft.clone(),
        calls: 0,
    };
    let original = store
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    let mut reminders = MemoryReminderMock::default();
    let schedule = Schedule {
        cycle: Cycle::CalendarMonth,
        anchor_date: NaiveDate::from_ymd_opt(2026, 9, 20).unwrap(),
        period_index: 1,
        expected_date: NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
        enabled: true,
        days_before: 3,
    };
    let settings = reminders
        .prepare_mock(
            &store,
            context("reminder-settings"),
            &original.subscription_id,
            schedule,
            NOW,
            EXPIRY,
        )
        .unwrap();
    reminders
        .confirm_mock(&store, &settings, NOW, &mut ScheduleGate(settings.clone()))
        .unwrap();
    let mut sink = Sink::default();
    let ordinary = reminders
        .scan_mock(&store, "user_demo", "2026-10-18T12:00:00+08:00", &mut sink)
        .unwrap();
    assert_eq!(ordinary.items[0].state, ItemState::SimulatedSuccess);
    let intent_time = "2026-10-19T12:00:00+08:00";
    let intent = store
        .prepare_intent_mock(
            context("cancel-intent"),
            &original.subscription_id,
            MockIntentKind::Record,
            intent_time,
            "2026-10-19T12:05:00+08:00",
        )
        .unwrap();
    store
        .commit_intent_mock(
            &intent.draft_id,
            &intent.context,
            intent_time,
            &mut IntentGate(intent.clone()),
        )
        .unwrap();
    let late = reminders
        .scan_mock(&store, "user_demo", "2026-10-21T12:00:00+08:00", &mut sink)
        .unwrap();
    assert_eq!(late.items[0].state, ItemState::SimulatedSuccess);
    assert!(late.items[0].late);
    let repeat = reminders
        .scan_mock(&store, "user_demo", "2026-10-22T12:00:00+08:00", &mut sink)
        .unwrap();
    assert_eq!(repeat.items[0].state, ItemState::AlreadySimulated);
    assert_eq!(
        sink.0,
        vec![ReminderKind::ChargeAdvance, ReminderKind::CancellationCheck]
    );
    assert!(!ordinary.notification_sent && !late.notification_sent && !repeat.notification_sent);
    assert_eq!(
        store
            .confirmation_result_mock("user_demo", "register")
            .unwrap(),
        &original
    );
    assert_eq!(original.state, "ACTIVE");
    assert_eq!(
        store.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Cancelling
    );
    assert_eq!(store.events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(store.intent_events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(gate.calls, 1);
}
