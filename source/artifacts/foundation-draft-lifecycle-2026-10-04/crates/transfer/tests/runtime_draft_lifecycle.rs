#[path = "../examples/support/mock_draft_write.rs"]
mod support;

use futures::{channel::oneshot, executor::block_on, future::FutureExt};
use safeflow_bank_gateway::{BankError, BankGateway, FindPayeesOutput, GetAccountBalanceOutput};
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::*;
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::{runtime_tools::*, DraftStatus, StoredDraft};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use support::{assemble, context, App, DemoClock};

fn create_input() -> Value {
    json!({"payer_account_id":"account_001", "payee_id":"payee_008", "amount":"200.00", "currency":"CNY", "remark":"原备注"})
}
fn setup(bank: Arc<dyn BankGateway>, clock: ManualClock) -> (App, StoredDraft) {
    let app = assemble(bank, DemoClock(clock)).unwrap();
    // Inspect the shared session binding as part of the trusted assembly.
    assert_eq!(
        app.sessions
            .resolve_binding(&SessionRef::parse("mock_session_001").unwrap())
            .unwrap()
            .binding_version,
        1
    );
    block_on(app.store.find_candidates(&context("lifecycle"), "小李")).unwrap();
    let (request, permit) = authorized(&app, MockDraftAction::Create, 1, create_input());
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    let saved = block_on(app.store.read_created_for_task(&context("lifecycle")))
        .unwrap()
        .unwrap();
    (app, saved)
}
fn ordinary() -> (App, StoredDraft) {
    setup(Arc::new(MockBankGateway::new()), ManualClock::new(1000))
}
fn make_request(
    app: &App,
    action: MockDraftAction,
    version: u32,
    arguments: Value,
) -> MockDraftWriteRequest {
    app.authority
        .request_for_action(
            action,
            SessionRef::parse("mock_session_001").unwrap(),
            TaskId::parse("lifecycle").unwrap(),
            NodeId::parse(format!("action_{version}")).unwrap(),
            version,
            arguments,
            60,
        )
        .unwrap()
}
fn authorized(
    app: &App,
    action: MockDraftAction,
    version: u32,
    arguments: Value,
) -> (MockDraftWriteRequest, MockDraftPermit) {
    let request = make_request(app, action, version, arguments);
    let confirmation = app
        .authority
        .record_explicit_confirmation(&request)
        .unwrap();
    let permit = app.authority.issue(&confirmation).unwrap();
    (request, permit)
}
fn modify_input(draft: &StoredDraft, changes: Value) -> Value {
    json!({"draft_id":draft.draft_id, "expected_version":draft.plan_version, "changes":changes})
}
fn cancel_input(draft: &StoredDraft) -> Value {
    json!({"draft_id":draft.draft_id, "expected_version":draft.plan_version})
}
fn read(app: &App, saved: &StoredDraft) -> StoredDraft {
    block_on(app.store.read(&context("lifecycle"), &saved.draft_id)).unwrap()
}
fn body(outcome: &RuntimeOutcome) -> &Value {
    let RuntimeOutcome::Completed { results, .. } = outcome else {
        panic!("must complete")
    };
    &results[0].data
}

#[test]
fn runtime_create_modify_cancel_readback_and_retries_keep_ids_ttl_and_distinct_versions() {
    let (app, initial) = ordinary();
    let (modify, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&initial, json!({"amount":"300.00", "remark":""})),
    );
    let outcome = block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(modify.clone(), &permit),
    )
    .unwrap();
    assert_eq!(
        block_on(app.assembly.runtime().execute_mock_draft(modify, &permit)).unwrap(),
        outcome
    );
    let updated = read(&app, &initial);
    assert_eq!(updated.draft_id, initial.draft_id);
    assert_eq!(updated.operation_id, initial.operation_id);
    assert_eq!(updated.expires_at, initial.expires_at);
    assert_eq!(updated.plan_version, 2);
    assert_eq!(updated.details.as_ref().unwrap().amount.as_str(), "300.00");
    assert_eq!(updated.details.as_ref().unwrap().remark, "");
    assert_eq!(body(&outcome)["action"], MOCK_DRAFT_MODIFY_ACTION);
    assert_eq!(body(&outcome)["expected_version"], 1);
    assert_eq!(body(&outcome)["runtime_plan_version"], 2);
    let (cancel, permit) = authorized(&app, MockDraftAction::Cancel, 3, cancel_input(&updated));
    let outcome = block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(cancel.clone(), &permit),
    )
    .unwrap();
    assert_eq!(
        block_on(app.assembly.runtime().execute_mock_draft(cancel, &permit)).unwrap(),
        outcome
    );
    assert_eq!(body(&outcome)["action"], MOCK_DRAFT_CANCEL_ACTION);
    assert_eq!(body(&outcome)["plan_version"], 2);
    assert_eq!(body(&outcome)["runtime_plan_version"], 3);
    assert_eq!(body(&outcome)["status"], "CANCELLED");
    let cancelled = read(&app, &initial);
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    assert_eq!(cancelled.expires_at, initial.expires_at);
    assert_eq!(cancelled.plan_version, 2);
    assert_eq!(app.store.saved_receipt_count().unwrap(), 3);
    let events = app.audit.events().unwrap();
    for kind in [
        AuditEventKind::ToolStarted,
        AuditEventKind::ToolCompleted,
        AuditEventKind::TaskCompleted,
    ] {
        assert_eq!(events.iter().filter(|event| event.kind == kind).count(), 3);
    }
}

