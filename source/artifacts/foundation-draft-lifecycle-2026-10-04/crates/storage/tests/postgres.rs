use safeflow_contracts::{
    AgentPlan, BusinessOperationRef, NodeId, OperationEvent, OperationState, PlanNode, SessionRef,
    TaskEvent, TaskId, TaskState, ToolName, UserId,
};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{
    new_operation_id, new_task_id, CheckpointChange, OperationChange, OperationRecord,
    PersistenceService, PersistentAuditKind, PgRepository, RecoveryDirective, RecoveryPoint,
    StorageError,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use std::sync::Arc;

struct TestDb {
    url: String,
    namespace: String,
    admin: PgPool,
    scoped: PgPool,
    repo: PgRepository,
    service: PersistenceService,
    session: SessionRef,
    other: SessionRef,
}

impl TestDb {
    async fn new() -> Self {
        let db = Self::unmigrated().await;
        db.repo.migrate().await.unwrap();
        db
    }

    async fn unmigrated() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("PostgreSQL tests require SAFEFLOW_TEST_DATABASE_URL; no test was skipped");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("cannot connect to isolated PostgreSQL test database"));
        let namespace = format!("sf_step4a_test_{}", uuid::Uuid::new_v4().simple());
        // CREATE (without IF NOT EXISTS) proves this test owns the new namespace.
        admin
            .execute(format!("CREATE SCHEMA {namespace}").as_str())
            .await
            .unwrap();
        let search_path = namespace.clone();
        let scoped = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |conn, _| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("test schema connection failed"));
        let repo = PgRepository::connect(&url, &namespace).await.unwrap();
        let session = SessionRef::parse("server_session_001").unwrap();
        let other = SessionRef::parse("server_session_002").unwrap();
        let mut sessions = StaticSessionResolver::new();
        sessions.insert(session.clone(), UserId::parse("user_001").unwrap());
        sessions.insert(other.clone(), UserId::parse("user_002").unwrap());
        let service = PersistenceService::for_trusted_backend(repo.clone(), Arc::new(sessions));
        Self {
            url,
            namespace,
            admin,
            scoped,
            repo,
            service,
            session,
            other,
        }
    }

    async fn migrate_v1(&self) -> Vec<u8> {
        let migration = sqlx::migrate::Migration::new(
            1,
            "recovery points".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0001_recovery_points.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let checksum = migration.checksum.to_vec();
        sqlx::migrate::Migrator {
            migrations: std::borrow::Cow::Owned(vec![migration]),
            ..sqlx::migrate::Migrator::DEFAULT
        }
        .run(&self.scoped)
        .await
        .unwrap();
        checksum
    }

    async fn task(&self) -> safeflow_storage::TaskRecord {
        self.service
            .create_task(&self.session, plan(new_task_id()), 1)
            .await
            .unwrap()
    }

    async fn op(&self, task: &TaskId) -> OperationRecord {
        let snapshot = self.service.recover(&self.session, task).await.unwrap();
        self.service
            .create_operation(
                &self.session,
                task,
                snapshot.task.revision,
                new_operation_id(),
                node("read"),
                1,
            )
            .await
            .unwrap()
    }

    async fn event(
        &self,
        op: &OperationRecord,
        event: OperationEvent,
        reference: Option<BusinessOperationRef>,
    ) -> OperationRecord {
        self.service
            .record_operation_event(
                &self.session,
                &op.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: op.revision,
                    event,
                    facts_version: op.facts_version + 1,
                    business_operation_ref: reference,
                },
            )
            .await
            .unwrap()
    }

    async fn fault_audit(&self, deferred: bool) {
        self.scoped.execute("CREATE FUNCTION fail_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected audit failure'; END $$").await.unwrap();
        let trigger = if deferred {
            "CREATE CONSTRAINT TRIGGER reject_audit AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_audit()"
        } else {
            "CREATE TRIGGER reject_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_audit()"
        };
        self.scoped.execute(trigger).await.unwrap();
    }

    async fn clear_fault(&self) {
        self.scoped
            .execute("DROP TRIGGER reject_audit ON audit_events")
            .await
            .unwrap();
        self.scoped
            .execute("DROP FUNCTION fail_audit()")
            .await
            .unwrap();
    }

    async fn finish(self) {
        self.repo.close().await;
        self.scoped.close().await;
        assert!(self.namespace.starts_with("sf_step4a_test_") && self.namespace.len() == 47);
        // Only this test's successfully created schema is removed, never a database.
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.namespace).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}

