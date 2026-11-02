//! Fixed fictional orchestration over existing module APIs, not a public contract.
//! No caller-controlled approval, identity, dates, transport or persistence.
use chrono::NaiveDate;
use safeflow_subscription_evidence::{
    billing_query_mock, confirmation_mock::*, managed_ledger_mock::*, recognition::GroupStatus,
    reminder_mock::*, *,
};
use serde_json::{json, Value};

const CAPTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));
const NOW: &str = "2026-10-13T12:00:00+08:00";
const EXPIRY: &str = "2026-10-13T12:05:00+08:00";

#[derive(Clone, Copy)]
pub enum Scenario {
    Complete,
    SourceIncomplete,
    LedgerUnavailable,
    ServiceUnknown,
    RegistrationUnconfirmed,
    SettingsUnconfirmed,
    DeliveryUnknown,
}
impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete_local_chain",
            Self::SourceIncomplete => "source_incomplete_stops_chain",
            Self::LedgerUnavailable => "ledger_unavailable_is_not_empty",
            Self::ServiceUnknown => "unknown_service_is_not_guessed",
            Self::RegistrationUnconfirmed => "registration_requires_confirmation",
            Self::SettingsUnconfirmed => "reminder_settings_require_confirmation",
            Self::DeliveryUnknown => "unknown_delivery_waits_for_review",
        }
    }
}

