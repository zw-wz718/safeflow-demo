//! Foundation mechanics using sealed permits, NOT final funds authorization.
#[path = "payment_execution_tests.rs"]
mod payment_execution;
use super::*;
use crate::{ExecutionRequest, ExecutionRequestStore, PublicAuditOutbox};
use safeflow_contracts::{BusinessOperationRef, IdempotencyKey, OperationId, ToolResultStatus};
use safeflow_transfer_integration::{
    funds_store::FundsStore,
    mock_execution::{MockBehavior, MockOutgoingState, MockResolution, MockTransferRequest},
};

fn resource(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}
fn requests(db: &Db) -> ExecutionRequestStore {
    ExecutionRequestStore::for_trusted_backend(db.repo.clone(), db.sessions.clone())
}
fn outbox(db: &Db) -> PublicAuditOutbox {
    PublicAuditOutbox::for_trusted_backend(db.repo.clone())
}

async fn setup(db: &Db) -> (StoredExecutionPermit, ExecutionRequest, MockTransferRequest) {
    let business = MockTransferRequest {
        task_id: new_task_id(),
        operation_id: OperationId::parse("business-original-id").unwrap(),
        draft_id: resource("public-contract-draft"),
        draft_version: 1,
        payer_account_id: resource("public-contract-payer"),
        payee_id: resource("public-contract-payee"),
        amount: safeflow_bank_gateway::DecimalAmount::parse("2.00").unwrap(),
        currency: safeflow_bank_gateway::CnyCurrency::Cny,
        remark: String::new(),
    };
    let mut v = db
        .fixture_parameters(
            1,
            business.task_id.clone(),
            serde_json::to_value(&business).unwrap(),
            business.draft_id.clone(),
        )
        .await;
    v.execution_scope = VerifiedPermitScope::Transfer;
    super::transfer_sql_tests::public_draft(db, &v).await;
    let request = ExecutionRequest {
        idempotency_key: IdempotencyKey::parse("isolated-original-key").unwrap(),
        business_operation_ref: BusinessOperationRef::parse(business.operation_id.as_str())
            .unwrap(),
        business_version: 1,
        binding: PermitBinding {
            snapshot: v.binding.clone(),
            fingerprint: compute_operation_fingerprint(&v.payload).unwrap(),
        },
        payload: v.payload.clone(),
    };
    (db.save(&v).await, request, business)
}

