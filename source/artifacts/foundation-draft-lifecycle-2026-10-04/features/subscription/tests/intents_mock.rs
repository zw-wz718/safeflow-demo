use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::*, *};

const NOW: &str = "2026-10-14T12:00:00+08:00";
const COMMIT: &str = "2026-10-14T12:01:00+08:00";
const EXPIRY: &str = "2026-10-14T12:05:00+08:00";

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
fn context(operation: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "session-demo".into(),
        task_id: "task-demo".into(),
        node_id: "intent-node".into(),
        operation_id: operation.into(),
        idempotency_key: format!("key-{operation}"),
        plan_version: 1,
    }
}
fn prepared(
    ledger: &mut MemoryConfirmationMock,
    kind: MockIntentKind,
    operation: &str,
) -> MockIntentBinding {
    ledger
        .prepare_intent_mock(context(operation), "subscription-A", kind, NOW, EXPIRY)
        .unwrap()
}

#[test]
fn intent_previews_show_local_effect_keep_history_and_do_not_execute() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    for (kind, id, before, after) in [
        (
            MockIntentKind::Record,
            "preview-cancel",
            LedgerStatus::Active,
            LedgerStatus::Cancelling,
        ),
        (
            MockIntentKind::Revoke,
            "preview-revoke",
            LedgerStatus::Cancelling,
            LedgerStatus::Active,
        ),
    ] {
        let binding = prepared(&mut ledger, kind, id);
        let count = ledger.intent_events_mock("user_demo").unwrap().len();
        let view = ledger
            .intent_preview_mock(&binding.draft_id, &binding.context, NOW)
            .unwrap();
        assert_eq!(view.before, before);
        assert_eq!(view.after, after);
        assert_eq!(ledger.list_mock("user_demo").unwrap()[0].status, before);
        assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), count);
        assert!(view.message.contains("本地") || view.message.contains("取消意图"));
        assert!(ledger
            .intent_preview_mock(&binding.draft_id, &binding.context, EXPIRY)
            .is_err());
        let mut gate = Gate::confirmed(&binding);
        ledger
            .commit_intent_mock(&binding.draft_id, &binding.context, COMMIT, &mut gate)
            .unwrap();
        assert_eq!(gate.calls, 1);
        assert!(ledger
            .intent_preview_mock(&binding.draft_id, &binding.context, COMMIT)
            .is_err());
    }
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 2);
}
struct Gate {
    expected: MockIntentBinding,
    outcome: MockGateOutcome,
    used: bool,
    calls: usize,
}
impl Gate {
    fn confirmed(binding: &MockIntentBinding) -> Self {
        Self {
            expected: binding.clone(),
            outcome: MockGateOutcome::Confirmed,
            used: false,
            calls: 0,
        }
    }
}
impl MockIntentGateVerifier for Gate {
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
        _now: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if binding != &self.expected || self.used {
            return MockGateOutcome::Denied;
        }
        if self.outcome == MockGateOutcome::Confirmed {
            self.used = true;
        }
        self.outcome
    }
}
struct RegistrationGate(MockConfirmationBinding);
impl MockGateVerifier for RegistrationGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
        _now: &str,
    ) -> MockGateOutcome {
        if binding == &self.0 {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}
fn consumption() -> Sample {
    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
    ));
    billing_query_mock::load_case(
        raw,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap()
}
fn with_other_service() -> MemoryConfirmationMock {
    let mut f = fixture(LedgerStatus::Active);
    f.entries[0].service_ref = Some("different-existing-service".into());
    MemoryConfirmationMock::from_fixture(f)
}