#[test]
fn strict_modify_and_cancel_inputs_reject_nulls_unknown_fields_empty_changes_and_illegal_values() {
    let bad_changes = vec![
        json!({}),
        Value::Null,
        json!({"amount":null}),
        json!({"remark":null}),
        json!({"currency":null}),
        json!({"payer_account_id":null}),
        json!({"payee_id":null}),
        json!({"amount":300.0}),
        json!({"amount":"300"}),
        json!({"amount":"01.00"}),
        json!({"amount":"-1.00"}),
        json!({"amount":"0.00"}),
        json!({"amount":"184467440737095516.16"}),
        json!({"currency":"USD"}),
        json!({"remark":"x".repeat(141)}),
        json!({"remark":"line\nline"}),
        json!({"payee_id":""}),
        json!({"payer_account_id":" account_001"}),
        json!({"payee_id":"x".repeat(129)}),
        json!({"payee_id":"bad/id"}),
        json!({"confirmed":true}),
        json!({"payee_query":"小李"}),
    ];
    for changes in bad_changes {
        let (app, saved) = ordinary();
        let (request, permit) = authorized(
            &app,
            MockDraftAction::Modify,
            2,
            modify_input(&saved, changes),
        );
        assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        assert_eq!(read(&app, &saved), saved);
    }
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        for (field, value) in [
            ("draft_id", Value::Null),
            ("draft_id", json!("")),
            ("expected_version", Value::Null),
            ("expected_version", json!(0)),
            ("expected_version", json!(1.0)),
            ("expected_version", json!(4294967296u64)),
            ("user_id", json!("user_001")),
            ("permit", json!("permit")),
        ] {
            let (app, saved) = ordinary();
            let mut arguments = if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"remark":"ok"}))
            } else {
                cancel_input(&saved)
            };
            arguments[field] = value;
            let (request, permit) = authorized(&app, action, 2, arguments);
            assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
            assert_eq!(read(&app, &saved), saved);
        }
        for field in ["draft_id", "expected_version", "changes"] {
            if action == MockDraftAction::Cancel && field == "changes" {
                continue;
            }
            let (app, saved) = ordinary();
            let mut arguments = if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"remark":"ok"}))
            } else {
                cancel_input(&saved)
            };
            arguments.as_object_mut().unwrap().remove(field);
            let (request, permit) = authorized(&app, action, 2, arguments);
            assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
            assert_eq!(read(&app, &saved), saved);
        }
    }
}

#[test]
fn omitted_changes_retain_values_and_explicit_same_value_edit_increments_once() {
    let (app, saved) = ordinary();
    let (request, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"currency":"CNY"})),
    );
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    let updated = read(&app, &saved);
    assert_eq!(updated.details, saved.details);
    assert_eq!(updated.expires_at, saved.expires_at);
    assert_eq!(updated.plan_version, saved.plan_version + 1);
}

#[test]
fn stale_versions_expiry_cancelled_and_cross_user_or_task_never_modify_or_cancel() {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        for variant in ["version", "draft", "user", "task", "expired", "cancelled"] {
            let clock = ManualClock::new(1000);
            let (app, saved) = setup(Arc::new(MockBankGateway::new()), clock.clone());
            if variant == "cancelled" {
                let (request, permit) =
                    authorized(&app, MockDraftAction::Cancel, 2, cancel_input(&saved));
                block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
            }
            if variant == "expired" {
                clock.set(saved.expires_at);
            }
            let before = read(&app, &saved);
            let mut arguments = if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"amount":"300.00"}))
            } else {
                cancel_input(&saved)
            };
            if variant == "version" {
                arguments["expected_version"] = json!(2);
            }
            if variant == "draft" {
                arguments["draft_id"] = json!("draft_missing");
            }
            let mut request = make_request(&app, action, 3, arguments);
            if variant == "user" {
                request.session_ref = SessionRef::parse("mock_session_002").unwrap();
            }
            if variant == "task" {
                request.task_id = TaskId::parse("other_task").unwrap();
            }
            let confirmation = app
                .authority
                .record_explicit_confirmation(&request)
                .unwrap();
            let permit = app.authority.issue(&confirmation).unwrap();
            let result = block_on(app.assembly.runtime().execute_mock_draft(request, &permit));
            if variant == "cancelled" && action == MockDraftAction::Cancel {
                assert!(result.is_ok()); // Core cancellation is idempotent at the same version.
            } else {
                assert!(result.is_err(), "{action:?} {variant}");
            }
            assert_eq!(read(&app, &saved), before);
        }
    }
}

#[test]
fn modify_preserves_same_name_candidates_requires_owned_explicit_id_and_rejects_foreign_accounts() {
    let (app, saved) = setup(
        Arc::new(MockBankGateway::with_same_name_payees()),
        ManualClock::new(1000),
    );
    let candidates = block_on(app.store.find_candidates(&context("lifecycle"), "小李")).unwrap();
    assert_eq!(candidates.candidates.len(), 2);
    for (index, changes) in [
        json!({"payee_id":"payee_foreign"}),
        json!({"payer_account_id":"account_002"}),
        json!({"payee_query":"小李"}),
    ]
    .into_iter()
    .enumerate()
    {
        let (request, permit) = authorized(
            &app,
            MockDraftAction::Modify,
            index as u32 + 2,
            modify_input(&saved, changes),
        );
        assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        assert_eq!(read(&app, &saved), saved);
    }
    let (request, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        5,
        modify_input(&saved, json!({"payee_id":"payee_009"})),
    );
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    assert_eq!(
        read(&app, &saved).details.unwrap().payee.payee_id.as_str(),
        "payee_009"
    );
}

