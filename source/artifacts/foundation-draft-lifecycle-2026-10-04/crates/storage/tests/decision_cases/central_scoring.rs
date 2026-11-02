//! Public-mechanism integration example, not a member business sample.
use super::*;
#[path = "../../../runtime/tests/support/central_risk.rs"]
mod probe;

fn configuration() -> DecisionConfig {
    DecisionConfig::new(
        Duration::from_secs(20),
        "central-probe-v1",
        vec![probe::input_contract()],
    )
    .unwrap()
}
fn assembly(db: &Db, checks: &[&str], calls: Arc<AtomicUsize>) -> RuntimeAssembly {
    probe::assemble(
        db.sessions.clone(),
        SecurityLevel::Green,
        checks,
        vec![probe::count_finding(probe::CHECK, 20)],
        calls,
    )
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn kernel_to_existing_store_nonzero_score_read_replay_concurrency_and_audit_example() {
    let db = Db::new().await;
    let mut setup = db.sql.begin().await.unwrap();
    safeflow_transfer_integration::funds_store::FundsStore::provision_account(
        &mut setup,
        &UserId::parse("user_001").unwrap(),
        &ResourceId::parse("account_001").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("1288.00").unwrap(),
    )
    .await
    .unwrap();
    setup.commit().await.unwrap();
    let funds_sql="SELECT jsonb_build_object('accounts',(SELECT coalesce(jsonb_agg(to_jsonb(a)),'[]') FROM transfer_accounts_v1 a),'funds',(SELECT coalesce(jsonb_agg(to_jsonb(f)),'[]') FROM transfer_fund_orders_v1 f),'ledger',(SELECT coalesce(jsonb_agg(to_jsonb(l)),'[]') FROM transfer_ledger_v1 l))";
    let funds_before: Value = sqlx::query_scalar(funds_sql)
        .fetch_one(&db.sql)
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let a = assembly(&db, &[probe::CHECK], calls.clone());
    let b = assembly(&db, &[probe::CHECK], calls.clone());
    let s = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        a.runtime(),
        configuration(),
        db.clock.clone(),
    )
    .unwrap();
    let other = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        b.runtime(),
        configuration(),
        db.clock.clone(),
    )
    .unwrap();
    let (task, op) = db.task(probe::TOOL, json!({})).await;
    let id = DecisionRequestId::new_server_id();
    let second_id = DecisionRequestId::new_server_id();
    let (one, two) = tokio::join!(
        s.evaluate_and_save(
            &db.session,
            &task.task_id,
            &op.operation_id,
            &id,
            SecurityPhase::PreAuth
        ),
        other.evaluate_and_save(
            &db.session,
            &task.task_id,
            &op.operation_id,
            &second_id,
            SecurityPhase::PreAuth
        )
    );
    let record = one.unwrap();
    assert_eq!(record, two.unwrap());
    for request in [&id, &second_id, &DecisionRequestId::new_server_id()] {
        assert_eq!(
            s.evaluate_and_save(
                &db.session,
                &task.task_id,
                &op.operation_id,
                request,
                SecurityPhase::PreAuth
            )
            .await
            .unwrap(),
            record
        );
    }
    assert_eq!(
        other
            .read(&db.session, &task.task_id, &record.stored_decision_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(record.decision.risk_score, 15);
    assert_eq!(record.decision.risk_tier, RiskTier::Low);
    assert_eq!(record.decision.outcome, SecurityOutcome::Allow);
    let contribution = &record.decision.findings[0];
    assert_eq!(contribution.applied_weight, Some(15));
    assert_eq!(contribution.score_delta, 15);
    assert_eq!(
        contribution.finding.policy_version.as_str(),
        DEMO_RISK_VERSION
    );
    assert_eq!(contribution.finding.event_ids.len(), 20);
    assert_eq!(
        record.assembly_metadata["runtime"]["central_scoring"]["mode"],
        "COMPUTE_AUDIT_ONLY"
    );
    assert_eq!(db.count("security_decisions").await, 1);
    assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 1);
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE kind='SECURITY_DECISION_RECORDED'",
    )
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert_eq!(details["decision_digest"], record.decision_digest.as_str());
    assert_eq!(details["assembly_digest"], record.assembly_digest.as_str());
    assert_eq!(
        record.assembly_metadata["runtime"]["central_scoring"]["version"],
        CENTRAL_SCORING_VERSION
    );
    let permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(permits, 0);
    let state: String = sqlx::query_scalar("SELECT state FROM operations WHERE operation_id=$1")
        .bind(op.operation_id.as_str())
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(state, "DRAFT");
    assert!(s
        .request_precommit_evidence(&db.session, &task.task_id)
        .await
        .is_err());
    let funds_after: Value = sqlx::query_scalar(funds_sql)
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(funds_before, funds_after);
    println!("Kernel -> DecisionService -> read/replay: score={}, weight={:?}, events={}, version={}, one decision and one audit, no Permit",record.decision.risk_score,contribution.applied_weight,contribution.finding.event_ids.len(),contribution.finding.policy_version);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn changed_catalog_content_cannot_reuse_or_reprice_old_snapshot() {
    let db = Db::new().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let old = assembly(&db, &[probe::CHECK], calls.clone());
    let s = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        old.runtime(),
        configuration(),
        db.clock.clone(),
    )
    .unwrap();
    let (task, op) = db.task(probe::TOOL, json!({})).await;
    let id = DecisionRequestId::new_server_id();
    let record = s
        .evaluate_and_save(
            &db.session,
            &task.task_id,
            &op.operation_id,
            &id,
            SecurityPhase::PreAuth,
        )
        .await
        .unwrap();
    let changed = assembly(
        &db,
        &[probe::CHECK, "COMMON_AUTH_FAILURE_10M"],
        calls.clone(),
    );
    let new = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        changed.runtime(),
        configuration(),
        db.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        new.read(&db.session, &task.task_id, &record.stored_decision_id)
            .await
            .err(),
        Some(StorageError::DecisionAssemblyChanged)
    );
    for request in [&id, &DecisionRequestId::new_server_id()] {
        assert!(new
            .evaluate_and_save(
                &db.session,
                &task.task_id,
                &op.operation_id,
                request,
                SecurityPhase::PreAuth
            )
            .await
            .is_err());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        s.read(&db.session, &task.task_id, &record.stored_decision_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(db.count("security_decisions").await, 1);
    // Removing all bindings also changes the catalog; it cannot turn an old scored
    // operation into a new unassessed zero-score snapshot under a fresh request ID.
    let disabled = probe::assemble(
        db.sessions.clone(),
        SecurityLevel::Green,
        &[],
        vec![],
        calls.clone(),
    );
    let disabled_service = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        disabled.runtime(),
        configuration(),
        db.clock.clone(),
    )
    .unwrap();
    assert!(disabled_service
        .evaluate_and_save(
            &db.session,
            &task.task_id,
            &op.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreAuth
        )
        .await
        .is_err());
    let invalidated = new
        .invalidate_if_stale(&db.session, &task.task_id, &record.stored_decision_id)
        .await
        .unwrap();
    assert_eq!(
        invalidated.invalidation_reason,
        Some(DecisionInvalidationReason::AssemblyChanged)
    );
    assert_eq!(invalidated.decision, record.decision);
    db.finish().await;
}

