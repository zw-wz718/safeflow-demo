//! Public assembly mechanism fixtures, NOT approved shared-demo card content.
use super::*;
#[path = "payment_http_tests.rs"]
mod payment_http_tests;
#[path = "card_payment_composition_tests/completion_candidate_tests.rs"]
mod completion_candidate_tests;
use safeflow_contracts::{
    ContractFuture, PolicyVersion, ResourceId, SecurityLevel, SessionRef, ToolName,
    ToolResultStatus,
};
use safeflow_runtime::{SessionBinding, ToolSecurityProfile};
use safeflow_storage::{
    BackendCardCatalog, BackendCardCatalogProvider, BackendCatalogCard, CardCatalogService,
    CardCatalogSourceBinding, PaymentExecutionService, PermitConfig, StorageError,
};
use safeflow_transfer_integration::{funds_store::FundsStore, mock_execution::MockBehavior};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct PaymentServer {
    inner: Server,
    final_runtime: safeflow_runtime::RuntimeAssembly,
}
impl PaymentServer {
    fn stop(self) {
        self.inner.stop();
    }
}
impl std::ops::Deref for PaymentServer {
    type Target = Server;
    fn deref(&self) -> &Server {
        &self.inner
    }
}
fn payment_profile(level: SecurityLevel) -> ToolSecurityProfile {
    ToolSecurityProfile {
        tool_name: ToolName::parse("execute_confirmed_payment").unwrap(),
        tool_schema_version: 1,
        policy_version: PolicyVersion::parse("transfer-funds-business-2026-10-18-v1").unwrap(),
        minimum_level: level,
    }
}
async fn server(db: &Db) -> PaymentServer {
    let inner = super::server(db).await;
    let final_runtime = safeflow_runtime::RuntimeBootstrap::build(
        safeflow_runtime::ToolRegistry::new(),
        safeflow_runtime::DomainPolicyRegistry::new(),
        safeflow_runtime::ToolSecurityCatalog::new(),
        inner.state.0.sessions.clone(),
        Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
    )
    .unwrap()
    .with_final_payment_policy_for_backend(
        safeflow_storage::payment_final_registration_for_backend(payment_profile(
            SecurityLevel::Yellow,
        ))
        .unwrap(),
    )
    .unwrap();
    PaymentServer {
        inner,
        final_runtime,
    }
}

