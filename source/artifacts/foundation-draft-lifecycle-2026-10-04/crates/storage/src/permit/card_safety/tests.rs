//! Real PostgreSQL; each invocation MUST use its own new empty database.
use super::*;
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeBootstrap, ToolRegistry,
    ToolSecurityCatalog,
};
use std::{collections::HashMap, sync::Mutex};
const CONTROL: &str = "isolated-unit-controller-secret-not-http";
const ACCOUNT: &str = "account_001";
const CARD: &str = "demo_card_salary_v1";
pub(super) struct Sessions(Mutex<HashMap<SessionRef, SessionBinding>>);
impl Sessions {
    pub(super) fn add(&self, s: &str, owner: &str, version: u64) -> SessionRef {
        let r = SessionRef::parse(s).unwrap();
        self.0.lock().unwrap().insert(
            r.clone(),
            SessionBinding {
                user_id: UserId::parse(owner).unwrap(),
                binding_version: version,
            },
        );
        r
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, s: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(s)?.user_id)
    }
    fn resolve_binding(&self, s: &SessionRef) -> Result<SessionBinding, SecurityError> {
        self.0.lock().unwrap().get(s).cloned().ok_or(SecurityError {
            reason_code: ReasonCode::parse("SESSION_INVALID").unwrap(),
            message: "unit session".into(),
        })
    }
}
pub(super) struct Clock(AtomicI64);
impl ConfirmationClock for Clock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.0.load(Ordering::SeqCst)).unwrap()
    }
}
impl Clock {
    pub(super) fn add(&self, n: i64) {
        self.0.fetch_add(n, Ordering::SeqCst);
    }
}
pub(super) async fn setup() -> (
    PgUnifiedFictionalSource,
    Arc<Sessions>,
    SessionRef,
    Arc<Clock>,
) {
    let url =
        std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("new owned PG required, no silent skip");
    let raw = std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").unwrap()).unwrap();
    let source = PgUnifiedFictionalSource::provision_new_database(
        &url,
        "sf_unified_card",
        crate::UnifiedFictionalDocument::read_adopted_v1(&raw).unwrap(),
    )
    .await
    .unwrap();
    let sessions = Arc::new(Sessions(Mutex::new(HashMap::new())));
    let s = sessions.add("owner_session", "user_001", 1);
    let catalog = CardCatalogSourceBinding::for_reviewed_backend_provider(
        UNIFIED_FICTIONAL_SOURCE,
        UNIFIED_FICTIONAL_IMPLEMENTATION,
        Arc::new(source.clone()),
    )
    .unwrap();
    crate::CardCatalogService::for_trusted_backend(
        source.repository_for_isolated_backend(),
        sessions.clone(),
        Some(catalog),
    )
    .initialize(&s, &ResourceId::parse(ACCOUNT).unwrap())
    .await
    .unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(chrono::Utc::now().timestamp_millis())));
    (source, sessions, s, clock)
}
pub(super) fn runtime(s: Arc<Sessions>) -> safeflow_runtime::RuntimeAssembly {
    RuntimeBootstrap::build(
        ToolRegistry::new(),
        DomainPolicyRegistry::new(),
        ToolSecurityCatalog::new(),
        s,
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap()
}
pub(super) fn host() -> CardSafetyHost {
    CardSafetyHost::for_isolated_native_host(Zeroizing::new(CONTROL.into())).unwrap()
}
pub(super) async fn prepared(service: &CardSafetyService<'_>, s: &SessionRef) -> CardSafetyView {
    service
        .prepare(
            s,
            &ResourceId::parse(ACCOUNT).unwrap(),
            CARD,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
}
pub(super) async fn verified(service: &CardSafetyService<'_>, s: &SessionRef, v: &CardSafetyView) -> String {
    service
        .confirm(s, &v.operation_id, &v.confirmation_ref)
        .await
        .unwrap();
    let mut c = service
        .challenge_for_native_controller(CONTROL, s, &v.operation_id)
        .await
        .unwrap();
    let code = c.take_secret().unwrap();
    assert!(c.take_secret().is_none());
    assert!(service
        .verify_code(
            s,
            &v.operation_id,
            &c.challenge_id,
            Zeroizing::new(code.expose_once_for_native_display().into())
        )
        .await
        .unwrap());
    c.challenge_id
}
async fn no_business(source: &PgUnifiedFictionalSource) {
    let db = &source.repository_for_isolated_backend().pool;
    let count:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM card_locks_v1)+(SELECT count(*) FROM transfer_fund_orders_v1)+(SELECT count(*) FROM transfer_ledger_v1)").fetch_one(db).await.unwrap();
    assert_eq!(count, 0);
    let balance: i64 =
        sqlx::query_scalar("SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id=$1")
            .bind(ACCOUNT)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(balance, 128800);
    let ack: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public_audit_outbox WHERE acknowledged_at IS NOT NULL",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(ack, 0);
}
#[tokio::test]
async fn card_safety_positive_single_consume_rollback_and_no_generic_bypass() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock.clone());
    let v = prepared(&service, &s).await;
    assert!(!v.business_executed);
    assert!(service.issue_permit(&s, &v.operation_id).await.is_err());
    clock.add(600001); // PREPARED age does not eat the explicit confirmation TTL.
    let c = service
        .confirm(&s, &v.operation_id, &v.confirmation_ref)
        .await
        .unwrap();
    assert_eq!(
        c.confirmation_expires_at.unwrap().timestamp_millis()
            - c.confirmed_at.unwrap().timestamp_millis(),
        300000
    );
    let again = service
        .confirm(&s, &v.operation_id, &v.confirmation_ref)
        .await
        .unwrap();
    assert_eq!(again.confirmed_at, c.confirmed_at);
    assert!(service
        .challenge_for_native_controller("wrong-controller", &s, &v.operation_id)
        .await
        .is_err());
    let cid = verified(&service, &s, &v).await;
    assert!(service.check_permit(&s, &v.operation_id).await.is_err()); // verified auth is not a permit
    assert!(service
        .challenge_for_native_controller(CONTROL, &s, &v.operation_id)
        .await
        .unwrap()
        .take_secret()
        .is_none());
    assert!(service
        .verify_code(&s, &v.operation_id, &cid, Zeroizing::new("000000".into()))
        .await
        .is_err());
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    service.check_permit(&s, &v.operation_id).await.unwrap();
    let (pid, bind) = {
        let (mut u, o) = service.unit(&s, &v.operation_id).await.unwrap();
        let p: String =
            sqlx::query_scalar("SELECT permit_id FROM execution_permits WHERE operation_id=$1")
                .bind(v.operation_id.as_str())
                .fetch_one(&mut **u.tx.as_mut().unwrap())
                .await
                .unwrap();
        u.rollback().await;
        (
            PermitId::parse(p).unwrap(),
            PermitBinding {
                snapshot: o.confirmation.binding.clone(),
                fingerprint: compute_operation_fingerprint(&payload(&o).unwrap()).unwrap(),
            },
        )
    };
    assert!(service
        .store
        .read(&s, &v.task_id, &pid, &bind)
        .await
        .is_err());
    let (mut u, _) = service.unit(&s, &v.operation_id).await.unwrap();
    u.consume(&pid, &bind).await.unwrap();
    assert!(u.consume(&pid, &bind).await.is_err()); // poisons and rolls back
    service.check_permit(&s, &v.operation_id).await.unwrap();
    let (mut u, _) = service.unit(&s, &v.operation_id).await.unwrap();
    u.consume(&pid, &bind).await.unwrap();
    assert!(u.finish(true).await.is_err()); // no fake completion commit
    service.check_permit(&s, &v.operation_id).await.unwrap();
    let row=sqlx::query("SELECT issued_at_ms,expires_at_ms,status,storage_version FROM execution_permits WHERE operation_id=$1").bind(v.operation_id.as_str()).fetch_one(&source.repository_for_isolated_backend().pool).await.unwrap();
    assert_eq!(
        row.get::<i64, _>("expires_at_ms") - row.get::<i64, _>("issued_at_ms"),
        60000
    );
    assert_eq!(row.get::<String, _>("status"), "ACTIVE");
    assert_eq!(row.get::<i16, _>("storage_version"), 3);
    clock.add(60000);
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    assert!(service.issue_permit(&s, &v.operation_id).await.is_err());
    no_business(&source).await;
}
#[tokio::test]
async fn card_safety_challenge_attempt_limit_concurrency_and_hash_only() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock);
    let v = prepared(&service, &s).await;
    service
        .confirm(&s, &v.operation_id, &v.confirmation_ref)
        .await
        .unwrap();
    let mut c = service
        .challenge_for_native_controller(CONTROL, &s, &v.operation_id)
        .await
        .unwrap();
    let code = c.take_secret().unwrap();
    let correct = code.expose_once_for_native_display();
    let wrong = format!("{:06}", (correct.parse::<u32>().unwrap() + 1) % 1000000);
    for _ in 0..5 {
        assert!(!service
            .verify_code(
                &s,
                &v.operation_id,
                &c.challenge_id,
                Zeroizing::new(wrong.clone())
            )
            .await
            .unwrap());
    }
    assert!(service
        .verify_code(
            &s,
            &v.operation_id,
            &c.challenge_id,
            Zeroizing::new(correct.into())
        )
        .await
        .is_err());
    assert!(service.issue_permit(&s, &v.operation_id).await.is_err());
    let r=sqlx::query("SELECT failures,state,code_hash,grant_ref FROM card_safety_challenges_v1 WHERE operation_id=$1").bind(v.operation_id.as_str()).fetch_one(&source.repository_for_isolated_backend().pool).await.unwrap();
    assert_eq!(r.get::<i16, _>("failures"), 5);
    assert_eq!(r.get::<String, _>("state"), "FAILED");
    assert_eq!(
        r.get::<String, _>("code_hash"),
        code_hash(&c.challenge_id, correct)
    );
    assert!(r.get::<Option<String>, _>("grant_ref").is_none());
    let v2 = prepared(&service, &s).await;
    service
        .confirm(&s, &v2.operation_id, &v2.confirmation_ref)
        .await
        .unwrap();
    let mut c2 = service
        .challenge_for_native_controller(CONTROL, &s, &v2.operation_id)
        .await
        .unwrap();
    let code2 = c2.take_secret().unwrap();
    let (a, b) = tokio::join!(
        service.verify_code(
            &s,
            &v2.operation_id,
            &c2.challenge_id,
            Zeroizing::new(code2.expose_once_for_native_display().into())
        ),
        service.verify_code(
            &s,
            &v2.operation_id,
            &c2.challenge_id,
            Zeroizing::new(code2.expose_once_for_native_display().into())
        )
    );
    assert_eq!([a, b].iter().filter(|r| matches!(r, Ok(true))).count(), 1);
    let text: String =
        sqlx::query_scalar("SELECT coalesce(string_agg(details::text,''),'') FROM audit_events")
            .fetch_one(&source.repository_for_isolated_backend().pool)
            .await
            .unwrap();
    for key in ["\"code\"", "code_hash", "controller_secret"] {
        assert!(!text.contains(key));
    }
    no_business(&source).await;
}
#[tokio::test]
async fn card_safety_exact_challenge_grant_confirmation_deadlines() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock.clone());
    let v = prepared(&service, &s).await;
    service
        .confirm(&s, &v.operation_id, &v.confirmation_ref)
        .await
        .unwrap();
    let mut c = service
        .challenge_for_native_controller(CONTROL, &s, &v.operation_id)
        .await
        .unwrap();
    let code = c.take_secret().unwrap();
    clock.add(120000);
    assert!(service
        .verify_code(
            &s,
            &v.operation_id,
            &c.challenge_id,
            Zeroizing::new(code.expose_once_for_native_display().into())
        )
        .await
        .is_err());
    let v2 = prepared(&service, &s).await;
    verified(&service, &s, &v2).await;
    clock.add(120000);
    assert!(service.issue_permit(&s, &v2.operation_id).await.is_err());
    let v3 = prepared(&service, &s).await;
    service
        .confirm(&s, &v3.operation_id, &v3.confirmation_ref)
        .await
        .unwrap();
    clock.add(300000);
    assert!(service
        .confirm(&s, &v3.operation_id, &v3.confirmation_ref)
        .await
        .is_err());
    assert!(service
        .challenge_for_native_controller(CONTROL, &s, &v3.operation_id)
        .await
        .is_err());
    assert_eq!(
        service.query(&s, &v3.operation_id).await.unwrap().state,
        "PREPARED"
    );
    no_business(&source).await;
}
#[tokio::test]
async fn card_safety_cancel_nonce_owner_rebind_restart_and_history() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let h = host();
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions.clone(), rt.runtime(), h)
            .unwrap()
            .with_clock(clock.clone());
    let n = uuid::Uuid::new_v4().to_string();
    let account = ResourceId::parse(ACCOUNT).unwrap();
    let v = service.prepare(&s, &account, CARD, &n).await.unwrap();
    assert_eq!(
        service
            .prepare(&s, &account, CARD, &n)
            .await
            .unwrap()
            .operation_id,
        v.operation_id
    );
    assert!(service
        .prepare(&s, &account, "another_card", &n)
        .await
        .is_err());
    let foreign = sessions.add("foreign_session", "user_002", 1);
    assert!(service.query(&foreign, &v.operation_id).await.is_err());
    assert!(service
        .prepare(&foreign, &account, CARD, &uuid::Uuid::new_v4().to_string())
        .await
        .is_err());
    verified(&service, &s, &v).await;
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    let reboot = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock.clone());
    assert!(reboot.check_permit(&s, &v.operation_id).await.is_err());
    assert_eq!(
        reboot
            .query(&s, &v.operation_id)
            .await
            .unwrap()
            .operation_id,
        v.operation_id
    );
    service.cancel(&s, &v.operation_id).await.unwrap();
    assert_eq!(
        service.query(&s, &v.operation_id).await.unwrap().state,
        "REVOKED"
    );
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    let new = sessions.add("new_owner_session", "user_001", 2);
    assert_eq!(
        service.query(&new, &v.operation_id).await.unwrap().state,
        "REVOKED"
    );
    assert!(service.prepare(&new, &account, CARD, &n).await.is_err());
    let v2 = prepared(&service, &s).await;
    verified(&service, &s, &v2).await;
    service.issue_permit(&s, &v2.operation_id).await.unwrap();
    sessions.add("owner_session", "user_001", 2);
    assert!(service.check_permit(&s, &v2.operation_id).await.is_err());
    assert_eq!(
        service
            .query(&s, &v2.operation_id)
            .await
            .unwrap()
            .operation_id,
        v2.operation_id
    );
    no_business(&source).await;
}
#[tokio::test]
async fn card_safety_durable_revocation_roots_source_and_serialization() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let v = prepared(&service, &s).await;
    verified(&service, &s, &v).await;
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    let (mut u, _) = service.unit(&s, &v.operation_id).await.unwrap();
    guard_unit(&mut u).await.unwrap();
    let rev = service.revoke_session(&s);
    tokio::pin!(rev);
    assert!(tokio::time::timeout(Duration::from_millis(150), &mut rev)
        .await
        .is_err());
    u.rollback().await;
    rev.await.unwrap();
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    assert_eq!(
        service.query(&s, &v.operation_id).await.unwrap().state,
        "PREPARED"
    );
    let new = sessions.add("new_s", "user_001", 2);
    let v2 = prepared(&service, &new).await;
    verified(&service, &new, &v2).await;
    service.issue_permit(&new, &v2.operation_id).await.unwrap();
    let (mut u, _) = service.unit(&new, &v2.operation_id).await.unwrap();
    guard_unit(&mut u).await.unwrap();
    let deactivate = source.deactivate_for_isolated_backend();
    tokio::pin!(deactivate);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut deactivate)
            .await
            .is_err()
    );
    u.rollback().await;
    deactivate.await.unwrap();
    assert!(service.check_permit(&new, &v2.operation_id).await.is_err());
    assert_eq!(
        service.query(&new, &v2.operation_id).await.unwrap().state,
        "PREPARED"
    );
    no_business(&source).await;
}
#[tokio::test]
async fn card_safety_account_fence_and_catalog_change_reject() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let h = host();
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions.clone(), rt.runtime(), h)
            .unwrap()
            .with_clock(clock.clone());
    let v = prepared(&service, &s).await;
    verified(&service, &s, &v).await;
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    let (mut u, _) = service.unit(&s, &v.operation_id).await.unwrap();
    guard_unit(&mut u).await.unwrap();
    let owner = UserId::parse("user_001").unwrap();
    let account = ResourceId::parse(ACCOUNT).unwrap();
    let rev = service.revoke_account_for_native_controller(CONTROL, &owner, &account);
    tokio::pin!(rev);
    assert!(tokio::time::timeout(Duration::from_millis(150), &mut rev)
        .await
        .is_err());
    u.rollback().await;
    rev.await.unwrap();
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    assert_eq!(
        service.query(&s, &v.operation_id).await.unwrap().state,
        "PREPARED"
    );
    let reboot =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock);
    assert!(reboot
        .prepare(&s, &account, CARD, &uuid::Uuid::new_v4().to_string())
        .await
        .is_err());
    no_business(&source).await;
}

