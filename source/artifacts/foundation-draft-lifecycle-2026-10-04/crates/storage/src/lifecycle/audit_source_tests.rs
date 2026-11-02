use super::*;
use safeflow_runtime::StaticSessionResolver;

fn reader(db: &Db) -> LifecycleAuditSourceReader {
    LifecycleAuditSourceReader::for_trusted_backend(db.repository.clone(), db.sessions.clone())
}

async fn execute(db: &Db, confirmed: &LifecycleView) -> LifecycleView {
    db.service()
        .execute(
            &session(),
            &confirmed.task.task_id,
            &confirmed.operation.operation_id,
            &confirmed.confirmation.as_ref().unwrap().confirmation_ref,
            &version(confirmed),
        )
        .await
        .unwrap()
}

async fn select_and_confirm(
    db: &Db,
    view: &LifecycleView,
    action: MockDraftAction,
    parameters: Value,
) -> LifecycleView {
    let service = db.service();
    let selected = service
        .select(
            &session(),
            &view.task.task_id,
            LifecycleSelection {
                selection_ticket: view.selection_ticket.clone(),
                task_revision: view.task.revision,
                plan_version: view.task.plan.plan_version,
                facts_version: view.task.facts_version,
                action,
                parameters,
            },
        )
        .await
        .unwrap();
    let prepared = service
        .prepare(
            &session(),
            &view.task.task_id,
            &selected.operation.operation_id,
        )
        .await
        .unwrap();
    service
        .act(
            &session(),
            &view.task.task_id,
            &selected.operation.operation_id,
            &prepared.confirmation.as_ref().unwrap().confirmation_ref,
            prepared.display_ticket.as_ref().unwrap(),
            PlanningConfirmationAction::Confirm,
        )
        .await
        .unwrap()
}