// A test-only real registration reproduces the review's existing-Profile/Policy
// counterexample. The Tool is never invoked and is not a production workaround.
struct AdoptedGate {
    name: ToolName,
    deny: bool,
    calls: Arc<AtomicU64>,
}
impl safeflow_contracts::AgentTool for AdoptedGate {
    fn definition(&self) -> safeflow_contracts::ToolDefinition {
        safeflow_contracts::ToolDefinition::new(
            self.name.clone(),
            "review gate regression",
            json!({"type":"object"}),
            1,
            true,
            "PAYMENT",
        )
        .unwrap()
    }
    fn execute<'a>(
        &'a self,
        _: &'a safeflow_contracts::ExecutionContext,
        _: safeflow_contracts::ToolExecutionAuthorization<'a>,
        _: Value,
    ) -> ContractFuture<'a, Result<safeflow_contracts::ToolResult, safeflow_contracts::ToolError>>
    {
        Box::pin(async { panic!("registered gate Tool must never execute") })
    }
}
impl safeflow_contracts::DomainPolicy for AdoptedGate {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        _: &'a safeflow_contracts::SecurityContext,
    ) -> ContractFuture<
        'a,
        Result<Vec<safeflow_contracts::SecurityFinding>, safeflow_contracts::SecurityError>,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.deny {
                return Ok(vec![]);
            }
            Ok(vec![safeflow_contracts::SecurityFinding {
                check_id: safeflow_contracts::CheckId::parse("COMMON-01").unwrap(),
                policy_version: payment_profile(SecurityLevel::Yellow).policy_version,
                source: safeflow_contracts::SourceName::parse("adopted_gate").unwrap(),
                kind: safeflow_contracts::FindingKind::HardDeny,
                reason_code: safeflow_contracts::ReasonCode::parse("REGISTERED_DENY").unwrap(),
                evidence_refs: vec![],
                observed_value: None,
                event_ids: vec![],
                dedup_key: None,
                window_start_ms: None,
                window_end_ms: None,
            }])
        })
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a safeflow_contracts::PostExecuteContext,
    ) -> ContractFuture<
        'a,
        Result<Vec<safeflow_contracts::SecurityFinding>, safeflow_contracts::SecurityError>,
    > {
        Box::pin(async { Ok(vec![]) })
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; adopted Kernel gates must not be bypassed"]
async fn card_payment_composition_registered_higher_gate_deny_missing_mismatched_configuration_have_no_effects(
) {
    use safeflow_runtime::{
        DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeBootstrap, ToolRegistry,
        ToolSecurityCatalog,
    };
    for mode in [
        "red",
        "critical",
        "deny",
        "missing",
        "version",
        "implementation",
        "schema",
    ] {
        let db = Db::current44().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (p, _, binding) = setup(&db, &server, &cookie).await;
        let before = payment_execution_tests::persistent_state(&db).await;
        let calls = Arc::new(AtomicU64::new(0));
        let mut tools = ToolRegistry::new();
        let mut policies = DomainPolicyRegistry::new();
        let mut catalog = ToolSecurityCatalog::new();
        if matches!(mode, "red" | "critical" | "deny" | "version" | "schema") {
            let gate = Arc::new(AdoptedGate {
                name: payment_profile(SecurityLevel::Yellow).tool_name,
                deny: mode == "deny",
                calls: calls.clone(),
            });
            let mut profile = payment_profile(if mode == "red" {
                SecurityLevel::Red
            } else if mode == "critical" {
                SecurityLevel::Critical
            } else {
                SecurityLevel::Yellow
            });
            if mode == "version" {
                profile.policy_version = PolicyVersion::parse("different-adopted-policy").unwrap();
            }
            tools.register(gate.clone()).unwrap();
            policies
                .register(gate, profile.policy_version.clone(), 1)
                .unwrap();
            catalog.register(profile).unwrap();
        }
        let runtime = RuntimeBootstrap::build(
            tools,
            policies,
            catalog,
            server.state.0.sessions.clone(),
            Arc::new(InMemoryAuditRecorder::new()),
        )
        .unwrap();
        let runtime = if mode == "missing" {
            Ok(runtime)
        } else if mode == "implementation" {
            runtime.with_final_payment_policy_for_backend(
                safeflow_runtime::BackendFinalPolicyRegistration::for_reviewed_backend(
                    payment_profile(SecurityLevel::Yellow),
                    "wrong_impl",
                    Arc::new(NeverFinalFacts),
                )
                .unwrap(),
            )
        } else {
            let mut profile = payment_profile(SecurityLevel::Yellow);
            if mode == "schema" {
                profile.tool_schema_version = 2;
            }
            let registration = if mode == "schema" {
                safeflow_runtime::BackendFinalPolicyRegistration::for_reviewed_backend(
                    profile,
                    "wrong_schema",
                    Arc::new(NeverFinalFacts),
                )
                .unwrap()
            } else {
                safeflow_storage::payment_final_registration_for_backend(profile).unwrap()
            };
            runtime.with_final_payment_policy_for_backend(registration)
        };
        match runtime {
            Err(_) => assert!(matches!(mode, "version" | "schema")),
            Ok(runtime) => {
                let svc = PaymentExecutionService::for_project_simulation(
                    db.repo.clone(),
                    server.state.0.sessions.clone(),
                    runtime.runtime(),
                    Some(binding),
                    payment_profile(SecurityLevel::Yellow),
                    PermitConfig::new(Duration::from_secs(30)).unwrap(),
                    MockBehavior::Immediate,
                );
                match svc {
                    Err(_) => assert!(matches!(mode, "missing" | "implementation")),
                    Ok(svc) => {
                        let error = execute(&svc, &cookie, &p)
                            .await
                            .err()
                            .expect("adopted gate was bypassed");
                        if matches!(mode, "deny" | "critical") {
                            assert!(
                                matches!(error, StorageError::DecisionSourcesUnavailable),
                                "{mode}: {error:?}"
                            );
                        } else {
                            assert!(
                                matches!(error, StorageError::FormalAuthUnavailable),
                                "{mode}: {error:?}"
                            );
                        }
                        assert!(
                            calls.load(Ordering::SeqCst) > 0,
                            "adopted policy was not evaluated"
                        );
                    }
                }
            }
        }
        assert_eq!(counts(&db).await, (0, 0, 0, 0, 0), "{mode}");
        assert_eq!(
            before,
            payment_execution_tests::persistent_state(&db).await,
            "{mode}"
        );
        server.stop();
        db.finish().await;
    }
}
struct NeverFinalFacts;
impl safeflow_runtime::BackendFinalPolicy for NeverFinalFacts {
    fn evaluate<'a>(
        &'a self,
        _: &'a safeflow_contracts::SecurityContext,
        _: &'a (dyn std::any::Any + Send + Sync),
    ) -> ContractFuture<
        'a,
        Result<Vec<safeflow_contracts::SecurityFinding>, safeflow_contracts::SecurityError>,
    > {
        Box::pin(async { panic!("mismatched registration must never evaluate") })
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; document the unadopted long-delay completion boundary"]
async fn card_payment_composition_delayed_confirmation_new_session_restart_keep_processing_without_new_authority(
) {
    for mode in ["expired", "new_session", "restart"] {
        let db = Db::current44().await;
        let live = server(&db).await;
        let cookie = live.login().await;
        let (p, _, binding) = setup(&db, &live, &cookie).await;
        let svc = service(
            &db,
            &live,
            Some(binding.clone()),
            MockBehavior::Pending,
            SecurityLevel::Yellow,
        );
        execute(&svc, &cookie, &p).await.unwrap();
        if mode == "expired" {
            sqlx::query("UPDATE server_confirmations SET prepared_at=clock_timestamp()-interval '10 seconds',confirmed_at=clock_timestamp()-interval '9 seconds',expires_at=clock_timestamp()-interval '1 second' WHERE confirmation_ref=$1")
                .bind(p["confirmation_ref"].as_str().unwrap()).execute(&db.scoped).await.unwrap();
        }
        let restarted = if mode == "restart" {
            Some(server(&db).await)
        } else {
            None
        };
        let current = restarted.as_ref().unwrap_or(&live);
        let current_cookie = if mode == "expired" {
            cookie.clone()
        } else {
            current.login().await
        };
        let before = payment_execution_tests::persistent_state(&db).await;
        let resolver = service(
            &db,
            current,
            Some(binding),
            MockBehavior::Pending,
            SecurityLevel::Yellow,
        );
        let task = serde_json::from_value(p["task_id"].clone()).unwrap();
        let op = serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap();
        assert!(
            resolver
                .resolve_simulated_success(&session(&current_cookie), &task, &op)
                .await
                .is_err(),
            "completion authority must not be inferred for {mode}"
        );
        let original = ok(query(
            current,
            &current_cookie,
            p["original_result_path"].as_str().unwrap(),
        )
        .await);
        assert_eq!(original["state"], "PROCESSING");
        assert_eq!(counts(&db).await, (1, 1, 1, 0, 1));
        // Expiration changes only confirmation test data; resolve never mutates
        // the already accepted business, holds, ledger, permit or Outbox.
        assert_eq!(
            before,
            payment_execution_tests::persistent_state(&db).await,
            "{mode}"
        );
        if let Some(restarted) = restarted {
            restarted.stop();
        }
        live.stop();
        db.finish().await;
    }
}

struct Catalog {
    fail: AtomicBool,
    calls: AtomicU64,
    fail_at: AtomicU64,
    pause: AtomicBool,
    pause_at: AtomicU64,
    revision_at: AtomicU64,
    revoke_at: AtomicU64,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl BackendCardCatalogProvider for Catalog {
    fn authorized_catalog<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(StorageError::DependencyUnavailable);
            }
            Ok(Some(BackendCardCatalog {
                owner: identity.user_id.clone(),
                account: account.clone(),
                source_revision: "isolated_catalog_v1".into(),
                cards: vec![BackendCatalogCard {
                    card_id: "isolated_card".into(),
                    last_four: "1234".into(),
                }],
            }))
        })
    }
    fn current_revision<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(identity.user_id.as_str(), "user_001");
            assert_eq!(account.as_str(), "account_001");
            if self.pause.swap(false, Ordering::SeqCst)
                || call == self.pause_at.load(Ordering::SeqCst)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if self.fail.load(Ordering::SeqCst) || call == self.fail_at.load(Ordering::SeqCst) {
                return Err(StorageError::DependencyUnavailable);
            }
            if call == self.revoke_at.load(Ordering::SeqCst) {
                return Err(StorageError::Session);
            }
            if call == self.revision_at.load(Ordering::SeqCst) {
                return Ok("isolated_catalog_v2".into());
            }
            Ok("isolated_catalog_v1".into())
        })
    }
}
fn session(cookie: &str) -> SessionRef {
    crate::session::token_reference(cookie.split(';').next().unwrap().split_once('=').unwrap().1)
        .unwrap()
}
fn source(provider: Arc<Catalog>) -> CardCatalogSourceBinding {
    CardCatalogSourceBinding::for_reviewed_backend_provider(
        "isolated_only_catalog",
        "test_impl",
        provider,
    )
    .unwrap()
}
fn service<'a>(
    db: &Db,
    server: &'a PaymentServer,
    source: Option<CardCatalogSourceBinding>,
    behavior: MockBehavior,
    level: SecurityLevel,
) -> PaymentExecutionService<'a> {
    PaymentExecutionService::for_project_simulation(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        server.final_runtime.runtime(),
        source,
        ToolSecurityProfile {
            tool_name: ToolName::parse("execute_confirmed_payment").unwrap(),
            tool_schema_version: 1,
            policy_version: PolicyVersion::parse("transfer-funds-business-2026-10-18-v1").unwrap(),
            minimum_level: level,
        },
        PermitConfig::new(Duration::from_secs(30)).unwrap(),
        behavior,
    )
    .unwrap()
}
async fn setup(
    db: &Db,
    server: &Server,
    cookie: &str,
) -> (Value, Arc<Catalog>, CardCatalogSourceBinding) {
    setup_amount(db, server, cookie, "200.00").await
}
async fn setup_amount(
    db: &Db,
    server: &Server,
    cookie: &str,
    amount: &str,
) -> (Value, Arc<Catalog>, CardCatalogSourceBinding) {
    provision_payer(db).await;
    let mut created = draft(server, cookie).await;
    if amount != "200.00" {
        let selected=select(server,cookie,&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":amount}})).await;
        created = run(server, cookie, &selected).await.1;
    }
    let p = prepared(server, cookie, &created).await;
    let p = decided(server, cookie, &p, "CONFIRM").await;
    let mut tx = db.scoped.begin().await.unwrap();
    let recipient = ResourceId::parse("composition_recipient").unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("composition_recipient_owner").unwrap(),
        &recipient,
        &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::provision_payee(
        &mut tx,
        &UserId::parse("user_001").unwrap(),
        &serde_json::from_value(p["snapshot"]["transfer"]["payee_id"].clone()).unwrap(),
        &recipient,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let provider = Arc::new(Catalog {
        fail: AtomicBool::new(false),
        calls: AtomicU64::new(0),
        fail_at: AtomicU64::new(0),
        pause: AtomicBool::new(false),
        pause_at: AtomicU64::new(0),
        revision_at: AtomicU64::new(0),
        revoke_at: AtomicU64::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let binding = source(provider.clone());
    CardCatalogService::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        Some(binding.clone()),
    )
    .initialize(&session(cookie), &ResourceId::parse("account_001").unwrap())
    .await
    .unwrap();
    (p, provider, binding)
}
async fn execute(
    s: &PaymentExecutionService<'_>,
    cookie: &str,
    p: &Value,
) -> Result<safeflow_storage::PaymentExecutionResult, StorageError> {
    s.execute(
        &session(cookie),
        &serde_json::from_value(p["task_id"].clone()).unwrap(),
        &serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap(),
        &serde_json::from_value(p["confirmation_ref"].clone()).unwrap(),
        p["display_ticket"].as_str().unwrap(),
    )
    .await
}
async fn counts(db: &Db) -> (i64, i64, i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM execution_permits),(SELECT count(*) FROM public_execution_requests),(SELECT count(*) FROM transfer_business_orders_v1),(SELECT count(*) FROM transfer_ledger_v1),(SELECT count(*) FROM payment_funds_execution_v1)").fetch_one(&db.scoped).await.unwrap()
}
async fn freeze(conn: &mut sqlx::PgConnection) {
    sqlx::query("SELECT card_save_freeze_v1('composition_lock','isolated_card','USER_SELF',NULL,NULL,true,'composition_freeze_record','user_001','account_001','composition_freeze_operation','USER_REPORT_LOST',NULL,NULL,'composition_freeze_key','composition_reason','CREATED')").execute(conn).await.unwrap();
}
async fn wait_blocked(db: &Db, pid: i32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let blocked: bool = sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1))>0")
            .bind(pid)
            .fetch_one(&db.scoped)
            .await
            .unwrap();
        if blocked {
            println!("actual pg_blocking_pids confirmed card/payment account wait");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "actual lock wait absent"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_freeze_first_commits_before_fresh_payment_read() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let mut freeze_tx = db.scoped.begin().await.unwrap();
    freeze(&mut freeze_tx).await;
    let blocking: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *freeze_tx)
        .await
        .unwrap();
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
    );
    let observe = async {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))",
            )
            .bind(blocking)
            .fetch_one(&db.scoped)
            .await
            .unwrap();
            if count > 0 {
                println!("actual payment wait on uncommitted freeze observed");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        freeze_tx.commit().await.unwrap();
    };
    let (result, _) = tokio::join!(execute(&s, &cookie, &p), observe);
    assert!(result.is_err());
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_payment_protects_empty_lock_set_until_commit() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    provider.pause.store(true, Ordering::SeqCst);
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
    );
    let observe = async {
        provider.entered.notified().await;
        let mut writer = db.scoped.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *writer)
            .await
            .unwrap();
        let handle = tokio::spawn(async move {
            freeze(&mut writer).await;
            writer.commit().await.unwrap();
        });
        wait_blocked(&db, pid).await;
        provider.release.notify_one();
        handle.await.unwrap();
    };
    let (result, _) = tokio::join!(execute(&s, &cookie, &p), observe);
    assert!(result.is_ok());
    assert_eq!(counts(&db).await, (1, 1, 1, 2, 1));
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM card_locks_v1 WHERE state='ACTIVE'")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(active, 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_final_source_failure_rolls_back_funds_permit_and_events() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    let before = checked_state(&db).await;
    let final_guard_call = provider.calls.load(Ordering::SeqCst) + 3;
    provider.fail_at.store(final_guard_call, Ordering::SeqCst);
    assert!(execute(
        &service(
            &db,
            &server,
            Some(binding),
            MockBehavior::Immediate,
            SecurityLevel::Yellow
        ),
        &cookie,
        &p
    )
    .await
    .is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), final_guard_call);
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    assert_eq!(before, checked_state(&db).await);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_session_revoked_during_source_wait_never_consumes() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    provider.pause.store(true, Ordering::SeqCst);
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
    );
    let revoke = async {
        provider.entered.notified().await;
        server.state.0.sessions.revoke(&session(&cookie)).unwrap();
        provider.release.notify_one();
    };
    let (result, _) = tokio::join!(execute(&s, &cookie, &p), revoke);
    assert!(matches!(result, Err(StorageError::Session)));
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_processing_frozen_card_cannot_succeed() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&s, &cookie, &p).await.unwrap();
    let mut tx = db.scoped.begin().await.unwrap();
    freeze(&mut tx).await;
    tx.commit().await.unwrap();
    let before = checked_state(&db).await;
    assert_processing_hold(&db, &p).await;
    let task = serde_json::from_value(p["task_id"].clone()).unwrap();
    let op = serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap();
    assert_eq!(
        s.resolve_simulated_success(&session(&cookie), &task, &op)
            .await
            .err()
            .unwrap(),
        StorageError::FactsConflict
    );
    assert_processing_hold(&db, &p).await;
    assert_eq!(before, checked_state(&db).await);
    assert_eq!(counts(&db).await, (1, 1, 1, 0, 1));
    assert_eq!(
        execute(&s, &cookie, &p)
            .await
            .unwrap()
            .business
            .latest_receipt
            .status,
        ToolResultStatus::Processing
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_altered_ticket_new_session_and_changed_draft_refuse() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
    );
    let mut altered = p.clone();
    altered["display_ticket"] = json!("changed_ticket");
    assert!(execute(&s, &cookie, &altered).await.is_err());
    let new_cookie = server.login().await;
    assert!(execute(&s, &new_cookie, &p).await.is_err());
    let current = ok(query(
        &server,
        &cookie,
        &format!(
            "/api/sim/draft-lifecycles/{}",
            p["task_id"].as_str().unwrap()
        ),
    )
    .await);
    let selected=select(&server,&cookie,&current,"MODIFY_MOCK_DRAFT",json!({"draft_id":p["snapshot"]["transfer"]["draft_id"],"expected_version":p["snapshot"]["transfer"]["draft_version"],"changes":{"amount":"100.00"}})).await;
    run(&server, &cookie, &selected).await;
    assert!(execute(&s, &cookie, &p).await.is_err());
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_current_confirmation_expiry_and_revocation_refuse() {
    for revoke in [false, true] {
        let db = Db::current44().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (p, _, binding) = setup(&db, &server, &cookie).await;
        if revoke {
            sqlx::query("UPDATE server_confirmations SET status='REVOKED',revoked_at=clock_timestamp() WHERE confirmation_ref=$1").bind(p["confirmation_ref"].as_str().unwrap()).execute(&db.scoped).await.unwrap();
            sqlx::query(
                "UPDATE payment_preparations SET invalid_reason='EXPIRED' WHERE operation_id=$1",
            )
            .bind(p["operation"]["operation_id"].as_str().unwrap())
            .execute(&db.scoped)
            .await
            .unwrap();
        } else {
            sqlx::query("UPDATE server_confirmations SET prepared_at=clock_timestamp()-interval '10 seconds',confirmed_at=clock_timestamp()-interval '9 seconds',expires_at=clock_timestamp()-interval '1 second' WHERE confirmation_ref=$1").bind(p["confirmation_ref"].as_str().unwrap()).execute(&db.scoped).await.unwrap();
        }
        assert!(execute(
            &service(
                &db,
                &server,
                Some(binding),
                MockBehavior::Immediate,
                SecurityLevel::Yellow
            ),
            &cookie,
            &p
        )
        .await
        .is_err());
        assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture, not adopted shared-demo content"]
async fn card_payment_composition_amount_threshold_is_yellow_through_1000_and_red_above() {
    for amount in ["100.00", "1000.00", "1000.01"] {
        let db = Db::current44().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (p, _, binding) = setup_amount(&db, &server, &cookie, amount).await;
        let result = execute(
            &service(
                &db,
                &server,
                Some(binding),
                MockBehavior::Immediate,
                SecurityLevel::Yellow,
            ),
            &cookie,
            &p,
        )
        .await;
        if amount == "1000.01" {
            assert!(matches!(result, Err(StorageError::FormalAuthUnavailable)));
            assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
        } else {
            assert_eq!(
                result.unwrap().business.latest_receipt.status,
                ToolResultStatus::Succeeded
            );
            assert_eq!(counts(&db).await, (1, 1, 1, 2, 1));
        }
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture, not actual adopted demo"]
async fn card_payment_composition_yellow_original_anchor_once_and_http_recovery() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM tasks t WHERE task_id=$1")
        .bind(p["task_id"].as_str().unwrap())
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
    );
    let (a, b) = tokio::join!(execute(&s, &cookie, &p), execute(&s, &cookie, &p));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a.replayed, b.replayed);
    assert_eq!(
        a.business.latest_receipt.status,
        ToolResultStatus::Succeeded
    );
    assert_eq!(counts(&db).await, (1, 1, 1, 2, 1));
    let after: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM tasks t WHERE task_id=$1")
        .bind(p["task_id"].as_str().unwrap())
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(before, after);
    let state: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT state,revision,business_operation_ref FROM operations WHERE operation_id=$1",
    )
    .bind(p["operation"]["operation_id"].as_str().unwrap())
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(state, ("DRAFT".into(), 1, None));
    let anchors:(String,String)=sqlx::query_as("SELECT p.lifecycle_anchor,c.lifecycle_anchor FROM execution_permits p JOIN server_confirmations c USING(confirmation_ref)").fetch_one(&db.scoped).await.unwrap();
    assert_eq!(anchors.0, anchors.1);
    provider.fail.store(true, Ordering::SeqCst);
    let calls = provider.calls.load(Ordering::SeqCst);
    assert!(execute(&s, &cookie, &p).await.unwrap().replayed);
    assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
    let original = ok(query(
        &server,
        &cookie,
        p["original_result_path"].as_str().unwrap(),
    )
    .await);
    assert_eq!(original["state"], "SUCCEEDED");
    assert!(original["verified_business_result"].is_object());
    assert_eq!(counts(&db).await, (1, 1, 1, 2, 1));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_missing_source_red_and_outbox_failure_never_leave_writes() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    assert!(matches!(
        execute(
            &service(
                &db,
                &server,
                None,
                MockBehavior::Immediate,
                SecurityLevel::Yellow
            ),
            &cookie,
            &p
        )
        .await,
        Err(StorageError::DependencyUnavailable)
    ));
    assert!(matches!(
        execute(
            &service(
                &db,
                &server,
                Some(binding.clone()),
                MockBehavior::Immediate,
                SecurityLevel::Red
            ),
            &cookie,
            &p
        )
        .await,
        Err(StorageError::FormalAuthUnavailable)
    ));
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    db.scoped.execute("CREATE FUNCTION reject_payment_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM transfer_business_orders_v1) THEN RAISE EXCEPTION 'injected outbox failure after business SQL'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_payment_outbox BEFORE INSERT ON public_audit_outbox FOR EACH ROW EXECUTE FUNCTION reject_payment_outbox()").await.unwrap();
    assert!(execute(
        &service(
            &db,
            &server,
            Some(binding),
            MockBehavior::Immediate,
            SecurityLevel::Yellow
        ),
        &cookie,
        &p
    )
    .await
    .is_err());
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    let balance: i64 = sqlx::query_scalar(
        "SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='account_001'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(balance, 128800);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; common mechanism fixture"]
async fn card_payment_composition_pending_success_reads_fresh_card_locks() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    let s = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    assert_eq!(
        execute(&s, &cookie, &p)
            .await
            .unwrap()
            .business
            .latest_receipt
            .status,
        ToolResultStatus::Processing
    );
    assert_eq!(counts(&db).await, (1, 1, 1, 0, 1));
    let calls = provider.calls.load(Ordering::SeqCst);
    let task = serde_json::from_value(p["task_id"].clone()).unwrap();
    let op = serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap();
    provider.fail.store(true, Ordering::SeqCst);
    assert!(s
        .resolve_simulated_success(&session(&cookie), &task, &op)
        .await
        .is_err());
    assert!(provider.calls.load(Ordering::SeqCst) > calls);
    assert_eq!(counts(&db).await, (1, 1, 1, 0, 1));
    provider.fail.store(false, Ordering::SeqCst);
    let result = s
        .resolve_simulated_success(&session(&cookie), &task, &op)
        .await
        .unwrap();
    assert_eq!(
        result.business.latest_receipt.status,
        ToolResultStatus::Succeeded
    );
    assert_eq!(counts(&db).await, (1, 1, 1, 2, 1));
    assert!(
        s.resolve_simulated_success(&session(&cookie), &task, &op)
            .await
            .unwrap()
            .replayed
    );
    server.stop();
    db.finish().await;
}

