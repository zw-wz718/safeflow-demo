#[path = "../examples/support/mock_draft_write.rs"]
mod support;

use futures::{channel::oneshot, executor::block_on, future::FutureExt};
use safeflow_bank_gateway::{BankError, BankGateway, FindPayeesOutput, GetAccountBalanceOutput};
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::*;
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::{runtime_tools::*, DraftStatus};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use support::{assemble, context, App, DemoClock};

fn input() -> Value {
    json!({"payer_account_id": "account_001", "payee_id": "payee_008", "amount": "200.00", "currency": "CNY", "remark": "", "replaces": null})
}
fn setup() -> App {
    setup_bank(Arc::new(MockBankGateway::new()), ManualClock::new(1000))
}
fn setup_bank(bank: Arc<dyn BankGateway>, clock: ManualClock) -> App {
    let app = assemble(bank, DemoClock(clock)).unwrap();
    block_on(app.store.find_candidates(&context("task_write"), "小李")).unwrap();
    app
}
fn authorized(app: &App, arguments: Value) -> (MockDraftWriteRequest, MockDraftPermit) {
    let request = app
        .authority
        .request(
            SessionRef::parse("mock_session_001").unwrap(),
            TaskId::parse("task_write").unwrap(),
            NodeId::parse("save_draft").unwrap(),
            1,
            arguments,
            60,
        )
        .unwrap();
    let confirmation = app
        .authority
        .record_explicit_confirmation(&request)
        .unwrap();
    let permit = app.authority.issue(&confirmation).unwrap();
    assert!(permit.same_instance(&app.authority.issue(&confirmation).unwrap()));
    (request, permit)
}
fn no_draft(app: &App) {
    assert_eq!(
        block_on(app.store.read_created_for_task(&context("task_write"))).unwrap(),
        None
    );
    assert_eq!(app.store.saved_receipt_count().unwrap(), 0);
}

#[test]
fn runtime_saves_reads_and_replays_one_real_draft_with_audit() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    assert!(!ToolExecutionAuthorization::MockDraftWriteV1(&permit).permits_side_effects());
    let outcome = block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    )
    .unwrap();
    assert_eq!(
        block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap(),
        outcome
    );
    let RuntimeOutcome::Completed { results, .. } = outcome else {
        panic!("must complete")
    };
    assert_eq!(results[0].data["amount"], "200.00");
    assert_eq!(results[0].data["plan_version"], 1);
    assert_eq!(results[0].data["runtime_plan_version"], 1);
    chrono::DateTime::parse_from_rfc3339(results[0].data["expires_at"].as_str().unwrap()).unwrap();
    assert_ne!(
        results[0].operation_id.as_str(),
        results[0].data["operation_id"].as_str().unwrap()
    );
    let draft = block_on(app.store.read_created_for_task(&context("task_write")))
        .unwrap()
        .unwrap();
    assert_eq!(draft.status, DraftStatus::ReadyToConfirm);
    assert_eq!(draft.details.unwrap().amount.as_str(), "200.00");
    let events = app.audit.events().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == AuditEventKind::ToolStarted)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == AuditEventKind::ToolCompleted)
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|e| e.kind == AuditEventKind::WriteReplayed));
}

#[test]
fn ordinary_runtime_and_direct_tool_calls_cannot_use_the_new_write_path() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    assert!(matches!(
        block_on(app.assembly.runtime().execute(
            request.planning_result().unwrap(),
            request.session_ref.clone()
        )),
        Err(RuntimeError::WriteToolRequiresLaterStage(_))
    ));
    let tool = CreateMockDraftTool::new(app.store.clone(), app.authority.clone());
    assert!(block_on(tool.execute(
        &context("task_write"),
        ToolExecutionAuthorization::ReadOnly,
        input()
    ))
    .is_err());
    assert!(block_on(tool.execute(
        &context("task_write"),
        ToolExecutionAuthorization::MockDraftWriteV1(&permit),
        input()
    ))
    .is_err());
    no_draft(&app);
}

