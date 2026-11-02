use safeflow_subscription_evidence::confirmation_mock::*;
use safeflow_subscription_evidence::managed_ledger_mock::{
    Disposition, LedgerEntry, LedgerFixture, LedgerStatus as LedgerState,
};
use safeflow_subscription_evidence::*;

const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));
const NOW: &str = "2026-10-13T12:01:00+08:00";
const EXPIRY: &str = "2026-10-13T12:05:00+08:00";

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
fn complete() -> ReadState {
    ReadState {
        state: ReadKind::Complete,
        is_complete: true,
        next_cursor: None,
        reason: None,
    }
}
fn fixture(entries: Vec<LedgerEntry>) -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("mock-ledger-1".into()),
        read: complete(),
        entries,
    }
}
fn entry(state: LedgerState, service: Option<&str>) -> LedgerEntry {
    LedgerEntry {
        subscription_id: "existing-demo".into(),
        owner_user_id: "user_demo".into(),
        source_id: Some("fictional-billing-input-v1".into()),
        merchant_id: Some("merchant_demo_video".into()),
        service_ref: service.map(str::to_owned),
        status: state,
    }
}
fn operation() -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "session-demo".into(),
        task_id: "task-demo".into(),
        node_id: "confirm-demo".into(),
        operation_id: "op-demo".into(),
        idempotency_key: "key-demo".into(),
        plan_version: 1,
    }
}
fn prepare(ledger: &mut MemoryConfirmationMock) -> MockConfirmationBinding {
    let s = sample();
    ledger
        .prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation(),
            0,
            EXPIRY,
        )
        .unwrap()
}

#[test]
fn registration_preview_reads_exact_evidence_without_registering_or_leaking_binding() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let binding = prepare(&mut ledger);
    let view = ledger
        .registration_preview_mock(&binding.draft_id, &binding.context, NOW)
        .unwrap();
    assert_eq!(view.charges.len(), binding.candidate.actual_charges.len());
    for (shown, actual) in view.charges.iter().zip(&binding.candidate.actual_charges) {
        assert_eq!(shown.amount_minor_units, actual.amount_minor_units);
        assert_eq!(shown.completed_at, actual.completed_at);
    }
    assert!(view.service_display_name.is_none());
    let json = serde_json::to_string(&view).unwrap();
    for field in [
        "session_ref",
        "idempotency_key",
        "merchant_id",
        "source_id",
        "transaction_id",
        "account_id",
    ] {
        assert!(!json.contains(field));
    }
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
    assert!(ledger.events_mock("user_demo").unwrap().is_empty());
    let mut gate = TestGate::confirmed(&binding);
    ledger
        .commit_mock(&binding.draft_id, &binding.context, NOW, &mut gate)
        .unwrap();
    assert_eq!(gate.calls, 1);
    assert!(ledger
        .registration_preview_mock(&binding.draft_id, &binding.context, NOW)
        .is_err());
}
#[test]
fn registration_preview_rejects_expired_superseded_and_foreign_drafts() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let first = prepare(&mut ledger);
    assert!(matches!(
        ledger.registration_preview_mock(&first.draft_id, &first.context, EXPIRY),
        Err(MockIssue::DraftExpired)
    ));
    let mut foreign = first.context.clone();
    foreign.user_id = "other".into();
    assert!(ledger
        .registration_preview_mock(&first.draft_id, &foreign, NOW)
        .is_err());
    let s = sample();
    let mut revised = first.context.clone();
    revised.plan_version += 1;
    let second = ledger
        .prepare_mock(&s.trusted_context_fixture, &s.snapshot, revised, 0, EXPIRY)
        .unwrap();
    assert!(matches!(
        ledger.registration_preview_mock(&first.draft_id, &first.context, NOW),
        Err(MockIssue::PlanConflict)
    ));
    assert!(ledger
        .registration_preview_mock(&second.draft_id, &second.context, NOW)
        .is_ok());
    let mut gate = TestGate::confirmed(&first);
    assert!(ledger
        .commit_mock(&first.draft_id, &first.context, NOW, &mut gate)
        .is_err());
    assert_eq!(gate.calls, 0);
}
struct TestGate {
    expected: MockConfirmationBinding,
    used: bool,
    calls: usize,
    outcome: MockGateOutcome,
}
impl TestGate {
    fn confirmed(binding: &MockConfirmationBinding) -> Self {
        Self {
            expected: binding.clone(),
            used: false,
            calls: 0,
            outcome: MockGateOutcome::Confirmed,
        }
    }
}
impl MockGateVerifier for TestGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
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

