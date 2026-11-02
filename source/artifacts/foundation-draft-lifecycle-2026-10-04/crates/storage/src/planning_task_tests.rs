//! Same approved Tool/Policy fixture as intake. PG tests never execute either one.
use super::*;
use crate::{
    new_task_id, RecoveryPoint, TaskQuerySecurityScope, TaskQueryService, TaskRecoveryHint,
};
#[path = "../examples/support/planning_runtime.rs"]
mod support;
use safeflow_contracts::*;
use safeflow_runtime::*;
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        RwLock,
    },
    time::Duration,
};

fn session() -> SessionRef {
    SessionRef::parse("planning_create_private_session").unwrap()
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
                user_id: UserId::parse("user_001").unwrap(),
                binding_version: 1,
            })),
            calls: AtomicUsize::new(0),
            revoke_at: AtomicUsize::new(usize::MAX),
        })
    }
    fn change(&self, user: Option<&str>) {
        *self.binding.write().unwrap() = user.map(|u| SessionBinding {
            user_id: UserId::parse(u).unwrap(),
            binding_version: 2,
        });
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        panic!("must use original versioned binding")
    }
    fn resolve_binding(&self, _: &SessionRef) -> Result<SessionBinding, SecurityError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.revoke_at.load(Ordering::SeqCst) {
            self.change(None);
        }
        self.binding.read().unwrap().clone().ok_or(SecurityError {
            reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
            message: "private diagnostic".into(),
        })
    }
}
fn clarify() -> PlanningResult {
    PlanningResult {
        contract_version: 1,
        understanding: Understanding {
            intent: "READ_BALANCE".into(),
            entities: json!({}),
            missing_fields: vec!["account_id".into()],
            context_refs: vec![],
            decision: UnderstandingDecision::Clarify,
        },
        plan: None,
        clarification: Some(Clarification {
            question: "which account?".into(),
        }),
    }
}
fn raw(proposal: &PlanningResult) -> Vec<u8> {
    serde_json::to_vec(proposal).unwrap()
}
fn error(
    outcome: Result<PlanningTaskCreationOutcome, PlanningTaskCreationError>,
) -> PlanningTaskCreationError {
    match outcome {
        Err(e) => e,
        Ok(_) => panic!("unexpected task/proposal acceptance"),
    }
}
fn task(outcome: PlanningTaskCreationOutcome) -> TaskRecord {
    match outcome {
        PlanningTaskCreationOutcome::TaskCreated(t) => t,
        _ => panic!("expected initial persistent task"),
    }
}
fn lazy_repo() -> PgRepository {
    PgRepository {
        revision_gate_pool: PgPoolOptions::new().max_connections(1)
            .connect_lazy("postgres://localhost/unused_planning_test")
            .unwrap(),
        simulation_source_pool: PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused_planning_test")
            .unwrap(),
        subscription_read_pool: PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused_planning_test")
            .unwrap(),
        lifecycle_time_pool: PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused_planning_test")
            .unwrap(),
        pool: PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused_planning_test")
            .unwrap(),
        namespace: "unused_planning_test".into(),
    }
}

#[tokio::test]
async fn clarify_has_no_task_and_never_acquires_a_database_connection_on_either_route() {
    let sessions = Sessions::new();
    let (assembly, observed, audit) = support::assemble(sessions, None);
    let repo = lazy_repo();
    let expected = new_task_id();
    let proposal = clarify();
    let mock = Arc::new(MockLlm::new(proposal.clone()));
    let service = PlanningTaskCreationService::for_trusted_backend(
        repo.clone(),
        assembly.runtime(),
        Some(mock.clone()),
        support::contracts(),
    )
    .unwrap();
    for outcome in [
        service
            .accept_json_and_create(&session(), &expected, 2, 1, &raw(&proposal))
            .await
            .unwrap(),
        service
            .plan_and_create(&session(), &expected, 2, 1, "query")
            .await
            .unwrap(),
    ] {
        match outcome {
            PlanningTaskCreationOutcome::Clarification(result) => assert_eq!(result, proposal),
            _ => panic!("CLARIFY created a task"),
        }
    }
    assert_eq!(mock.call_count(), 1);
    assert_eq!(repo.pool.size(), 0);
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
}