#[test]
fn unregistered_permit_and_missing_confirmation_are_rejected_and_audited() {
    let app = setup();
    let request = app
        .authority
        .request(
            SessionRef::parse("mock_session_001").unwrap(),
            TaskId::parse("task_write").unwrap(),
            NodeId::parse("save_draft").unwrap(),
            1,
            input(),
            60,
        )
        .unwrap();
    assert!(block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(request, &MockDraftPermit::unregistered())
    )
    .is_err());
    assert!(app
        .audit
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditEventKind::WriteRejected));
    no_draft(&app);
}

#[test]
fn every_envelope_binding_is_checked_without_consuming_the_original_request() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    let mut variants = vec![];
    let mut changed = request.clone();
    changed.arguments["amount"] = json!("300.00");
    variants.push(changed);
    let mut changed = request.clone();
    changed.task_id = TaskId::parse("other_task").unwrap();
    variants.push(changed);
    let mut changed = request.clone();
    changed.node_id = NodeId::parse("other_node").unwrap();
    variants.push(changed);
    let mut changed = request.clone();
    changed.session_ref = SessionRef::parse("mock_session_002").unwrap();
    variants.push(changed);
    let mut changed = request.clone();
    changed.plan_version = 2;
    variants.push(changed);
    let mut changed = request.clone();
    changed.tool_name = ToolName::parse("execute_transfer").unwrap();
    variants.push(changed);
    let mut changed = request.clone();
    changed.contract_version = 99;
    variants.push(changed);
    let mut changed = request.clone();
    changed.request_id = IdempotencyKey::parse("fake_request").unwrap();
    variants.push(changed);
    let mut changed = request.clone();
    changed.expires_at += 1;
    variants.push(changed);
    for changed in variants {
        assert!(block_on(app.assembly.runtime().execute_mock_draft(changed, &permit)).is_err());
        no_draft(&app);
    }
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
}

#[test]
fn consumed_permit_cannot_authorize_a_new_request_or_a_second_tool_call() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    let new_request = app
        .authority
        .request(
            SessionRef::parse("mock_session_001").unwrap(),
            TaskId::parse("task_write").unwrap(),
            NodeId::parse("save_draft").unwrap(),
            1,
            input(),
            60,
        )
        .unwrap();
    assert!(block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(new_request, &permit)
    )
    .is_err());
    let tool = CreateMockDraftTool::new(app.store.clone(), app.authority.clone());
    assert!(block_on(tool.execute(
        &context("task_write"),
        ToolExecutionAuthorization::MockDraftWriteV1(&permit),
        input()
    ))
    .is_err());
    assert_eq!(app.store.saved_receipt_count().unwrap(), 1);
}

#[test]
fn expired_and_revoked_permits_never_save() {
    for expired in [true, false] {
        let clock = ManualClock::new(1000);
        let app = setup_bank(Arc::new(MockBankGateway::new()), clock.clone());
        let (request, permit) = authorized(&app, input());
        if expired {
            clock.set(1060);
        } else {
            app.authority.revoke(&permit).unwrap();
        }
        assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        no_draft(&app);
    }
}

#[test]
fn task_revocation_invalidates_confirmations_and_cached_result_access() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    )
    .unwrap();
    app.authority.revoke_task(&request.task_id).unwrap();
    assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
}

#[test]
fn missing_invalid_or_identity_fields_and_replacement_requests_do_not_save() {
    let mut variants = vec![];
    for field in ["payer_account_id", "payee_id", "amount", "currency"] {
        let mut value = input();
        value.as_object_mut().unwrap().remove(field);
        variants.push(value);
    }
    for amount in [
        json!(200.0),
        json!("0.00"),
        json!("-1.00"),
        json!("1e2"),
        json!("200.001"),
        json!("18446744073709551616.00"),
    ] {
        let mut value = input();
        value["amount"] = amount;
        variants.push(value);
    }
    for field in [
        "user_id",
        "approved",
        "verified",
        "session_ref",
        "confirmation_ref",
        "permit",
        "plan_version",
    ] {
        let mut value = input();
        value[field] = json!("fake");
        variants.push(value);
    }
    let mut value = input();
    value["currency"] = json!("USD");
    variants.push(value);
    let mut value = input();
    value["replaces"] = json!({"draft_id":"draft_1","plan_version":1});
    variants.push(value);
    for value in variants {
        let app = setup();
        let (request, permit) = authorized(&app, value);
        assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        no_draft(&app);
    }
}