#[test]
fn recording_requires_confirmation_then_preserves_record_and_metadata() {
    let f = fixture(LedgerStatus::Active);
    let original = f.entries[0].clone();
    let mut ledger = MemoryConfirmationMock::from_fixture(f);
    let draft = prepared(&mut ledger, MockIntentKind::Record, "record");
    assert_eq!(draft.minimum_level, "YELLOW");
    assert_eq!(ledger.list_mock("user_demo").unwrap()[0], original);
    let mut gate = Gate::confirmed(&draft);
    gate.outcome = MockGateOutcome::Denied;
    assert_eq!(
        ledger.commit_intent_mock(&draft.draft_id, &draft.context, COMMIT, &mut gate),
        Err(MockIssue::ConfirmationRequired)
    );
    assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
    gate.outcome = MockGateOutcome::Confirmed;
    let receipt = ledger
        .commit_intent_mock(&draft.draft_id, &draft.context, COMMIT, &mut gate)
        .unwrap();
    let mut expected = original;
    expected.status = LedgerStatus::Cancelling;
    assert_eq!(ledger.list_mock("user_demo").unwrap(), &[expected]);
    assert_eq!(receipt.local_intent_state, "RECORDED");
    assert!(receipt.message.contains("商户官方渠道"));
    assert!(!receipt.merchant_action_executed && !receipt.merchant_cancellation_confirmed);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn revoke_only_changes_local_state_and_never_claims_service_restoration() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Cancelling));
    let draft = prepared(&mut ledger, MockIntentKind::Revoke, "revoke");
    let receipt = ledger
        .commit_intent_mock(
            &draft.draft_id,
            &draft.context,
            COMMIT,
            &mut Gate::confirmed(&draft),
        )
        .unwrap();
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Active
    );
    assert_eq!(receipt.local_intent_state, "REVOKED");
    assert!(receipt.message.contains("无法替您恢复商户服务"));
    assert!(
        !receipt.merchant_service_restored
            && !receipt.notification_sent
            && !receipt.audit_integrated
    );
    let event = &ledger.intent_events_mock("user_demo").unwrap()[0];
    assert_eq!(
        (event.previous_status, event.next_status),
        (LedgerStatus::Cancelling, LedgerStatus::Active)
    );
}

#[test]
fn record_revoke_and_original_record_replay_preserve_two_events_and_history() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    let record = prepared(&mut ledger, MockIntentKind::Record, "record");
    let mut gate = Gate::confirmed(&record);
    let first = ledger
        .commit_intent_mock(&record.draft_id, &record.context, COMMIT, &mut gate)
        .unwrap();
    let revoke = prepared(&mut ledger, MockIntentKind::Revoke, "revoke");
    ledger
        .commit_intent_mock(
            &revoke.draft_id,
            &revoke.context,
            COMMIT,
            &mut Gate::confirmed(&revoke),
        )
        .unwrap();
    assert_eq!(
        ledger
            .commit_intent_mock(
                &record.draft_id,
                &record.context,
                "2027-01-01T00:00:00+08:00",
                &mut gate
            )
            .unwrap(),
        first
    );
    assert_eq!(
        ledger.intent_result_mock("user_demo", "record").unwrap(),
        &first
    );
    assert_eq!(first.ledger_state_at_operation, "CANCELLING");
    assert_eq!(
        ledger.list_mock("user_demo").unwrap()[0].status,
        LedgerStatus::Active
    );
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 2);
    assert_eq!(gate.calls, 1);
}

#[test]
fn a_new_record_after_revoke_appends_history_without_creating_a_second_subscription() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    for (kind, op) in [
        (MockIntentKind::Record, "r1"),
        (MockIntentKind::Revoke, "v1"),
        (MockIntentKind::Record, "r2"),
    ] {
        let draft = prepared(&mut ledger, kind, op);
        ledger
            .commit_intent_mock(
                &draft.draft_id,
                &draft.context,
                COMMIT,
                &mut Gate::confirmed(&draft),
            )
            .unwrap();
    }
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 3);
    assert_eq!(
        ledger
            .intent_result_mock("user_demo", "r1")
            .unwrap()
            .local_intent_state,
        "RECORDED"
    );
    assert_eq!(
        ledger
            .intent_result_mock("user_demo", "v1")
            .unwrap()
            .local_intent_state,
        "REVOKED"
    );
}