#[test]
fn mock_lifecycle_requires_runtime_reservation_and_exact_unforgeable_action_envelope() {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        let (app, saved) = ordinary();
        let arguments = if action == MockDraftAction::Modify {
            modify_input(&saved, json!({"amount":"300.00"}))
        } else {
            cancel_input(&saved)
        };
        let (request, permit) = authorized(&app, action, 2, arguments.clone());
        assert!(block_on(
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &MockDraftPermit::unregistered())
        )
        .is_err());
        assert!(matches!(
            block_on(app.assembly.runtime().execute(
                request.planning_result().unwrap(),
                request.session_ref.clone()
            )),
            Err(RuntimeError::WriteToolRequiresLaterStage(_))
        ));
        let tool = if action == MockDraftAction::Modify {
            MockDraftLifecycleTool::modify(app.store.clone(), app.authority.clone())
        } else {
            MockDraftLifecycleTool::cancel(app.store.clone(), app.authority.clone())
        };
        assert!(block_on(tool.execute(
            &context("lifecycle"),
            ToolExecutionAuthorization::MockDraftWriteV1(&permit),
            arguments
        ))
        .is_err());
        let mut variants = vec![];
        for (field, value) in [
            ("draft_id", json!("draft_other")),
            ("expected_version", json!(2)),
        ] {
            let mut changed = request.clone();
            changed.arguments[field] = value;
            variants.push(changed);
        }
        if action == MockDraftAction::Modify {
            let mut changed = request.clone();
            changed.arguments["changes"]["amount"] = json!("400.00");
            variants.push(changed);
        }
        let mut changed = request.clone();
        changed.tool_name = ToolName::parse(if action == MockDraftAction::Modify {
            MOCK_DRAFT_CANCEL_TOOL
        } else {
            MOCK_DRAFT_MODIFY_TOOL
        })
        .unwrap();
        variants.push(changed);
        let mut changed = request.clone();
        changed.tool_name = ToolName::parse("execute_transfer").unwrap();
        variants.push(changed);
        let mut changed = request.clone();
        changed.contract_version = 1;
        variants.push(changed);
        let mut changed = request.clone();
        changed.session_ref = SessionRef::parse("mock_session_002").unwrap();
        variants.push(changed);
        let mut changed = request.clone();
        changed.node_id = NodeId::parse("changed_node").unwrap();
        variants.push(changed);
        let mut changed = request.clone();
        changed.task_id = TaskId::parse("changed_task").unwrap();
        variants.push(changed);
        let mut changed = request.clone();
        changed.plan_version = 3;
        variants.push(changed);
        let mut changed = request.clone();
        changed.expires_at += 1;
        variants.push(changed);
        let mut changed = request.clone();
        changed.request_id = IdempotencyKey::parse("new_request").unwrap();
        variants.push(changed);
        for changed in variants {
            assert!(block_on(app.assembly.runtime().execute_mock_draft(changed, &permit)).is_err());
            assert_eq!(read(&app, &saved), saved);
        }
        block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
        let new_request = make_request(
            &app,
            action,
            3,
            if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"amount":"300.00"}))
            } else {
                cancel_input(&saved)
            },
        );
        assert!(block_on(
            app.assembly
                .runtime()
                .execute_mock_draft(new_request, &permit)
        )
        .is_err());
        assert_eq!(app.store.saved_receipt_count().unwrap(), 2);
    }
}

#[test]
fn lifecycle_contract_rejects_missing_confirmation_and_unsupported_tools() {
    let (app, saved) = ordinary();
    let request = make_request(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"amount":"300.00"})),
    );
    assert!(block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &MockDraftPermit::unregistered())
    )
    .is_err());
    let mut funds = request;
    funds.tool_name = ToolName::parse("execute_transfer").unwrap();
    assert!(app.authority.record_explicit_confirmation(&funds).is_err());
    assert_eq!(read(&app, &saved), saved);
}

struct FaultBank {
    inner: MockBankGateway,
    calls: AtomicUsize,
    fail_at: AtomicUsize,
    pause_at: AtomicUsize,
    hide_at: AtomicUsize,
    offline: AtomicBool,
    pause: Mutex<Option<oneshot::Receiver<()>>>,
}
impl FaultBank {
    fn new() -> Self {
        Self {
            inner: MockBankGateway::new(),
            calls: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(0),
            pause_at: AtomicUsize::new(0),
            hide_at: AtomicUsize::new(0),
            offline: AtomicBool::new(false),
            pause: Mutex::new(None),
        }
    }
    fn pause_last_modify_read(&self) -> oneshot::Sender<()> {
        self.pause_at
            .store(self.calls.load(Ordering::SeqCst) + 4, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        *self.pause.lock().unwrap() = Some(receiver);
        sender
    }
}
impl BankGateway for FaultBank {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.offline.load(Ordering::SeqCst) || call == self.fail_at.load(Ordering::SeqCst) {
                return Err(BankError::Unavailable);
            }
            if call == self.pause_at.load(Ordering::SeqCst) {
                let receiver = self.pause.lock().unwrap().take().unwrap();
                receiver.await.map_err(|_| BankError::Unavailable)?;
            }
            self.inner.get_account_balance(user, id).await
        })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async move {
            if self.offline.load(Ordering::SeqCst) {
                return Err(BankError::Unavailable);
            }
            if self.calls.load(Ordering::SeqCst) == self.hide_at.load(Ordering::SeqCst)
                && self.hide_at.load(Ordering::SeqCst) != 0
            {
                return Ok(FindPayeesOutput { candidates: vec![] });
            }
            self.inner.find_payees(user, query).await
        })
    }
}