#[test]
fn foreign_account_or_payee_and_unselected_same_name_never_save() {
    for (field, id) in [
        ("payer_account_id", "account_002"),
        ("payee_id", "payee_002"),
        ("payee_id", "小李"),
    ] {
        let app = setup_bank(
            Arc::new(MockBankGateway::with_same_name_payees()),
            ManualClock::new(1000),
        );
        let mut value = input();
        value[field] = json!(id);
        let (request, permit) = authorized(&app, value);
        assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        no_draft(&app);
    }
}

#[test]
fn same_name_requires_stable_selection_and_preserves_both_candidates() {
    let app = setup_bank(
        Arc::new(MockBankGateway::with_same_name_payees()),
        ManualClock::new(1000),
    );
    let candidates = block_on(app.store.find_candidates(&context("task_write"), "小李")).unwrap();
    assert_eq!(candidates.candidates.len(), 2);
    no_draft(&app);
    let mut value = input();
    value["payee_id"] = json!("payee_009");
    let (request, permit) = authorized(&app, value);
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    assert_eq!(
        block_on(app.store.read_created_for_task(&context("task_write")))
            .unwrap()
            .unwrap()
            .details
            .unwrap()
            .payee
            .payee_id
            .as_str(),
        "payee_009"
    );
}

// Faults at the formal bank boundary, including the final async read before save.
struct FaultBank {
    inner: MockBankGateway,
    calls: AtomicUsize,
    fail_at: usize,
    advance_at: usize,
    clock: ManualClock,
    revoke_at: Mutex<Option<(Arc<MockDraftAuthority>, MockDraftPermit)>>,
    pause: Mutex<Option<oneshot::Receiver<()>>>,
}
impl FaultBank {
    fn new(clock: ManualClock) -> Self {
        Self {
            inner: MockBankGateway::new(),
            calls: AtomicUsize::new(0),
            fail_at: 0,
            advance_at: 0,
            clock,
            revoke_at: Mutex::new(None),
            pause: Mutex::new(None),
        }
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
            if call == self.fail_at {
                return Err(BankError::Unavailable);
            }
            if call == self.advance_at {
                self.clock.set(2000);
            }
            if call == 4 {
                let revoke = self.revoke_at.lock().unwrap().take();
                if let Some((authority, permit)) = revoke {
                    authority.revoke(&permit).unwrap();
                }
                let pause = self.pause.lock().unwrap().take();
                if let Some(receiver) = pause {
                    receiver.await.map_err(|_| BankError::Unavailable)?;
                }
            }
            self.inner.get_account_balance(user, id).await
        })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.inner.find_payees(user, query)
    }
}

#[test]
fn bank_failure_at_policy_or_actual_save_is_terminal_and_does_not_repeat() {
    for fail_at in [1, 4] {
        let clock = ManualClock::new(1000);
        let mut bank = FaultBank::new(clock.clone());
        bank.fail_at = fail_at;
        let bank = Arc::new(bank);
        let app = setup_bank(bank.clone(), clock);
        let (request, permit) = authorized(&app, input());
        let first = block_on(
            app.assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
        );
        assert!(first.is_err());
        let calls = bank.calls.load(Ordering::SeqCst);
        assert_eq!(
            block_on(app.assembly.runtime().execute_mock_draft(request, &permit)),
            first
        );
        assert_eq!(bank.calls.load(Ordering::SeqCst), calls);
        assert!(app
            .audit
            .events()
            .unwrap()
            .iter()
            .any(|e| e.kind == AuditEventKind::WriteFailed));
        no_draft(&app);
    }
}