#[tokio::test]
async fn card_safety_actual_catalog_aba_and_account_owner_change_reject() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock);
    let v = prepared(&service, &s).await;
    verified(&service, &s, &v).await;
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    let repo = source.repository_for_isolated_backend();
    sqlx::query("UPDATE card_catalog_v1 SET last_four='1234' WHERE card_id=$1")
        .bind(CARD)
        .execute(&repo.pool)
        .await
        .unwrap();
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    assert_eq!(
        service
            .query(&s, &v.operation_id)
            .await
            .unwrap()
            .operation_id,
        v.operation_id
    ); // original historical parameters
    sqlx::query("UPDATE card_catalog_v1 SET last_four='6628' WHERE card_id=$1")
        .bind(CARD)
        .execute(&repo.pool)
        .await
        .unwrap();
    assert!(service.check_permit(&s, &v.operation_id).await.is_err()); // ABA revision changed
                                                                       // Adopted 0024 expressly forbids ownership transfer for initialized cards.
                                                                       // Keep that real trigger; do not invent a supported ownership-change API.
    let owner_change =
        sqlx::query("UPDATE transfer_accounts_v1 SET owner_id='changed_owner' WHERE account_id=$1")
            .bind(ACCOUNT)
            .execute(&repo.pool)
            .await
            .unwrap_err();
    assert!(owner_change
        .to_string()
        .contains("CARD_OWNER_CHANGE_NOT_ENABLED"));
    let actual: String =
        sqlx::query_scalar("SELECT owner_id FROM transfer_accounts_v1 WHERE account_id=$1")
            .bind(ACCOUNT)
            .fetch_one(&repo.pool)
            .await
            .unwrap();
    assert_eq!(actual, "user_001");
    for sql in ["INSERT INTO transfer_accounts_v1(account_id,owner_id,opening_cents,booked_cents) VALUES('extra','extra',0,0)","UPDATE transfer_accounts_v1 SET owner_id='extra' WHERE account_id='account_002'","UPDATE transfer_payee_routes_v1 SET payee_id='changed'","DELETE FROM transfer_payee_routes_v1"]{
        let error=sqlx::query(sql).execute(&repo.pool).await.unwrap_err();assert!(error.to_string().contains("ADOPTED_UNIFIED_V1_SCOPE_IMMUTABLE"));
    }
    assert_eq!(
        service
            .query(&s, &v.operation_id)
            .await
            .unwrap()
            .operation_id,
        v.operation_id
    );
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    no_business(&source).await;
}