#[test]
fn known_same_service_is_filtered_but_same_merchant_other_service_is_not() {
    let s = sample();
    for (service, disposition) in [
        (Some("service_demo_video_1"), Disposition::AlreadyManaged),
        (Some("other-service"), Disposition::NotInLedger),
    ] {
        let ledger = MemoryConfirmationMock::from_fixture(fixture(vec![entry(
            LedgerState::Active,
            service,
        )]));
        let result = ledger
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap();
        assert_eq!(result.groups[0].disposition, disposition);
        assert_eq!(result.groups[0].group.actual_charges.len(), 2);
        assert!(!result.production_registered);
        assert!(!result.team_contract_approved);
    }
}

#[test]
fn cancelling_and_error_entries_keep_evidence_for_review_without_new_registration() {
    let s = sample();
    for (state, disposition) in [
        (LedgerState::Cancelling, Disposition::NeedsReview),
        (LedgerState::Error, Disposition::NeedsReview),
    ] {
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![entry(
            state,
            Some("service_demo_video_1"),
        )]));
        let r = ledger
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap();
        assert_eq!(r.groups[0].disposition, disposition);
        assert_eq!(r.groups[0].group.actual_charges.len(), 2);
        assert!(ledger
            .prepare_mock(
                &s.trusted_context_fixture,
                &s.snapshot,
                operation(),
                0,
                EXPIRY
            )
            .is_err());
        assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
        assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 0);
    }
}

#[test]
fn missing_service_and_duplicate_ledger_matches_never_hide_or_auto_select() {
    let s = sample();
    let mut duplicate = entry(LedgerState::Active, Some("service_demo_video_1"));
    duplicate.subscription_id = "existing-second".into();
    for entries in [
        vec![entry(LedgerState::Active, None)],
        vec![
            entry(LedgerState::Active, Some("service_demo_video_1")),
            duplicate,
        ],
    ] {
        let ledger = MemoryConfirmationMock::from_fixture(fixture(entries));
        let r = ledger
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap();
        assert_eq!(r.groups[0].disposition, Disposition::NeedsReview);
        assert_eq!(r.groups[0].group.actual_charges.len(), 2);
    }
}

#[test]
fn incomplete_or_unknown_ledger_and_foreign_user_cannot_become_empty() {
    let s = sample();
    for state in [ReadKind::Unavailable, ReadKind::Incomplete] {
        let mut source = fixture(vec![]);
        source.read = ReadState {
            state,
            is_complete: false,
            next_cursor: None,
            reason: Some("unavailable".into()),
        };
        let mut ledger = MemoryConfirmationMock::from_fixture(source);
        assert!(matches!(
            ledger.discover_mock(&s.trusted_context_fixture, &s.snapshot),
            Err(MockIssue::LedgerUnavailable)
        ));
        assert!(ledger
            .prepare_mock(
                &s.trusted_context_fixture,
                &s.snapshot,
                operation(),
                0,
                EXPIRY
            )
            .is_err());
    }
    let ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    assert!(matches!(
        ledger.list_mock("foreign"),
        Err(MockIssue::ResourceUnavailable)
    ));
    let mut ctx = s.trusted_context_fixture.clone();
    ctx.user_id = "foreign".into();
    assert!(matches!(
        ledger.discover_mock(&ctx, &s.snapshot),
        Err(MockIssue::ResourceUnavailable)
    ));
}

#[test]
fn failed_consumption_or_unauthorized_accounts_cannot_prepare_a_confirmation() {
    for wrong_account in [false, true] {
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
        let mut s = sample();
        if wrong_account {
            s.snapshot.transactions[0].account_id = "foreign".into();
        } else {
            s.snapshot.transactions_read.state = ReadKind::Incomplete;
            s.snapshot.transactions_read.is_complete = false;
        }
        let r = ledger
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap();
        assert!(r.groups.is_empty());
        assert!(!r.matching_executed);
        assert!(ledger
            .prepare_mock(
                &s.trusted_context_fixture,
                &s.snapshot,
                operation(),
                0,
                EXPIRY
            )
            .is_err());
    }
}