#[test]
fn expiration_or_revocation_during_bank_reads_is_checked_before_save() {
    for expired in [true, false] {
        let clock = ManualClock::new(1000);
        let mut bank = FaultBank::new(clock.clone());
        if expired {
            bank.advance_at = 4;
        }
        let bank = Arc::new(bank);
        let app = setup_bank(bank.clone(), clock);
        let (request, permit) = authorized(&app, input());
        if !expired {
            *bank.revoke_at.lock().unwrap() = Some((app.authority.clone(), permit.clone()));
        }
        assert!(block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        assert_eq!(bank.calls.load(Ordering::SeqCst), 4);
        no_draft(&app);
    }
}

#[test]
fn concurrent_duplicate_waits_for_same_result_and_saves_only_once() {
    let clock = ManualClock::new(1000);
    let bank = Arc::new(FaultBank::new(clock.clone()));
    let (sender, receiver) = oneshot::channel();
    *bank.pause.lock().unwrap() = Some(receiver);
    let app = setup_bank(bank.clone(), clock);
    let (request, permit) = authorized(&app, input());
    let (first, second, ()) = block_on(async {
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
    assert_eq!(first, second);
    assert_eq!(bank.calls.load(Ordering::SeqCst), 4);
    assert_eq!(app.store.saved_receipt_count().unwrap(), 1);
    assert_eq!(
        app.audit
            .events()
            .unwrap()
            .iter()
            .filter(|e| e.kind == AuditEventKind::ToolStarted)
            .count(),
        1
    );
}

#[test]
fn dropped_execution_is_terminal_unknown_and_retry_does_not_write() {
    let clock = ManualClock::new(1000);
    let bank = Arc::new(FaultBank::new(clock.clone()));
    let (_sender, receiver) = oneshot::channel();
    *bank.pause.lock().unwrap() = Some(receiver);
    let app = setup_bank(bank.clone(), clock);
    let (request, permit) = authorized(&app, input());
    let mut execution = Box::pin(
        app.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    );
    assert!(execution.as_mut().now_or_never().is_none());
    drop(execution);
    let result = block_on(app.assembly.runtime().execute_mock_draft(request, &permit));
    assert!(
        matches!(result, Err(RuntimeError::MockDraftAuthorization(ref reason)) if reason == "EXECUTION_INTERRUPTED_RESULT_UNKNOWN")
    );
    assert_eq!(bank.calls.load(Ordering::SeqCst), 4);
    no_draft(&app);
    assert!(app
        .audit
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditEventKind::WriteFailed));
}

struct BrokenResultTool {
    inner: CreateMockDraftTool<DemoClock>,
    status: ToolResultStatus,
    wrong_reference: bool,
    wrong_body: bool,
}
impl AgentTool for BrokenResultTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
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
            result.status = self.status;
            if self.wrong_reference {
                result.result_ref = ToolResultRef::parse("forged_result").unwrap();
            }
            if self.wrong_body {
                result.data["amount"] = json!("900.00");
            }
            Ok(result)
        })
    }
}
fn rebuild(
    app: &App,
    tool: Arc<dyn AgentTool>,
    level: SecurityLevel,
    audit: Arc<dyn AuditRecorder>,
) -> RuntimeAssembly {
    rebuild_policy(
        app,
        tool,
        Arc::new(CreateMockDraftPolicy::new(app.store.clone())),
        level,
        audit,
    )
}
fn rebuild_policy(
    app: &App,
    tool: Arc<dyn AgentTool>,
    policy: Arc<dyn DomainPolicy>,
    level: SecurityLevel,
    audit: Arc<dyn AuditRecorder>,
) -> RuntimeAssembly {
    let mut tools = ToolRegistry::new();
    tools.register(tool).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            policy,
            PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            MOCK_DRAFT_SCHEMA_VERSION,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: ToolName::parse(MOCK_DRAFT_CREATE_TOOL).unwrap(),
            tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            minimum_level: level,
        })
        .unwrap();
    RuntimeBootstrap::build_mock_draft(
        tools,
        policies,
        catalog,
        app.sessions.clone(),
        audit,
        app.authority.clone(),
    )
    .unwrap()
}