#[tokio::test]
async fn rejected_envelopes_parameters_versions_and_facts_never_reach_database_or_tools() {
    let (assembly, observed, audit) = support::assemble(Sessions::new(), None);
    let repo = lazy_repo();
    let id = new_task_id();
    let service = PlanningTaskCreationService::for_trusted_backend(
        repo.clone(),
        assembly.runtime(),
        None,
        support::contracts(),
    )
    .unwrap();
    let base = serde_json::to_value(support::proposal(&id, 2)).unwrap();
    for (pointer, changed) in [
        ("/plan/plan_version", json!(3)),
        ("/plan/task_id", json!("another_task")),
        ("/plan/nodes/0/tool", json!("unknown_tool")),
        ("/plan/nodes/0/arguments/account_id", json!(200.0)),
    ] {
        let mut invalid = base.clone();
        *invalid.pointer_mut(pointer).unwrap() = changed;
        assert!(service
            .accept_json_and_create(
                &session(),
                &id,
                2,
                1,
                &serde_json::to_vec(&invalid).unwrap()
            )
            .await
            .is_err());
    }
    assert!(service
        .accept_json_and_create(&session(), &id, 2, 1, b"{broken")
        .await
        .is_err());
    let mut injected = base.clone();
    injected["owner_id"] = json!("user_other");
    assert!(service
        .accept_json_and_create(
            &session(),
            &id,
            2,
            1,
            &serde_json::to_vec(&injected).unwrap()
        )
        .await
        .is_err());
    assert!(service
        .accept_json_and_create(&session(), &id, 2, 0, &raw(&clarify()))
        .await
        .is_err());
    assert!(service
        .plan_and_create(&session(), &id, 2, 1, "query")
        .await
        .is_err());
    assert_eq!(repo.pool.size(), 0);
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
}

#[tokio::test]
async fn late_same_user_rebind_invalidates_the_sealed_review_and_other_runtime_cannot_use_it() {
    let sessions = Sessions::new();
    let (assembly, observed, audit) = support::assemble(sessions.clone(), None);
    let intake =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, support::contracts())
            .unwrap();
    let id = new_task_id();
    let review = intake
        .accept_json_for_persistence(&session(), &id, 2, &raw(&support::proposal(&id, 2)))
        .await
        .unwrap();
    let (other, _, _) = support::assemble(sessions.clone(), None);
    let second =
        PlanningIntakeService::for_trusted_backend(other.runtime(), None, support::contracts())
            .unwrap();
    assert_eq!(
        second.check_persistence_review(&review),
        Err(PlanningIntakeError::Registration)
    );
    sessions.change(Some("user_001"));
    assert_eq!(
        intake.check_persistence_review(&review),
        Err(PlanningIntakeError::Session)
    );
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
}