// Include the permit, public mapping and adopted card rows omitted by the older
// query-only snapshot helper, so callback failures cannot leave half a payment.
async fn checked_state(db: &Db) -> Vec<Value> {
    let mut state = payment_execution_tests::persistent_state(db).await;
    for table in [
        "execution_permits",
        "payment_funds_execution_v1",
        "card_account_versions_v1",
        "card_catalog_v1",
        "card_locks_v1",
        "public_card_catalog_adoptions_v1",
    ] {
        state.push(sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t"))
            .fetch_one(&db.scoped).await.unwrap());
    }
    state
}

async fn assert_processing_hold(db: &Db, p: &Value) {
    let mut conn = db.scoped.acquire().await.unwrap();
    let accounting = FundsStore::check_accounting(
        &mut conn,
        &UserId::parse("user_001").unwrap(),
        &serde_json::from_value(p["snapshot"]["transfer"]["operation_id"].clone()).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        accounting.record.latest_receipt.status,
        ToolResultStatus::Processing
    );
    assert_eq!(accounting.operation_reserved.as_str(), "200.00");
    assert_eq!(accounting.payer_funds.reserved.as_str(), "200.00");
    assert_eq!(accounting.payer_funds.booked.as_str(), "1288.00");
    assert_eq!(accounting.payer_funds.available.as_str(), "1088.00");
}

