//! Shared real-PG fixture with decisions.rs; all tests remain opt-in and require configuration.
use super::*;

fn confirmations(db: &Db, ttl: Duration) -> ConfirmationService {
    ConfirmationService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.sessions.clone(),
        ConfirmationConfig::new(ttl).unwrap(),
        db.clock.clone(),
    )
}
async fn confirmed(db: &Db, decision: &StoredSecurityDecision) -> ConfirmationRecord {
    let service = confirmations(db, Duration::from_secs(10));
    let prepared = service
        .prepare(
            &db.session,
            &decision.binding.task_id,
            &decision.binding.operation_id,
        )
        .await
        .unwrap();
    assert_eq!(prepared.binding, decision.binding);
    service
        .record_explicit_confirmation(
            &db.session,
            &prepared.binding.task_id,
            &prepared.confirmation_ref,
            &prepared.binding,
        )
        .await
        .unwrap()
}
async fn check(
    db: &Db,
    decision: &StoredSecurityDecision,
    confirmation: &ConfirmationRecord,
) -> Result<VerifiedPreflightConfirmation, StorageError> {
    db.service()
        .verify_with_confirmation(
            &db.session,
            &decision.binding.task_id,
            &decision.stored_decision_id,
            &confirmation.confirmation_ref,
            &decision.binding,
        )
        .await
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_reads_both_sources_replays_and_recovers_without_authority_or_side_effects() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmation = confirmed(&db, &decision).await;
    let audit_before = db.count("audit_events").await;
    let bank_before = db.bank.reads.load(Ordering::SeqCst);
    for _ in 0..2 {
        let receipt = check(&db, &decision, &confirmation).await.unwrap();
        assert_eq!(
            receipt.contract_version(),
            PREFLIGHT_CONFIRMATION_CHECK_VERSION
        );
        assert_eq!(receipt.preflight_decision(), &decision);
        assert_eq!(receipt.confirmation(), &confirmation);
        assert_eq!(receipt.preflight_decision().evidence_kind, "PREFLIGHT_ONLY");
        assert_ne!(
            receipt.preflight_decision().decision.outcome,
            SecurityOutcome::ReadyToExecute
        );
        assert_eq!(receipt.expires_at(), confirmation.expires_at);
        assert!(receipt.checked_at() < receipt.expires_at());
    }
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repository = PgRepository::connect_existing(&url, &db.schema)
        .await
        .unwrap();
    let service = DecisionService::for_trusted_backend_with_clock(
        repository,
        db.assembly.runtime(),
        config(),
        db.clock.clone(),
    )
    .unwrap();
    assert!(service
        .verify_with_confirmation(
            &db.session,
            &decision.binding.task_id,
            &decision.stored_decision_id,
            &confirmation.confirmation_ref,
            &decision.binding
        )
        .await
        .is_ok());
    assert_eq!(
        service
            .request_precommit_evidence(&db.session, &decision.binding.task_id)
            .await,
        Err(StorageError::DecisionSourcesUnavailable)
    );
    let permits = PermitStore::for_trusted_backend(
        db.repo.clone(),
        db.sessions.clone(),
        PermitConfig::new(Duration::from_secs(1)).unwrap(),
    );
    assert_eq!(
        permits
            .request_issuance(&db.session, &decision.binding.task_id)
            .await
            .err(),
        Some(StorageError::PermitIssuanceUnavailable)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(db.count("audit_events").await, audit_before);
    assert_eq!(db.bank.reads.load(Ordering::SeqCst), bank_before);
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_prepared_revoked_and_missing_confirmation_never_count_as_confirmed() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let service = confirmations(&db, Duration::from_secs(10));
    let prepared = service
        .prepare(
            &db.session,
            &decision.binding.task_id,
            &decision.binding.operation_id,
        )
        .await
        .unwrap();
    assert_eq!(
        check(&db, &decision, &prepared).await.err(),
        Some(StorageError::ConfirmationRequired)
    );
    let confirmed = confirmed(&db, &decision).await;
    service
        .revoke(
            &db.session,
            &decision.binding.task_id,
            &confirmed.confirmation_ref,
        )
        .await
        .unwrap();
    assert_eq!(
        check(&db, &decision, &confirmed).await.err(),
        Some(StorageError::ConfirmationRevoked)
    );
    let missing = ConfirmationRef::parse(format!("confirmation_{}", uuid::Uuid::new_v4())).unwrap();
    assert_eq!(
        db.service()
            .verify_with_confirmation(
                &db.session,
                &decision.binding.task_id,
                &decision.stored_decision_id,
                &missing,
                &decision.binding
            )
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_enforces_original_user_session_and_exact_expected_binding() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmation = confirmed(&db, &decision).await;
    for session in [&db.same, &db.other] {
        assert_eq!(
            db.service()
                .verify_with_confirmation(
                    session,
                    &decision.binding.task_id,
                    &decision.stored_decision_id,
                    &confirmation.confirmation_ref,
                    &decision.binding
                )
                .await
                .err(),
            Some(StorageError::NotFound)
        );
    }
    let other_decision = db.saved().await;
    let other_confirmation = confirmed(&db, &other_decision).await;
    assert_eq!(
        check(&db, &decision, &other_confirmation).await.err(),
        Some(StorageError::NotFound)
    );
    for field in 0..10 {
        let mut binding = decision.binding.clone();
        match field {
            0 => binding.user_id = UserId::parse("user_002").unwrap(),
            1 => binding.task_id = new_task_id(),
            2 => binding.operation_id = new_operation_id(),
            3 => binding.node_id = NodeId::parse("different").unwrap(),
            4 => binding.tool_name = ToolName::parse("different").unwrap(),
            5 => binding.task_revision += 1,
            6 => binding.plan_version += 1,
            7 => binding.operation_revision += 1,
            8 => binding.facts_version += 1,
            _ => binding.arguments_hash = ArgumentsHash::parse("f".repeat(64)).unwrap(),
        }
        assert_eq!(
            db.service()
                .verify_with_confirmation(
                    &db.session,
                    &decision.binding.task_id,
                    &decision.stored_decision_id,
                    &confirmation.confirmation_ref,
                    &binding
                )
                .await
                .err(),
            Some(StorageError::DecisionBindingMismatch)
        );
    }
    db.sessions.bind(&db.session, "user_001", 2);
    assert_eq!(
        check(&db, &decision, &confirmation).await.err(),
        Some(StorageError::NotFound)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_same_task_different_operation_confirmation_cannot_be_substituted() {
    let db = Db::new().await;
    let storage = PersistenceService::for_trusted_backend(db.repo.clone(), db.sessions.clone());
    let task = storage
        .create_task(
            &db.session,
            AgentPlan::new(
                new_task_id(),
                1,
                ["first", "second"]
                    .into_iter()
                    .map(|name| PlanNode::Tool {
                        id: NodeId::parse(name).unwrap(),
                        tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
                        arguments: json!({"account_id":"account_001"}),
                        depends_on: vec![],
                    })
                    .collect(),
            )
            .unwrap(),
            1,
        )
        .await
        .unwrap();
    let first = storage
        .create_operation(
            &db.session,
            &task.task_id,
            task.revision,
            new_operation_id(),
            NodeId::parse("first").unwrap(),
            1,
        )
        .await
        .unwrap();
    let task = storage
        .recover(&db.session, &task.task_id)
        .await
        .unwrap()
        .task;
    let second = storage
        .create_operation(
            &db.session,
            &task.task_id,
            task.revision,
            new_operation_id(),
            NodeId::parse("second").unwrap(),
            1,
        )
        .await
        .unwrap();
    let service = db.service();
    let decision = service
        .evaluate_and_save(
            &db.session,
            &task.task_id,
            &first.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool,
        )
        .await
        .unwrap();
    let other = service
        .evaluate_and_save(
            &db.session,
            &task.task_id,
            &second.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreTool,
        )
        .await
        .unwrap();
    let other_confirmation = confirmed(&db, &other).await;
    assert_eq!(
        check(&db, &decision, &other_confirmation).await.err(),
        Some(StorageError::ConfirmationBindingMismatch)
    );
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_rejects_decision_revocation_invalidation_and_assembly_change() {
    let db = Db::new().await;
    for status in ["REVOKED", "INVALIDATED"] {
        let decision = db.saved().await;
        let confirmation = confirmed(&db, &decision).await;
        sqlx::query("UPDATE security_decisions SET status=$1,ended_at_ms=evaluated_at_ms,invalidation_reason=CASE WHEN $1='INVALIDATED' THEN 'SNAPSHOT_CHANGED' ELSE NULL END WHERE stored_decision_id=$2")
            .bind(status).bind(decision.stored_decision_id.as_str()).execute(&db.sql).await.unwrap();
        assert_eq!(
            check(&db, &decision, &confirmation).await.err(),
            Some(StorageError::DecisionInactive)
        );
    }
    let decision = db.saved().await;
    let confirmation = confirmed(&db, &decision).await;
    let changed = DecisionConfig::new(
        Duration::from_secs(20),
        "changed-deployment",
        support::input_contracts(),
    )
    .unwrap();
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.assembly.runtime(),
        changed,
        db.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        service
            .verify_with_confirmation(
                &db.session,
                &decision.binding.task_id,
                &decision.stored_decision_id,
                &confirmation.confirmation_ref,
                &decision.binding
            )
            .await
            .err(),
        Some(StorageError::DecisionAssemblyChanged)
    );
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_checks_both_expiry_boundaries_and_durable_clock_rollback() {
    for confirmation_first in [true, false] {
        let db = Db::new().await;
        let decision = db.saved().await;
        let service = confirmations(
            &db,
            Duration::from_secs(if confirmation_first { 10 } else { 30 }),
        );
        let prepared = service
            .prepare(
                &db.session,
                &decision.binding.task_id,
                &decision.binding.operation_id,
            )
            .await
            .unwrap();
        let confirmation = service
            .record_explicit_confirmation(
                &db.session,
                &decision.binding.task_id,
                &prepared.confirmation_ref,
                &prepared.binding,
            )
            .await
            .unwrap();
        let expiry = if confirmation_first {
            confirmation.expires_at.timestamp_millis()
        } else {
            decision.expires_at_ms
        };
        db.clock.set(expiry);
        assert_eq!(
            check(&db, &decision, &confirmation).await.err(),
            Some(if confirmation_first {
                StorageError::ConfirmationExpired
            } else {
                StorageError::DecisionExpired
            })
        );
        db.clock.set(expiry - 1);
        assert!(matches!(
            check(&db, &decision, &confirmation).await.err(),
            Some(StorageError::Invalid(_))
        ));
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_task_lock_wait_rechecks_session_expiry_and_current_database_snapshot() {
    for mode in 0..5 {
        let db = Db::new().await;
        let decision = db.saved().await;
        let confirmation = confirmed(&db, &decision).await;
        let (mut lock, pid) = blocker(&db, false).await;
        let (result, ()) = tokio::join!(check(&db, &decision, &confirmation), async {
            wait_blocked(&db, pid).await;
            match mode {
                0 => db.sessions.bind(&db.session, "user_001", 2),
                1 => db.clock.set(confirmation.expires_at.timestamp_millis()),
                2 => {
                    sqlx::query("UPDATE tasks SET revision=revision+1")
                        .execute(&mut *lock)
                        .await
                        .unwrap();
                }
                3 => {
                    sqlx::query(
                        "UPDATE operations SET facts_version=facts_version+1,revision=revision+1",
                    )
                    .execute(&mut *lock)
                    .await
                    .unwrap();
                }
                _ => {
                    sqlx::query("UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments,account_id}','\"account_002\"'),revision=revision+1").execute(&mut *lock).await.unwrap();
                }
            }
            lock.commit().await.unwrap();
        });
        assert!(result.is_err());
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_confirmation_lock_wait_rechecks_session_expiry_and_revocation() {
    for mode in 0..3 {
        let db = Db::new().await;
        let decision = db.saved().await;
        let confirmation = confirmed(&db, &decision).await;
        let mut lock = db.sql.begin().await.unwrap();
        let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT confirmation_ref FROM server_confirmations FOR UPDATE")
            .fetch_all(&mut *lock)
            .await
            .unwrap();
        let (result, ()) = tokio::join!(check(&db, &decision, &confirmation), async {
            wait_blocked(&db, pid).await;
            match mode {
                0 => {
                    db.sessions.0.write().unwrap().remove(&db.session);
                }
                1 => db.clock.set(confirmation.expires_at.timestamp_millis()),
                _ => {
                    sqlx::query(
                        "UPDATE server_confirmations SET status='REVOKED',revoked_at=prepared_at",
                    )
                    .execute(&mut *lock)
                    .await
                    .unwrap();
                }
            }
            lock.commit().await.unwrap();
        });
        assert_eq!(
            result.err(),
            Some(match mode {
                0 => StorageError::Session,
                1 => StorageError::ConfirmationExpired,
                _ => StorageError::ConfirmationRevoked,
            })
        );
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_rechecks_session_at_every_time_observation_including_the_last() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmation = confirmed(&db, &decision).await;
    let probe = Arc::new(ObservingClock {
        source: db.clock.clone(),
        sessions: db.sessions.clone(),
        session: db.session.clone(),
        calls: AtomicUsize::new(0),
        change_at: usize::MAX,
        rollback: false,
    });
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.assembly.runtime(),
        config(),
        probe.clone(),
    )
    .unwrap();
    assert!(service
        .verify_with_confirmation(
            &db.session,
            &decision.binding.task_id,
            &decision.stored_decision_id,
            &confirmation.confirmation_ref,
            &decision.binding
        )
        .await
        .is_ok());
    let observations = probe.calls.load(Ordering::SeqCst);
    assert!(observations >= 5);
    for change_at in 1..=observations {
        db.sessions.bind(&db.session, "user_001", 1);
        let clock = Arc::new(ObservingClock {
            source: db.clock.clone(),
            sessions: db.sessions.clone(),
            session: db.session.clone(),
            calls: AtomicUsize::new(0),
            change_at,
            rollback: false,
        });
        let service = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            db.assembly.runtime(),
            config(),
            clock,
        )
        .unwrap();
        assert_eq!(
            service
                .verify_with_confirmation(
                    &db.session,
                    &decision.binding.task_id,
                    &decision.stored_decision_id,
                    &confirmation.confirmation_ref,
                    &decision.binding
                )
                .await
                .err(),
            Some(StorageError::Session)
        );
    }
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_transaction_read_failure_returns_no_receipt_or_success_audit() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmation = confirmed(&db, &decision).await;
    let before = db.count("audit_events").await;
    db.sql
        .execute("ALTER TABLE server_confirmations RENAME TO hidden_confirmations")
        .await
        .unwrap();
    assert_eq!(
        check(&db, &decision, &confirmation).await.err(),
        Some(StorageError::Database)
    );
    db.sql
        .execute("ALTER TABLE hidden_confirmations RENAME TO server_confirmations")
        .await
        .unwrap();
    assert_eq!(db.count("audit_events").await, before);
    assert!(check(&db, &decision, &confirmation).await.is_ok());
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_final_operation_lock_wait_rereads_changed_snapshot() {
    for mutation in [
        "revision=revision+1",
        "facts_version=facts_version+1,revision=revision+1",
        "arguments_hash=repeat('a',64),revision=revision+1",
    ] {
        let db = Db::new().await;
        let decision = db.saved().await;
        let confirmation = confirmed(&db, &decision).await;
        let before = db.count("audit_events").await;
        let mut lock = db.sql.begin().await.unwrap();
        let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT operation_id FROM operations FOR UPDATE")
            .fetch_all(&mut *lock)
            .await
            .unwrap();
        let (result, ()) = tokio::join!(check(&db, &decision, &confirmation), async {
            wait_blocked(&db, pid).await;
            sqlx::query(&format!("UPDATE operations SET {mutation}"))
                .execute(&mut *lock)
                .await
                .unwrap();
            lock.commit().await.unwrap();
        });
        assert!(result.is_err());
        assert_eq!(db.count("audit_events").await, before);
        db.finish().await;
    }
}

struct FinalExpiryClock {
    before: DateTime<Utc>,
    expires: DateTime<Utc>,
    change_at: usize,
    calls: AtomicUsize,
}
impl ConfirmationClock for FinalExpiryClock {
    fn now(&self) -> DateTime<Utc> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 >= self.change_at {
            self.expires
        } else {
            self.before
        }
    }
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_final_time_rechecks_confirmation_expiry_at_exact_microsecond() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmation_service = confirmations(&db, Duration::from_micros(1));
    let prepared = confirmation_service
        .prepare(
            &db.session,
            &decision.binding.task_id,
            &decision.binding.operation_id,
        )
        .await
        .unwrap();
    let confirmation = confirmation_service
        .record_explicit_confirmation(
            &db.session,
            &decision.binding.task_id,
            &prepared.confirmation_ref,
            &prepared.binding,
        )
        .await
        .unwrap();
    assert_eq!(
        confirmation.prepared_at.timestamp_millis(),
        confirmation.expires_at.timestamp_millis()
    );
    let probe = Arc::new(ObservingClock {
        source: db.clock.clone(),
        sessions: db.sessions.clone(),
        session: db.session.clone(),
        calls: AtomicUsize::new(0),
        change_at: usize::MAX,
        rollback: false,
    });
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.assembly.runtime(),
        config(),
        probe.clone(),
    )
    .unwrap();
    assert!(service
        .verify_with_confirmation(
            &db.session,
            &decision.binding.task_id,
            &decision.stored_decision_id,
            &confirmation.confirmation_ref,
            &decision.binding
        )
        .await
        .is_ok());
    let clock = Arc::new(FinalExpiryClock {
        before: confirmation.prepared_at,
        expires: confirmation.expires_at,
        change_at: probe.calls.load(Ordering::SeqCst),
        calls: AtomicUsize::new(0),
    });
    let final_service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.assembly.runtime(),
        config(),
        clock,
    )
    .unwrap();
    assert_eq!(
        final_service
            .verify_with_confirmation(
                &db.session,
                &decision.binding.task_id,
                &decision.stored_decision_id,
                &confirmation.confirmation_ref,
                &decision.binding
            )
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    drop(final_service);
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_submitted_processing_and_terminal_operations_remain_unavailable() {
    let db = Db::new().await;
    for state in ["SUBMITTED", "PROCESSING", "SUCCEEDED", "FAILED"] {
        let decision = db.saved().await;
        let confirmation = confirmed(&db, &decision).await;
        sqlx::query("UPDATE operations SET state=$1,revision=revision+1,business_operation_ref='unconnected_reference' WHERE operation_id=$2")
            .bind(state).bind(decision.binding.operation_id.as_str()).execute(&db.sql).await.unwrap();
        assert!(check(&db, &decision, &confirmation).await.is_err());
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_provenance_check_never_promotes_a_valid_denial_to_approval() {
    let db = Db::new().await;
    let (task, operation) = db
        .task(GET_ACCOUNT_BALANCE, json!({"account_id":"account_other"}))
        .await;
    let service = db.service();
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
    assert_eq!(decision.decision.outcome, SecurityOutcome::Blocked);
    let confirmation = confirmed(&db, &decision).await;
    let receipt = check(&db, &decision, &confirmation).await.unwrap();
    assert_eq!(
        receipt.preflight_decision().decision.outcome,
        SecurityOutcome::Blocked
    );
    assert_eq!(receipt.preflight_decision().phase, SecurityPhase::PreAuth);
    assert_eq!(
        service
            .request_precommit_evidence(&db.session, &task.task_id)
            .await,
        Err(StorageError::DecisionSourcesUnavailable)
    );
    drop(service);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn combination_expired_microsecond_observation_survives_rebuild_and_same_millisecond_rollback(
) {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmations = confirmations(&db, Duration::from_micros(1));
    let prepared = confirmations
        .prepare(
            &db.session,
            &decision.binding.task_id,
            &decision.binding.operation_id,
        )
        .await
        .unwrap();
    let confirmation = confirmations
        .record_explicit_confirmation(
            &db.session,
            &decision.binding.task_id,
            &prepared.confirmation_ref,
            &prepared.binding,
        )
        .await
        .unwrap();
    assert_eq!(
        confirmation.expires_at,
        confirmation.prepared_at + chrono::Duration::microseconds(1)
    );
    let expired_clock = Arc::new(FinalExpiryClock {
        before: confirmation.expires_at,
        expires: confirmation.expires_at,
        change_at: 0,
        calls: AtomicUsize::new(0),
    });
    let service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        db.assembly.runtime(),
        config(),
        expired_clock,
    )
    .unwrap();
    let audit_before = db.count("audit_events").await;
    assert_eq!(
        service
            .verify_with_confirmation(
                &db.session,
                &decision.binding.task_id,
                &decision.stored_decision_id,
                &confirmation.confirmation_ref,
                &decision.binding
            )
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    assert_eq!(
        confirmation.expires_at.timestamp_millis(),
        confirmation.prepared_at.timestamp_millis()
    );
    drop(service);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repository = PgRepository::connect_existing(&url, &db.schema)
        .await
        .unwrap();
    let rollback_clock = Arc::new(FinalExpiryClock {
        before: confirmation.prepared_at,
        expires: confirmation.prepared_at,
        change_at: 0,
        calls: AtomicUsize::new(0),
    });
    let rebuilt = DecisionService::for_trusted_backend_with_clock(
        repository,
        db.assembly.runtime(),
        config(),
        rollback_clock,
    )
    .unwrap();
    assert_eq!(
        rebuilt
            .verify_with_confirmation(
                &db.session,
                &decision.binding.task_id,
                &decision.stored_decision_id,
                &confirmation.confirmation_ref,
                &decision.binding
            )
            .await
            .err(),
        Some(StorageError::Invalid("decision clock moved backwards"))
    );
    let fence: (i64, i64) = sqlx::query_as(
        "SELECT last_observed_ms,last_observed_us FROM security_decision_time_fence WHERE scope=1",
    )
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert_eq!(
        fence,
        (
            confirmation.expires_at.timestamp_millis(),
            confirmation.expires_at.timestamp_micros()
        )
    );
    assert_eq!(db.count("audit_events").await, audit_before);
    drop(rebuilt);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn migration_eight_preserves_old_checksums_records_and_millisecond_fence_conservatively() {
    let db = Db::new().await;
    let decision = db.saved().await;
    let confirmation = confirmed(&db, &decision).await;
    let previous_ms = db.clock.0.load(Ordering::SeqCst);
    let checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=7 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(checksums.len(), 7);
    db.sql.execute("ALTER TABLE security_decision_time_fence DROP COLUMN last_observed_us; DELETE FROM _sqlx_migrations WHERE version=8").await.unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=7 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(after, checksums);
    let fence: (i64, i64) = sqlx::query_as(
        "SELECT last_observed_ms,last_observed_us FROM security_decision_time_fence WHERE scope=1",
    )
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert_eq!(fence, (previous_ms, previous_ms * 1000 + 999));
    assert_eq!(
        check(&db, &decision, &confirmation).await.err(),
        Some(StorageError::Invalid("decision clock moved backwards"))
    );
    db.clock.set(previous_ms + 1);
    let receipt = check(&db, &decision, &confirmation).await.unwrap();
    assert_eq!(receipt.preflight_decision(), &decision);
    assert_eq!(receipt.confirmation(), &confirmation);
    for value in ["-1", "NULL"] {
        assert!(db
            .sql
            .execute(
                format!("UPDATE security_decision_time_fence SET last_observed_us={value}")
                    .as_str()
            )
            .await
            .is_err());
    }
    sqlx::query("UPDATE security_decision_time_fence SET last_observed_ms=$1")
        .bind(previous_ms + 2)
        .execute(&db.sql)
        .await
        .unwrap();
    db.clock.set(previous_ms + 2);
    assert_eq!(
        check(&db, &decision, &confirmation).await.err(),
        Some(StorageError::Invalid("decision clock moved backwards"))
    );
    db.clock.set(previous_ms + 3);
    assert!(check(&db, &decision, &confirmation).await.is_ok());
    db.finish().await;
}