#[test]
fn invalid_state_transitions_fail_without_issuing_a_draft() {
    for (status, kind) in [
        (LedgerStatus::Active, MockIntentKind::Revoke),
        (LedgerStatus::Cancelling, MockIntentKind::Record),
        (LedgerStatus::Error, MockIntentKind::Record),
        (LedgerStatus::Error, MockIntentKind::Revoke),
    ] {
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture(status));
        assert_eq!(
            ledger.prepare_intent_mock(context("x"), "subscription-A", kind, NOW, EXPIRY),
            Err(MockIssue::StateConflict)
        );
        assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
    }
}

#[test]
fn missing_foreign_and_mixed_owner_data_never_leak_operation_results() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    assert_eq!(
        ledger.prepare_intent_mock(context("x"), "missing", MockIntentKind::Record, NOW, EXPIRY),
        Err(MockIssue::ResourceUnavailable)
    );
    let mut foreign = context("x");
    foreign.user_id = "foreign".into();
    assert_eq!(
        ledger.prepare_intent_mock(
            foreign,
            "subscription-A",
            MockIntentKind::Record,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::ResourceUnavailable)
    );
    assert!(matches!(
        ledger.intent_result_mock("foreign", "x"),
        Err(MockIssue::ResourceUnavailable)
    ));
    let mut f = fixture(LedgerStatus::Active);
    let mut row = f.entries[0].clone();
    row.subscription_id = "foreign-secret".into();
    row.owner_user_id = "foreign".into();
    f.entries.push(row);
    let mut ledger = MemoryConfirmationMock::from_fixture(f);
    assert_eq!(
        ledger.prepare_intent_mock(
            context("x"),
            "subscription-A",
            MockIntentKind::Record,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::ResourceUnavailable)
    );
}

#[test]
fn unavailable_incomplete_and_contradictory_ledger_cannot_write_intents() {
    for state in [ReadKind::Unavailable, ReadKind::Incomplete] {
        let mut f = fixture(LedgerStatus::Active);
        f.read.state = state;
        f.read.is_complete = false;
        let mut ledger = MemoryConfirmationMock::from_fixture(f);
        assert_eq!(
            ledger.prepare_intent_mock(
                context("x"),
                "subscription-A",
                MockIntentKind::Record,
                NOW,
                EXPIRY
            ),
            Err(MockIssue::LedgerUnavailable)
        );
    }
    let mut f = fixture(LedgerStatus::Active);
    f.read.next_cursor = Some("next".into());
    assert_eq!(
        MemoryConfirmationMock::from_fixture(f).prepare_intent_mock(
            context("x"),
            "subscription-A",
            MockIntentKind::Record,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::LedgerUnavailable)
    );
}

#[test]
fn repeated_prepare_is_stable_and_target_changes_need_new_version_and_confirmation() {
    let mut f = fixture(LedgerStatus::Active);
    let mut other = f.entries[0].clone();
    other.subscription_id = "subscription-B".into();
    f.entries.push(other);
    let mut ledger = MemoryConfirmationMock::from_fixture(f);
    let first = prepared(&mut ledger, MockIntentKind::Record, "x");
    assert_eq!(prepared(&mut ledger, MockIntentKind::Record, "x"), first);
    assert_eq!(
        ledger.prepare_intent_mock(
            context("x"),
            "subscription-B",
            MockIntentKind::Record,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::PlanConflict)
    );
    let mut ctx = context("x");
    ctx.plan_version = 2;
    let next = ledger
        .prepare_intent_mock(ctx, "subscription-B", MockIntentKind::Record, NOW, EXPIRY)
        .unwrap();
    let mut old_gate = Gate::confirmed(&first);
    assert_eq!(
        ledger.commit_intent_mock(&first.draft_id, &first.context, COMMIT, &mut old_gate),
        Err(MockIssue::PlanConflict)
    );
    assert_eq!(
        ledger.commit_intent_mock(&next.draft_id, &next.context, COMMIT, &mut old_gate),
        Err(MockIssue::ConfirmationRequired)
    );
    assert!(ledger
        .list_mock("user_demo")
        .unwrap()
        .iter()
        .all(|e| e.status == LedgerStatus::Active));
}