async fn source_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE details ? 'lifecycle_settled'")
        .fetch_one(&db.sql)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_bind_three_actions_and_original_replays() {
    let db = Db::new().await;
    let create = db.confirmed().await;
    let created = execute(&db, &create).await;
    let first = reader(&db)
        .read_task(&session(), &create.task.task_id)
        .await
        .unwrap();
    assert_eq!(first.records.len(), 1);
    let receipt = created.receipt.as_ref().unwrap();
    let modify = select_and_confirm(&db, &created, MockDraftAction::Modify,
        json!({"draft_id":receipt.draft_id,"expected_version":1,"changes":{"amount":"300.00","remark":""}})).await;
    let modified = execute(&db, &modify).await;
    let receipt = modified.receipt.as_ref().unwrap();
    let stale = db.service().select(&session(), &modified.task.task_id, LifecycleSelection {
        selection_ticket: modified.selection_ticket.clone(), task_revision: modified.task.revision,
        plan_version: modified.task.plan.plan_version, facts_version: modified.task.facts_version,
        action: MockDraftAction::Modify,
        parameters: json!({"draft_id":receipt.draft_id,"expected_version":1,"changes":{"amount":"400.00"}}),
    }).await;
    assert!(matches!(
        stale,
        Err(StorageError::ConfirmationBindingMismatch)
    ));
    assert_eq!(source_count(&db).await, 2);
    let cancel = select_and_confirm(
        &db,
        &modified,
        MockDraftAction::Cancel,
        json!({"draft_id":receipt.draft_id,"expected_version":2}),
    )
    .await;
    let cancelled = execute(&db, &cancel).await;
    let view = reader(&db)
        .read_task(&session(), &create.task.task_id)
        .await
        .unwrap();
    assert_eq!(view.records.len(), 3);
    assert!(view.unresolved_operations.is_empty());
    assert!(view.operations_without_source.is_empty());
    assert!(view
        .records
        .windows(2)
        .all(|w| w[0].sequence < w[1].sequence
            && w[0].event_id != w[1].event_id
            && w[0].operation_id != w[1].operation_id));
    for ((record, execution), (action, business_version)) in view
        .records
        .iter()
        .zip([&created, &modified, &cancelled])
        .zip([
            (MockDraftAction::Create, 1),
            (MockDraftAction::Modify, 2),
            (MockDraftAction::Cancel, 2),
        ])
    {
        let original = execution.receipt.as_ref().unwrap();
        assert!(record.receipt_for_backend() == original);
        assert_eq!(record.task_id, create.task.task_id);
        assert_eq!(record.operation_id, execution.operation.operation_id);
        assert_eq!(record.node_id, execution.operation.node_id);
        assert_eq!(record.tool_name.as_str(), action.tool_name());
        assert_eq!(record.user_id.as_str(), "user_001");
        assert_eq!(record.scope, "MOCK_DRAFT_LIFECYCLE_ONLY");
        assert_eq!(record.source_mode, SOURCE_MODE);
        assert!(record.identity_source.contains("SIMULATION"));
        assert!(record.business_occurred_at.is_none());
        assert_eq!(record.result_captured_at, original.executed_at);
        assert_eq!(record.receipt_digest, hash(original).unwrap());
        assert_eq!(original.action, action.action());
        assert_eq!(original.draft_version, business_version);
        assert_eq!(
            original.result.data["amount"],
            if business_version == 1 {
                "200.00"
            } else {
                "300.00"
            }
        );
        assert_eq!(
            original.draft_id,
            created.receipt.as_ref().unwrap().draft_id
        );
        assert_eq!(
            original.business_operation_id,
            created.receipt.as_ref().unwrap().business_operation_id
        );
        let persisted: (String, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as("SELECT event_id,occurred_at FROM audit_events WHERE sequence=$1")
                .bind(record.sequence)
                .fetch_one(&db.sql)
                .await
                .unwrap();
        assert_eq!(record.event_id.as_str(), persisted.0);
        assert_eq!(record.source_recorded_at, persisted.1);
    }
    assert!(first.records[0] == view.records[0]);
    let calls = db.calls();
    for original in [&create, &modify, &cancel] {
        assert!(execute(&db, original).await.replayed);
    }
    let replayed = reader(&db)
        .read_task(&session(), &create.task.task_id)
        .await
        .unwrap();
    assert!(replayed.records == view.records);
    assert_eq!(db.calls(), calls);
    assert_eq!(source_count(&db).await, 3);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_reconnect_without_runtime_or_business_store() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    execute(&db, &confirmed).await;
    let before = reader(&db)
        .read_task(&session(), &confirmed.task.task_id)
        .await
        .unwrap();
    let calls = db.calls();
    db.repository.close().await;
    let repository = PgRepository::connect(
        &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
        &db.namespace,
    )
    .await
    .unwrap();
    let replacement_session = SessionRef::parse("current_read_session_after_restart").unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(
        replacement_session.clone(),
        safeflow_contracts::UserId::parse("user_001").unwrap(),
    );
    let restored =
        LifecycleAuditSourceReader::for_trusted_backend(repository.clone(), Arc::new(sessions))
            .read_task(&replacement_session, &confirmed.task.task_id)
            .await
            .unwrap();
    assert!(restored.records == before.records);
    assert_eq!(db.calls(), calls);
    assert_eq!(source_count(&db).await, 1);
    repository.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and built SAFEFLOW_AUDIT_SOURCE_EXAMPLE_BIN"]
async fn audit_sources_backend_example_reads_persisted_evidence_and_redacts_body() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    let executed = execute(&db, &confirmed).await;
    let output = std::process::Command::new(
        std::env::var("SAFEFLOW_AUDIT_SOURCE_EXAMPLE_BIN")
            .expect("build lifecycle_audit_sources and provide its executable path"),
    )
    .arg(confirmed.task.task_id.as_str())
    .env(
        "SAFEFLOW_DATABASE_URL",
        std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
    )
    .env("SAFEFLOW_STORAGE_SCHEMA", &db.namespace)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["records"].as_array().unwrap().len(), 1);
    let record = &json["records"][0];
    assert_eq!(
        record["runtime_operation_id"],
        json!(confirmed.operation.operation_id)
    );
    assert_eq!(
        record["draft_id"],
        json!(executed.receipt.as_ref().unwrap().draft_id)
    );
    assert_eq!(record["scope"], "MOCK_DRAFT_LIFECYCLE_ONLY");
    assert!(record["business_occurred_at"].is_null());
    let raw = String::from_utf8(output.stdout).unwrap();
    for sensitive in [
        "user_001",
        "confirmation_ref",
        "session_digest",
        "receipt_digest",
        "payer_account_id",
        "payee_id",
        "remark",
        "pre_auth",
    ] {
        assert!(!raw.contains(sensitive), "example exposed {sensitive}");
    }
    assert_eq!(db.calls(), 1);
    assert_eq!(source_count(&db).await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_authorize_current_session_and_recheck_binding() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    execute(&db, &confirmed).await;
    assert!(matches!(
        reader(&db)
            .read_task(
                &SessionRef::parse("missing").unwrap(),
                &confirmed.task.task_id
            )
            .await,
        Err(StorageError::Session)
    ));
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(
        session(),
        safeflow_contracts::UserId::parse("other_user").unwrap(),
    );
    let other =
        LifecycleAuditSourceReader::for_trusted_backend(db.repository.clone(), Arc::new(sessions));
    assert!(matches!(
        other.read_task(&session(), &confirmed.task.task_id).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        reader(&db).read_task(&session(), &new_task_id()).await,
        Err(StorageError::NotFound)
    ));
    struct ChangingSession(std::sync::atomic::AtomicUsize);
    impl SessionResolver for ChangingSession {
        fn resolve(
            &self,
            reference: &SessionRef,
        ) -> Result<safeflow_contracts::UserId, safeflow_contracts::SecurityError> {
            Ok(self.resolve_binding(reference)?.user_id)
        }
        fn resolve_binding(
            &self,
            _: &SessionRef,
        ) -> Result<SessionBinding, safeflow_contracts::SecurityError> {
            let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SessionBinding {
                user_id: safeflow_contracts::UserId::parse("user_001").unwrap(),
                binding_version: if call < 3 { 1 } else { 2 },
            })
        }
    }
    let changed = LifecycleAuditSourceReader::for_trusted_backend(
        db.repository.clone(),
        Arc::new(ChangingSession(std::sync::atomic::AtomicUsize::new(0))),
    );
    assert!(matches!(
        changed.read_task(&session(), &confirmed.task.task_id).await,
        Err(StorageError::Session)
    ));
    assert_eq!(source_count(&db).await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_preserve_absent_and_rejected_actions_without_backfill() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    let view = reader(&db)
        .read_task(&session(), &confirmed.task.task_id)
        .await
        .unwrap();
    assert!(view.records.is_empty());
    assert!(view.unresolved_operations.is_empty());
    assert_eq!(
        view.operations_without_source,
        vec![confirmed.operation.operation_id.clone()]
    );
    let mut stale = version(&confirmed);
    stale.plan_version += 1;
    assert_eq!(
        db.service()
            .execute(
                &session(),
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                &confirmed.confirmation.as_ref().unwrap().confirmation_ref,
                &stale
            )
            .await
            .err(),
        Some(StorageError::StalePlan)
    );
    let after = reader(&db)
        .read_task(&session(), &confirmed.task.task_id)
        .await
        .unwrap();
    assert!(after.records.is_empty());
    assert_eq!(
        after.operations_without_source,
        view.operations_without_source
    );
    assert_eq!(source_count(&db).await, 0);
    assert_eq!(db.calls(), 0);
    db.finish().await;
}

async fn rollback_at_source(deferred: bool) {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    db.sql.execute("CREATE FUNCTION fail_settlement_source() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'lifecycle_settled' THEN RAISE EXCEPTION 'isolated settlement source failure'; END IF; RETURN NEW; END $$").await.unwrap();
    let trigger = if deferred {
        "CREATE CONSTRAINT TRIGGER fail_settlement_source AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_settlement_source()"
    } else {
        "CREATE TRIGGER fail_settlement_source BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_settlement_source()"
    };
    db.sql.execute(trigger).await.unwrap();
    let result = db
        .service()
        .execute(
            &session(),
            &confirmed.task.task_id,
            &confirmed.operation.operation_id,
            &confirmed.confirmation.as_ref().unwrap().confirmation_ref,
            &version(&confirmed),
        )
        .await;
    assert!(matches!(result, Err(StorageError::Database)));
    let view = reader(&db)
        .read_task(&session(), &confirmed.task.task_id)
        .await
        .unwrap();
    assert!(view.records.is_empty());
    assert_eq!(
        view.unresolved_operations,
        vec![confirmed.operation.operation_id.clone()]
    );
    assert_eq!(source_count(&db).await, 0);
    let row: (String, Option<Value>, String) = sqlx::query_as("SELECT c.status,c.receipt,o.state FROM mock_draft_lifecycle_claims c JOIN operations o USING(operation_id)").fetch_one(&db.sql).await.unwrap();
    assert_eq!(row, ("CLAIMED".into(), None, "DRAFT".into()));
    // Business is still in memory and has already changed; source rollback cannot undo it.
    assert_eq!(db.calls(), 1);
    let replay = execute(&db, &confirmed).await;
    assert!(replay.receipt.is_none());
    assert_eq!(db.calls(), 1);
    assert_eq!(source_count(&db).await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_and_public_receipt_roll_back_at_insert() {
    rollback_at_source(false).await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_and_public_receipt_roll_back_at_commit() {
    rollback_at_source(true).await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn audit_sources_reject_corrupt_or_missing_evidence_without_repair() {
    for mutation in [
        "UPDATE mock_draft_lifecycle_claims SET receipt_digest=repeat('0',64)",
        "UPDATE mock_draft_lifecycle_claims SET source_digest=repeat('0',64)",
        "UPDATE mock_draft_lifecycle_claims SET source=jsonb_set(source,'{session_user}','\"another_user\"')",
        "UPDATE audit_events SET task_revision=task_revision+1 WHERE details ? 'lifecycle_settled'",
        "DELETE FROM audit_events WHERE details ? 'lifecycle_settled'",
    ] {
        let db = Db::new().await;
        let confirmed = db.confirmed().await;
        execute(&db, &confirmed).await;
        let mut tx = db.sql.begin().await.unwrap();
        if mutation.starts_with("DELETE FROM audit_events") {
            // Deliberately corrupt isolated evidence, including its retained Outbox reference.
            sqlx::query("ALTER TABLE public_audit_outbox DISABLE TRIGGER public_audit_outbox_immutable").execute(&mut *tx).await.unwrap();
            sqlx::query("DELETE FROM public_audit_outbox WHERE event_id IN (SELECT event_id FROM audit_events WHERE details ? 'lifecycle_settled')").execute(&mut *tx).await.unwrap();
            sqlx::query("ALTER TABLE public_audit_outbox ENABLE TRIGGER public_audit_outbox_immutable").execute(&mut *tx).await.unwrap();
        }
        sqlx::query(mutation).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let count = source_count(&db).await;
        assert!(matches!(reader(&db).read_task(&session(), &confirmed.task.task_id).await, Err(StorageError::CorruptRecord)), "mutation: {mutation}");
        assert_eq!(source_count(&db).await, count);
        assert_eq!(db.calls(), 1);
        db.finish().await;
    }
}
