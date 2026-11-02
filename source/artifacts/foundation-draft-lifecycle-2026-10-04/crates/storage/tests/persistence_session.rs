//! Real PostgreSQL guards for existing persistence commands, never business execution.
use safeflow_bank_gateway::GET_ACCOUNT_BALANCE;
use safeflow_contracts::*;
use safeflow_runtime::{SessionBinding, SessionResolver, StaticSessionResolver};
use safeflow_storage::*;
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

fn session() -> SessionRef {
    SessionRef::parse("persistence_private_session").unwrap()
}
fn node() -> NodeId {
    NodeId::parse("read").unwrap()
}
fn user(id: &str) -> UserId {
    UserId::parse(id).unwrap()
}
fn session_error() -> SecurityError {
    SecurityError {
        reason_code: ReasonCode::parse("SESSION_INVALID").unwrap(),
        message: "private resolver diagnostic never exposed".into(),
    }
}
struct Sessions {
    binding: RwLock<Option<SessionBinding>>,
    calls: AtomicUsize,
    revoke_at: AtomicUsize,
}
impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            binding: RwLock::new(Some(SessionBinding {
                user_id: user("user_001"),
                binding_version: 1,
            })),
            calls: AtomicUsize::new(0),
            revoke_at: AtomicUsize::new(0),
        })
    }
    fn change(&self, owner: Option<&str>) {
        let mut old = self.binding.write().unwrap();
        let version = old.as_ref().map_or(2, |b| b.binding_version + 1);
        *old = owner.map(|id| SessionBinding {
            user_id: user(id),
            binding_version: version,
        });
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        panic!("versioned persistence must never fall back to resolve")
    }
    fn resolve_binding(&self, _: &SessionRef) -> Result<SessionBinding, SecurityError> {
        let point = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if point == self.revoke_at.load(Ordering::SeqCst) {
            self.change(None);
        }
        self.binding
            .read()
            .unwrap()
            .clone()
            .ok_or_else(session_error)
    }
}