#[test]
fn only_confirmed_mock_binding_registers_once_and_replay_keeps_original_receipt() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let draft = prepare(&mut ledger);
    assert_eq!(draft.minimum_level, "YELLOW");
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
    let mut gate = TestGate::confirmed(&draft);
    let first = ledger
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    let replay = ledger
        .commit_mock(
            &draft.draft_id,
            &draft.context,
            "2027-01-01T00:00:00+08:00",
            &mut gate,
        )
        .unwrap();
    assert_eq!(first, replay);
    assert_eq!(gate.calls, 1);
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 1);
    assert_eq!(
        ledger.events_mock("user_demo").unwrap()[0]
            .evidence
            .actual_charges
            .len(),
        2
    );
    assert!(first.fictional);
    assert!(
        !first.production_registered
            && !first.audit_integrated
            && !first.notification_sent
            && !first.merchant_action_executed
    );
    let s = sample();
    assert_eq!(
        ledger
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap()
            .groups[0]
            .disposition,
        Disposition::AlreadyManaged
    );
}

#[test]
fn missing_confirmation_higher_auth_or_failed_gate_never_writes() {
    for (outcome, expected) in [
        (MockGateOutcome::Denied, MockIssue::ConfirmationRequired),
        (MockGateOutcome::StrongAuthRequired, MockIssue::WaitingAuth),
        (MockGateOutcome::Unavailable, MockIssue::GateUnavailable),
    ] {
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
        let draft = prepare(&mut ledger);
        let mut gate = TestGate::confirmed(&draft);
        gate.outcome = outcome;
        assert_eq!(
            ledger.commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate),
            Err(expected)
        );
        assert!(!gate.used);
        assert!(ledger.list_mock("user_demo").unwrap().is_empty());
        assert!(ledger.events_mock("user_demo").unwrap().is_empty());
    }
}

#[test]
fn edited_binding_session_task_plan_or_confirmation_cannot_be_reused() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let draft = prepare(&mut ledger);
    let mut gate = TestGate::confirmed(&draft);
    let mut changes = vec![];
    let mut ctx = draft.context.clone();
    ctx.session_ref = "other".into();
    changes.push(ctx);
    let mut ctx = draft.context.clone();
    ctx.task_id = "other".into();
    changes.push(ctx);
    let mut ctx = draft.context.clone();
    ctx.plan_version = 2;
    changes.push(ctx);
    let mut ctx = draft.context.clone();
    ctx.operation_id = "other".into();
    changes.push(ctx);
    for ctx in changes {
        assert!(ledger
            .commit_mock(&draft.draft_id, &ctx, NOW, &mut gate)
            .is_err());
    }
    assert_eq!(gate.calls, 0);
    gate.expected.candidate.actual_charges[0].amount_minor_units += 1;
    assert_eq!(
        ledger.commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate),
        Err(MockIssue::ConfirmationRequired)
    );
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
}

#[test]
fn expiry_boundary_and_backwards_time_fail_before_consuming_confirmation() {
    for now in [EXPIRY, "2026-10-13T11:59:59+08:00", "bad-time"] {
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
        let draft = prepare(&mut ledger);
        let mut gate = TestGate::confirmed(&draft);
        assert!(ledger
            .commit_mock(&draft.draft_id, &draft.context, now, &mut gate)
            .is_err());
        assert_eq!(gate.calls, 0);
        assert!(ledger.list_mock("user_demo").unwrap().is_empty());
    }
}

#[test]
fn newer_plan_invalidates_old_draft_and_old_confirmation() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let old = prepare(&mut ledger);
    let mut gate = TestGate::confirmed(&old);
    let s = sample();
    let mut ctx = operation();
    ctx.plan_version = 2;
    let new = ledger
        .prepare_mock(&s.trusted_context_fixture, &s.snapshot, ctx, 0, EXPIRY)
        .unwrap();
    assert_eq!(
        ledger.commit_mock(&old.draft_id, &old.context, NOW, &mut gate),
        Err(MockIssue::PlanConflict)
    );
    assert_eq!(
        ledger.commit_mock(&new.draft_id, &new.context, NOW, &mut gate),
        Err(MockIssue::ConfirmationRequired)
    );
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
}