#[test]
fn failed_bank_or_disappeared_final_payee_is_atomic_and_same_request_never_refreshes_again() {
    for mode in ["pre_policy", "final_read", "final_payee"] {
        let bank = Arc::new(FaultBank::new());
        let (app, saved) = setup(bank.clone(), ManualClock::new(1000));
        let next = bank.calls.load(Ordering::SeqCst);
        if mode == "final_payee" {
            bank.hide_at.store(next + 4, Ordering::SeqCst);
        } else {
            bank.fail_at.store(
                next + if mode == "pre_policy" { 1 } else { 4 },
                Ordering::SeqCst,
            );
        }
        let (request, permit) = authorized(
            &app,
            MockDraftAction::Modify,
            2,
            modify_input(&saved, json!({"amount":"300.00"})),
        );
        let first = block_on(
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
        );
        assert!(first.is_err(), "{mode}");
        let calls = bank.calls.load(Ordering::SeqCst);
        assert_eq!(
            block_on(app.assembly.runtime().execute_mock_draft(request, &permit)),
            first
        );
        assert_eq!(bank.calls.load(Ordering::SeqCst), calls);
        assert_eq!(read(&app, &saved), saved);
        assert_eq!(app.store.saved_receipt_count().unwrap(), 1);
        assert!(app
            .audit
            .events()
            .unwrap()
            .iter()
            .any(|event| event.kind == AuditEventKind::WriteFailed));
    }
}

#[test]
fn modify_bank_wait_checks_revocation_permit_expiry_task_revoke_and_business_expiry_before_save() {
    for mode in ["revoke", "permit_expiry", "task_revoke", "draft_expiry"] {
        let clock = ManualClock::new(1000);
        let bank = Arc::new(FaultBank::new());
        let (app, saved) = setup(bank.clone(), clock.clone());
        let sender = bank.pause_last_modify_read();
        let mut request = make_request(
            &app,
            MockDraftAction::Modify,
            2,
            modify_input(&saved, json!({"amount":"300.00"})),
        );
        if mode == "draft_expiry" {
            request.expires_at = 1600; // Keep the permit live to isolate business expiry.
        }
        let confirmation = app
            .authority
            .record_explicit_confirmation(&request)
            .unwrap();
        let permit = app.authority.issue(&confirmation).unwrap();
        let mut execution = Box::pin(app.assembly.runtime().execute_mock_draft(request, &permit));
        assert!(execution.as_mut().now_or_never().is_none());
        match mode {
            "revoke" => app.authority.revoke(&permit).unwrap(),
            "task_revoke" => app
                .authority
                .revoke_task(&TaskId::parse("lifecycle").unwrap())
                .unwrap(),
            "permit_expiry" => clock.set(1060),
            _ => clock.set(saved.expires_at),
        }
        sender.send(()).unwrap();
        assert!(block_on(execution).is_err());
        let actual = read(&app, &saved);
        assert_eq!(actual.plan_version, 1);
        assert_eq!(actual.details, saved.details);
        assert_eq!(actual.expires_at, saved.expires_at);
        assert_eq!(
            actual.status,
            if mode == "draft_expiry" {
                DraftStatus::Expired
            } else {
                saved.status
            }
        );
    }
}

#[test]
fn newly_confirmed_cancel_during_modify_bank_wait_invalidates_old_write_then_cancels_without_reviving(
) {
    let bank = Arc::new(FaultBank::new());
    let (app, saved) = setup(bank.clone(), ManualClock::new(1000));
    let sender = bank.pause_last_modify_read();
    let (modify, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"amount":"300.00"})),
    );
    let mut execution = Box::pin(app.assembly.runtime().execute_mock_draft(modify, &permit));
    assert!(execution.as_mut().now_or_never().is_none());
    let (cancel, cancel_permit) =
        authorized(&app, MockDraftAction::Cancel, 3, cancel_input(&saved));
    let mut duplicate_cancel = Box::pin(
        app.assembly
            .runtime()
            .execute_mock_draft(cancel.clone(), &cancel_permit),
    );
    let mut cancellation = Box::pin(
        app.assembly
            .runtime()
            .execute_mock_draft(cancel, &cancel_permit),
    );
    assert!(cancellation.as_mut().now_or_never().is_none()); // waits for the draft mutex, not bank interruption
    assert!(duplicate_cancel.as_mut().now_or_never().is_none()); // joins the same reserved cancellation
    sender.send(()).unwrap();
    let (modified, cancelled, duplicate) =
        block_on(async { futures::join!(execution, cancellation, duplicate_cancel) });
    assert!(modified.is_err());
    assert!(cancelled.is_ok());
    assert_eq!(cancelled, duplicate);
    let actual = read(&app, &saved);
    assert_eq!(actual.status, DraftStatus::Cancelled);
    assert_eq!(actual.plan_version, 1);
    assert_eq!(actual.details, saved.details);
    assert_eq!(bank.calls.load(Ordering::SeqCst), 8);
}

#[test]
fn concurrent_duplicate_modify_and_cancel_reserve_one_execution_and_replay_identical_results() {
    let bank = Arc::new(FaultBank::new());
    let (app, saved) = setup(bank.clone(), ManualClock::new(1000));
    let sender = bank.pause_last_modify_read();
    let (request, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"amount":"300.00"})),
    );
    let (first, duplicate, ()) = block_on(async {
        futures::join!(
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
            async {
                sender.send(()).unwrap();
            }
        )
    });
    assert!(first.is_ok());
    assert_eq!(first, duplicate);
    let updated = read(&app, &saved);
    assert_eq!(updated.plan_version, 2);
    let (request, permit) = authorized(&app, MockDraftAction::Cancel, 3, cancel_input(&updated));
    let (first, duplicate) = block_on(async {
        futures::join!(
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit)
        )
    });
    assert!(first.is_ok());
    assert_eq!(first, duplicate);
    assert_eq!(bank.calls.load(Ordering::SeqCst), 8);
    assert_eq!(app.store.saved_receipt_count().unwrap(), 3);
    assert_eq!(
        app.audit
            .events()
            .unwrap()
            .iter()
            .filter(|event| event.kind == AuditEventKind::ToolStarted)
            .count(),
        3
    );
}

