use chrono::{DateTime, Utc};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, BusinessOperationRef, ConfirmationRef, NodeId, OperationEvent,
    OperationState, PlanNode, ReasonCode, SecurityError, SessionRef, TaskEvent, ToolName, UserId,
};
use safeflow_runtime::{SessionBinding, SessionResolver};
use safeflow_storage::{
    new_operation_id, new_task_id, CheckpointChange, ConfirmationClock, ConfirmationConfig,
    ConfirmationRecord, ConfirmationService, ConfirmationStatus, OperationChange, OperationRecord,
    PersistenceService, PgRepository, RecoveryPoint, StorageError, TaskRecord,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Default)]
struct Sessions(RwLock<HashMap<SessionRef, SessionBinding>>);
impl Sessions {
    fn bind(&self, session: &SessionRef, user: &str, version: u64) {
        self.0.write().unwrap().insert(
            session.clone(),
            SessionBinding {
                user_id: UserId::parse(user).unwrap(),
                binding_version: version,
            },
        );
    }
    fn revoke(&self, session: &SessionRef) {
        self.0.write().unwrap().remove(session);
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, s: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(s)?.user_id)
    }
    fn resolve_binding(&self, s: &SessionRef) -> Result<SessionBinding, SecurityError> {
        self.0
            .read()
            .unwrap()
            .get(s)
            .cloned()
            .ok_or_else(|| SecurityError {
                reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
                message: "revoked".into(),
            })
    }
}
struct Clock(AtomicI64);
impl Clock {
    fn set(&self, time: DateTime<Utc>) {
        self.0.store(time.timestamp_micros(), Ordering::SeqCst);
    }
    fn tick(&self) {
        self.0.fetch_add(1_000_000, Ordering::SeqCst);
    }
}
impl ConfirmationClock for Clock {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(self.0.load(Ordering::SeqCst)).unwrap()
    }
}

