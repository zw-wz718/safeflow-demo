//! Real PostgreSQL regression tests; actual registered Kernel/Policy, never Tool execution.
#[path = "decision_cases/central_scoring.rs"]
mod central_scoring;
#[path = "decision_cases/precommit_sources.rs"]
mod precommit_sources;
#[path = "decision_cases/source_combination.rs"]
mod source_combination;
#[path = "../examples/support/decision_runtime.rs"]
mod support;
use chrono::{DateTime, Utc};
use safeflow_bank_gateway::*;
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::*;
use safeflow_storage::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

#[derive(Default)]
struct Sessions(RwLock<HashMap<SessionRef, SessionBinding>>);
impl Sessions {
    fn bind(&self, s: &SessionRef, user: &str, version: u64) {
        self.0.write().unwrap().insert(
            s.clone(),
            SessionBinding {
                user_id: UserId::parse(user).unwrap(),
                binding_version: version,
            },
        );
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, s: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(s)?.user_id)
    }
    fn resolve_binding(&self, s: &SessionRef) -> Result<SessionBinding, SecurityError> {
        self.0.read().unwrap().get(s).cloned().ok_or(SecurityError {
            reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
            message: "revoked".into(),
        })
    }
}
struct Clock(AtomicI64);
impl Clock {
    fn set(&self, t: i64) {
        self.0.store(t, Ordering::SeqCst);
    }
}
impl ConfirmationClock for Clock {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.0.load(Ordering::SeqCst)).unwrap()
    }
}
struct Bank {
    inner: MockBankGateway,
    reads: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    wait: std::sync::atomic::AtomicBool,
}
impl Bank {
    fn new() -> Self {
        Self {
            inner: MockBankGateway::new(),
            reads: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            wait: std::sync::atomic::AtomicBool::new(false),
        }
    }
}
impl BankGateway for Bank {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.wait.load(Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.get_account_balance(user, id).await
        })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        q: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.find_payees(user, q)
    }
}
fn config() -> DecisionConfig {
    DecisionConfig::new(
        Duration::from_secs(20),
        "approved-demo-bank-v1",
        support::input_contracts(),
    )
    .unwrap()
}
struct Db {
    schema: String,
    admin: PgPool,
    sql: PgPool,
    repo: PgRepository,
    sessions: Arc<Sessions>,
    clock: Arc<Clock>,
    assembly: RuntimeAssembly,
    bank: Arc<Bank>,
    tool_calls: Arc<AtomicUsize>,
    session: SessionRef,
    other: SessionRef,
    same: SessionRef,
}
impl Db {
    async fn new() -> Self {
        let url =
            std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("real PostgreSQL required; no skip");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("database connection failed"));
        let schema = format!("sf_decision_{}", uuid::Uuid::new_v4().simple());
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        let path = schema.clone();
        let sql = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |c, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(path)
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let sessions = Arc::new(Sessions::default());
        let session = SessionRef::parse("decision_backend_A").unwrap();
        let same = SessionRef::parse("decision_backend_B").unwrap();
        let other = SessionRef::parse("decision_backend_C").unwrap();
        sessions.bind(&session, "user_001", 1);
        sessions.bind(&same, "user_001", 1);
        sessions.bind(&other, "user_002", 1);
        let bank = Arc::new(Bank::new());
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let assembly = support::assemble(
            sessions.clone(),
            bank.clone(),
            SecurityLevel::Green,
            tool_calls.clone(),
        );
        Self {
            schema,
            admin,
            sql,
            repo,
            sessions,
            clock: Arc::new(Clock(AtomicI64::new(1_791_172_000_000))),
            assembly,
            bank,
            tool_calls,
            session,
            same,
            other,
        }
    }
    fn service(&self) -> DecisionService<'_> {
        DecisionService::for_trusted_backend_with_clock(
            self.repo.clone(),
            self.assembly.runtime(),
            config(),
            self.clock.clone(),
        )
        .unwrap()
    }
    async fn task(&self, tool: &str, args: Value) -> (TaskRecord, OperationRecord) {
        let service =
            PersistenceService::for_trusted_backend(self.repo.clone(), self.sessions.clone());
        let task = service
            .create_task(
                &self.session,
                AgentPlan::new(
                    new_task_id(),
                    1,
                    vec![PlanNode::Tool {
                        id: NodeId::parse("query").unwrap(),
                        tool: ToolName::parse(tool).unwrap(),
                        arguments: args,
                        depends_on: vec![],
                    }],
                )
                .unwrap(),
                1,
            )
            .await
            .unwrap();
        let op = service
            .create_operation(
                &self.session,
                &task.task_id,
                task.revision,
                new_operation_id(),
                NodeId::parse("query").unwrap(),
                1,
            )
            .await
            .unwrap();
        (task, op)
    }
    async fn balance(&self) -> (TaskRecord, OperationRecord) {
        self.task(GET_ACCOUNT_BALANCE, json!({"account_id":"account_001"}))
            .await
    }
    async fn saved(&self) -> StoredSecurityDecision {
        let (t, o) = self.balance().await;
        self.service()
            .evaluate_and_save(
                &self.session,
                &t.task_id,
                &o.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreTool,
            )
            .await
            .unwrap()
    }
    async fn count(&self, table: &str) -> i64 {
        assert!(matches!(table, "security_decisions" | "audit_events"));
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn audits(&self, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE kind=$1")
            .bind(kind)
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn finish(self) {
        assert_eq!(self.tool_calls.load(Ordering::SeqCst), 0);
        self.sql.close().await;
        self.admin
            .execute(format!("DROP SCHEMA \"{}\" CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn digest(v: &impl serde::Serialize) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(v).unwrap()))
}

#[test]
fn configuration_and_server_ids_are_bounded_and_required() {
    for ttl in [Duration::ZERO, Duration::from_nanos(1), Duration::MAX] {
        assert!(DecisionConfig::new(ttl, "v1", support::input_contracts()).is_err());
    }
    for revision in ["", "a b", "中文"] {
        assert!(
            DecisionConfig::new(Duration::from_secs(1), revision, support::input_contracts())
                .is_err()
        );
    }
    assert!(DecisionConfig::new(Duration::from_secs(1), "v1", vec![]).is_err());
    let mut duplicate = support::input_contracts();
    duplicate.push(duplicate[0].clone());
    assert!(DecisionConfig::new(Duration::from_secs(1), "v1", duplicate).is_err());
    let id = StoredDecisionId::new_server_id();
    assert_eq!(StoredDecisionId::parse(id.as_str()).unwrap(), id);
    assert!(
        StoredDecisionId::parse("stored_decision_00000000-0000-0000-0000-00000000000X").is_err()
    );
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn actual_kernel_and_bank_policy_produce_traceable_preflight_without_tool_execution() {
    let db = Db::new().await;
    let r = db.saved().await;
    assert_eq!(r.decision.outcome, SecurityOutcome::Allow);
    assert_eq!(r.evidence_kind, "PREFLIGHT_ONLY");
    assert_eq!(r.decision.policy_version.as_str(), BALANCE_POLICY_VERSION);
    assert_eq!(db.bank.reads.load(Ordering::SeqCst), 1);
    assert_eq!(r.decision_digest.as_str(), digest(&r.decision));
    assert_eq!(
        db.service()
            .verify(
                &db.session,
                &r.binding.task_id,
                &r.stored_decision_id,
                &r.binding
            )
            .await
            .unwrap(),
        r
    );
    assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 1);
    let op: String = sqlx::query_scalar("SELECT state FROM operations WHERE operation_id=$1")
        .bind(r.binding.operation_id.as_str())
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(op, "DRAFT");
    assert!(db
        .service()
        .request_precommit_evidence(&db.session, &r.binding.task_id)
        .await
        .is_err());
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn payees_and_real_denial_findings_are_preserved_without_raw_data() {
    let db = Db::new().await;
    for (tool, args, outcome) in [
        (FIND_PAYEES, json!({"query":"小李"}), SecurityOutcome::Allow),
        (
            GET_ACCOUNT_BALANCE,
            json!({"account_id":"account_other"}),
            SecurityOutcome::Blocked,
        ),
    ] {
        let (t, o) = db.task(tool, args).await;
        let r = db
            .service()
            .evaluate_and_save(
                &db.session,
                &t.task_id,
                &o.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreAuth,
            )
            .await
            .unwrap();
        assert_eq!(r.decision.outcome, outcome);
        if outcome == SecurityOutcome::Blocked {
            assert!(!r.decision.findings.is_empty());
        }
        let serialized = serde_json::to_string(&r).unwrap();
        assert!(!serialized.contains(db.session.as_str()));
        assert!(!serialized.contains("account_other"));
        assert!(!serialized.contains("小李"));
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn duplicate_and_concurrent_original_requests_do_not_reevaluate_or_extend() {
    let db = Db::new().await;
    let (t, o) = db.balance().await;
    let s = db.service();
    let id = DecisionRequestId::new_server_id();
    let (a, b) = tokio::join!(
        s.evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &id,
            SecurityPhase::PreTool
        ),
        s.evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &id,
            SecurityPhase::PreTool
        )
    );
    let r = a.unwrap();
    assert_eq!(r, b.unwrap());
    db.clock.set(r.evaluated_at_ms + 1000);
    assert_eq!(
        s.evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &id,
            SecurityPhase::PreTool
        )
        .await
        .unwrap(),
        r
    );
    assert_eq!(db.bank.reads.load(Ordering::SeqCst), 1);
    assert_eq!(db.count("security_decisions").await, 1);
    assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 1);
    drop(s);
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn same_request_other_phase_task_user_or_session_conflicts() {
    let db = Db::new().await;
    let r = db.saved().await;
    let s = db.service();
    let (t, o) = db.balance().await;
    assert!(s
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &r.request_id,
            SecurityPhase::PreTool
        )
        .await
        .is_err());
    assert!(s
        .evaluate_and_save(
            &db.session,
            &r.binding.task_id,
            &r.binding.operation_id,
            &r.request_id,
            SecurityPhase::PreAuth
        )
        .await
        .is_err());
    for session in [&db.other, &db.same] {
        assert!(s
            .read(session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .is_err());
    }
    db.sessions.bind(&db.session, "user_001", 2);
    assert!(s
        .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    assert_eq!(db.count("security_decisions").await, 1);
    drop(s);
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn new_runtime_and_connection_restore_and_disambiguate_kernel_sequence() {
    let db = Db::new().await;
    let r = db.saved().await;
    let assembly = support::assemble(
        db.sessions.clone(),
        db.bank.clone(),
        SecurityLevel::Green,
        db.tool_calls.clone(),
    );
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect(&url, &db.schema).await.unwrap();
    let service = DecisionService::for_trusted_backend_with_clock(
        repo,
        assembly.runtime(),
        config(),
        db.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        service
            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .unwrap(),
        r
    );
    let (t, o) = db.balance().await;
    let r2 = service
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool,
        )
        .await
        .unwrap();
    assert_eq!(r.decision.decision_id, r2.decision.decision_id);
    assert_ne!(r.source_instance_id, r2.source_instance_id);
    assert_ne!(r.stored_decision_id, r2.stored_decision_id);
    drop(service);
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn minimum_profile_and_deployment_or_projection_changes_reject_old_records() {
    let db = Db::new().await;
    let r = db.saved().await;
    let assembly = support::assemble(
        db.sessions.clone(),
        db.bank.clone(),
        SecurityLevel::Red,
        db.tool_calls.clone(),
    );
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        config(),
        db.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        service
            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .err(),
        Some(StorageError::DecisionAssemblyChanged)
    );
    let (t, o) = db.balance().await;
    let gated = service
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool,
        )
        .await
        .unwrap();
    assert_ne!(gated.decision.outcome, SecurityOutcome::Allow);
    assert!(!gated.decision.required_gates.is_empty());
    for change in 0..3 {
        let mut contracts = support::input_contracts();
        if change == 1 {
            contracts[0].projection_revision = "v2".into();
        }
        let cfg = DecisionConfig::new(
            Duration::from_secs(if change == 2 { 21 } else { 20 }),
            if change == 0 {
                "v2"
            } else {
                "approved-demo-bank-v1"
            },
            contracts,
        )
        .unwrap();
        let changed = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            db.assembly.runtime(),
            cfg,
            db.clock.clone(),
        )
        .unwrap();
        assert!(changed
            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .is_err());
    }
    drop(service);
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn unconfirmed_projection_unknown_tool_missing_inputs_references_and_precommit_fail_closed() {
    let db = Db::new().await;
    for (tool, args) in [
        ("unconfirmed_tool", json!({"account_id":"account_001"})),
        (GET_ACCOUNT_BALANCE, json!({})),
        (GET_ACCOUNT_BALANCE, json!({"account_id":{"$ref":"node_a"}})),
        (
            GET_ACCOUNT_BALANCE,
            json!({"account_id":"${node_a.account_id}"}),
        ),
        (
            GET_ACCOUNT_BALANCE,
            json!({"account_id":"account_001","owner_id":"user_001"}),
        ),
        (GET_ACCOUNT_BALANCE, json!({"account_id":1})),
    ] {
        let (t, o) = db.task(tool, args).await;
        assert!(db
            .service()
            .evaluate_and_save(
                &db.session,
                &t.task_id,
                &o.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreTool
            )
            .await
            .is_err());
    }
    let (t, o) = db.balance().await;
    assert!(db
        .service()
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreCommit
        )
        .await
        .is_err());
    let cfg = DecisionConfig::new(
        Duration::from_secs(20),
        "v1",
        vec![support::input_contracts().remove(1)],
    )
    .unwrap();
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    assert!(service
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool
        )
        .await
        .is_err());
    assert_eq!(db.count("security_decisions").await, 0);
    assert_eq!(db.bank.reads.load(Ordering::SeqCst), 0);
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn every_expected_binding_field_is_comparison_only() {
    let db = Db::new().await;
    let r = db.saved().await;
    for field in 0..10 {
        let mut b = r.binding.clone();
        match field {
            0 => b.user_id = UserId::parse("user_002").unwrap(),
            1 => b.task_id = new_task_id(),
            2 => b.operation_id = new_operation_id(),
            3 => b.node_id = NodeId::parse("other_node").unwrap(),
            4 => b.tool_name = ToolName::parse(FIND_PAYEES).unwrap(),
            5 => b.task_revision += 1,
            6 => b.plan_version += 1,
            7 => b.operation_revision += 1,
            8 => b.facts_version += 1,
            _ => b.arguments_hash = ArgumentsHash::parse("a".repeat(64)).unwrap(),
        }
        assert_eq!(
            db.service()
                .verify(&db.session, &r.binding.task_id, &r.stored_decision_id, &b)
                .await
                .err(),
            Some(StorageError::DecisionBindingMismatch)
        );
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn persisted_snapshot_including_sibling_changes_cannot_reuse_old_decision() {
    for mode in 0..8 {
        let db = Db::new().await;
        let r = db.saved().await;
        let query=match mode {
            0=>"UPDATE tasks SET revision=revision+1",
            1=>"UPDATE tasks SET facts_version=facts_version+1",
            2=>"UPDATE tasks SET plan=jsonb_set(plan,'{plan_version}','2'),plan_version=2",
            3=>"UPDATE operations SET revision=revision+1",
            4=>"UPDATE operations SET facts_version=facts_version+1",
            5=>"UPDATE operations SET state='CANCELLED',revision=revision+1",
            6=>"UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments,account_id}','\"account_other\"')",
            _=>"UPDATE tasks SET state='CANCELLED',revision=revision+1",
        };
        db.sql.execute(query).await.unwrap();
        assert!(db
            .service()
            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .is_err());
        let ended = db
            .service()
            .invalidate_if_stale(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .unwrap();
        assert_eq!(ended.status, StoredDecisionStatus::Invalidated);
        assert_eq!(
            ended.invalidation_reason,
            Some(DecisionInvalidationReason::SnapshotChanged)
        );
        db.finish().await;
    }
    let db = Db::new().await;
    let (t, o) = db.balance().await;
    let mut plan = t.plan.clone();
    plan.nodes.push(PlanNode::Tool {
        id: NodeId::parse("other").unwrap(),
        tool: ToolName::parse(FIND_PAYEES).unwrap(),
        arguments: json!({"query":"小李"}),
        depends_on: vec![],
    });
    sqlx::query("UPDATE tasks SET plan=$1")
        .bind(sqlx::types::Json(plan))
        .execute(&db.sql)
        .await
        .unwrap();
    let r = db
        .service()
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool,
        )
        .await
        .unwrap();
    // Sibling operation creation changes the complete snapshot even without task revision changes.
    sqlx::query("INSERT INTO operations (operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES ($1,$2,'other','DRAFT',1,1,1,$3)")
        .bind(new_operation_id().as_str()).bind(t.task_id.as_str()).bind(digest(&json!({"query":"小李"})))
        .execute(&db.sql).await.unwrap();
    assert!(db
        .service()
        .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn revoke_and_invalidate_are_terminal_and_never_revive_replays() {
    let db = Db::new().await;
    let r = db.saved().await;
    assert_eq!(
        db.service()
            .invalidate_if_stale(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .err(),
        Some(StorageError::DecisionStillCurrent)
    );
    let revoked = db
        .service()
        .revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .unwrap();
    db.clock.set(r.evaluated_at_ms + 1000);
    assert_eq!(
        db.service()
            .revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(db.audits("SECURITY_DECISION_REVOKED").await, 1);
    assert!(db
        .service()
        .evaluate_and_save(
            &db.session,
            &r.binding.task_id,
            &r.binding.operation_id,
            &r.request_id,
            r.phase
        )
        .await
        .is_err());
    let fresh = db
        .service()
        .evaluate_and_save(
            &db.session,
            &r.binding.task_id,
            &r.binding.operation_id,
            &DecisionRequestId::new_server_id(),
            r.phase,
        )
        .await
        .unwrap();
    assert_ne!(fresh.stored_decision_id, r.stored_decision_id);
    assert!(db
        .service()
        .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn expiry_equality_and_observed_error_survive_service_rebuild_and_clock_rollback() {
    let db = Db::new().await;
    let r = db.saved().await;
    db.clock.set(r.expires_at_ms);
    assert_eq!(
        db.service()
            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .err(),
        Some(StorageError::DecisionExpired)
    );
    db.clock.set(r.evaluated_at_ms + 1);
    assert!(db
        .service()
        .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    assert!(db
        .service()
        .evaluate_and_save(
            &db.session,
            &r.binding.task_id,
            &r.binding.operation_id,
            &DecisionRequestId::new_server_id(),
            r.phase
        )
        .await
        .is_err());
    db.clock.set(r.expires_at_ms);
    let expired = db
        .service()
        .invalidate_if_stale(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .unwrap();
    assert_eq!(
        expired.invalidation_reason,
        Some(DecisionInvalidationReason::Expired)
    );
    assert!(db
        .service()
        .evaluate_and_save(
            &db.session,
            &r.binding.task_id,
            &r.binding.operation_id,
            &r.request_id,
            r.phase
        )
        .await
        .is_err());
    db.finish().await;
}
async fn fault(db: &Db, table: &str, deferred: bool) {
    assert!(matches!(
        table,
        "audit_events" | "security_decisions" | "security_decision_time_fence"
    ));
    db.sql.execute("CREATE FUNCTION injected_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated failure'; END $$").await.unwrap();
    let q = if deferred {
        format!("CREATE CONSTRAINT TRIGGER reject_write AFTER INSERT OR UPDATE ON {table} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION injected_failure()")
    } else {
        format!("CREATE TRIGGER reject_write BEFORE INSERT OR UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION injected_failure()")
    };
    db.sql.execute(q.as_str()).await.unwrap();
}
async fn clear_fault(db: &Db, table: &str) {
    db.sql
        .execute(format!("DROP TRIGGER reject_write ON {table}").as_str())
        .await
        .unwrap();
    db.sql
        .execute("DROP FUNCTION injected_failure()")
        .await
        .unwrap();
}
async fn atomic_failures(table: &str, deferred: bool) {
    let db = Db::new().await;
    let (t, o) = db.balance().await;
    fault(&db, table, deferred).await;
    assert_eq!(
        db.service()
            .evaluate_and_save(
                &db.session,
                &t.task_id,
                &o.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreTool
            )
            .await
            .err(),
        Some(StorageError::Database)
    );
    assert_eq!(db.count("security_decisions").await, 0);
    assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 0);
    clear_fault(&db, table).await;
    if table != "security_decision_time_fence" {
        for invalidate in [false, true] {
            let r = db.saved().await;
            if invalidate {
                db.sql
                    .execute("UPDATE tasks SET revision=revision+1")
                    .await
                    .unwrap();
            }
            fault(&db, table, deferred).await;
            let s = db.service();
            let result = if invalidate {
                s.invalidate_if_stale(&db.session, &r.binding.task_id, &r.stored_decision_id)
                    .await
            } else {
                s.revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
                    .await
            };
            assert_eq!(result.err(), Some(StorageError::Database));
            let status: String = sqlx::query_scalar(
                "SELECT status FROM security_decisions WHERE stored_decision_id=$1",
            )
            .bind(r.stored_decision_id.as_str())
            .fetch_one(&db.sql)
            .await
            .unwrap();
            assert_eq!(status, "ACTIVE");
            assert_eq!(
                db.audits(if invalidate {
                    "SECURITY_DECISION_INVALIDATED"
                } else {
                    "SECURITY_DECISION_REVOKED"
                })
                .await,
                0
            );
            clear_fault(&db, table).await;
        }
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn immediate_audit_and_record_errors_roll_back_all_decision_effects() {
    for table in ["audit_events", "security_decisions"] {
        atomic_failures(table, false).await;
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn delayed_commit_audit_and_record_errors_roll_back_all_decision_effects() {
    for table in ["audit_events", "security_decisions"] {
        atomic_failures(table, true).await;
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn persistent_time_fence_failure_rejects_without_decision_or_audit() {
    atomic_failures("security_decision_time_fence", false).await;
    atomic_failures("security_decision_time_fence", true).await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn audit_trigger_changes_to_task_operation_arguments_and_facts_roll_back() {
    for cleanup in [false, true] {
        for mutation in ["UPDATE tasks SET revision=revision+1;","UPDATE operations SET revision=revision+1;","UPDATE operations SET facts_version=facts_version+1;","UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments,account_id}','\"account_other\"');"] {
        let db=Db::new().await;let r=if cleanup {Some(db.saved().await)}else{None};let(t,o)=if let Some(r)=&r {(r.binding.task_id.clone(),r.binding.operation_id.clone())}else{let(t,o)=db.balance().await;(t.task_id,o.operation_id)};
        db.sql.execute(format!("CREATE FUNCTION mutate_snapshot() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN {mutation} RETURN NEW; END $$").as_str()).await.unwrap();
        db.sql.execute("CREATE TRIGGER mutate_snapshot BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind LIKE 'SECURITY_DECISION_%') EXECUTE FUNCTION mutate_snapshot()").await.unwrap();
        let s=db.service();let result=if let Some(r)=&r {s.revoke(&db.session,&t,&r.stored_decision_id).await}else{s.evaluate_and_save(&db.session,&t,&o,&DecisionRequestId::new_server_id(),SecurityPhase::PreTool).await};assert!(result.is_err());
        if let Some(r)=r {let status:String=sqlx::query_scalar("SELECT status FROM security_decisions WHERE stored_decision_id=$1").bind(r.stored_decision_id.as_str()).fetch_one(&db.sql).await.unwrap();assert_eq!(status,"ACTIVE");assert_eq!(db.audits("SECURITY_DECISION_REVOKED").await,0);}
        else {assert_eq!(db.count("security_decisions").await,0);assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await,0);}
        drop(s);db.finish().await;
    }
    }
}
struct ObservingClock {
    source: Arc<Clock>,
    sessions: Arc<Sessions>,
    session: SessionRef,
    calls: AtomicUsize,
    change_at: usize,
    rollback: bool,
}
impl ConfirmationClock for ObservingClock {
    fn now(&self) -> DateTime<Utc> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.change_at {
            if self.rollback {
                self.source.set(self.source.0.load(Ordering::SeqCst) - 1);
            } else {
                self.sessions.bind(&self.session, "user_001", 2);
            }
        }
        self.source.now()
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn every_time_observation_rechecks_session_and_persists_clock_even_on_rollback() {
    for action in 0..4 {
        let db = Db::new().await;
        let make_clock = |at, rollback| {
            Arc::new(ObservingClock {
                source: db.clock.clone(),
                sessions: db.sessions.clone(),
                session: db.session.clone(),
                calls: AtomicUsize::new(0),
                change_at: at,
                rollback,
            })
        };
        let initial = make_clock(usize::MAX, false);
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            db.assembly.runtime(),
            config(),
            initial.clone(),
        )
        .unwrap();
        let r = db.saved().await;
        if action == 3 {
            db.sql
                .execute("UPDATE tasks SET revision=revision+1")
                .await
                .unwrap();
        }
        match action {
            0 => {
                service
                    .evaluate_and_save(
                        &db.session,
                        &r.binding.task_id,
                        &r.binding.operation_id,
                        &DecisionRequestId::new_server_id(),
                        r.phase,
                    )
                    .await
                    .unwrap();
            }
            1 => {
                service
                    .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
                    .await
                    .unwrap();
            }
            2 => {
                service
                    .revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
                    .await
                    .unwrap();
            }
            _ => {
                service
                    .invalidate_if_stale(&db.session, &r.binding.task_id, &r.stored_decision_id)
                    .await
                    .unwrap();
            }
        }
        let observations = initial.calls.load(Ordering::SeqCst);
        assert!(observations >= 2);
        drop(service);
        for rollback in [false, true] {
            for at in 1..=observations {
                db.sessions.bind(&db.session, "user_001", 1);
                db.clock.set(db.clock.0.load(Ordering::SeqCst) + 1000);
                let r = db.saved().await;
                if action == 3 {
                    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE task_id=$1")
                        .bind(r.binding.task_id.as_str())
                        .execute(&db.sql)
                        .await
                        .unwrap();
                }
                let before = db.count("security_decisions").await;
                let audit_before = db.count("audit_events").await;
                let clock = make_clock(at, rollback);
                let service = DecisionService::for_trusted_backend_with_clock(
                    db.repo.clone(),
                    db.assembly.runtime(),
                    config(),
                    clock.clone(),
                )
                .unwrap();
                let result = match action {
                    0 => {
                        service
                            .evaluate_and_save(
                                &db.session,
                                &r.binding.task_id,
                                &r.binding.operation_id,
                                &DecisionRequestId::new_server_id(),
                                r.phase,
                            )
                            .await
                    }
                    1 => {
                        service
                            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
                            .await
                    }
                    2 => {
                        service
                            .revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
                            .await
                    }
                    _ => {
                        service
                            .invalidate_if_stale(
                                &db.session,
                                &r.binding.task_id,
                                &r.stored_decision_id,
                            )
                            .await
                    }
                };
                assert!(
                    result.is_err(),
                    "action {action} observation {at} rollback {rollback}"
                );
                assert_eq!(db.count("security_decisions").await, before);
                assert_eq!(db.count("audit_events").await, audit_before);
                let status: String = sqlx::query_scalar(
                    "SELECT status FROM security_decisions WHERE stored_decision_id=$1",
                )
                .bind(r.stored_decision_id.as_str())
                .fetch_one(&db.sql)
                .await
                .unwrap();
                assert_eq!(status, "ACTIVE");
            }
        }
        db.finish().await;
    }
}
async fn wait_blocked(db: &Db, pid: i32) {
    for _ in 0..300 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",
        )
        .bind(pid)
        .fetch_one(&db.admin)
        .await
        .unwrap();
        if blocked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no lock wait observed");
}
async fn blocker(db: &Db, audit: bool) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut tx = db.sql.begin().await.unwrap();
    let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    if audit {
        sqlx::query("SELECT pg_advisory_xact_lock(778899)")
            .execute(&mut *tx)
            .await
            .unwrap();
        db.sql.execute("CREATE FUNCTION wait_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(778899); RETURN NEW; END $$").await.unwrap();
        db.sql.execute("CREATE TRIGGER wait_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind LIKE 'SECURITY_DECISION_%') EXECUTE FUNCTION wait_audit()").await.unwrap();
    } else {
        sqlx::query("SELECT task_id FROM tasks FOR UPDATE")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    (tx, pid)
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn task_and_audit_lock_waits_recheck_session_expiry_and_database_snapshot() {
    for audit in [false, true] {
        for mode in 0..3 {
            let db = Db::new().await;
            let r = db.saved().await;
            let (mut lock, pid) = blocker(&db, audit).await;
            let s = db.service();
            let request = async {
                if audit {
                    s.evaluate_and_save(
                        &db.session,
                        &r.binding.task_id,
                        &r.binding.operation_id,
                        &DecisionRequestId::new_server_id(),
                        r.phase,
                    )
                    .await
                } else {
                    s.read(&db.session, &r.binding.task_id, &r.stored_decision_id)
                        .await
                }
            };
            let (result, _) = tokio::join!(request, async {
                wait_blocked(&db, pid).await;
                match mode {
                    0 => db.sessions.bind(&db.session, "user_001", 2),
                    1 => db.clock.set(r.expires_at_ms),
                    _ => {
                        if audit {
                            db.sql
                                .execute("UPDATE operations SET revision=revision+1")
                                .await
                                .unwrap();
                        } else {
                            sqlx::query("UPDATE tasks SET revision=revision+1")
                                .execute(&mut *lock)
                                .await
                                .unwrap();
                        }
                    }
                };
                lock.commit().await.unwrap();
            });
            assert!(result.is_err(), "audit {audit} mode {mode}");
            assert_eq!(db.count("security_decisions").await, 1);
            drop(s);
            db.finish().await;
        }
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn real_policy_query_wait_rechecks_session_time_and_operations_before_saving() {
    for mode in 0..3 {
        let db = Db::new().await;
        let (t, o) = db.balance().await;
        db.bank.wait.store(true, Ordering::SeqCst);
        let service = db.service();
        let request = DecisionRequestId::new_server_id();
        let (result, _) = tokio::join!(
            service.evaluate_and_save(
                &db.session,
                &t.task_id,
                &o.operation_id,
                &request,
                SecurityPhase::PreTool
            ),
            async {
                db.bank.entered.notified().await;
                match mode {
                    0 => db.sessions.bind(&db.session, "user_001", 2),
                    1 => db.clock.set(1_791_172_020_000),
                    _ => {
                        db.sql.execute("UPDATE operations SET revision=revision+1,facts_version=facts_version+1").await.unwrap();
                    }
                }
                db.bank.release.notify_one();
            }
        );
        assert!(result.is_err());
        assert_eq!(db.count("security_decisions").await, 0);
        assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 0);
        drop(service);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn cancel_at_policy_and_audit_waits_leaves_no_partial_records_or_revocations() {
    for action in 0..4 {
        let db = Db::new().await;
        let r = db.saved().await;
        if action == 3 {
            db.sql
                .execute("UPDATE tasks SET revision=revision+1")
                .await
                .unwrap();
        }
        let service = db.service();
        let id = DecisionRequestId::new_server_id();
        if action == 0 {
            db.bank.wait.store(true, Ordering::SeqCst);
            let mut future = Box::pin(service.evaluate_and_save(
                &db.session,
                &r.binding.task_id,
                &r.binding.operation_id,
                &id,
                r.phase,
            ));
            tokio::select! {result=&mut future=>panic!("completed early {result:?}"),_=db.bank.entered.notified()=>{}}
            drop(future);
        } else {
            let (lock, pid) = blocker(&db, true).await;
            let mut future = Box::pin(async {
                match action {
                    1 => {
                        service
                            .evaluate_and_save(
                                &db.session,
                                &r.binding.task_id,
                                &r.binding.operation_id,
                                &id,
                                r.phase,
                            )
                            .await
                    }
                    2 => {
                        service
                            .revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
                            .await
                    }
                    _ => {
                        service
                            .invalidate_if_stale(
                                &db.session,
                                &r.binding.task_id,
                                &r.stored_decision_id,
                            )
                            .await
                    }
                }
            });
            tokio::select! {result=&mut future=>panic!("completed early {result:?}"),_=wait_blocked(&db,pid)=>{}}
            drop(future);
            lock.rollback().await.unwrap();
        }
        // Synchronize with SQLx's rollback before reading final committed state.
        sqlx::query("SELECT task_id FROM tasks FOR UPDATE")
            .execute(&db.sql)
            .await
            .unwrap();
        assert_eq!(db.count("security_decisions").await, 1);
        assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 1);
        assert_eq!(db.audits("SECURITY_DECISION_REVOKED").await, 0);
        assert_eq!(db.audits("SECURITY_DECISION_INVALIDATED").await, 0);
        let status: String = sqlx::query_scalar("SELECT status FROM security_decisions")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(status, "ACTIVE");
        drop(service);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn observed_time_survives_audit_rollback_and_parallel_observation_without_deadlock() {
    let db = Db::new().await;
    let r = db.saved().await;
    fault(&db, "audit_events", false).await;
    db.clock.set(r.evaluated_at_ms + 1000);
    assert!(db
        .service()
        .revoke(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    clear_fault(&db, "audit_events").await;
    db.clock.set(r.evaluated_at_ms);
    assert!(db
        .service()
        .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    db.clock.set(r.evaluated_at_ms + 1000);
    let r2 = db.saved().await;
    let (lock, pid) = blocker(&db, true).await;
    let service = db.service();
    let request = DecisionRequestId::new_server_id();
    let (result, _) = tokio::join!(
        service.evaluate_and_save(
            &db.session,
            &r.binding.task_id,
            &r.binding.operation_id,
            &request,
            r.phase
        ),
        async {
            wait_blocked(&db, pid).await;
            db.clock.set(r.expires_at_ms + 1000);
            let parallel = DecisionService::for_trusted_backend_with_clock(
                db.repo.clone(),
                db.assembly.runtime(),
                config(),
                db.clock.clone(),
            )
            .unwrap();
            let read = tokio::time::timeout(
                Duration::from_secs(3),
                parallel.read(&db.session, &r2.binding.task_id, &r2.stored_decision_id),
            )
            .await
            .unwrap();
            assert!(read.is_err());
            lock.rollback().await.unwrap();
        }
    );
    assert!(result.is_err());
    drop(service);
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn record_integrity_and_database_constraints_reject_corruption_and_impossible_states() {
    let db = Db::new().await;
    let r = db.saved().await;
    for sql in [
        "UPDATE security_decisions SET phase='PRE_COMMIT'",
        "UPDATE security_decisions SET evidence_kind='READY_TO_EXECUTE'",
        "UPDATE security_decisions SET expires_at_ms=evaluated_at_ms",
        "UPDATE security_decisions SET status='REVOKED'",
        "UPDATE security_decisions SET invalidation_reason='EXPIRED'",
        "UPDATE security_decisions SET decision='[]'::jsonb",
    ] {
        assert!(db.sql.execute(sql).await.is_err());
    }
    sqlx::query("UPDATE security_decisions SET decision_digest=$1")
        .bind("0".repeat(64))
        .execute(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        db.service()
            .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
            .await
            .err(),
        Some(StorageError::CorruptRecord)
    );
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn migration_six_preserves_one_through_five_checksums_and_prior_records() {
    let db = Db::new().await;
    let (t, o) = db.balance().await;
    let confirmations = ConfirmationService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.sessions.clone(),
        ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
        db.clock.clone(),
    );
    let c = confirmations
        .prepare(&db.session, &t.task_id, &o.operation_id)
        .await
        .unwrap();
    let c = confirmations
        .record_explicit_confirmation(&db.session, &t.task_id, &c.confirmation_ref, &c.binding)
        .await
        .unwrap();
    // Seed first, then remove every post-v5 object in dependency order.
    db.sql.execute(
        "DROP TRIGGER payment_registration_operation_no_execution ON operations;
         DROP FUNCTION payment_registration_operation_immutable();
         DROP TABLE payment_execution_links;
         DROP FUNCTION payment_execution_link_immutable();
         DROP TABLE payment_preparations;
         DROP FUNCTION payment_preparation_immutable();
         DROP TRIGGER preserve_permit_execution_scope ON execution_permits;
         DROP FUNCTION preserve_permit_execution_scope();
         ALTER TABLE execution_permits DROP COLUMN execution_scope;
         DROP TRIGGER public_audit_enqueue ON audit_events;
         DROP FUNCTION enqueue_public_audit();
         DROP TABLE public_audit_outbox;
         DROP FUNCTION protect_public_audit_outbox();
         DROP TABLE public_execution_requests;
         DROP FUNCTION preserve_public_execution_request();
         DROP TABLE mock_draft_lifecycle_claims;
         DROP TABLE mock_draft_lifecycle_displays;
         ALTER TABLE mock_draft_lifecycle_tasks DROP CONSTRAINT lifecycle_active_action_fk,
             DROP CONSTRAINT lifecycle_last_success_fk;
         DROP TABLE mock_draft_lifecycle_actions;
         DROP TABLE mock_draft_lifecycle_plans;
         DROP TABLE mock_draft_lifecycle_tasks;
         ALTER TABLE server_confirmations DROP COLUMN lifecycle_anchor;
         DROP TABLE mock_draft_execution_claims;
         DROP TABLE planning_confirmation_displays;
         DROP TABLE planning_confirmation_requests;
         DROP TABLE runtime_read_continuations;
         DROP TABLE runtime_read_claims;
         DROP TABLE security_decisions;
         DROP TABLE security_decision_time_fence;
         ALTER TABLE execution_permits DROP CONSTRAINT execution_permits_authentication_check,
             DROP COLUMN storage_version, DROP COLUMN authentication_state, DROP COLUMN required_gates,
             ALTER COLUMN auth_grant_ref SET NOT NULL;
         ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
         ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
             kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
                 'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
                 'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED',
                 'PERMIT_SAVED','PERMIT_CONSUMED','PERMIT_REVOKED',
                 'SIM_AUTH_CHALLENGE_CREATED','SIM_AUTH_FAILED','SIM_AUTH_VERIFIED',
                 'SIM_AUTH_GRANT_ISSUED','SIM_AUTH_CHALLENGE_REVOKED','SIM_AUTH_GRANT_REVOKED'));
         DELETE FROM _sqlx_migrations WHERE version>5;"
    ).await.unwrap();
    let checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=5 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(checksums.len(), 5);
    let before = PersistenceService::for_trusted_backend(db.repo.clone(), db.sessions.clone())
        .recover(&db.session, &t.task_id)
        .await
        .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    assert_eq!(versions, (1..=21).collect::<Vec<_>>());
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=5 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(after, checksums);
    assert_eq!(
        confirmations
            .verify(&db.session, &t.task_id, &c.confirmation_ref, &c.binding)
            .await
            .unwrap(),
        c
    );
    assert_eq!(
        PersistenceService::for_trusted_backend(db.repo.clone(), db.sessions.clone())
            .recover(&db.session, &t.task_id)
            .await
            .unwrap(),
        before
    );
    db.finish().await;
}
struct DraftClock;
impl safeflow_transfer_integration::Clock for DraftClock {
    fn now(&self) -> u64 {
        1_791_172_000
    }
}
impl MockDraftClock for DraftClock {
    fn now_seconds(&self) -> u64 {
        1_791_172_000
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn existing_write_tool_preflight_is_generic_but_never_saves_business_or_issues_permit() {
    use safeflow_transfer_integration::runtime_tools::*;
    let db = Db::new().await;
    let store = Arc::new(MockDraftStore::new(db.bank.clone(), DraftClock, 300).unwrap());
    let authority = Arc::new(MockDraftAuthority::new(
        db.sessions.clone(),
        Arc::new(DraftClock),
    ));
    for minimum in [
        SecurityLevel::Green,
        SecurityLevel::Red,
        SecurityLevel::Critical,
    ] {
        let mut tools = ToolRegistry::new();
        let mut policies = DomainPolicyRegistry::new();
        let mut catalog = ToolSecurityCatalog::new();
        let tool = Arc::new(CreateMockDraftTool::new(store.clone(), authority.clone()));
        let definition = tool.definition();
        tools
            .register(support::counted(tool, db.tool_calls.clone()))
            .unwrap();
        policies
            .register(
                Arc::new(CreateMockDraftPolicy::new(store.clone())),
                PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
                MOCK_DRAFT_SCHEMA_VERSION,
            )
            .unwrap();
        catalog
            .register(ToolSecurityProfile {
                tool_name: definition.name.clone(),
                tool_schema_version: definition.schema_version,
                policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
                minimum_level: minimum,
            })
            .unwrap();
        let assembly = RuntimeBootstrap::build(
            tools,
            policies,
            catalog,
            db.sessions.clone(),
            Arc::new(InMemoryAuditRecorder::new()),
        )
        .unwrap();
        let cfg = DecisionConfig::new(
            Duration::from_secs(20),
            "existing-mock-draft-v1",
            vec![PreflightInputContract {
                tool_name: definition.name,
                tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
                policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
                projection_revision: "existing-create-direct-v1".into(),
            }],
        )
        .unwrap();
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            assembly.runtime(),
            cfg,
            db.clock.clone(),
        )
        .unwrap();
        let(t,o)=db.task(MOCK_DRAFT_CREATE_TOOL,json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null})).await;
        let context = ExecutionContext {
            task_id: t.task_id.clone(),
            node_id: o.node_id.clone(),
            operation_id: o.operation_id.clone(),
            authenticated_user_id: UserId::parse("user_001").unwrap(),
            plan_version: 1,
            idempotency_key: None,
        };
        store.find_candidates(&context, "小李").await.unwrap();
        for phase in [SecurityPhase::PreTool, SecurityPhase::PreAuth] {
            let r = service
                .evaluate_and_save(
                    &db.session,
                    &t.task_id,
                    &o.operation_id,
                    &DecisionRequestId::new_server_id(),
                    phase,
                )
                .await
                .unwrap();
            assert_eq!(
                r.decision.outcome,
                if minimum == SecurityLevel::Green {
                    SecurityOutcome::ReadyToConfirm
                } else if minimum == SecurityLevel::Red {
                    SecurityOutcome::WaitingAuth
                } else {
                    SecurityOutcome::Blocked
                }
            );
            assert!(r
                .decision
                .required_gates
                .contains(&SecurityGate::ExplicitConfirmation));
            let confirmations = ConfirmationService::for_trusted_backend_with_clock(
                db.repo.clone(),
                db.sessions.clone(),
                ConfirmationConfig::new(Duration::from_secs(10)).unwrap(),
                db.clock.clone(),
            );
            let prepared = confirmations
                .prepare(&db.session, &t.task_id, &o.operation_id)
                .await
                .unwrap();
            let confirmed = confirmations
                .record_explicit_confirmation(
                    &db.session,
                    &t.task_id,
                    &prepared.confirmation_ref,
                    &prepared.binding,
                )
                .await
                .unwrap();
            let receipt = service
                .verify_with_confirmation(
                    &db.session,
                    &t.task_id,
                    &r.stored_decision_id,
                    &confirmed.confirmation_ref,
                    &r.binding,
                )
                .await
                .unwrap();
            assert_eq!(receipt.preflight_decision(), &r);
            assert_eq!(receipt.preflight_decision().phase, phase);
            assert_eq!(
                receipt.preflight_decision().decision.required_gates,
                if minimum == SecurityLevel::Green {
                    vec![SecurityGate::ExplicitConfirmation]
                } else {
                    vec![
                        SecurityGate::ExplicitConfirmation,
                        SecurityGate::StrongAuthentication,
                    ]
                }
            );
            assert_eq!(store.saved_receipt_count().unwrap(), 0);
            assert_eq!(db.tool_calls.load(Ordering::SeqCst), 0);
        }
        assert!(store
            .read_created_for_task(&context)
            .await
            .unwrap()
            .is_none());
        assert_eq!(store.saved_receipt_count().unwrap(), 0);
        assert!(service
            .request_precommit_evidence(&db.session, &t.task_id)
            .await
            .is_err());
        let permits = PermitStore::for_trusted_backend_with_clock(
            db.repo.clone(),
            db.sessions.clone(),
            PermitConfig::new(Duration::from_secs(20)).unwrap(),
            db.clock.clone(),
        );
        assert_eq!(
            permits
                .request_issuance(&db.session, &t.task_id)
                .await
                .err(),
            Some(StorageError::PermitIssuanceUnavailable)
        );
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn fence_is_sampled_after_lock_wait_and_expired_observation_is_durable() {
    let db = Db::new().await;
    let r = db.saved().await;
    let mut lock = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT scope FROM security_decision_time_fence FOR UPDATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let service = db.service();
    let (result, _) = tokio::join!(
        service.read(&db.session, &r.binding.task_id, &r.stored_decision_id),
        async {
            wait_blocked(&db, pid).await;
            db.clock.set(r.expires_at_ms);
            lock.commit().await.unwrap();
        }
    );
    assert_eq!(result.err(), Some(StorageError::DecisionExpired));
    db.clock.set(r.evaluated_at_ms + 1);
    assert!(db
        .service()
        .read(&db.session, &r.binding.task_id, &r.stored_decision_id)
        .await
        .is_err());
    drop(service);
    db.finish().await;
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn bounded_inputs_facts_and_reconciliation_states_never_create_records() {
    let db = Db::new().await;
    let (t, o) = db
        .task(GET_ACCOUNT_BALANCE, json!({"account_id":"a".repeat(4097)}))
        .await;
    assert!(db
        .service()
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool
        )
        .await
        .is_err());
    let (t, o) = db.balance().await;
    sqlx::query("UPDATE tasks SET facts_version=4294967296 WHERE task_id=$1")
        .bind(t.task_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET facts_version=4294967296 WHERE task_id=$1")
        .bind(t.task_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert!(db
        .service()
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool
        )
        .await
        .is_err());
    for state in [
        "SUBMITTED",
        "PROCESSING",
        "SUCCEEDED",
        "FAILED",
        "CANCELLED",
    ] {
        let (t, o) = db.balance().await;
        sqlx::query("UPDATE operations SET state=$1,business_operation_ref='unconnected_reference' WHERE operation_id=$2").bind(state).bind(o.operation_id.as_str()).execute(&db.sql).await.unwrap();
        assert!(db
            .service()
            .evaluate_and_save(
                &db.session,
                &t.task_id,
                &o.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreTool
            )
            .await
            .is_err());
    }
    assert_eq!(db.count("security_decisions").await, 0);
    assert_eq!(db.bank.reads.load(Ordering::SeqCst), 0);
    db.finish().await;
}
struct DefinitionObservedTool {
    inner: Arc<dyn AgentTool>,
    description: Arc<RwLock<String>>,
}
impl AgentTool for DefinitionObservedTool {
    fn definition(&self) -> ToolDefinition {
        let mut d = self.inner.definition();
        d.description = self.description.read().unwrap().clone();
        d
    }
    fn execute<'a>(
        &'a self,
        c: &'a ExecutionContext,
        a: ToolExecutionAuthorization<'a>,
        v: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.inner.execute(c, a, v)
    }
}
struct DefinitionClock {
    source: Arc<Clock>,
    description: Arc<RwLock<String>>,
    calls: AtomicUsize,
    change_at: usize,
}
impl ConfirmationClock for DefinitionClock {
    fn now(&self) -> DateTime<Utc> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.change_at {
            *self.description.write().unwrap() = "changed registered definition".into();
        }
        self.source.now()
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn registered_definition_change_at_last_time_observation_rolls_back_record_and_audit() {
    let db = Db::new().await;
    let description = Arc::new(RwLock::new("approved registered definition".into()));
    let mut tools = ToolRegistry::new();
    let mut policies = DomainPolicyRegistry::new();
    let mut catalog = ToolSecurityCatalog::new();
    tools
        .register(support::counted(
            Arc::new(DefinitionObservedTool {
                inner: Arc::new(GetAccountBalanceTool::new(db.bank.clone())),
                description: description.clone(),
            }),
            db.tool_calls.clone(),
        ))
        .unwrap();
    policies
        .register(
            Arc::new(GetAccountBalancePolicy::new(db.bank.clone())),
            PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
            BANK_QUERY_SCHEMA_VERSION,
        )
        .unwrap();
    catalog
        .register(ToolSecurityProfile {
            tool_name: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
            minimum_level: SecurityLevel::Green,
        })
        .unwrap();
    let assembly = RuntimeBootstrap::build(
        tools,
        policies,
        catalog,
        db.sessions.clone(),
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap();
    let make_clock = |at| {
        Arc::new(DefinitionClock {
            source: db.clock.clone(),
            description: description.clone(),
            calls: AtomicUsize::new(0),
            change_at: at,
        })
    };
    let initial = make_clock(usize::MAX);
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        config(),
        initial.clone(),
    )
    .unwrap();
    let (t, o) = db.balance().await;
    service
        .evaluate_and_save(
            &db.session,
            &t.task_id,
            &o.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool,
        )
        .await
        .unwrap();
    let observations = initial.calls.load(Ordering::SeqCst);
    assert!(observations >= 5);
    drop(service);
    let clock = make_clock(observations);
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        config(),
        clock,
    )
    .unwrap();
    let (t, o) = db.balance().await;
    assert_eq!(
        service
            .evaluate_and_save(
                &db.session,
                &t.task_id,
                &o.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreTool
            )
            .await
            .err(),
        Some(StorageError::DecisionAssemblyChanged)
    );
    assert_eq!(db.count("security_decisions").await, 1);
    assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 1);
    drop(service);
    db.finish().await;
}
