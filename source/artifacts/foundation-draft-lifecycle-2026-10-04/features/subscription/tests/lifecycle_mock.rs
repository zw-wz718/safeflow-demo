//! Composition checks using the ID returned by actual mock registration.
use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::*, *};

const NOW: &str = "2026-10-14T12:00:00+08:00";
const LATER: &str = "2026-10-14T12:01:00+08:00";
const EXPIRY: &str = "2026-10-14T12:05:00+08:00";
const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));

fn sample() -> Sample {
    billing_query_mock::load_case(
        INPUT,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap()
}
fn empty() -> MemoryConfirmationMock {
    MemoryConfirmationMock::from_fixture(LedgerFixture {
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
    })
}
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
struct IntentGate {
    expected: MockIntentBinding,
    calls: usize,
}
impl MockIntentGateVerifier for IntentGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
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
fn register(
    ledger: &mut MemoryConfirmationMock,
    s: &Sample,
) -> (
    MockConfirmationBinding,
    MockConfirmationReceipt,
    RegistrationGate,
) {
    let draft = ledger
        .prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation("registration"),
            0,
            EXPIRY,
        )
        .unwrap();
    let mut gate = RegistrationGate {
        expected: draft.clone(),
        calls: 0,
    };
    let receipt = ledger
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    (draft, receipt, gate)
}
fn intent(
    ledger: &mut MemoryConfirmationMock,
    id: &str,
    kind: MockIntentKind,
    op: &str,
) -> (MockIntentBinding, MockIntentReceipt, IntentGate) {
    let draft = ledger
        .prepare_intent_mock(operation(op), id, kind, NOW, EXPIRY)
        .unwrap();
    let mut gate = IntentGate {
        expected: draft.clone(),
        calls: 0,
    };
    let receipt = ledger
        .commit_intent_mock(&draft.draft_id, &draft.context, LATER, &mut gate)
        .unwrap();
    (draft, receipt, gate)
}

#[test]
fn discovery_registration_recording_and_revocation_share_one_subscription_and_evidence() {
    let s = sample();
    let mut ledger = empty();
    let first = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(first.groups[0].disposition, Disposition::NotInLedger);
    let (_, registered, _) = register(&mut ledger, &s);
    let row = ledger.list_mock("user_demo").unwrap()[0].clone();
    assert_eq!(row.subscription_id, registered.subscription_id);
    let managed = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(managed.groups[0].disposition, Disposition::AlreadyManaged);
    intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Record,
        "record",
    );
    let review = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(review.groups[0].disposition, Disposition::NeedsReview);
    assert_eq!(review.groups[0].reason, "CANCELLATION_INTENT_NEEDS_REVIEW");
    assert_eq!(
        review.groups[0].group.actual_charges,
        first.groups[0].group.actual_charges
    );
    intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Revoke,
        "revoke",
    );
    assert_eq!(ledger.list_mock("user_demo").unwrap(), &[row]);
    let managed = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(managed.groups[0].disposition, Disposition::AlreadyManaged);
    assert_eq!(
        managed.groups[0].group.actual_charges,
        first.groups[0].group.actual_charges
    );
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 2);
}

#[test]
fn historical_registration_and_record_retries_cannot_overwrite_current_revoked_state() {
    let s = sample();
    let mut ledger = empty();
    let (registration, registered, mut registration_gate) = register(&mut ledger, &s);
    let (record, recorded, mut record_gate) = intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Record,
        "record",
    );
    let (_, revoked, _) = intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Revoke,
        "revoke",
    );
    let future = "2027-01-01T00:00:00+08:00";
    assert_eq!(
        ledger
            .commit_mock(
                &registration.draft_id,
                &registration.context,
                future,
                &mut registration_gate
            )
            .unwrap(),
        registered
    );
    assert_eq!(
        ledger
            .commit_intent_mock(&record.draft_id, &record.context, future, &mut record_gate)
            .unwrap(),
        recorded
    );
    assert_eq!(registration_gate.calls, 1);
    assert_eq!(record_gate.calls, 1);
    assert_eq!(recorded.ledger_state_at_operation, "CANCELLING");
    assert_eq!(revoked.ledger_state_at_operation, "ACTIVE");
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Active
    );
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 2);
}