#[test]
fn failed_pending_or_unverified_result_never_reports_success_or_reexecutes() {
    for (status, wrong_reference, wrong_body) in [
        (ToolResultStatus::Failed, false, false),
        (ToolResultStatus::Processing, false, false),
        (ToolResultStatus::Succeeded, true, false),
        (ToolResultStatus::Succeeded, false, true),
    ] {
        let app = setup();
        let (request, permit) = authorized(&app, input());
        let assembly = rebuild(
            &app,
            Arc::new(BrokenResultTool {
                inner: CreateMockDraftTool::new(app.store.clone(), app.authority.clone()),
                status,
                wrong_reference,
                wrong_body,
            }),
            SecurityLevel::Green,
            app.audit.clone(),
        );
        let first = block_on(
            assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
        );
        assert!(first.is_err());
        assert_eq!(
            block_on(assembly.runtime().execute_mock_draft(request, &permit)),
            first
        );
        assert_eq!(app.store.saved_receipt_count().unwrap(), 1);
        assert!(
            block_on(app.store.read_created_for_task(&context("task_write")))
                .unwrap()
                .is_some()
        );
        assert!(!app
            .audit
            .events()
            .unwrap()
            .iter()
            .any(|e| e.kind == AuditEventKind::ToolCompleted));
    }
}