#[test]
fn cancelled_business_draft_is_independent_of_authority_revoke_and_bank_availability() {
    let bank = Arc::new(FaultBank::new());
    let (app, saved) = setup(bank.clone(), ManualClock::new(1000));
    let (request, permit) = authorized(&app, MockDraftAction::Cancel, 2, cancel_input(&saved));
    app.authority.revoke(&permit).unwrap();
    assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
    assert_eq!(read(&app, &saved), saved);
    bank.offline.store(true, Ordering::SeqCst);
    let (request, permit) = authorized(&app, MockDraftAction::Cancel, 3, cancel_input(&saved));
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    assert_eq!(bank.calls.load(Ordering::SeqCst), 4);
    assert_eq!(read(&app, &saved).status, DraftStatus::Cancelled);
}

#[test]
fn a_new_task_plan_version_across_nodes_revokes_unexecuted_lifecycle_and_forbids_rollback_conflict()
{
    let (app, saved) = ordinary();
    let (old, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"amount":"300.00"})),
    );
    let new = make_request(
        &app,
        MockDraftAction::Modify,
        3,
        modify_input(&saved, json!({"amount":"400.00"})),
    );
    let confirmation = app.authority.record_explicit_confirmation(&new).unwrap();
    let new_permit = app.authority.issue(&confirmation).unwrap();
    assert!(block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(old.clone(), &permit)
    )
    .is_err());
    assert!(app.authority.record_explicit_confirmation(&old).is_err());
    let mut conflict = new.clone();
    conflict.node_id = NodeId::parse("same_version_other_node").unwrap();
    conflict.request_id = IdempotencyKey::parse("conflict_request").unwrap();
    assert!(app
        .authority
        .record_explicit_confirmation(&conflict)
        .is_err());
    block_on(app.assembly.runtime().execute_mock_draft(new, &new_permit)).unwrap();
    let updated = read(&app, &saved);
    assert_eq!(updated.plan_version, 2); // Runtime version 3 is not business version 3.
    assert_eq!(updated.details.unwrap().amount.as_str(), "400.00");
}

fn lifecycle_parts(
    app: &App,
    action: MockDraftAction,
) -> (Arc<dyn AgentTool>, Arc<dyn DomainPolicy>) {
    match action {
        MockDraftAction::Modify => (
            Arc::new(MockDraftLifecycleTool::modify(
                app.store.clone(),
                app.authority.clone(),
            )),
            Arc::new(MockDraftLifecyclePolicy::modify(app.store.clone())),
        ),
        MockDraftAction::Cancel => (
            Arc::new(MockDraftLifecycleTool::cancel(
                app.store.clone(),
                app.authority.clone(),
            )),
            Arc::new(MockDraftLifecyclePolicy::cancel(app.store.clone())),
        ),
        MockDraftAction::Create => panic!("lifecycle only"),
    }
}
fn assembly_one(
    tool: Arc<dyn AgentTool>,
    policy: Arc<dyn DomainPolicy>,
    level: SecurityLevel,
    audit: Arc<dyn AuditRecorder>,
    sessions: Arc<dyn SessionResolver>,
    authority: Arc<MockDraftAuthority>,
) -> RuntimeAssembly {
    let name = tool.definition().name;
    let mut tools = ToolRegistry::new();
    tools.register(tool).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            policy,
            PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).unwrap(),
            MOCK_DRAFT_SCHEMA_VERSION,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: name,
            tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).unwrap(),
            minimum_level: level,
        })
        .unwrap();
    RuntimeBootstrap::build_mock_draft(tools, policies, catalog, sessions, audit, authority)
        .unwrap()
}

#[test]
fn lifecycle_startup_requires_policy_profile_and_schema_version_matching() {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        let (app, _) = ordinary();
        let (tool, policy) = lifecycle_parts(&app, action);
        let mut tools = ToolRegistry::new();
        tools.register(tool).unwrap();
        let mut policies = DomainPolicyRegistry::new();
        let mut catalog = ToolSecurityCatalog::new();
        assert!(matches!(
            validate_tool_policy_bindings(&tools, &policies, &catalog),
            Err(StartupValidationError::MissingPolicy(_))
        ));
        policies
            .register(
                policy,
                PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).unwrap(),
                MOCK_DRAFT_SCHEMA_VERSION,
            )
            .unwrap();
        assert!(matches!(
            validate_tool_policy_bindings(&tools, &policies, &catalog),
            Err(StartupValidationError::MissingSecurityProfile(_))
        ));
        catalog
            .register(ToolSecurityProfile {
                tool_name: ToolName::parse(action.tool_name()).unwrap(),
                tool_schema_version: 2,
                policy_version: PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).unwrap(),
                minimum_level: SecurityLevel::Green,
            })
            .unwrap();
        assert!(matches!(
            validate_tool_policy_bindings(&tools, &policies, &catalog),
            Err(StartupValidationError::CatalogSchemaMismatch { .. })
        ));
    }
}