#[tokio::test]
#[ignore = "real PostgreSQL 16 required"]
async fn missing_or_bad_scoring_evidence_never_enters_storage_and_partial_data_is_audited() {
    let db = Db::new().await;
    for bad in [false, true] {
        let mut f = probe::count_finding(probe::CHECK, 20);
        let checks = if bad {
            f.evidence_refs.clear();
            vec![probe::CHECK]
        } else {
            vec![probe::CHECK, "COMMON_AUTH_FAILURE_10M"]
        };
        let a = probe::assemble(
            db.sessions.clone(),
            SecurityLevel::Green,
            &checks,
            vec![f],
            Arc::new(AtomicUsize::new(0)),
        );
        let s = DecisionService::for_trusted_backend_with_clock(
            db.repo.clone(),
            a.runtime(),
            configuration(),
            db.clock.clone(),
        )
        .unwrap();
        let (task, op) = db.task(probe::TOOL, json!({})).await;
        let rejected = s
            .evaluate_and_save(
                &db.session,
                &task.task_id,
                &op.operation_id,
                &DecisionRequestId::new_server_id(),
                SecurityPhase::PreAuth,
            )
            .await
            .unwrap();
        assert_eq!(rejected.decision.outcome, SecurityOutcome::Blocked);
        assert_eq!(
            rejected
                .decision
                .findings
                .last()
                .unwrap()
                .finding
                .reason_code
                .as_str(),
            "CENTRAL_RISK_INVALID_OR_UNAVAILABLE"
        );
        assert!(rejected
            .decision
            .findings
            .iter()
            .all(|f| f.finding.observed_value.is_none() && f.finding.evidence_refs.is_empty()));
        assert_eq!(
            s.read(&db.session, &task.task_id, &rejected.stored_decision_id)
                .await
                .unwrap(),
            rejected
        );
    }
    assert_eq!(db.count("security_decisions").await, 2);
    assert_eq!(db.audits("SECURITY_DECISION_RECORDED").await, 2);
    let a = probe::assemble(
        db.sessions.clone(),
        SecurityLevel::Green,
        &[probe::CHECK],
        vec![],
        Arc::new(AtomicUsize::new(0)),
    );
    let s = DecisionService::for_trusted_backend_with_clock(
        db.repo.clone(),
        a.runtime(),
        configuration(),
        db.clock.clone(),
    )
    .unwrap();
    let (task, op) = db.task(probe::TOOL, json!({})).await;
    let record = s
        .evaluate_and_save(
            &db.session,
            &task.task_id,
            &op.operation_id,
            &DecisionRequestId::new_server_id(),
            SecurityPhase::PreAuth,
        )
        .await
        .unwrap();
    assert_eq!(
        record.decision.findings[0].finding.reason_code.as_str(),
        "MISSING_AUXILIARY_DATA"
    );
    assert_eq!(
        record.decision.findings[1].finding.reason_code.as_str(),
        "RISK_SCORING_PARTIAL_COMPUTE_AUDIT_ONLY"
    );
    assert_eq!(
        s.read(&db.session, &task.task_id, &record.stored_decision_id)
            .await
            .unwrap(),
        record
    );
    db.finish().await;
}