struct Db {
    schema: String,
    admin: PgPool,
    scoped: PgPool,
    repo: PgRepository,
    healthy: PersistenceService,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("real PostgreSQL configuration required, no silent skips");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("isolated PostgreSQL unavailable"));
        let schema = format!("sf_session_test_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let path = schema.clone();
        let scoped = PgPoolOptions::new()
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
            .unwrap_or_else(|_| panic!("test schema unavailable"));
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        let mut sessions = StaticSessionResolver::new();
        sessions.insert(session(), user("user_001"));
        let healthy = PersistenceService::for_trusted_backend(repo.clone(), Arc::new(sessions));
        Self {
            schema,
            admin,
            scoped,
            repo,
            healthy,
        }
    }
    fn service(&self, sessions: Arc<dyn SessionResolver>) -> PersistenceService {
        PersistenceService::for_trusted_backend(self.repo.clone(), sessions)
    }
    async fn snapshot(&self) -> Value {
        sqlx::query_scalar("SELECT jsonb_build_object('tasks',COALESCE((SELECT jsonb_agg(to_jsonb(t) ORDER BY task_id) FROM tasks t),'[]'::jsonb),'operations',COALESCE((SELECT jsonb_agg(to_jsonb(o) ORDER BY operation_id) FROM operations o),'[]'::jsonb),'audit',COALESCE((SELECT jsonb_agg(to_jsonb(a) ORDER BY sequence) FROM audit_events a),'[]'::jsonb))")
            .fetch_one(&self.scoped).await.unwrap()
    }
    async fn wait_lock(&self, table: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation=$1::regclass AND NOT granted)")
                    .bind(format!("{}.{}", self.schema, table))
                    .fetch_one(&self.admin).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("request did not enter real SQL lock wait");
    }
    async fn finish(self) {
        self.repo.close().await;
        self.scoped.close().await;
        assert!(self.schema.starts_with("sf_session_test_") && self.schema.len() == 48);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn plan() -> AgentPlan {
    AgentPlan::new(
        new_task_id(),
        1,
        vec![
            PlanNode::Tool {
                id: node(),
                tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
                arguments: json!({"account_id":"account_001"}),
                depends_on: vec![],
            },
            PlanNode::UserDecision {
                id: NodeId::parse("choose").unwrap(),
                depends_on: vec![node()],
            },
        ],
    )
    .unwrap()
}
#[derive(Clone, Copy, Debug)]
enum Attempt {
    Create,
    Recover,
    Checkpoint,
    Replace,
    CreateOperation,
    Event,
    RepeatCreate,
    SamePlan,
    RepeatOperation,
    RejectCheckpoint,
    RejectPlan,
    RejectOperation,
    RejectEvent,
}
const SIX: [Attempt; 6] = [
    Attempt::Create,
    Attempt::Recover,
    Attempt::Checkpoint,
    Attempt::Replace,
    Attempt::CreateOperation,
    Attempt::Event,
];
struct Fixture {
    task: TaskRecord,
    operation_id: OperationId,
    fresh_plan: AgentPlan,
}
impl Fixture {
    async fn new(db: &Db, attempt: Attempt) -> Self {
        let mut task = db.healthy.create_task(&session(), plan(), 1).await.unwrap();
        // Existing operation/event data, not a new business plugin or funds receipt.
        let operation_id = if matches!(
            attempt,
            Attempt::Event | Attempt::RepeatOperation | Attempt::RejectEvent
        ) {
            let op = db
                .healthy
                .create_operation(
                    &session(),
                    &task.task_id,
                    task.revision,
                    new_operation_id(),
                    node(),
                    1,
                )
                .await
                .unwrap();
            task = db
                .healthy
                .recover(&session(), &task.task_id)
                .await
                .unwrap()
                .task;
            op.operation_id
        } else {
            new_operation_id()
        };
        Self {
            task,
            operation_id,
            fresh_plan: plan(),
        }
    }
    async fn run(
        &self,
        service: &PersistenceService,
        attempt: Attempt,
    ) -> Result<(), StorageError> {
        let task = &self.task;
        match attempt {
            Attempt::Create => service
                .create_task(&session(), self.fresh_plan.clone(), 1)
                .await
                .map(|_| ()),
            Attempt::RepeatCreate => service
                .create_task(&session(), task.plan.clone(), 1)
                .await
                .map(|_| ()),
            Attempt::Recover => service.recover(&session(), &task.task_id).await.map(|_| ()),
            Attempt::Checkpoint | Attempt::RejectCheckpoint => service
                .save_checkpoint(
                    &session(),
                    &task.task_id,
                    CheckpointChange {
                        expected_revision: if matches!(attempt, Attempt::RejectCheckpoint) {
                            999
                        } else {
                            task.revision
                        },
                        event: None,
                        checkpoint: task.checkpoint.clone(),
                        completed_nodes: vec![],
                        facts_version: 1,
                    },
                )
                .await
                .map(|_| ()),
            Attempt::Replace | Attempt::SamePlan | Attempt::RejectPlan => {
                let mut replacement = task.plan.clone();
                if matches!(attempt, Attempt::Replace) {
                    replacement.plan_version += 1;
                }
                service
                    .replace_plan(
                        &session(),
                        &task.task_id,
                        if matches!(attempt, Attempt::RejectPlan) {
                            999
                        } else {
                            task.revision
                        },
                        replacement,
                        1,
                    )
                    .await
                    .map(|_| ())
            }
            Attempt::CreateOperation | Attempt::RepeatOperation | Attempt::RejectOperation => {
                service
                    .create_operation(
                        &session(),
                        &task.task_id,
                        if matches!(attempt, Attempt::RejectOperation) {
                            999
                        } else {
                            task.revision
                        },
                        self.operation_id.clone(),
                        node(),
                        1,
                    )
                    .await
                    .map(|_| ())
            }
            Attempt::Event | Attempt::RejectEvent => service
                .record_operation_event(
                    &session(),
                    &task.task_id,
                    &self.operation_id,
                    OperationChange {
                        expected_revision: if matches!(attempt, Attempt::RejectEvent) {
                            999
                        } else {
                            1
                        },
                        event: OperationEvent::Cancel,
                        facts_version: 1,
                        business_operation_ref: None,
                    },
                )
                .await
                .map(|_| ()),
        }
    }
}
fn spawn(
    service: PersistenceService,
    fixture: Arc<Fixture>,
    attempt: Attempt,
) -> tokio::task::JoinHandle<Result<(), StorageError>> {
    tokio::spawn(async move { fixture.run(&service, attempt).await })
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn all_six_versioned_entries_and_existing_idempotency_remain_compatible() {
    let db = Db::new().await;
    for attempt in SIX.into_iter().chain([
        Attempt::RepeatCreate,
        Attempt::SamePlan,
        Attempt::RepeatOperation,
    ]) {
        let fixture = Fixture::new(&db, attempt).await;
        let before = db.snapshot().await;
        fixture
            .run(&db.service(Sessions::new()), attempt)
            .await
            .unwrap();
        if matches!(
            attempt,
            Attempt::Recover | Attempt::RepeatCreate | Attempt::SamePlan | Attempt::RepeatOperation
        ) {
            assert_eq!(db.snapshot().await, before);
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn unversioned_zero_and_revoked_resolvers_reject_all_entries_without_changes() {
    struct Legacy;
    impl SessionResolver for Legacy {
        fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
            Ok(user("user_001"))
        }
    }
    let db = Db::new().await;
    for attempt in SIX {
        let fixture = Fixture::new(&db, attempt).await;
        let zero = Sessions::new();
        zero.binding
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .binding_version = 0;
        let revoked = Sessions::new();
        revoked.change(None);
        for resolver in [Arc::new(Legacy) as Arc<dyn SessionResolver>, zero, revoked] {
            let before = db.snapshot().await;
            let error = fixture
                .run(&db.service(resolver), attempt)
                .await
                .unwrap_err();
            assert_eq!(error, StorageError::Session);
            assert_eq!(error.to_string(), "recovery storage: Session");
            assert_eq!(db.snapshot().await, before);
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn revoke_rebind_and_renew_during_task_lock_wait_roll_back_all_six_entries() {
    let db = Db::new().await;
    for attempt in SIX {
        for owner in [None, Some("user_002"), Some("user_001")] {
            let fixture = Arc::new(Fixture::new(&db, attempt).await);
            let before = db.snapshot().await;
            let sessions = Sessions::new();
            let mut lock = db.scoped.begin().await.unwrap();
            lock.execute("LOCK TABLE tasks IN ACCESS EXCLUSIVE MODE")
                .await
                .unwrap();
            let pending = spawn(db.service(sessions.clone()), fixture, attempt);
            db.wait_lock("tasks").await;
            sessions.change(owner);
            lock.commit().await.unwrap();
            assert_eq!(
                pending.await.unwrap(),
                Err(StorageError::Session),
                "{attempt:?}"
            );
            assert_eq!(db.snapshot().await, before);
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn later_operation_reads_and_success_audit_waits_are_guarded_before_commit() {
    let db = Db::new().await;
    for attempt in SIX {
        for table in ["operations", "audit_events"] {
            if matches!(attempt, Attempt::Create) && table == "operations" {
                continue;
            }
            let fixture = Arc::new(Fixture::new(&db, attempt).await);
            let before = db.snapshot().await;
            let sessions = Sessions::new();
            let mut lock = db.scoped.begin().await.unwrap();
            lock.execute(format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE").as_str())
                .await
                .unwrap();
            let pending = spawn(db.service(sessions.clone()), fixture, attempt);
            db.wait_lock(table).await;
            sessions.change(None);
            lock.commit().await.unwrap();
            assert_eq!(
                pending.await.unwrap(),
                Err(StorageError::Session),
                "{attempt:?} / {table}"
            );
            assert_eq!(db.snapshot().await, before);
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn idempotent_returns_and_rejection_audit_waits_do_not_commit_on_invalid_sessions() {
    let db = Db::new().await;
    for attempt in [
        Attempt::RepeatCreate,
        Attempt::SamePlan,
        Attempt::RepeatOperation,
        Attempt::RejectCheckpoint,
        Attempt::RejectPlan,
        Attempt::RejectOperation,
        Attempt::RejectEvent,
    ] {
        let fixture = Arc::new(Fixture::new(&db, attempt).await);
        let before = db.snapshot().await;
        let sessions = Sessions::new();
        let table = if matches!(
            attempt,
            Attempt::RepeatCreate | Attempt::SamePlan | Attempt::RepeatOperation
        ) {
            "tasks"
        } else {
            "audit_events"
        };
        let mut lock = db.scoped.begin().await.unwrap();
        lock.execute(format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE").as_str())
            .await
            .unwrap();
        let pending = spawn(db.service(sessions.clone()), fixture, attempt);
        db.wait_lock(table).await;
        sessions.change(Some("user_001"));
        lock.commit().await.unwrap();
        assert_eq!(pending.await.unwrap(), Err(StorageError::Session));
        assert_eq!(db.snapshot().await, before);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn every_precommit_observation_including_noop_and_rejection_is_rechecked() {
    let db = Db::new().await;
    for attempt in SIX.into_iter().chain([
        Attempt::RepeatCreate,
        Attempt::SamePlan,
        Attempt::RepeatOperation,
        Attempt::RejectCheckpoint,
        Attempt::RejectPlan,
        Attempt::RejectOperation,
        Attempt::RejectEvent,
    ]) {
        let fixture = Fixture::new(&db, attempt).await;
        let sessions = Sessions::new();
        let _ = fixture.run(&db.service(sessions.clone()), attempt).await;
        let observations = sessions.calls.load(Ordering::SeqCst);
        assert!(observations >= 7);
        // The last observation follows COMMIT; that separate ambiguity is documented.
        for point in 1..observations {
            let fixture = Fixture::new(&db, attempt).await;
            let before = db.snapshot().await;
            let sessions = Sessions::new();
            sessions.revoke_at.store(point, Ordering::SeqCst);
            assert_eq!(
                fixture.run(&db.service(sessions), attempt).await,
                Err(StorageError::Session),
                "{attempt:?} / observation {point}"
            );
            assert_eq!(
                db.snapshot().await,
                before,
                "{attempt:?} / observation {point}"
            );
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cancelling_a_real_wait_rolls_back_prior_mutations_and_audit() {
    let db = Db::new().await;
    for attempt in [
        Attempt::Create,
        Attempt::Recover,
        Attempt::Checkpoint,
        Attempt::CreateOperation,
        Attempt::Event,
    ] {
        let fixture = Arc::new(Fixture::new(&db, attempt).await);
        let before = db.snapshot().await;
        let mut lock = db.scoped.begin().await.unwrap();
        lock.execute("LOCK TABLE audit_events IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let pending = spawn(db.service(Sessions::new()), fixture, attempt);
        db.wait_lock("audit_events").await;
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        lock.commit().await.unwrap();
        let after = tokio::time::timeout(Duration::from_secs(10), db.snapshot())
            .await
            .unwrap();
        assert_eq!(after, before);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn pool_acquisition_wait_rechecks_binding_before_first_write() {
    let db = Db::new().await;
    let a = Arc::new(Fixture::new(&db, Attempt::Recover).await);
    let b = Arc::new(Fixture::new(&db, Attempt::Recover).await);
    let c = Arc::new(Fixture::new(&db, Attempt::Create).await);
    let before = db.snapshot().await;
    let mut lock = db.scoped.begin().await.unwrap();
    lock.execute("LOCK TABLE audit_events IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let first = spawn(db.healthy.clone(), a, Attempt::Recover);
    db.wait_lock("audit_events").await;
    let second = spawn(db.healthy.clone(), b, Attempt::Recover);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks WHERE relation=$1::regclass AND NOT granted",
            )
            .bind(format!("{}.audit_events", db.schema))
            .fetch_one(&db.admin)
            .await
            .unwrap();
            if count == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let sessions = Sessions::new();
    let third = spawn(db.service(sessions.clone()), c, Attempt::Create);
    tokio::time::timeout(Duration::from_secs(10), async {
        while sessions.calls.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(sessions.calls.load(Ordering::SeqCst), 2);
    sessions.change(None);
    lock.commit().await.unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(third.await.unwrap(), Err(StorageError::Session));
    assert_eq!(db.snapshot().await, before);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn duplicate_id_insert_wait_cannot_commit_rejection_or_return_old_task() {
    let db = Db::new().await;
    let fixture = Arc::new(Fixture::new(&db, Attempt::RepeatCreate).await);
    // UPDATE holds the actual row/transaction dependency used by ON CONFLICT.
    let mut lock = db.scoped.begin().await.unwrap();
    sqlx::query("UPDATE tasks SET revision=revision WHERE task_id=$1")
        .bind(fixture.task.task_id.as_str())
        .execute(&mut *lock)
        .await
        .unwrap();
    let before = db.snapshot().await;
    let sessions = Sessions::new();
    let pending = spawn(db.service(sessions.clone()), fixture, Attempt::RepeatCreate);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE a.wait_event_type='Lock' AND a.query LIKE 'INSERT INTO tasks %' AND EXISTS(SELECT 1 FROM pg_locks l WHERE l.pid=a.pid AND l.relation=$1::regclass))")
                .bind(format!("{}.tasks", db.schema)).fetch_one(&db.admin).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("duplicate insert did not wait");
    sessions.change(Some("user_002"));
    lock.commit().await.unwrap();
    assert_eq!(pending.await.unwrap(), Err(StorageError::Session));
    assert_eq!(db.snapshot().await, before);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn deferred_commit_rejection_rolls_back_every_write_and_its_audit() {
    let db = Db::new().await;
    for attempt in [
        Attempt::Create,
        Attempt::Checkpoint,
        Attempt::Replace,
        Attempt::CreateOperation,
        Attempt::Event,
        Attempt::RejectCheckpoint,
    ] {
        let fixture = Fixture::new(&db, attempt).await;
        let before = db.snapshot().await;
        db.scoped.execute("CREATE FUNCTION fail_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated commit failure'; END $$").await.unwrap();
        db.scoped.execute("CREATE CONSTRAINT TRIGGER reject_commit AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_commit()").await.unwrap();
        assert_eq!(
            fixture.run(&db.service(Sessions::new()), attempt).await,
            Err(StorageError::Database)
        );
        assert_eq!(db.snapshot().await, before);
        db.scoped
            .execute("DROP TRIGGER reject_commit ON audit_events")
            .await
            .unwrap();
        db.scoped
            .execute("DROP FUNCTION fail_commit()")
            .await
            .unwrap();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cross_user_existing_ids_return_not_found_without_rejection_audit() {
    let db = Db::new().await;
    for attempt in [
        Attempt::RepeatCreate,
        Attempt::Recover,
        Attempt::Checkpoint,
        Attempt::Replace,
        Attempt::CreateOperation,
        Attempt::Event,
    ] {
        let fixture = Fixture::new(&db, attempt).await;
        let before = db.snapshot().await;
        let sessions = Sessions::new();
        sessions.change(Some("user_002"));
        assert_eq!(
            fixture.run(&db.service(sessions), attempt).await,
            Err(StorageError::NotFound)
        );
        assert_eq!(db.snapshot().await, before);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn revocation_after_commit_reports_error_but_does_not_claim_to_undo_saved_task() {
    let db = Db::new().await;
    let fixture = Fixture::new(&db, Attempt::Create).await;
    let sessions = Sessions::new();
    fixture
        .run(&db.service(sessions.clone()), Attempt::Create)
        .await
        .unwrap();
    let last_observation = sessions.calls.load(Ordering::SeqCst);
    let fixture = Fixture::new(&db, Attempt::Create).await;
    let sessions = Sessions::new();
    sessions.revoke_at.store(last_observation, Ordering::SeqCst);
    assert_eq!(
        fixture.run(&db.service(sessions), Attempt::Create).await,
        Err(StorageError::Session)
    );
    let saved = db
        .healthy
        .recover(&session(), &fixture.fresh_plan.task_id)
        .await
        .unwrap();
    assert_eq!(saved.task.plan, fixture.fresh_plan);
    assert_eq!(saved.task.revision, 1);
    assert_eq!(saved.audit.len(), 1);
    assert_eq!(saved.audit[0].kind, PersistentAuditKind::TaskCreated);
    // Reconcile by the original ID; an error is not permission for a new command.
    db.finish().await;
}