fn context(id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "fictional-handoff-session".into(),
        task_id: "fictional-handoff-task".into(),
        node_id: format!("fictional-{id}"),
        operation_id: id.into(),
        idempotency_key: format!("fictional-key-{id}"),
        plan_version: 1,
    }
}
fn ledger_fixture(unavailable: bool) -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: (!unavailable).then(|| "fictional-independent-ledger".into()),
        read: ReadState {
            state: if unavailable {
                ReadKind::Unavailable
            } else {
                ReadKind::Complete
            },
            is_complete: !unavailable,
            next_cursor: None,
            reason: unavailable.then(|| "FICTIONAL_STORAGE_UNAVAILABLE".into()),
        },
        entries: vec![],
    }
}
struct RegistrationGate {
    expected: MockConfirmationBinding,
    allow: bool,
    calls: usize,
}
impl MockGateVerifier for RegistrationGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if self.allow && self.calls == 1 && binding == &self.expected {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
struct SettingsGate {
    expected: ScheduleBinding,
    allow: bool,
    calls: usize,
}
impl MockScheduleGate for SettingsGate {
    fn verify_and_consume_mock(&mut self, binding: &ScheduleBinding) -> MockGateOutcome {
        self.calls += 1;
        if self.allow && self.calls == 1 && binding == &self.expected {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
struct IntentGate(MockIntentBinding, usize);
impl MockIntentGateVerifier for IntentGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.1 += 1;
        if self.1 == 1 && binding == &self.0 {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
#[derive(Default)]
struct Sink {
    messages: Vec<ReminderMessage>,
    outcomes: std::collections::VecDeque<DeliveryOutcome>,
}
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome {
        self.messages.push(message.clone());
        self.outcomes
            .pop_front()
            .unwrap_or(DeliveryOutcome::SimulatedSuccess)
    }
}
fn step(
    steps: &mut Vec<Value>,
    name: &str,
    result: Value,
    store: &MemoryConfirmationMock,
    sink: &Sink,
) {
    steps.push(json!({
        "step": name, "result": result,
        "subscription_count": store.list_mock("user_demo").ok().map(|rows| rows.len()),
        "registration_event_count": store.events_mock("user_demo").ok().map(|events| events.len()),
        "intent_event_count": store.intent_events_mock("user_demo").ok().map(|events| events.len()),
        "simulated_delivery_attempts": sink.messages.len(),
    }));
}
fn report(scenario: Scenario, stopped_at: &str, steps: Vec<Value>, sink: &Sink) -> Value {
    json!({
        "name": scenario.name(), "stopped_at": stopped_at, "steps": steps,
        "simulated_messages": sink.messages, "fictional": true,
        "production_registered": false, "notification_sent": false,
        "database_connected": false, "audit_integrated": false,
        "security_kernel_connected": false, "merchant_action_executed": false,
        "merchant_subscription_cancelled": false, "merchant_service_restored": false,
    })
}
fn scan(
    store: &MemoryConfirmationMock,
    reminders: &mut MemoryReminderMock,
    sink: &mut Sink,
    now: &str,
) -> Result<DeliveryReport, &'static str> {
    reminders.scan_mock(store, "user_demo", now, sink)
}
fn change_intent(
    store: &mut MemoryConfirmationMock,
    id: &str,
    kind: MockIntentKind,
    operation: &str,
    day: &str,
) -> Result<MockIntentReceipt, &'static str> {
    let now = format!("{day}T12:00:00+08:00");
    let expiry = format!("{day}T12:05:00+08:00");
    let binding = store
        .prepare_intent_mock(context(operation), id, kind, &now, &expiry)
        .map_err(|_| "INTENT_PREPARE_FAILED")?;
    store
        .commit_intent_mock(
            &binding.draft_id,
            &binding.context,
            &now,
            &mut IntentGate(binding.clone(), 0),
        )
        .map_err(|_| "INTENT_CONFIRM_FAILED")
}

pub fn run_mock(scenario: Scenario) -> Result<Value, &'static str> {
    let mut store = MemoryConfirmationMock::from_fixture(ledger_fixture(matches!(
        scenario,
        Scenario::LedgerUnavailable
    )));
    let mut reminders = MemoryReminderMock::default();
    let mut sink = Sink::default();
    let mut steps = Vec::new();
    let source_case = match scenario {
        Scenario::SourceIncomplete => "second-page-failed",
        Scenario::ServiceUnknown => "same-merchant-unknown-service",
        _ => "complete",
    };
    let sample = match billing_query_mock::load_case(
        CAPTURE,
        source_case,
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    ) {
        Ok(sample) => sample,
        Err(error) => {
            step(
                &mut steps,
                "consumption_read",
                json!({"source_case": source_case, "error": error}),
                &store,
                &sink,
            );
            return Ok(report(scenario, "consumption_read", steps, &sink));
        }
    };
    step(
        &mut steps,
        "consumption_read",
        json!({
            "source_case": source_case, "window": {
                "as_of": sample.snapshot.window.as_of,
                "start_inclusive": sample.snapshot.window.start_inclusive,
                "end_exclusive": sample.snapshot.window.end_exclusive,
                "timezone": sample.snapshot.window.timezone,
            },
            "observed_transaction_count": sample.snapshot.transactions.len(),
            "source_commit": "1044e918d7d58301e0ab49aa03016f035642245f",
            "source_is_fictional": true, "source_capture_unchanged": true,
        }),
        &store,
        &sink,
    );
    let discovered = match store.discover_mock(&sample.trusted_context_fixture, &sample.snapshot) {
        Ok(review) => review,
        Err(error) => {
            step(
                &mut steps,
                "ledger_and_discovery",
                json!({"error": format!("{error:?}")}),
                &store,
                &sink,
            );
            return Ok(report(scenario, "ledger_and_discovery", steps, &sink));
        }
    };
    let can_register = discovered.matching_executed
        && discovered.groups.len() == 1
        && discovered.groups[0].disposition == Disposition::NotInLedger
        && discovered.groups[0].group.status == GroupStatus::Suspected;
    step(
        &mut steps,
        "ledger_and_discovery",
        json!(discovered),
        &store,
        &sink,
    );
    if !can_register {
        return Ok(report(scenario, "service_review", steps, &sink));
    }
    // The unchanged capture is queried in October but its last two charges are
    // July/August. The September suggestion is overdue, so do not roll it forward
    // or change the source clock to manufacture a convenient prediction.
    let preview =
        predict_candidate_mock(&store, &sample.trusted_context_fixture, &sample.snapshot, 0);
    if preview != Err("DATE_NEEDS_INPUT") {
        return Err("EXPECTED_DATE_INPUT_REQUIRED");
    }
    step(
        &mut steps,
        "date_preview_requires_input",
        json!({"error": "DATE_NEEDS_INPUT", "notification_profile_created": false}),
        &store,
        &sink,
    );
    let registration = store
        .prepare_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            context("register"),
            0,
            EXPIRY,
        )
        .map_err(|_| "REGISTRATION_PREPARE_FAILED")?;
    let mut registration_gate = RegistrationGate {
        expected: registration.clone(),
        allow: !matches!(scenario, Scenario::RegistrationUnconfirmed),
        calls: 0,
    };
    let registered = match store.commit_mock(
        &registration.draft_id,
        &registration.context,
        NOW,
        &mut registration_gate,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            step(
                &mut steps,
                "registration_confirmation",
                json!({"error": format!("{error:?}")}),
                &store,
                &sink,
            );
            return Ok(report(scenario, "registration_confirmation", steps, &sink));
        }
    };
    step(
        &mut steps,
        "registration_confirmation",
        json!(registered),
        &store,
        &sink,
    );
    let repeated = store
        .commit_mock(
            &registration.draft_id,
            &registration.context,
            NOW,
            &mut registration_gate,
        )
        .map_err(|_| "REGISTRATION_REPLAY_FAILED")?;
    if repeated != registered || registration_gate.calls != 1 {
        return Err("REGISTRATION_NOT_IDEMPOTENT");
    }
    step(
        &mut steps,
        "registration_retry",
        json!({"original_receipt_reused": true, "confirmation_calls": registration_gate.calls}),
        &store,
        &sink,
    );
    // This explicit, fixed, fictional user form is NOT inferred from consumption.
    // The October occurrence is edited to the 17th, the following month keeps 15th.
    let manual = Schedule {
        cycle: safeflow_subscription_evidence::recognition::Cycle::CalendarMonth,
        anchor_date: NaiveDate::from_ymd_opt(2026, 7, 15).ok_or("DATE_INVALID")?,
        period_index: 3,
        expected_date: NaiveDate::from_ymd_opt(2026, 10, 17).ok_or("DATE_INVALID")?,
        enabled: true,
        days_before: 3,
    };
    let settings = reminders.prepare_mock(
        &store,
        context("settings"),
        &registered.subscription_id,
        manual,
        NOW,
        EXPIRY,
    )?;
    let mut settings_gate = SettingsGate {
        expected: settings.clone(),
        allow: !matches!(scenario, Scenario::SettingsUnconfirmed),
        calls: 0,
    };
    let settings_receipt = match reminders.confirm_mock(&store, &settings, NOW, &mut settings_gate)
    {
        Ok(receipt) => receipt,
        Err(error) => {
            let no_delivery = scan(
                &store,
                &mut reminders,
                &mut sink,
                "2026-10-14T12:00:00+08:00",
            )?;
            step(
                &mut steps,
                "reminder_settings_confirmation",
                json!({"error": error, "scan": no_delivery, "date_source": "EXPLICIT_FICTIONAL_USER_FORM"}),
                &store,
                &sink,
            );
            return Ok(report(
                scenario,
                "reminder_settings_confirmation",
                steps,
                &sink,
            ));
        }
    };
    step(
        &mut steps,
        "reminder_settings_confirmation",
        json!({"receipt": settings_receipt, "date_source": "EXPLICIT_FICTIONAL_USER_FORM", "estimated_not_merchant_promise": true, "registration_and_settings_confirmed_separately": true}),
        &store,
        &sink,
    );
    sink.outcomes = if matches!(scenario, Scenario::DeliveryUnknown) {
        [DeliveryOutcome::Unknown].into()
    } else {
        [
            DeliveryOutcome::DefiniteFailure,
            DeliveryOutcome::SimulatedSuccess,
        ]
        .into()
    };
    let first = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-14T12:00:00+08:00",
    )?;
    step(
        &mut steps,
        "ordinary_first_attempt",
        json!(first),
        &store,
        &sink,
    );
    let same_day = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-14T12:01:00+08:00",
    )?;
    step(
        &mut steps,
        "ordinary_same_day_repeat",
        json!(same_day),
        &store,
        &sink,
    );
    let next_day = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-15T12:00:00+08:00",
    )?;
    step(
        &mut steps,
        "ordinary_next_day",
        json!(next_day),
        &store,
        &sink,
    );
    if matches!(scenario, Scenario::DeliveryUnknown) {
        return Ok(report(scenario, "delivery_result_review", steps, &sink));
    }
    let repeated_delivery = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-15T12:01:00+08:00",
    )?;
    step(
        &mut steps,
        "ordinary_success_repeat",
        json!(repeated_delivery),
        &store,
        &sink,
    );
    let recorded = change_intent(
        &mut store,
        &registered.subscription_id,
        MockIntentKind::Record,
        "cancel",
        "2026-10-16",
    )?;
    step(
        &mut steps,
        "record_cancel_intent",
        json!(recorded),
        &store,
        &sink,
    );
    let late = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-18T12:00:00+08:00",
    )?;
    step(&mut steps, "late_cancel_check", json!(late), &store, &sink);
    let late_repeated = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-18T12:01:00+08:00",
    )?;
    step(
        &mut steps,
        "late_cancel_check_repeat",
        json!(late_repeated),
        &store,
        &sink,
    );
    let revoked = change_intent(
        &mut store,
        &registered.subscription_id,
        MockIntentKind::Revoke,
        "revoke",
        "2026-10-19",
    )?;
    step(
        &mut steps,
        "revoke_local_intent",
        json!(revoked),
        &store,
        &sink,
    );
    let after_revoke = scan(
        &store,
        &mut reminders,
        &mut sink,
        "2026-10-19T12:00:00+08:00",
    )?;
    let next = reminders.current_schedule_mock(&store, "user_demo", &registered.subscription_id)?;
    step(
        &mut steps,
        "next_cycle_keeps_original_anchor",
        json!({"scan": after_revoke, "schedule": next}),
        &store,
        &sink,
    );
    let history = store
        .confirmation_result_mock("user_demo", "register")
        .map_err(|_| "HISTORY_QUERY_FAILED")?;
    if history != &registered {
        return Err("HISTORY_CHANGED");
    }
    let current = store
        .list_mock("user_demo")
        .map_err(|_| "CURRENT_LEDGER_FAILED")?;
    let current: Vec<_> = current.iter().map(|entry| json!({
        "subscription_id": entry.subscription_id,
        "status": match entry.status { LedgerStatus::Active => "ACTIVE", LedgerStatus::Cancelling => "CANCELLING", LedgerStatus::Error => "ERROR" },
    })).collect();
    step(
        &mut steps,
        "registration_history_and_current_state",
        json!({"original_receipt": history, "current_ledger": current}),
        &store,
        &sink,
    );
    Ok(report(scenario, "complete", steps, &sink))
}

pub fn replay_mock() -> Result<Value, &'static str> {
    let scenarios = [
        Scenario::Complete,
        Scenario::SourceIncomplete,
        Scenario::LedgerUnavailable,
        Scenario::ServiceUnknown,
        Scenario::RegistrationUnconfirmed,
        Scenario::SettingsUnconfirmed,
        Scenario::DeliveryUnknown,
    ];
    let cases: Result<Vec<_>, _> = scenarios.into_iter().map(run_mock).collect();
    Ok(
        json!({"schema_version": 1, "mode": "OFFLINE_FICTIONAL_SUBSCRIPTION_HANDOFF", "fictional": true,
        "source_commit": "1044e918d7d58301e0ab49aa03016f035642245f", "subscription_base": "26fcea7cb8efeddbf7a2c24434d4004047971562",
        "formal_integration_accepted": false, "production_registered": false, "notification_sent": false,
        "database_connected": false, "security_kernel_connected": false, "audit_integrated": false,
        "merchant_action_executed": false, "case_count": scenarios.len(), "cases": cases?}),
    )
}
