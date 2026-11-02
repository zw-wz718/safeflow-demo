use safeflow_bank_gateway::{
    BankError, BankGateway, FindPayeesOutput, GetAccountBalanceOutput, GetAccountBalancePolicy,
    GetAccountBalanceTool, GET_ACCOUNT_BALANCE,
};
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{
    AuditError, AuditEvent, AuditEventKind, AuditRecorder, DomainPolicyRegistry,
    InMemoryAuditRecorder, Runtime, RuntimeAssembly, RuntimeBootstrap, RuntimeError,
    SessionBinding, SessionResolver, ToolRegistry, ToolSecurityCatalog, ToolSecurityProfile,
};
use safeflow_storage::{
    new_operation_id, new_task_id, CheckpointChange, OperationChange, PersistenceService,
    PersistentAuditKind, PgRepository, RecoveryDirective, RecoveryPoint, RuntimeRecoveryError,
    RuntimeRecoveryService, StorageError, WaitingTaskVersion,
};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, RwLock,
};
use std::time::Duration;
use tokio::sync::Notify;

#[path = "runtime_continuation_cases/mod.rs"]
mod runtime_continuation_cases;
#[path = "runtime_execution_cases/mod.rs"]
mod runtime_execution_cases;

fn session() -> SessionRef {
    SessionRef::parse("runtime_session_001").unwrap()
}
fn node(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}
fn plan() -> AgentPlan {
    AgentPlan::new(
        new_task_id(),
        2,
        vec![
            PlanNode::Tool {
                id: node("read"),
                tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
                arguments: json!({"account_id":"account_001"}),
                depends_on: vec![],
            },
            PlanNode::UserDecision {
                id: node("choose"),
                depends_on: vec![node("read")],
            },
        ],
    )
    .unwrap()
}

struct Sessions(RwLock<(u64, Option<UserId>)>);
impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self(RwLock::new((
            1,
            Some(UserId::parse("user_001").unwrap()),
        ))))
    }
    fn set(&self, user: Option<&str>) {
        let mut binding = self.0.write().unwrap();
        binding.0 += 1;
        binding.1 = user.map(|user| UserId::parse(user).unwrap());
    }
}
impl SessionResolver for Sessions {
    fn resolve_binding(&self, reference: &SessionRef) -> Result<SessionBinding, SecurityError> {
        if reference == &session() {
            let binding = self.0.read().unwrap();
            Ok(SessionBinding {
                user_id: binding.1.clone().ok_or(SecurityError {
                    reason_code: ReasonCode::parse("SESSION_INVALID").unwrap(),
                    message: "test session invalid".into(),
                })?,
                binding_version: binding.0,
            })
        } else {
            Ok(SessionBinding {
                user_id: self.resolve(reference)?,
                binding_version: 1,
            })
        }
    }

    fn resolve(&self, reference: &SessionRef) -> Result<UserId, SecurityError> {
        let user = if reference == &session() {
            self.0.read().unwrap().1.clone()
        } else if reference.as_str() == "other_session" {
            Some(UserId::parse("user_002").unwrap())
        } else {
            None
        };
        user.ok_or(SecurityError {
            reason_code: ReasonCode::parse("SESSION_INVALID").unwrap(),
            message: "test session is invalid".into(),
        })
    }
}

#[derive(Default)]
struct Gate {
    enabled: AtomicBool,
    reached: Notify,
    release: Notify,
}
impl Gate {
    fn armed() -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(true),
            ..Self::default()
        })
    }
    async fn wait(&self) {
        if self.enabled.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

#[derive(Clone, Copy, Default)]
enum ToolMode {
    #[default]
    Normal,
    Error,
    FailedResult,
    MismatchedResult,
}
struct CountedTool {
    inner: GetAccountBalanceTool,
    definition: Mutex<ToolDefinition>,
    calls: AtomicUsize,
    mode: ToolMode,
    gate: Option<Arc<Gate>>,
}
impl AgentTool for CountedTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.lock().unwrap().clone()
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.wait().await;
            }
            if matches!(self.mode, ToolMode::Error) {
                return Err(ToolError {
                    reason_code: ReasonCode::parse("INJECTED_TOOL_FAILURE").unwrap(),
                    message: "test failure".into(),
                });
            }
            let mut result = self
                .inner
                .execute(context, authorization, arguments)
                .await?;
            match self.mode {
                ToolMode::FailedResult => result.status = ToolResultStatus::Failed,
                ToolMode::MismatchedResult => {
                    result.operation_id = OperationId::parse("wrong_operation").unwrap()
                }
                _ => {}
            }
            Ok(result)
        })
    }
}