#[test]
fn stricter_central_profiles_are_not_satisfied_by_a_mock_storage_confirmation() {
    for level in [
        SecurityLevel::Yellow,
        SecurityLevel::Red,
        SecurityLevel::Critical,
    ] {
        let app = setup();
        let (request, permit) = authorized(&app, input());
        let assembly = rebuild(
            &app,
            Arc::new(CreateMockDraftTool::new(
                app.store.clone(),
                app.authority.clone(),
            )),
            level,
            app.audit.clone(),
        );
        assert!(block_on(assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        no_draft(&app);
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
fn audit_failure_before_or_after_save_is_terminal_and_never_claims_success() {
    for on in [AuditEventKind::ToolStarted, AuditEventKind::ToolCompleted] {
        let app = setup();
        let (request, permit) = authorized(&app, input());
        let audit = Arc::new(FailingAudit {
            on,
            inner: InMemoryAuditRecorder::new(),
        });
        let assembly = rebuild(
            &app,
            Arc::new(CreateMockDraftTool::new(
                app.store.clone(),
                app.authority.clone(),
            )),
            SecurityLevel::Green,
            audit.clone(),
        );
        let first = block_on(
            assembly
                .runtime()
                .execute_mock_draft(request.clone(), &permit),
        );
        assert!(matches!(first, Err(RuntimeError::Audit(_))));
        assert_eq!(
            block_on(assembly.runtime().execute_mock_draft(request, &permit)),
            first
        );
        assert_eq!(
            app.store.saved_receipt_count().unwrap(),
            usize::from(on == AuditEventKind::ToolCompleted)
        );
        assert!(audit
            .inner
            .events()
            .unwrap()
            .iter()
            .any(|e| e.kind == AuditEventKind::WriteFailed));
    }
}

#[test]
fn draft_read_enforces_owner_task_and_reports_business_expiry() {
    let clock = ManualClock::new(1000);
    let app = setup_bank(Arc::new(MockBankGateway::new()), clock.clone());
    let (request, permit) = authorized(&app, input());
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    let saved = block_on(app.store.read_created_for_task(&context("task_write")))
        .unwrap()
        .unwrap();
    let mut foreign = context("task_write");
    foreign.authenticated_user_id = UserId::parse("user_002").unwrap();
    assert!(block_on(app.store.read(&foreign, &saved.draft_id)).is_err());
    assert!(block_on(app.store.read(&context("other_task"), &saved.draft_id)).is_err());
    clock.set(1300);
    assert_eq!(
        block_on(app.store.read(&context("task_write"), &saved.draft_id))
            .unwrap()
            .status,
        DraftStatus::Expired
    );
}

fn confirm_v2(app: &App) -> (MockDraftWriteRequest, MockDraftPermit) {
    let mut value = input();
    value["amount"] = json!("300.00");
    let request = app
        .authority
        .request(
            SessionRef::parse("mock_session_001").unwrap(),
            TaskId::parse("task_write").unwrap(),
            NodeId::parse("save_draft").unwrap(),
            2,
            value,
            60,
        )
        .unwrap();
    let confirmation = app
        .authority
        .record_explicit_confirmation(&request)
        .unwrap();
    let permit = app.authority.issue(&confirmation).unwrap();
    (request, permit)
}

#[test]
fn confirming_new_version_revokes_unexecuted_old_request_and_rejects_rollback_or_conflict() {
    let app = setup();
    let (v1, old_permit) = authorized(&app, input());
    let (v2, new_permit) = confirm_v2(&app);
    assert!(block_on(app.assembly.runtime().execute_mock_draft(v1, &old_permit)).is_err());
    no_draft(&app);
    let rollback = app
        .authority
        .request(
            v2.session_ref.clone(),
            v2.task_id.clone(),
            v2.node_id.clone(),
            1,
            input(),
            60,
        )
        .unwrap();
    assert!(app
        .authority
        .record_explicit_confirmation(&rollback)
        .is_err());
    let conflict = app
        .authority
        .request(
            v2.session_ref.clone(),
            v2.task_id.clone(),
            v2.node_id.clone(),
            2,
            input(),
            60,
        )
        .unwrap();
    assert!(app
        .authority
        .record_explicit_confirmation(&conflict)
        .is_err());
    let same = app.authority.record_explicit_confirmation(&v2).unwrap();
    assert!(new_permit.same_instance(&app.authority.issue(&same).unwrap()));
    let first = block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(v2.clone(), &new_permit),
    )
    .unwrap();
    assert_eq!(
        block_on(app.assembly.runtime().execute_mock_draft(v2, &new_permit)).unwrap(),
        first
    );
    let RuntimeOutcome::Completed { results, .. } = first else {
        panic!("must complete")
    };
    assert_eq!(results[0].data["amount"], "300.00");
    assert_eq!(results[0].data["runtime_plan_version"], 2);
    assert_eq!(results[0].data["plan_version"], 1); // First business save, not a business modification.
}

#[test]
fn confirming_v2_while_v1_waits_at_bank_prevents_the_old_save() {
    let clock = ManualClock::new(1000);
    let bank = Arc::new(FaultBank::new(clock.clone()));
    let (sender, receiver) = oneshot::channel();
    *bank.pause.lock().unwrap() = Some(receiver);
    let app = setup_bank(bank, clock);
    let (v1, old_permit) = authorized(&app, input());
    let mut old_execution = Box::pin(app.assembly.runtime().execute_mock_draft(v1, &old_permit));
    assert!(old_execution.as_mut().now_or_never().is_none());
    let (v2, new_permit) = confirm_v2(&app);
    sender.send(()).unwrap();
    assert!(block_on(old_execution).is_err());
    no_draft(&app);
    block_on(app.assembly.runtime().execute_mock_draft(v2, &new_permit)).unwrap();
    assert_eq!(
        block_on(app.store.read_created_for_task(&context("task_write")))
            .unwrap()
            .unwrap()
            .details
            .unwrap()
            .amount
            .as_str(),
        "300.00"
    );
}

#[test]
fn new_confirmation_does_not_rollback_or_modify_an_already_saved_draft() {
    let app = setup();
    let (v1, old_permit) = authorized(&app, input());
    block_on(app.assembly.runtime().execute_mock_draft(v1, &old_permit)).unwrap();
    let saved = block_on(app.store.read_created_for_task(&context("task_write")))
        .unwrap()
        .unwrap();
    let (v2, new_permit) = confirm_v2(&app);
    assert!(block_on(app.assembly.runtime().execute_mock_draft(v2, &new_permit)).is_err());
    assert_eq!(
        block_on(app.store.read_created_for_task(&context("task_write")))
            .unwrap()
            .unwrap(),
        saved
    );
    assert_eq!(app.store.saved_receipt_count().unwrap(), 1);
}

#[test]
fn task_version_supersession_crosses_nodes_and_same_version_cannot_change_node() {
    let app = setup();
    let (v1, old_permit) = authorized(&app, input());
    let mut value = input();
    value["amount"] = json!("300.00");
    let v2 = app
        .authority
        .request(
            v1.session_ref.clone(),
            v1.task_id.clone(),
            NodeId::parse("new_save_node").unwrap(),
            2,
            value,
            60,
        )
        .unwrap();
    let confirmation = app.authority.record_explicit_confirmation(&v2).unwrap();
    let new_permit = app.authority.issue(&confirmation).unwrap();
    assert!(block_on(app.assembly.runtime().execute_mock_draft(v1, &old_permit)).is_err());
    no_draft(&app);
    for version in [1, 2] {
        let conflicting = app
            .authority
            .request(
                v2.session_ref.clone(),
                v2.task_id.clone(),
                NodeId::parse("third_node").unwrap(),
                version,
                input(),
                60,
            )
            .unwrap();
        assert!(app
            .authority
            .record_explicit_confirmation(&conflicting)
            .is_err());
    }
    block_on(app.assembly.runtime().execute_mock_draft(v2, &new_permit)).unwrap();
    assert_eq!(
        block_on(app.store.read_created_for_task(&context("task_write")))
            .unwrap()
            .unwrap()
            .details
            .unwrap()
            .amount
            .as_str(),
        "300.00"
    );
}

#[test]
fn unknown_session_cannot_record_confirmation_and_separate_authorities_cannot_share_permits() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    let mut unknown = request.clone();
    unknown.session_ref = SessionRef::parse("unknown_session").unwrap();
    assert!(app
        .authority
        .record_explicit_confirmation(&unknown)
        .is_err());
    let other = Arc::new(MockDraftAuthority::new(
        app.sessions.clone(),
        Arc::new(DemoClock(ManualClock::new(1000))),
    ));
    let confirmation = other.record_explicit_confirmation(&request).unwrap();
    let other_permit = other.issue(&confirmation).unwrap();
    assert!(!permit.same_instance(&other_permit));
    assert!(block_on(
        app.assembly
            .runtime()
            .execute_mock_draft(request, &other_permit)
    )
    .is_err());
    no_draft(&app);
    assert!(matches!(
        RuntimeBootstrap::build_mock_draft(
            ToolRegistry::new(),
            DomainPolicyRegistry::new(),
            ToolSecurityCatalog::new(),
            Arc::new(StaticSessionResolver::new()),
            app.audit.clone(),
            app.authority.clone()
        ),
        Err(StartupValidationError::MockDraftSessionResolverMismatch)
    ));
}

struct LiveSession {
    mode: AtomicUsize,
}
impl SessionResolver for LiveSession {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => Ok(UserId::parse("user_001").unwrap()),
            2 => Ok(UserId::parse("user_002").unwrap()),
            _ => Err(SecurityError {
                reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
                message: "revoked".into(),
            }),
        }
    }
}
fn live_assembly(
    app: &App,
    sessions: Arc<LiveSession>,
) -> (RuntimeAssembly, Arc<MockDraftAuthority>) {
    let authority = Arc::new(MockDraftAuthority::new(
        sessions.clone(),
        Arc::new(DemoClock(ManualClock::new(1000))),
    ));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CreateMockDraftTool::new(
            app.store.clone(),
            authority.clone(),
        )))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(CreateMockDraftPolicy::new(app.store.clone())),
            PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            MOCK_DRAFT_SCHEMA_VERSION,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: ToolName::parse(MOCK_DRAFT_CREATE_TOOL).unwrap(),
            tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            minimum_level: SecurityLevel::Green,
        })
        .unwrap();
    let assembly = RuntimeBootstrap::build_mock_draft(
        tools,
        policies,
        catalog,
        sessions,
        app.audit.clone(),
        authority.clone(),
    )
    .unwrap();
    (assembly, authority)
}
fn live_request(authority: &MockDraftAuthority) -> (MockDraftWriteRequest, MockDraftPermit) {
    let request = authority
        .request(
            SessionRef::parse("live_session").unwrap(),
            TaskId::parse("task_write").unwrap(),
            NodeId::parse("save_draft").unwrap(),
            1,
            input(),
            60,
        )
        .unwrap();
    let confirmation = authority.record_explicit_confirmation(&request).unwrap();
    let permit = authority.issue(&confirmation).unwrap();
    (request, permit)
}