struct Db {
    schema: String,
    admin: PgPool,
    sql: PgPool,
    repo: PgRepository,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("real PG16 required; no skip");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("database unavailable"));
        let schema = format!("sf_planning_create_{}", uuid::Uuid::new_v4().simple());
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
        Self {
            schema,
            admin,
            sql,
            repo,
        }
    }
    async fn snapshot(&self) -> Value {
        sqlx::query_scalar("SELECT jsonb_build_object('tasks',COALESCE((SELECT jsonb_agg(to_jsonb(t) ORDER BY task_id) FROM tasks t),'[]'::jsonb),'audit',COALESCE((SELECT jsonb_agg(to_jsonb(a) ORDER BY sequence) FROM audit_events a),'[]'::jsonb))").fetch_one(&self.sql).await.unwrap()
    }
    async fn assert_no_authority(&self) {
        let count:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM operations)+(SELECT count(*) FROM server_confirmations)+(SELECT count(*) FROM security_decisions)+(SELECT count(*) FROM execution_permits)+(SELECT count(*) FROM simulation_auth_grants)").fetch_one(&self.sql).await.unwrap();
        assert_eq!(count, 0);
    }
    async fn wait_lock(&self, table: &str) {
        tokio::time::timeout(Duration::from_secs(10),async { loop { let wait:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation=$1::regclass AND NOT granted)").bind(format!("{}.{}",self.schema,table)).fetch_one(&self.admin).await.unwrap(); if wait { break; } tokio::time::sleep(Duration::from_millis(5)).await; } }).await.expect("request did not wait for real SQL lock");
    }
    async fn finish(self) {
        self.repo.close().await;
        self.sql.close().await;
        assert!(self.schema.starts_with("sf_planning_create_"));
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
struct WaitingGateway {
    mock: MockLlm,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl LlmGateway for WaitingGateway {
    fn generate<'a>(
        &'a self,
        input: &'a PlanningInput,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        Box::pin(async move {
            let result = self.mock.generate(input).await;
            self.entered.notify_one();
            self.release.notified().await;
            result
        })
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn pass_raw_and_gateway_create_initial_tasks_queryable_by_current_owner_without_execution() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let (assembly, observed, audit) = support::assemble(sessions.clone(), None);
    for typed in [false, true] {
        let id = new_task_id();
        let proposal = support::proposal(&id, 2);
        let mock = Arc::new(MockLlm::new(proposal.clone()));
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            Some(mock.clone()),
            support::contracts(),
        )
        .unwrap();
        let saved = task(if typed {
            service
                .plan_and_create(&session(), &id, 2, 7, "query")
                .await
                .unwrap()
        } else {
            service
                .accept_json_and_create(&session(), &id, 2, 7, &raw(&proposal))
                .await
                .unwrap()
        });
        assert_eq!(saved.plan, proposal.plan.unwrap());
        assert_eq!(saved.owner_id, UserId::parse("user_001").unwrap());
        assert_eq!(saved.state, TaskState::Running);
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.facts_version, 7);
        assert!(saved.completed_nodes.is_empty());
        assert_eq!(
            saved.checkpoint,
            RecoveryPoint::AtNode {
                node_id: NodeId::parse("read").unwrap()
            }
        );
        assert_eq!(mock.call_count(), usize::from(typed));
        let query = TaskQueryService::for_trusted_backend(db.repo.clone(), assembly.runtime());
        let view = query.query(&session(), &id).await.unwrap();
        assert_eq!(view.task_state, TaskState::Running);
        assert!(view.operations.is_empty());
        assert_eq!(
            view.security,
            TaskQuerySecurityScope::CurrentDecisionNotProvided
        );
        assert_eq!(
            view.recovery,
            TaskRecoveryHint::ReviewNode {
                node_id: NodeId::parse("read").unwrap()
            }
        );
    }
    db.assert_no_authority().await;
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn clarify_and_invalid_model_results_have_zero_database_writes_on_both_routes() {
    let db = Db::new().await;
    let (assembly, observed, audit) = support::assemble(Sessions::new(), None);
    let initial = db.snapshot().await;
    let id = new_task_id();
    for proposal in [clarify(), support::proposal(&new_task_id(), 2)] {
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            Some(Arc::new(MockLlm::new(proposal.clone()))),
            support::contracts(),
        )
        .unwrap();
        for result in [
            service
                .plan_and_create(&session(), &id, 2, 1, "query")
                .await,
            service
                .accept_json_and_create(&session(), &id, 2, 1, &raw(&proposal))
                .await,
        ] {
            if proposal.understanding.decision == UnderstandingDecision::Clarify {
                assert!(matches!(
                    result,
                    Ok(PlanningTaskCreationOutcome::Clarification(_))
                ));
            } else {
                assert!(result.is_err());
            }
        }
    }
    for mutation in 0..5 {
        let mut value = serde_json::to_value(support::proposal(&id, 2)).unwrap();
        match mutation {
            0 => value["plan"]["nodes"][0]["tool"] = json!("unknown_tool"),
            1 => value["plan"]["nodes"][0]["arguments"] = json!({"account_id":200}),
            2 => value["plan"]["plan_version"] = json!(1),
            3 => value["state"] = json!("COMPLETED"),
            _ => value["plan"]["nodes"][0]["arguments"]["account_id"] = json!(200.0),
        }
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            None,
            support::contracts(),
        )
        .unwrap();
        assert!(service
            .accept_json_and_create(&session(), &id, 2, 1, &serde_json::to_vec(&value).unwrap())
            .await
            .is_err());
    }
    assert_eq!(db.snapshot().await, initial);
    db.assert_no_authority().await;
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn existing_same_or_foreign_task_is_rejected_without_reset_or_extra_audit() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let (assembly, observed, _) = support::assemble(sessions.clone(), None);
    let id = new_task_id();
    let proposal = support::proposal(&id, 2);
    let raw = raw(&proposal);
    let service = PlanningTaskCreationService::for_trusted_backend(
        db.repo.clone(),
        assembly.runtime(),
        None,
        support::contracts(),
    )
    .unwrap();
    let saved = task(
        service
            .accept_json_and_create(&session(), &id, 2, 1, &raw)
            .await
            .unwrap(),
    );
    let original = db.snapshot().await;
    assert_eq!(
        error(
            service
                .accept_json_and_create(&session(), &id, 2, 1, &raw)
                .await
        ),
        PlanningTaskCreationError::Storage(StorageError::IdConflict)
    );
    assert_eq!(db.snapshot().await, original);
    sessions.change(Some("user_other"));
    assert_eq!(
        error(
            service
                .accept_json_and_create(&session(), &id, 2, 1, &raw)
                .await
        ),
        PlanningTaskCreationError::Storage(StorageError::NotFound)
    );
    assert_eq!(db.snapshot().await, original);
    assert_eq!(saved.revision, 1);
    observed.assert_intake_only();
    db.assert_no_authority().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn concurrent_duplicate_task_creates_one_initial_record_and_one_audit_only() {
    let db = Db::new().await;
    let (assembly, observed, _) = support::assemble(Sessions::new(), None);
    let id = new_task_id();
    let raw = raw(&support::proposal(&id, 2));
    let s = session();
    let service = PlanningTaskCreationService::for_trusted_backend(
        db.repo.clone(),
        assembly.runtime(),
        None,
        support::contracts(),
    )
    .unwrap();
    let (a, b) = tokio::join!(
        service.accept_json_and_create(&s, &id, 2, 1, &raw),
        service.accept_json_and_create(&s, &id, 2, 1, &raw)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let failure = if a.is_err() { error(a) } else { error(b) };
    assert_eq!(
        failure,
        PlanningTaskCreationError::Storage(StorageError::IdConflict)
    );
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM tasks),(SELECT count(*) FROM audit_events)")
            .fetch_one(&db.sql)
            .await
            .unwrap();
    assert_eq!(counts, (1, 1));
    observed.assert_intake_only();
    db.assert_no_authority().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn model_wait_rebinding_revocation_cross_user_or_registration_drift_cannot_create_task() {
    for mode in 0..4 {
        let db = Db::new().await;
        let sessions = Sessions::new();
        let schema = Arc::new(RwLock::new(None));
        let (assembly, observed, _) = support::assemble(sessions.clone(), Some(schema.clone()));
        let id = new_task_id();
        let gateway = Arc::new(WaitingGateway {
            mock: MockLlm::new(support::proposal(&id, 2)),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            Some(gateway.clone()),
            support::contracts(),
        )
        .unwrap();
        let s = session();
        let mut attempt = Box::pin(service.plan_and_create(&s, &id, 2, 1, "query"));
        tokio::time::timeout(Duration::from_secs(5),async { tokio::select! { _=gateway.entered.notified()=>{}, _=&mut attempt=>panic!("model did not wait") } }).await.unwrap();
        match mode {
            0 => sessions.change(None),
            1 => sessions.change(Some("user_001")),
            2 => sessions.change(Some("user_other")),
            _ => {
                *schema.write().unwrap() =
                    Some(json!({"type":"object","properties":{},"additionalProperties":false}))
            }
        }
        gateway.release.notify_one();
        assert!(attempt.await.is_err());
        assert_eq!(gateway.mock.call_count(), 1);
        assert_eq!(db.snapshot().await["tasks"], json!([]));
        assert_eq!(db.snapshot().await["audit"], json!([]));
        observed.assert_intake_only();
        db.assert_no_authority().await;
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn database_insert_and_audit_lock_wait_rechecks_initial_session_and_registration() {
    for table in ["tasks", "audit_events"] {
        for mode in 0..4 {
            let db = Db::new().await;
            let sessions = Sessions::new();
            let schema = Arc::new(RwLock::new(None));
            let (assembly, observed, _) = support::assemble(sessions.clone(), Some(schema.clone()));
            let service = PlanningTaskCreationService::for_trusted_backend(
                db.repo.clone(),
                assembly.runtime(),
                None,
                support::contracts(),
            )
            .unwrap();
            let id = new_task_id();
            let data = raw(&support::proposal(&id, 2));
            let s = session();
            let mut lock = db.sql.begin().await.unwrap();
            lock.execute(format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE").as_str())
                .await
                .unwrap();
            let (result, ()) = tokio::join!(
                service.accept_json_and_create(&s, &id, 2, 1, &data),
                async {
                    db.wait_lock(table).await;
                    match mode {
                        0 => sessions.change(None),
                        1 => sessions.change(Some("user_001")),
                        2 => sessions.change(Some("user_other")),
                        _ => {
                            *schema.write().unwrap() = Some(
                                json!({"type":"object","properties":{},"additionalProperties":false}),
                            )
                        }
                    }
                    lock.commit().await.unwrap();
                }
            );
            assert_eq!(
                error(result),
                PlanningTaskCreationError::Storage(if mode == 3 {
                    StorageError::Invalid("planning intake registration no longer current")
                } else {
                    StorageError::Session
                })
            );
            assert_eq!(db.snapshot().await["tasks"], json!([]));
            assert_eq!(db.snapshot().await["audit"], json!([]));
            observed.assert_intake_only();
            db.assert_no_authority().await;
            db.finish().await;
        }
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn database_connection_acquisition_wait_keeps_the_original_review() {
    for drift in [false, true] {
        let db = Db::new().await;
        let sessions = Sessions::new();
        let schema = Arc::new(RwLock::new(None));
        let (assembly, observed, _) = support::assemble(sessions.clone(), Some(schema.clone()));
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            None,
            support::contracts(),
        )
        .unwrap();
        let a = db.repo.pool.acquire().await.unwrap();
        let b = db.repo.pool.acquire().await.unwrap();
        let s = session();
        let id = new_task_id();
        let data = raw(&support::proposal(&id, 2));
        let mut attempt = Box::pin(service.accept_json_and_create(&s, &id, 2, 1, &data));
        tokio::select! { _=&mut attempt=>panic!("pool should be exhausted"), _=tokio::time::sleep(Duration::from_millis(50))=>{} }
        if drift {
            *schema.write().unwrap() =
                Some(json!({"type":"object","properties":{},"additionalProperties":false}));
        } else {
            sessions.change(Some("user_001"));
        }
        drop(a);
        drop(b);
        assert!(attempt.await.is_err());
        assert_eq!(db.snapshot().await["tasks"], json!([]));
        assert_eq!(db.snapshot().await["audit"], json!([]));
        observed.assert_intake_only();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn every_session_observation_through_final_commit_guard_rejects_or_reports_postcommit_uncertainty(
) {
    let db = Db::new().await;
    let probe = Sessions::new();
    let (assembly, _, _) = support::assemble(probe.clone(), None);
    let service = PlanningTaskCreationService::for_trusted_backend(
        db.repo.clone(),
        assembly.runtime(),
        None,
        support::contracts(),
    )
    .unwrap();
    let id = new_task_id();
    assert!(service
        .accept_json_and_create(&session(), &id, 2, 1, &raw(&support::proposal(&id, 2)))
        .await
        .is_ok());
    let observations = probe.calls.load(Ordering::SeqCst);
    assert!(observations >= 8);
    db.finish().await;
    for point in 1..=observations {
        let db = Db::new().await;
        let sessions = Sessions::new();
        let (assembly, observed, _) = support::assemble(sessions.clone(), None);
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            None,
            support::contracts(),
        )
        .unwrap();
        sessions.revoke_at.store(point, Ordering::SeqCst);
        let id = new_task_id();
        assert!(
            service
                .accept_json_and_create(&session(), &id, 2, 1, &raw(&support::proposal(&id, 2)))
                .await
                .is_err(),
            "point {point}"
        );
        let snapshot = db.snapshot().await;
        let tasks = snapshot["tasks"].as_array().unwrap().len();
        let audit = snapshot["audit"].as_array().unwrap().len();
        assert_eq!(tasks, audit);
        assert!(tasks <= 1);
        // Repository and service each check twice after COMMIT; all earlier checks precede it.
        if point < observations - 3 {
            assert_eq!(tasks, 0, "precommit point {point}");
        }
        observed.assert_intake_only();
        db.assert_no_authority().await;
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn sql_audit_and_deferred_commit_failures_roll_back_task_and_creation_audit() {
    for deferred in [false, true] {
        let db = Db::new().await;
        let (assembly, observed, _) = support::assemble(Sessions::new(), None);
        db.sql.execute("CREATE FUNCTION fail_creation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'private database failure'; END $$").await.unwrap();
        db.sql.execute(if deferred {"CREATE CONSTRAINT TRIGGER fail_creation AFTER INSERT ON tasks DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_creation()"}else{"CREATE TRIGGER fail_creation BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_creation()"}).await.unwrap();
        let service = PlanningTaskCreationService::for_trusted_backend(
            db.repo.clone(),
            assembly.runtime(),
            None,
            support::contracts(),
        )
        .unwrap();
        let id = new_task_id();
        assert_eq!(
            error(
                service
                    .accept_json_and_create(&session(), &id, 2, 1, &raw(&support::proposal(&id, 2)))
                    .await
            ),
            PlanningTaskCreationError::Storage(StorageError::Database)
        );
        assert_eq!(db.snapshot().await["tasks"], json!([]));
        assert_eq!(db.snapshot().await["audit"], json!([]));
        observed.assert_intake_only();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn post_audit_task_mutation_is_detected_before_commit_and_rolled_back() {
    let db = Db::new().await;
    let (assembly, observed, _) = support::assemble(Sessions::new(), None);
    db.sql.execute("CREATE FUNCTION change_initial() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE tasks SET revision=revision+1 WHERE task_id=NEW.task_id; RETURN NEW; END $$; CREATE TRIGGER change_initial AFTER INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION change_initial()").await.unwrap();
    let service = PlanningTaskCreationService::for_trusted_backend(
        db.repo.clone(),
        assembly.runtime(),
        None,
        support::contracts(),
    )
    .unwrap();
    let id = new_task_id();
    assert_eq!(
        error(
            service
                .accept_json_and_create(&session(), &id, 2, 1, &raw(&support::proposal(&id, 2)))
                .await
        ),
        PlanningTaskCreationError::Storage(StorageError::CorruptRecord)
    );
    assert_eq!(db.snapshot().await["tasks"], json!([]));
    assert_eq!(db.snapshot().await["audit"], json!([]));
    observed.assert_intake_only();
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn cancellation_during_database_wait_rolls_back_without_automatic_model_retry() {
    let db = Db::new().await;
    let (assembly, observed, _) = support::assemble(Sessions::new(), None);
    let id = new_task_id();
    let s = session();
    let mock = Arc::new(MockLlm::new(support::proposal(&id, 2)));
    let service = PlanningTaskCreationService::for_trusted_backend(
        db.repo.clone(),
        assembly.runtime(),
        Some(mock.clone()),
        support::contracts(),
    )
    .unwrap();
    let mut lock = db.sql.begin().await.unwrap();
    lock.execute("LOCK TABLE audit_events IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let mut attempt = Box::pin(service.plan_and_create(&s, &id, 2, 1, "query"));
    tokio::select! { _=&mut attempt=>panic!("expected audit SQL wait"), _=db.wait_lock("audit_events")=>{} }
    drop(attempt);
    lock.commit().await.unwrap();
    // Let SQLx finish the dropped transaction's rollback before checking another connection.
    let mut barrier = db.sql.begin().await.unwrap();
    barrier
        .execute("LOCK TABLE tasks IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    barrier.commit().await.unwrap();
    assert_eq!(db.snapshot().await["tasks"], json!([]));
    assert_eq!(db.snapshot().await["audit"], json!([]));
    assert_eq!(mock.call_count(), 1);
    observed.assert_intake_only();
    db.finish().await;
}
