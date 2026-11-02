//! Public technical tests using unchanged member ledger JSON and the real
//! member Store/Reader, common confirmations, Kernel and PermitTransaction.
//! No fabricated subscription domain event or external ACK is installed.
use super::*;
#[path = "subscription_a2_tests.rs"]
mod a2_tests;
#[path = "subscription_repair_tests.rs"]
mod repair_tests;
#[path = "subscription_registration_tests.rs"]
mod registration_tests;
#[path = "simulation_source_tests.rs"]
mod simulation_source_tests;
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeBootstrap, ToolRegistry,
    ToolSecurityCatalog,
};
use safeflow_subscription_evidence::managed_ledger_mock::LedgerFixture;
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::{collections::HashMap, sync::RwLock};
#[derive(Default)]
struct Sessions(RwLock<HashMap<SessionRef, SessionBinding>>);
impl Sessions {
    fn bind(&self, s: &SessionRef, user: &str, v: u64) {
        self.0.write().unwrap().insert(
            s.clone(),
            SessionBinding {
                user_id: UserId::parse(user).unwrap(),
                binding_version: v,
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
            reason_code: ReasonCode::parse("SESSION_MISSING").unwrap(),
            message: "missing".into(),
        })
    }
}
pub(crate) fn initial(case: &str) -> LedgerFixture {
    let v: Value =
        serde_json::from_str(include_str!(env!("SAFEFLOW_SUBSCRIPTION_LEDGER_FIXTURE"))).unwrap();
    serde_json::from_value(
        v["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["case_id"] == case)
            .unwrap()["ledger"]
            .clone(),
    )
    .unwrap()
}
struct Source(&'static str);
impl crate::SubscriptionInitialSource for Source {
    fn load<'a>(&'a self, _: &'a str) -> ContractFuture<'a, Result<LedgerFixture, StorageError>> {
        Box::pin(async move { Ok(initial(self.0)) })
    }
}
struct Db {
    schema: String,
    admin: PgPool,
    read: SubscriptionService,
    sessions: Arc<Sessions>,
    session: SessionRef,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("explicit isolated PostgreSQL required");
        let schema = format!("sf_subscription_test_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        let sessions = Arc::new(Sessions::default());
        let session = SessionRef::parse("http_sim_isolated_non_replay_reference").unwrap();
        sessions.bind(&session, "user_demo", 1);
        let read = SubscriptionService::for_trusted_backend(repo, sessions.clone());
        Self {
            schema,
            admin,
            read,
            sessions,
            session,
        }
    }
    async fn initialize(&self, case: &'static str) {
        self.read
            .clone()
            .with_initial_source(Arc::new(Source(case)))
            .initialize_for_host(&self.session)
            .await
            .unwrap();
    }
    async fn journal(&self) -> Value {
        sqlx::query_scalar(
            "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
        )
        .fetch_one(&self.read.repository.pool)
        .await
        .unwrap()
    }
    async fn facts(&self) -> Value {
        let mut all = serde_json::Map::new();
        for name in [
            "subscription_runtime_records",
            "subscription_public_bindings_v1",
            "subscription_preparations_v1",
            "subscription_host_generations_v1",
            "tasks",
            "operations",
            "server_confirmations",
            "execution_permits",
            "public_execution_requests",
            "audit_events",
            "public_audit_outbox",
        ] {
            let rows:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(v ORDER BY v::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) v FROM {name} t) q")).fetch_one(&self.read.repository.pool).await.unwrap();
            all.insert(name.into(), rows);
        }
        Value::Object(all)
    }
    fn assembly(&self) -> safeflow_runtime::RuntimeAssembly {
        RuntimeBootstrap::build(
            ToolRegistry::new(),
            DomainPolicyRegistry::new(),
            ToolSecurityCatalog::new(),
            self.sessions.clone(),
            Arc::new(InMemoryAuditRecorder::new()),
        )
        .unwrap()
    }
    fn writes<'a>(&self, runtime: &'a Runtime) -> SubscriptionWriteService<'a> {
        SubscriptionWriteService::for_trusted_backend(
            self.read.repository.clone(),
            self.sessions.clone(),
            Arc::new(safeflow_mock_bank::MockBankGateway::new()),
            runtime,
            ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
            PermitConfig::new(Duration::from_secs(30)).unwrap(),
        )
        .unwrap()
    }
    async fn finish(self) {
        self.read.repository.close().await;
        assert!(self.schema.starts_with("sf_subscription_test_") && self.schema.len() == 53);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn request(action: Action) -> SubscriptionChangeRequestMock {
    let subscription_id = "fictional-subscription-1".into();
    match action {
        Action::RecordCancellation => {
            SubscriptionChangeRequestMock::CreateCancellationIntent { subscription_id }
        }
        Action::RevokeCancellation => {
            SubscriptionChangeRequestMock::RevokeCancellationIntent { subscription_id }
        }
        // Exact member shared_ledger_write_probe.rs schedule, unchanged.
        Action::ReminderSettings => SubscriptionChangeRequestMock::UpdateReminderSettings {
            subscription_id,
            schedule: Schedule {
                cycle: safeflow_subscription_evidence::recognition::Cycle::CalendarMonth,
                anchor_date: "2026-09-17".parse().unwrap(),
                period_index: 1,
                expected_date: "2026-10-17".parse().unwrap(),
                enabled: true,
                days_before: 3,
            },
        },
        Action::Registration => panic!("A1 cannot fabricate a selection"),
    }
}
#[tokio::test]
#[ignore]
async fn subscription_selected_details_are_owner_session_version_expiry_bound_and_readonly() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let before = db.facts().await;
    let list = db.read.read(&db.session).await.unwrap();
    let choice = list["items"][0]["selection_ref"].as_str().unwrap();
    let detail = db.read.details(&db.session, choice).await.unwrap();
    assert_eq!(detail["current_state"], "ACTIVE");
    assert_eq!(detail["cancellation"]["history"], json!([]));
    assert_eq!(
        detail["cancellation"]["merchant_cancellation_confirmed"],
        false
    );
    assert_eq!(detail["reminder"]["notification_sent"], false);
    for private in [
        "user_demo",
        "subscription_id",
        "merchant_id",
        "session_ref",
        "original_binding",
        "journal",
        "anchor_date",
        "cycle_ref",
    ] {
        assert!(!detail.to_string().contains(private), "{private}");
    }
    let same = SessionRef::parse("same_owner_new_session").unwrap();
    db.sessions.bind(&same, "user_demo", 1);
    assert_eq!(
        db.read.details(&same, choice).await.err(),
        Some(StorageError::FactsConflict)
    );
    let other = SessionRef::parse("different_detail_owner").unwrap();
    db.sessions.bind(&other, "another_user", 1);
    assert_eq!(
        db.read.details(&other, choice).await.err(),
        Some(StorageError::FactsConflict)
    );
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        db.read.details(&db.session, choice).await.err(),
        Some(StorageError::FactsConflict)
    );
    let list = db.read.read(&db.session).await.unwrap();
    let choice = list["items"][0]["selection_ref"].as_str().unwrap();
    db.read.expire_read_selections_for_test();
    assert_eq!(
        db.read.details(&db.session, choice).await.err(),
        Some(StorageError::FactsConflict)
    );
    let list = db.read.read(&db.session).await.unwrap();
    let choice = list["items"][0]["selection_ref"].as_str().unwrap();
    let _fresh = db.read.read(&db.session).await.unwrap();
    assert_eq!(
        db.read.details(&db.session, choice).await.err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(
        before,
        db.facts().await,
        "all list/detail attempts must be read-only"
    );
    let list = db.read.read(&db.session).await.unwrap();
    let choice = list["items"][0]["selection_ref"].as_str().unwrap();
    db.read.startup_for_host(&db.session).await.unwrap();
    let changed = db.facts().await;
    assert_eq!(
        db.read.details(&db.session, choice).await.err(),
        Some(StorageError::FactsConflict)
    );
    let fresh = db.read.read(&db.session).await.unwrap();
    assert!(db
        .read
        .details(
            &db.session,
            fresh["items"][0]["selection_ref"].as_str().unwrap()
        )
        .await
        .is_ok());
    assert_eq!(changed, db.facts().await);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_missing_source_owner_reads_and_no_implicit_restart() {
    let db = Db::new().await;
    let before = db.facts().await;
    assert_eq!(
        db.read.read(&db.session).await.unwrap()["status"],
        "NO_LEDGER"
    );
    assert_eq!(
        db.read.initialize_for_host(&db.session).await.err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(before, db.facts().await);
    db.initialize("already-active").await;
    let before = db.facts().await;
    let v = db.read.read(&db.session).await.unwrap();
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    let encoded = v.to_string();
    for private in [
        "user_demo",
        "fictional-subscription-1",
        "owner_id",
        "journal",
        "session_ref",
        "operation_id",
    ] {
        assert!(!encoded.contains(private));
    }
    assert_eq!(before, db.facts().await);
    db.initialize("already-active").await;
    assert_eq!(before, db.facts().await);
    let other = SessionRef::parse("different_owner").unwrap();
    db.sessions.bind(&other, "another_user", 1);
    assert_eq!(db.read.read(&other).await.unwrap()["status"], "NO_LEDGER");
    let current = db.sessions.resolve_binding(&db.session).unwrap();
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        db.read.check(&db.session, &current).err(),
        Some(StorageError::Session)
    );
    db.sessions.0.write().unwrap().remove(&db.session);
    assert_eq!(
        db.read.read(&db.session).await.err(),
        Some(StorageError::Session)
    );
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_migration_full_prefix_cas_and_codec() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    for sql in [
        "DELETE FROM subscription_runtime_records",
        "UPDATE subscription_runtime_records SET owner_id='someone_else'",
        "UPDATE subscription_runtime_records SET encoding_version='new'",
        "UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{initial,snapshot_id}','\"changed\"')",
        "UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{rule_version}','\"changed\"')",
        "UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{fictional}','false')",
        "UPDATE subscription_runtime_records SET storage_version=storage_version+1",
    ]{let mut tx=db.read.repository.pool.begin().await.unwrap();assert!(sqlx::query(sql).execute(&mut *tx).await.is_err(),"accepted mutation");tx.rollback().await.unwrap();}
    let store = db.read.store().unwrap();
    let mut one = db.read.repository.pool.acquire().await.unwrap();
    let mut a = store
        .load_work_on_connection_mock(&mut one, "user_demo")
        .await
        .unwrap();
    let mut b = store
        .load_work_on_connection_mock(&mut one, "user_demo")
        .await
        .unwrap();
    a.invalidate_pending_for_restart_mock().await.unwrap();
    b.invalidate_pending_for_restart_mock().await.unwrap();
    use sqlx::Connection;
    let mut tx = one.begin().await.unwrap();
    let pending = store.save_on_connection_mock(&mut tx, a).await.unwrap();
    tx.commit().await.unwrap();
    assert!(pending
        .read_committed_on_connection_mock(&mut one)
        .await
        .is_err());
    let mut independent = db.read.repository.pool.acquire().await.unwrap();
    pending
        .read_committed_on_connection_mock(&mut independent)
        .await
        .unwrap();
    drop(independent);
    let mut tx = one.begin().await.unwrap();
    assert!(store.save_on_connection_mock(&mut tx, b).await.is_err());
    tx.rollback().await.unwrap();
    drop(one);
    let before = db.journal().await;
    let mut tx = db.read.repository.pool.begin().await.unwrap();
    assert!(sqlx::query(
        "UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{commands,0}','{}')"
    )
    .execute(&mut *tx)
    .await
    .is_err());
    tx.rollback().await.unwrap();
    assert_eq!(before, db.journal().await);
    // SQL metadata is intact, but member decoder must reject malformed commands.
    let mut tx = db.read.repository.pool.begin().await.unwrap();
    sqlx::query("UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{commands}',journal->'commands'||'[{}]'::jsonb),storage_version=storage_version+1").execute(&mut *tx).await.unwrap();
    assert!(matches!(
        ProposedLedgerReaderMock::new(&db.schema)
            .unwrap()
            .load_on_connection_mock(&mut tx, "user_demo")
            .await,
        Err(ProposedLedgerReadIssue::InvalidStoredRecord)
    ));
    tx.rollback().await.unwrap();
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_startup_fences_old_host_and_invalidates_drafts() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = db.writes(assembly.runtime());
    let p = writes
        .prepare_technical(
            &db.session,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            Preparation::Change(request(Action::RecordCancellation)),
        )
        .await
        .unwrap();
    let before = db.facts().await;
    assert_eq!(
        writes
            .prepare_change(
                &db.session,
                &host,
                &uuid::Uuid::new_v4().to_string(),
                request(Action::RecordCancellation)
            )
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(before, db.facts().await);
    let new = db.read.startup_for_host(&db.session).await.unwrap();
    let mut tx = db.read.repository.pool.begin().await.unwrap();
    assert_eq!(
        db.read.check_host(&mut tx, &host, "user_demo").await.err(),
        Some(StorageError::FactsConflict)
    );
    db.read
        .check_host(&mut tx, &new, "user_demo")
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let id = db.sessions.resolve_binding(&db.session).unwrap();
    let mut c = db.read.repository.pool.acquire().await.unwrap();
    let (_, _, original) = load_binding(&mut c, &id, &db.session, &p.operation.operation_id)
        .await
        .unwrap();
    let snapshot = db.read.load(&mut c, "user_demo").await.unwrap();
    assert!(matches!(
        snapshot.original_result_mock(&original).unwrap(),
        OriginalResultMock::NeedsReview
    ));
    drop(c);
    let view = db.read.read(&db.session).await.unwrap();
    assert!(!view["pending"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["state"] == "AWAITING_CONFIRMATION"));
    drop(writes);
    drop(assembly);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_three_real_permit_stages_rollback_without_a2() {
    for (action, case) in [
        (Action::RecordCancellation, "already-active"),
        (Action::RevokeCancellation, "cancel-intent-still-managed"),
        (Action::ReminderSettings, "already-active"),
    ] {
        let db = Db::new().await;
        db.initialize(case).await;
        let host = db.read.startup_for_host(&db.session).await.unwrap();
        let assembly = db.assembly();
        let writes = db.writes(assembly.runtime());
        let p = writes
            .prepare_technical(
                &db.session,
                &host,
                &uuid::Uuid::new_v4().to_string(),
                Preparation::Change(request(action)),
            )
            .await
            .unwrap();
        writes
            .confirmations
            .record_explicit_confirmation(
                &db.session,
                &p.task.task_id,
                &p.confirmation.confirmation_ref,
                &p.confirmation.binding,
            )
            .await
            .unwrap();
        let before = db.facts().await;
        let err = writes
            .execute_technical(
                &db.session,
                &host,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
            .await
            .err();
        assert_eq!(
            err,
            Some(StorageError::DependencyUnavailable),
            "must reach missing domain mapping AFTER real member stage and common submission"
        );
        assert_eq!(
            before,
            db.facts().await,
            "member/public/permit/audit/outbox must all roll back"
        );
        let safe = db.read.read(&db.session).await.unwrap();
        assert_eq!(safe["originals"][0]["status"], "NEEDS_REVIEW");
        assert_eq!(
            writes
                .execute(
                    &db.session,
                    &host,
                    &p.task.task_id,
                    &p.operation.operation_id,
                    &p.confirmation.confirmation_ref
                )
                .await
                .err(),
            Some(StorageError::DependencyUnavailable)
        );
        assert_eq!(before, db.facts().await);
        drop(writes);
        drop(assembly);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn subscription_public_binding_immutable_full_gate_and_current_owner_original() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = db.writes(assembly.runtime());
    let p = writes
        .prepare_technical(
            &db.session,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            Preparation::Change(request(Action::RecordCancellation)),
        )
        .await
        .unwrap();
    let row =
        sqlx::query("SELECT business_ref,member_binding FROM subscription_public_bindings_v1")
            .fetch_one(&db.read.repository.pool)
            .await
            .unwrap();
    let b: String = row.try_get("business_ref").unwrap();
    let member: Value = row.try_get("member_binding").unwrap();
    assert_ne!(b, p.operation.operation_id.to_string());
    for statement in [
        "DELETE FROM subscription_public_bindings_v1",
        "UPDATE subscription_public_bindings_v1 SET member_binding='{}'",
        "UPDATE subscription_public_bindings_v1 SET operation_id='changed'",
        "UPDATE subscription_public_bindings_v1 SET public_parameters='{}'",
        "UPDATE subscription_public_bindings_v1 SET binding_version=binding_version+1",
    ] {
        let mut tx = db.read.repository.pool.begin().await.unwrap();
        assert!(sqlx::query(statement).execute(&mut *tx).await.is_err());
        tx.rollback().await.unwrap();
    }
    for field in [
        "context",
        "target",
        "ledger_revision",
        "expires_at",
        "draft_id",
        "kind",
    ] {
        let mut changed = member.clone();
        changed[field] = Value::Null;
        let mut gate = Gate {
            expected: member.clone(),
            time: now(),
            used: false,
        };
        assert_eq!(gate.check(changed, None), MockGateOutcome::Denied);
    }
    let other = SessionRef::parse("same_owner_new_session").unwrap();
    db.sessions.bind(&other, "user_demo", 2);
    assert_eq!(
        db.read.original(&other, &b).await.unwrap()["status"],
        "NEEDS_REVIEW"
    );
    let foreign = SessionRef::parse("foreign_owner").unwrap();
    db.sessions.bind(&foreign, "someone_else", 1);
    assert_eq!(
        db.read.original(&foreign, &b).await.err(),
        Some(StorageError::NotFound)
    );
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        writes
            .execute_technical(
                &db.session,
                &host,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    drop(writes);
    drop(assembly);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_audit_error_and_dropped_future_rollback_after_member_stage() {
    for cancel in [false, true] {
        let db = Db::new().await;
        db.initialize("already-active").await;
        let host = db.read.startup_for_host(&db.session).await.unwrap();
        let assembly = db.assembly();
        let writes = db.writes(assembly.runtime());
        let p = writes
            .prepare_technical(
                &db.session,
                &host,
                &uuid::Uuid::new_v4().to_string(),
                Preparation::Change(request(Action::RecordCancellation)),
            )
            .await
            .unwrap();
        writes
            .confirmations
            .record_explicit_confirmation(
                &db.session,
                &p.task.task_id,
                &p.confirmation.confirmation_ref,
                &p.confirmation.binding,
            )
            .await
            .unwrap();
        let before = db.facts().await;
        let effect = if cancel {
            "PERFORM pg_advisory_xact_lock(93872111);"
        } else {
            "RAISE EXCEPTION 'isolated audit failure';"
        };
        let sql=format!("CREATE FUNCTION fail_subscription_audit_test() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='OPERATION_TRANSITIONED' AND NEW.details->>'event'='SUBMISSION_RECORDED' THEN {effect} END IF; RETURN NEW; END $$; CREATE TRIGGER fail_subscription_audit_test BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_subscription_audit_test();");
        sqlx::raw_sql(&sql)
            .execute(&db.read.repository.pool)
            .await
            .unwrap();
        if cancel {
            let mut blocker = db.admin.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock(93872111)")
                .execute(&mut *blocker)
                .await
                .unwrap();
            let execute = writes.execute_technical(
                &db.session,
                &host,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            );
            let mut execute = Box::pin(execute);
            let wait = async {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory' AND query LIKE 'INSERT INTO audit_events%'").fetch_one(&db.admin).await.unwrap();
                    if count > 0 {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "stage did not reach the post-member audit lock"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            tokio::select! {result=&mut execute=>panic!("execution completed before cancellation: {:?}",result.err()),_=wait=>{}}
            // Drop the actual execution future while it waits AFTER member CAS,
            // public permit consumption and submission writes; then release SQL.
            drop(execute);
            blocker.rollback().await.unwrap();
        } else {
            assert_eq!(
                writes
                    .execute_technical(
                        &db.session,
                        &host,
                        &p.task.task_id,
                        &p.operation.operation_id,
                        &p.confirmation.confirmation_ref
                    )
                    .await
                    .err(),
                Some(StorageError::Database)
            );
        }
        assert_eq!(
            before,
            db.facts().await,
            "failed audit or cancellation must retain every original fact"
        );
        drop(writes);
        drop(assembly);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn subscription_parallel_host_startups_have_distinct_readback_backend() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            db.read.startup_for_host(&db.session),
            db.read.startup_for_host(&db.session)
        )
    })
    .await
    .expect("verification must not deadlock behind mutation pool");
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a.generation, b.generation);
    let mut reader = db
        .read
        .repository
        .subscription_read_pool
        .acquire()
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE subscription_host_generations_v1 SET generation=generation+1")
            .execute(&mut *reader)
            .await
            .is_err()
    );
    drop(reader);
    let journal = db.journal().await;
    assert_eq!(journal["commands"].as_array().unwrap().len(), 2);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_known_empty_conflicting_initial_and_corruption_are_distinct() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let empty = db.read.read(&db.session).await.unwrap();
    assert_eq!(empty["status"], "AVAILABLE");
    assert_eq!(empty["items"], json!([]));
    let before = db.facts().await;
    assert!(db
        .read
        .clone()
        .with_initial_source(Arc::new(Source("already-active")))
        .initialize_for_host(&db.session)
        .await
        .is_err());
    assert_eq!(before, db.facts().await);
    sqlx::query("UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{commands}',journal->'commands'||'[{}]'::jsonb),storage_version=storage_version+1").execute(&db.read.repository.pool).await.unwrap();
    assert_eq!(
        db.read.read(&db.session).await.err(),
        Some(StorageError::CorruptRecord)
    );
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_revocation_during_real_read_never_publishes_old_owner_data() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let mut blocker = db.admin.begin().await.unwrap();
    sqlx::query(&format!(
        "LOCK TABLE {}.subscription_runtime_records IN ACCESS EXCLUSIVE MODE",
        db.schema
    ))
    .execute(&mut *blocker)
    .await
    .unwrap();
    let mut read = Box::pin(db.read.read(&db.session));
    let change = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT storage_version,encoding_version,journal FROM%'").fetch_one(&db.admin).await.unwrap();
            if n > 0 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        db.sessions.bind(&db.session, "another_user", 2);
        blocker.rollback().await.unwrap();
    };
    let (result, _) = tokio::join!(&mut read, change);
    assert_eq!(result.err(), Some(StorageError::Session));
    drop(read);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_real_pg_restart_seed() {
    let root = std::path::PathBuf::from(
        std::env::var("SAFEFLOW_SUBSCRIPTION_RESTART_ROOT")
            .expect("explicit restart evidence directory"),
    );
    assert!(root.is_absolute());
    let state = root.join("restart-state.json");
    assert!(!state.exists());
    let db = Db::new().await;
    db.initialize("already-active").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let mut writes = db.writes(assembly.runtime());
    writes.config = ConfirmationConfig::new(Duration::from_secs(600)).unwrap();
    writes.confirmations = ConfirmationService::for_trusted_backend(
        db.read.repository.clone(),
        db.sessions.clone(),
        writes.config.clone(),
    );
    let nonce = uuid::Uuid::new_v4().to_string();
    let p = writes
        .prepare_technical(
            &db.session,
            &host,
            &nonce,
            Preparation::Change(request(Action::RecordCancellation)),
        )
        .await
        .unwrap();
    let business: String =
        sqlx::query_scalar("SELECT business_ref FROM subscription_public_bindings_v1")
            .fetch_one(&db.read.repository.pool)
            .await
            .unwrap();
    let journal = db.journal().await;
    std::fs::write(state,serde_json::to_vec_pretty(&json!({"schema":db.schema,"journal":journal,"business_ref":business,"operation":p.operation.operation_id,"nonce":nonce,"preparation":p,"host":{"namespace":host.namespace,"owner":host.owner,"instance":host.instance,"generation":host.generation}})).unwrap()).unwrap();
    drop(writes);
    drop(assembly);
    db.read.repository.close().await;
    db.admin.close().await;
}
#[tokio::test]
#[ignore]
async fn subscription_real_pg_restart_verify() {
    let root = std::path::PathBuf::from(
        std::env::var("SAFEFLOW_SUBSCRIPTION_RESTART_ROOT")
            .expect("explicit restart evidence directory"),
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.join("restart-state.json")).unwrap()).unwrap();
    let schema = saved["schema"].as_str().unwrap();
    assert!(schema.starts_with("sf_subscription_test_") && schema.len() == 53);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect_existing(&url, schema).await.unwrap();
    let sessions = Arc::new(Sessions::default());
    let s = SessionRef::parse("new_process_new_session_reference").unwrap();
    sessions.bind(&s, "user_demo", 2);
    let original_session = SessionRef::parse("http_sim_isolated_non_replay_reference").unwrap();
    sessions.bind(&original_session, "user_demo", 1);
    let assembly = RuntimeBootstrap::build(
        ToolRegistry::new(),
        DomainPolicyRegistry::new(),
        ToolSecurityCatalog::new(),
        sessions.clone(),
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap();
    let writes = SubscriptionWriteService::for_trusted_backend(
        repo.clone(),
        sessions.clone(),
        Arc::new(safeflow_mock_bank::MockBankGateway::new()),
        assembly.runtime(),
        ConfirmationConfig::new(Duration::from_secs(600)).unwrap(),
        PermitConfig::new(Duration::from_secs(30)).unwrap(),
    )
    .unwrap();
    let host = SubscriptionHost {
        namespace: saved["host"]["namespace"].as_str().unwrap().into(),
        owner: saved["host"]["owner"].as_str().unwrap().into(),
        instance: saved["host"]["instance"].as_str().unwrap().into(),
        generation: saved["host"]["generation"].as_i64().unwrap(),
    };
    let count_before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    let replay = writes
        .prepare_technical(
            &original_session,
            &host,
            saved["nonce"].as_str().unwrap(),
            Preparation::Change(request(Action::RecordCancellation)),
        )
        .await
        .unwrap();
    assert_eq!(json!(replay),saved["preparation"],"new process after real PostgreSQL restart must recover exact original nonce/O/B/key/deadline");
    let count_after: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(count_before, count_after);
    drop(writes);
    drop(assembly);
    let read = SubscriptionService::for_trusted_backend(repo, sessions);
    let actual: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&read.repository.pool)
    .await
    .unwrap();
    assert_eq!(actual, saved["journal"]);
    let before = read.read(&s).await.unwrap();
    assert_eq!(before["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        read.original(&s, saved["business_ref"].as_str().unwrap())
            .await
            .unwrap()["status"],
        "NEEDS_REVIEW"
    );
    read.startup_for_host(&s).await.unwrap();
    let actual: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&read.repository.pool)
    .await
    .unwrap();
    let old = saved["journal"]["commands"].as_array().unwrap();
    assert!(actual["commands"].as_array().unwrap().starts_with(old));
    assert_eq!(actual["commands"].as_array().unwrap().len(), old.len() + 1);
    let out = root.join("restart-verified.json");
    assert!(!out.exists());
    std::fs::write(out,serde_json::to_vec_pretty(&json!({"actual_pg_restart":true,"same_nonce_exact_preparation_recovered":true,"replay_audit_count_unchanged":true,"original_journal_preserved":true,"new_session_owner_read":true,"explicit_startup_appended_restart":true,"original_result":"NEEDS_REVIEW","no_business_reexecution":true})).unwrap()).unwrap();
    read.repository.close().await;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}