fn denial() -> SecurityFinding {
    SecurityFinding {
        check_id: CheckId::parse("COMMON-01").unwrap(),
        policy_version: PolicyVersion::parse("test-policy-v1").unwrap(),
        source: SourceName::parse("test_policy").unwrap(),
        kind: FindingKind::HardDeny,
        reason_code: ReasonCode::parse("INJECTED_POLICY_DENIAL").unwrap(),
        evidence_refs: vec![],
        observed_value: None,
        event_ids: vec![],
        dedup_key: None,
        window_start_ms: None,
        window_end_ms: None,
    }
}
struct Policy {
    inner: GetAccountBalancePolicy,
    versions: Mutex<Vec<u32>>,
    deny: AtomicBool,
    post_deny: bool,
    gate: Option<Arc<Gate>>,
}
impl DomainPolicy for Policy {
    fn tool_name(&self) -> &ToolName {
        self.inner.tool_name()
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            self.versions.lock().unwrap().push(context.facts_version);
            if let Some(gate) = &self.gate {
                gate.wait().await;
            }
            if self.deny.load(Ordering::SeqCst) {
                return Ok(vec![denial()]);
            }
            self.inner.evaluate(context).await
        })
    }
    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            if self.post_deny {
                return Ok(vec![denial()]);
            }
            self.inner.verify_execution(context).await
        })
    }
}
struct Audit {
    recorder: InMemoryAuditRecorder,
    fail: Option<AuditEventKind>,
}
impl AuditRecorder for Audit {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        if self.fail == Some(event.kind) {
            return Err(AuditError::Unavailable);
        }
        self.recorder.record(event)
    }
}
#[derive(Default)]
struct Options {
    mode: ToolMode,
    post_deny: bool,
    audit_fail: Option<AuditEventKind>,
    tool_gate: Option<Arc<Gate>>,
    policy_gate: Option<Arc<Gate>>,
    schema: Option<u32>,
    policy_version: Option<&'static str>,
    side_effect: bool,
    empty: bool,
    bank_unavailable: bool,
}

struct UnavailableBank;
impl BankGateway for UnavailableBank {
    fn get_account_balance<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async { Err(BankError::Unavailable) })
    }
    fn find_payees<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async { Err(BankError::Unavailable) })
    }
}
struct Rig {
    assembly: RuntimeAssembly,
    tool: Arc<CountedTool>,
    policy: Arc<Policy>,
    audit: Arc<Audit>,
}
impl Rig {
    fn new(sessions: Arc<Sessions>, options: Options) -> Self {
        Self::with_resolver(sessions, options)
    }
    fn with_resolver(sessions: Arc<dyn SessionResolver>, options: Options) -> Self {
        let bank: Arc<dyn BankGateway> = if options.bank_unavailable {
            Arc::new(UnavailableBank)
        } else {
            Arc::new(MockBankGateway::new())
        };
        let inner = GetAccountBalanceTool::new(bank.clone());
        let mut definition = inner.definition();
        definition.schema_version = options.schema.unwrap_or(1);
        definition.has_side_effect = options.side_effect;
        let tool = Arc::new(CountedTool {
            inner,
            definition: Mutex::new(definition),
            calls: AtomicUsize::new(0),
            mode: options.mode,
            gate: options.tool_gate,
        });
        let policy = Arc::new(Policy {
            inner: GetAccountBalancePolicy::new(bank),
            versions: Mutex::new(vec![]),
            deny: AtomicBool::new(false),
            post_deny: options.post_deny,
            gate: options.policy_gate,
        });
        let audit = Arc::new(Audit {
            recorder: InMemoryAuditRecorder::new(),
            fail: options.audit_fail,
        });
        let mut tools = ToolRegistry::new();
        let mut policies = DomainPolicyRegistry::new();
        let mut catalog = ToolSecurityCatalog::new();
        if !options.empty {
            tools.register(tool.clone()).unwrap();
            let version =
                PolicyVersion::parse(options.policy_version.unwrap_or("balance-policy-v1"))
                    .unwrap();
            policies
                .register(policy.clone(), version.clone(), options.schema.unwrap_or(1))
                .unwrap();
            catalog
                .register(ToolSecurityProfile {
                    tool_name: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
                    tool_schema_version: options.schema.unwrap_or(1),
                    policy_version: version,
                    minimum_level: SecurityLevel::Green,
                })
                .unwrap();
        }
        let assembly =
            RuntimeBootstrap::build(tools, policies, catalog, sessions, audit.clone()).unwrap();
        Self {
            assembly,
            tool,
            policy,
            audit,
        }
    }
    fn runtime(&self) -> &Runtime {
        self.assembly.runtime()
    }
    fn calls(&self) -> usize {
        self.tool.calls.load(Ordering::SeqCst)
    }
}

