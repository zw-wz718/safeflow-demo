//! One fixed fictional lifecycle; no real identity, authorization or merchant calls.
use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::*, *};
use serde_json::{json, Value};

const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));
const NOW: &str = "2026-10-14T12:00:00+08:00";
const LATER: &str = "2026-10-14T12:01:00+08:00";
const EXPIRY: &str = "2026-10-14T12:05:00+08:00";
fn operation(id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "mock-session".into(),
        task_id: "mock-lifecycle-task".into(),
        node_id: format!("mock-node-{id}"),
        operation_id: id.into(),
        idempotency_key: format!("mock-key-{id}"),
        plan_version: 1,
    }
}
struct RegistrationGate {
    expected: MockConfirmationBinding,
    confirmed: bool,
    calls: usize,
}
impl MockGateVerifier for RegistrationGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
        _now: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if self.confirmed && self.calls == 1 && binding == &self.expected {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
struct IntentGate {
    expected: MockIntentBinding,
    confirmed: bool,
    calls: usize,
}
impl MockIntentGateVerifier for IntentGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
        _now: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if self.confirmed && self.calls == 1 && binding == &self.expected {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
fn snapshot(name: &str, ledger: &MemoryConfirmationMock, s: &Sample) -> Value {
    let rows = ledger.list_mock("user_demo").unwrap();
    let review = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    let group = &review.groups[0];
    json!({"step":name,"subscription_count":rows.len(),"subscription_id":rows.first().map(|r|r.subscription_id.as_str()),
        "local_ledger_status":rows.first().map(|r|match r.status {LedgerStatus::Active=>"ACTIVE",LedgerStatus::Cancelling=>"CANCELLING",LedgerStatus::Error=>"ERROR"}),
        "disposition":group.disposition,"reason":group.reason,"actual_charge_count":group.group.actual_charges.len(),
        "registration_events":ledger.events_mock("user_demo").unwrap().len(),"intent_events":ledger.intent_events_mock("user_demo").unwrap().len()})
}
fn replay() -> Value {
    let s = billing_query_mock::load_case(
        INPUT,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap();
    let mut ledger = MemoryConfirmationMock::from_fixture(LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("mock-lifecycle".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![],
    });
    let mut steps = vec![snapshot("discover_unregistered", &ledger, &s)];
    let registration = ledger
        .prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation("register"),
            0,
            EXPIRY,
        )
        .unwrap();
    let mut rg = RegistrationGate {
        expected: registration.clone(),
        confirmed: false,
        calls: 0,
    };
    assert_eq!(
        ledger.commit_mock(&registration.draft_id, &registration.context, NOW, &mut rg),
        Err(MockIssue::ConfirmationRequired)
    );
    steps.push(snapshot("registration_unconfirmed", &ledger, &s));
    rg = RegistrationGate {
        expected: registration.clone(),
        confirmed: true,
        calls: 0,
    };
    let registered = ledger
        .commit_mock(&registration.draft_id, &registration.context, NOW, &mut rg)
        .unwrap();
    steps.push(snapshot("registration_confirmed", &ledger, &s));
    assert_eq!(
        ledger
            .commit_mock(
                &registration.draft_id,
                &registration.context,
                LATER,
                &mut rg
            )
            .unwrap(),
        registered
    );
    assert_eq!(rg.calls, 1);
    steps.push(snapshot("registration_retried", &ledger, &s));

    let record = ledger
        .prepare_intent_mock(
            operation("record"),
            &registered.subscription_id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .unwrap();
    let mut ig = IntentGate {
        expected: record.clone(),
        confirmed: false,
        calls: 0,
    };
    assert_eq!(
        ledger.commit_intent_mock(&record.draft_id, &record.context, LATER, &mut ig),
        Err(MockIssue::ConfirmationRequired)
    );
    steps.push(snapshot("record_unconfirmed", &ledger, &s));
    ig = IntentGate {
        expected: record.clone(),
        confirmed: true,
        calls: 0,
    };
    let recorded = ledger
        .commit_intent_mock(&record.draft_id, &record.context, LATER, &mut ig)
        .unwrap();
    steps.push(snapshot("record_confirmed", &ledger, &s));
    let registration_history_while_cancelling = ledger
        .confirmation_result_mock("user_demo", "register")
        .unwrap()
        .clone();
    assert_eq!(registration_history_while_cancelling, registered);
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Cancelling
    );
    assert_eq!(
        ledger
            .commit_intent_mock(&record.draft_id, &record.context, LATER, &mut ig)
            .unwrap(),
        recorded
    );
    assert_eq!(ig.calls, 1);
    steps.push(snapshot("record_retried", &ledger, &s));

    let revoke = ledger
        .prepare_intent_mock(
            operation("revoke"),
            &registered.subscription_id,
            MockIntentKind::Revoke,
            NOW,
            EXPIRY,
        )
        .unwrap();
    let mut vg = IntentGate {
        expected: revoke.clone(),
        confirmed: false,
        calls: 0,
    };
    assert_eq!(
        ledger.commit_intent_mock(&revoke.draft_id, &revoke.context, LATER, &mut vg),
        Err(MockIssue::ConfirmationRequired)
    );
    steps.push(snapshot("revoke_unconfirmed", &ledger, &s));
    vg = IntentGate {
        expected: revoke.clone(),
        confirmed: true,
        calls: 0,
    };
    let revoked = ledger
        .commit_intent_mock(&revoke.draft_id, &revoke.context, LATER, &mut vg)
        .unwrap();
    steps.push(snapshot("revoke_confirmed", &ledger, &s));
    assert_eq!(
        ledger
            .commit_intent_mock(&revoke.draft_id, &revoke.context, LATER, &mut vg)
            .unwrap(),
        revoked
    );
    assert_eq!(vg.calls, 1);
    steps.push(snapshot("revoke_retried", &ledger, &s));

    let future = "2027-01-01T00:00:00+08:00";
    assert_eq!(
        ledger
            .commit_mock(
                &registration.draft_id,
                &registration.context,
                future,
                &mut rg
            )
            .unwrap(),
        registered
    );
    assert_eq!(
        ledger
            .commit_intent_mock(&record.draft_id, &record.context, future, &mut ig)
            .unwrap(),
        recorded
    );
    assert_eq!(
        ledger.intent_result_mock("user_demo", "record").unwrap(),
        &recorded
    );
    assert_eq!(rg.calls, 1);
    assert_eq!(ig.calls, 1);
    steps.push(snapshot("historical_retries_after_revoke", &ledger, &s));
    assert_eq!(
        ledger
            .confirmation_result_mock("user_demo", "register")
            .unwrap(),
        &registered
    );
    assert_eq!(
        ledger.prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation("duplicate-registration"),
            0,
            EXPIRY
        ),
        Err(MockIssue::NeedsInput)
    );
    steps.push(snapshot("duplicate_registration_blocked", &ledger, &s));

    assert_eq!(steps.len(), 12);
    for step in &steps {
        assert_eq!(step["actual_charge_count"], 2);
    }
    for step in &steps[2..] {
        assert_eq!(step["subscription_count"], 1);
        assert_eq!(step["subscription_id"], registered.subscription_id);
    }
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Active
    );
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 2);
    json!({"schema_version":1,"fictional":true,"production_registered":false,"security_kernel_connected":false,
        "database_connected":false,"audit_integrated":false,"merchant_action_executed":false,"notification_sent":false,
        "clock_source":"FIXED_FICTIONAL_FIXTURE","task_ref":"mock-lifecycle-task","step_count":steps.len(),"steps":steps,
        "registration_receipt":registered,"record_receipt":recorded,"revoke_receipt":revoked,
        "registration_history_while_cancelling":registration_history_while_cancelling,
        "history_semantics":"Receipts describe past local operations; steps show local state at that point. Neither is merchant status.",
        "audit_semantics":"Memory business events only. No C-AUD mapping, event delivery or acknowledgement is implemented."})
}
fn main() {
    if std::env::args_os().len() != 1 {
        eprintln!("FIXED_MOCK_ONLY: no external authorization arguments accepted");
        std::process::exit(2);
    }
    println!("{}", serde_json::to_string_pretty(&replay()).unwrap());
}