fn node(value: &str) -> NodeId {
    NodeId::parse(value).unwrap()
}

fn plan(task_id: TaskId) -> AgentPlan {
    AgentPlan::new(
        task_id,
        1,
        vec![
            PlanNode::Tool {
                id: node("read"),
                tool: ToolName::parse("get_account_balance").unwrap(),
                arguments: serde_json::json!({"account_id": "account_001", "amount": "200.00"}),
                depends_on: vec![],
            },
            PlanNode::UserDecision {
                id: node("choose"),
                depends_on: vec![node("read")],
            },
            PlanNode::Tool {
                id: node("save"),
                tool: ToolName::parse("create_mock_transfer_draft").unwrap(),
                arguments: serde_json::json!({"amount":"200.00","currency":"CNY"}),
                depends_on: vec![node("choose")],
            },
        ],
    )
    .unwrap()
}

fn waiting(revision: u64) -> CheckpointChange {
    CheckpointChange {
        expected_revision: revision,
        event: Some(TaskEvent::AwaitUser),
        checkpoint: RecoveryPoint::AtNode {
            node_id: node("choose"),
        },
        completed_nodes: vec![node("read")],
        facts_version: 2,
    }
}

const INSERT_PLAN_SQL: &str = "INSERT INTO tasks (task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES ($1,'user_001',1,'RUNNING',1,1,1,$2,repeat('a',64),$3,'[]')";

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn database_plan_constraint_rejects_missing_null_mistyped_and_mismatched_fields() {
    let db = TestDb::new().await;
    // Numeric-looking identity catches coercion of a JSON number to matching text.
    let p = plan(TaskId::parse("123").unwrap());
    let valid = serde_json::to_value(&p).unwrap();
    let checkpoint = serde_json::json!({"kind":"AT_NODE", "node_id":"read"});
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query(INSERT_PLAN_SQL)
        .bind(p.task_id.as_str())
        .bind(&valid)
        .bind(&checkpoint)
        .execute(&mut *tx)
        .await
        .expect("valid plan must insert");
    tx.rollback().await.unwrap();

    let mut cases = vec![
        ("empty object".to_owned(), serde_json::json!({})),
        ("JSON null".to_owned(), serde_json::Value::Null),
        ("array".to_owned(), serde_json::json!([])),
    ];
    for field in ["task_id", "plan_version"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        cases.push((format!("missing {field}"), missing));
        for (label, value) in [
            ("null", serde_json::Value::Null),
            ("boolean", serde_json::json!(true)),
            ("array", serde_json::json!([])),
            ("object", serde_json::json!({})),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            cases.push((format!("{label} {field}"), invalid));
        }
    }
    for (label, field, value) in [
        ("numeric identity", "task_id", serde_json::json!(123)),
        ("wrong identity", "task_id", serde_json::json!("other_task")),
        ("string version", "plan_version", serde_json::json!("1")),
        ("wrong version", "plan_version", serde_json::json!(2)),
        ("zero version", "plan_version", serde_json::json!(0)),
        ("negative version", "plan_version", serde_json::json!(-1)),
        ("fractional version", "plan_version", serde_json::json!(1.5)),
        ("decimal version", "plan_version", serde_json::json!(1.0)),
        (
            "overflow version",
            "plan_version",
            serde_json::json!(4294967296_u64),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        cases.push((label.to_owned(), invalid));
    }
    let mut accepted = vec![];
    let mut unexpected = vec![];
    for (label, invalid) in cases {
        for update in [false, true] {
            let mut tx = db.scoped.begin().await.unwrap();
            let result = if update {
                sqlx::query(INSERT_PLAN_SQL)
                    .bind(p.task_id.as_str())
                    .bind(&valid)
                    .bind(&checkpoint)
                    .execute(&mut *tx)
                    .await
                    .expect("valid update control must insert");
                sqlx::query("UPDATE tasks SET plan=$2 WHERE task_id=$1")
                    .bind(p.task_id.as_str())
                    .bind(&invalid)
                    .execute(&mut *tx)
                    .await
            } else {
                sqlx::query(INSERT_PLAN_SQL)
                    .bind(p.task_id.as_str())
                    .bind(&invalid)
                    .bind(&checkpoint)
                    .execute(&mut *tx)
                    .await
            };
            let mode = if update { "update" } else { "insert" };
            match result {
                Ok(_) => accepted.push(format!("{mode}: {label}")),
                Err(sqlx::Error::Database(error))
                    if error
                        .code()
                        .is_some_and(|code| code.starts_with("23") || code.starts_with("22")) => {}
                Err(_) => unexpected.push(format!("{mode}: {label}")),
            }
            tx.rollback().await.unwrap();
        }
    }
    assert!(
        unexpected.is_empty(),
        "Unexpected database errors: {unexpected:?}"
    );
    assert!(
        accepted.is_empty(),
        "Database accepted invalid plans: {accepted:?}"
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn migration_v2_preserves_v1_checksum_and_existing_recovery_data() {
    let db = TestDb::unmigrated().await;
    let checksum = db.migrate_v1().await;
    let source = TestDb::new().await;
    let task = source.task().await;
    source
        .service
        .save_checkpoint(&source.session, &task.task_id, waiting(1))
        .await
        .unwrap();
    // Populate the real v1 schema using its unchanged task and audit columns.
    db.scoped.execute(format!(
        "INSERT INTO tasks SELECT * FROM {0}.tasks;
         INSERT INTO audit_events SELECT * FROM {0}.audit_events;
         SELECT setval(pg_get_serial_sequence('audit_events','sequence'), (SELECT max(sequence) FROM audit_events));",
        source.namespace
    ).as_str()).await.unwrap();
    source.finish().await;
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=1 AND success")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(stored, checksum);
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&db.scoped)
            .await
            .unwrap();
    assert_eq!(versions, (1..=21).collect::<Vec<_>>());
    let mut tx = db.scoped.begin().await.unwrap();
    let invalid = sqlx::query("UPDATE tasks SET plan='{}'::jsonb WHERE task_id=$1")
        .bind(task.task_id.as_str())
        .execute(&mut *tx)
        .await;
    assert!(
        matches!(invalid, Err(sqlx::Error::Database(ref error)) if error.code().as_deref() == Some("23514"))
    );
    tx.rollback().await.unwrap();
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn migration_v2_rejects_invalid_legacy_data_without_rewriting_it() {
    let db = TestDb::unmigrated().await;
    db.migrate_v1().await;
    sqlx::query(INSERT_PLAN_SQL)
        .bind("invalid_legacy_task")
        .bind(serde_json::json!({}))
        .bind(serde_json::json!({"kind":"AT_NODE", "node_id":"read"}))
        .execute(&db.scoped)
        .await
        .unwrap();
    assert_eq!(db.repo.migrate().await, Err(StorageError::Migration));
    let remaining: serde_json::Value =
        sqlx::query_scalar("SELECT plan FROM tasks WHERE task_id='invalid_legacy_task'")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(remaining, serde_json::json!({}));
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version=2")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(applied, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn completed_tool_node_rejects_new_draft_without_corrupting_recovery() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let waiting = db
        .service
        .save_checkpoint(&db.session, &task.task_id, waiting(1))
        .await
        .unwrap();
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        db.service
            .create_operation(
                &db.session,
                &task.task_id,
                waiting.revision,
                new_operation_id(),
                node("read"),
                1,
            )
            .await,
        Err(StorageError::Invalid("operation node is not succeeded"))
    );
    let after = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(after.task, waiting);
    assert_eq!(after.operations, before.operations);
    assert_eq!(after.directive, before.directive);
    assert_eq!(&after.audit[..before.audit.len()], before.audit.as_slice());
    assert_eq!(after.audit.len(), before.audit.len() + 1);
    assert_eq!(
        after.audit.last().unwrap().kind,
        PersistentAuditKind::ChangeRejected
    );
    assert_eq!(after.audit.last().unwrap().task_revision, waiting.revision);
    assert!(!after
        .audit
        .iter()
        .any(|event| event.kind == PersistentAuditKind::OperationCreated));
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn completed_tool_node_preserves_existing_succeeded_operation_idempotency() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let ready = db.event(&op, OperationEvent::GatesSatisfied, None).await;
    let reference = BusinessOperationRef::parse("original_mock_operation").unwrap();
    let submitted = db
        .event(
            &ready,
            OperationEvent::SubmissionRecorded,
            Some(reference.clone()),
        )
        .await;
    let succeeded = db
        .event(
            &submitted,
            OperationEvent::ExecutionSucceeded,
            Some(reference),
        )
        .await;
    let recorded = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    let running = db
        .service
        .save_checkpoint(
            &db.session,
            &task.task_id,
            CheckpointChange {
                expected_revision: recorded.task.revision,
                event: Some(TaskEvent::ExternalResolved),
                checkpoint: RecoveryPoint::AtNode {
                    node_id: node("choose"),
                },
                completed_nodes: vec![node("read")],
                facts_version: recorded.task.facts_version,
            },
        )
        .await
        .unwrap();
    let mut change = waiting(running.revision);
    change.facts_version = running.facts_version;
    let waiting = db
        .service
        .save_checkpoint(&db.session, &task.task_id, change)
        .await
        .unwrap();
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    let repeated = db
        .service
        .create_operation(
            &db.session,
            &task.task_id,
            waiting.revision,
            op.operation_id,
            node("read"),
            1,
        )
        .await
        .unwrap();
    assert_eq!(repeated, succeeded);
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: run dedicated --ignored command with SAFEFLOW_TEST_DATABASE_URL"]
async fn repeated_migrations_and_exact_round_trip() {
    let db = TestDb::new().await;
    db.repo.migrate().await.unwrap();
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(snapshot.task.plan, task.plan);
    assert_eq!(snapshot.task.owner_id.as_str(), "user_001");
    assert_eq!(snapshot.operations, vec![op]);
    assert_eq!(snapshot.audit.len(), 2);
    assert_eq!(snapshot.audit[0].kind, PersistentAuditKind::TaskCreated);
    assert_eq!(snapshot.audit[1].task_revision, snapshot.task.revision);
    assert_ne!(snapshot.audit[0].event_id, snapshot.audit[1].event_id);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn restart_reads_original_progress_and_operation() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db
        .service
        .create_operation(
            &db.session,
            &task.task_id,
            1,
            new_operation_id(),
            node("save"),
            1,
        )
        .await
        .unwrap();
    db.service
        .save_checkpoint(&db.session, &task.task_id, waiting(2))
        .await
        .unwrap();
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.repo.close().await;
    let fresh_repo = PgRepository::connect(&db.url, &db.namespace).await.unwrap();
    fresh_repo.migrate().await.unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(db.session.clone(), UserId::parse("user_001").unwrap());
    let fresh = PersistenceService::for_trusted_backend(fresh_repo.clone(), Arc::new(sessions));
    let after = fresh.recover(&db.session, &task.task_id).await.unwrap();
    assert_eq!(before, after);
    assert_eq!(after.operations[0], op);
    assert!(matches!(
        after.directive,
        RecoveryDirective::AwaitUser { .. }
    ));
    fresh_repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn concurrent_revision_competition_has_one_winner() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let (a, b) = tokio::join!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, waiting(1)),
        db.service
            .save_checkpoint(&db.session, &task.task_id, waiting(1))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(a == Err(StorageError::RevisionConflict) || b == Err(StorageError::RevisionConflict));
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(snapshot.task.revision, 2);
    assert_eq!(
        snapshot
            .audit
            .iter()
            .filter(|e| e.kind == PersistentAuditKind::CheckpointSaved)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .audit
            .iter()
            .filter(|e| e.kind == PersistentAuditKind::ChangeRejected)
            .count(),
        1
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn owner_and_session_isolation_cover_reads_and_mutations() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    assert_eq!(
        db.service.recover(&db.other, &task.task_id).await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.service
            .save_checkpoint(&db.other, &task.task_id, waiting(2))
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.service
            .create_task(&db.other, task.plan.clone(), 1)
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.service
            .create_operation(
                &db.other,
                &task.task_id,
                2,
                new_operation_id(),
                node("save"),
                1
            )
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.service
            .record_operation_event(
                &db.other,
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: 1,
                    event: OperationEvent::Cancel,
                    facts_version: 2,
                    business_operation_ref: None
                }
            )
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.service
            .replace_plan(&db.other, &task.task_id, 2, task.plan.clone(), 1)
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.service
            .recover(&SessionRef::parse("forged").unwrap(), &task.task_id)
            .await,
        Err(StorageError::Session)
    );
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap()
            .audit
            .len(),
        2
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn concurrent_operation_events_reject_stale_revision_without_duplicate_transition() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let change = OperationChange {
        expected_revision: 1,
        event: OperationEvent::GatesSatisfied,
        facts_version: 2,
        business_operation_ref: None,
    };
    let (a, b) = tokio::join!(
        db.service.record_operation_event(
            &db.session,
            &task.task_id,
            &op.operation_id,
            change.clone()
        ),
        db.service.record_operation_event(
            &db.session,
            &task.task_id,
            &op.operation_id,
            change.clone()
        )
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(a == Err(StorageError::RevisionConflict) || b == Err(StorageError::RevisionConflict));
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(before.operations[0].revision, 2);
    assert_eq!(before.task.revision, 3);
    assert_eq!(
        before
            .audit
            .iter()
            .filter(|e| e.kind == PersistentAuditKind::OperationTransitioned)
            .count(),
        1
    );
    assert_eq!(
        db.service
            .record_operation_event(&db.session, &task.task_id, &op.operation_id, change)
            .await,
        Err(StorageError::RevisionConflict)
    );
    let after = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(after.task, before.task);
    assert_eq!(after.operations, before.operations);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn plans_reject_rollback_same_version_conflict_and_old_facts() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let same = db
        .service
        .replace_plan(&db.session, &task.task_id, 1, task.plan.clone(), 1)
        .await
        .unwrap();
    assert_eq!(same, task);
    let mut conflict = task.plan.clone();
    let PlanNode::Tool { arguments, .. } = &mut conflict.nodes[0] else {
        unreachable!()
    };
    *arguments = serde_json::json!({"amount":"300.00"});
    assert_eq!(
        db.service
            .replace_plan(&db.session, &task.task_id, 1, conflict.clone(), 1)
            .await,
        Err(StorageError::PlanConflict)
    );
    conflict.plan_version = 2;
    let next = db
        .service
        .replace_plan(&db.session, &task.task_id, 1, conflict, 2)
        .await
        .unwrap();
    assert_eq!(next.plan.plan_version, 2);
    assert_eq!(
        db.service
            .replace_plan(&db.session, &task.task_id, 2, task.plan, 2)
            .await,
        Err(StorageError::StalePlan)
    );
    let mut change = waiting(2);
    change.facts_version = 1;
    assert_eq!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, change)
            .await,
        Err(StorageError::FactsConflict)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn illegal_and_terminal_states_never_reopen() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let invalid = OperationChange {
        expected_revision: 1,
        event: OperationEvent::SubmissionRecorded,
        facts_version: 2,
        business_operation_ref: None,
    };
    assert_eq!(
        db.service
            .record_operation_event(&db.session, &task.task_id, &op.operation_id, invalid)
            .await,
        Err(StorageError::IllegalTransition)
    );
    let cancelled = db.event(&op, OperationEvent::Cancel, None).await;
    assert_eq!(cancelled.state, OperationState::Cancelled);
    assert_eq!(
        db.service
            .record_operation_event(
                &db.session,
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: cancelled.revision,
                    event: OperationEvent::GatesSatisfied,
                    facts_version: 3,
                    business_operation_ref: None
                }
            )
            .await,
        Err(StorageError::Terminal)
    );
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    let finish = CheckpointChange {
        expected_revision: snapshot.task.revision,
        event: Some(TaskEvent::Cancel),
        checkpoint: RecoveryPoint::Finished,
        completed_nodes: vec![],
        facts_version: 3,
    };
    let ended = db
        .service
        .save_checkpoint(&db.session, &task.task_id, finish)
        .await
        .unwrap();
    assert_eq!(ended.state, TaskState::Cancelled);
    assert_eq!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, waiting(ended.revision))
            .await,
        Err(StorageError::FactsConflict)
    );
    let mut reopen = waiting(ended.revision);
    reopen.facts_version = 3;
    assert_eq!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, reopen)
            .await,
        Err(StorageError::Terminal)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn submitted_processing_require_original_reconciliation_and_no_new_submission() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let ready = db.event(&op, OperationEvent::GatesSatisfied, None).await;
    let reference = BusinessOperationRef::parse("original_mock_bank_order_001").unwrap();
    let submitted = db
        .event(
            &ready,
            OperationEvent::SubmissionRecorded,
            Some(reference.clone()),
        )
        .await;
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        snapshot.directive,
        RecoveryDirective::ReconcileOriginal {
            operation_id: op.operation_id.clone(),
            business_operation_ref: reference.clone()
        }
    );
    assert_eq!(
        db.service
            .create_operation(
                &db.session,
                &task.task_id,
                snapshot.task.revision,
                new_operation_id(),
                node("save"),
                1
            )
            .await,
        Err(StorageError::ReconcileRequired)
    );
    let processing = db
        .event(
            &submitted,
            OperationEvent::OutcomeUnknown,
            Some(reference.clone()),
        )
        .await;
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(snapshot.operations[0].state, OperationState::Processing);
    let mut change = waiting(snapshot.task.revision);
    change.facts_version = processing.facts_version;
    assert_eq!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, change)
            .await,
        Err(StorageError::IllegalTransition)
    );
    assert_eq!(
        db.service
            .record_operation_event(
                &db.session,
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: processing.revision,
                    event: OperationEvent::SubmissionRecorded,
                    facts_version: 5,
                    business_operation_ref: Some(reference.clone())
                }
            )
            .await,
        Err(StorageError::IllegalTransition)
    );
    let succeeded = db
        .event(
            &processing,
            OperationEvent::StatusConfirmedSucceeded,
            Some(reference),
        )
        .await;
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        snapshot.directive,
        RecoveryDirective::ReviewRecordedResult {
            operation_id: op.operation_id
        }
    );
    assert_eq!(snapshot.operations.len(), 1);
    assert_eq!(succeeded.state, OperationState::Succeeded);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn audit_failure_rolls_back_task_state_and_progress() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.fault_audit(false).await;
    assert_eq!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, waiting(1))
            .await,
        Err(StorageError::Database)
    );
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    db.clear_fault().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn audit_failure_rolls_back_operation_and_task_together() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.fault_audit(false).await;
    assert_eq!(
        db.service
            .record_operation_event(
                &db.session,
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: 1,
                    event: OperationEvent::GatesSatisfied,
                    facts_version: 2,
                    business_operation_ref: None
                }
            )
            .await,
        Err(StorageError::Database)
    );
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    db.clear_fault().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn failed_creation_leaves_no_orphan_and_retry_uses_original_ids() {
    let db = TestDb::new().await;
    let p = plan(new_task_id());
    db.fault_audit(false).await;
    assert_eq!(
        db.service.create_task(&db.session, p.clone(), 1).await,
        Err(StorageError::Database)
    );
    assert_eq!(
        db.service.recover(&db.session, &p.task_id).await,
        Err(StorageError::NotFound)
    );
    db.clear_fault().await;
    let task = db.service.create_task(&db.session, p, 1).await.unwrap();
    let id = new_operation_id();
    db.fault_audit(false).await;
    assert_eq!(
        db.service
            .create_operation(&db.session, &task.task_id, 1, id.clone(), node("read"), 1)
            .await,
        Err(StorageError::Database)
    );
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap()
            .operations
            .len(),
        0
    );
    db.clear_fault().await;
    let op = db
        .service
        .create_operation(&db.session, &task.task_id, 1, id.clone(), node("read"), 1)
        .await
        .unwrap();
    assert_eq!(op.operation_id, id);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn commit_time_failure_rolls_back_state_and_already_inserted_audit() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let before = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.fault_audit(true).await;
    assert_eq!(
        db.service
            .save_checkpoint(&db.session, &task.task_id, waiting(1))
            .await,
        Err(StorageError::Database)
    );
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    db.clear_fault().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn conflicting_ids_and_duplicate_creates_survive_service_reconstruction() {
    let db = TestDb::new().await;
    let task = db.task().await;
    let op = db.op(&task.task_id).await;
    let waiting_task = db
        .service
        .create_task(&db.session, task.plan.clone(), 1)
        .await
        .unwrap();
    assert_eq!(waiting_task.revision, 2);
    assert_eq!(
        db.service
            .recover(&db.session, &task.task_id)
            .await
            .unwrap()
            .audit
            .len(),
        2
    );
    let other_task = db.task().await;
    db.repo.close().await;
    let fresh_repo = PgRepository::connect(&db.url, &db.namespace).await.unwrap();
    fresh_repo.migrate().await.unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(db.session.clone(), UserId::parse("user_001").unwrap());
    let fresh = PersistenceService::for_trusted_backend(fresh_repo.clone(), Arc::new(sessions));
    assert_eq!(
        fresh
            .create_task(&db.session, task.plan.clone(), 1)
            .await
            .unwrap(),
        waiting_task
    );
    let mut other_content = task.plan.clone();
    other_content.plan_version = 2;
    assert_eq!(
        fresh.create_task(&db.session, other_content, 1).await,
        Err(StorageError::IdConflict)
    );
    assert_eq!(
        fresh
            .create_operation(
                &db.session,
                &other_task.task_id,
                1,
                op.operation_id,
                node("read"),
                1
            )
            .await,
        Err(StorageError::IdConflict)
    );
    assert_eq!(
        fresh
            .create_operation(
                &db.session,
                &task.task_id,
                2,
                new_operation_id(),
                node("read"),
                1
            )
            .await,
        Err(StorageError::IdConflict)
    );
    assert_eq!(
        fresh
            .recover(&db.session, &task.task_id)
            .await
            .unwrap()
            .operations
            .len(),
        1
    );
    fresh_repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL: dedicated command"]
async fn invalid_progress_references_and_missing_original_reference_are_rejected() {
    let db = TestDb::new().await;
    assert!(matches!(
        PgRepository::connect(&db.url, "public").await,
        Err(StorageError::Invalid("unsafe storage namespace"))
    ));
    assert!(matches!(
        PgRepository::connect(&db.url, "schema;drop").await,
        Err(StorageError::Invalid("unsafe storage namespace"))
    ));
    let task = db.task().await;
    let mut change = waiting(1);
    change.completed_nodes = vec![node("save")];
    assert!(db
        .service
        .save_checkpoint(&db.session, &task.task_id, change)
        .await
        .is_err());
    let op = db.op(&task.task_id).await;
    let ready = db.event(&op, OperationEvent::GatesSatisfied, None).await;
    assert_eq!(
        db.service
            .record_operation_event(
                &db.session,
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: ready.revision,
                    event: OperationEvent::SubmissionRecorded,
                    facts_version: 3,
                    business_operation_ref: None
                }
            )
            .await,
        Err(StorageError::Invalid("submission needs original reference"))
    );
    let mut p = task.plan;
    p.plan_version = 2;
    let snapshot = db
        .service
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        db.service
            .replace_plan(&db.session, &task.task_id, snapshot.task.revision, p, 3)
            .await,
        Err(StorageError::Invalid(
            "replanning after operation creation is outside this slice"
        ))
    );
    assert_eq!(snapshot.operations[0].state, OperationState::Ready);
    db.finish().await;
}