#[test]
fn intervening_registration_changes_revision_and_rejects_stale_draft() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let first = prepare(&mut ledger);
    let s = sample();
    let mut ctx = operation();
    ctx.operation_id = "other-op".into();
    ctx.idempotency_key = "other-key".into();
    let second = ledger
        .prepare_mock(&s.trusted_context_fixture, &s.snapshot, ctx, 0, EXPIRY)
        .unwrap();
    ledger
        .commit_mock(
            &first.draft_id,
            &first.context,
            NOW,
            &mut TestGate::confirmed(&first),
        )
        .unwrap();
    let mut gate = TestGate::confirmed(&second);
    assert_eq!(
        ledger.commit_mock(&second.draft_id, &second.context, NOW, &mut gate),
        Err(MockIssue::StaleFacts)
    );
    assert_eq!(gate.calls, 0);
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn idempotency_conflict_cannot_reuse_receipt_for_changed_parameters() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let draft = prepare(&mut ledger);
    let mut gate = TestGate::confirmed(&draft);
    ledger
        .commit_mock(&draft.draft_id, &draft.context, NOW, &mut gate)
        .unwrap();
    let mut changed = draft.context.clone();
    changed.operation_id = "different".into();
    assert_eq!(
        ledger.commit_mock(&draft.draft_id, &changed, NOW, &mut gate),
        Err(MockIssue::IdempotencyConflict)
    );
    changed.user_id = "foreign".into();
    assert_eq!(
        ledger.commit_mock(&draft.draft_id, &changed, NOW, &mut gate),
        Err(MockIssue::ResourceUnavailable)
    );
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn contradictory_ledger_rows_are_rejected() {
    let one = entry(LedgerState::Active, Some("service_demo_video_1"));
    let ledger = MemoryConfirmationMock::from_fixture(fixture(vec![one.clone(), one]));
    assert!(ledger.list_mock("user_demo").is_err());
    let mut foreign = entry(LedgerState::Active, None);
    foreign.owner_user_id = "foreign".into();
    let ledger = MemoryConfirmationMock::from_fixture(fixture(vec![foreign]));
    assert!(matches!(
        ledger.list_mock("user_demo"),
        Err(MockIssue::ResourceUnavailable)
    ));
}

#[test]
fn known_registration_does_not_hide_multiple_charge_review_warning() {
    let mut s = sample();
    let mut duplicate = s.snapshot.transactions[0].clone();
    duplicate.transaction_id = "second-charge".into();
    s.snapshot.transactions.push(duplicate);
    let ledger = MemoryConfirmationMock::from_fixture(fixture(vec![entry(
        LedgerState::Active,
        Some("service_demo_video_1"),
    )]));
    let r = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(r.groups[0].disposition, Disposition::NeedsReview);
    assert_eq!(r.groups[0].group.actual_charges.len(), 3);
    assert!(public_review_message(r.groups[0].reason).contains("多笔扣款"));
}

#[test]
fn repeating_prepare_keeps_draft_but_changed_parameters_require_new_version() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let first = prepare(&mut ledger);
    assert_eq!(first, prepare(&mut ledger));
    let mut s = sample();
    s.snapshot.transactions[0].amount = "29.90".into();
    assert_eq!(
        ledger.prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation(),
            0,
            EXPIRY
        ),
        Err(MockIssue::PlanConflict)
    );
    let mut next = operation();
    next.plan_version = 2;
    let draft = ledger
        .prepare_mock(&s.trusted_context_fixture, &s.snapshot, next, 0, EXPIRY)
        .unwrap();
    let mut old_confirmation = TestGate::confirmed(&first);
    assert_eq!(
        ledger.commit_mock(&draft.draft_id, &draft.context, NOW, &mut old_confirmation),
        Err(MockIssue::ConfirmationRequired)
    );
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
}

#[test]
fn error_record_for_one_service_does_not_stop_another_candidate() {
    let mut s = sample();
    let mut extra = vec![];
    for row in &s.snapshot.transactions {
        if row.transaction_type == TransactionType::Consumption
            && row.status == TransactionStatus::Completed
        {
            let mut other = row.clone();
            other.transaction_id.push_str("-other-service");
            other.mock_service_ref = Some("second-service".into());
            extra.push(other);
        }
    }
    s.snapshot.transactions.extend(extra);
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![entry(
        LedgerState::Error,
        Some("service_demo_video_1"),
    )]));
    let discovery = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert!(discovery
        .groups
        .iter()
        .any(|g| g.reason == "REGISTERED_ERROR_NEEDS_REVIEW"));
    let index = discovery
        .groups
        .iter()
        .position(|g| g.disposition == Disposition::NotInLedger)
        .unwrap();
    assert!(ledger
        .prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation(),
            index,
            EXPIRY
        )
        .is_ok());
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 1);
}

