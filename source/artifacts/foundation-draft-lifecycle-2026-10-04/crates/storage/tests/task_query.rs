use safeflow_contracts::*;
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeAssembly, RuntimeBootstrap, SessionBinding,
    SessionResolver, ToolRegistry, ToolSecurityCatalog,
};
use safeflow_storage::*;
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

fn session() -> SessionRef {
    SessionRef::parse("server_query_session").unwrap()
}
fn node(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}
fn plan() -> AgentPlan {
    AgentPlan::new(
        new_task_id(),
        1,
        vec![PlanNode::Tool {
            id: node("stored_node"),
            tool: ToolName::parse("stored_contract_tool").unwrap(),
            arguments: json!({"amount":"200.00", "private_note":"do-not-return-parameters"}),
            depends_on: vec![],
        }],
    )
    .unwrap()
}

struct Sessions {
    binding: RwLock<(u64, Option<UserId>)>,
    calls: AtomicUsize,
    change_at: AtomicUsize,
    revoke: AtomicUsize,
}
impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            binding: RwLock::new((1, Some(UserId::parse("user_001").unwrap()))),
            calls: AtomicUsize::new(0),
            change_at: AtomicUsize::new(0),
            revoke: AtomicUsize::new(0),
        })
    }
    fn reset(&self) {
        self.calls.store(0, Ordering::SeqCst);
    }
    fn change(&self, revoke: bool) {
        let mut binding = self.binding.write().unwrap();
        binding.0 += 1;
        if revoke {
            binding.1 = None;
        }
    }
    fn user(&self, reference: &SessionRef) -> Result<UserId, SecurityError> {
        let user = if reference == &session() {
            self.binding.read().unwrap().1.clone()
        } else if reference.as_str() == "other_query_session" {
            Some(UserId::parse("user_002").unwrap())
        } else {
            None
        };
        user.ok_or(SecurityError {
            reason_code: ReasonCode::parse("SESSION_MISSING").unwrap(),
            message: "isolated session unavailable".into(),
        })
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, reference: &SessionRef) -> Result<UserId, SecurityError> {
        self.user(reference)
    }
    fn resolve_binding(&self, reference: &SessionRef) -> Result<SessionBinding, SecurityError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.change_at.load(Ordering::SeqCst) {
            self.change(self.revoke.load(Ordering::SeqCst) == 1);
        }
        if reference == &session() {
            let binding = self.binding.read().unwrap();
            Ok(SessionBinding {
                user_id: binding.1.clone().ok_or(SecurityError {
                    reason_code: ReasonCode::parse("SESSION_MISSING").unwrap(),
                    message: "isolated session unavailable".into(),
                })?,
                binding_version: binding.0,
            })
        } else {
            Ok(SessionBinding {
                user_id: self.user(reference)?,
                binding_version: 1,
            })
        }
    }
}
fn runtime(sessions: Arc<dyn SessionResolver>) -> RuntimeAssembly {
    RuntimeBootstrap::build(
        ToolRegistry::new(),
        DomainPolicyRegistry::new(),
        ToolSecurityCatalog::new(),
        sessions,
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap()
}

struct Db {
    url: String,
    schema: String,
    admin: PgPool,
    scoped: PgPool,
    repo: PgRepository,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("real PostgreSQL configuration required; query tests were not skipped");
        let schema = format!("sf_task_query_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("isolated PostgreSQL unavailable");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let path = schema.clone();
        let scoped = PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |conn, _| {
                let path = path.clone();
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
            .expect("isolated schema unavailable");
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        Self {
            url,
            schema,
            admin,
            scoped,
            repo,
        }
    }
    fn service(&self, sessions: Arc<Sessions>) -> PersistenceService {
        PersistenceService::for_trusted_backend(self.repo.clone(), sessions)
    }
    async fn task(&self, sessions: Arc<Sessions>) -> TaskRecord {
        self.service(sessions)
            .create_task(&session(), plan(), 7)
            .await
            .unwrap()
    }
    async fn operation(&self, sessions: Arc<Sessions>, task: &TaskId) -> OperationRecord {
        let service = self.service(sessions);
        let t = service.recover(&session(), task).await.unwrap().task;
        service
            .create_operation(
                &session(),
                task,
                t.revision,
                new_operation_id(),
                node("stored_node"),
                1,
            )
            .await
            .unwrap()
    }
    async fn event(
        &self,
        sessions: Arc<Sessions>,
        task: &TaskId,
        op: &OperationRecord,
        event: OperationEvent,
    ) -> OperationRecord {
        self.service(sessions)
            .record_operation_event(
                &session(),
                task,
                &op.operation_id,
                OperationChange {
                    expected_revision: op.revision,
                    event,
                    facts_version: op.facts_version,
                    business_operation_ref: if event == OperationEvent::SubmissionRecorded {
                        Some(
                            BusinessOperationRef::parse("private_original_bank_reference").unwrap(),
                        )
                    } else {
                        op.business_operation_ref.clone()
                    },
                },
            )
            .await
            .unwrap()
    }
    async fn finish(self) {
        self.repo.close().await;
        self.scoped.close().await;
        self.admin.close().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn owner_unknown_and_foreign_tasks_have_no_existence_leak() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    let assembly = runtime(sessions);
    let service = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    assert_eq!(
        service
            .query(&session(), &task.task_id)
            .await
            .unwrap()
            .task_state,
        TaskState::Running
    );
    assert_eq!(
        service
            .query(
                &SessionRef::parse("other_query_session").unwrap(),
                &task.task_id
            )
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        service.query(&session(), &new_task_id()).await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        service
            .query(
                &SessionRef::parse("missing_session").unwrap(),
                &task.task_id
            )
            .await,
        Err(StorageError::Session)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn pre_v13_mode_recovers_but_missing_migrated_mode_table_fails_closed() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    db.operation(sessions.clone(), &task.task_id).await;
    let assembly = runtime(sessions.clone());
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    let persistence = db.service(sessions);
    let before_query = query.query(&session(), &task.task_id).await.unwrap();
    let before_recovery = persistence
        .recover(&session(), &task.task_id)
        .await
        .unwrap();
    db.scoped
        .execute(
            "DROP TABLE mock_draft_lifecycle_claims;
             DROP TABLE mock_draft_lifecycle_displays;
             ALTER TABLE mock_draft_lifecycle_tasks DROP CONSTRAINT lifecycle_active_action_fk,
                 DROP CONSTRAINT lifecycle_last_success_fk;
             DROP TABLE mock_draft_lifecycle_actions;
             DROP TABLE mock_draft_lifecycle_plans;
             DROP TABLE mock_draft_lifecycle_tasks;
             ALTER TABLE server_confirmations DROP COLUMN lifecycle_anchor;
             ALTER TABLE security_decisions DROP CONSTRAINT security_decisions_source_mode_check,
                 DROP COLUMN lifecycle_anchor;
             ALTER TABLE security_decisions ADD CONSTRAINT security_decisions_source_mode_check
                 CHECK (source_mode='TRUSTED_RUNTIME_V1');
             CREATE TABLE mode_future_migrations AS SELECT * FROM _sqlx_migrations WHERE version>13;
             DELETE FROM _sqlx_migrations WHERE version>=13;",
        )
        .await
        .unwrap();
    assert_eq!(
        query.query(&session(), &task.task_id).await.unwrap(),
        before_query
    );
    assert_eq!(
        persistence
            .recover(&session(), &task.task_id)
            .await
            .unwrap(),
        before_recovery
    );
    assert!(!db
        .repo
        .is_lifecycle_task(&task.owner_id, &task.task_id)
        .await
        .unwrap());
    db.scoped.execute("INSERT INTO _sqlx_migrations SELECT * FROM mode_future_migrations; DROP TABLE mode_future_migrations").await.unwrap();
    db.repo.migrate().await.unwrap();
    assert_eq!(
        query.query(&session(), &task.task_id).await.unwrap(),
        before_query
    );
    assert_eq!(
        persistence
            .recover(&session(), &task.task_id)
            .await
            .unwrap(),
        before_recovery
    );
    db.scoped
        .execute("ALTER TABLE mock_draft_lifecycle_tasks RENAME TO unavailable_mode_table")
        .await
        .unwrap();
    assert_eq!(
        query.query(&session(), &task.task_id).await,
        Err(StorageError::CorruptRecord)
    );
    assert_eq!(
        persistence.recover(&session(), &task.task_id).await,
        Err(StorageError::CorruptRecord)
    );
    assert_eq!(
        db.repo
            .is_lifecycle_task(&task.owner_id, &task.task_id)
            .await,
        Err(StorageError::CorruptRecord)
    );
    db.finish().await;
}

struct LegacySessions;
impl SessionResolver for LegacySessions {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(UserId::parse("user_001").unwrap())
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn legacy_or_zero_version_resolvers_cannot_use_query() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    let assembly = runtime(Arc::new(LegacySessions));
    assert_eq!(
        TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
            .query(&session(), &task.task_id)
            .await,
        Err(StorageError::Session)
    );
    sessions.binding.write().unwrap().0 = 0;
    let assembly = runtime(sessions);
    assert_eq!(
        TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
            .query(&session(), &task.task_id)
            .await,
        Err(StorageError::Session)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn session_rebind_and_revocation_at_every_observation_deny_return() {
    let db = Db::new().await;
    for revoked in [false, true] {
        for observation in 2..=7 {
            let sessions = Sessions::new();
            let task = db.task(sessions.clone()).await;
            sessions.reset();
            sessions.change_at.store(observation, Ordering::SeqCst);
            sessions
                .revoke
                .store(usize::from(revoked), Ordering::SeqCst);
            let assembly = runtime(sessions);
            assert_eq!(
                TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
                    .query(&session(), &task.task_id)
                    .await,
                Err(StorageError::Session),
                "observation {observation}, revoked {revoked}"
            );
        }
    }
    db.finish().await;
}

async fn wait_for_lock(db: &Db, table: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation=$1::regclass AND NOT granted)",
            )
            .bind(format!("{}.{}", db.schema, table))
            .fetch_one(&db.admin)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("query did not reach database lock wait");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated PostgreSQL 16"]
async fn session_changes_during_task_or_operation_table_wait_reject() {
    let db = Db::new().await;
    for table in ["tasks", "operations"] {
        for revoked in [false, true] {
            let sessions = Sessions::new();
            let task = db.task(sessions.clone()).await;
            let mut lock = db.scoped.begin().await.unwrap();
            lock.execute(format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE").as_str())
                .await
                .unwrap();
            let assembly = Arc::new(runtime(sessions.clone()));
            let repo = db.repo.clone();
            let task_id = task.task_id;
            let pending = tokio::spawn(async move {
                TaskQueryService::for_trusted_backend(repo, assembly.runtime())
                    .query(&session(), &task_id)
                    .await
            });
            wait_for_lock(&db, table).await;
            sessions.change(revoked);
            lock.commit().await.unwrap();
            assert_eq!(pending.await.unwrap(), Err(StorageError::Session));
        }
    }
    db.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_operation_commit_does_not_mix_task_and_operation_versions() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    let op = db.operation(sessions.clone(), &task.task_id).await;
    let before = db
        .service(sessions.clone())
        .recover(&session(), &task.task_id)
        .await
        .unwrap();
    let mut writer = db.scoped.begin().await.unwrap();
    writer
        .execute("LOCK TABLE operations IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let assembly = Arc::new(runtime(sessions.clone()));
    let repo = db.repo.clone();
    let task_id = task.task_id.clone();
    let cloned = assembly.clone();
    let pending = tokio::spawn(async move {
        TaskQueryService::for_trusted_backend(repo, cloned.runtime())
            .query(&session(), &task_id)
            .await
    });
    wait_for_lock(&db, "operations").await;
    // Only contract state fixtures: commit between the task and operation SELECTs.
    sqlx::query("UPDATE tasks SET revision=revision+1,facts_version=8 WHERE task_id=$1")
        .bind(task.task_id.as_str())
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET state='READY',revision=revision+1,facts_version=8 WHERE operation_id=$1")
        .bind(op.operation_id.as_str()).execute(&mut *writer).await.unwrap();
    writer.commit().await.unwrap();
    let old = pending.await.unwrap().unwrap();
    assert_eq!((old.revision, old.facts_version), (before.task.revision, 7));
    assert_eq!(
        (
            old.operations[0].operation_state,
            old.operations[0].revision,
            old.operations[0].facts_version
        ),
        (OperationState::Draft, 1, 7)
    );
    let new = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
        .query(&session(), &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        (new.revision, new.facts_version),
        (before.task.revision + 1, 8)
    );
    assert_eq!(
        (
            new.operations[0].operation_state,
            new.operations[0].revision,
            new.operations[0].facts_version
        ),
        (OperationState::Ready, 2, 8)
    );
    db.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_replan_is_visible_only_in_the_next_complete_snapshot() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    let mut writer = db.scoped.begin().await.unwrap();
    writer
        .execute("LOCK TABLE operations IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let assembly = Arc::new(runtime(sessions.clone()));
    let cloned = assembly.clone();
    let repo = db.repo.clone();
    let id = task.task_id.clone();
    let pending = tokio::spawn(async move {
        TaskQueryService::for_trusted_backend(repo, cloned.runtime())
            .query(&session(), &id)
            .await
    });
    wait_for_lock(&db, "operations").await;
    let mut updated = task.plan.clone();
    updated.plan_version = 2;
    sqlx::query("UPDATE tasks SET revision=revision+1,plan_version=2,facts_version=8,plan=$2 WHERE task_id=$1")
        .bind(task.task_id.as_str()).bind(sqlx::types::Json(updated)).execute(&mut *writer).await.unwrap();
    writer.commit().await.unwrap();
    let old = pending.await.unwrap().unwrap();
    assert_eq!(
        (old.revision, old.plan_version, old.facts_version),
        (1, 1, 7)
    );
    let new = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
        .query(&session(), &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        (new.revision, new.plan_version, new.facts_version),
        (2, 2, 8)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn all_task_states_and_wait_positions_are_projected_without_authority() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let assembly = runtime(sessions.clone());
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    for (event, state) in [
        (TaskEvent::Complete, TaskState::Completed),
        (TaskEvent::Fail, TaskState::Failed),
        (TaskEvent::Cancel, TaskState::Cancelled),
    ] {
        let task = db.task(sessions.clone()).await;
        let updated = db
            .service(sessions.clone())
            .save_checkpoint(
                &session(),
                &task.task_id,
                CheckpointChange {
                    expected_revision: task.revision,
                    event: Some(event),
                    checkpoint: RecoveryPoint::Finished,
                    completed_nodes: if state == TaskState::Completed {
                        vec![node("stored_node")]
                    } else {
                        vec![]
                    },
                    facts_version: 7,
                },
            )
            .await
            .unwrap();
        let view = query.query(&session(), &task.task_id).await.unwrap();
        assert_eq!(view.task_state, state);
        assert_eq!(view.revision, updated.revision);
        assert_eq!(
            view.recovery,
            TaskRecoveryHint::Finished { task_state: state }
        );
        assert_eq!(
            view.security,
            TaskQuerySecurityScope::CurrentDecisionNotProvided
        );
    }
    let p = AgentPlan::new(
        new_task_id(),
        1,
        vec![PlanNode::UserDecision {
            id: node("choose"),
            depends_on: vec![],
        }],
    )
    .unwrap();
    let task = db
        .service(sessions.clone())
        .create_task(&session(), p, 7)
        .await
        .unwrap();
    db.service(sessions)
        .save_checkpoint(
            &session(),
            &task.task_id,
            CheckpointChange {
                expected_revision: 1,
                event: Some(TaskEvent::AwaitUser),
                checkpoint: RecoveryPoint::AtNode {
                    node_id: node("choose"),
                },
                completed_nodes: vec![],
                facts_version: 7,
            },
        )
        .await
        .unwrap();
    let view = query.query(&session(), &task.task_id).await.unwrap();
    assert_eq!(view.task_state, TaskState::WaitingUser);
    assert_eq!(
        view.recovery,
        TaskRecoveryHint::AwaitUser {
            node_id: node("choose")
        }
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn all_operation_states_and_unknown_result_keep_original_id_without_bank_claims() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let assembly = runtime(sessions.clone());
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    for terminal in [
        OperationEvent::StatusConfirmedSucceeded,
        OperationEvent::StatusConfirmedFailed,
    ] {
        let task = db.task(sessions.clone()).await;
        let mut op = db.operation(sessions.clone(), &task.task_id).await;
        for event in [
            None,
            Some(OperationEvent::GatesSatisfied),
            Some(OperationEvent::SubmissionRecorded),
            Some(OperationEvent::OutcomeUnknown),
            Some(terminal),
        ] {
            if let Some(event) = event {
                op = db.event(sessions.clone(), &task.task_id, &op, event).await;
            }
            let view = query.query(&session(), &task.task_id).await.unwrap();
            assert_eq!(view.operations[0].operation_id, op.operation_id);
            assert_eq!(view.operations[0].operation_state, op.state);
            if matches!(
                op.state,
                OperationState::Submitted | OperationState::Processing
            ) {
                assert_eq!(view.task_state, TaskState::WaitingExternal);
                assert_eq!(
                    view.recovery,
                    TaskRecoveryHint::ReconcileOriginal {
                        operation_id: op.operation_id.clone()
                    }
                );
            }
            if op.state.is_terminal() {
                assert_eq!(
                    view.recovery,
                    TaskRecoveryHint::ReviewRecordedResult {
                        operation_id: op.operation_id.clone()
                    }
                );
            }
            let output = serde_json::to_string(&view).unwrap();
            assert!(!output.contains("private_original_bank_reference"));
            assert!(!output.contains("READY_TO_EXECUTE"));
            assert!(!output.contains("ALLOW"));
        }
    }
    let task = db.task(sessions.clone()).await;
    let op = db.operation(sessions.clone(), &task.task_id).await;
    let cancelled = db
        .event(sessions, &task.task_id, &op, OperationEvent::Cancel)
        .await;
    assert_eq!(
        query
            .query(&session(), &task.task_id)
            .await
            .unwrap()
            .operations[0]
            .operation_state,
        cancelled.state
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn projection_contains_only_documented_fields_and_query_has_no_writes() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    db.operation(sessions.clone(), &task.task_id).await;
    let raw = db.service(sessions.clone());
    let before = raw.recover(&session(), &task.task_id).await.unwrap();
    let assembly = runtime(sessions);
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    let view = query.query(&session(), &task.task_id).await.unwrap();
    let output = serde_json::to_value(&view).unwrap();
    let keys: Vec<_> = output
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "contract_version",
            "facts_version",
            "operations",
            "plan_version",
            "recovery",
            "revision",
            "security",
            "task_id",
            "task_state"
        ]
    );
    let keys: Vec<_> = output["operations"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "facts_version",
            "node_id",
            "operation_id",
            "operation_state",
            "plan_version",
            "revision"
        ]
    );
    let serialized = serde_json::to_string(&view).unwrap();
    for forbidden in [
        "user_001",
        "server_query_session",
        "200.00",
        "private_note",
        "arguments",
        "audit",
        "confirmation",
        "permit",
        "grant",
        "plan\"",
    ] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }
    for _ in 0..3 {
        assert_eq!(query.query(&session(), &task.task_id).await.unwrap(), view);
    }
    assert_eq!(
        raw.recover(&session(), &task.task_id).await.unwrap(),
        before
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn rebuilding_repository_and_runtime_reads_the_same_view() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    db.operation(sessions.clone(), &task.task_id).await;
    let assembly = runtime(sessions);
    let before = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
        .query(&session(), &task.task_id)
        .await
        .unwrap();
    let repo = PgRepository::connect_existing(&db.url, &db.schema)
        .await
        .unwrap();
    let assembly = runtime(Sessions::new());
    assert_eq!(
        TaskQueryService::for_trusted_backend(repo.clone(), assembly.runtime())
            .query(&session(), &task.task_id)
            .await
            .unwrap(),
        before
    );
    repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn query_connection_never_creates_missing_namespace() {
    let db = Db::new().await;
    let missing = format!("sf_task_query_missing_{}", uuid::Uuid::new_v4().simple());
    assert!(matches!(
        PgRepository::connect_existing(&db.url, &missing).await,
        Err(StorageError::NotFound)
    ));
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
            .bind(&missing)
            .fetch_one(&db.admin)
            .await
            .unwrap();
    assert!(!exists);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn invalid_database_associations_hashes_versions_and_checkpoints_fail_closed() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let assembly = runtime(sessions.clone());
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    let changes=["UPDATE operations SET node_id='missing' WHERE task_id=$1","UPDATE operations SET plan_version=2 WHERE task_id=$1","UPDATE operations SET facts_version=8 WHERE task_id=$1","UPDATE operations SET arguments_hash=repeat('a',64) WHERE task_id=$1","UPDATE tasks SET checkpoint='{\"kind\":\"AT_NODE\",\"node_id\":\"missing\"}' WHERE task_id=$1","UPDATE tasks SET completed_nodes='[\"stored_node\"]' WHERE task_id=$1","UPDATE tasks SET plan=jsonb_set(plan,'{contract_version}','99') WHERE task_id=$1"];
    for change in changes {
        let task = db.task(sessions.clone()).await;
        db.operation(sessions.clone(), &task.task_id).await;
        sqlx::query(change)
            .bind(task.task_id.as_str())
            .execute(&db.scoped)
            .await
            .unwrap();
        assert_eq!(
            query.query(&session(), &task.task_id).await,
            Err(StorageError::CorruptRecord),
            "change {change}"
        );
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn invalid_json_and_unknown_state_are_not_skipped() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let assembly = runtime(sessions.clone());
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    for change in ["UPDATE tasks SET plan=jsonb_set(plan,'{nodes}','\"wrong-type\"') WHERE task_id=$1","UPDATE tasks SET checkpoint='{\"kind\":\"AT_NODE\",\"node_id\":\"stored_node\",\"extra\":true}' WHERE task_id=$1"] {
        let task=db.task(sessions.clone()).await; sqlx::query(change).bind(task.task_id.as_str()).execute(&db.scoped).await.unwrap(); assert_eq!(query.query(&session(),&task.task_id).await,Err(StorageError::CorruptRecord));
    }
    db.scoped
        .execute("ALTER TABLE operations DROP CONSTRAINT operations_state_check")
        .await
        .unwrap();
    let task = db.task(sessions.clone()).await;
    db.operation(sessions, &task.task_id).await;
    sqlx::query("UPDATE operations SET state='ALLOW' WHERE task_id=$1")
        .bind(task.task_id.as_str())
        .execute(&db.scoped)
        .await
        .unwrap();
    assert_eq!(
        query.query(&session(), &task.task_id).await,
        Err(StorageError::CorruptRecord)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn oversized_plan_and_floating_parameters_are_rejected_without_exposure() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let assembly = runtime(sessions.clone());
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    for arguments in [
        json!({"private": "x".repeat(TASK_QUERY_MAX_PLAN_BYTES)}),
        json!({"amount":200.5}),
        json!({"nested":[{"amount":0.1}]}),
    ] {
        let mut p = plan();
        let PlanNode::Tool {
            arguments: stored, ..
        } = &mut p.nodes[0]
        else {
            unreachable!()
        };
        *stored = arguments;
        let task = db
            .service(sessions.clone())
            .create_task(&session(), p, 7)
            .await
            .unwrap();
        assert_eq!(
            query.query(&session(), &task.task_id).await,
            Err(StorageError::CorruptRecord)
        );
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn operation_list_limit_is_rejection_not_silent_truncation() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) SELECT 'op_'||n,$1,'node_'||n,'DRAFT',1,1,7,repeat('a',64) FROM generate_series(1,257) n").bind(task.task_id.as_str()).execute(&db.scoped).await.unwrap();
    let assembly = runtime(sessions);
    assert_eq!(
        TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
            .query(&session(), &task.task_id)
            .await,
        Err(StorageError::CorruptRecord)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn full_operation_limit_is_complete_and_sorted_without_unregistered_tool_calls() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let p = AgentPlan::new(
        new_task_id(),
        1,
        (0..256)
            .map(|n| PlanNode::Tool {
                id: node(&format!("node_{n:03}")),
                tool: ToolName::parse("existing_metadata_only").unwrap(),
                arguments: json!({}),
                depends_on: vec![],
            })
            .collect(),
    )
    .unwrap();
    let task = db
        .service(sessions.clone())
        .create_task(&session(), p, 7)
        .await
        .unwrap();
    for n in (0..256).rev() {
        sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'DRAFT',1,1,7,$4)")
            .bind(format!("op_{n:03}")).bind(task.task_id.as_str()).bind(format!("node_{n:03}")).bind("44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a").execute(&db.scoped).await.unwrap();
    }
    let assembly = runtime(sessions);
    let view = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime())
        .query(&session(), &task.task_id)
        .await
        .unwrap();
    assert_eq!(view.operations.len(), 256);
    assert_eq!(
        view.operations.first().unwrap().operation_id.as_str(),
        "op_000"
    );
    assert_eq!(
        view.operations.last().unwrap().operation_id.as_str(),
        "op_255"
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn bad_request_ids_and_database_failure_return_sanitized_errors() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let task = db.task(sessions.clone()).await;
    let assembly = runtime(sessions);
    let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
    assert!(TaskId::parse(" x").is_err());
    for id in ["x\ncontrol", &"x".repeat(201)] {
        assert!(matches!(
            query.query(&session(), &TaskId::parse(id).unwrap()).await,
            Err(StorageError::Invalid(_))
        ));
    }
    db.repo.close().await;
    let err = query.query(&session(), &task.task_id).await.unwrap_err();
    assert_eq!(err, StorageError::Database);
    assert!(!err.to_string().contains(&db.url));
    db.finish().await;
}