#[test]
fn stricter_central_profiles_and_funds_action_under_a_mock_name_remain_blocked() {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        for level in [
            SecurityLevel::Yellow,
            SecurityLevel::Red,
            SecurityLevel::Critical,
        ] {
            let (app, saved) = ordinary();
            let (tool, policy) = lifecycle_parts(&app, action);
            let assembly = assembly_one(
                tool,
                policy,
                level,
                app.audit.clone(),
                app.sessions.clone(),
                app.authority.clone(),
            );
            let input = if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"amount":"300.00"}))
            } else {
                cancel_input(&saved)
            };
            let (request, permit) = authorized(&app, action, 2, input);
            assert!(block_on(assembly.runtime().execute_mock_draft(request, &permit)).is_err());
            assert_eq!(read(&app, &saved), saved);
        }
        let (app, saved) = ordinary();
        let (tool, policy) = lifecycle_parts(&app, action);
        let tool = Arc::new(MutatedTool {
            inner: tool,
            mutation: "funds_definition",
        });
        let assembly = assembly_one(
            tool,
            policy,
            SecurityLevel::Green,
            app.audit.clone(),
            app.sessions.clone(),
            app.authority.clone(),
        );
        let input = if action == MockDraftAction::Modify {
            modify_input(&saved, json!({"amount":"300.00"}))
        } else {
            cancel_input(&saved)
        };
        let (request, permit) = authorized(&app, action, 2, input);
        assert!(block_on(assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        assert_eq!(read(&app, &saved), saved);
    }
}

struct MutatedTool {
    inner: Arc<dyn AgentTool>,
    mutation: &'static str,
}
impl AgentTool for MutatedTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        if self.mutation == "funds_definition" {
            definition.action = "SUBMIT_FUNDS".into();
        }
        definition
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let mut result = self
                .inner
                .execute(context, authorization, arguments)
                .await?;
            match self.mutation {
                "action" => result.data["action"] = json!(MOCK_DRAFT_CREATE_ACTION),
                "version" => result.data["plan_version"] = json!(9),
                "expected" => result.data["expected_version"] = json!(9),
                "body" => result.data["amount"] = json!("900.00"),
                "reference" => result.result_ref = ToolResultRef::parse("fake_result").unwrap(),
                "failed" => result.status = ToolResultStatus::Failed,
                "processing" => result.status = ToolResultStatus::Processing,
                _ => {}
            }
            Ok(result)
        })
    }
}

#[test]
fn tampered_or_failed_lifecycle_receipts_never_report_success_and_retries_never_mutate_again() {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        for mutation in [
            "action",
            "version",
            "expected",
            "body",
            "reference",
            "failed",
            "processing",
        ] {
            let (app, saved) = ordinary();
            let (tool, policy) = lifecycle_parts(&app, action);
            let tool = Arc::new(MutatedTool {
                inner: tool,
                mutation,
            });
            let assembly = assembly_one(
                tool,
                policy,
                SecurityLevel::Green,
                app.audit.clone(),
                app.sessions.clone(),
                app.authority.clone(),
            );
            let input = if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"amount":"300.00"}))
            } else {
                cancel_input(&saved)
            };
            let (request, permit) = authorized(&app, action, 2, input);
            let first = block_on(
                assembly
                    .runtime()
                    .execute_mock_draft(request.clone(), &permit),
            );
            assert!(first.is_err(), "{action:?} {mutation}");
            assert_eq!(
                block_on(assembly.runtime().execute_mock_draft(request, &permit)),
                first
            );
            let actual = read(&app, &saved);
            assert_eq!(
                actual.plan_version,
                if action == MockDraftAction::Modify {
                    2
                } else {
                    1
                }
            );
            assert_eq!(
                actual.status,
                if action == MockDraftAction::Modify {
                    DraftStatus::ReadyToConfirm
                } else {
                    DraftStatus::Cancelled
                }
            );
            assert_eq!(app.store.saved_receipt_count().unwrap(), 2);
            assert_eq!(
                app.audit
                    .events()
                    .unwrap()
                    .iter()
                    .filter(|event| event.kind == AuditEventKind::ToolCompleted)
                    .count(),
                1
            );
        }
    }
}

struct FailingAudit {
    on: AuditEventKind,
    inner: InMemoryAuditRecorder,
}
impl AuditRecorder for FailingAudit {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        if event.kind == self.on {
            Err(AuditError::Unavailable)
        } else {
            self.inner.record(event)
        }
    }
}
#[test]
fn lifecycle_audit_failure_before_or_after_mutation_is_terminal_without_fake_success_or_second_save(
) {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        for on in [
            AuditEventKind::ToolStarted,
            AuditEventKind::ToolCompleted,
            AuditEventKind::TaskCompleted,
        ] {
            let (app, saved) = ordinary();
            let (tool, policy) = lifecycle_parts(&app, action);
            let audit = Arc::new(FailingAudit {
                on,
                inner: InMemoryAuditRecorder::new(),
            });
            let assembly = assembly_one(
                tool,
                policy,
                SecurityLevel::Green,
                audit.clone(),
                app.sessions.clone(),
                app.authority.clone(),
            );
            let input = if action == MockDraftAction::Modify {
                modify_input(&saved, json!({"amount":"300.00"}))
            } else {
                cancel_input(&saved)
            };
            let (request, permit) = authorized(&app, action, 2, input);
            let first = block_on(
                assembly
                    .runtime()
                    .execute_mock_draft(request.clone(), &permit),
            );
            assert!(matches!(first, Err(RuntimeError::Audit(_))));
            let after = read(&app, &saved);
            assert_eq!(
                block_on(assembly.runtime().execute_mock_draft(request, &permit)),
                first
            );
            assert_eq!(read(&app, &saved), after);
            if on == AuditEventKind::ToolStarted {
                assert_eq!(after, saved);
            } else if action == MockDraftAction::Modify {
                assert_eq!(after.plan_version, 2);
            } else {
                assert_eq!(after.status, DraftStatus::Cancelled);
            }
            assert!(audit
                .inner
                .events()
                .unwrap()
                .iter()
                .any(|event| event.kind == AuditEventKind::WriteFailed));
        }
    }
}