#[test]
fn cross_source_group_cannot_be_registered_without_a_confirmed_service_mapping() {
    let mut s = sample();
    s.snapshot
        .transactions
        .iter_mut()
        .find(|t| t.transaction_id == "fictional-consumption-august")
        .unwrap()
        .source_id = Some("fictional-other-source".into());
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let discovery = ledger
        .discover_mock(&s.trusted_context_fixture, &s.snapshot)
        .unwrap();
    assert_eq!(discovery.groups[0].disposition, Disposition::NeedsReview);
    assert_eq!(discovery.groups[0].group.actual_charges.len(), 2);
    assert_eq!(
        ledger.prepare_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation(),
            0,
            EXPIRY
        ),
        Err(MockIssue::NeedsInput)
    );
}

fn sample_for_another_service() -> Sample {
    let mut s = sample();
    for transaction in &mut s.snapshot.transactions {
        if transaction.mock_service_ref.as_deref() == Some("service_demo_video_1") {
            transaction.mock_service_ref = Some("fictional-service-B".into());
        }
    }
    s
}
#[test]
fn changing_service_before_save_requires_fresh_confirmation_and_only_saves_new_service() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let original = prepare(&mut ledger);
    let sample = sample_for_another_service();
    let mut changed = operation();
    changed.plan_version = 2;
    let replacement = ledger
        .prepare_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            changed,
            0,
            EXPIRY,
        )
        .unwrap();
    assert_ne!(
        original.candidate.mock_service_ref,
        replacement.candidate.mock_service_ref
    );
    let mut old_gate = TestGate::confirmed(&original);
    assert_eq!(
        ledger.commit_mock(&original.draft_id, &original.context, NOW, &mut old_gate),
        Err(MockIssue::PlanConflict)
    );
    assert_eq!(old_gate.calls, 0);
    assert_eq!(
        ledger.commit_mock(
            &replacement.draft_id,
            &replacement.context,
            NOW,
            &mut old_gate
        ),
        Err(MockIssue::ConfirmationRequired)
    );
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
    ledger
        .commit_mock(
            &replacement.draft_id,
            &replacement.context,
            NOW,
            &mut TestGate::confirmed(&replacement),
        )
        .unwrap();
    let saved = ledger.list_mock("user_demo").unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].service_ref.as_deref(), Some("fictional-service-B"));
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 1);
}
#[test]
fn changing_service_after_save_preserves_original_and_requires_new_operation() {
    let mut ledger = MemoryConfirmationMock::from_fixture(fixture(vec![]));
    let original = prepare(&mut ledger);
    let receipt = ledger
        .commit_mock(
            &original.draft_id,
            &original.context,
            NOW,
            &mut TestGate::confirmed(&original),
        )
        .unwrap();
    let original_entry = ledger.list_mock("user_demo").unwrap()[0].clone();
    let sample = sample_for_another_service();
    let mut changed = operation();
    changed.plan_version = 2;
    assert_eq!(
        ledger.prepare_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            changed.clone(),
            0,
            EXPIRY
        ),
        Err(MockIssue::IdempotencyConflict)
    );
    changed.operation_id = "op-service-B".into();
    changed.idempotency_key = "key-service-B".into();
    let second = ledger
        .prepare_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            changed,
            0,
            EXPIRY,
        )
        .unwrap();
    assert_eq!(
        ledger.list_mock("user_demo").unwrap(),
        std::slice::from_ref(&original_entry)
    );
    ledger
        .commit_mock(
            &second.draft_id,
            &second.context,
            NOW,
            &mut TestGate::confirmed(&second),
        )
        .unwrap();
    assert_eq!(ledger.list_mock("user_demo").unwrap().len(), 2);
    assert_eq!(ledger.list_mock("user_demo").unwrap()[0], original_entry);
    assert_eq!(
        ledger
            .confirmation_result_mock("user_demo", &original.context.operation_id)
            .unwrap(),
        &receipt
    );
    assert_eq!(ledger.events_mock("user_demo").unwrap().len(), 2);
}
