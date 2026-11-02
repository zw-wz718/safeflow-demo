// Public-contract storage fixtures only: no business projector, auth provider or Tool.
// This child module alone can construct the otherwise sealed verified handoff.
use super::*;
#[path = "audit_sender_pg_tests.rs"]
mod audit_sender_pg;
#[path = "daily_usage_tests.rs"]
mod daily_usage;
#[path = "integration_batch_tests.rs"]
mod integration_batch;
#[path = "public_execution_tests.rs"]
mod public_execution;
#[path = "transfer_sql_tests.rs"]
mod transfer_sql_tests;
use crate::{
    new_operation_id, new_task_id, ConfirmationConfig, ConfirmationService, OperationChange,
    PersistenceService,
};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, NodeId, OperationEvent, PlanNode, ReasonCode, ResourceId,
    SecurityError, ToolName, UserId,
};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    RwLock,
};

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
    fn set(&self, millis: i64) {
        self.0.store(millis, Ordering::SeqCst);
    }
    fn get(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl ConfirmationClock for Clock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.get()).unwrap()
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
    store: PermitStore,
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
    async fn current44() -> Self {
        let db=Self::new().await;
        current_maintenance_fixture(&db.sql,&db.schema).await;
        db
    }
    async fn unmigrated() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("PostgreSQL configuration required; no tests skipped");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("isolated database connection failed"));
        let schema = format!("sf_permit_test_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let path = schema.clone();
        let sql = PgPoolOptions::new()
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
            .unwrap_or_else(|_| panic!("schema connection failed"));
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        let sessions = Arc::new(Sessions::default());
        let session = SessionRef::parse("isolated_permit_secret_A").unwrap();
        let same_user = SessionRef::parse("isolated_permit_secret_B").unwrap();
        let other = SessionRef::parse("isolated_permit_secret_C").unwrap();
        sessions.bind(&session, "user_001", 1);
        sessions.bind(&same_user, "user_001", 1);
        sessions.bind(&other, "user_002", 1);
        let clock = Arc::new(Clock(AtomicI64::new(1_791_172_000_000)));
        let storage = PersistenceService::for_trusted_backend(repo.clone(), sessions.clone());
        let confirmation = ConfirmationService::for_trusted_backend_with_clock(
            repo.clone(),
            sessions.clone(),
            ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
            clock.clone(),
        );
        let store = Self::store(repo.clone(), sessions.clone(), clock.clone());
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
            store,
            session,
            same_user,
            other,
        }
    }
    fn store(repo: PgRepository, sessions: Arc<Sessions>, clock: Arc<Clock>) -> PermitStore {
        // Explicit isolated test TTL, not a product setting.
        PermitStore::for_trusted_backend_with_clock(
            repo,
            sessions,
            PermitConfig::new(Duration::from_secs(20)).unwrap(),
            clock,
        )
    }
    async fn fixture(&self) -> VerifiedPermitSnapshot {
        self.fixture_facts(1).await
    }
    async fn fixture_facts(&self, facts: u64) -> VerifiedPermitSnapshot {
        let args = json!({"value":"200.00","label":"foundation storage fixture"});
        self.fixture_parameters(
            facts,
            new_task_id(),
            args,
            ResourceId::parse("foundation_resource_001").unwrap(),
        )
        .await
    }
    async fn fixture_parameters(
        &self,
        facts: u64,
        task_id: TaskId,
        args: serde_json::Value,
        resource: ResourceId,
    ) -> VerifiedPermitSnapshot {
        let task = self
            .storage
            .create_task(
                &self.session,
                AgentPlan::new(
                    task_id,
                    1,
                    vec![
                        PlanNode::UserDecision {
                            id: node("review"),
                            depends_on: vec![],
                        },
                        PlanNode::Tool {
                            id: node("snapshot"),
                            tool: ToolName::parse("foundation_snapshot_only").unwrap(),
                            arguments: args.clone(),
                            depends_on: vec![node("review")],
                        },
                    ],
                )
                .unwrap(),
                facts,
            )
            .await
            .unwrap();
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
        // Trusted storage observation fixture. This does not verify any real gates.
        let latest = self
            .storage
            .recover(&self.session, &task.task_id)
            .await
            .unwrap()
            .task;
        self.storage
            .save_checkpoint(
                &self.session,
                &task.task_id,
                crate::CheckpointChange {
                    expected_revision: latest.revision,
                    event: None,
                    checkpoint: RecoveryPoint::AtNode {
                        node_id: node("snapshot"),
                    },
                    completed_nodes: vec![node("review")],
                    facts_version: facts,
                },
            )
            .await
            .unwrap();
        self.storage
            .record_operation_event(
                &self.session,
                &task.task_id,
                &op.operation_id,
                OperationChange {
                    expected_revision: op.revision,
                    event: OperationEvent::GatesSatisfied,
                    facts_version: facts,
                    business_operation_ref: None,
                },
            )
            .await
            .unwrap();
        let c = self
            .confirmation
            .prepare(&self.session, &task.task_id, &op.operation_id)
            .await
            .unwrap();
        let c = self
            .confirmation
            .record_explicit_confirmation(
                &self.session,
                &task.task_id,
                &c.confirmation_ref,
                &c.binding,
            )
            .await
            .unwrap();
        VerifiedPermitSnapshot {
            payload: FingerprintPayloadV1 {
                user_id: c.binding.user_id.clone(),
                operation_id: op.operation_id,
                tool_name: c.binding.tool_name.clone(),
                resource_id: resource,
                normalized_arguments: args,
                plan_version: 1,
                schema_version: 1,
            },
            binding: c.binding,
            decision_id: DecisionId::parse("isolated_contract_decision").unwrap(),
            confirmation_ref: c.confirmation_ref,
            required_gates: vec![
                SecurityGate::ExplicitConfirmation,
                SecurityGate::StrongAuthentication,
            ],
            authentication: VerifiedPermitAuthentication::Verified {
                grant_ref: AuthGrantRef::parse("isolated_contract_auth_reference").unwrap(),
            },
            execution_scope: VerifiedPermitScope::Common,
            session_digest: hash(&("safeflow-confirmation-session:v1", self.session.as_str()))
                .unwrap()
                .as_str()
                .to_owned(),
            session_version: 1,
        }
    }
    async fn save(&self, v: &VerifiedPermitSnapshot) -> StoredExecutionPermit {
        self.store.save_verified(&self.session, v).await.unwrap()
    }
    async fn read(&self, p: &StoredExecutionPermit) -> Result<StoredExecutionPermit, StorageError> {
        self.store
            .read(
                &self.session,
                &p.binding.snapshot.task_id,
                p.permit_id(),
                p.binding(),
            )
            .await
    }
    async fn consume(&self, p: &StoredExecutionPermit) -> Result<(), StorageError> {
        consume(&self.store, &self.session, p.permit_id(), p.binding()).await
    }
    async fn count(&self, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE kind=$1")
            .bind(kind)
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn copy_legacy_fixture_from(&self, source: &Self) {
        // Only unchanged public storage tables are copied into the actual old schema.
        self.sql.execute(format!(
            "INSERT INTO tasks SELECT * FROM {0}.tasks;
             INSERT INTO operations SELECT * FROM {0}.operations;
             INSERT INTO server_confirmations SELECT confirmation_ref,contract_version,owner_id,task_id,operation_id,node_id,tool_name,session_digest,session_binding_version,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,status,prepared_at,expires_at,confirmed_at,revoked_at FROM {0}.server_confirmations;
             INSERT INTO audit_events SELECT * FROM {0}.audit_events;
             SELECT setval(pg_get_serial_sequence('audit_events','sequence'), (SELECT max(sequence) FROM audit_events));",
            source.schema
        ).as_str()).await.unwrap();
    }
    async fn assert_latest_migration_versions(&self) {
        let versions: Vec<i64> = sqlx::query_scalar(
            "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version",
        )
        .fetch_all(&self.sql)
        .await
        .unwrap();
        assert_eq!(versions, (1..=35).collect::<Vec<_>>());
    }
    async fn status(&self, p: &StoredExecutionPermit) -> String {
        sqlx::query_scalar("SELECT status FROM execution_permits WHERE permit_id=$1")
            .bind(p.permit_id().as_str())
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn sentinel(&self) {
        self.sql
            .execute("CREATE TABLE foundation_atomicity_sentinel (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
    }
    async fn sentinel_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM foundation_atomicity_sentinel")
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn fault(&self, deferred: bool, kind: &str) {
        assert!(matches!(
            kind,
            "PERMIT_SAVED" | "PERMIT_CONSUMED" | "PERMIT_REVOKED"
        ));
        self.sql.execute("CREATE FUNCTION injected_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated failure'; END $$").await.unwrap();
        let when = if deferred {
            "AFTER INSERT"
        } else {
            "BEFORE INSERT"
        };
        let constraint = if deferred { "CONSTRAINT " } else { "" };
        let deferral = if deferred {
            "DEFERRABLE INITIALLY DEFERRED "
        } else {
            ""
        };
        self.sql.execute(format!("CREATE {constraint}TRIGGER reject_write {when} ON audit_events {deferral}FOR EACH ROW WHEN (NEW.kind='{kind}') EXECUTE FUNCTION injected_failure()").as_str()).await.unwrap();
    }
    async fn finish(self) {
        self.repo.close().await;
        self.sql.close().await;
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn node(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}
async fn consume(
    store: &PermitStore,
    session: &SessionRef,
    id: &PermitId,
    binding: &PermitBinding,
) -> Result<(), StorageError> {
    let id = id.clone();
    let binding = binding.clone();
    let task = binding.snapshot.task_id.clone();
    store
        .transaction(session, &task, move |unit| {
            Box::pin(async move { unit.consume(&id, &binding).await })
        })
        .await
}
async fn sentinel_work(db: &Db, p: &StoredExecutionPermit, fail: bool) -> Result<(), StorageError> {
    let id = p.permit_id().clone();
    let binding = p.binding().clone();
    db.store
        .transaction(
            &db.session,
            &binding.snapshot.task_id.clone(),
            move |unit| {
                Box::pin(async move {
                    unit.consume(&id, &binding).await?;
                    sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)")
                        .execute(unit.connection()?)
                        .await?;
                    if fail {
                        return Err(StorageError::Invalid("caller requested rollback"));
                    }
                    Ok(())
                })
            },
        )
        .await
}

#[test]
fn ttl_requires_explicit_positive_whole_milliseconds() {
    assert!(PermitConfig::new(Duration::ZERO).is_err());
    assert!(PermitConfig::new(Duration::from_nanos(1)).is_err());
    assert!(PermitConfig::new(Duration::MAX).is_err());
    assert!(PermitConfig::new(Duration::from_millis(1)).is_ok());
}
#[test]
fn facts_conversion_is_checked_and_preserves_u32_max() {
    assert_eq!(facts_u32(u32::MAX as u64), Ok(u32::MAX));
    assert_eq!(
        facts_u32(u32::MAX as u64 + 1),
        Err(StorageError::PermitVersionOverflow)
    );
    assert_eq!(facts_u32(0), Err(StorageError::PermitVersionOverflow));
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn missing_trusted_sources_never_enable_formal_issuance() {
    let db = Db::new().await;
    let v = db.fixture().await;
    assert_eq!(
        db.store
            .request_issuance(&db.session, &v.binding.task_id)
            .await
            .err(),
        Some(StorageError::PermitIssuanceUnavailable)
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn saves_full_fingerprint_replay_does_not_extend_expiry_or_duplicate_audit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    assert_ne!(
        p.binding.fingerprint.as_str(),
        v.binding.arguments_hash.as_str()
    );
    assert_eq!(
        p.binding.fingerprint,
        compute_operation_fingerprint(&v.payload).unwrap()
    );
    db.clock.set(db.clock.get() + 1_000);
    let replay = db.save(&v).await;
    assert_eq!(p.permit_id(), replay.permit_id());
    assert_eq!(
        p.execution_permit().expires_at_ms(),
        replay.execution_permit().expires_at_ms()
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    let contents: String =
        sqlx::query_scalar("SELECT row_to_json(p)::text FROM execution_permits p")
            .fetch_one(&db.sql)
            .await
            .unwrap();
    assert!(!contents.contains(db.session.as_str()));
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_saves_share_one_permit_and_one_audit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let (a, b) = tokio::join!(
        db.store.save_verified(&db.session, &v),
        db.store.save_verified(&db.session, &v)
    );
    assert_eq!(a.unwrap().permit_id(), b.unwrap().permit_id());
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn unique_constraint_prevents_a_second_permit_even_after_consumption() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.consume(&p).await.unwrap();
    let columns="contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,operation_fingerprint,fingerprint_payload,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,issued_at_ms,expires_at_ms,status,consumed_at_ms";
    let result=sqlx::query(&format!("INSERT INTO execution_permits (permit_id,{columns}) SELECT $1,{columns} FROM execution_permits WHERE permit_id=$2"))
        .bind(format!("permit_{}",uuid::Uuid::new_v4())).bind(p.permit_id().as_str()).execute(&db.sql).await;
    assert_eq!(
        result
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23505")
    );
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitNotActive)
    );
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_consumption_only_one_callback_can_commit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    let (a, b) = tokio::join!(sentinel_work(&db, &p, false), sentinel_work(&db, &p, false));
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(db.sentinel_count().await, 1);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    assert_eq!(db.consume(&p).await, Err(StorageError::PermitNotActive));
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn same_transaction_commits_consumption_sentinel_and_audit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    sentinel_work(&db, &p, false).await.unwrap();
    assert_eq!(db.status(&p).await, "CONSUMED");
    assert_eq!(db.sentinel_count().await, 1);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn callback_error_rolls_back_consumption_sentinel_and_audit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    assert!(sentinel_work(&db, &p, true).await.is_err());
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.sentinel_count().await, 0);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.consume(&p).await.unwrap();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn immediate_audit_failure_rolls_back_consumption() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    db.fault(false, "PERMIT_CONSUMED").await;
    assert_eq!(
        sentinel_work(&db, &p, false).await,
        Err(StorageError::Database)
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.sentinel_count().await, 0);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn deferred_commit_failure_rolls_back_consumption_and_sentinel() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    db.fault(true, "PERMIT_CONSUMED").await;
    assert_eq!(
        sentinel_work(&db, &p, false).await,
        Err(StorageError::Database)
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.sentinel_count().await, 0);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn ignored_consume_error_poisoned_transaction_cannot_commit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let id = p.permit_id().clone();
    let mut b = p.binding.clone();
    b.snapshot.plan_version += 1;
    let result = db
        .store
        .transaction(&db.session, &v.binding.task_id, move |unit| {
            Box::pin(async move {
                assert!(unit.connection().is_err());
                assert!(unit.consume(&id, &b).await.is_err());
                assert!(unit.connection().is_err());
                Ok(())
            })
        })
        .await;
    assert_eq!(result, Err(StorageError::TransactionAborted));
    assert_eq!(db.status(&p).await, "ACTIVE");
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cancelling_callback_future_rolls_back_all_changes() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    let store = db.store.clone();
    let session = db.session.clone();
    let id = p.permit_id().clone();
    let b = p.binding.clone();
    let task = v.binding.task_id.clone();
    let (sent, received) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        store
            .transaction(&session, &task, move |unit| {
                Box::pin(async move {
                    unit.consume(&id, &b).await?;
                    sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)")
                        .execute(unit.connection()?)
                        .await?;
                    sent.send(()).unwrap();
                    std::future::pending::<Result<(), StorageError>>().await
                })
            })
            .await
    });
    received.await.unwrap();
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    // A task lock waits for SQLx's dropped-transaction rollback to finish.
    db.read(&p).await.unwrap();
    assert_eq!(db.sentinel_count().await, 0);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn every_expected_binding_field_and_full_fingerprint_must_match() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    for field in 0..13 {
        let mut b = p.binding.clone();
        match field {
            0 => b.snapshot.user_id = UserId::parse("other").unwrap(),
            1 => b.snapshot.task_id = new_task_id(),
            2 => b.snapshot.operation_id = new_operation_id(),
            3 => b.snapshot.node_id = node("other"),
            4 => b.snapshot.tool_name = ToolName::parse("other_tool").unwrap(),
            5 => b.snapshot.task_revision += 1,
            6 => b.snapshot.plan_version += 1,
            7 => b.snapshot.operation_revision += 1,
            8 => b.snapshot.facts_version += 1,
            9 => b.snapshot.arguments_hash = ArgumentsHash::parse("0".repeat(64)).unwrap(),
            _ => {
                let mut payload = v.payload.clone();
                match field {
                    10 => payload.resource_id = ResourceId::parse("different_resource").unwrap(),
                    11 => payload.schema_version += 1,
                    _ => payload.normalized_arguments["value"] = json!("300.00"),
                }
                b.fingerprint = compute_operation_fingerprint(&payload).unwrap();
            }
        }
        assert_eq!(
            db.store
                .read(&db.session, &v.binding.task_id, p.permit_id(), &b)
                .await
                .err(),
            Some(StorageError::PermitBindingMismatch)
        );
        let id = p.permit_id().clone();
        let task = v.binding.task_id.clone();
        assert_eq!(
            db.store
                .transaction(&db.session, &task, move |unit| Box::pin(async move {
                    unit.consume(&id, &b).await
                }))
                .await,
            Err(StorageError::PermitBindingMismatch)
        );
    }
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn conflicting_verified_sources_resource_and_schema_cannot_replace_active_permit() {
    let db = Db::new().await;
    let mut v = db.fixture().await;
    let p = db.save(&v).await;
    let original = v.payload.clone();
    v.payload.resource_id = ResourceId::parse("different_resource").unwrap();
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitConflict)
    );
    v.payload = original.clone();
    v.payload.schema_version += 1;
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitConflict)
    );
    v.payload = original;
    let decision = v.decision_id.clone();
    v.decision_id = DecisionId::parse("different_decision").unwrap();
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitConflict)
    );
    v.decision_id = decision;
    v.authentication = VerifiedPermitAuthentication::Verified {
        grant_ref: AuthGrantRef::parse("different_auth_reference").unwrap(),
    };
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitConflict)
    );
    assert_eq!(db.read(&p).await.unwrap().permit_id(), p.permit_id());
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn invalid_projection_payload_cannot_be_stored() {
    let db = Db::new().await;
    let mut v = db.fixture().await;
    let original = v.payload.clone();
    for field in 0..9 {
        v.payload = original.clone();
        match field {
            0 => v.payload.user_id = UserId::parse("other").unwrap(),
            1 => v.payload.operation_id = new_operation_id(),
            2 => v.payload.tool_name = ToolName::parse("other_tool").unwrap(),
            3 => v.payload.plan_version += 1,
            4 => v.payload.schema_version = 0,
            5 => v.payload.normalized_arguments = json!({"nested":[200.5]}),
            6 => v.payload.normalized_arguments["value"] = json!("300.00"),
            7 => v.payload.resource_id = ResourceId::parse("bad\nresource").unwrap(),
            _ => v.payload.plan_version = 0,
        }
        assert!(db.store.save_verified(&db.session, &v).await.is_err());
    }
    assert_eq!(db.count("PERMIT_SAVED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cross_user_other_session_wrong_task_and_forged_id_cannot_access() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    for session in [&db.other, &db.same_user] {
        assert_eq!(
            db.store
                .read(session, &v.binding.task_id, p.permit_id(), p.binding())
                .await
                .err(),
            Some(StorageError::NotFound)
        );
        assert_eq!(
            consume(&db.store, session, p.permit_id(), p.binding()).await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.store
                .revoke(session, &v.binding.task_id, p.permit_id())
                .await,
            Err(StorageError::NotFound)
        );
        assert!(db.store.save_verified(session, &v).await.is_err());
    }
    let forged = PermitId::parse(format!("permit_{}", uuid::Uuid::new_v4())).unwrap();
    assert_eq!(
        db.store
            .read(&db.session, &v.binding.task_id, &forged, p.binding())
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(
        consume(&db.store, &db.session, &forged, p.binding()).await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        db.store
            .read(&db.session, &new_task_id(), p.permit_id(), p.binding())
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    assert_eq!(db.count("PERMIT_REVOKED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn renewed_rebound_and_revoked_session_cannot_read_consume_or_reissue() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    for (user, version) in [("user_001", 2), ("user_002", 3)] {
        db.sessions.bind(&db.session, user, version);
        assert_eq!(db.read(&p).await.err(), Some(StorageError::NotFound));
        assert_eq!(db.consume(&p).await, Err(StorageError::NotFound));
        assert!(db.store.save_verified(&db.session, &v).await.is_err());
    }
    db.sessions.revoke(&db.session);
    assert_eq!(db.read(&p).await.err(), Some(StorageError::Session));
    assert_eq!(db.consume(&p).await, Err(StorageError::Session));
    assert_eq!(db.status(&p).await, "ACTIVE");
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
async fn resolver_without_binding_version_fails_closed() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let store = PermitStore::for_trusted_backend(
        db.repo.clone(),
        Arc::new(LegacyResolver),
        PermitConfig::new(Duration::from_secs(20)).unwrap(),
    );
    assert_eq!(
        store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::Session)
    );
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn expiry_equality_and_clock_before_issue_reject_read_consume_and_reissue() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let issued = db.clock.get();
    let p = db.save(&v).await;
    db.clock.set(issued - 1);
    assert!(matches!(
        db.read(&p).await.err(),
        Some(StorageError::Invalid(_))
    ));
    assert!(matches!(
        db.consume(&p).await,
        Err(StorageError::Invalid(_))
    ));
    db.clock.set(p.execution_permit().expires_at_ms());
    assert_eq!(db.read(&p).await.err(), Some(StorageError::PermitExpired));
    assert_eq!(db.consume(&p).await, Err(StorageError::PermitExpired));
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitExpired)
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn revoke_is_idempotent_and_never_revives_or_rolls_back_consumed_work() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    for _ in 0..2 {
        db.store
            .revoke(&db.session, &v.binding.task_id, p.permit_id())
            .await
            .unwrap();
    }
    assert_eq!(db.status(&p).await, "REVOKED");
    assert_eq!(db.count("PERMIT_REVOKED").await, 1);
    assert_eq!(db.consume(&p).await, Err(StorageError::PermitNotActive));
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitNotActive)
    );
    let v2 = db.fixture().await;
    let p2 = db.save(&v2).await;
    db.consume(&p2).await.unwrap();
    assert_eq!(
        db.store
            .revoke(&db.session, &v2.binding.task_id, p2.permit_id())
            .await,
        Err(StorageError::PermitNotActive)
    );
    assert_eq!(db.status(&p2).await, "CONSUMED");
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn original_confirmation_must_remain_confirmed_unexpired_and_current() {
    let db = Db::new().await;
    for mode in 0..3 {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        match mode {
            0 => {
                db.confirmation
                    .revoke(&db.session, &v.binding.task_id, &v.confirmation_ref)
                    .await
                    .unwrap();
            }
            1 => {
                sqlx::query("UPDATE server_confirmations SET status='PREPARED',confirmed_at=NULL WHERE confirmation_ref=$1").bind(v.confirmation_ref.as_str()).execute(&db.sql).await.unwrap();
            }
            _ => {
                sqlx::query("UPDATE server_confirmations SET expires_at=prepared_at+interval '1 millisecond' WHERE confirmation_ref=$1").bind(v.confirmation_ref.as_str()).execute(&db.sql).await.unwrap();
                db.clock.set(db.clock.get() + 1);
            }
        }
        assert!(db.read(&p).await.is_err());
        assert!(db.consume(&p).await.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn prepared_confirmation_alone_cannot_save_verified_snapshot() {
    let db = Db::new().await;
    let v = db.fixture().await;
    sqlx::query("UPDATE server_confirmations SET status='PREPARED',confirmed_at=NULL WHERE confirmation_ref=$1").bind(v.confirmation_ref.as_str()).execute(&db.sql).await.unwrap();
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::ConfirmationRequired)
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn task_plan_fact_argument_and_operation_changes_invalidate_old_permits() {
    let db = Db::new().await;
    for mode in 0..6 {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        let query=match mode {
            0=>"UPDATE tasks SET revision=revision+1 WHERE task_id=$1",
            1=>"UPDATE tasks SET facts_version=facts_version+1 WHERE task_id=$1",
            2=>"UPDATE tasks SET plan=jsonb_set(plan,'{plan_version}','2'),plan_version=2 WHERE task_id=$1",
            3=>"UPDATE tasks SET plan=jsonb_set(plan,'{nodes,1,arguments,value}','\"300.00\"') WHERE task_id=$1",
            4=>"UPDATE operations SET revision=revision+1 WHERE task_id=$1",
            _=>"UPDATE operations SET arguments_hash=repeat('0',64) WHERE task_id=$1",
        };
        sqlx::query(query)
            .bind(v.binding.task_id.as_str())
            .execute(&db.sql)
            .await
            .unwrap();
        assert!(db.read(&p).await.is_err());
        assert!(db.consume(&p).await.is_err());
        assert!(db.store.save_verified(&db.session, &v).await.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn draft_submitted_processing_terminal_and_waiting_task_cannot_consume_or_reissue() {
    let db = Db::new().await;
    for state in [
        "DRAFT",
        "SUBMITTED",
        "PROCESSING",
        "SUCCEEDED",
        "FAILED",
        "CANCELLED",
    ] {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        sqlx::query("UPDATE operations SET state=$1,business_operation_ref=CASE WHEN $1 IN ('SUBMITTED','PROCESSING','SUCCEEDED','FAILED') THEN 'isolated_reference' ELSE NULL END WHERE operation_id=$2")
            .bind(state)
            .bind(v.binding.operation_id.as_str())
            .execute(&db.sql)
            .await
            .unwrap();
        assert!(db.read(&p).await.is_err());
        assert!(db.consume(&p).await.is_err());
        assert!(db.store.save_verified(&db.session, &v).await.is_err());
    }
    for state in ["WAITING_USER", "WAITING_EXTERNAL", "FAILED", "CANCELLED"] {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        sqlx::query("UPDATE tasks SET state=$1 WHERE task_id=$2")
            .bind(state)
            .bind(v.binding.task_id.as_str())
            .execute(&db.sql)
            .await
            .unwrap();
        assert!(db.read(&p).await.is_err());
        assert!(db.consume(&p).await.is_err());
        assert!(db.store.save_verified(&db.session, &v).await.is_err());
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn database_version_u32_max_is_accepted_but_larger_facts_rejected() {
    let db = Db::new().await;
    let v = db.fixture_facts(u32::MAX as u64).await;
    let p = db.save(&v).await;
    db.read(&p).await.unwrap();
    db.consume(&p).await.unwrap();
    let v = db.fixture_facts(u32::MAX as u64 + 1).await;
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitVersionOverflow)
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn save_and_revoke_audit_failures_roll_back_state() {
    for deferred in [false, true] {
        let db = Db::new().await;
        let v = db.fixture().await;
        db.fault(deferred, "PERMIT_SAVED").await;
        assert_eq!(
            db.store.save_verified(&db.session, &v).await.err(),
            Some(StorageError::Database)
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(db.count("PERMIT_SAVED").await, 0);
        db.finish().await;
        let db = Db::new().await;
        let v = db.fixture().await;
        let p = db.save(&v).await;
        db.fault(deferred, "PERMIT_REVOKED").await;
        assert_eq!(
            db.store
                .revoke(&db.session, &v.binding.task_id, p.permit_id())
                .await,
            Err(StorageError::Database)
        );
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.count("PERMIT_REVOKED").await, 0);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn schema_change_and_explicit_rollback_cannot_commit_partial_consumption() {
    for mode in 0..2 {
        let db = Db::new().await;
        let v = db.fixture().await;
        let p = db.save(&v).await;
        db.sentinel().await;
        let id = p.permit_id().clone();
        let b = p.binding.clone();
        let result = db
            .store
            .transaction(&db.session, &v.binding.task_id, move |unit| {
                Box::pin(async move {
                    unit.consume(&id, &b).await?;
                    sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)")
                        .execute(unit.connection()?)
                        .await?;
                    if mode == 0 {
                        sqlx::query("SET LOCAL search_path TO public")
                            .execute(unit.connection()?)
                            .await?;
                    } else {
                        unit.rollback().await;
                    }
                    Ok(())
                })
            })
            .await;
        assert_eq!(
            result,
            Err(if mode == 0 {
                StorageError::TransactionScope
            } else {
                StorageError::TransactionAborted
            })
        );
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.sentinel_count().await, 0);
        assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn another_repository_schema_cannot_consume_original_permit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let other = Db::new().await;
    assert_eq!(
        consume(&other.store, &db.session, p.permit_id(), p.binding()).await,
        Err(StorageError::NotFound)
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    other.finish().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn callback_cannot_change_confirmed_plan_arguments_or_task_versions_before_commit() {
    let db = Db::new().await;
    db.sentinel().await;
    for mode in 0..5 {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        let before = db
            .storage
            .recover(&db.session, &v.binding.task_id)
            .await
            .unwrap();
        let id = p.permit_id().clone();
        let b = p.binding.clone();
        let result=db.store.transaction(&db.session,&v.binding.task_id,move|unit|Box::pin(async move {
            unit.consume(&id,&b).await?;
            let sql=match mode {
                0=>"UPDATE tasks SET plan=jsonb_set(plan,'{nodes,1,arguments,value}','\"300.00\"'),revision=revision+1 WHERE task_id=$1",
                1=>"UPDATE tasks SET facts_version=facts_version+1,revision=revision+1 WHERE task_id=$1",
                2=>"UPDATE tasks SET plan=jsonb_set(plan,'{plan_version}','2'),plan_version=2,revision=revision+1 WHERE task_id=$1",
                3=>"UPDATE tasks SET revision=revision+1 WHERE task_id=$1",
                _=>"UPDATE tasks SET owner_id='other_user' WHERE task_id=$1",
            };
            sqlx::query(sql).bind(b.snapshot.task_id.as_str()).execute(unit.connection()?).await?;
            sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)").execute(unit.connection()?).await?;Ok(())
        })).await;
        assert!(result.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.sentinel_count().await, 0);
        assert_eq!(
            db.storage
                .recover(&db.session, &v.binding.task_id)
                .await
                .unwrap(),
            before
        );
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn callback_cannot_change_operation_parameters_facts_plan_node_or_revision_before_commit() {
    let db = Db::new().await;
    db.sentinel().await;
    for mode in 0..5 {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        let before = db
            .storage
            .recover(&db.session, &v.binding.task_id)
            .await
            .unwrap();
        let id = p.permit_id().clone();
        let b = p.binding.clone();
        let result=db.store.transaction(&db.session,&v.binding.task_id,move|unit|Box::pin(async move {
            unit.consume(&id,&b).await?;
            let sql=match mode {
                0=>"UPDATE operations SET arguments_hash=repeat('0',64),revision=revision+1 WHERE operation_id=$1",
                1=>"UPDATE operations SET facts_version=facts_version+1,revision=revision+1 WHERE operation_id=$1",
                2=>"UPDATE operations SET plan_version=plan_version+1,revision=revision+1 WHERE operation_id=$1",
                3=>"UPDATE operations SET node_id='different_node',revision=revision+1 WHERE operation_id=$1",
                _=>"UPDATE operations SET revision=revision+1 WHERE operation_id=$1",
            };
            sqlx::query(sql).bind(b.snapshot.operation_id.as_str()).execute(unit.connection()?).await?;
            sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)").execute(unit.connection()?).await?;Ok(())
        })).await;
        assert!(result.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.sentinel_count().await, 0);
        assert_eq!(
            db.storage
                .recover(&db.session, &v.binding.task_id)
                .await
                .unwrap(),
            before
        );
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn callback_cannot_replace_stored_permit_authority_before_commit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    let id = p.permit_id().clone();
    let b = p.binding.clone();
    let result=db.store.transaction(&db.session,&v.binding.task_id,move|unit|Box::pin(async move {
        unit.consume(&id,&b).await?;
        sqlx::query("UPDATE execution_permits SET decision_id='different_decision' WHERE permit_id=$1").bind(id.as_str()).execute(unit.connection()?).await?;
        sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)").execute(unit.connection()?).await?;Ok(())
    })).await;
    assert_eq!(result, Err(StorageError::PermitBindingMismatch));
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.sentinel_count().await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn existing_submission_recorded_shape_can_commit_without_changing_authorized_content() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let id = p.permit_id().clone();
    let b = p.binding.clone();
    db.store.transaction(&db.session,&v.binding.task_id,move|unit|Box::pin(async move {
        unit.consume(&id,&b).await?;
        sqlx::query("UPDATE operations SET state='SUBMITTED',revision=revision+1,business_operation_ref='isolated_original_reference' WHERE operation_id=$1").bind(b.snapshot.operation_id.as_str()).execute(unit.connection()?).await?;
        let checkpoint=RecoveryPoint::ReconcileOperation{operation_id:b.snapshot.operation_id.clone()};
        sqlx::query("UPDATE tasks SET state='WAITING_EXTERNAL',revision=revision+1,checkpoint=$1 WHERE task_id=$2").bind(Json(checkpoint)).bind(b.snapshot.task_id.as_str()).execute(unit.connection()?).await?;Ok(())
    })).await.unwrap();
    assert_eq!(db.status(&p).await, "CONSUMED");
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::ReconcileRequired)
    );
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
    panic!("database lock wait was not observed");
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn lock_wait_rechecks_session_expiry_and_current_task_content() {
    let db = Db::new().await;
    for mode in 0..3 {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        let mut lock = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(v.binding.task_id.as_str())
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let store = db.store.clone();
        let session = db.session.clone();
        let id = p.permit_id().clone();
        let b = p.binding.clone();
        let job = tokio::spawn(async move { consume(&store, &session, &id, &b).await });
        wait_for_blocked(&db, pid).await;
        if mode == 0 {
            db.sessions.bind(&db.session, "user_001", 2);
        } else if mode == 1 {
            db.clock.set(p.execution_permit().expires_at_ms());
        } else {
            sqlx::query("UPDATE tasks SET revision=revision+1 WHERE task_id=$1")
                .bind(v.binding.task_id.as_str())
                .execute(&mut *lock)
                .await
                .unwrap();
        }
        lock.commit().await.unwrap();
        assert!(job.await.unwrap().is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
        // Reset only the isolated resolver, never a production revocation source.
        db.sessions.bind(&db.session, "user_001", 1);
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn final_guard_rechecks_session_expiry_and_in_transaction_clock_rollback() {
    for mode in 0..3 {
        let db = Db::new().await;
        let v = db.fixture().await;
        let p = db.save(&v).await;
        db.sentinel().await;
        let id = p.permit_id().clone();
        let b = p.binding.clone();
        let clock = db.clock.clone();
        let sessions = db.sessions.clone();
        let session = db.session.clone();
        let expires = p.execution_permit().expires_at_ms();
        clock.set(clock.get() + 1_000);
        let result = db
            .store
            .transaction(&db.session, &v.binding.task_id, move |unit| {
                Box::pin(async move {
                    unit.consume(&id, &b).await?;
                    sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)")
                        .execute(unit.connection()?)
                        .await?;
                    match mode {
                        0 => sessions.bind(&session, "user_001", 2),
                        1 => clock.set(expires),
                        _ => clock.set(clock.get() - 1),
                    };
                    Ok(())
                })
            })
            .await;
        assert!(result.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.sentinel_count().await, 0);
        assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn session_revocation_during_consumption_audit_wait_rolls_back_update() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let mut lock = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(987654322)")
        .execute(&mut *lock)
        .await
        .unwrap();
    db.sql.execute("CREATE FUNCTION wait_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(987654322); RETURN NEW; END $$").await.unwrap();
    db.sql.execute("CREATE TRIGGER wait_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='PERMIT_CONSUMED') EXECUTE FUNCTION wait_audit()").await.unwrap();
    let store = db.store.clone();
    let session = db.session.clone();
    let id = p.permit_id().clone();
    let b = p.binding.clone();
    let job = tokio::spawn(async move { consume(&store, &session, &id, &b).await });
    wait_for_blocked(&db, pid).await;
    db.sessions.revoke(&db.session);
    lock.commit().await.unwrap();
    assert_eq!(job.await.unwrap(), Err(StorageError::Session));
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn new_connections_recover_same_active_then_consumed_record() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    repo.migrate().await.unwrap();
    let store = Db::store(repo.clone(), db.sessions.clone(), db.clock.clone());
    let read = store
        .read(&db.session, &v.binding.task_id, p.permit_id(), p.binding())
        .await
        .unwrap();
    assert_eq!(read.permit_id(), p.permit_id());
    assert_eq!(
        read.execution_permit().expires_at_ms(),
        p.execution_permit().expires_at_ms()
    );
    consume(&store, &db.session, p.permit_id(), p.binding())
        .await
        .unwrap();
    repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    let store = Db::store(repo.clone(), db.sessions.clone(), db.clock.clone());
    assert_eq!(
        store
            .read(&db.session, &v.binding.task_id, p.permit_id(), p.binding())
            .await
            .err(),
        Some(StorageError::PermitNotActive)
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    repo.close().await;
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn migration_four_preserves_previous_checksums_confirmations_and_tasks() {
    let db = Db::unmigrated().await;
    let sql = [
        include_str!("../migrations/0001_recovery_points.sql"),
        include_str!("../migrations/0002_plan_identity_version.sql"),
        include_str!("../migrations/0003_server_confirmations.sql"),
    ];
    let migrations = sql
        .iter()
        .enumerate()
        .map(|(i, sql)| {
            sqlx::migrate::Migration::new(
                (i + 1) as i64,
                format!("baseline {}", i + 1).into(),
                sqlx::migrate::MigrationType::Simple,
                sql.replace("\r\n", "\n").into(),
                false,
            )
        })
        .collect();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&db.repo.pool)
    .await
    .unwrap();
    let source = Db::new().await;
    let v = source.fixture().await;
    db.copy_legacy_fixture_from(&source).await;
    source.finish().await;
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    db.assert_latest_migration_versions().await;
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=3 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(db.count("EXPLICIT_CONFIRMATION_RECORDED").await, 1);
    let p = db.save(&v).await;
    db.consume(&p).await.unwrap();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn save_rechecks_database_snapshot_after_audit_before_commit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let before = db
        .storage
        .recover(&db.session, &v.binding.task_id)
        .await
        .unwrap();
    db.sql.execute("CREATE FUNCTION mutate_task() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE tasks SET revision=revision+1 WHERE task_id=NEW.task_id; RETURN NEW; END $$").await.unwrap();
    db.sql.execute("CREATE TRIGGER mutate_task BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='PERMIT_SAVED') EXECUTE FUNCTION mutate_task()").await.unwrap();
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitBindingMismatch)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        db.storage
            .recover(&db.session, &v.binding.task_id)
            .await
            .unwrap(),
        before
    );
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn permit_expiry_is_capped_by_original_confirmation_and_expired_status_cannot_revive() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let store = PermitStore::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.sessions.clone(),
        PermitConfig::new(Duration::from_secs(120)).unwrap(),
        db.clock.clone(),
    );
    let p = store.save_verified(&db.session, &v).await.unwrap();
    let c = db
        .confirmation
        .verify(
            &db.session,
            &v.binding.task_id,
            &v.confirmation_ref,
            &v.binding,
        )
        .await
        .unwrap();
    assert_eq!(
        p.execution_permit().expires_at_ms(),
        c.expires_at.timestamp_millis()
    );
    sqlx::query("UPDATE execution_permits SET status='EXPIRED' WHERE permit_id=$1")
        .bind(p.permit_id().as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert_eq!(db.read(&p).await.err(), Some(StorageError::PermitNotActive));
    assert_eq!(db.consume(&p).await, Err(StorageError::PermitNotActive));
    assert_eq!(
        store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitNotActive)
    );
    db.finish().await;
}

fn without_strong_authentication(v: &mut VerifiedPermitSnapshot) {
    v.required_gates = vec![SecurityGate::ExplicitConfirmation];
    v.authentication = VerifiedPermitAuthentication::NotRequired;
}

#[test]
fn authentication_representation_rejects_missing_proof_and_conflicting_gates() {
    let explicit = SecurityGate::ExplicitConfirmation;
    let strong = SecurityGate::StrongAuthentication;
    let verified = VerifiedPermitAuthentication::Verified {
        grant_ref: AuthGrantRef::parse("isolated_contract_auth_reference").unwrap(),
    };
    assert!(StoredPermitAuthentication::verified(
        &[explicit],
        &VerifiedPermitAuthentication::NotRequired
    )
    .is_ok());
    assert!(StoredPermitAuthentication::verified(&[explicit, strong], &verified).is_ok());
    for gates in [
        vec![],
        vec![strong],
        vec![explicit, explicit],
        vec![strong, explicit],
    ] {
        assert!(StoredPermitAuthentication::verified(&gates, &verified).is_err());
        assert!(StoredPermitAuthentication::verified(
            &gates,
            &VerifiedPermitAuthentication::NotRequired
        )
        .is_err());
    }
    assert!(StoredPermitAuthentication::verified(&[explicit], &verified).is_err());
    assert!(StoredPermitAuthentication::verified(
        &[explicit, strong],
        &VerifiedPermitAuthentication::NotRequired
    )
    .is_err());
    let simulation = VerifiedPermitAuthentication::Verified {
        grant_ref: AuthGrantRef::parse(crate::SimulationGrantId::new_server_id().as_str()).unwrap(),
    };
    assert!(StoredPermitAuthentication::verified(&[explicit, strong], &simulation).is_err());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn no_strong_authentication_round_trips_without_a_synthetic_grant() {
    let db = Db::new().await;
    let mut v = db.fixture().await;
    without_strong_authentication(&mut v);
    let p = db.save(&v).await;
    let row: (i16, String, Option<String>, Json<serde_json::Value>) = sqlx::query_as(
        "SELECT storage_version,authentication_state,auth_grant_ref,required_gates FROM execution_permits WHERE permit_id=$1",
    ).bind(p.permit_id().as_str()).fetch_one(&db.sql).await.unwrap();
    assert_eq!((row.0, row.1.as_str(), row.2), (2, "NOT_REQUIRED", None));
    assert_eq!(row.3 .0, json!(["EXPLICIT_CONFIRMATION"]));
    db.clock.set(db.clock.get() + 1_000);
    let replay = db.save(&v).await;
    assert_eq!(replay.permit_id(), p.permit_id());
    assert_eq!(
        replay.execution_permit().expires_at_ms(),
        p.execution_permit().expires_at_ms()
    );
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    let repo = PgRepository::connect_existing(&db.url, &db.schema)
        .await
        .unwrap();
    let store = Db::store(repo.clone(), db.sessions.clone(), db.clock.clone());
    let restored = store
        .read(&db.session, &v.binding.task_id, p.permit_id(), p.binding())
        .await
        .unwrap();
    consume(
        &store,
        &db.session,
        restored.permit_id(),
        restored.binding(),
    )
    .await
    .unwrap();
    assert_eq!(db.status(&p).await, "CONSUMED");
    assert_eq!(db.consume(&p).await, Err(StorageError::PermitNotActive));
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn verified_strong_authentication_persists_the_required_gate_and_reference() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let row: (i16, String, String, Json<serde_json::Value>) = sqlx::query_as(
        "SELECT storage_version,authentication_state,auth_grant_ref,required_gates FROM execution_permits WHERE permit_id=$1",
    ).bind(p.permit_id().as_str()).fetch_one(&db.sql).await.unwrap();
    assert_eq!(
        (row.0, row.1.as_str(), row.2.as_str()),
        (2, "VERIFIED_REQUIRED", "isolated_contract_auth_reference")
    );
    assert_eq!(
        row.3 .0,
        json!(["EXPLICIT_CONFIRMATION", "STRONG_AUTHENTICATION"])
    );
    db.read(&p).await.unwrap();
    db.consume(&p).await.unwrap();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn contradictory_authentication_or_isolated_simulation_reference_cannot_save() {
    let db = Db::new().await;
    for mode in 0..4 {
        let mut v = db.fixture().await;
        match mode {
            0 => v.authentication = VerifiedPermitAuthentication::NotRequired,
            1 => v.required_gates = vec![SecurityGate::ExplicitConfirmation],
            2 => v.required_gates.clear(),
            _ => {
                v.authentication = VerifiedPermitAuthentication::Verified {
                    grant_ref: AuthGrantRef::parse(
                        crate::SimulationGrantId::new_server_id().as_str(),
                    )
                    .unwrap(),
                }
            }
        }
        assert_eq!(
            db.store.save_verified(&db.session, &v).await.err(),
            Some(StorageError::PermitBindingMismatch)
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(db.count("PERMIT_SAVED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn database_authentication_check_rejects_null_unknown_and_contradictory_shapes() {
    let db = Db::new().await;
    for no_auth in [false, true] {
        let mut v = db.fixture().await;
        if no_auth {
            without_strong_authentication(&mut v);
        }
        let p = db.save(&v).await;
        let updates = [
            "required_gates=NULL",
            "required_gates='null'::jsonb",
            "required_gates='{}'::jsonb",
            "required_gates='[]'::jsonb",
            "required_gates='[\"UNKNOWN\"]'::jsonb",
            "authentication_state='UNKNOWN'",
            "authentication_state=NULL",
            "storage_version=3",
            "storage_version=NULL",
            "storage_version=1,authentication_state='LEGACY_REQUIRED',required_gates=NULL,auth_grant_ref=NULL",
            if no_auth { "auth_grant_ref='unexpected_grant'" } else { "auth_grant_ref=NULL" },
            if no_auth { "authentication_state='VERIFIED_REQUIRED',auth_grant_ref='unexpected_grant'" } else { "authentication_state='NOT_REQUIRED',auth_grant_ref=NULL" },
            "authentication_state='VERIFIED_REQUIRED',required_gates='[\"EXPLICIT_CONFIRMATION\",\"STRONG_AUTHENTICATION\"]',auth_grant_ref='sim_grant_00000000-0000-0000-0000-000000000001'",
        ];
        for update in updates {
            let result = sqlx::query(&format!(
                "UPDATE execution_permits SET {update} WHERE permit_id=$1"
            ))
            .bind(p.permit_id().as_str())
            .execute(&db.sql)
            .await;
            let error = result.unwrap_err();
            let code = error.as_database_error().unwrap().code().unwrap();
            assert!(
                matches!(code.as_ref(), "23514" | "23502"),
                "{update}: {code}"
            );
        }
        db.read(&p).await.unwrap();
        db.consume(&p).await.unwrap();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn decoding_corrupt_authentication_never_defaults_to_not_required() {
    let db = Db::new().await;
    db.sql
        .execute(
            "ALTER TABLE execution_permits DROP CONSTRAINT execution_permits_authentication_check",
        )
        .await
        .unwrap();
    for update in [
        "auth_grant_ref=NULL",
        "required_gates=NULL",
        "required_gates='null'::jsonb",
        "required_gates='{}'::jsonb",
        "required_gates='[\"UNKNOWN\"]'::jsonb",
        "authentication_state='NOT_REQUIRED',auth_grant_ref=NULL",
        "authentication_state='UNKNOWN'",
        "storage_version=9",
        "auth_grant_ref='sim_grant_00000000-0000-0000-0000-000000000001'",
    ] {
        let v = db.fixture().await;
        let p = db.save(&v).await;
        sqlx::query(&format!(
            "UPDATE execution_permits SET {update} WHERE permit_id=$1"
        ))
        .bind(p.permit_id().as_str())
        .execute(&db.sql)
        .await
        .unwrap();
        assert_eq!(db.read(&p).await.err(), Some(StorageError::CorruptRecord));
        assert_eq!(db.consume(&p).await, Err(StorageError::CorruptRecord));
        assert_eq!(db.status(&p).await, "ACTIVE");
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn changing_authentication_requirement_cannot_replay_or_replace_a_permit() {
    let db = Db::new().await;
    for no_auth in [false, true] {
        let mut v = db.fixture().await;
        if no_auth {
            without_strong_authentication(&mut v);
        }
        let p = db.save(&v).await;
        if no_auth {
            v.required_gates.push(SecurityGate::StrongAuthentication);
            v.authentication = VerifiedPermitAuthentication::Verified {
                grant_ref: AuthGrantRef::parse("isolated_contract_auth_reference").unwrap(),
            };
        } else {
            without_strong_authentication(&mut v);
        }
        assert_eq!(
            db.store.save_verified(&db.session, &v).await.err(),
            Some(StorageError::PermitConflict)
        );
        db.read(&p).await.unwrap();
        db.consume(&p).await.unwrap();
        assert_eq!(
            db.store.save_verified(&db.session, &v).await.err(),
            Some(StorageError::PermitConflict)
        );
    }
    assert_eq!(db.count("PERMIT_SAVED").await, 2);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn callback_cannot_change_authentication_mode_or_proof_before_commit() {
    let db = Db::new().await;
    db.sentinel().await;
    for (no_auth, update) in [
        (false, "authentication_state='NOT_REQUIRED',required_gates='[\"EXPLICIT_CONFIRMATION\"]',auth_grant_ref=NULL"),
        (false, "auth_grant_ref='different_auth_reference'"),
        (false, "storage_version=1,authentication_state='LEGACY_REQUIRED',required_gates=NULL"),
        (true, "authentication_state='VERIFIED_REQUIRED',required_gates='[\"EXPLICIT_CONFIRMATION\",\"STRONG_AUTHENTICATION\"]',auth_grant_ref='unexpected_auth_reference'"),
    ] {
        let mut v = db.fixture().await;
        if no_auth {
            without_strong_authentication(&mut v);
        }
        let p = db.save(&v).await;
        let id = p.permit_id().clone();
        let b = p.binding().clone();
        let result = db.store.transaction(&db.session, &v.binding.task_id, move |unit| Box::pin(async move {
            unit.consume(&id, &b).await?;
            sqlx::query(&format!("UPDATE execution_permits SET {update} WHERE permit_id=$1"))
                .bind(id.as_str()).execute(unit.connection()?).await?;
            sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)").execute(unit.connection()?).await?;
            Ok(())
        })).await;
        assert_eq!(result, Err(StorageError::PermitBindingMismatch));
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.sentinel_count().await, 0);
        db.read(&p).await.unwrap();
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn save_final_guard_rejects_audit_trigger_changing_authentication() {
    let db = Db::new().await;
    let v = db.fixture().await;
    db.sql.execute("CREATE FUNCTION change_authentication() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE execution_permits SET authentication_state='NOT_REQUIRED',required_gates='[\"EXPLICIT_CONFIRMATION\"]',auth_grant_ref=NULL; RETURN NEW; END $$").await.unwrap();
    db.sql.execute("CREATE TRIGGER change_authentication BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='PERMIT_SAVED') EXECUTE FUNCTION change_authentication()").await.unwrap();
    assert_eq!(
        db.store.save_verified(&db.session, &v).await.err(),
        Some(StorageError::PermitBindingMismatch)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(db.count("PERMIT_SAVED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn no_strong_authentication_concurrent_save_and_consume_are_still_once_only() {
    let db = Db::new().await;
    let mut v = db.fixture().await;
    without_strong_authentication(&mut v);
    let (first, second) = tokio::join!(
        db.store.save_verified(&db.session, &v),
        db.store.save_verified(&db.session, &v)
    );
    let p = first.unwrap();
    assert_eq!(p.permit_id(), second.unwrap().permit_id());
    db.sentinel().await;
    let (first, second) =
        tokio::join!(sentinel_work(&db, &p, false), sentinel_work(&db, &p, false));
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert_eq!(db.sentinel_count().await, 1);
    assert_eq!(db.count("PERMIT_SAVED").await, 1);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn no_strong_authentication_still_rejects_revoked_session_and_exact_expiry() {
    let db = Db::new().await;
    let mut v = db.fixture().await;
    without_strong_authentication(&mut v);
    let p = db.save(&v).await;
    db.sessions.bind(&db.session, "user_001", 2);
    assert_eq!(db.read(&p).await.err(), Some(StorageError::NotFound));
    assert_eq!(db.consume(&p).await, Err(StorageError::NotFound));
    db.sessions.bind(&db.session, "user_001", 1);
    db.clock.set(p.execution_permit().expires_at_ms());
    assert_eq!(db.read(&p).await.err(), Some(StorageError::PermitExpired));
    assert_eq!(db.consume(&p).await, Err(StorageError::PermitExpired));
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn no_strong_authentication_does_not_bypass_explicit_confirmation() {
    let db = Db::new().await;
    for update in [
        "status='PREPARED',confirmed_at=NULL",
        "status='REVOKED',revoked_at=confirmed_at",
    ] {
        let mut v = db.fixture().await;
        without_strong_authentication(&mut v);
        let p = db.save(&v).await;
        sqlx::query(&format!(
            "UPDATE server_confirmations SET {update} WHERE confirmation_ref=$1"
        ))
        .bind(v.confirmation_ref.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
        assert!(db.read(&p).await.is_err());
        assert!(db.consume(&p).await.is_err());
        assert!(db.store.save_verified(&db.session, &v).await.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn no_strong_authentication_callback_and_deferred_failure_roll_back_all_effects() {
    for deferred in [false, true] {
        let db = Db::new().await;
        let mut v = db.fixture().await;
        without_strong_authentication(&mut v);
        let p = db.save(&v).await;
        db.sentinel().await;
        if deferred {
            db.fault(true, "PERMIT_CONSUMED").await;
        }
        assert!(sentinel_work(&db, &p, !deferred).await.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.sentinel_count().await, 0);
        assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn authentication_storage_never_enables_formal_issuance() {
    let db = Db::new().await;
    for no_auth in [false, true] {
        let mut v = db.fixture().await;
        if no_auth {
            without_strong_authentication(&mut v);
        }
        db.save(&v).await;
        assert_eq!(
            db.store
                .request_issuance(&db.session, &v.binding.task_id)
                .await
                .err(),
            Some(StorageError::PermitIssuanceUnavailable)
        );
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn migration_seven_preserves_legacy_rows_checksums_and_required_reference_semantics() {
    let db = Db::unmigrated().await;
    let scripts = [
        include_str!("../migrations/0001_recovery_points.sql"),
        include_str!("../migrations/0002_plan_identity_version.sql"),
        include_str!("../migrations/0003_server_confirmations.sql"),
        include_str!("../migrations/0004_execution_permits.sql"),
        include_str!("../migrations/0005_simulation_auth.sql"),
        include_str!("../migrations/0006_security_decisions.sql"),
    ];
    let migrations = scripts
        .iter()
        .enumerate()
        .map(|(i, script)| {
            sqlx::migrate::Migration::new(
                (i + 1) as i64,
                format!("baseline {}", i + 1).into(),
                sqlx::migrate::MigrationType::Simple,
                script.replace("\r\n", "\n").into(),
                false,
            )
        })
        .collect();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&db.repo.pool)
    .await
    .unwrap();
    let checksums: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    let mut fixtures = vec![];
    let source = Db::new().await;
    let mut legacy_snapshots = vec![];
    for status in ["ACTIVE", "CONSUMED", "REVOKED", "EXPIRED"] {
        legacy_snapshots.push((source.fixture().await, status));
    }
    db.copy_legacy_fixture_from(&source).await;
    source.finish().await;
    for (v, status) in legacy_snapshots {
        let b = &v.binding;
        let id = PermitId::parse(format!("permit_{}", uuid::Uuid::new_v4())).unwrap();
        let fingerprint = compute_operation_fingerprint(&v.payload).unwrap();
        let consumed_at = if status == "CONSUMED" {
            Some(db.clock.get())
        } else {
            None
        };
        // The pre-v2 writer has no gate columns; preserve this exact historical shape.
        sqlx::query("INSERT INTO execution_permits (permit_id,contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,operation_fingerprint,fingerprint_payload,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,issued_at_ms,expires_at_ms,status,consumed_at_ms) VALUES ($1,1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23)")
            .bind(id.as_str()).bind(v.decision_id.as_str()).bind(b.user_id.as_str()).bind(b.task_id.as_str())
            .bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str()).bind(b.task_revision as i64)
            .bind(b.plan_version as i64).bind(b.operation_revision as i64).bind(b.facts_version as i64).bind(b.arguments_hash.as_str())
            .bind(hash(b).unwrap().as_str()).bind(fingerprint.as_str()).bind(Json(&v.payload)).bind(&v.session_digest)
            .bind(v.session_version as i64).bind(v.confirmation_ref.as_str()).bind("legacy_auth_reference")
            .bind(db.clock.get()).bind(db.clock.get()+15_000).bind(status).bind(consumed_at).execute(&db.sql).await.unwrap();
        fixtures.push((v, id, fingerprint, status));
    }
    let original: Vec<Json<serde_json::Value>> =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM execution_permits p ORDER BY permit_id")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    db.assert_latest_migration_versions().await;
    // Migration 24 adds a nullable lifecycle anchor; legacy rows must keep it NULL.
    let anchored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM execution_permits WHERE lifecycle_anchor IS NOT NULL",
    )
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert_eq!(anchored, 0);
    let retained: Vec<Json<serde_json::Value>> = sqlx::query_scalar("SELECT to_jsonb(p)-'storage_version'-'authentication_state'-'required_gates'-'execution_scope'-'lifecycle_anchor'-'card_attempt_no' FROM execution_permits p ORDER BY permit_id")
        .fetch_all(&db.sql).await.unwrap();
    let retained_checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=6 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(retained, original);
    assert_eq!(retained_checksums, checksums);
    let nonzero:i64=sqlx::query_scalar("SELECT count(*) FROM execution_permits WHERE card_attempt_no<>0").fetch_one(&db.sql).await.unwrap();assert_eq!(nonzero,0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits WHERE storage_version=1 AND authentication_state='LEGACY_REQUIRED' AND required_gates IS NULL AND auth_grant_ref='legacy_auth_reference'")
        .fetch_one(&db.sql).await.unwrap();
    assert_eq!(rows, 4);
    for (v, id, fingerprint, status) in fixtures {
        let binding = PermitBinding {
            snapshot: v.binding.clone(),
            fingerprint,
        };
        let read = db
            .store
            .read(&db.session, &v.binding.task_id, &id, &binding)
            .await;
        if status == "ACTIVE" {
            let p = read.unwrap();
            assert_eq!(
                db.store.save_verified(&db.session, &v).await.err(),
                Some(StorageError::PermitConflict)
            );
            let error =
                sqlx::query("UPDATE execution_permits SET auth_grant_ref=NULL WHERE permit_id=$1")
                    .bind(id.as_str())
                    .execute(&db.sql)
                    .await
                    .unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("23514")
            );
            db.consume(&p).await.unwrap();
        } else {
            assert_eq!(read.err(), Some(StorageError::PermitNotActive));
            assert_eq!(
                consume(&db.store, &db.session, &id, &binding).await,
                Err(StorageError::PermitNotActive)
            );
        }
    }
    db.finish().await;
}


// Representative real 0032 -> current 0034 upgrade. Public storage fixtures only:
// legacy v1 plus current v2, all four persisted permit states, old confirmation
// and audit history. This is not a business writer or a waiting/fault program.
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn migration_thirty_three_preserves_0032_confirmations_permits_and_history() {upgrade_card_baseline(32).await;}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn migration_thirty_five_preserves_0034_and_noncard_tombstones(){upgrade_card_baseline(34).await;}
async fn upgrade_card_baseline(baseline:i64) {
    async fn rows(db: &Db) -> Vec<(&'static str, Vec<Json<serde_json::Value>>)> {
        let mut result = vec![];
        for (table, query) in [
            ("tasks", "SELECT to_jsonb(r) FROM tasks r ORDER BY task_id"),
            ("operations", "SELECT to_jsonb(r) FROM operations r ORDER BY operation_id"),
            ("server_confirmations", "SELECT to_jsonb(r) FROM server_confirmations r ORDER BY confirmation_ref"),
            ("execution_permits", "SELECT to_jsonb(r)-'card_attempt_no' FROM execution_permits r ORDER BY permit_id"),
            ("audit_events", "SELECT to_jsonb(r) FROM audit_events r ORDER BY sequence"),
        ] {
            result.push((table, sqlx::query_scalar(query).fetch_all(&db.sql).await.unwrap()));
        }
        result
    }
    let db = Db::unmigrated().await;
    let scripts = [
        include_str!("../migrations/0001_recovery_points.sql"),
        include_str!("../migrations/0002_plan_identity_version.sql"),
        include_str!("../migrations/0003_server_confirmations.sql"),
        include_str!("../migrations/0004_execution_permits.sql"),
        include_str!("../migrations/0005_simulation_auth.sql"),
        include_str!("../migrations/0006_security_decisions.sql"),
        include_str!("../migrations/0007_permit_authentication_gates.sql"),
        include_str!("../migrations/0008_decision_microsecond_fence.sql"),
        include_str!("../migrations/0009_read_only_runtime_claims.sql"),
        include_str!("../migrations/0010_read_only_runtime_continuations.sql"),
        include_str!("../migrations/0011_planning_confirmation_preparations.sql"),
        include_str!("../migrations/0012_persistent_mock_draft_execution.sql"),
        include_str!("../migrations/0013_mock_draft_lifecycle_v2.sql"),
        include_str!("../migrations/0014_public_execution_requests.sql"),
        include_str!("../migrations/0015_transfer_business_orders_v1.sql"),
        include_str!("../migrations/0016_transfer_funds_v1.sql"),
        include_str!("../migrations/0017_public_audit_outbox.sql"),
        include_str!("../migrations/0018_permit_execution_scope.sql"),
        include_str!("../migrations/0019_payment_preparation.sql"),
        include_str!("../migrations/0020_payment_minimum_decisions.sql"),
        include_str!("../migrations/0021_payment_execution_links.sql"),
        include_str!("../migrations/0022_payment_business_preflight.sql"),
        include_str!("../migrations/0023_payment_agent_bindings.sql"),
        include_str!("../migrations/0024_card_catalog_and_freeze.sql"),
        include_str!("../migrations/0025_billing_feedback.sql"),
        include_str!("../migrations/0026_subscription_journal.sql"),
        include_str!("../migrations/0027_billing_prepare_recovery.sql"),
        include_str!("../migrations/0028_billing_domain_audit.sql"),
        include_str!("../migrations/0029_subscription_a2_audit.sql"),
        include_str!("../migrations/0030_simulation_sources.sql"),
        include_str!("../migrations/0031_payment_completion_candidate.sql"),
        include_str!("../migrations/0032_unified_fictional_source.sql"),
        include_str!("../migrations/0033_card_public_safety.sql"),
        include_str!("../migrations/0034_payment_background_acceptance.sql"),
    ];
    let migrations = scripts.iter().take(baseline as usize).enumerate().map(|(i, script)| {
        // Match the original migrator exactly, including migration26's schema
        // substitution. Its checksum must replay under the unmodified repository.
        let sql = if i == 25 { script.replace("__SCHEMA__", &db.schema) }
            else { script.replace("\r\n", "\n") };
        sqlx::migrate::Migration::new((i+1) as i64, format!("baseline {}", i+1).into(),
            sqlx::migrate::MigrationType::Simple, sql.into(), false)
    }).collect();
    sqlx::migrate::Migrator { migrations: std::borrow::Cow::Owned(migrations),
        ..sqlx::migrate::Migrator::DEFAULT }.run(&db.repo.pool).await.unwrap();
    let before_checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE success ORDER BY version"
    ).fetch_all(&db.sql).await.unwrap();
    assert_eq!(before_checksums.iter().map(|(v,_)|*v).collect::<Vec<_>>(), (1..=baseline).collect::<Vec<_>>());
    let absent: Option<String> = sqlx::query_scalar("SELECT to_regclass('public_card_safety_requests_v1')::text")
        .fetch_one(&db.sql).await.unwrap();
    assert_eq!(absent.is_some(),baseline>=33);
    let mut fixtures = vec![];
    for mode in ["LEGACY_REQUIRED", "VERIFIED_REQUIRED", "NOT_REQUIRED"] {
        for status in ["ACTIVE", "CONSUMED", "REVOKED", "EXPIRED"] {
            let mut v = db.fixture().await;
            let id = if mode == "LEGACY_REQUIRED" {
                let b = &v.binding;
                let id = PermitId::parse(format!("permit_{}", uuid::Uuid::new_v4())).unwrap();
                let fingerprint = compute_operation_fingerprint(&v.payload).unwrap();
                let consumed_at = if status == "CONSUMED" { Some(db.clock.get()) } else { None };
                // Original pre-v2 writer shape, using unchanged v1 defaults at0032.
                sqlx::query("INSERT INTO execution_permits (permit_id,contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,operation_fingerprint,fingerprint_payload,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,issued_at_ms,expires_at_ms,status,consumed_at_ms) VALUES ($1,1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23)")
                    .bind(id.as_str()).bind(v.decision_id.as_str()).bind(b.user_id.as_str()).bind(b.task_id.as_str())
                    .bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str()).bind(b.task_revision as i64)
                    .bind(b.plan_version as i64).bind(b.operation_revision as i64).bind(b.facts_version as i64).bind(b.arguments_hash.as_str())
                    .bind(hash(b).unwrap().as_str()).bind(fingerprint.as_str()).bind(Json(&v.payload)).bind(&v.session_digest)
                    .bind(v.session_version as i64).bind(v.confirmation_ref.as_str()).bind("legacy_auth_reference")
                    .bind(db.clock.get()).bind(db.clock.get()+15_000).bind(status).bind(consumed_at).execute(&db.sql).await.unwrap();
                id
            } else {
                if mode == "NOT_REQUIRED" {
                    v.required_gates = vec![SecurityGate::ExplicitConfirmation];
                    v.authentication = VerifiedPermitAuthentication::NotRequired;
                }
                let p = db.save(&v).await;
                // Ordinary persisted history fixture. No claim of business effects.
                sqlx::query("UPDATE execution_permits SET status=$1,consumed_at_ms=$2 WHERE permit_id=$3")
                    .bind(status).bind(if status=="CONSUMED" {Some(db.clock.get())} else {None})
                    .bind(p.permit_id().as_str()).execute(&db.sql).await.unwrap();
                p.permit_id().clone()
            };
            let expected_version: i16 = if mode=="LEGACY_REQUIRED" {1} else {2};
            let actual: (i16,String,String,String) = sqlx::query_as(
                "SELECT storage_version,authentication_state,status,execution_scope FROM execution_permits WHERE permit_id=$1"
            ).bind(id.as_str()).fetch_one(&db.sql).await.unwrap();
            assert_eq!(actual, (expected_version, mode.into(), status.into(),
                if expected_version==1 {"LEGACY".into()} else {"COMMON".into()}));
            fixtures.push((v,id,mode,status));
        }
    }
    let revoked = db.fixture().await;
    db.confirmation.revoke(&db.session, &revoked.binding.task_id, &revoked.confirmation_ref).await.unwrap();
    let before = rows(&db).await;
    assert_eq!(before.iter().find(|(table,_)|*table=="execution_permits").unwrap().1.len(),12);
    assert_eq!(before.iter().find(|(table,_)|*table=="server_confirmations").unwrap().1.len(),13);
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    db.assert_latest_migration_versions().await;
    let after_checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=$1 ORDER BY version"
    ).bind(baseline).fetch_all(&db.sql).await.unwrap();
    assert_eq!(before_checksums, after_checksums);
    assert_eq!(rows(&db).await, before);
    let nonzero:i64=sqlx::query_scalar("SELECT count(*) FROM execution_permits WHERE card_attempt_no<>0").fetch_one(&db.sql).await.unwrap();assert_eq!(nonzero,0);
    // Each old row is still a tombstone. Positive retry numbers cannot be used
    // for COMMON/LEGACY (including SQL NULL) to bypass the old unique identity.
    let original_id=fixtures[0].1.as_str();
    for scope in [Some("COMMON"),Some("LEGACY"),None] {
        let mut tx=db.sql.begin().await.unwrap();
        let result=sqlx::query("UPDATE execution_permits SET execution_scope=$1,card_attempt_no=1 WHERE permit_id=$2").bind(scope).bind(original_id).execute(&mut *tx).await;
        assert!(result.is_err());tx.rollback().await.unwrap();
    }
    assert_eq!(rows(&db).await,before);
    let present: Option<String> = sqlx::query_scalar("SELECT to_regclass('public_card_safety_requests_v1')::text")
        .fetch_one(&db.sql).await.unwrap();
    assert!(present.is_some());
    let new_permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits WHERE storage_version=3 OR execution_scope='ISOLATED_CARD'")
        .fetch_one(&db.sql).await.unwrap();
    assert_eq!(new_permits,0);
    for (v,id,mode,status) in fixtures {
        let binding = PermitBinding { snapshot: v.binding.clone(),
            fingerprint: compute_operation_fingerprint(&v.payload).unwrap() };
        let read = db.store.read(&db.session,&v.binding.task_id,&id,&binding).await;
        if status == "ACTIVE" {
            let p = read.unwrap();
            let c = db.confirmation.verify(&db.session,&v.binding.task_id,&v.confirmation_ref,&v.binding).await.unwrap();
            assert_eq!(c.status,crate::ConfirmationStatus::Confirmed);
            db.consume(&p).await.unwrap();
            assert_eq!(db.status(&p).await,"CONSUMED");
            assert_eq!(db.read(&p).await.err(),Some(StorageError::PermitNotActive));
        } else {
            assert_eq!(read.err(),Some(StorageError::PermitNotActive));
            assert_eq!(consume(&db.store,&db.session,&id,&binding).await,Err(StorageError::PermitNotActive));
            let original: String = sqlx::query_scalar("SELECT status FROM execution_permits WHERE permit_id=$1")
                .bind(id.as_str()).fetch_one(&db.sql).await.unwrap();
            assert_eq!(original,status);
        }
        let grant: Option<String> = sqlx::query_scalar("SELECT auth_grant_ref FROM execution_permits WHERE permit_id=$1")
            .bind(id.as_str()).fetch_one(&db.sql).await.unwrap();
        assert_eq!(grant.is_none(),mode=="NOT_REQUIRED");
    }
    let mut tx = db.repo.pool.begin().await.unwrap();
    let old = crate::confirmation_repository::read_historical_confirmation(
        &mut tx,&revoked.binding.task_id,&revoked.confirmation_ref).await.unwrap();
    assert_eq!(old.record.status,crate::ConfirmationStatus::Revoked);
    tx.rollback().await.unwrap();
    assert_eq!(db.confirmation.verify(&db.session,&revoked.binding.task_id,&revoked.confirmation_ref,&revoked.binding).await.err(),
        Some(StorageError::ConfirmationRevoked));
    let business: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM card_locks_v1)+(SELECT count(*) FROM transfer_fund_orders_v1)+(SELECT count(*) FROM transfer_ledger_v1)+(SELECT count(*) FROM public_audit_outbox WHERE acknowledged_at IS NOT NULL)")
        .fetch_one(&db.sql).await.unwrap();
    assert_eq!(business,0);
    println!("0032->0034: 32 original checksums and full tasks/operations/confirmations/permits/audit rows unchanged; 12 permits: v1 LEGACY_REQUIRED/LEGACY and v2 VERIFIED_REQUIRED|NOT_REQUIRED/COMMON, each ACTIVE|CONSUMED|REVOKED|EXPIRED; 13 confirmations CONFIRMED|REVOKED; three ACTIVE original permits read/consume once; no v3/v4 conversion or business/ACK writes; original function name retained as historical test identity");
    db.finish().await;
}

// Test-only current maintenance schema, using the REAL reviewed 43/44 SQL.
// No zero-return stub, fabricated continuation or background permission. This
// generic isolated fixture has no deployed cohort; native deployment/CAS is
// exercised separately by the owned 42 -> 43 -> 44 -> 45 integration harness.
async fn current_maintenance_fixture(pool:&sqlx::PgPool,schema:&str) {
    assert!(schema.starts_with("sf_") && schema.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'));
    let mut tx=pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('search_path',$1,true)").bind(schema).execute(&mut *tx).await.unwrap();
    for (version,name,raw) in [
        (43_i64,"noon new authority",include_str!("../maintenance/0043_noon_new_authority.sql")),
        (44_i64,"simulation balance and draft intent",include_str!("../maintenance/0044_simulation_balance.sql")),
    ] {
        let migration=sqlx::migrate::Migration::new(version,name.into(),sqlx::migrate::MigrationType::Simple,raw.replace("__SCHEMA__",schema).replace("\r\n","\n").into(),false);
        sqlx::raw_sql(&migration.sql).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES($1,$2,true,$3,0)")
            .bind(version).bind(name).bind(migration.checksum.as_ref()).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
}