// Ordinary trusted-clock and current-session regression. This does not insert
// SQL waits or claim to reproduce either reviewed asynchronous interleaving.
#[tokio::test]
async fn card_safety_repair_verification_time_and_current_history() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let v = prepared(&service, &s).await;
    let confirmed = service
        .confirm(&s, &v.operation_id, &v.confirmation_ref)
        .await
        .unwrap();
    let mut challenge = service
        .challenge_for_native_controller(CONTROL, &s, &v.operation_id)
        .await
        .unwrap();
    let code = challenge.take_secret().unwrap();
    clock.add(17000);
    let actual = clock.now().timestamp_millis();
    assert!(service
        .verify_code(
            &s,
            &v.operation_id,
            &challenge.challenge_id,
            Zeroizing::new(code.expose_once_for_native_display().into())
        )
        .await
        .unwrap());
    let repo = source.repository_for_isolated_backend();
    let row = sqlx::query("SELECT verified_ms,grant_expires_ms,code_hash FROM card_safety_challenges_v1 WHERE operation_id=$1")
        .bind(v.operation_id.as_str()).fetch_one(&repo.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("verified_ms"), actual);
    assert_eq!(row.get::<i64, _>("grant_expires_ms"), actual + 120000);
    assert_ne!(
        row.get::<String, _>("code_hash"),
        code.expose_once_for_native_display()
    );
    service.issue_permit(&s, &v.operation_id).await.unwrap();
    service.check_permit(&s, &v.operation_id).await.unwrap();
    clock.add(120000);
    assert!(service.check_permit(&s, &v.operation_id).await.is_err());
    assert_eq!(
        service
            .query(&s, &v.operation_id)
            .await
            .unwrap()
            .confirmed_at,
        confirmed.confirmed_at
    );
    sessions.0.lock().unwrap().remove(&s);
    assert!(matches!(
        service.query(&s, &v.operation_id).await,
        Err(StorageError::Session)
    ));
    let fresh = sessions.add("repair_fresh_owner_session", "user_001", 2);
    clock.add(300000);
    let restarted = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock);
    let history = restarted.query(&fresh, &v.operation_id).await.unwrap();
    assert_eq!(history.operation_id, v.operation_id);
    assert_eq!(history.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(history.confirmed_at, confirmed.confirmed_at);
    assert_eq!(
        history.confirmation_expires_at,
        confirmed.confirmation_expires_at
    );
    assert!(!history.business_executed);
    assert!(restarted
        .issue_permit(&fresh, &v.operation_id)
        .await
        .is_err());
    no_business(&source).await;
}
