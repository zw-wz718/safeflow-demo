//! Fixed fictional local-ledger cases. No external authorization arguments.
use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::*, *};
use serde_json::{json, Value};

const NOW: &str = "2026-10-14T12:00:00+08:00";
const COMMIT: &str = "2026-10-14T12:01:00+08:00";
const EXPIRY: &str = "2026-10-14T12:05:00+08:00";

struct FixtureGate {
    expected: MockIntentBinding,
    outcome: MockGateOutcome,
    used: bool,
    calls: usize,
}
impl FixtureGate {
    fn new(binding: &MockIntentBinding, outcome: MockGateOutcome) -> Self {
        Self {
            expected: binding.clone(),
            outcome,
            used: false,
            calls: 0,
        }
    }
}
impl MockIntentGateVerifier for FixtureGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
        _now: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if binding != &self.expected || self.used || binding.minimum_level != "YELLOW" {
            return MockGateOutcome::Denied;
        }
        if self.outcome == MockGateOutcome::Confirmed {
            self.used = true;
        }
        self.outcome
    }
}
fn context(operation: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "mock-session".into(),
        task_id: "mock-task".into(),
        node_id: "mock-intent".into(),
        operation_id: operation.into(),
        idempotency_key: format!("key-{operation}"),
        plan_version: 1,
    }
}
fn fixture(status: LedgerStatus) -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("mock-ledger".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![LedgerEntry {
            subscription_id: "subscription-A".into(),
            owner_user_id: "user_demo".into(),
            source_id: Some("fictional-billing-input-v1".into()),
            merchant_id: Some("merchant_demo_video".into()),
            service_ref: Some("service_demo_video_1".into()),
            status,
        }],
    }
}
fn prepare(
    ledger: &mut MemoryConfirmationMock,
    kind: MockIntentKind,
    operation: &str,
) -> MockIntentBinding {
    ledger
        .prepare_intent_mock(context(operation), "subscription-A", kind, NOW, EXPIRY)
        .unwrap()
}
fn replay() -> Value {
    let mut cases = Vec::new();
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    let record = prepare(&mut ledger, MockIntentKind::Record, "record");
    let mut gate = FixtureGate::new(&record, MockGateOutcome::Denied);
    let blocked = ledger
        .commit_intent_mock(&record.draft_id, &record.context, COMMIT, &mut gate)
        .unwrap_err();
    assert_eq!(blocked, MockIssue::ConfirmationRequired);
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Active
    );
    assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
    cases.push(json!({"name":"unconfirmed", "issue":format!("{blocked:?}"), "event_count":0,"ledger_state":"ACTIVE"}));

    gate = FixtureGate::new(&record, MockGateOutcome::Confirmed);
    let receipt = ledger
        .commit_intent_mock(&record.draft_id, &record.context, COMMIT, &mut gate)
        .unwrap();
    let retry = ledger
        .commit_intent_mock(&record.draft_id, &record.context, COMMIT, &mut gate)
        .unwrap();
    assert_eq!(receipt, retry);
    assert_eq!(gate.calls, 1);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 1);
    cases.push(json!({"name":"record_confirmed_and_retried", "receipt":receipt, "same_original_receipt":true,
        "subscription_count":ledger.list_mock("user_demo").unwrap().len(),"event_count":1,"gate_calls":1}));

    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
    ));
    let sample = billing_query_mock::load_case(
        raw,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap();
    let result = ledger
        .discover_mock(&sample.trusted_context_fixture, &sample.snapshot)
        .unwrap();
    let group = &result.groups[0];
    assert_eq!(group.disposition, Disposition::NeedsReview);
    cases.push(json!({"name":"consumption_after_intent", "disposition":group.disposition,
        "reason":group.reason,"message":public_review_message(group.reason),"actual_charge_count":group.group.actual_charges.len()}));

    let revoke = prepare(&mut ledger, MockIntentKind::Revoke, "revoke");
    let mut gate = FixtureGate::new(&revoke, MockGateOutcome::Confirmed);
    let receipt = ledger
        .commit_intent_mock(&revoke.draft_id, &revoke.context, COMMIT, &mut gate)
        .unwrap();
    assert_eq!(
        ledger
            .commit_intent_mock(&revoke.draft_id, &revoke.context, COMMIT, &mut gate)
            .unwrap(),
        receipt
    );
    assert_eq!(gate.calls, 1);
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Active
    );
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 2);
    cases.push(json!({"name":"revoke_confirmed_and_retried", "receipt":receipt, "same_original_receipt":true,
        "current_ledger_state":"ACTIVE", "subscription_count":1,"event_count":2,"gate_calls":1}));

    let historical = ledger.intent_result_mock("user_demo", "record").unwrap();
    assert_eq!(historical.ledger_state_at_operation, "CANCELLING");
    cases.push(
        json!({"name":"historical_result_after_revoke", "original_record_receipt":historical,
        "current_ledger_state":"ACTIVE","event_count":2,"result_is_historical":true}),
    );

    let mut error_ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Error));
    let error = error_ledger
        .prepare_intent_mock(
            context("error"),
            "subscription-A",
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .unwrap_err();
    assert_eq!(error, MockIssue::StateConflict);
    cases.push(json!({"name":"abnormal_registration_blocked","issue":format!("{error:?}"),"event_count":0}));

    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    let draft = prepare(&mut ledger, MockIntentKind::Record, "strong-auth");
    let mut gate = FixtureGate::new(&draft, MockGateOutcome::StrongAuthRequired);
    let issue = ledger
        .commit_intent_mock(&draft.draft_id, &draft.context, COMMIT, &mut gate)
        .unwrap_err();
    assert_eq!(issue, MockIssue::WaitingAuth);
    assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
    cases.push(json!({"name":"strong_auth_pending","issue":format!("{issue:?}"),"event_count":0,"ledger_state":"ACTIVE"}));

    json!({"schema_version":1,"fictional":true,"production_registered":false,"security_kernel_connected":false,
        "database_connected":false,"audit_integrated":false,"merchant_action_executed":false,
        "notification_sent":false,"clock_source":"FIXED_FICTIONAL_FIXTURE","case_count":cases.len(),"cases":cases})
}
fn main() {
    if std::env::args_os().len() != 1 {
        eprintln!("FIXED_MOCK_ONLY: external confirmation arguments are not accepted");
        std::process::exit(2);
    }
    println!("{}", serde_json::to_string_pretty(&replay()).unwrap());
}