#[test]
fn live_session_revocation_or_rebinding_invalidates_the_grant() {
    for mode in [0, 2] {
        let app = setup();
        let sessions = Arc::new(LiveSession {
            mode: AtomicUsize::new(1),
        });
        let (assembly, authority) = live_assembly(&app, sessions.clone());
        let (request, permit) = live_request(&authority);
        sessions.mode.store(mode, Ordering::SeqCst);
        assert!(block_on(assembly.runtime().execute_mock_draft(request, &permit)).is_err());
        no_draft(&app);
    }
}

#[test]
fn session_revocation_during_bank_wait_is_checked_before_save() {
    let clock = ManualClock::new(1000);
    let bank = Arc::new(FaultBank::new(clock.clone()));
    let (sender, receiver) = oneshot::channel();
    *bank.pause.lock().unwrap() = Some(receiver);
    let app = setup_bank(bank, clock);
    let sessions = Arc::new(LiveSession {
        mode: AtomicUsize::new(1),
    });
    let (assembly, authority) = live_assembly(&app, sessions.clone());
    let (request, permit) = live_request(&authority);
    let mut execution = Box::pin(assembly.runtime().execute_mock_draft(request, &permit));
    assert!(execution.as_mut().now_or_never().is_none());
    sessions.mode.store(0, Ordering::SeqCst);
    sender.send(()).unwrap();
    assert!(block_on(execution).is_err());
    no_draft(&app);
}

