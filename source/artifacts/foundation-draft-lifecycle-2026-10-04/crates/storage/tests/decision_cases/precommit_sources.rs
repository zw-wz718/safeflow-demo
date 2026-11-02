//! Read actual sources using existing mock-draft Policy; never execute a business Tool.
use super::*;
use safeflow_transfer_integration::runtime_tools::*;

type WriteFixture = (
    RuntimeAssembly,
    DecisionConfig,
    Arc<MockDraftStore<DraftClock>>,
    StoredSecurityDecision,
    ConfirmationRecord,
);

async fn fixture(db: &Db, minimum: SecurityLevel) -> WriteFixture {
    fixture_with_confirmation_ttl(db, minimum, Duration::from_secs(10)).await
}

async fn fixture_with_confirmation_ttl(
    db: &Db,
    minimum: SecurityLevel,
    ttl: Duration,
) -> WriteFixture {
    let store = Arc::new(MockDraftStore::new(db.bank.clone(), DraftClock, 300).unwrap());
    let authority = Arc::new(MockDraftAuthority::new(
        db.sessions.clone(),
        Arc::new(DraftClock),
    ));
    let mut tools = ToolRegistry::new();
    let mut policies = DomainPolicyRegistry::new();
    let mut catalog = ToolSecurityCatalog::new();
    let tool = Arc::new(CreateMockDraftTool::new(store.clone(), authority));
    let definition = tool.definition();
    tools
        .register(support::counted(tool, db.tool_calls.clone()))
        .unwrap();
    policies
        .register(
            Arc::new(CreateMockDraftPolicy::new(store.clone())),
            PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            MOCK_DRAFT_SCHEMA_VERSION,
        )
        .unwrap();
    catalog
        .register(ToolSecurityProfile {
            tool_name: definition.name.clone(),
            tool_schema_version: definition.schema_version,
            policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            minimum_level: minimum,
        })
        .unwrap();
    let assembly = RuntimeBootstrap::build(
        tools,
        policies,
        catalog,
        db.sessions.clone(),
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap();
    let config = DecisionConfig::new(
        Duration::from_secs(20),
        "existing-mock-draft-v1",
        vec![PreflightInputContract {
            tool_name: definition.name,
            tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
            projection_revision: "existing-create-direct-v1".into(),
        }],
    )
    .unwrap();
    let (task, operation) = db.task(MOCK_DRAFT_CREATE_TOOL, json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null})).await;
    let context = ExecutionContext {
        task_id: task.task_id.clone(),
        node_id: operation.node_id.clone(),
        operation_id: operation.operation_id.clone(),
        authenticated_user_id: UserId::parse("user_001").unwrap(),
        plan_version: 1,
        idempotency_key: None,
    };
    store.find_candidates(&context, "小李").await.unwrap();
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        config.clone(),
        db.clock.clone(),
    )
    .unwrap();
    let decision = service
        .evaluate_and_save(
            &db.session,
            &task.task_id,
            &operation.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreAuth,
        )
        .await
        .unwrap();
    let confirmations = ConfirmationService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.sessions.clone(),
        ConfirmationConfig::new(ttl).unwrap(),
        db.clock.clone(),
    );
    let prepared = confirmations
        .prepare(&db.session, &task.task_id, &operation.operation_id)
        .await
        .unwrap();
    let confirmed = confirmations
        .record_explicit_confirmation(
            &db.session,
            &task.task_id,
            &prepared.confirmation_ref,
            &prepared.binding,
        )
        .await
        .unwrap();
    drop(service);
    (assembly, config, store, decision, confirmed)
}