async fn submit(
    db: &Db,
    p: &StoredExecutionPermit,
    r: &ExecutionRequest,
    b: &MockTransferRequest,
    behavior: MockBehavior,
    result: bool,
) -> Result<PublicSubmissionReceipt, StorageError> {
    let id = p.permit_id().clone();
    let binding = p.binding().clone();
    let r2 = r.clone();
    let b = b.clone();
    db.store
        .submission_transaction(&db.session, r, move |unit| {
            Box::pin(async move {
                unit.consume(&id, &binding).await?;
                FundsStore::submit(
                    unit.connection()?,
                    &binding.snapshot.user_id,
                    &r2.idempotency_key,
                    &b,
                    behavior,
                    MockOutgoingState::Allowed,
                )
                .await
                .map_err(crate::transfer_sql::business_error)?;
                unit.record_submission(&r2).await?;
                if result {
                    unit.record_transfer_result().await?;
                }
                Ok(())
            })
        })
        .await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_first_current_receipts_atomic_results_and_original_replay() {
    for (behavior, state, booked) in [
        (MockBehavior::Immediate, OperationState::Succeeded, "8.00"),
        (MockBehavior::Decline, OperationState::Failed, "10.00"),
        (MockBehavior::Pending, OperationState::Processing, "10.00"),
        (
            MockBehavior::LostReplyAfterCommit,
            OperationState::Succeeded,
            "8.00",
        ),
    ] {
        let db = Db::new().await;
        let (p, r, b) = setup(&db).await;
        let first = submit(&db, &p, &r, &b, behavior, true).await.unwrap();
        assert_eq!(first.operation.state, OperationState::Submitted);
        let view = requests(&db)
            .original_transfer(&db.session, &r.idempotency_key)
            .await
            .unwrap();
        assert_eq!(view.public.first_submission, Some(first.clone()));
        assert_eq!(view.public.current_operation.state, state);
        if matches!(behavior, MockBehavior::LostReplyAfterCommit) {
            assert_eq!(
                view.business.first_receipt.status,
                ToolResultStatus::Processing
            );
            assert_eq!(
                view.business.latest_receipt.status,
                ToolResultStatus::Succeeded
            );
        }
        let events = outbox(&db).pending(100).await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.envelope["public_event"]["details"]["source"] == "transfer-funds-v1"));
        let replay = db
            .store
            .submission_transaction(&db.same_user, &r, |_| {
                Box::pin(async { panic!("no second callback/debit") })
            })
            .await
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(outbox(&db).pending(100).await.unwrap(), events);
        let again = requests(&db)
            .reconcile_transfer(&db.session, &r.idempotency_key)
            .await
            .unwrap();
        assert_eq!(again, view);
        let funds =
            crate::TransferSqlReader::for_trusted_backend(db.repo.clone(), db.sessions.clone())
                .funds(&db.session, &b.payer_account_id)
                .await
                .unwrap();
        assert_eq!(funds.booked.as_str(), booked);
        assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
        let mut conflict = r.clone();
        conflict.business_version += 1;
        assert_eq!(
            db.store
                .submission_transaction(&db.session, &conflict, |_| Box::pin(async {
                    panic!("conflict before work")
                }))
                .await
                .unwrap_err(),
            StorageError::ExecutionKeyConflict
        );
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_unknown_recovery_uses_original_sql_and_survives_repository_restart() {
    for resolution in [MockResolution::Succeed, MockResolution::Fail] {
        let db = Db::new().await;
        let (p, r, b) = setup(&db).await;
        let first = submit(&db, &p, &r, &b, MockBehavior::Pending, true)
            .await
            .unwrap();
        let mut tx = db.sql.begin().await.unwrap();
        FundsStore::resolve(
            &mut tx,
            &r.binding.snapshot.user_id,
            &b.operation_id,
            resolution,
            MockOutgoingState::Allowed,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let reopened = PgRepository::connect_existing(&db.url, &db.schema)
            .await
            .unwrap();
        let service =
            ExecutionRequestStore::for_trusted_backend(reopened.clone(), db.sessions.clone());
        let a = service
            .reconcile_transfer(&db.same_user, &r.idempotency_key)
            .await
            .unwrap();
        let target = if matches!(resolution, MockResolution::Succeed) {
            OperationState::Succeeded
        } else {
            OperationState::Failed
        };
        assert_eq!(a.public.current_operation.state, target);
        assert_eq!(a.public.first_submission, Some(first));
        assert_eq!(
            a.business.first_receipt.status,
            ToolResultStatus::Processing
        );
        assert_eq!(
            service
                .reconcile_transfer(&db.session, &r.idempotency_key)
                .await
                .unwrap(),
            a
        );
        assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
        assert_eq!(
            service
                .original_transfer(&db.other, &r.idempotency_key)
                .await
                .unwrap_err(),
            StorageError::NotFound
        );
        let child=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","permit::tests::integration_batch::process_child","--ignored","--nocapture"])
            .env("SAFEFLOW_BATCH_SCHEMA",&db.schema)
            .env("SAFEFLOW_BATCH_EXPECTED",serde_json::to_string(&serde_json::json!({"state":target,"first":a.business.first_receipt,"latest":a.business.latest_receipt,"pending":outbox(&db).pending(100).await.unwrap().iter().map(|e|serde_json::json!([e.event_id,e.digest])).collect::<Vec<_>>()})).unwrap())
            .output().unwrap();
        assert!(
            child.status.success(),
            "child failed: {} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("BATCH_ORIGINAL_RESULT_OUTBOX_READ_OK")
        );
        reopened.close().await;
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "parent supplies isolated schema; invoked as a new process"]
async fn process_child() {
    let schema = std::env::var("SAFEFLOW_BATCH_SCHEMA").expect("parent schema required");
    let suffix = schema.strip_prefix("sf_permit_test_").unwrap();
    assert_eq!(suffix.len(), 32);
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    let repository = PgRepository::connect_existing(
        &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
        &schema,
    )
    .await
    .unwrap();
    let mut sessions = safeflow_runtime::StaticSessionResolver::new();
    let session = SessionRef::parse("isolated-child-session").unwrap();
    sessions.insert(session.clone(), UserId::parse("user_001").unwrap());
    let store = ExecutionRequestStore::for_trusted_backend(repository.clone(), Arc::new(sessions));
    let view = store
        .original_transfer(
            &session,
            &IdempotencyKey::parse("isolated-original-key").unwrap(),
        )
        .await
        .unwrap();
    let pending = PublicAuditOutbox::for_trusted_backend(repository.clone())
        .pending(100)
        .await
        .unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(&std::env::var("SAFEFLOW_BATCH_EXPECTED").unwrap()).unwrap();
    assert_eq!(
        serde_json::json!({"state":view.public.current_operation.state,"first":view.business.first_receipt,"latest":view.business.latest_receipt,"pending":pending.iter().map(|e|serde_json::json!([e.event_id,e.digest])).collect::<Vec<_>>()}),
        expected
    );
    repository.close().await;
    println!("BATCH_ORIGINAL_RESULT_OUTBOX_READ_OK");
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_upgrade_16_backfills_original_events_and_retains_all_checksums() {
    let db = Db::unmigrated().await;
    let source = sqlx::migrate::Migrator::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"),
    )
    .await
    .unwrap();
    let historical = source
        .iter()
        .filter(|m| m.version <= 16)
        .map(|m| {
            sqlx::migrate::Migration::new(
                m.version,
                m.description.clone(),
                m.migration_type,
                m.sql.replace("\r\n", "\n").into(),
                false,
            )
        })
        .collect();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(historical),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&db.sql)
    .await
    .unwrap();
    let _ = db.fixture().await;
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    let original: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=16 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(before, after);
    let pending = outbox(&db).pending(100).await.unwrap();
    assert_eq!(pending.len() as i64, original);
    assert!(pending.iter().all(|e| e.historical));
    let matching:i64=sqlx::query_scalar("SELECT count(*) FROM public_audit_outbox p JOIN audit_events a USING(event_id) WHERE p.envelope->'public_event'=to_jsonb(a)").fetch_one(&db.sql).await.unwrap();
    assert_eq!(matching, original);
    assert!(db
        .sql
        .execute("UPDATE public_audit_outbox SET receiver='incomplete-ack'")
        .await
        .is_err());
    db.assert_latest_migration_versions().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_business_version_mismatch_and_missing_original_result_refuse_without_writes() {
    let db = Db::new().await;
    let (p, r, b) = setup(&db).await;
    let mut wrong = r.clone();
    wrong.business_version = 2;
    assert_eq!(
        submit(&db, &p, &wrong, &b, MockBehavior::Immediate, true)
            .await
            .unwrap_err(),
        StorageError::PermitBindingMismatch
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    // A funding-shaped public submission without a bank original must roll back.
    let id = p.permit_id().clone();
    let r2 = r.clone();
    let error = db
        .store
        .submission_transaction(&db.session, &r, move |unit| {
            Box::pin(async move {
                unit.consume(&id, &r2.binding).await?;
                unit.record_submission(&r2).await?;
                Ok(())
            })
        })
        .await
        .unwrap_err();
    assert_eq!(error, StorageError::ReconcileRequired);
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(
        requests(&db)
            .by_key(&db.session, &r.idempotency_key)
            .await
            .unwrap_err(),
        StorageError::NotFound
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_funds_public_audit_outbox_and_commit_failures_rollback_together() {
    for failure in ["ledger", "audit", "outbox", "commit"] {
        let db = Db::new().await;
        let (p, r, b) = setup(&db).await;
        let before = outbox(&db).pending(100).await.unwrap();
        db.sql.execute("CREATE FUNCTION batch_reject() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$").await.unwrap();
        db.sql.execute(match failure {
            "ledger"=>"CREATE TRIGGER batch_failure BEFORE INSERT ON transfer_ledger_v1 FOR EACH ROW WHEN (NEW.direction='CREDIT') EXECUTE FUNCTION batch_reject()",
            "audit"=>"CREATE TRIGGER batch_failure BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='OPERATION_TRANSITIONED') EXECUTE FUNCTION batch_reject()",
            "outbox"=>"CREATE TRIGGER batch_failure BEFORE INSERT ON public_audit_outbox FOR EACH ROW WHEN (NEW.envelope->'public_event'->>'kind'='OPERATION_TRANSITIONED') EXECUTE FUNCTION batch_reject()",
            _=>"CREATE CONSTRAINT TRIGGER batch_failure AFTER INSERT ON public_audit_outbox DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION batch_reject()",
        }).await.unwrap();
        assert_eq!(
            submit(&db, &p, &r, &b, MockBehavior::Immediate, true)
                .await
                .unwrap_err(),
            StorageError::Database
        );
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
        assert_eq!(outbox(&db).pending(100).await.unwrap(), before);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_ledger_v1")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        let mappings: i64 = sqlx::query_scalar("SELECT count(*) FROM public_execution_requests")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(mappings, 0);
        let balance:i64=sqlx::query_scalar("SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='public-contract-payer'").fetch_one(&db.sql).await.unwrap();
        assert_eq!(balance, 1000);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_outbox_restart_identical_retry_ack_conflicts_and_immutable_evidence() {
    let db = Db::new().await;
    let _ = db.fixture().await;
    let pending = outbox(&db).pending(100).await.unwrap();
    assert!(!pending.is_empty());
    assert!(pending.iter().all(|p| !p.historical));
    let reopened = PgRepository::connect_existing(&db.url, &db.schema)
        .await
        .unwrap();
    let worker = PublicAuditOutbox::for_trusted_backend(reopened.clone());
    assert_eq!(worker.pending(100).await.unwrap(), pending);
    let p = &pending[0];
    assert_eq!(
        worker
            .acknowledge(
                &p.event_id,
                &ArgumentsHash::parse("0".repeat(64)).unwrap(),
                "isolated-receiver",
                "receipt"
            )
            .await
            .unwrap_err(),
        StorageError::ExecutionKeyConflict
    );
    worker
        .acknowledge(&p.event_id, &p.digest, "isolated-receiver", "receipt")
        .await
        .unwrap();
    worker
        .acknowledge(&p.event_id, &p.digest, "isolated-receiver", "receipt")
        .await
        .unwrap();
    assert_eq!(
        worker
            .acknowledge(&p.event_id, &p.digest, "isolated-receiver", "another")
            .await
            .unwrap_err(),
        StorageError::ExecutionKeyConflict
    );
    assert_eq!(worker.pending(100).await.unwrap(), pending[1..]);
    assert!(db
        .sql
        .execute("UPDATE public_audit_outbox SET envelope='{}'")
        .await
        .is_err());
    assert!(db
        .sql
        .execute("DELETE FROM public_audit_outbox")
        .await
        .is_err());
    reopened.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_unsupported_partial_payload_never_becomes_verified_bank_result() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let r = ExecutionRequest {
        idempotency_key: IdempotencyKey::parse("key").unwrap(),
        business_operation_ref: BusinessOperationRef::parse("business").unwrap(),
        business_version: 1,
        binding: p.binding().clone(),
        payload: v.payload.clone(),
    };
    let id = p.permit_id().clone();
    let r2 = r.clone();
    assert_eq!(
        db.store
            .submission_transaction(&db.session, &r, move |unit| Box::pin(async move {
                unit.consume(&id, &r2.binding).await?;
                unit.record_submission(&r2).await?;
                assert_eq!(
                    unit.record_transfer_result().await.unwrap_err(),
                    StorageError::DependencyUnavailable
                );
                Ok(()) // Swallowing the error must not allow a partial submission.
            }))
            .await
            .unwrap_err(),
        StorageError::TransactionAborted
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn batch_reconcile_read_and_versioned_identity_change_during_wait() {
    let db = Db::new().await;
    let (p, r, b) = setup(&db).await;
    submit(&db, &p, &r, &b, MockBehavior::Pending, true)
        .await
        .unwrap();
    let mut lock = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(b.task_id.as_str())
        .execute(&mut *lock)
        .await
        .unwrap();
    let service = requests(&db);
    let session = db.session.clone();
    let key = r.idempotency_key.clone();
    let job = tokio::spawn(async move { service.reconcile_transfer(&session, &key).await });
    wait_for_blocked(&db, pid).await;
    db.sessions.bind(&db.session, "user_001", 2);
    lock.commit().await.unwrap();
    assert_eq!(job.await.unwrap().unwrap_err(), StorageError::Session);
    db.finish().await;
}