struct Db {
    namespace: String,
    url: String,
    admin: PgPool,
    scoped: PgPool,
    repo: PgRepository,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("real PostgreSQL configuration required; tests were not silently skipped");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("isolated database unavailable");
        let namespace = format!("sf_step4b_test_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {namespace}").as_str())
            .await
            .unwrap();
        let search_path = namespace.clone();
        let scoped = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |conn, _| {
                let path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let repo = PgRepository::connect(&url, &namespace).await.unwrap();
        repo.migrate().await.unwrap();
        Self {
            namespace,
            url,
            admin,
            scoped,
            repo,
        }
    }
    fn service<'a>(&self, rig: &'a Rig) -> RuntimeRecoveryService<'a> {
        RuntimeRecoveryService::for_trusted_backend(self.repo.clone(), rig.runtime())
    }
    fn raw(&self, sessions: Arc<Sessions>) -> PersistenceService {
        PersistenceService::for_trusted_backend(self.repo.clone(), sessions)
    }
    async fn finish(self) {
        self.repo.close().await;
        self.scoped.close().await;
        assert!(self.namespace.starts_with("sf_step4b_test_") && self.namespace.len() == 47);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.namespace).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
async fn verified_wait_uses_actual_facts_and_only_successfully_verified_results() {
    for mode in [
        ToolMode::Normal,
        ToolMode::Error,
        ToolMode::FailedResult,
        ToolMode::MismatchedResult,
    ] {
        let rig = Rig::new(
            Sessions::new(),
            Options {
                mode,
                ..Options::default()
            },
        );
        let review = rig
            .runtime()
            .review_read_only_plan(plan(), session(), 7)
            .await
            .unwrap();
        let receipt = rig.runtime().execute_reviewed_read_only_wait(&review).await;
        if matches!(mode, ToolMode::Normal) {
            let receipt = receipt.unwrap();
            assert_eq!(receipt.completed_nodes(), &[node("read")]);
            assert_eq!(receipt.facts_version(), 7);
            assert_eq!(receipt.node_id(), &node("choose"));
        } else {
            assert!(receipt.is_err());
        }
        assert_eq!(rig.calls(), 1);
        assert_eq!(*rig.policy.versions.lock().unwrap(), vec![7, 7]);
    }
    let rig = Rig::new(Sessions::new(), Options::default());
    for facts in [0, u64::from(u32::MAX) + 1] {
        assert!(rig
            .runtime()
            .review_read_only_plan(plan(), session(), facts)
            .await
            .is_err());
    }
    assert_eq!(rig.calls(), 0);
}

#[tokio::test]
async fn mixed_or_write_only_plan_is_rejected_before_any_query() {
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let write = Arc::new(WriteFixture {
        name: ToolName::parse("write_fixture").unwrap(),
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    let mut policies = DomainPolicyRegistry::new();
    let mut catalog = ToolSecurityCatalog::new();
    tools.register(rig.tool.clone()).unwrap();
    tools.register(write.clone()).unwrap();
    for policy in [
        rig.policy.clone() as Arc<dyn DomainPolicy>,
        write.clone() as Arc<dyn DomainPolicy>,
    ] {
        let name = policy.tool_name().clone();
        let version = PolicyVersion::parse("fixture-policy-v1").unwrap();
        policies.register(policy, version.clone(), 1).unwrap();
        catalog
            .register(ToolSecurityProfile {
                tool_name: name,
                tool_schema_version: 1,
                policy_version: version,
                minimum_level: SecurityLevel::Green,
            })
            .unwrap();
    }
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, sessions, rig.audit.clone()).unwrap();
    let mut mixed = plan();
    mixed.nodes.push(PlanNode::Tool {
        id: node("write"),
        tool: write.name.clone(),
        arguments: json!({}),
        depends_on: vec![node("choose")],
    });
    let mut only_write = plan();
    let PlanNode::Tool { tool, .. } = &mut only_write.nodes[0] else {
        unreachable!()
    };
    *tool = write.name.clone();
    for p in [mixed, only_write] {
        assert!(matches!(
            assembly
                .runtime()
                .review_read_only_plan(p, session(), 7)
                .await,
            Err(RuntimeError::WriteToolRequiresLaterStage(_))
        ));
    }
    assert_eq!(rig.calls(), 0);
    assert_eq!(write.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resolver_without_versioned_binding_cannot_use_persistent_read_only_path() {
    struct LegacyResolver;
    impl SessionResolver for LegacyResolver {
        fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
            Ok(UserId::parse("user_001").unwrap())
        }
    }
    let rig = Rig::with_resolver(Arc::new(LegacyResolver), Options::default());
    assert!(rig
        .runtime()
        .review_read_only_plan(plan(), session(), 7)
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
}

struct WriteFixture {
    name: ToolName,
    calls: AtomicUsize,
}
impl AgentTool for WriteFixture {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            "Declared write fixture",
            json!({"type":"object"}),
            1,
            true,
            "WRITE_FIXTURE",
        )
        .unwrap()
    }
    fn execute<'a>(
        &'a self,
        _: &'a ExecutionContext,
        _: ToolExecutionAuthorization<'a>,
        _: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ToolError {
                reason_code: ReasonCode::parse("MUST_NOT_EXECUTE").unwrap(),
                message: "write fixture must never run".into(),
            })
        })
    }
}
impl DomainPolicy for WriteFixture {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async { Ok(vec![]) })
    }
}