fn declaration(
    db: &Db,
    decision: &StoredSecurityDecision,
    confirmation: &ConfirmationRecord,
    authentication: PreCommitAuthenticationV2,
) -> PreCommitSecurityRequestV2 {
    let binding = &decision.binding;
    PreCommitSecurityRequestV2 {
        contract_version: PRE_COMMIT_SECURITY_CONTRACT_VERSION,
        task_id: binding.task_id.clone(),
        node_id: binding.node_id.clone(),
        operation_id: binding.operation_id.clone(),
        facts_version: u32::try_from(binding.facts_version).unwrap(),
        phase: SecurityPhase::PreCommit,
        tool_name: binding.tool_name.clone(),
        plan_version: binding.plan_version,
        session_ref: db.session.clone(),
        normalized_arguments: json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null}),
        arguments_hash: binding.arguments_hash.clone(),
        idempotency_key: IdempotencyKey::parse("unverified_business_key_declaration").unwrap(),
        confirmation_ref: confirmation.confirmation_ref.clone(),
        authentication,
    }
}
fn required() -> PreCommitAuthenticationV2 {
    PreCommitAuthenticationV2::Required {
        auth_grant_ref: AuthGrantRef::parse("unverified_formal_grant_reference").unwrap(),
    }
}
fn assert_rejected(report: &PreCommitPublicSourceReport, formal_auth_missing: bool) {
    assert_eq!(
        report.contract_version(),
        PRE_COMMIT_PUBLIC_SOURCE_REPORT_VERSION
    );
    assert_eq!(
        report.disposition(),
        PreCommitPublicSourceDisposition::RejectedMissingSources
    );
    assert!(report
        .missing_sources()
        .contains(&MissingPreCommitSource::BusinessIdempotency));
    assert!(report
        .missing_sources()
        .contains(&MissingPreCommitSource::BusinessProjection));
    assert!(report
        .missing_sources()
        .contains(&MissingPreCommitSource::FinalApprovalRules));
    assert_eq!(
        report
            .missing_sources()
            .contains(&MissingPreCommitSource::FormalAuthentication),
        formal_auth_missing
    );
    assert_eq!(
        report
            .preflight_sources()
            .preflight_decision()
            .evidence_kind,
        "PREFLIGHT_ONLY"
    );
    assert_ne!(
        report
            .preflight_sources()
            .preflight_decision()
            .decision
            .outcome,
        SecurityOutcome::ReadyToExecute
    );
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_inspects_real_sources_replays_and_recovers_but_reports_rejection_and_zero_writes() {
    let db = Db::new().await;
    let (assembly, cfg, store, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg.clone(),
        db.clock.clone(),
    )
    .unwrap();
    let request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    let audits = db.count("audit_events").await;
    let bank = db.bank.reads.load(Ordering::SeqCst);
    for _ in 0..2 {
        let report = service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .unwrap();
        assert_rejected(&report, false);
        assert_eq!(report.request_declaration(), &request);
        assert_eq!(
            report.unverified_idempotency_key(),
            &request.idempotency_key
        );
        assert_eq!(report.preflight_sources().preflight_decision(), &decision);
        assert_eq!(report.preflight_sources().confirmation(), &confirmation);
        assert_eq!(decision.phase, SecurityPhase::PreAuth);
        assert_eq!(decision.decision.outcome, SecurityOutcome::ReadyToConfirm);
        assert_eq!(
            decision.decision.required_gates,
            vec![SecurityGate::ExplicitConfirmation]
        );
    }
    drop(service);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect_existing(&url, &db.schema)
        .await
        .unwrap();
    let rebuilt = DecisionService::for_trusted_backend_with_clock(
        repo,
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let report = rebuilt
        .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
        .await
        .unwrap();
    assert_rejected(&report, false);
    assert_eq!(
        rebuilt
            .request_precommit_evidence(&db.session, &request.task_id)
            .await,
        Err(StorageError::DecisionSourcesUnavailable)
    );
    assert_eq!(
        assembly
            .runtime()
            .evaluate_pre_commit_v2_for_backend(request.clone())
            .await
            .err()
            .unwrap()
            .reason_code
            .as_str(),
        "TRUSTED_PRE_COMMIT_VERIFICATION_UNAVAILABLE"
    );
    let permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(permits, 0);
    assert_eq!(db.count("audit_events").await, audits);
    assert_eq!(db.bank.reads.load(Ordering::SeqCst), bank);
    assert_eq!(store.saved_receipt_count().unwrap(), 0);
    drop(rebuilt);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_required_cannot_downgrade_and_a_reference_never_supplies_formal_authentication() {
    for minimum in [SecurityLevel::Red, SecurityLevel::Critical] {
        let db = Db::new().await;
        let (assembly, cfg, store, decision, confirmation) = fixture(&db, minimum).await;
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            assembly.runtime(),
            cfg,
            db.clock.clone(),
        )
        .unwrap();
        let mut request = declaration(&db, &decision, &confirmation, required());
        let report = service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .unwrap();
        assert_rejected(&report, true);
        assert_eq!(report.preflight_sources().preflight_decision(), &decision);
        assert_eq!(
            decision.decision.outcome,
            if minimum == SecurityLevel::Red {
                SecurityOutcome::WaitingAuth
            } else {
                SecurityOutcome::Blocked
            }
        );
        request.authentication = PreCommitAuthenticationV2::NotRequired {};
        assert_eq!(
            service
                .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
                .await
                .err(),
            Some(StorageError::Invalid("PRE_COMMIT authentication gates"))
        );
        assert_eq!(store.saved_receipt_count().unwrap(), 0);
        let permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(permits, 0);
        drop(service);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_extra_authentication_and_isolated_grants_cannot_change_trusted_gate_semantics() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let mut request = declaration(&db, &decision, &confirmation, required());
    assert_eq!(
        service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .err(),
        Some(StorageError::Invalid("PRE_COMMIT authentication gates"))
    );
    request.authentication = PreCommitAuthenticationV2::Required {
        auth_grant_ref: AuthGrantRef::parse(format!("sim_grant_{}", uuid::Uuid::new_v4())).unwrap(),
    };
    assert_eq!(
        service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .err(),
        Some(StorageError::Invalid("PRE_COMMIT request contract"))
    );
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_request_identifiers_versions_and_parameters_are_compared_with_current_storage() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    for field in 0..14 {
        let mut changed = request.clone();
        match field {
            0 => changed.task_id = new_task_id(),
            1 => changed.operation_id = new_operation_id(),
            2 => changed.node_id = NodeId::parse("other").unwrap(),
            3 => changed.tool_name = ToolName::parse("other_tool").unwrap(),
            4 => changed.plan_version += 1,
            5 => changed.facts_version += 1,
            6 => changed.arguments_hash = ArgumentsHash::parse("0".repeat(64)).unwrap(),
            7 => changed.normalized_arguments["amount"] = json!("300.00"),
            8 => {
                changed.normalized_arguments["amount"] = json!("300.00");
                changed.arguments_hash =
                    ArgumentsHash::parse(digest(&changed.normalized_arguments)).unwrap();
            }
            9 => {
                changed.confirmation_ref =
                    ConfirmationRef::parse(format!("confirmation_{}", uuid::Uuid::new_v4()))
                        .unwrap()
            }
            10 => changed.normalized_arguments["amount"] = json!(200.0),
            11 => changed.phase = SecurityPhase::PreAuth,
            12 => changed.contract_version = 1,
            _ => changed.normalized_arguments["unknown"] = json!(true),
        }
        assert!(
            service
                .inspect_precommit_public_sources(&changed, &decision.stored_decision_id)
                .await
                .is_err(),
            "field {field}"
        );
    }
    assert_rejected(
        &service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .unwrap(),
        false,
    );
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_arbitrary_or_decision_request_keys_remain_unverified_and_never_fix_missing_idempotency()
{
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let mut request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    let audits = db.count("audit_events").await;
    for key in [
        "client_chosen_key",
        decision.request_id.as_str(),
        request.operation_id.as_str(),
    ] {
        let mut changed = request.clone();
        changed.idempotency_key = IdempotencyKey::parse(key).unwrap();
        let report = service
            .inspect_precommit_public_sources(&changed, &decision.stored_decision_id)
            .await
            .unwrap();
        assert_rejected(&report, false);
        assert_eq!(report.unverified_idempotency_key().as_str(), key);
    }
    request.idempotency_key = IdempotencyKey::parse("a".repeat(201)).unwrap();
    assert!(service
        .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
        .await
        .is_err());
    let key_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('precommit_request_keys')::text")
            .fetch_one(&db.sql)
            .await
            .unwrap();
    assert_eq!(key_table, None);
    assert_eq!(db.count("audit_events").await, audits);
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_original_user_session_and_session_version_cannot_be_replaced() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    for session in [&db.other, &db.same] {
        let mut changed = request.clone();
        changed.session_ref = session.clone();
        assert!(service
            .inspect_precommit_public_sources(&changed, &decision.stored_decision_id)
            .await
            .is_err());
    }
    db.sessions.bind(&db.session, "user_001", 2);
    assert!(service
        .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
        .await
        .is_err());
    db.sessions.0.write().unwrap().remove(&db.session);
    assert_eq!(
        service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .err(),
        Some(StorageError::Session)
    );
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_task_lock_wait_rechecks_parameters_facts_session_and_expiry() {
    for mode in 0..4 {
        let db = Db::new().await;
        let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            assembly.runtime(),
            cfg,
            db.clock.clone(),
        )
        .unwrap();
        let request = declaration(
            &db,
            &decision,
            &confirmation,
            PreCommitAuthenticationV2::NotRequired {},
        );
        let mut lock = db.sql.begin().await.unwrap();
        let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(request.task_id.as_str())
            .execute(&mut *lock)
            .await
            .unwrap();
        let (result, ()) = tokio::join!(
            service.inspect_precommit_public_sources(&request, &decision.stored_decision_id),
            async {
                wait_blocked(&db, pid).await;
                match mode {
                    0 => {
                        sqlx::query("UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments,amount}','\"300.00\"'),revision=revision+1 WHERE task_id=$1").bind(request.task_id.as_str()).execute(&mut *lock).await.unwrap();
                    }
                    1 => {
                        sqlx::query("UPDATE operations SET facts_version=facts_version+1,revision=revision+1 WHERE operation_id=$1").bind(request.operation_id.as_str()).execute(&mut *lock).await.unwrap();
                    }
                    2 => db.clock.set(confirmation.expires_at.timestamp_millis()),
                    _ => db.sessions.bind(&db.session, "user_001", 2),
                }
                lock.commit().await.unwrap();
            }
        );
        assert!(result.is_err());
        drop(service);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_final_operation_lock_wait_rejects_changes_after_initial_source_reads() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    let mut lock = db.sql.begin().await.unwrap();
    let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT operation_id FROM operations WHERE operation_id=$1 FOR UPDATE")
        .bind(request.operation_id.as_str())
        .execute(&mut *lock)
        .await
        .unwrap();
    let (result, ()) = tokio::join!(
        service.inspect_precommit_public_sources(&request, &decision.stored_decision_id),
        async {
            wait_blocked(&db, pid).await;
            sqlx::query("UPDATE operations SET facts_version=facts_version+1,revision=revision+1 WHERE operation_id=$1").bind(request.operation_id.as_str()).execute(&mut *lock).await.unwrap();
            lock.commit().await.unwrap();
        }
    );
    // The operation's facts changed while the task's facts stayed unchanged.
    assert_eq!(result.err(), Some(StorageError::FactsConflict));
    drop(service);
    db.finish().await;
}

struct ObservingClock {
    clock: Arc<Clock>,
    sessions: Arc<Sessions>,
    session: SessionRef,
    change_at: usize,
    calls: AtomicUsize,
}
impl ConfirmationClock for ObservingClock {
    fn now(&self) -> DateTime<Utc> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.change_at {
            self.sessions.bind(&self.session, "user_001", 2);
        }
        self.clock.now()
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_each_time_observation_including_final_rechecks_original_session() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    let probe = Arc::new(ObservingClock {
        clock: db.clock.clone(),
        sessions: db.sessions.clone(),
        session: db.session.clone(),
        change_at: usize::MAX,
        calls: AtomicUsize::new(0),
    });
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg.clone(),
        probe.clone(),
    )
    .unwrap();
    assert_rejected(
        &service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .unwrap(),
        false,
    );
    let observations = probe.calls.load(Ordering::SeqCst);
    assert!(observations >= 5);
    drop(service);
    for change_at in 1..=observations {
        db.sessions.bind(&db.session, "user_001", 1);
        let clock = Arc::new(ObservingClock {
            clock: db.clock.clone(),
            sessions: db.sessions.clone(),
            session: db.session.clone(),
            change_at,
            calls: AtomicUsize::new(0),
        });
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            assembly.runtime(),
            cfg.clone(),
            clock,
        )
        .unwrap();
        assert_eq!(
            service
                .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
                .await
                .err(),
            Some(StorageError::Session),
            "observation {change_at}"
        );
        drop(service);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_confirmation_decision_revocation_and_terminal_operation_never_produce_a_report() {
    for mutation in [
        "UPDATE server_confirmations SET status='REVOKED',revoked_at=prepared_at",
        "UPDATE security_decisions SET status='REVOKED',ended_at_ms=evaluated_at_ms",
        "UPDATE operations SET state='CANCELLED',revision=revision+1",
    ] {
        let db = Db::new().await;
        let (assembly, cfg, store, decision, confirmation) =
            fixture(&db, SecurityLevel::Green).await;
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            assembly.runtime(),
            cfg,
            db.clock.clone(),
        )
        .unwrap();
        let request = declaration(
            &db,
            &decision,
            &confirmation,
            PreCommitAuthenticationV2::NotRequired {},
        );
        db.sql.execute(mutation).await.unwrap();
        assert!(service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .is_err());
        assert_eq!(store.saved_receipt_count().unwrap(), 0);
        drop(service);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_empty_duplicate_reversed_or_missing_confirmation_gates_reject() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, confirmation) = fixture(&db, SecurityLevel::Green).await;
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        db.clock.clone(),
    )
    .unwrap();
    let request = declaration(
        &db,
        &decision,
        &confirmation,
        PreCommitAuthenticationV2::NotRequired {},
    );
    for gates in [
        vec![],
        vec![
            SecurityGate::ExplicitConfirmation,
            SecurityGate::ExplicitConfirmation,
        ],
        vec![
            SecurityGate::StrongAuthentication,
            SecurityGate::ExplicitConfirmation,
        ],
        vec![SecurityGate::StrongAuthentication],
    ] {
        let mut changed = decision.decision.clone();
        changed.required_gates = gates;
        sqlx::query("UPDATE security_decisions SET decision=$1,decision_digest=$2 WHERE stored_decision_id=$3")
            .bind(sqlx::types::Json(&changed)).bind(digest(&changed)).bind(decision.stored_decision_id.as_str()).execute(&db.sql).await.unwrap();
        assert_eq!(
            service
                .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
                .await
                .err(),
            Some(StorageError::Invalid("PRE_COMMIT authentication gates"))
        );
    }
    drop(service);
    db.finish().await;
}

struct PreciseClock(DateTime<Utc>);
impl ConfirmationClock for PreciseClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}
#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_microsecond_expiry_observation_remains_durable_across_service_rebuild() {
    let db = Db::new().await;
    let (assembly, cfg, _, decision, original) =
        fixture_with_confirmation_ttl(&db, SecurityLevel::Green, Duration::from_micros(1)).await;
    assert_eq!(
        original.expires_at,
        original.prepared_at + chrono::Duration::microseconds(1)
    );
    let request = declaration(
        &db,
        &decision,
        &original,
        PreCommitAuthenticationV2::NotRequired {},
    );
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg.clone(),
        Arc::new(PreciseClock(original.expires_at)),
    )
    .unwrap();
    assert_eq!(
        service
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    drop(service);
    let rebuilt = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        assembly.runtime(),
        cfg,
        Arc::new(PreciseClock(original.prepared_at)),
    )
    .unwrap();
    assert_eq!(
        rebuilt
            .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
            .await
            .err(),
        Some(StorageError::Invalid("decision clock moved backwards"))
    );
    drop(rebuilt);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn v2_missing_database_source_and_assembly_change_return_no_partial_report_or_success_audit()
{
    for mode in 0..2 {
        let db = Db::new().await;
        let (assembly, mut cfg, store, decision, confirmation) =
            fixture(&db, SecurityLevel::Green).await;
        let request = declaration(
            &db,
            &decision,
            &confirmation,
            PreCommitAuthenticationV2::NotRequired {},
        );
        let audits = db.count("audit_events").await;
        if mode == 0 {
            db.sql
                .execute("ALTER TABLE server_confirmations RENAME TO hidden_confirmations")
                .await
                .unwrap();
        } else {
            cfg = DecisionConfig::new(
                Duration::from_secs(20),
                "changed-deployment-v1",
                vec![PreflightInputContract {
                    tool_name: decision.binding.tool_name.clone(),
                    tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
                    policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).unwrap(),
                    projection_revision: "existing-create-direct-v1".into(),
                }],
            )
            .unwrap();
        }
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            assembly.runtime(),
            cfg,
            db.clock.clone(),
        )
        .unwrap();
        assert_eq!(
            service
                .inspect_precommit_public_sources(&request, &decision.stored_decision_id)
                .await
                .err(),
            Some(if mode == 0 {
                StorageError::Database
            } else {
                StorageError::DecisionAssemblyChanged
            })
        );
        assert_eq!(db.count("audit_events").await, audits);
        assert_eq!(store.saved_receipt_count().unwrap(), 0);
        drop(service);
        db.finish().await;
    }
}