#[test]
fn historical_success_receipt_is_not_current_draft_and_saved_modification_is_not_rolled_back() {
    let (app, saved) = ordinary();
    let (request, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"amount":"300.00"})),
    );
    let outcome = block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    )
    .unwrap();
    let RuntimeOutcome::Completed { results, .. } = &outcome else {
        panic!("complete")
    };
    let receipt = PostExecuteContext {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: request.task_id.clone(),
        node_id: request.node_id.clone(),
        operation_id: results[0].operation_id.clone(),
        tool_name: results[0].tool_name.clone(),
        authenticated_user_id: UserId::parse("user_001").unwrap(),
        observation: ExecutionObservation::ResultReceived {
            attempt_id: AttemptId::parse("mock_attempt").unwrap(),
            tool_result_ref: results[0].result_ref.clone(),
            business_operation_ref: BusinessOperationRef::parse("mock_operation").unwrap(),
        },
    };
    let policy = MockDraftLifecyclePolicy::modify(app.store.clone());
    assert!(block_on(policy.verify_execution(&receipt))
        .unwrap()
        .is_empty());
    let (stale_cancel, stale_permit) =
        authorized(&app, MockDraftAction::Cancel, 3, cancel_input(&saved));
    assert!(block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(stale_cancel, &stale_permit)
    )
    .is_err());
    let updated = read(&app, &saved);
    assert_eq!(updated.plan_version, 2);
    assert_eq!(updated.details.as_ref().unwrap().amount.as_str(), "300.00");
    let (cancel, cancel_permit) =
        authorized(&app, MockDraftAction::Cancel, 4, cancel_input(&updated));
    block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(cancel, &cancel_permit),
    )
    .unwrap();
    assert!(block_on(policy.verify_execution(&receipt))
        .unwrap()
        .iter()
        .any(|finding| finding.kind == FindingKind::HardDeny));
    assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err()); // superseded capability
    assert_eq!(body(&outcome)["status"], "READY_TO_CONFIRM"); // only historical snapshot
    assert_eq!(read(&app, &saved).status, DraftStatus::Cancelled);
}

#[test]
fn interrupted_lifecycle_execution_is_unknown_and_does_not_repeat_the_bank_or_business_write() {
    let bank = Arc::new(FaultBank::new());
    let (app, saved) = setup(bank.clone(), ManualClock::new(1000));
    let _sender = bank.pause_last_modify_read();
    let (request, permit) = authorized(
        &app,
        MockDraftAction::Modify,
        2,
        modify_input(&saved, json!({"amount":"300.00"})),
    );
    let mut execution = Box::pin(
        app.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    );
    assert!(execution.as_mut().now_or_never().is_none());
    drop(execution);
    let retried = block_on(app.assembly.runtime().execute_mock_draft(request, &permit));
    assert!(
        matches!(retried, Err(RuntimeError::MockDraftAuthorization(ref reason)) if reason == "EXECUTION_INTERRUPTED_RESULT_UNKNOWN")
    );
    assert_eq!(bank.calls.load(Ordering::SeqCst), 8);
    assert_eq!(read(&app, &saved), saved);
}

struct LiveSession {
    state: Mutex<Option<SessionBinding>>,
}
impl SessionResolver for LiveSession {
    fn resolve(&self, session: &SessionRef) -> Result<UserId, SecurityError> {
        self.resolve_binding(session).map(|binding| binding.user_id)
    }
    fn resolve_binding(&self, session: &SessionRef) -> Result<SessionBinding, SecurityError> {
        if session.as_str() == "mock_session_001" {
            if let Some(binding) = self.state.lock().unwrap().clone() {
                return Ok(binding);
            }
        }
        Err(SecurityError {
            reason_code: ReasonCode::parse("INVALID_SESSION").unwrap(),
            message: "invalid session".into(),
        })
    }
}
struct UnversionedSession;
impl SessionResolver for UnversionedSession {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(UserId::parse("user_001").unwrap())
    }
}

#[test]
fn lifecycle_requires_versioned_sessions_and_rejects_revoked_rebound_or_renewed_grants_including_same_user(
) {
    let (app, saved) = ordinary();
    let unversioned = MockDraftAuthority::new(
        Arc::new(UnversionedSession),
        Arc::new(DemoClock(ManualClock::new(1000))),
    );
    let request = make_request(&app, MockDraftAction::Cancel, 2, cancel_input(&saved));
    assert!(unversioned.record_explicit_confirmation(&request).is_err());
    for mode in [
        "revoked",
        "other_user",
        "same_user",
        "zero_version",
        "expired",
        "wrong_session",
    ] {
        let (app, saved) = ordinary();
        let clock = ManualClock::new(1000);
        let sessions = Arc::new(LiveSession {
            state: Mutex::new(Some(SessionBinding {
                user_id: UserId::parse("user_001").unwrap(),
                binding_version: 1,
            })),
        });
        let authority = Arc::new(MockDraftAuthority::new(
            sessions.clone(),
            Arc::new(DemoClock(clock.clone())),
        ));
        let tool = Arc::new(MockDraftLifecycleTool::cancel(
            app.store.clone(),
            authority.clone(),
        ));
        let policy = Arc::new(MockDraftLifecyclePolicy::cancel(app.store.clone()));
        let assembly = assembly_one(
            tool,
            policy,
            SecurityLevel::Green,
            app.audit.clone(),
            sessions.clone(),
            authority.clone(),
        );
        let request = authority
            .request_for_action(
                MockDraftAction::Cancel,
                SessionRef::parse("mock_session_001").unwrap(),
                TaskId::parse("lifecycle").unwrap(),
                NodeId::parse("cancel").unwrap(),
                2,
                cancel_input(&saved),
                60,
            )
            .unwrap();
        let confirmation = authority.record_explicit_confirmation(&request).unwrap();
        let permit = authority.issue(&confirmation).unwrap();
        match mode {
            "revoked" => *sessions.state.lock().unwrap() = None,
            "other_user" => {
                *sessions.state.lock().unwrap() = Some(SessionBinding {
                    user_id: UserId::parse("user_002").unwrap(),
                    binding_version: 2,
                })
            }
            "same_user" => {
                *sessions.state.lock().unwrap() = Some(SessionBinding {
                    user_id: UserId::parse("user_001").unwrap(),
                    binding_version: 2,
                })
            }
            "zero_version" => {
                sessions
                    .state
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .binding_version = 0
            }
            "expired" => clock.set(1060),
            _ => {}
        }
        let mut request = request;
        if mode == "wrong_session" {
            request.session_ref = SessionRef::parse("mock_session_002").unwrap();
        }
        assert!(
            block_on(assembly.runtime().execute_mock_draft(request, &permit)).is_err(),
            "{mode}"
        );
        assert_eq!(read(&app, &saved), saved);
    }
}