struct RefusingPostPolicy {
    inner: CreateMockDraftPolicy<DemoClock>,
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
                check_id: CheckId::parse("REJECT-RECEIPT").unwrap(),
                policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
                source: SourceName::parse("test_post_policy").unwrap(),
                kind: FindingKind::HardDeny,
                reason_code: ReasonCode::parse("RESULT_VERIFICATION_FAILED").unwrap(),
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
fn post_execute_denial_preserves_the_saved_draft_but_never_reports_success_or_retries() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    let assembly = rebuild_policy(
        &app,
        Arc::new(CreateMockDraftTool::new(
            app.store.clone(),
            app.authority.clone(),
        )),
        Arc::new(RefusingPostPolicy {
            inner: CreateMockDraftPolicy::new(app.store.clone()),
        }),
        SecurityLevel::Green,
        app.audit.clone(),
    );
    let first = block_on(
        assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    );
    assert_eq!(
        first,
        Err(RuntimeError::PostExecute(
            PostExecuteOutcome::VerifiedFailed
        ))
    );
    assert_eq!(
        block_on(assembly.runtime().execute_mock_draft(request, &permit)),
        first
    );
    assert_eq!(app.store.saved_receipt_count().unwrap(), 1);
    assert!(
        block_on(app.store.read_created_for_task(&context("task_write")))
            .unwrap()
            .is_some()
    );
}

struct WrongActionTool {
    inner: CreateMockDraftTool<DemoClock>,
}
impl AgentTool for WrongActionTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        definition.action = "EXECUTE_TRANSFER".into();
        definition
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.inner.execute(context, authorization, arguments)
    }
}

#[test]
fn mock_storage_permit_cannot_enable_a_funds_action_even_under_the_same_tool_name() {
    let app = setup();
    let (request, permit) = authorized(&app, input());
    let assembly = rebuild(
        &app,
        Arc::new(WrongActionTool {
            inner: CreateMockDraftTool::new(app.store.clone(), app.authority.clone()),
        }),
        SecurityLevel::Green,
        app.audit.clone(),
    );
    assert!(block_on(assembly.runtime().execute_mock_draft(request, &permit)).is_err());
    no_draft(&app);
}