#[test]
fn registered_service_cannot_be_registered_again_in_either_local_intent_state() {
    let s = sample();
    let mut ledger = empty();
    let (_, registered, _) = register(&mut ledger, &s);
    for (step, kind) in [
        ("after-record", MockIntentKind::Record),
        ("after-revoke", MockIntentKind::Revoke),
    ] {
        intent(&mut ledger, &registered.subscription_id, kind, step);
        assert_eq!(
            ledger.prepare_mock(
                &s.trusted_context_fixture,
                &s.snapshot,
                operation(&format!("new-{step}")),
                0,
                EXPIRY
            ),
            Err(MockIssue::NeedsInput)
        );
        assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
    }
}

#[test]
fn foreign_context_cannot_replay_successful_registration_or_intent_history() {
    let s = sample();
    let mut ledger = empty();
    let (registration, registered, mut rg) = register(&mut ledger, &s);
    let (record, _, mut ig) = intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Record,
        "record",
    );
    let mut foreign = registration.context.clone();
    foreign.user_id = "other-user".into();
    assert_eq!(
        ledger.commit_mock(&registration.draft_id, &foreign, LATER, &mut rg),
        Err(MockIssue::ResourceUnavailable)
    );
    let mut foreign = record.context.clone();
    foreign.user_id = "other-user".into();
    assert_eq!(
        ledger.commit_intent_mock(&record.draft_id, &foreign, LATER, &mut ig),
        Err(MockIssue::ResourceUnavailable)
    );
    assert!(matches!(
        ledger.intent_result_mock("other-user", "record"),
        Err(MockIssue::ResourceUnavailable)
    ));
    assert!(matches!(
        ledger.events_mock("other-user"),
        Err(MockIssue::ResourceUnavailable)
    ));
    assert!(matches!(
        ledger.intent_events_mock("other-user"),
        Err(MockIssue::ResourceUnavailable)
    ));
    assert_eq!(rg.calls, 1);
    assert_eq!(ig.calls, 1);
}

#[test]
fn competing_intent_drafts_on_new_registration_consume_only_one_confirmation() {
    let s = sample();
    let mut ledger = empty();
    let (_, registered, _) = register(&mut ledger, &s);
    let a = ledger
        .prepare_intent_mock(
            operation("a"),
            &registered.subscription_id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .unwrap();
    let b = ledger
        .prepare_intent_mock(
            operation("b"),
            &registered.subscription_id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .unwrap();
    let mut ag = IntentGate {
        expected: a.clone(),
        calls: 0,
    };
    let mut bg = IntentGate {
        expected: b.clone(),
        calls: 0,
    };
    ledger
        .commit_intent_mock(&a.draft_id, &a.context, LATER, &mut ag)
        .unwrap();
    assert_eq!(
        ledger.commit_intent_mock(&b.draft_id, &b.context, LATER, &mut bg),
        Err(MockIssue::StaleFacts)
    );
    assert_eq!(bg.calls, 0);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn operation_history_is_stable_and_never_claims_merchant_or_audit_execution() {
    let s = sample();
    let mut ledger = empty();
    let (_, registered, _) = register(&mut ledger, &s);
    let (_, recorded, _) = intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Record,
        "record",
    );
    let (_, revoked, _) = intent(
        &mut ledger,
        &registered.subscription_id,
        MockIntentKind::Revoke,
        "revoke",
    );
    let registration = &ledger.events_mock("user_demo").unwrap()[0];
    let events = ledger.intent_events_mock("user_demo").unwrap();
    assert_eq!(registration.subscription_id, registered.subscription_id);
    assert_eq!(registration.occurred_at, NOW);
    assert_eq!(
        events
            .iter()
            .map(|e| e.operation_id.as_str())
            .collect::<Vec<_>>(),
        vec!["record", "revoke"]
    );
    assert!(events
        .iter()
        .all(|e| e.subscription_id == registered.subscription_id && e.occurred_at == LATER));
    assert!(
        !registered.merchant_action_executed
            && !registered.audit_integrated
            && !registered.notification_sent
    );
    for result in [recorded, revoked] {
        assert!(
            !result.merchant_action_executed
                && !result.merchant_cancellation_confirmed
                && !result.merchant_service_restored
        );
        assert!(
            !result.audit_integrated && !result.notification_sent && !result.production_registered
        );
        assert_eq!(
            ledger
                .intent_result_mock("user_demo", &result.operation_id)
                .unwrap(),
            &result
        );
    }
}