#[test]
fn session_rebinding_to_same_user_during_final_modify_bank_wait_is_checked_before_save() {
    let bank = Arc::new(FaultBank::new());
    let clock = ManualClock::new(1000);
    let (app, saved) = setup(bank.clone(), clock.clone());
    let sessions = Arc::new(LiveSession {
        state: Mutex::new(Some(SessionBinding {
            user_id: UserId::parse("user_001").unwrap(),
            binding_version: 1,
        })),
    });
    let authority = Arc::new(MockDraftAuthority::new(
        sessions.clone(),
        Arc::new(DemoClock(clock)),
    ));
    let assembly = assembly_one(
        Arc::new(MockDraftLifecycleTool::modify(
            app.store.clone(),
            authority.clone(),
        )),
        Arc::new(MockDraftLifecyclePolicy::modify(app.store.clone())),
        SecurityLevel::Green,
        app.audit.clone(),
        sessions.clone(),
        authority.clone(),
    );
    let sender = bank.pause_last_modify_read();
    let request = authority
        .request_for_action(
            MockDraftAction::Modify,
            SessionRef::parse("mock_session_001").unwrap(),
            TaskId::parse("lifecycle").unwrap(),
            NodeId::parse("modify").unwrap(),
            2,
            modify_input(&saved, json!({"amount":"300.00"})),
            60,
        )
        .unwrap();
    let confirmation = authority.record_explicit_confirmation(&request).unwrap();
    let permit = authority.issue(&confirmation).unwrap();
    let mut execution = Box::pin(assembly.runtime().execute_mock_draft(request, &permit));
    assert!(execution.as_mut().now_or_never().is_none());
    sessions
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .binding_version = 2;
    sender.send(()).unwrap();
    assert!(block_on(execution).is_err());
    assert_eq!(read(&app, &saved), saved);
}

struct RefusingPostPolicy {
    inner: Arc<dyn DomainPolicy>,
}
impl DomainPolicy for RefusingPostPolicy {
    fn tool_name(&self) -> &ToolName {
        self.inner.tool_name()
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        self.inner.evaluate(context)
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async {
            Ok(vec![SecurityFinding {
                check_id: CheckId::parse("MOCK-DRAFT-POST").unwrap(),
                policy_version: PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).unwrap(),
                source: SourceName::parse("mock_draft_policy").unwrap(),
                kind: FindingKind::HardDeny,
                reason_code: ReasonCode::parse("POST_EXECUTE_DENIED").unwrap(),
                evidence_refs: vec![],
                observed_value: None,
                event_ids: vec![],
                dedup_key: None,
                window_start_ms: None,
                window_end_ms: None,
            }])
        })
    }
}

#[test]
fn post_execute_lifecycle_denial_preserves_written_business_state_without_success_or_reexecution() {
    for action in [MockDraftAction::Modify, MockDraftAction::Cancel] {
        let (app, saved) = ordinary();
        let (tool, policy) = lifecycle_parts(&app, action);
        let assembly = assembly_one(
            tool,
            Arc::new(RefusingPostPolicy { inner: policy }),
            SecurityLevel::Green,
            app.audit.clone(),
            app.sessions.clone(),
            app.authority.clone(),
        );
        let input = if action == MockDraftAction::Modify {
            modify_input(&saved, json!({"amount":"300.00"}))
        } else {
            cancel_input(&saved)
        };
        let (request, permit) = authorized(&app, action, 2, input);
        let first = block_on(
            assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
        );
        assert!(first.is_err());
        let after = read(&app, &saved);
        assert_eq!(
            block_on(assembly.runtime().execute_mock_draft(request, &permit)),
            first
        );
        assert_eq!(read(&app, &saved), after);
        assert_eq!(
            after.plan_version,
            if action == MockDraftAction::Modify {
                2
            } else {
                1
            }
        );
        assert_eq!(
            after.status,
            if action == MockDraftAction::Modify {
                DraftStatus::ReadyToConfirm
            } else {
                DraftStatus::Cancelled
            }
        );
        assert_eq!(app.store.saved_receipt_count().unwrap(), 2);
        assert_eq!(
            app.audit
                .events()
                .unwrap()
                .iter()
                .filter(|event| event.kind == AuditEventKind::TaskCompleted)
                .count(),
            1
        );
    }
}
