//! Real PostgreSQL tests for foundation-owned authentication mechanics only.
use chrono::{DateTime, Utc};
use safeflow_contracts::{
    AgentPlan, ArgumentsHash, NodeId, OperationState, PlanNode, ReasonCode, SecurityError,
    SessionRef, ToolName, UserId,
};
use safeflow_runtime::{SessionBinding, SessionResolver};
use safeflow_storage::*;
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc, RwLock,
};
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
    fn get(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}
impl ConfirmationClock for Clock {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.get()).unwrap()
    }
}
fn config() -> SimulationAuthConfig {
    // Explicit isolated test settings, not product rules or a transfer policy.
    SimulationAuthConfig::new(Duration::from_secs(20), Duration::from_secs(10), 3, 32).unwrap()
}
struct Fixture {
    confirmation: ConfirmationRecord,
    challenge: SimulationChallengeRecord,
    proof: SimulationProof,
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
    confirmations: ConfirmationService,
    auth: SimulationAuthService,
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
            .expect("real PostgreSQL required; no checks skipped");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|_| panic!("isolated database connection failed"));
        let schema = format!("sf_auth_sim_{}", uuid::Uuid::new_v4().simple());
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
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
            .unwrap_or_else(|_| panic!("isolated schema connection failed"));
        let session = SessionRef::parse("isolated_auth_backend_A").unwrap();
        let same_user = SessionRef::parse("isolated_auth_backend_B").unwrap();
        let other = SessionRef::parse("isolated_auth_backend_C").unwrap();
        let sessions = Arc::new(Sessions::default());
        sessions.bind(&session, "user_001", 1);
        sessions.bind(&same_user, "user_001", 1);
        sessions.bind(&other, "user_002", 1);
        let clock = Arc::new(Clock(AtomicI64::new(1_791_172_000_000)));
        let storage = PersistenceService::for_trusted_backend(repo.clone(), sessions.clone());
        let confirmations = ConfirmationService::for_trusted_backend_with_clock(
            repo.clone(),
            sessions.clone(),
            ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
            clock.clone(),
        );
        let auth = Self::service(repo.clone(), sessions.clone(), clock.clone(), config());
        Self {
            url,
            schema,
            admin,
            sql,
            repo,
            sessions,
            clock,
            storage,
            confirmations,
            auth,
            session,
            same_user,
            other,
        }
    }
    fn service(
        repo: PgRepository,
        sessions: Arc<Sessions>,
        clock: Arc<Clock>,
        config: SimulationAuthConfig,
    ) -> SimulationAuthService {
        SimulationAuthService::for_isolated_backend_with_clock(
            repo,
            sessions,
            config,
            SimulationAuthMode::FoundationIsolatedV1,
            clock,
        )
        .unwrap()
    }
    async fn prepared(&self) -> ConfirmationRecord {
        let task = self.storage.create_task(&self.session, AgentPlan::new(new_task_id(), 1, vec![
            PlanNode::Tool { id: NodeId::parse("snapshot").unwrap(), tool: ToolName::parse("foundation_snapshot_only").unwrap(),
                arguments: serde_json::json!({"amount":"200.00","fixture":"foundation_snapshot_only"}), depends_on: vec![] }
        ]).unwrap(), 1).await.unwrap();
        let op = self
            .storage
            .create_operation(
                &self.session,
                &task.task_id,
                task.revision,
                new_operation_id(),
                NodeId::parse("snapshot").unwrap(),
                1,
            )
            .await
            .unwrap();
        self.confirmations
            .prepare(&self.session, &task.task_id, &op.operation_id)
            .await
            .unwrap()
    }
    async fn confirmed(&self) -> ConfirmationRecord {
        let c = self.prepared().await;
        self.confirmations
            .record_explicit_confirmation(
                &self.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding,
            )
            .await
            .unwrap()
    }
    async fn create(
        &self,
        c: &ConfirmationRecord,
    ) -> Result<SimulationChallengeDelivery, StorageError> {
        self.auth
            .create_challenge(
                &self.session,
                &c.binding.task_id,
                &c.binding.operation_id,
                &c.confirmation_ref,
            )
            .await
    }
    async fn fixture(&self) -> Fixture {
        let confirmation = self.confirmed().await;
        let mut delivery = self.create(&confirmation).await.unwrap();
        let secret = delivery.take_secret().unwrap();
        let proof = SimulationProof::from_material(secret.expose_for_isolated_backend()).unwrap();
        assert!(delivery.take_secret().is_none());
        Fixture {
            confirmation,
            challenge: delivery.record().clone(),
            proof,
        }
    }
    async fn submit(
        &self,
        f: &Fixture,
        id: &SimulationSubmissionId,
        proof: &SimulationProof,
    ) -> Result<SimulationSubmissionRecord, StorageError> {
        self.auth
            .submit(
                &self.session,
                &f.challenge.binding.task_id,
                &f.challenge.challenge_id,
                id,
                proof,
            )
            .await
    }
    async fn success(&self, f: &Fixture) -> SimulationAuthGrantRecord {
        self.submit(f, &SimulationSubmissionId::new_server_id(), &f.proof)
            .await
            .unwrap()
            .grant
            .unwrap()
    }
    async fn read(&self, f: &Fixture) -> Result<SimulationChallengeRecord, StorageError> {
        self.auth
            .read_challenge(
                &self.session,
                &f.challenge.binding.task_id,
                &f.challenge.challenge_id,
            )
            .await
    }
    async fn count(&self, table: &str) -> i64 {
        assert!(matches!(
            table,
            "simulation_auth_challenges" | "simulation_auth_grants" | "simulation_auth_submissions"
        ));
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
    async fn raw(&self, f: &Fixture) -> (String, i32) {
        sqlx::query_as(
            "SELECT status,failures FROM simulation_auth_challenges WHERE challenge_id=$1",
        )
        .bind(f.challenge.challenge_id.as_str())
        .fetch_one(&self.sql)
        .await
        .unwrap()
    }
    async fn finish(self) {
        self.repo.close().await;
        self.sql.close().await;
        assert!(self.schema.starts_with("sf_auth_sim_") && self.schema.len() == 44);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn wrong() -> SimulationProof {
    SimulationProof::from_material(&[0]).unwrap()
}

#[test]
fn explicit_configuration_has_finite_bounds_and_millisecond_precision() {
    for (a, b, c, d) in [
        (Duration::ZERO, Duration::from_secs(1), 3, 32),
        (Duration::from_nanos(1), Duration::from_secs(1), 3, 32),
        (Duration::MAX, Duration::from_secs(1), 3, 32),
        (Duration::from_secs(1), Duration::ZERO, 3, 32),
        (Duration::from_secs(1), Duration::from_secs(1), 0, 32),
        (Duration::from_secs(1), Duration::from_secs(1), 101, 32),
        (Duration::from_secs(1), Duration::from_secs(1), 3, 15),
        (Duration::from_secs(1), Duration::from_secs(1), 3, 65),
    ] {
        assert!(SimulationAuthConfig::new(a, b, c, d).is_err());
    }
    assert!(
        SimulationAuthConfig::new(Duration::from_millis(1), Duration::from_millis(1), 1, 16)
            .is_ok()
    );
}
#[test]
fn material_and_identifiers_reject_unbounded_or_noncanonical_input() {
    assert!(SimulationProof::from_material(&[]).is_err());
    assert!(SimulationProof::from_material(&[0; 65]).is_err());
    assert!(SimulationProof::from_material(&[0; 64]).is_ok());
    for id in [
        "",
        "sim_challenge_wrong",
        "sim_challenge_550e8400-e29b-41d4-a716-44665544000A",
        "550e8400-e29b-41d4-a716-446655440000",
    ] {
        assert!(SimulationChallengeId::parse(id).is_err());
    }
    let a = SimulationChallengeId::new_server_id();
    assert_eq!(SimulationChallengeId::parse(a.as_str()).unwrap(), a);
    assert_ne!(a, SimulationChallengeId::new_server_id());
    assert!(SimulationGrantId::parse(a.as_str()).is_err());
    assert!(SimulationSubmissionId::parse(a.as_str()).is_err());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn actual_verification_preserves_snapshot_and_cannot_issue_formal_permit() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let before = db
        .storage
        .recover(&db.session, &f.challenge.binding.task_id)
        .await
        .unwrap();
    let grant = db.success(&f).await;
    assert_eq!(
        db.auth
            .verify_grant(
                &db.session,
                &f.challenge.binding.task_id,
                &grant.grant_id,
                &grant.binding
            )
            .await
            .unwrap(),
        grant
    );
    let after = db
        .storage
        .recover(&db.session, &f.challenge.binding.task_id)
        .await
        .unwrap();
    assert_eq!(before.task, after.task);
    assert_eq!(before.operations, after.operations);
    assert_eq!(after.operations[0].state, OperationState::Draft);
    assert_eq!(
        db.auth
            .request_formal_auth(&db.session, &grant.binding.task_id)
            .await,
        Err(StorageError::FormalAuthUnavailable)
    );
    let permits = PermitStore::for_trusted_backend(
        db.repo.clone(),
        db.sessions.clone(),
        PermitConfig::new(Duration::from_secs(10)).unwrap(),
    );
    assert_eq!(
        permits
            .request_issuance(&db.session, &grant.binding.task_id)
            .await
            .err(),
        Some(StorageError::PermitIssuanceUnavailable)
    );
    assert_eq!(db.audits("SIM_AUTH_VERIFIED").await, 1);
    assert_eq!(db.audits("SIM_AUTH_GRANT_ISSUED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn secret_export_is_once_and_create_replay_never_extends_or_resets() {
    let db = Db::new().await;
    let f = db.fixture().await;
    db.submit(&f, &SimulationSubmissionId::new_server_id(), &wrong())
        .await
        .unwrap();
    db.clock.set(db.clock.get() + 1000);
    let mut retry = db.create(&f.confirmation).await.unwrap();
    assert!(retry.take_secret().is_none());
    assert_eq!(retry.record().failures, 1);
    assert_eq!(retry.record().challenge_id, f.challenge.challenge_id);
    assert_eq!(retry.record().expires_at_ms, f.challenge.expires_at_ms);
    assert_eq!(db.count("simulation_auth_challenges").await, 1);
    assert_eq!(db.audits("SIM_AUTH_CHALLENGE_CREATED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn failure_and_success_replay_have_exactly_once_effects() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let failed_id = SimulationSubmissionId::new_server_id();
    let failed = db.submit(&f, &failed_id, &wrong()).await.unwrap();
    assert_eq!(failed.result, SimulationVerificationResult::NotVerified);
    assert_eq!(failed.failures, 1);
    db.clock.set(db.clock.get() + 1000);
    assert_eq!(db.submit(&f, &failed_id, &wrong()).await.unwrap(), failed);
    assert_eq!(
        db.submit(&f, &failed_id, &f.proof).await,
        Err(StorageError::AuthSubmissionConflict)
    );
    let id = SimulationSubmissionId::new_server_id();
    let success = db.submit(&f, &id, &f.proof).await.unwrap();
    db.clock.set(db.clock.get() + 1000);
    assert_eq!(db.submit(&f, &id, &f.proof).await.unwrap(), success);
    assert_eq!(db.submit(&f, &failed_id, &wrong()).await.unwrap(), failed);
    assert_eq!(db.count("simulation_auth_grants").await, 1);
    assert_eq!(db.count("simulation_auth_submissions").await, 2);
    assert_eq!(db.audits("SIM_AUTH_FAILED").await, 1);
    assert_eq!(db.audits("SIM_AUTH_GRANT_ISSUED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn same_submission_id_cannot_be_reused_for_another_challenge() {
    let db = Db::new().await;
    let a = db.fixture().await;
    let b = db.fixture().await;
    let id = SimulationSubmissionId::new_server_id();
    db.submit(&a, &id, &wrong()).await.unwrap();
    assert_eq!(
        db.submit(&b, &id, &b.proof).await,
        Err(StorageError::AuthSubmissionConflict)
    );
    assert_eq!(db.raw(&b).await, ("PENDING".into(), 0));
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn failed_terminal_cannot_be_revived_by_late_success_or_new_challenge() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let id = SimulationSubmissionId::new_server_id();
    for i in 1..=3 {
        let request = if i == 3 {
            id.clone()
        } else {
            SimulationSubmissionId::new_server_id()
        };
        assert_eq!(db.submit(&f, &request, &wrong()).await.unwrap().failures, i);
    }
    assert_eq!(
        db.read(&f).await.unwrap().status,
        SimulationChallengeStatus::Failed
    );
    assert_eq!(db.submit(&f, &id, &wrong()).await.unwrap().failures, 3);
    assert_eq!(
        db.submit(&f, &SimulationSubmissionId::new_server_id(), &f.proof)
            .await,
        Err(StorageError::AuthChallengeInactive)
    );
    assert_eq!(
        db.create(&f.confirmation).await.err(),
        Some(StorageError::AuthChallengeInactive)
    );
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn changed_configuration_or_confirmation_cannot_reset_attempt_budget() {
    let db = Db::new().await;
    let f = db.fixture().await;
    db.submit(&f, &SimulationSubmissionId::new_server_id(), &wrong())
        .await
        .unwrap();
    let configs = [
        SimulationAuthConfig::new(Duration::from_secs(21), Duration::from_secs(10), 3, 32).unwrap(),
        SimulationAuthConfig::new(Duration::from_secs(20), Duration::from_secs(11), 3, 32).unwrap(),
        SimulationAuthConfig::new(Duration::from_secs(20), Duration::from_secs(10), 4, 32).unwrap(),
        SimulationAuthConfig::new(Duration::from_secs(20), Duration::from_secs(10), 3, 33).unwrap(),
    ];
    for config in configs {
        let service = Db::service(
            db.repo.clone(),
            db.sessions.clone(),
            db.clock.clone(),
            config,
        );
        assert_eq!(
            service
                .create_challenge(
                    &db.session,
                    &f.challenge.binding.task_id,
                    &f.challenge.binding.operation_id,
                    &f.confirmation.confirmation_ref
                )
                .await
                .err(),
            Some(StorageError::AuthChallengeConflict)
        );
    }
    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE task_id=$1")
        .bind(f.challenge.binding.task_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    let changed = db
        .confirmations
        .prepare(
            &db.session,
            &f.challenge.binding.task_id,
            &f.challenge.binding.operation_id,
        )
        .await
        .unwrap();
    db.confirmations
        .record_explicit_confirmation(
            &db.session,
            &changed.binding.task_id,
            &changed.confirmation_ref,
            &changed.binding,
        )
        .await
        .unwrap();
    assert_eq!(
        db.create(&changed).await.err(),
        Some(StorageError::AuthChallengeConflict)
    );
    assert_eq!(db.raw(&f).await, ("PENDING".into(), 1));
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_same_submission_returns_one_identical_grant() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let id = SimulationSubmissionId::new_server_id();
    let mut jobs = vec![];
    for _ in 0..6 {
        let (auth, s, t, c, id) = (
            db.auth.clone(),
            db.session.clone(),
            f.challenge.binding.task_id.clone(),
            f.challenge.challenge_id.clone(),
            id.clone(),
        );
        jobs.push((auth, s, t, c, id));
    }
    let shared = Arc::new(f.proof);
    let mut handles = vec![];
    for (auth, s, t, c, id) in jobs {
        let proof = shared.clone();
        handles.push(tokio::spawn(async move {
            auth.submit(&s, &t, &c, &id, &proof).await.unwrap()
        }));
    }
    let mut first = None;
    for handle in handles {
        let result = handle.await.unwrap();
        if let Some(ref value) = first {
            assert_eq!(value, &result);
        } else {
            first = Some(result);
        }
    }
    assert_eq!(db.count("simulation_auth_grants").await, 1);
    assert_eq!(db.count("simulation_auth_submissions").await, 1);
    assert_eq!(db.audits("SIM_AUTH_GRANT_ISSUED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_different_success_requests_do_not_issue_second_grant() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let proof = Arc::new(f.proof);
    let mut handles = vec![];
    for _ in 0..6 {
        let (a, s, t, c, p) = (
            db.auth.clone(),
            db.session.clone(),
            f.challenge.binding.task_id.clone(),
            f.challenge.challenge_id.clone(),
            proof.clone(),
        );
        handles.push(tokio::spawn(async move {
            a.submit(&s, &t, &c, &SimulationSubmissionId::new_server_id(), &p)
                .await
        }));
    }
    let mut successes = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => successes += 1,
            Err(e) => assert_eq!(e, StorageError::AuthChallengeInactive),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(db.count("simulation_auth_grants").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn new_connection_reads_and_verifies_then_observes_persisted_revocation() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let grant = db.success(&f).await;
    db.repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    repo.migrate().await.unwrap();
    let a = Db::service(
        repo.clone(),
        db.sessions.clone(),
        db.clock.clone(),
        config(),
    );
    assert_eq!(
        a.verify_grant(
            &db.session,
            &grant.binding.task_id,
            &grant.grant_id,
            &grant.binding
        )
        .await
        .unwrap(),
        grant
    );
    let revoked = a
        .revoke_grant(&db.session, &grant.binding.task_id, &grant.grant_id)
        .await
        .unwrap();
    assert_eq!(
        a.revoke_grant(&db.session, &grant.binding.task_id, &grant.grant_id)
            .await
            .unwrap(),
        revoked
    );
    repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    let a = Db::service(
        repo.clone(),
        db.sessions.clone(),
        db.clock.clone(),
        config(),
    );
    assert_eq!(
        a.read_grant(&db.session, &grant.binding.task_id, &grant.grant_id)
            .await,
        Err(StorageError::AuthGrantRevoked)
    );
    assert_eq!(db.audits("SIM_AUTH_GRANT_REVOKED").await, 1);
    repo.close().await;
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cross_user_cross_session_rebind_and_revocation_reject_all_capability_paths() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let grant = db.success(&f).await;
    for s in [&db.same_user, &db.other] {
        assert_eq!(
            db.auth
                .read_challenge(s, &grant.binding.task_id, &f.challenge.challenge_id)
                .await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.auth
                .submit(
                    s,
                    &grant.binding.task_id,
                    &f.challenge.challenge_id,
                    &SimulationSubmissionId::new_server_id(),
                    &f.proof
                )
                .await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.auth
                .read_grant(s, &grant.binding.task_id, &grant.grant_id)
                .await,
            Err(StorageError::NotFound)
        );
        assert_eq!(
            db.auth
                .revoke_challenge(s, &grant.binding.task_id, &f.challenge.challenge_id)
                .await,
            Err(StorageError::NotFound)
        );
    }
    db.sessions.bind(&db.session, "user_001", 2);
    assert_eq!(db.read(&f).await, Err(StorageError::NotFound));
    db.sessions.bind(&db.session, "user_002", 3);
    assert_eq!(db.read(&f).await, Err(StorageError::NotFound));
    db.sessions.revoke(&db.session);
    assert_eq!(db.read(&f).await, Err(StorageError::Session));
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn legacy_unversioned_resolver_and_nonisolated_namespace_fail_closed() {
    struct Legacy;
    impl SessionResolver for Legacy {
        fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
            Ok(UserId::parse("user_001").unwrap())
        }
    }
    let db = Db::new().await;
    let f = db.fixture().await;
    let a = SimulationAuthService::for_isolated_backend_with_clock(
        db.repo.clone(),
        Arc::new(Legacy),
        config(),
        SimulationAuthMode::FoundationIsolatedV1,
        db.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        a.read_challenge(
            &db.session,
            &f.challenge.binding.task_id,
            &f.challenge.challenge_id
        )
        .await,
        Err(StorageError::Session)
    );
    let name = format!("sf_auth_other_{}", uuid::Uuid::new_v4().simple());
    let repo = PgRepository::connect(&db.url, &name).await.unwrap();
    assert_eq!(
        SimulationAuthService::for_isolated_backend(
            repo.clone(),
            db.sessions.clone(),
            config(),
            SimulationAuthMode::FoundationIsolatedV1
        )
        .err(),
        Some(StorageError::SimulationModeRequired)
    );
    repo.close().await;
    db.admin
        .execute(format!("DROP SCHEMA {name} CASCADE").as_str())
        .await
        .unwrap();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn prepared_revoked_expired_and_wrong_confirmation_never_create_challenges() {
    let db = Db::new().await;
    let c = db.prepared().await;
    assert_eq!(
        db.create(&c).await.err(),
        Some(StorageError::ConfirmationRequired)
    );
    db.confirmations
        .record_explicit_confirmation(
            &db.session,
            &c.binding.task_id,
            &c.confirmation_ref,
            &c.binding,
        )
        .await
        .unwrap();
    let other = db.confirmed().await;
    assert_eq!(
        db.auth
            .create_challenge(
                &db.session,
                &c.binding.task_id,
                &c.binding.operation_id,
                &other.confirmation_ref
            )
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    db.confirmations
        .revoke(&db.session, &c.binding.task_id, &c.confirmation_ref)
        .await
        .unwrap();
    assert_eq!(
        db.create(&c).await.err(),
        Some(StorageError::ConfirmationRevoked)
    );
    db.clock.set(other.expires_at.timestamp_millis());
    assert_eq!(
        db.create(&other).await.err(),
        Some(StorageError::ConfirmationExpired)
    );
    assert_eq!(db.count("simulation_auth_challenges").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn confirmation_revocation_invalidates_existing_challenge_and_grant() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let grant = db.success(&f).await;
    db.confirmations
        .revoke(&db.session, &grant.binding.task_id, &grant.confirmation_ref)
        .await
        .unwrap();
    assert_eq!(db.read(&f).await, Err(StorageError::ConfirmationRevoked));
    assert_eq!(
        db.auth
            .read_grant(&db.session, &grant.binding.task_id, &grant.grant_id)
            .await,
        Err(StorageError::ConfirmationRevoked)
    );
    // Authorized cleanup remains possible; it does not authorize a new attempt.
    db.auth
        .revoke_challenge(
            &db.session,
            &grant.binding.task_id,
            &f.challenge.challenge_id,
        )
        .await
        .unwrap();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn expected_binding_is_comparison_only_every_field_must_match() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let grant = db.success(&f).await;
    for i in 0..10 {
        let mut b = grant.binding.clone();
        match i {
            0 => b.user_id = UserId::parse("other").unwrap(),
            1 => b.task_id = new_task_id(),
            2 => b.operation_id = new_operation_id(),
            3 => b.node_id = NodeId::parse("other").unwrap(),
            4 => b.tool_name = ToolName::parse("other").unwrap(),
            5 => b.task_revision += 1,
            6 => b.plan_version += 1,
            7 => b.operation_revision += 1,
            8 => b.facts_version += 1,
            _ => b.arguments_hash = ArgumentsHash::parse("0".repeat(64)).unwrap(),
        }
        assert_eq!(
            db.auth
                .verify_grant(&db.session, &grant.binding.task_id, &grant.grant_id, &b)
                .await,
            Err(StorageError::AuthBindingMismatch)
        );
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn persisted_task_operation_arguments_facts_node_and_tool_changes_invalidate_auth() {
    let db = Db::new().await;
    for mutation in [
        "UPDATE tasks SET revision=revision+1",
        "UPDATE tasks SET facts_version=facts_version+1",
        "UPDATE tasks SET plan_version=plan_version+1,plan=jsonb_set(plan,'{plan_version}',to_jsonb(plan_version+1))",
        "UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments,amount}','\"300.00\"')",
        "UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,tool}','\"other_tool\"')",
        "UPDATE operations SET revision=revision+1",
        "UPDATE operations SET facts_version=facts_version+1",
        "UPDATE operations SET node_id='other_node'",
        "UPDATE operations SET arguments_hash=repeat('0',64)",
    ] {let f=db.fixture().await;let g=db.success(&f).await;
        let filter=if mutation.starts_with("UPDATE tasks") {"task_id"} else {"operation_id"};
        let id=if filter=="task_id" {f.challenge.binding.task_id.as_str()} else {f.challenge.binding.operation_id.as_str()};
        sqlx::query(&format!("{mutation} WHERE {filter}=$1")).bind(id).execute(&db.sql).await.unwrap();
        assert!(db.read(&f).await.is_err(),"{mutation}");
        assert!(db.auth.read_grant(&db.session,&g.binding.task_id,&g.grant_id).await.is_err(),"{mutation}");
        assert!(db.submit(&f,&SimulationSubmissionId::new_server_id(),&f.proof).await.is_err(),"{mutation}");
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn submitted_processing_terminal_operation_and_task_are_not_authorization() {
    let db = Db::new().await;
    for state in [
        "SUBMITTED",
        "PROCESSING",
        "SUCCEEDED",
        "FAILED",
        "CANCELLED",
    ] {
        let f = db.fixture().await;
        sqlx::query("UPDATE operations SET state=$1,business_operation_ref='foundation_state_test' WHERE operation_id=$2").bind(state).bind(f.challenge.binding.operation_id.as_str()).execute(&db.sql).await.unwrap();
        assert!(db
            .submit(&f, &SimulationSubmissionId::new_server_id(), &f.proof)
            .await
            .is_err());
    }
    for state in ["WAITING_EXTERNAL", "COMPLETED", "FAILED", "CANCELLED"] {
        let f = db.fixture().await;
        sqlx::query("UPDATE tasks SET state=$1 WHERE task_id=$2")
            .bind(state)
            .bind(f.challenge.binding.task_id.as_str())
            .execute(&db.sql)
            .await
            .unwrap();
        assert!(db.read(&f).await.is_err());
        assert!(db
            .submit(&f, &SimulationSubmissionId::new_server_id(), &f.proof)
            .await
            .is_err());
    }
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn revoked_challenge_is_idempotent_and_late_success_is_rejected() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let revoked = db
        .auth
        .revoke_challenge(
            &db.session,
            &f.challenge.binding.task_id,
            &f.challenge.challenge_id,
        )
        .await
        .unwrap();
    db.clock.set(db.clock.get() + 1000);
    assert_eq!(
        db.auth
            .revoke_challenge(
                &db.session,
                &f.challenge.binding.task_id,
                &f.challenge.challenge_id
            )
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(
        db.submit(&f, &SimulationSubmissionId::new_server_id(), &f.proof)
            .await,
        Err(StorageError::AuthChallengeInactive)
    );
    assert_eq!(
        db.create(&f.confirmation).await.err(),
        Some(StorageError::AuthChallengeInactive)
    );
    assert_eq!(db.audits("SIM_AUTH_CHALLENGE_REVOKED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn observed_challenge_expiry_survives_clock_rollback_and_recreation() {
    let db = Db::new().await;
    let f = db.fixture().await;
    db.clock.set(f.challenge.expires_at_ms);
    assert_eq!(db.read(&f).await, Err(StorageError::AuthExpired));
    db.repo.close().await;
    let repo = PgRepository::connect(&db.url, &db.schema).await.unwrap();
    db.clock.set(f.challenge.created_at_ms + 1000);
    let a = Db::service(
        repo.clone(),
        db.sessions.clone(),
        db.clock.clone(),
        config(),
    );
    assert!(matches!(
        a.submit(
            &db.session,
            &f.challenge.binding.task_id,
            &f.challenge.challenge_id,
            &SimulationSubmissionId::new_server_id(),
            &f.proof
        )
        .await,
        Err(StorageError::Invalid(_))
    ));
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    repo.close().await;
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn observed_grant_expiry_blocks_replay_after_clock_rollback() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let id = SimulationSubmissionId::new_server_id();
    let g = db.submit(&f, &id, &f.proof).await.unwrap().grant.unwrap();
    db.clock.set(g.expires_at_ms);
    assert_eq!(
        db.auth
            .read_grant(&db.session, &g.binding.task_id, &g.grant_id)
            .await,
        Err(StorageError::AuthExpired)
    );
    db.clock.set(g.issued_at_ms + 1000);
    let a = Db::service(
        db.repo.clone(),
        db.sessions.clone(),
        db.clock.clone(),
        config(),
    );
    assert!(matches!(
        a.read_grant(&db.session, &g.binding.task_id, &g.grant_id)
            .await,
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        db.submit(&f, &id, &f.proof).await,
        Err(StorageError::Invalid(_))
    ));
    assert_eq!(db.count("simulation_auth_grants").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn failed_submission_and_successful_read_time_survive_recreation() {
    let db = Db::new().await;
    for read_only in [false, true] {
        let f = db.fixture().await;
        let later = db.clock.get() + 5000;
        db.clock.set(later);
        if read_only {
            db.read(&f).await.unwrap();
        } else {
            db.submit(&f, &SimulationSubmissionId::new_server_id(), &wrong())
                .await
                .unwrap();
        }
        db.clock.set(later - 4000);
        let a = Db::service(
            db.repo.clone(),
            db.sessions.clone(),
            db.clock.clone(),
            config(),
        );
        assert!(matches!(
            a.submit(
                &db.session,
                &f.challenge.binding.task_id,
                &f.challenge.challenge_id,
                &SimulationSubmissionId::new_server_id(),
                &f.proof
            )
            .await,
            Err(StorageError::Invalid(_))
        ));
        db.clock.set(later);
        assert_eq!(db.success(&f).await.issued_at_ms, later);
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn persistent_clock_fence_failure_cannot_issue_or_count_an_attempt() {
    let db = Db::new().await;
    let f = db.fixture().await;
    fault(&db, "simulation_auth_time_fence", false).await;
    assert_eq!(
        db.submit(&f, &SimulationSubmissionId::new_server_id(), &f.proof)
            .await,
        Err(StorageError::Database)
    );
    assert_eq!(db.raw(&f).await, ("PENDING".into(), 0));
    assert_eq!(db.count("simulation_auth_submissions").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn persisted_data_and_ordinary_returns_contain_no_raw_secret_or_session() {
    let db = Db::new().await;
    let c = db.confirmed().await;
    let mut delivery = db.create(&c).await.unwrap();
    let secret = delivery.take_secret().unwrap();
    let hex: String = secret
        .expose_for_isolated_backend()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let proof = SimulationProof::from_material(secret.expose_for_isolated_backend()).unwrap();
    let r = delivery.record().clone();
    let submit = db
        .auth
        .submit(
            &db.session,
            &r.binding.task_id,
            &r.challenge_id,
            &SimulationSubmissionId::new_server_id(),
            &proof,
        )
        .await
        .unwrap();
    let snapshot: String =
        sqlx::query_scalar("SELECT row_to_json(c)::text FROM simulation_auth_challenges c")
            .fetch_one(&db.sql)
            .await
            .unwrap();
    let rendered = format!("{} {submit:?}", serde_json::to_string(&submit).unwrap());
    for value in [&snapshot, &rendered] {
        assert!(!value.contains(&hex));
        assert!(!value.contains(db.session.as_str()));
    }
    assert!(!rendered.contains("secret_digest"));
    assert!(!rendered.contains("session_digest"));
    db.finish().await;
}

async fn fault(db: &Db, table: &str, deferred: bool) {
    assert!(matches!(
        table,
        "audit_events"
            | "simulation_auth_challenges"
            | "simulation_auth_grants"
            | "simulation_auth_submissions"
            | "simulation_auth_time_fence"
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
    let c = db.confirmed().await;
    if table != "simulation_auth_grants" && table != "simulation_auth_submissions" {
        fault(&db, table, deferred).await;
        assert_eq!(db.create(&c).await.err(), Some(StorageError::Database));
        assert_eq!(db.count("simulation_auth_challenges").await, 0);
        assert_eq!(db.audits("SIM_AUTH_CHALLENGE_CREATED").await, 0);
        clear_fault(&db, table).await;
    }
    let mut d = db.create(&c).await.unwrap();
    let proof =
        SimulationProof::from_material(d.take_secret().unwrap().expose_for_isolated_backend())
            .unwrap();
    let f = Fixture {
        confirmation: c,
        challenge: d.record().clone(),
        proof,
    };
    for passed in [false, true] {
        if !passed && table == "simulation_auth_grants" {
            continue;
        }
        fault(&db, table, deferred).await;
        let bad = wrong();
        assert_eq!(
            db.submit(
                &f,
                &SimulationSubmissionId::new_server_id(),
                if passed { &f.proof } else { &bad }
            )
            .await,
            Err(StorageError::Database)
        );
        assert_eq!(db.raw(&f).await, ("PENDING".into(), 0));
        assert_eq!(db.count("simulation_auth_submissions").await, 0);
        assert_eq!(db.count("simulation_auth_grants").await, 0);
        assert_eq!(db.audits("SIM_AUTH_FAILED").await, 0);
        assert_eq!(db.audits("SIM_AUTH_VERIFIED").await, 0);
        clear_fault(&db, table).await;
    }
    let grant = db.success(&f).await;
    if table != "simulation_auth_submissions" {
        fault(&db, table, deferred).await;
        assert_eq!(
            db.auth
                .revoke_grant(&db.session, &grant.binding.task_id, &grant.grant_id)
                .await,
            Err(StorageError::Database)
        );
        assert_eq!(db.raw(&f).await, ("SUCCEEDED".into(), 0));
        assert_eq!(db.audits("SIM_AUTH_GRANT_REVOKED").await, 0);
        clear_fault(&db, table).await;
        assert_eq!(
            db.auth
                .read_grant(&db.session, &grant.binding.task_id, &grant.grant_id)
                .await
                .unwrap(),
            grant
        );
    }
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn immediate_audit_failure_rolls_back_creation_failure_success_and_revocation() {
    atomic_failures("audit_events", false).await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn deferred_commit_audit_failure_rolls_back_all_auth_effects() {
    atomic_failures("audit_events", true).await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn challenge_grant_and_submission_write_failures_cannot_report_success() {
    for table in [
        "simulation_auth_challenges",
        "simulation_auth_grants",
        "simulation_auth_submissions",
    ] {
        atomic_failures(table, false).await;
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
    panic!("no database lock wait observed");
}
async fn audit_blocker(db: &Db, kind: &str) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut tx = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(987654323)")
        .execute(&mut *tx)
        .await
        .unwrap();
    db.sql.execute("CREATE FUNCTION wait_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(987654323); RETURN NEW; END $$").await.unwrap();
    db.sql.execute(format!("CREATE TRIGGER wait_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='{kind}') EXECUTE FUNCTION wait_audit()").as_str()).await.unwrap();
    (tx, pid)
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn task_lock_wait_rechecks_session_expiry_and_current_database_snapshot() {
    for mode in 0..4 {
        let db = Db::new().await;
        let f = db.fixture().await;
        let mut lock = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(f.challenge.binding.task_id.as_str())
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let (a, s, t, c, p) = (
            db.auth.clone(),
            db.session.clone(),
            f.challenge.binding.task_id.clone(),
            f.challenge.challenge_id.clone(),
            f.proof,
        );
        let job = tokio::spawn(async move {
            a.submit(&s, &t, &c, &SimulationSubmissionId::new_server_id(), &p)
                .await
        });
        wait_blocked(&db, pid).await;
        match mode {
            0 => db.sessions.revoke(&db.session),
            1 => db.sessions.bind(&db.session, "user_001", 2),
            2 => db.clock.set(f.challenge.expires_at_ms),
            _ => {
                sqlx::query("UPDATE tasks SET revision=revision+1 WHERE task_id=$1")
                    .bind(f.challenge.binding.task_id.as_str())
                    .execute(&mut *lock)
                    .await
                    .unwrap();
            }
        }
        lock.commit().await.unwrap();
        assert!(job.await.unwrap().is_err());
        assert_eq!(db.count("simulation_auth_grants").await, 0);
        assert_eq!(db.count("simulation_auth_submissions").await, 0);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn audit_wait_rechecks_identity_expiry_and_rollback_before_commit() {
    for mode in 0..4 {
        let db = Db::new().await;
        let f = db.fixture().await;
        let (blocker, pid) = audit_blocker(&db, "SIM_AUTH_VERIFIED").await;
        let (a, s, t, c, p) = (
            db.auth.clone(),
            db.session.clone(),
            f.challenge.binding.task_id.clone(),
            f.challenge.challenge_id.clone(),
            f.proof,
        );
        let job = tokio::spawn(async move {
            a.submit(&s, &t, &c, &SimulationSubmissionId::new_server_id(), &p)
                .await
        });
        wait_blocked(&db, pid).await;
        match mode {
            0 => db.sessions.revoke(&db.session),
            1 => db.clock.set(f.challenge.expires_at_ms),
            2 => db.clock.set(f.challenge.created_at_ms - 1),
            _ => db.clock.set(f.challenge.created_at_ms + 10_000),
        }
        blocker.commit().await.unwrap();
        assert!(job.await.unwrap().is_err());
        assert_eq!(db.count("simulation_auth_grants").await, 0);
        assert_eq!(db.count("simulation_auth_submissions").await, 0);
        assert_eq!(db.audits("SIM_AUTH_VERIFIED").await, 0);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn cancelled_create_submit_and_revoke_transactions_leave_no_partial_authorization() {
    for kind in [
        "SIM_AUTH_CHALLENGE_CREATED",
        "SIM_AUTH_VERIFIED",
        "SIM_AUTH_FAILED",
        "SIM_AUTH_GRANT_REVOKED",
    ] {
        let db = Db::new().await;
        let c = db.confirmed().await;
        let f = if kind == "SIM_AUTH_CHALLENGE_CREATED" {
            None
        } else {
            let mut d = db.create(&c).await.unwrap();
            Some(Fixture {
                confirmation: c.clone(),
                challenge: d.record().clone(),
                proof: SimulationProof::from_material(
                    d.take_secret().unwrap().expose_for_isolated_backend(),
                )
                .unwrap(),
            })
        };
        let grant = if kind == "SIM_AUTH_GRANT_REVOKED" {
            Some(db.success(f.as_ref().unwrap()).await)
        } else {
            None
        };
        let (blocker, pid) = audit_blocker(&db, kind).await;
        let (a, s) = (db.auth.clone(), db.session.clone());
        let request = c.clone();
        let job = tokio::spawn(async move {
            match kind {
                "SIM_AUTH_CHALLENGE_CREATED" => a
                    .create_challenge(
                        &s,
                        &request.binding.task_id,
                        &request.binding.operation_id,
                        &request.confirmation_ref,
                    )
                    .await
                    .map(|_| ()),
                "SIM_AUTH_GRANT_REVOKED" => {
                    let g = grant.unwrap();
                    a.revoke_grant(&s, &g.binding.task_id, &g.grant_id)
                        .await
                        .map(|_| ())
                }
                _ => {
                    let f = f.unwrap();
                    let bad = wrong();
                    a.submit(
                        &s,
                        &f.challenge.binding.task_id,
                        &f.challenge.challenge_id,
                        &SimulationSubmissionId::new_server_id(),
                        if kind == "SIM_AUTH_FAILED" {
                            &bad
                        } else {
                            &f.proof
                        },
                    )
                    .await
                    .map(|_| ())
                }
            }
        });
        wait_blocked(&db, pid).await;
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        blocker.commit().await.unwrap();
        // Wait for SQLx's asynchronous rollback by acquiring the original task lock.
        let mut tx = db.sql.begin().await.unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(c.binding.task_id.as_str())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(db.audits(kind).await, 0);
        assert_eq!(
            db.count("simulation_auth_submissions").await,
            if kind == "SIM_AUTH_GRANT_REVOKED" {
                1
            } else {
                0
            }
        );
        assert_eq!(
            db.count("simulation_auth_grants").await,
            if kind == "SIM_AUTH_GRANT_REVOKED" {
                1
            } else {
                0
            }
        );
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM simulation_auth_challenges")
                .fetch_optional(&db.sql)
                .await
                .unwrap();
        assert_eq!(
            status,
            match kind {
                "SIM_AUTH_CHALLENGE_CREATED" => None,
                "SIM_AUTH_GRANT_REVOKED" => Some("SUCCEEDED".into()),
                _ => Some("PENDING".into()),
            }
        );
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn audit_trigger_changes_to_task_operation_confirmation_or_attempt_roll_back() {
    for mutation in [
        "UPDATE tasks SET revision=revision+1,plan=jsonb_set(plan,'{nodes,0,arguments,amount}','\"300.00\"') WHERE task_id=NEW.task_id;",
        "UPDATE operations SET revision=revision+1 WHERE operation_id=NEW.operation_id;",
        "UPDATE operations SET facts_version=facts_version+1 WHERE operation_id=NEW.operation_id;",
        "UPDATE operations SET arguments_hash=repeat('0',64) WHERE operation_id=NEW.operation_id;",
        "UPDATE server_confirmations SET status='REVOKED',revoked_at=clock_timestamp() WHERE task_id=NEW.task_id;",
        "UPDATE simulation_auth_submissions SET failures=failures+1;",
        "UPDATE simulation_auth_grants SET expires_at_ms=expires_at_ms+1;",
    ] {let db=Db::new().await;let f=db.fixture().await;
        let before=db.storage.recover(&db.session,&f.challenge.binding.task_id).await.unwrap();
        db.sql.execute(format!("CREATE FUNCTION mutate_snapshot() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN {mutation} RETURN NEW; END $$").as_str()).await.unwrap();
        db.sql.execute("CREATE TRIGGER mutate_snapshot BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='SIM_AUTH_VERIFIED') EXECUTE FUNCTION mutate_snapshot()").await.unwrap();
        assert!(db.submit(&f,&SimulationSubmissionId::new_server_id(),&f.proof).await.is_err(),"{mutation}");
        assert_eq!(db.storage.recover(&db.session,&f.challenge.binding.task_id).await.unwrap(),before);
        assert_eq!(db.raw(&f).await,("PENDING".into(),0));assert_eq!(db.count("simulation_auth_grants").await,0);assert_eq!(db.count("simulation_auth_submissions").await,0);db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn clock_fence_is_sampled_after_its_lock_wait_and_rejects_expiry() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let mut lock = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT scope FROM simulation_auth_time_fence WHERE scope=1 FOR UPDATE")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let (a, s, t, c, p) = (
        db.auth.clone(),
        db.session.clone(),
        f.challenge.binding.task_id.clone(),
        f.challenge.challenge_id.clone(),
        f.proof,
    );
    let job = tokio::spawn(async move {
        a.submit(&s, &t, &c, &SimulationSubmissionId::new_server_id(), &p)
            .await
    });
    wait_blocked(&db, pid).await;
    db.clock.set(f.challenge.expires_at_ms);
    lock.commit().await.unwrap();
    assert_eq!(job.await.unwrap(), Err(StorageError::AuthExpired));
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn migration_five_preserves_old_checksums_confirmations_permits_and_tasks() {
    let db = Db::unmigrated().await;
    let sql = [
        include_str!("../migrations/0001_recovery_points.sql"),
        include_str!("../migrations/0002_plan_identity_version.sql"),
        include_str!("../migrations/0003_server_confirmations.sql"),
        include_str!("../migrations/0004_execution_permits.sql"),
    ];
    let migrations = sql
        .iter()
        .enumerate()
        .map(|(i, s)| {
            sqlx::migrate::Migration::new(
                (i + 1) as i64,
                format!("baseline {}", i + 1).into(),
                sqlx::migrate::MigrationType::Simple,
                s.replace("\r\n", "\n").into(),
                false,
            )
        })
        .collect();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&db.sql)
    .await
    .unwrap();
    let source = Db::new().await;
    let c = source.confirmed().await;
    // Keep the v4 schema and copy only its pre-authentication storage records.
    db.sql.execute(format!(
        "INSERT INTO tasks SELECT * FROM {0}.tasks;
         INSERT INTO operations SELECT * FROM {0}.operations;
         INSERT INTO server_confirmations SELECT confirmation_ref,contract_version,owner_id,task_id,operation_id,node_id,tool_name,session_digest,session_binding_version,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,status,prepared_at,expires_at,confirmed_at,revoked_at FROM {0}.server_confirmations;
         INSERT INTO audit_events SELECT * FROM {0}.audit_events;
         SELECT setval(pg_get_serial_sequence('audit_events','sequence'), (SELECT max(sequence) FROM audit_events));",
        source.schema
    ).as_str()).await.unwrap();
    source.finish().await;
    let snapshot = db
        .storage
        .recover(&db.session, &c.binding.task_id)
        .await
        .unwrap();
    // Existing non-authorizing raw storage row; no sealed Permit constructor or issuer is opened.
    sqlx::query("INSERT INTO execution_permits (permit_id,contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,fingerprint_payload,operation_fingerprint,binding_digest,issued_at_ms,expires_at_ms,status) SELECT $1,1,'unconnected_storage_fixture',owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,session_digest,session_binding_version,confirmation_ref,'unconnected_auth','{}',repeat('0',64),repeat('0',64),1,2,'REVOKED' FROM server_confirmations")
        .bind(format!("permit_{}",uuid::Uuid::new_v4())).execute(&db.sql).await.unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.sql)
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
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=4 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        db.storage
            .recover(&db.session, &c.binding.task_id)
            .await
            .unwrap(),
        snapshot
    );
    assert_eq!(
        db.confirmations
            .verify(
                &db.session,
                &c.binding.task_id,
                &c.confirmation_ref,
                &c.binding
            )
            .await
            .unwrap(),
        c
    );
    let permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(permits, 1);
    let mut delivery = db.create(&c).await.unwrap();
    let proof = SimulationProof::from_material(
        delivery
            .take_secret()
            .unwrap()
            .expose_for_isolated_backend(),
    )
    .unwrap();
    db.auth
        .submit(
            &db.session,
            &c.binding.task_id,
            &delivery.record().challenge_id,
            &SimulationSubmissionId::new_server_id(),
            &proof,
        )
        .await
        .unwrap();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn database_constraints_reject_wrong_mode_version_and_impossible_states() {
    let db = Db::new().await;
    let f = db.fixture().await;
    for change in [
        "contract_version=2",
        "mode='PRODUCTION'",
        "failures=max_failures",
        "status='SUCCEEDED'",
        "status='REVOKED'",
        "completed_at_ms=created_at_ms-1",
        "secret_bytes=0",
        "max_failures=0",
    ] {
        assert!(
            db.sql
                .execute(format!("UPDATE simulation_auth_challenges SET {change}").as_str())
                .await
                .is_err(),
            "{change}"
        );
    }
    let g = db.success(&f).await;
    for change in [
        "contract_version=2",
        "mode='PRODUCTION'",
        "status='REVOKED'",
        "expires_at_ms=issued_at_ms",
    ] {
        assert!(
            db.sql
                .execute(format!("UPDATE simulation_auth_grants SET {change}").as_str())
                .await
                .is_err(),
            "{change}"
        );
    }
    assert_eq!(
        db.auth
            .verify_grant(&db.session, &g.binding.task_id, &g.grant_id, &g.binding)
            .await
            .unwrap(),
        g
    );
    db.finish().await;
}

struct RebindingClock {
    source: Arc<Clock>,
    sessions: Arc<Sessions>,
    session: SessionRef,
    calls: AtomicUsize,
    change_at: usize,
}
impl ConfirmationClock for RebindingClock {
    fn now(&self) -> DateTime<Utc> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.change_at {
            self.sessions.bind(&self.session, "user_001", 2);
        }
        self.source.now()
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn session_change_at_every_time_observation_including_last_await_rolls_back() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let make_clock = |at| {
        Arc::new(RebindingClock {
            source: db.clock.clone(),
            sessions: db.sessions.clone(),
            session: db.session.clone(),
            calls: AtomicUsize::new(0),
            change_at: at,
        })
    };
    let initial = make_clock(usize::MAX);
    let service = SimulationAuthService::for_isolated_backend_with_clock(
        db.repo.clone(),
        db.sessions.clone(),
        config(),
        SimulationAuthMode::FoundationIsolatedV1,
        initial.clone(),
    )
    .unwrap();
    service
        .submit(
            &db.session,
            &f.challenge.binding.task_id,
            &f.challenge.challenge_id,
            &SimulationSubmissionId::new_server_id(),
            &f.proof,
        )
        .await
        .unwrap();
    let observations = initial.calls.load(Ordering::SeqCst);
    assert!(observations >= 9);
    for at in 1..=observations {
        db.sessions.bind(&db.session, "user_001", 1);
        let f = db.fixture().await;
        let clock = make_clock(at);
        let service = SimulationAuthService::for_isolated_backend_with_clock(
            db.repo.clone(),
            db.sessions.clone(),
            config(),
            SimulationAuthMode::FoundationIsolatedV1,
            clock.clone(),
        )
        .unwrap();
        assert!(
            service
                .submit(
                    &db.session,
                    &f.challenge.binding.task_id,
                    &f.challenge.challenge_id,
                    &SimulationSubmissionId::new_server_id(),
                    &f.proof
                )
                .await
                .is_err(),
            "time observation {at}"
        );
        assert!(clock.calls.load(Ordering::SeqCst) >= at);
        assert_eq!(db.raw(&f).await, ("PENDING".into(), 0));
        let grants: i64 =
            sqlx::query_scalar("SELECT count(*) FROM simulation_auth_grants WHERE challenge_id=$1")
                .bind(f.challenge.challenge_id.as_str())
                .fetch_one(&db.sql)
                .await
                .unwrap();
        assert_eq!(grants, 0);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn equal_length_wrong_material_must_fail_actual_digest_comparison() {
    let db = Db::new().await;
    let c = db.confirmed().await;
    let mut delivery = db.create(&c).await.unwrap();
    let secret = delivery.take_secret().unwrap();
    let proof = SimulationProof::from_material(secret.expose_for_isolated_backend()).unwrap();
    let mut bad = zeroize::Zeroizing::new(secret.expose_for_isolated_backend().to_vec());
    bad[0] ^= 1;
    let bad = SimulationProof::from_material(&bad).unwrap();
    let f = Fixture {
        confirmation: c,
        challenge: delivery.record().clone(),
        proof,
    };
    assert_eq!(
        db.submit(&f, &SimulationSubmissionId::new_server_id(), &bad)
            .await
            .unwrap()
            .result,
        SimulationVerificationResult::NotVerified
    );
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    db.success(&f).await;
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_failure_duplicates_count_once() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let id = SimulationSubmissionId::new_server_id();
    let mut jobs = vec![];
    for _ in 0..6 {
        let (a, s, t, c, i) = (
            db.auth.clone(),
            db.session.clone(),
            f.challenge.binding.task_id.clone(),
            f.challenge.challenge_id.clone(),
            id.clone(),
        );
        jobs.push(tokio::spawn(async move {
            a.submit(&s, &t, &c, &i, &wrong()).await.unwrap()
        }));
    }
    let mut first = None;
    for job in jobs {
        let r = job.await.unwrap();
        if let Some(ref expected) = first {
            assert_eq!(expected, &r);
        } else {
            first = Some(r);
        }
    }
    assert_eq!(db.raw(&f).await, ("PENDING".into(), 1));
    assert_eq!(db.count("simulation_auth_submissions").await, 1);
    assert_eq!(db.audits("SIM_AUTH_FAILED").await, 1);
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn time_observation_survives_authorization_rollback_without_committing_auth_effects() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let later = db.clock.get() + 5000;
    db.clock.set(later);
    fault(&db, "audit_events", false).await;
    let id = SimulationSubmissionId::new_server_id();
    assert_eq!(
        db.submit(&f, &id, &f.proof).await,
        Err(StorageError::Database)
    );
    clear_fault(&db, "audit_events").await;
    assert_eq!(db.raw(&f).await, ("PENDING".into(), 0));
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    assert_eq!(db.count("simulation_auth_submissions").await, 0);
    let observed: i64 =
        sqlx::query_scalar("SELECT last_observed_ms FROM simulation_auth_time_fence")
            .fetch_one(&db.sql)
            .await
            .unwrap();
    assert_eq!(observed, later);
    db.clock.set(later - 1000);
    assert!(matches!(
        db.submit(&f, &id, &f.proof).await,
        Err(StorageError::Invalid(_))
    ));
    db.clock.set(later);
    db.submit(&f, &id, &f.proof).await.unwrap();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn parallel_time_observation_invalidates_waiting_success_without_pool_deadlock() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let other = db.fixture().await;
    let (blocker, pid) = audit_blocker(&db, "SIM_AUTH_VERIFIED").await;
    let (a, s, t, c, p) = (
        db.auth.clone(),
        db.session.clone(),
        f.challenge.binding.task_id.clone(),
        f.challenge.challenge_id.clone(),
        f.proof,
    );
    let job = tokio::spawn(async move {
        a.submit(&s, &t, &c, &SimulationSubmissionId::new_server_id(), &p)
            .await
    });
    wait_blocked(&db, pid).await;
    let advanced = Arc::new(Clock(AtomicI64::new(other.challenge.expires_at_ms)));
    let observer = Db::service(db.repo.clone(), db.sessions.clone(), advanced, config());
    assert_eq!(
        observer
            .read_challenge(
                &db.session,
                &other.challenge.binding.task_id,
                &other.challenge.challenge_id
            )
            .await,
        Err(StorageError::AuthExpired)
    );
    blocker.commit().await.unwrap();
    assert!(matches!(job.await.unwrap(), Err(StorageError::Invalid(_))));
    assert_eq!(db.count("simulation_auth_grants").await, 0);
    db.finish().await;
}

struct JumpClock {
    source: Arc<Clock>,
    calls: AtomicUsize,
    jump_at: usize,
    jump_to: i64,
}
impl ConfirmationClock for JumpClock {
    fn now(&self) -> DateTime<Utc> {
        let time = if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.jump_at {
            self.jump_to
        } else {
            self.source.get()
        };
        DateTime::from_timestamp_millis(time).unwrap()
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn each_time_observation_is_durable_even_when_verification_errors() {
    let db = Db::new().await;
    let f = db.fixture().await;
    let clock = Arc::new(JumpClock {
        source: db.clock.clone(),
        calls: AtomicUsize::new(0),
        jump_at: usize::MAX,
        jump_to: f.confirmation.expires_at.timestamp_millis(),
    });
    let a = SimulationAuthService::for_isolated_backend_with_clock(
        db.repo.clone(),
        db.sessions.clone(),
        config(),
        SimulationAuthMode::FoundationIsolatedV1,
        clock.clone(),
    )
    .unwrap();
    a.submit(
        &db.session,
        &f.challenge.binding.task_id,
        &f.challenge.challenge_id,
        &SimulationSubmissionId::new_server_id(),
        &f.proof,
    )
    .await
    .unwrap();
    let observations = clock.calls.load(Ordering::SeqCst);
    db.finish().await;
    for at in 1..=observations {
        let db = Db::new().await;
        let f = db.fixture().await;
        let later = f.confirmation.expires_at.timestamp_millis();
        let clock = Arc::new(JumpClock {
            source: db.clock.clone(),
            calls: AtomicUsize::new(0),
            jump_at: at,
            jump_to: later,
        });
        let a = SimulationAuthService::for_isolated_backend_with_clock(
            db.repo.clone(),
            db.sessions.clone(),
            config(),
            SimulationAuthMode::FoundationIsolatedV1,
            clock.clone(),
        )
        .unwrap();
        assert!(
            a.submit(
                &db.session,
                &f.challenge.binding.task_id,
                &f.challenge.challenge_id,
                &SimulationSubmissionId::new_server_id(),
                &f.proof
            )
            .await
            .is_err(),
            "observation {at}"
        );
        let observed: i64 =
            sqlx::query_scalar("SELECT last_observed_ms FROM simulation_auth_time_fence")
                .fetch_one(&db.sql)
                .await
                .unwrap();
        assert_eq!(observed, later, "observation {at}");
        assert_eq!(db.count("simulation_auth_grants").await, 0);
        assert_eq!(db.count("simulation_auth_submissions").await, 0);
        assert!(matches!(db.read(&f).await, Err(StorageError::Invalid(_))));
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn oversized_confirmation_reference_and_invalid_session_version_do_not_write_auth() {
    let db = Db::new().await;
    let c = db.confirmed().await;
    let reference = safeflow_contracts::ConfirmationRef::parse("x".repeat(201)).unwrap();
    assert!(matches!(
        db.auth
            .create_challenge(
                &db.session,
                &c.binding.task_id,
                &c.binding.operation_id,
                &reference
            )
            .await,
        Err(StorageError::Invalid(_))
    ));
    for version in [0, u64::MAX] {
        db.sessions.bind(&db.session, "user_001", version);
        assert!(matches!(db.create(&c).await, Err(StorageError::Invalid(_))));
    }
    assert_eq!(db.count("simulation_auth_challenges").await, 0);
    db.finish().await;
}