#[tokio::test]
async fn registration_changes_during_tool_await_do_not_create_a_verified_receipt() {
    let gate = Gate::armed();
    let rig = Rig::new(
        Sessions::new(),
        Options {
            tool_gate: Some(gate.clone()),
            ..Options::default()
        },
    );
    let review = rig
        .runtime()
        .review_read_only_plan(plan(), session(), 7)
        .await
        .unwrap();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            rig.runtime().execute_reviewed_read_only_wait(&review),
            async {
                gate.reached.notified().await;
                rig.tool.definition.lock().unwrap().schema_version = 2;
                gate.release.notify_one();
            }
        )
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert_eq!(rig.calls(), 1);
}

#[tokio::test]
async fn identity_loss_or_rebinding_during_policy_await_prevents_tool_call() {
    for user in [None, Some("user_002"), Some("user_001")] {
        let sessions = Sessions::new();
        let gate = Gate::armed();
        let rig = Rig::new(
            sessions.clone(),
            Options {
                policy_gate: Some(gate.clone()),
                ..Options::default()
            },
        );
        let review = rig
            .runtime()
            .review_read_only_plan(plan(), session(), 7)
            .await
            .unwrap();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                rig.runtime().execute_reviewed_read_only_wait(&review),
                async {
                    gate.reached.notified().await;
                    sessions.set(user);
                    gate.release.notify_one();
                }
            )
        })
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(rig.calls(), 0);
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn actual_runtime_wait_survives_reconstruction_and_repeated_restore_executes_zero_tools() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let p = plan();
    let saved = db
        .service(&rig)
        .execute_until_wait(session(), p.clone(), 7)
        .await
        .unwrap();
    assert_eq!(rig.calls(), 1);
    assert_eq!(saved.task.state, TaskState::WaitingUser);
    assert_eq!(saved.task.revision, 2);
    assert_eq!(saved.task.plan, p);
    assert_eq!(saved.task.completed_nodes, vec![node("read")]);
    assert_eq!(saved.revalidated_facts_version, 7);
    let before = db
        .raw(sessions)
        .recover(&session(), &saved.task.task_id)
        .await
        .unwrap();
    assert_eq!(before.audit.len(), 2);
    assert_eq!(before.audit[1].kind, PersistentAuditKind::CheckpointSaved);
    assert!(rig
        .audit
        .recorder
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == AuditEventKind::ToolCompleted));
    db.repo.close().await;
    let fresh_repo = PgRepository::connect(&db.url, &db.namespace).await.unwrap();
    fresh_repo.migrate().await.unwrap();
    let fresh_sessions = Sessions::new();
    let fresh = Rig::new(fresh_sessions.clone(), Options::default());
    let service = RuntimeRecoveryService::for_trusted_backend(fresh_repo.clone(), fresh.runtime());
    for _ in 0..2 {
        assert_eq!(
            service
                .recover_wait(
                    session(),
                    &saved.task.task_id,
                    WaitingTaskVersion::of(&saved.task)
                )
                .await
                .unwrap(),
            saved
        );
    }
    assert_eq!(fresh.calls(), 0);
    assert_eq!(*fresh.policy.versions.lock().unwrap(), vec![7, 7, 7, 7]);
    assert_eq!(
        PersistenceService::for_trusted_backend(fresh_repo.clone(), fresh_sessions)
            .recover(&session(), &saved.task.task_id)
            .await
            .unwrap(),
        before
    );
    fresh_repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn concurrent_duplicate_initial_requests_do_not_execute_tools_twice() {
    let db = Db::new().await;
    let rig = Rig::new(Sessions::new(), Options::default());
    let p = plan();
    let service = db.service(&rig);
    let (a, b) = tokio::join!(
        service.execute_until_wait(session(), p.clone(), 7),
        service.execute_until_wait(session(), p, 7)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(a == Err(StorageError::IdConflict.into()) || b == Err(StorageError::IdConflict.into()));
    assert_eq!(rig.calls(), 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn initial_runtime_task_creation_wait_rechecks_review_before_any_commit() {
    let db = Db::new().await;
    for owner in [None, Some("user_001"), Some("user_002")] {
        let sessions = Sessions::new();
        let rig = Arc::new(Rig::new(sessions.clone(), Options::default()));
        let mut lock = db.scoped.begin().await.unwrap();
        lock.execute("LOCK TABLE tasks IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let repo = db.repo.clone();
        let executing = rig.clone();
        let pending = tokio::spawn(async move {
            RuntimeRecoveryService::for_trusted_backend(repo, executing.runtime())
                .execute_until_wait(session(), plan(), 7)
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation=$1::regclass AND NOT granted)")
                    .bind(format!("{}.tasks", db.namespace)).fetch_one(&db.admin).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("Runtime creation did not enter SQL lock wait");
        sessions.set(owner);
        lock.commit().await.unwrap();
        assert!(pending.await.unwrap().is_err());
        assert_eq!(rig.calls(), 0);
        for table in ["tasks", "operations", "audit_events"] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&db.scoped)
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn current_identity_and_expected_versions_are_required_on_restore() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let service = db.service(&rig);
    let saved = service
        .execute_until_wait(session(), plan(), 7)
        .await
        .unwrap();
    let expected = WaitingTaskVersion::of(&saved.task);
    assert_eq!(
        service
            .recover_wait(
                SessionRef::parse("other_session").unwrap(),
                &saved.task.task_id,
                expected
            )
            .await,
        Err(StorageError::NotFound.into())
    );
    assert!(service
        .recover_wait(
            SessionRef::parse("missing").unwrap(),
            &saved.task.task_id,
            expected
        )
        .await
        .is_err());
    for version in [
        WaitingTaskVersion {
            plan_version: 1,
            ..expected
        },
        WaitingTaskVersion {
            facts_version: 1,
            ..expected
        },
        WaitingTaskVersion {
            task_revision: 1,
            ..expected
        },
    ] {
        assert!(service
            .recover_wait(session(), &saved.task.task_id, version)
            .await
            .is_err());
    }
    for user in [None, Some("user_002")] {
        sessions.set(user);
        assert!(service
            .recover_wait(session(), &saved.task.task_id, expected)
            .await
            .is_err());
    }
    assert_eq!(rig.calls(), 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn handwritten_progress_cannot_masquerade_as_a_runtime_wait() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let raw = db.raw(sessions);
    let task = raw.create_task(&session(), plan(), 7).await.unwrap();
    let task = raw
        .save_checkpoint(
            &session(),
            &task.task_id,
            CheckpointChange {
                expected_revision: 1,
                event: Some(TaskEvent::AwaitUser),
                checkpoint: RecoveryPoint::AtNode {
                    node_id: node("choose"),
                },
                completed_nodes: vec![node("read")],
                facts_version: 7,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        db.service(&rig)
            .recover_wait(session(), &task.task_id, WaitingTaskVersion::of(&task))
            .await,
        Err(RuntimeRecoveryError::ReviewRequired(_))
    ));
    assert_eq!(rig.calls(), 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn changed_registrations_or_new_write_tool_require_review_without_execution() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let original = Rig::new(sessions.clone(), Options::default());
    let saved = db
        .service(&original)
        .execute_until_wait(session(), plan(), 7)
        .await
        .unwrap();
    for options in [
        Options {
            schema: Some(2),
            ..Options::default()
        },
        Options {
            policy_version: Some("balance-policy-v2"),
            ..Options::default()
        },
        Options {
            side_effect: true,
            ..Options::default()
        },
        Options {
            empty: true,
            ..Options::default()
        },
    ] {
        let rig = Rig::new(sessions.clone(), options);
        assert!(db
            .service(&rig)
            .recover_wait(
                session(),
                &saved.task.task_id,
                WaitingTaskVersion::of(&saved.task)
            )
            .await
            .is_err());
        assert_eq!(rig.calls(), 0);
    }
    original.policy.deny.store(true, Ordering::SeqCst);
    assert!(db
        .service(&original)
        .recover_wait(
            session(),
            &saved.task.task_id,
            WaitingTaskVersion::of(&saved.task)
        )
        .await
        .is_err());
    assert_eq!(original.calls(), 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn tool_result_post_verification_and_runtime_audit_failures_never_save_success_progress() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    for options in [
        Options {
            mode: ToolMode::Error,
            ..Options::default()
        },
        Options {
            mode: ToolMode::FailedResult,
            ..Options::default()
        },
        Options {
            mode: ToolMode::MismatchedResult,
            ..Options::default()
        },
        Options {
            post_deny: true,
            ..Options::default()
        },
        Options {
            audit_fail: Some(AuditEventKind::ToolCompleted),
            ..Options::default()
        },
        Options {
            audit_fail: Some(AuditEventKind::TaskPaused),
            ..Options::default()
        },
    ] {
        let rig = Rig::new(sessions.clone(), options);
        let p = plan();
        assert!(db
            .service(&rig)
            .execute_until_wait(session(), p.clone(), 7)
            .await
            .is_err());
        let snapshot = db
            .raw(sessions.clone())
            .recover(&session(), &p.task_id)
            .await
            .unwrap();
        assert_eq!(snapshot.task.state, TaskState::Running);
        assert_eq!(snapshot.task.revision, 1);
        assert!(snapshot.task.completed_nodes.is_empty());
        assert_eq!(snapshot.audit.len(), 1);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn bank_policy_denial_and_unavailable_database_do_not_report_a_saved_wait() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let mut p = plan();
    let PlanNode::Tool { arguments, .. } = &mut p.nodes[0] else {
        unreachable!()
    };
    *arguments = json!({"account_id":"account_not_owned"});
    assert!(db
        .service(&rig)
        .execute_until_wait(session(), p.clone(), 7)
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    assert!(db
        .raw(sessions)
        .recover(&session(), &p.task_id)
        .await
        .unwrap()
        .task
        .completed_nodes
        .is_empty());
    let unavailable = Rig::new(
        Sessions::new(),
        Options {
            bank_unavailable: true,
            ..Options::default()
        },
    );
    assert!(db
        .service(&unavailable)
        .execute_until_wait(session(), plan(), 7)
        .await
        .is_err());
    assert_eq!(unavailable.calls(), 0);
    db.repo.close().await;
    assert_eq!(
        db.service(&rig)
            .execute_until_wait(session(), plan(), 7)
            .await,
        Err(StorageError::Database.into())
    );
    assert_eq!(rig.calls(), 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn checkpoint_update_audit_and_commit_failures_roll_back_wait_and_evidence() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    for fault in ["update", "audit", "deferred_audit"] {
        let rig = Rig::new(sessions.clone(), Options::default());
        let p = plan();
        if fault == "update" {
            db.scoped.execute("CREATE FUNCTION reject_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state='WAITING_USER' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$").await.unwrap();
            db.scoped.execute("CREATE TRIGGER reject_wait BEFORE UPDATE ON tasks FOR EACH ROW EXECUTE FUNCTION reject_wait()").await.unwrap();
        } else {
            db.scoped.execute("CREATE FUNCTION reject_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='CHECKPOINT_SAVED' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$").await.unwrap();
            db.scoped.execute(if fault == "audit" { "CREATE TRIGGER reject_wait BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_wait()" }
                else { "CREATE CONSTRAINT TRIGGER reject_wait AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_wait()" }).await.unwrap();
        }
        assert_eq!(
            db.service(&rig)
                .execute_until_wait(session(), p.clone(), 7)
                .await,
            Err(StorageError::Database.into())
        );
        let snapshot = db
            .raw(sessions.clone())
            .recover(&session(), &p.task_id)
            .await
            .unwrap();
        assert_eq!(rig.calls(), 1);
        assert_eq!(snapshot.task.state, TaskState::Running);
        assert_eq!(snapshot.task.revision, 1);
        assert!(snapshot.task.completed_nodes.is_empty());
        assert_eq!(snapshot.audit.len(), 1);
        db.scoped
            .execute(
                format!(
                    "DROP TRIGGER reject_wait ON {}",
                    if fault == "update" {
                        "tasks"
                    } else {
                        "audit_events"
                    }
                )
                .as_str(),
            )
            .await
            .unwrap();
        db.scoped
            .execute("DROP FUNCTION reject_wait()")
            .await
            .unwrap();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn session_changes_during_tool_await_cannot_persist_verified_progress() {
    let db = Db::new().await;
    for user in [None, Some("user_002")] {
        let sessions = Sessions::new();
        let gate = Gate::armed();
        let rig = Rig::new(
            sessions.clone(),
            Options {
                tool_gate: Some(gate.clone()),
                ..Options::default()
            },
        );
        let p = plan();
        let service = db.service(&rig);
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(service.execute_until_wait(session(), p.clone(), 7), async {
                gate.reached.notified().await;
                if user == Some("user_001") {
                    sessions.set(None);
                }
                sessions.set(user);
                gate.release.notify_one();
            })
        })
        .await
        .unwrap();
        assert!(result.is_err());
        sessions.set(Some("user_001"));
        let snapshot = db
            .raw(sessions)
            .recover(&session(), &p.task_id)
            .await
            .unwrap();
        assert!(snapshot.task.completed_nodes.is_empty());
        assert_eq!(snapshot.task.revision, 1);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn plan_or_fact_version_changes_while_tool_awaits_are_not_overwritten() {
    let db = Db::new().await;
    for change_plan in [true, false] {
        let sessions = Sessions::new();
        let gate = Gate::armed();
        let rig = Rig::new(
            sessions.clone(),
            Options {
                tool_gate: Some(gate.clone()),
                ..Options::default()
            },
        );
        let p = plan();
        let service = db.service(&rig);
        let raw = db.raw(sessions);
        let (result, updated) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(service.execute_until_wait(session(), p.clone(), 7), async {
                gate.reached.notified().await;
                let updated = if change_plan {
                    let mut next = p.clone();
                    next.plan_version += 1;
                    raw.replace_plan(&session(), &p.task_id, 1, next, 8)
                        .await
                        .unwrap()
                } else {
                    raw.save_checkpoint(
                        &session(),
                        &p.task_id,
                        CheckpointChange {
                            expected_revision: 1,
                            event: None,
                            checkpoint: RecoveryPoint::AtNode {
                                node_id: node("read"),
                            },
                            completed_nodes: vec![],
                            facts_version: 8,
                        },
                    )
                    .await
                    .unwrap()
                };
                gate.release.notify_one();
                updated
            })
        })
        .await
        .unwrap();
        assert_eq!(result, Err(StorageError::RevisionConflict.into()));
        assert_eq!(
            raw.recover(&session(), &p.task_id).await.unwrap().task,
            updated
        );
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn identity_is_rechecked_after_waiting_for_the_database_task_lock() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let gate = Gate::armed();
    let rig = Rig::new(
        sessions.clone(),
        Options {
            tool_gate: Some(gate.clone()),
            ..Options::default()
        },
    );
    let service = db.service(&rig);
    let p = plan();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(service.execute_until_wait(session(), p.clone(), 7), async {
            gate.reached.notified().await;
            let mut tx = db.scoped.begin().await.unwrap();
            sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE").bind(p.task_id.as_str()).execute(&mut *tx).await.unwrap();
            gate.release.notify_one();
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE%'").fetch_one(&db.admin).await.unwrap();
                if waiting > 0 { break; } tokio::time::sleep(Duration::from_millis(5)).await;
            }
            sessions.set(None); sessions.set(Some("user_001")); tx.commit().await.unwrap();
        })
    }).await.unwrap();
    assert!(result.is_err());
    sessions.set(Some("user_001"));
    let snapshot = db
        .raw(sessions)
        .recover(&session(), &p.task_id)
        .await
        .unwrap();
    assert_eq!(snapshot.task.state, TaskState::Running);
    assert_eq!(snapshot.task.revision, 1);
    assert!(snapshot.task.completed_nodes.is_empty());
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn plan_changes_during_restore_policy_await_require_new_review() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let initial = Rig::new(sessions.clone(), Options::default());
    let saved = db
        .service(&initial)
        .execute_until_wait(session(), plan(), 7)
        .await
        .unwrap();
    let gate = Gate::armed();
    let fresh = Rig::new(
        sessions.clone(),
        Options {
            policy_gate: Some(gate.clone()),
            ..Options::default()
        },
    );
    let service = db.service(&fresh);
    let raw = db.raw(sessions);
    let (result, updated) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            service.recover_wait(
                session(),
                &saved.task.task_id,
                WaitingTaskVersion::of(&saved.task)
            ),
            async {
                gate.reached.notified().await;
                let mut p = saved.task.plan.clone();
                p.plan_version += 1;
                let updated = raw
                    .replace_plan(&session(), &saved.task.task_id, saved.task.revision, p, 8)
                    .await
                    .unwrap();
                gate.release.notify_one();
                updated
            }
        )
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert_eq!(fresh.calls(), 0);
    assert_eq!(
        raw.recover(&session(), &saved.task.task_id)
            .await
            .unwrap()
            .task,
        updated
    );
    assert!(service
        .recover_wait(
            session(),
            &saved.task.task_id,
            WaitingTaskVersion::of(&updated)
        )
        .await
        .is_err());
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn corrupted_plan_or_checkpoint_cannot_pass_runtime_restore() {
    let db = Db::new().await;
    let rig = Rig::new(Sessions::new(), Options::default());
    for column in ["plan", "checkpoint", "completed_nodes"] {
        let saved = db
            .service(&rig)
            .execute_until_wait(session(), plan(), 7)
            .await
            .unwrap();
        let statement = match column {
            "plan" => "UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments,account_id}','\"different_account\"') WHERE task_id=$1",
            "checkpoint" => "UPDATE tasks SET checkpoint='{\"kind\":\"AT_NODE\",\"node_id\":\"missing\"}' WHERE task_id=$1",
            _ => "UPDATE tasks SET completed_nodes='[]' WHERE task_id=$1",
        };
        sqlx::query(statement)
            .bind(saved.task.task_id.as_str())
            .execute(&db.scoped)
            .await
            .unwrap();
        assert!(db
            .service(&rig)
            .recover_wait(
                session(),
                &saved.task.task_id,
                WaitingTaskVersion::of(&saved.task)
            )
            .await
            .is_err());
    }
    assert_eq!(rig.calls(), 3);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn non_waiting_terminal_and_unknown_operations_never_reenter_execution() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let raw = db.raw(sessions);
    let task = raw.create_task(&session(), plan(), 7).await.unwrap();
    let service = db.service(&rig);
    assert!(matches!(
        service
            .recover_wait(session(), &task.task_id, WaitingTaskVersion::of(&task))
            .await,
        Err(RuntimeRecoveryError::NotWaiting { .. })
    ));
    let task = raw
        .save_checkpoint(
            &session(),
            &task.task_id,
            CheckpointChange {
                expected_revision: 1,
                event: Some(TaskEvent::Cancel),
                checkpoint: RecoveryPoint::Finished,
                completed_nodes: vec![],
                facts_version: 7,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        service
            .recover_wait(session(), &task.task_id, WaitingTaskVersion::of(&task))
            .await,
        Err(RuntimeRecoveryError::NotWaiting {
            directive: RecoveryDirective::Finished {
                state: TaskState::Cancelled
            }
        })
    ));
    assert_eq!(
        service.execute_until_wait(session(), task.plan, 7).await,
        Err(StorageError::IdConflict.into())
    );
    let task = raw.create_task(&session(), plan(), 7).await.unwrap();
    let mut op = raw
        .create_operation(
            &session(),
            &task.task_id,
            1,
            new_operation_id(),
            node("read"),
            2,
        )
        .await
        .unwrap();
    let reference = BusinessOperationRef::parse("original_unknown_operation").unwrap();
    for (event, business_ref) in [
        (OperationEvent::GatesSatisfied, None),
        (OperationEvent::SubmissionRecorded, Some(reference.clone())),
        (OperationEvent::OutcomeUnknown, Some(reference.clone())),
    ] {
        op = raw
            .record_operation_event(
                &session(),
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: op.revision,
                    event,
                    facts_version: op.facts_version,
                    business_operation_ref: business_ref,
                },
            )
            .await
            .unwrap();
        if matches!(
            op.state,
            OperationState::Submitted | OperationState::Processing
        ) {
            let snapshot = raw.recover(&session(), &task.task_id).await.unwrap();
            assert_eq!(
                service
                    .recover_wait(
                        session(),
                        &task.task_id,
                        WaitingTaskVersion::of(&snapshot.task)
                    )
                    .await,
                Err(RuntimeRecoveryError::NotWaiting {
                    directive: RecoveryDirective::ReconcileOriginal {
                        operation_id: op.operation_id.clone(),
                        business_operation_ref: reference.clone()
                    }
                })
            );
        }
    }
    assert_eq!(rig.calls(), 0);
    db.finish().await;
}