struct Db {
    url: String,
    schema: String,
    admin: PgPool,
    sql: PgPool,
    repo: PgRepository,
    sessions: Arc<Sessions>,
    clock: Arc<Clock>,
    storage: PersistenceService,
    confirmation: ConfirmationService,
    session: SessionRef,
    same_user: SessionRef,
    other: SessionRef,
}
impl Db {
    async fn new() -> Self {
        let db = Self::unmigrated().await;
        db.repo.migrate().await.unwrap();
        db
    }
    async fn unmigrated() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("PostgreSQL configuration required; no checks skipped");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("isolated database connection failed"));
        let schema = format!("sf_confirmation_test_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let search_path = schema.clone();
        let sql = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |conn, _| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("schema connection failed"));
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        let sessions = Arc::new(Sessions::default());
        let session = SessionRef::parse("isolated_server_secret_A").unwrap();
        let same_user = SessionRef::parse("isolated_server_secret_B").unwrap();
        let other = SessionRef::parse("isolated_server_secret_C").unwrap();
        sessions.bind(&session, "user_001", 1);
        sessions.bind(&same_user, "user_001", 1);
        sessions.bind(&other, "user_002", 1);
        let clock = Arc::new(Clock(AtomicI64::new(1_791_172_000_000_000)));
        let storage = PersistenceService::for_trusted_backend(repo.clone(), sessions.clone());
        let confirmation = Self::service(repo.clone(), sessions.clone(), clock.clone());
        Self {
            url,
            schema,
            admin,
            sql,
            repo,
            sessions,
            clock,
            storage,
            confirmation,
            session,
            same_user,
            other,
        }
    }
    fn service(
        repo: PgRepository,
        sessions: Arc<Sessions>,
        clock: Arc<Clock>,
    ) -> ConfirmationService {
        // Test configuration only; this does not define a product TTL.
        ConfirmationService::for_trusted_backend_with_clock(
            repo,
            sessions,
            ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
            clock,
        )
    }
    async fn fixture(&self) -> (TaskRecord, OperationRecord) {
        let task = self.storage.create_task(&self.session, AgentPlan::new(new_task_id(),1,vec![
            PlanNode::UserDecision {id:node("review"),depends_on:vec![]},
            PlanNode::Tool {id:node("snapshot"),tool:ToolName::parse("foundation_snapshot_only").unwrap(),
                arguments:serde_json::json!({"amount":"200.00","label":"foundation storage fixture only"}),depends_on:vec![node("review")]},
        ]).unwrap(),1).await.unwrap();
        let op = self
            .storage
            .create_operation(
                &self.session,
                &task.task_id,
                task.revision,
                new_operation_id(),
                node("snapshot"),
                1,
            )
            .await
            .unwrap();
        let task = self
            .storage
            .recover(&self.session, &task.task_id)
            .await
            .unwrap()
            .task;
        (task, op)
    }
    async fn prepare(&self) -> ConfirmationRecord {
        let (task, op) = self.fixture().await;
        self.confirmation
            .prepare(&self.session, &task.task_id, &op.operation_id)
            .await
            .unwrap()
    }
    async fn confirm(&self, c: &ConfirmationRecord) -> Result<ConfirmationRecord, StorageError> {
        self.confirmation
            .record_explicit_confirmation(
                &self.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding,
            )
            .await
    }
    async fn verify(&self, c: &ConfirmationRecord) -> Result<ConfirmationRecord, StorageError> {
        self.confirmation
            .verify(
                &self.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding,
            )
            .await
    }
    async fn count(&self, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE kind=$1")
            .bind(kind)
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn stored_status(&self, c: &ConfirmationRecord) -> String {
        sqlx::query_scalar("SELECT status FROM server_confirmations WHERE confirmation_ref=$1")
            .bind(c.confirmation_ref.as_str())
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn event(
        &self,
        op: &OperationRecord,
        event: OperationEvent,
        reference: Option<BusinessOperationRef>,
    ) -> OperationRecord {
        self.storage
            .record_operation_event(
                &self.session,
                &op.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: op.revision,
                    event,
                    facts_version: op.facts_version,
                    business_operation_ref: reference,
                },
            )
            .await
            .unwrap()
    }
    async fn fault(&self, table: &str, deferred: bool) {
        assert!(matches!(table, "audit_events" | "server_confirmations"));
        self.sql.execute("CREATE FUNCTION injected_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated failure'; END $$").await.unwrap();
        let query = if deferred {
            format!("CREATE CONSTRAINT TRIGGER reject_write AFTER INSERT OR UPDATE ON {table} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION injected_failure()")
        } else {
            format!("CREATE TRIGGER reject_write BEFORE INSERT OR UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION injected_failure()")
        };
        self.sql.execute(query.as_str()).await.unwrap();
    }
    async fn clear_fault(&self, table: &str) {
        self.sql
            .execute(format!("DROP TRIGGER reject_write ON {table}").as_str())
            .await
            .unwrap();
        self.sql
            .execute("DROP FUNCTION injected_failure()")
            .await
            .unwrap();
    }
    async fn finish(self) {
        self.repo.close().await;
        self.sql.close().await;
        assert!(self.schema.starts_with("sf_confirmation_test_") && self.schema.len() == 53);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn node(s: &str) -> NodeId {
    NodeId::parse(s).unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn prepares_confirms_reads_and_verifies_without_granting_execution() {
    let db = Db::new().await;
    let c = db.prepare().await;
    assert_eq!(c.status, ConfirmationStatus::Prepared);
    assert_eq!(db.verify(&c).await, Err(StorageError::ConfirmationRequired));
    db.clock.tick();
    let confirmed = db.confirm(&c).await.unwrap();
    assert_eq!(confirmed.status, ConfirmationStatus::Confirmed);
    assert_eq!(db.verify(&c).await.unwrap(), confirmed);
    assert_eq!(
        db.confirmation
            .read(&db.session, &c.binding.task_id, &c.confirmation_ref)
            .await
            .unwrap(),
        confirmed
    );
    let snapshot = db
        .storage
        .recover(&db.session, &c.binding.task_id)
        .await
        .unwrap();
    assert_eq!(snapshot.operations[0].state, OperationState::Draft);
    assert_eq!(snapshot.task.revision, c.binding.task_revision);
    let text = serde_json::to_string(&confirmed).unwrap()
        + &serde_json::to_string(&snapshot.audit).unwrap();
    for secret in [
        db.session.as_str(),
        "session_digest",
        "session_binding_version",
    ] {
        assert!(!text.contains(secret));
    }
    let stored_json: String = sqlx::query_scalar(
        "SELECT row_to_json(c)::text FROM server_confirmations c WHERE confirmation_ref=$1",
    )
    .bind(c.confirmation_ref.as_str())
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert!(!stored_json.contains(db.session.as_str()));
    assert_eq!(db.count("CONFIRMATION_PREPARED").await, 1);
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 1);
    db.clock.set(c.prepared_at);
    assert!(matches!(db.verify(&c).await, Err(StorageError::Invalid(_))));
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn preparation_and_explicit_confirmation_replays_do_not_extend_or_reaudit() {
    let db = Db::new().await;
    let c = db.prepare().await;
    db.clock.tick();
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
            .await
            .unwrap(),
        c
    );
    let confirmed = db.confirm(&c).await.unwrap();
    db.clock.tick();
    assert_eq!(db.confirm(&c).await.unwrap(), confirmed);
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
            .await
            .unwrap(),
        confirmed
    );
    assert_eq!(confirmed.expires_at, c.expires_at);
    assert_eq!(db.count("CONFIRMATION_PREPARED").await, 1);
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn revoked_records_cannot_be_revived_and_revocation_is_idempotent() {
    let db = Db::new().await;
    let c = db.prepare().await;
    db.confirm(&c).await.unwrap();
    let revoked = db
        .confirmation
        .revoke(&db.session, &c.binding.task_id, &c.confirmation_ref)
        .await
        .unwrap();
    db.clock.tick();
    assert_eq!(
        db.confirmation
            .revoke(&db.session, &c.binding.task_id, &c.confirmation_ref)
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(db.confirm(&c).await, Err(StorageError::ConfirmationRevoked));
    assert_eq!(db.verify(&c).await, Err(StorageError::ConfirmationRevoked));
    assert_eq!(
        db.confirmation
            .read(&db.session, &c.binding.task_id, &c.confirmation_ref)
            .await,
        Err(StorageError::ConfirmationRevoked)
    );
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
            .await,
        Err(StorageError::ConfirmationRevoked)
    );
    assert_eq!(db.count("CONFIRMATION_REVOKED").await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn expiry_equal_is_rejected_using_timestamp_round_tripped_through_postgres() {
    let db = Db::new().await;
    let c = db.prepare().await;
    let expiry: DateTime<Utc> =
        sqlx::query_scalar("SELECT expires_at FROM server_confirmations WHERE confirmation_ref=$1")
            .bind(c.confirmation_ref.as_str())
            .fetch_one(&db.sql)
            .await
            .unwrap();
    db.clock.set(expiry - chrono::Duration::microseconds(1));
    db.confirm(&c).await.unwrap();
    db.clock.set(expiry);
    assert_eq!(db.confirm(&c).await, Err(StorageError::ConfirmationExpired));
    assert_eq!(db.verify(&c).await, Err(StorageError::ConfirmationExpired));
    assert_eq!(
        db.confirmation
            .read(&db.session, &c.binding.task_id, &c.confirmation_ref)
            .await,
        Err(StorageError::ConfirmationExpired)
    );
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
            .await,
        Err(StorageError::ConfirmationExpired)
    );
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cross_user_other_session_wrong_task_and_forged_ref_are_rejected() {
    let db = Db::new().await;
    let c = db.prepare().await;
    for session in [&db.other, &db.same_user] {
        assert_eq!(
            db.confirmation
                .record_explicit_confirmation(
                    session,
                    &c.binding.task_id,
                    &c.confirmation_ref,
                    &c.binding
                )
                .await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.confirmation
                .read(session, &c.binding.task_id, &c.confirmation_ref)
                .await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.confirmation
                .revoke(session, &c.binding.task_id, &c.confirmation_ref)
                .await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.confirmation
                .verify(session, &c.binding.task_id, &c.confirmation_ref, &c.binding)
                .await,
            Err(StorageError::NotFound)
        );
    }
    assert_eq!(
        db.confirmation
            .prepare(&db.other, &c.binding.task_id, &c.binding.operation_id)
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.confirmation
            .record_explicit_confirmation(
                &db.session,
                &new_task_id(),
                &c.confirmation_ref,
                &c.binding
            )
            .await,
        Err(StorageError::NotFound)
    );
    let forged = ConfirmationRef::parse(format!("confirmation_{}", uuid::Uuid::new_v4())).unwrap();
    assert_eq!(
        db.confirmation
            .record_explicit_confirmation(&db.session, &c.binding.task_id, &forged, &c.binding)
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn renewed_rebound_and_revoked_sessions_cannot_use_original_confirmation() {
    let db = Db::new().await;
    let c = db.prepare().await;
    db.confirm(&c).await.unwrap();
    db.sessions.bind(&db.session, "user_001", 2);
    assert_eq!(db.verify(&c).await, Err(StorageError::NotFound));
    assert_eq!(db.confirm(&c).await, Err(StorageError::NotFound));
    db.sessions.bind(&db.session, "user_002", 3);
    assert_eq!(db.verify(&c).await, Err(StorageError::NotFound));
    db.sessions.revoke(&db.session);
    assert_eq!(db.verify(&c).await, Err(StorageError::Session));
    db.finish().await;
}

struct LegacyResolver;
impl SessionResolver for LegacyResolver {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(UserId::parse("user_001").unwrap())
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn legacy_resolver_without_versioned_binding_fails_closed() {
    let db = Db::new().await;
    let c = db.prepare().await;
    let service = ConfirmationService::for_trusted_backend(
        db.repo.clone(),
        Arc::new(LegacyResolver),
        ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
    );
    assert_eq!(
        service
            .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
            .await,
        Err(StorageError::Session)
    );
    assert_eq!(
        service
            .record_explicit_confirmation(
                &db.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding
            )
            .await,
        Err(StorageError::Session)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn every_displayed_binding_field_is_compared_before_confirmation() {
    let db = Db::new().await;
    let c = db.prepare().await;
    for field in 0..10 {
        let mut changed = c.binding.clone();
        match field {
            0 => changed.user_id = UserId::parse("other").unwrap(),
            1 => changed.task_id = new_task_id(),
            2 => changed.operation_id = new_operation_id(),
            3 => changed.node_id = node("other"),
            4 => changed.tool_name = ToolName::parse("other_tool").unwrap(),
            5 => changed.task_revision += 1,
            6 => changed.plan_version += 1,
            7 => changed.operation_revision += 1,
            8 => changed.facts_version += 1,
            _ => changed.arguments_hash = ArgumentsHash::parse("0".repeat(64)).unwrap(),
        }
        assert_eq!(
            db.confirmation
                .record_explicit_confirmation(
                    &db.session,
                    &c.binding.task_id,
                    &c.confirmation_ref,
                    &changed
                )
                .await,
            Err(StorageError::ConfirmationBindingMismatch)
        );
    }
    assert_eq!(db.stored_status(&c).await, "PREPARED");
    db.confirm(&c).await.unwrap();
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn database_argument_changes_cannot_silently_confirm_new_content() {
    let db = Db::new().await;
    let c = db.prepare().await;
    sqlx::query("UPDATE tasks SET plan=jsonb_set(plan,'{nodes,1,arguments,amount}','\"300.00\"'::jsonb) WHERE task_id=$1").bind(c.binding.task_id.as_str()).execute(&db.sql).await.unwrap();
    assert_eq!(
        db.confirm(&c).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
            .await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    assert_eq!(db.stored_status(&c).await, "PREPARED");
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn plan_and_node_changes_in_database_invalidate_original_binding() {
    let db = Db::new().await;
    let c = db.prepare().await;
    db.confirm(&c).await.unwrap();
    sqlx::query("UPDATE tasks SET plan=jsonb_set(plan,'{plan_version}','2'),plan_version=2 WHERE task_id=$1").bind(c.binding.task_id.as_str()).execute(&db.sql).await.unwrap();
    assert_eq!(db.verify(&c).await, Err(StorageError::StalePlan));
    sqlx::query("UPDATE operations SET plan_version=2 WHERE operation_id=$1")
        .bind(c.binding.operation_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        db.verify(&c).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    let next = db
        .confirmation
        .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
        .await
        .unwrap();
    assert_ne!(next.confirmation_ref, c.confirmation_ref);
    assert_eq!(
        db.confirm(&c).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    db.confirm(&next).await.unwrap();
    sqlx::query(
        "UPDATE tasks SET plan=jsonb_set(plan,'{nodes,1,id}','\"new_snapshot\"') WHERE task_id=$1",
    )
    .bind(c.binding.task_id.as_str())
    .execute(&db.sql)
    .await
    .unwrap();
    sqlx::query("UPDATE operations SET node_id='new_snapshot' WHERE operation_id=$1")
        .bind(c.binding.operation_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        db.verify(&next).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn current_fact_and_task_revisions_are_database_truth() {
    let db = Db::new().await;
    let c = db.prepare().await;
    db.confirm(&c).await.unwrap();
    db.storage
        .save_checkpoint(
            &db.session,
            &c.binding.task_id,
            CheckpointChange {
                expected_revision: c.binding.task_revision,
                event: Some(TaskEvent::AwaitUser),
                checkpoint: RecoveryPoint::AtNode {
                    node_id: node("review"),
                },
                completed_nodes: vec![],
                facts_version: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(db.verify(&c).await, Err(StorageError::FactsConflict));
    sqlx::query("UPDATE operations SET facts_version=2 WHERE operation_id=$1")
        .bind(c.binding.operation_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        db.verify(&c).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    let next = db
        .confirmation
        .prepare(&db.session, &c.binding.task_id, &c.binding.operation_id)
        .await
        .unwrap();
    assert_eq!(next.binding.facts_version, 2);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn operation_revision_changes_require_a_new_display_and_confirmation() {
    let db = Db::new().await;
    let (task, op) = db.fixture().await;
    let c = db
        .confirmation
        .prepare(&db.session, &task.task_id, &op.operation_id)
        .await
        .unwrap();
    db.storage
        .save_checkpoint(
            &db.session,
            &task.task_id,
            CheckpointChange {
                expected_revision: task.revision,
                event: None,
                checkpoint: RecoveryPoint::AtNode {
                    node_id: node("snapshot"),
                },
                completed_nodes: vec![node("review")],
                facts_version: 1,
            },
        )
        .await
        .unwrap();
    let ready = db.event(&op, OperationEvent::GatesSatisfied, None).await;
    assert_eq!(ready.state, OperationState::Ready);
    assert_eq!(
        db.confirm(&c).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    let next = db
        .confirmation
        .prepare(&db.session, &task.task_id, &op.operation_id)
        .await
        .unwrap();
    db.confirm(&next).await.unwrap();
    assert_eq!(
        db.verify(&c).await,
        Err(StorageError::ConfirmationBindingMismatch)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn submitted_and_processing_operations_cannot_be_confirmed_or_verified() {
    let db = Db::new().await;
    let (task, op) = db.fixture().await;
    db.storage
        .save_checkpoint(
            &db.session,
            &task.task_id,
            CheckpointChange {
                expected_revision: task.revision,
                event: None,
                checkpoint: RecoveryPoint::AtNode {
                    node_id: node("snapshot"),
                },
                completed_nodes: vec![node("review")],
                facts_version: 1,
            },
        )
        .await
        .unwrap();
    let ready = db.event(&op, OperationEvent::GatesSatisfied, None).await;
    let c = db
        .confirmation
        .prepare(&db.session, &task.task_id, &op.operation_id)
        .await
        .unwrap();
    db.confirm(&c).await.unwrap();
    // Trusted state observation fixture only. No bank Tool or funds run here.
    let original = BusinessOperationRef::parse("isolated_observation_only").unwrap();
    let submitted = db
        .event(
            &ready,
            OperationEvent::SubmissionRecorded,
            Some(original.clone()),
        )
        .await;
    assert_eq!(db.verify(&c).await, Err(StorageError::ReconcileRequired));
    assert_eq!(db.confirm(&c).await, Err(StorageError::ReconcileRequired));
    db.event(&submitted, OperationEvent::OutcomeUnknown, Some(original))
        .await;
    assert_eq!(db.verify(&c).await, Err(StorageError::ReconcileRequired));
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &task.task_id, &op.operation_id)
            .await,
        Err(StorageError::ReconcileRequired)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn terminal_task_or_operation_rejects_confirmation_but_allows_explicit_cleanup() {
    let db = Db::new().await;
    let (task, op) = db.fixture().await;
    let c = db
        .confirmation
        .prepare(&db.session, &task.task_id, &op.operation_id)
        .await
        .unwrap();
    db.event(&op, OperationEvent::Cancel, None).await;
    assert_eq!(db.confirm(&c).await, Err(StorageError::Terminal));
    db.confirmation
        .revoke(&db.session, &task.task_id, &c.confirmation_ref)
        .await
        .unwrap();
    let c = db.prepare().await;
    db.storage
        .save_checkpoint(
            &db.session,
            &c.binding.task_id,
            CheckpointChange {
                expected_revision: c.binding.task_revision,
                event: Some(TaskEvent::Cancel),
                checkpoint: RecoveryPoint::Finished,
                completed_nodes: vec![],
                facts_version: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(db.confirm(&c).await, Err(StorageError::Terminal));
    db.clock.set(c.expires_at);
    db.confirmation
        .revoke(&db.session, &c.binding.task_id, &c.confirmation_ref)
        .await
        .unwrap();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn missing_operations_and_nested_float_arguments_are_not_prepared() {
    let db = Db::new().await;
    let (task, op) = db.fixture().await;
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &task.task_id, &new_operation_id())
            .await,
        Err(StorageError::NotFound)
    );
    sqlx::query("UPDATE tasks SET plan=jsonb_set(plan,'{nodes,1,arguments}','{\"nested\":[200.5]}'::jsonb) WHERE task_id=$1").bind(task.task_id.as_str()).execute(&db.sql).await.unwrap();
    assert!(matches!(
        db.confirmation
            .prepare(&db.session, &task.task_id, &op.operation_id)
            .await,
        Err(StorageError::Invalid(_))
    ));
    assert_eq!(db.count("CONFIRMATION_PREPARED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_preparation_and_confirmation_across_connections_have_one_record_and_audit() {
    let db = Db::new().await;
    let (task, op) = db.fixture().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    let service = Db::service(repo.clone(), db.sessions.clone(), db.clock.clone());
    let mut jobs = vec![];
    for i in 0..12 {
        let service = if i % 2 == 0 {
            service.clone()
        } else {
            db.confirmation.clone()
        };
        let session = db.session.clone();
        let task_id = task.task_id.clone();
        let operation_id = op.operation_id.clone();
        jobs.push(tokio::spawn(async move {
            service.prepare(&session, &task_id, &operation_id).await
        }));
    }
    let mut records = vec![];
    for job in jobs {
        records.push(job.await.unwrap().unwrap());
    }
    let c = records[0].clone();
    assert!(records.iter().all(|r| r == &c));
    let mut jobs = vec![];
    for i in 0..12 {
        let service = if i % 2 == 0 {
            service.clone()
        } else {
            db.confirmation.clone()
        };
        let session = db.session.clone();
        let c = c.clone();
        jobs.push(tokio::spawn(async move {
            service
                .record_explicit_confirmation(
                    &session,
                    &c.binding.task_id,
                    &c.confirmation_ref,
                    &c.binding,
                )
                .await
        }));
    }
    let first = jobs.remove(0).await.unwrap().unwrap();
    for job in jobs {
        assert_eq!(job.await.unwrap().unwrap(), first);
    }
    assert_eq!(db.count("CONFIRMATION_PREPARED").await, 1);
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 1);
    repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_confirmation_and_revocation_serialize_without_reviving_the_record() {
    let db = Db::new().await;
    let c = db.prepare().await;
    let (confirmed, revoked) = tokio::join!(
        db.confirm(&c),
        db.confirmation
            .revoke(&db.session, &c.binding.task_id, &c.confirmation_ref)
    );
    assert_eq!(revoked.unwrap().status, ConfirmationStatus::Revoked);
    assert!(confirmed.is_ok() || confirmed == Err(StorageError::ConfirmationRevoked));
    assert_eq!(db.verify(&c).await, Err(StorageError::ConfirmationRevoked));
    assert_eq!(db.count("CONFIRMATION_REVOKED").await, 1);
    assert!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await <= 1);
    db.finish().await;
}

async fn wait_for_blocked(db: &Db, pid: i32) {
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
    panic!("test did not observe database lock wait");
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn session_revoke_and_same_user_rebind_during_task_lock_wait_roll_back() {
    let db = Db::new().await;
    for mode in 0..2 {
        let c = db.prepare().await;
        let mut lock = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(c.binding.task_id.as_str())
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let service = db.confirmation.clone();
        let session = db.session.clone();
        let request = c.clone();
        let job = tokio::spawn(async move {
            service
                .record_explicit_confirmation(
                    &session,
                    &request.binding.task_id,
                    &request.confirmation_ref,
                    &request.binding,
                )
                .await
        });
        wait_for_blocked(&db, pid).await;
        db.sessions.revoke(&db.session);
        if mode == 1 {
            db.sessions.bind(&db.session, "user_001", 3);
        }
        lock.commit().await.unwrap();
        assert_eq!(job.await.unwrap(), Err(StorageError::Session));
        assert_eq!(db.stored_status(&c).await, "PREPARED");
        db.sessions.bind(&db.session, "user_001", mode * 2 + 2);
    }
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn expiry_and_state_change_during_lock_wait_are_rechecked() {
    let db = Db::new().await;
    for mode in 0..2 {
        let c = db.prepare().await;
        let mut lock = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(c.binding.task_id.as_str())
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let service = db.confirmation.clone();
        let session = db.session.clone();
        let request = c.clone();
        let job = tokio::spawn(async move {
            service
                .record_explicit_confirmation(
                    &session,
                    &request.binding.task_id,
                    &request.confirmation_ref,
                    &request.binding,
                )
                .await
        });
        wait_for_blocked(&db, pid).await;
        let error = if mode == 0 {
            db.clock.set(c.expires_at);
            StorageError::ConfirmationExpired
        } else {
            sqlx::query("UPDATE tasks SET revision=revision+1 WHERE task_id=$1")
                .bind(c.binding.task_id.as_str())
                .execute(&mut *lock)
                .await
                .unwrap();
            StorageError::ConfirmationBindingMismatch
        };
        lock.commit().await.unwrap();
        assert_eq!(job.await.unwrap(), Err(error));
        assert_eq!(db.stored_status(&c).await, "PREPARED");
    }
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 0);
    db.finish().await;
}

async fn rollback_at_audit(deferred: bool) {
    let db = Db::new().await;
    let (task, op) = db.fixture().await;
    let before = db
        .storage
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.fault("audit_events", deferred).await;
    assert_eq!(
        db.confirmation
            .prepare(&db.session, &task.task_id, &op.operation_id)
            .await,
        Err(StorageError::Database)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM server_confirmations")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.clear_fault("audit_events").await;
    assert_eq!(
        db.storage
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    let c = db
        .confirmation
        .prepare(&db.session, &task.task_id, &op.operation_id)
        .await
        .unwrap();
    db.fault("audit_events", deferred).await;
    assert_eq!(db.confirm(&c).await, Err(StorageError::Database));
    assert_eq!(db.stored_status(&c).await, "PREPARED");
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 0);
    db.clear_fault("audit_events").await;
    let confirmed = db.confirm(&c).await.unwrap();
    db.fault("audit_events", deferred).await;
    assert_eq!(
        db.confirmation
            .revoke(&db.session, &task.task_id, &c.confirmation_ref)
            .await,
        Err(StorageError::Database)
    );
    assert_eq!(db.stored_status(&c).await, "CONFIRMED");
    assert_eq!(db.count("CONFIRMATION_REVOKED").await, 0);
    db.clear_fault("audit_events").await;
    assert_eq!(db.verify(&c).await.unwrap(), confirmed);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn immediate_audit_failure_rolls_back_prepare_confirm_and_revoke() {
    rollback_at_audit(false).await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn deferred_commit_audit_failure_rolls_back_prepare_confirm_and_revoke() {
    rollback_at_audit(true).await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn confirmation_storage_failure_cannot_emit_success_audit() {
    let db = Db::new().await;
    let c = db.prepare().await;
    db.fault("server_confirmations", false).await;
    assert_eq!(db.confirm(&c).await, Err(StorageError::Database));
    assert_eq!(db.stored_status(&c).await, "PREPARED");
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 0);
    db.clear_fault("server_confirmations").await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn session_revocation_during_audit_wait_rolls_back_already_updated_confirmation() {
    let db = Db::new().await;
    let c = db.prepare().await;
    let mut blocker = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(987654321)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    db.sql.execute("CREATE FUNCTION wait_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(987654321); RETURN NEW; END $$").await.unwrap();
    db.sql.execute("CREATE TRIGGER wait_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION wait_audit()").await.unwrap();
    let service = db.confirmation.clone();
    let session = db.session.clone();
    let request = c.clone();
    let job = tokio::spawn(async move {
        service
            .record_explicit_confirmation(
                &session,
                &request.binding.task_id,
                &request.confirmation_ref,
                &request.binding,
            )
            .await
    });
    wait_for_blocked(&db, pid).await;
    db.sessions.revoke(&db.session);
    blocker.commit().await.unwrap();
    assert_eq!(job.await.unwrap(), Err(StorageError::Session));
    assert_eq!(db.stored_status(&c).await, "PREPARED");
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 0);
    db.sql
        .execute("DROP TRIGGER wait_audit ON audit_events")
        .await
        .unwrap();
    db.sql.execute("DROP FUNCTION wait_audit()").await.unwrap();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn new_connection_restores_same_confirmation_and_revocation() {
    let db = Db::new().await;
    let c = db.prepare().await;
    let confirmed = db.confirm(&c).await.unwrap();
    db.repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    repo.migrate().await.unwrap();
    let service = Db::service(repo.clone(), db.sessions.clone(), db.clock.clone());
    assert_eq!(
        service
            .verify(
                &db.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding
            )
            .await
            .unwrap(),
        confirmed
    );
    service
        .revoke(&db.session, &c.binding.task_id, &c.confirmation_ref)
        .await
        .unwrap();
    repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    let service = Db::service(repo.clone(), db.sessions.clone(), db.clock.clone());
    assert_eq!(
        service
            .record_explicit_confirmation(
                &db.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding
            )
            .await,
        Err(StorageError::ConfirmationRevoked)
    );
    repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn migration_v3_preserves_existing_v2_data_and_checksums() {
    let db = Db::unmigrated().await;
    let migrations = vec![
        sqlx::migrate::Migration::new(
            1,
            "recovery points".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0001_recovery_points.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        ),
        sqlx::migrate::Migration::new(
            2,
            "plan identity version".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0002_plan_identity_version.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        ),
    ];
    let checksums = migrations
        .iter()
        .map(|m| m.checksum.to_vec())
        .collect::<Vec<_>>();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&db.sql)
    .await
    .unwrap();
    let source = Db::new().await;
    let (task, _) = source.fixture().await;
    // Populate only the v2 tables; the current writer requires the latest claim table.
    db.sql.execute(format!(
        "INSERT INTO tasks SELECT * FROM {0}.tasks;
         INSERT INTO operations SELECT * FROM {0}.operations;
         INSERT INTO audit_events SELECT * FROM {0}.audit_events;
         SELECT setval(pg_get_serial_sequence('audit_events','sequence'), (SELECT max(sequence) FROM audit_events));",
        source.schema
    ).as_str()).await.unwrap();
    source.finish().await;
    let before = db
        .storage
        .recover(&db.session, &task.task_id)
        .await
        .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    // The original v1/v2 checksums below remain exact; the single migration chain
    // now also includes the billing preparation recovery candidate (27).
    assert_eq!(versions, (1..=27).collect::<Vec<_>>());
    assert_eq!(
        db.storage
            .recover(&db.session, &task.task_id)
            .await
            .unwrap(),
        before
    );
    let stored: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT checksum FROM _sqlx_migrations WHERE version<3 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(stored, checksums);
    assert_eq!(db.count("OPERATION_CREATED").await, 1);
    db.finish().await;
}