#[test]
fn expiry_equality_backwards_time_and_bad_time_stop_before_gate_consumption() {
    for now in [EXPIRY, "2026-10-14T11:59:59+08:00", "bad-time"] {
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
        let draft = prepared(&mut ledger, MockIntentKind::Record, "x");
        let mut gate = Gate::confirmed(&draft);
        assert!(ledger
            .commit_intent_mock(&draft.draft_id, &draft.context, now, &mut gate)
            .is_err());
        assert_eq!(gate.calls, 0);
        assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
    }
}

#[test]
fn changed_session_task_node_operation_key_or_version_cannot_reuse_confirmation() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    let draft = prepared(&mut ledger, MockIntentKind::Record, "x");
    let mut gate = Gate::confirmed(&draft);
    let mut variants = vec![];
    for field in 0..6 {
        let mut ctx = draft.context.clone();
        match field {
            0 => ctx.session_ref = "other".into(),
            1 => ctx.task_id = "other".into(),
            2 => ctx.node_id = "other".into(),
            3 => ctx.operation_id = "other".into(),
            4 => ctx.idempotency_key = "other".into(),
            _ => ctx.plan_version = 2,
        };
        variants.push(ctx);
    }
    for ctx in variants {
        assert!(ledger
            .commit_intent_mock(&draft.draft_id, &ctx, COMMIT, &mut gate)
            .is_err());
    }
    assert_eq!(gate.calls, 0);
    assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
}

#[test]
fn denied_stronger_auth_and_unavailable_gate_leave_both_intent_kinds_unchanged() {
    for kind in [MockIntentKind::Record, MockIntentKind::Revoke] {
        for (outcome, error) in [
            (MockGateOutcome::Denied, MockIssue::ConfirmationRequired),
            (MockGateOutcome::StrongAuthRequired, MockIssue::WaitingAuth),
            (MockGateOutcome::Unavailable, MockIssue::GateUnavailable),
        ] {
            let initial = if kind == MockIntentKind::Record {
                LedgerStatus::Active
            } else {
                LedgerStatus::Cancelling
            };
            let mut ledger = MemoryConfirmationMock::from_fixture(fixture(initial));
            let draft = prepared(&mut ledger, kind, "x");
            let mut gate = Gate::confirmed(&draft);
            gate.outcome = outcome;
            assert_eq!(
                ledger.commit_intent_mock(&draft.draft_id, &draft.context, COMMIT, &mut gate),
                Err(error)
            );
            assert!(!gate.used);
            assert_eq!(ledger.list_mock("user_demo").unwrap()[0].status, initial);
            assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
        }
    }
}