#[tokio::test]
#[ignore = "requires isolated PG; new checked callback must preserve original errors and holds"]
async fn card_payment_composition_checked_provider_failures_rollback_submit_and_processing() {
    for processing in [false, true] {
        for mode in ["unavailable", "revision", "revoked"] {
            let db = Db::current44().await;
            let server = server(&db).await;
            let cookie = server.login().await;
            let (p, provider, binding) = setup(&db, &server, &cookie).await;
            let svc = service(
                &db,
                &server,
                Some(binding),
                MockBehavior::Pending,
                SecurityLevel::Yellow,
            );
            if processing {
                execute(&svc, &cookie, &p).await.unwrap();
            }
            let before = checked_state(&db).await;
            let callback = provider.calls.load(Ordering::SeqCst) + 2;
            let (fault, expected) = match mode {
                "unavailable" => (&provider.fail_at, StorageError::DependencyUnavailable),
                "revision" => (&provider.revision_at, StorageError::FactsConflict),
                _ => (&provider.revoke_at, StorageError::Session),
            };
            fault.store(callback, Ordering::SeqCst);
            let result = if processing {
                svc.resolve_simulated_success(
                    &session(&cookie),
                    &serde_json::from_value(p["task_id"].clone()).unwrap(),
                    &serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap(),
                )
                .await
            } else {
                execute(&svc, &cookie, &p).await
            };
            assert_eq!(result.err().unwrap(), expected, "{processing}/{mode}");
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                callback,
                "must fail in checked callback"
            );
            assert_eq!(before, checked_state(&db).await, "{processing}/{mode}");
            if processing {
                assert_processing_hold(&db, &p).await;
            }
            println!(
                "checked callback {mode}, processing={processing}: whole transaction unchanged"
            );
            server.stop();
            db.finish().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; callback must read same-transaction adopted facts after permit consumption"]
async fn card_payment_composition_checked_rereads_adoption_revision_restriction_and_sql_failure() {
    for mode in ["revision", "restriction", "adoption", "sql"] {
        let db = Db::current44().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (p, provider, binding) = setup(&db, &server, &cookie).await;
        let action = match mode {
            "revision" => "UPDATE card_account_versions_v1 SET revision=revision+1 WHERE account_id='account_001';",
            "restriction" => "INSERT INTO card_locks_v1(lock_id,card_id,source,state) VALUES ('checked_fault_lock','isolated_card','USER_SELF','ACTIVE');",
            "adoption" => "INSERT INTO public_card_catalog_adoptions_v1(account_id,owner_id,ordinal,source_id,implementation_revision,source_revision,catalog_digest) SELECT account_id,owner_id,ordinal+1,'unadopted_source',implementation_revision,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id='account_001' ORDER BY ordinal DESC LIMIT 1;",
            _ => "ALTER TABLE public_card_catalog_adoptions_v1 RENAME TO checked_unavailable_adoption;",
        };
        // Disposable SQL fault injection, installed only in this random test
        // schema. It changes facts inside the existing payment transaction after
        // final policy and before the callback; no production callback hooks.
        sqlx::Executor::execute(&db.scoped, format!(
            "CREATE FUNCTION checked_fault() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$ BEGIN {action} RETURN NEW; END $$;
             CREATE TRIGGER checked_fault AFTER UPDATE OF status ON execution_permits FOR EACH ROW WHEN (NEW.status='CONSUMED') EXECUTE FUNCTION checked_fault();"
        ).as_str()).await.unwrap();
        let before = checked_state(&db).await;
        let calls = provider.calls.load(Ordering::SeqCst);
        let svc = service(
            &db,
            &server,
            Some(binding),
            MockBehavior::Immediate,
            SecurityLevel::Yellow,
        );
        let error = tokio::time::timeout(Duration::from_secs(10), execute(&svc, &cookie, &p))
            .await
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(
            error,
            if mode == "sql" {
                StorageError::Database
            } else {
                StorageError::FactsConflict
            },
            "{mode}"
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            calls + 1,
            "reject before callback provider/final guard"
        );
        assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
        assert_eq!(before, checked_state(&db).await, "{mode}");
        println!("same-transaction checked read {mode}: rejected and fully rolled back");
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; dropping checked read must rollback permit consumption or preserve pending hold"]
async fn card_payment_composition_checked_cancellation_releases_locks_and_preserves_state() {
    for processing in [false, true] {
        let db = Db::current44().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (p, provider, binding) = setup(&db, &server, &cookie).await;
        let svc = service(
            &db,
            &server,
            Some(binding),
            MockBehavior::Pending,
            SecurityLevel::Yellow,
        );
        if processing {
            execute(&svc, &cookie, &p).await.unwrap();
        }
        let before = checked_state(&db).await;
        let callback = provider.calls.load(Ordering::SeqCst) + 2;
        provider.pause_at.store(callback, Ordering::SeqCst);
        let run = async {
            if processing {
                svc.resolve_simulated_success(
                    &session(&cookie),
                    &serde_json::from_value(p["task_id"].clone()).unwrap(),
                    &serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap(),
                )
                .await
            } else {
                execute(&svc, &cookie, &p).await
            }
        };
        let mut run = Box::pin(run);
        tokio::select! {
            _ = &mut run => panic!("did not pause in checked callback"),
            entered = tokio::time::timeout(Duration::from_secs(10), provider.entered.notified()) => entered.unwrap(),
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), callback);
        // Force a real conflicting account writer while the checked read is
        // pending, then cancel and require it to acquire the lock after rollback.
        let mut waiter = db.scoped.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *waiter)
            .await
            .unwrap();
        let writer = tokio::spawn(async move {
            sqlx::query("SELECT account_id FROM transfer_accounts_v1 WHERE account_id='account_001' FOR UPDATE").fetch_one(&mut *waiter).await.unwrap();
            waiter.rollback().await.unwrap();
        });
        wait_blocked(&db, pid).await;
        drop(run);
        tokio::time::timeout(Duration::from_secs(10), writer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before, checked_state(&db).await);
        if processing {
            assert_processing_hold(&db, &p).await;
        }
        println!("checked cancellation processing={processing}: writer released, state unchanged");
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; replay/HTTP restart queries never read a later broken source"]
async fn card_payment_composition_checked_original_replay_restart_ignores_later_source_failure() {
    for behavior in [MockBehavior::Immediate, MockBehavior::Pending] {
        let db = Db::current44().await;
        let live = server(&db).await;
        let cookie = live.login().await;
        let (p, provider, binding) = setup(&db, &live, &cookie).await;
        let svc = service(&db, &live, Some(binding), behavior, SecurityLevel::Yellow);
        let first = execute(&svc, &cookie, &p).await.unwrap();
        let mut tx = db.scoped.begin().await.unwrap();
        freeze(&mut tx).await;
        tx.commit().await.unwrap();
        provider.fail.store(true, Ordering::SeqCst);
        let calls = provider.calls.load(Ordering::SeqCst);
        let before = checked_state(&db).await;
        let absent = service(&db, &live, None, behavior, SecurityLevel::Yellow);
        for replay_service in [&svc, &absent] {
            let replay = execute(replay_service, &cookie, &p).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.business, first.business);
        }
        // New API/session state, same persisted database and original endpoint.
        let restarted = server(&db).await;
        let new_cookie = restarted.login().await;
        let original = ok(query(
            &restarted,
            &new_cookie,
            p["original_result_path"].as_str().unwrap(),
        )
        .await);
        assert_eq!(
            original["state"],
            if matches!(behavior, MockBehavior::Pending) {
                "PROCESSING"
            } else {
                "SUCCEEDED"
            }
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
        assert_eq!(before, checked_state(&db).await);
        if matches!(behavior, MockBehavior::Pending) {
            assert_processing_hold(&db, &p).await;
        }
        restarted.stop();
        live.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PG; PROCESSING must read committed freeze after actual account wait"]
async fn card_payment_composition_checked_processing_waits_for_freeze_and_keeps_hold() {
    let db = Db::current44().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    let mut freeze_tx = db.scoped.begin().await.unwrap();
    freeze(&mut freeze_tx).await;
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *freeze_tx)
        .await
        .unwrap();
    let before = payment_execution_tests::persistent_state(&db).await;
    let before_counts = counts(&db).await;
    let observe = async {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))",
            )
            .bind(blocker)
            .fetch_one(&db.scoped)
            .await
            .unwrap();
            if blocked > 0 {
                println!("actual PROCESSING resolution waits on freeze commit");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        freeze_tx.commit().await.unwrap();
    };
    let session = session(&cookie);
    let task = serde_json::from_value(p["task_id"].clone()).unwrap();
    let op = serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap();
    let (result, _) = tokio::join!(svc.resolve_simulated_success(&session, &task, &op), observe);
    assert_eq!(result.err().unwrap(), StorageError::FactsConflict);
    assert_processing_hold(&db, &p).await;
    // The separately committed freeze changes only card facts, never funding or
    // public operation state. Compare all original payment rows independently.
    assert_eq!(before, payment_execution_tests::persistent_state(&db).await);
    assert_eq!(before_counts, counts(&db).await);
    server.stop();
    db.finish().await;
}