#[test]
fn another_intent_changes_revision_and_stops_a_previously_prepared_intent() {
    let mut f = fixture(LedgerStatus::Active);
    let mut b = f.entries[0].clone();
    b.subscription_id = "subscription-B".into();
    f.entries.push(b);
    let mut ledger = MemoryConfirmationMock::from_fixture(f);
    let a = prepared(&mut ledger, MockIntentKind::Record, "a");
    let b = ledger
        .prepare_intent_mock(
            context("b"),
            "subscription-B",
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .unwrap();
    ledger
        .commit_intent_mock(&b.draft_id, &b.context, COMMIT, &mut Gate::confirmed(&b))
        .unwrap();
    let mut gate = Gate::confirmed(&a);
    assert_eq!(
        ledger.commit_intent_mock(&a.draft_id, &a.context, COMMIT, &mut gate),
        Err(MockIssue::StaleFacts)
    );
    assert_eq!(gate.calls, 0);
}

#[test]
fn completed_operation_id_or_key_cannot_be_relabelled_as_a_new_intent() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    let draft = prepared(&mut ledger, MockIntentKind::Record, "x");
    ledger
        .commit_intent_mock(
            &draft.draft_id,
            &draft.context,
            COMMIT,
            &mut Gate::confirmed(&draft),
        )
        .unwrap();
    let mut changed = context("y");
    changed.idempotency_key = draft.context.idempotency_key.clone();
    assert_eq!(
        ledger.prepare_intent_mock(
            changed,
            "subscription-A",
            MockIntentKind::Revoke,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::IdempotencyConflict)
    );
    let mut changed = context("x");
    changed.idempotency_key = "different-key".into();
    assert_eq!(
        ledger.prepare_intent_mock(
            changed,
            "subscription-A",
            MockIntentKind::Revoke,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::IdempotencyConflict)
    );
    assert_eq!(ledger.intent_events_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn an_intent_key_cannot_be_reused_by_confirmation_registration() {
    let mut ledger = with_other_service();
    let intent = prepared(&mut ledger, MockIntentKind::Record, "intent");
    let s = consumption();
    let mut reused = context("registration");
    reused.idempotency_key = intent.context.idempotency_key.clone();
    assert_eq!(
        ledger.prepare_mock(&s.trusted_context_fixture, &s.snapshot, reused, 0, EXPIRY),
        Err(MockIssue::IdempotencyConflict)
    );
}

#[test]
fn a_registration_key_cannot_be_reused_by_an_intent() {
    let mut ledger = with_other_service();
    let s = consumption();
    let registration = ledger
        .prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            context("registration"),
            0,
            EXPIRY,
        )
        .unwrap();
    let mut reused = context("intent");
    reused.idempotency_key = registration.context.idempotency_key.clone();
    assert_eq!(
        ledger.prepare_intent_mock(
            reused,
            "subscription-A",
            MockIntentKind::Record,
            NOW,
            EXPIRY
        ),
        Err(MockIssue::IdempotencyConflict)
    );
}

#[test]
fn registration_and_intent_share_revision_for_final_checks() {
    let s = consumption();
    for commit_registration_first in [false, true] {
        let mut ledger = with_other_service();
        let registration = ledger
            .prepare_mock(
                &s.trusted_context_fixture,
                &s.snapshot,
                context("registration"),
                0,
                EXPIRY,
            )
            .unwrap();
        let intent = prepared(&mut ledger, MockIntentKind::Record, "intent");
        if commit_registration_first {
            ledger
                .commit_mock(
                    &registration.draft_id,
                    &registration.context,
                    COMMIT,
                    &mut RegistrationGate(registration.clone()),
                )
                .unwrap();
            let mut gate = Gate::confirmed(&intent);
            assert_eq!(
                ledger.commit_intent_mock(&intent.draft_id, &intent.context, COMMIT, &mut gate),
                Err(MockIssue::StaleFacts)
            );
            assert_eq!(gate.calls, 0);
        } else {
            ledger
                .commit_intent_mock(
                    &intent.draft_id,
                    &intent.context,
                    COMMIT,
                    &mut Gate::confirmed(&intent),
                )
                .unwrap();
            assert_eq!(
                ledger.commit_mock(
                    &registration.draft_id,
                    &registration.context,
                    COMMIT,
                    &mut RegistrationGate(registration.clone())
                ),
                Err(MockIssue::StaleFacts)
            );
        }
    }
}

#[test]
fn recorded_intent_appears_as_review_and_revoked_intent_returns_to_managed() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(LedgerStatus::Active));
    let s = consumption();
    let record = prepared(&mut ledger, MockIntentKind::Record, "record");
    ledger
        .commit_intent_mock(
            &record.draft_id,
            &record.context,
            COMMIT,
            &mut Gate::confirmed(&record),
        )
        .unwrap();
    let result = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(result.groups[0].reason, "CANCELLATION_INTENT_NEEDS_REVIEW");
    assert_eq!(result.groups[0].group.actual_charges.len(), 2);
    let revoke = prepared(&mut ledger, MockIntentKind::Revoke, "revoke");
    ledger
        .commit_intent_mock(
            &revoke.draft_id,
            &revoke.context,
            COMMIT,
            &mut Gate::confirmed(&revoke),
        )
        .unwrap();
    assert_eq!(
        ledger
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap()
            .groups[0]
            .disposition,
        Disposition::AlreadyManaged
    );
}
