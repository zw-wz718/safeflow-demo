// Reuse the sealed public-contract fixture; never register or execute a business Tool.
use super::*;
use crate::{ExecutionRequest, ExecutionRequestStore, PublicSubmissionReceipt};
use safeflow_contracts::{BusinessOperationRef, IdempotencyKey};

fn request(v: &VerifiedPermitSnapshot) -> ExecutionRequest {
    ExecutionRequest {
        idempotency_key: IdempotencyKey::parse("foundation_original_key").unwrap(),
        business_operation_ref: BusinessOperationRef::parse("foundation_original_business")
            .unwrap(),
        business_version: 7,
        binding: PermitBinding {
            snapshot: v.binding.clone(),
            fingerprint: compute_operation_fingerprint(&v.payload).unwrap(),
        },
        payload: v.payload.clone(),
    }
}

fn requests(db: &Db) -> ExecutionRequestStore {
    ExecutionRequestStore::for_trusted_backend(db.repo.clone(), db.sessions.clone())
}

async fn submit(
    db: &Db,
    p: &StoredExecutionPermit,
    request: &ExecutionRequest,
) -> Result<PublicSubmissionReceipt, StorageError> {
    let id = p.permit_id().clone();
    let binding = p.binding().clone();
    let recorded = request.clone();
    db.store
        .submission_transaction(&db.session, request, move |unit| {
            Box::pin(async move {
                unit.consume(&id, &binding).await?;
                unit.record_submission(&recorded).await?;
                Ok(())
            })
        })
        .await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn overlapping_original_submissions_invoke_callback_and_consume_only_once() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let r = request(&v);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut lock = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(v.binding.task_id.as_str())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let store = db.store.clone();
        let session = db.session.clone();
        let id = p.permit_id().clone();
        let b = p.binding().clone();
        let r = r.clone();
        let recorded = r.clone();
        let calls = calls.clone();
        jobs.push(tokio::spawn(async move {
            store
                .submission_transaction(&session, &r, move |unit| {
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        unit.consume(&id, &b).await?;
                        unit.record_submission(&recorded).await?;
                        Ok(())
                    })
                })
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "WITH RECURSIVE waiters(pid) AS (SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) UNION SELECT a.pid FROM pg_stat_activity a JOIN waiters w ON w.pid=ANY(pg_blocking_pids(a.pid))) SELECT count(*) FROM waiters",
            )
            .bind(pid)
            .fetch_one(&db.admin)
            .await
            .unwrap();
            if waiting == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both submissions must overlap before release");
    lock.commit().await.unwrap();
    let a = jobs.remove(0).await.unwrap().unwrap();
    let b = jobs.remove(0).await.unwrap().unwrap();
    assert_eq!(a, b);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(mapping_count(&db).await, 1);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    db.clock.set(p.execution_permit().expires_at_ms());
    let replay = db
        .store
        .submission_transaction(&db.same_user, &r, |_| {
            Box::pin(async { panic!("replay must never enter work") })
        })
        .await
        .unwrap();
    assert_eq!(replay, a);
    db.finish().await;
}

async fn mapping_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM public_execution_requests")
        .fetch_one(&db.sql)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn reserve_round_trip_is_metadata_only_and_reconnect_reads_original() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let r = request(&v);
    let view = requests(&db).reserve(&db.session, &r).await.unwrap();
    assert_eq!(view.request, r);
    assert_eq!(view.request.business_version, 7);
    assert_eq!(view.current_operation.plan_version, 1);
    assert_eq!(view.current_operation.state, OperationState::Ready);
    assert!(view.first_submission.is_none());
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    assert_eq!(db.count("EXECUTION_REQUEST_RESERVED").await, 1);
    assert_eq!(requests(&db).reserve(&db.session, &r).await.unwrap(), view);
    let reopened = PgRepository::connect_existing(&db.url, &db.schema)
        .await
        .unwrap();
    let restarted =
        ExecutionRequestStore::for_trusted_backend(reopened.clone(), db.sessions.clone());
    assert_eq!(
        restarted
            .by_key(&db.same_user, &r.idempotency_key)
            .await
            .unwrap(),
        view
    );
    assert_eq!(
        restarted
            .by_business_operation(&db.same_user, &r.business_operation_ref)
            .await
            .unwrap(),
        view
    );
    assert_eq!(restarted.replay(&db.same_user, &r).await.unwrap(), None);
    reopened.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn simultaneous_identical_reservations_return_one_mapping_and_audit() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let r = request(&v);
    let mut lock = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(v.binding.task_id.as_str())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let store = requests(&db);
        let session = db.session.clone();
        let r = r.clone();
        jobs.push(tokio::spawn(
            async move { store.reserve(&session, &r).await },
        ));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "WITH RECURSIVE waiters(pid) AS (SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) UNION SELECT a.pid FROM pg_stat_activity a JOIN waiters w ON w.pid=ANY(pg_blocking_pids(a.pid))) SELECT count(*) FROM waiters",
            )
            .bind(pid)
            .fetch_one(&db.admin)
            .await
            .unwrap();
            if waiting == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both reservations must actually overlap at the task lock");
    lock.commit().await.unwrap();
    let a = jobs.remove(0).await.unwrap().unwrap();
    let b = jobs.remove(0).await.unwrap().unwrap();
    assert_eq!(a, b);
    assert_eq!(mapping_count(&db).await, 1);
    assert_eq!(db.count("EXECUTION_REQUEST_RESERVED").await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn same_key_changed_parameters_version_or_original_is_a_conflict() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let r = request(&v);
    let original = requests(&db).reserve(&db.session, &r).await.unwrap();
    for mode in 0..6 {
        let mut changed = r.clone();
        match mode {
            0 => changed.payload.resource_id = ResourceId::parse("another_resource").unwrap(),
            1 => changed.business_version += 1,
            2 => {
                changed.business_operation_ref =
                    BusinessOperationRef::parse("another_original").unwrap()
            }
            3 => changed.payload.schema_version += 1,
            4 => {
                changed.payload.normalized_arguments["label"] = json!("changed original label");
                changed.binding.snapshot.arguments_hash =
                    hash(&changed.payload.normalized_arguments).unwrap();
            }
            _ => changed.binding.snapshot.task_revision += 1,
        }
        changed.binding.fingerprint = compute_operation_fingerprint(&changed.payload).unwrap();
        assert_eq!(
            requests(&db).reserve(&db.session, &changed).await,
            Err(StorageError::ExecutionKeyConflict)
        );
        assert_eq!(
            requests(&db).replay(&db.session, &changed).await,
            Err(StorageError::ExecutionKeyConflict)
        );
    }
    let v2 = db.fixture().await;
    assert_eq!(
        requests(&db).reserve(&db.session, &request(&v2)).await,
        Err(StorageError::ExecutionKeyConflict)
    );
    assert_eq!(
        requests(&db)
            .by_key(&db.session, &r.idempotency_key)
            .await
            .unwrap(),
        original
    );
    assert_eq!(mapping_count(&db).await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn same_business_or_public_operation_cannot_acquire_another_key() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let r = request(&v);
    requests(&db).reserve(&db.session, &r).await.unwrap();
    let mut changed = r.clone();
    changed.idempotency_key = IdempotencyKey::parse("new_key").unwrap();
    assert_eq!(
        requests(&db).reserve(&db.session, &changed).await,
        Err(StorageError::ExecutionOperationConflict)
    );
    changed.business_operation_ref = BusinessOperationRef::parse("new_business").unwrap();
    assert_eq!(
        requests(&db).reserve(&db.session, &changed).await,
        Err(StorageError::ExecutionOperationConflict)
    );
    let other = db.fixture().await;
    changed = request(&other);
    changed.idempotency_key = IdempotencyKey::parse("new_key").unwrap();
    assert_eq!(
        requests(&db).reserve(&db.session, &changed).await,
        Err(StorageError::ExecutionOperationConflict)
    );
    assert_eq!(mapping_count(&db).await, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn ownership_is_current_and_identical_keys_are_scoped_to_owner() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let r = request(&v);
    requests(&db).reserve(&db.session, &r).await.unwrap();
    assert_eq!(
        requests(&db).by_key(&db.other, &r.idempotency_key).await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        requests(&db)
            .by_business_operation(&db.other, &r.business_operation_ref)
            .await,
        Err(StorageError::NotFound)
    );
    assert_eq!(
        requests(&db).reserve(&db.other, &r).await,
        Err(StorageError::NotFound)
    );
    let mut absent = r.clone();
    absent.binding.snapshot.operation_id = new_operation_id();
    absent.payload.operation_id = absent.binding.snapshot.operation_id.clone();
    absent.binding.fingerprint = compute_operation_fingerprint(&absent.payload).unwrap();
    assert_eq!(
        requests(&db).reserve(&db.session, &absent).await,
        Err(StorageError::NotFound)
    );
    db.sessions.bind(&db.session, "user_002", 1);
    let second = request(&db.fixture().await);
    requests(&db).reserve(&db.session, &second).await.unwrap();
    assert_eq!(mapping_count(&db).await, 2);
    assert_eq!(
        requests(&db)
            .by_key(&db.same_user, &r.idempotency_key)
            .await
            .unwrap()
            .request,
        r
    );
    db.sessions.revoke(&db.session);
    assert_eq!(
        requests(&db)
            .by_key(&db.session, &second.idempotency_key)
            .await,
        Err(StorageError::Session)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn first_public_submission_is_frozen_while_query_reads_latest_operation() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let r = request(&v);
    let first = submit(&db, &p, &r).await.unwrap();
    assert_eq!(first.operation.state, OperationState::Submitted);
    assert_eq!(db.status(&p).await, "CONSUMED");
    let mut current = first.operation.clone();
    // Public storage observations only: these events do not prove a bank result.
    for (event, state) in [
        (OperationEvent::OutcomeUnknown, OperationState::Processing),
        (
            OperationEvent::StatusConfirmedSucceeded,
            OperationState::Succeeded,
        ),
    ] {
        current = db
            .storage
            .record_operation_event(
                &db.session,
                &v.binding.task_id,
                &v.binding.operation_id,
                OperationChange {
                    expected_revision: current.revision,
                    event,
                    facts_version: current.facts_version,
                    business_operation_ref: Some(r.business_operation_ref.clone()),
                },
            )
            .await
            .unwrap();
        let view = requests(&db)
            .by_business_operation(&db.same_user, &r.business_operation_ref)
            .await
            .unwrap();
        assert_eq!(view.current_operation.state, state);
        assert_eq!(view.first_submission, Some(first.clone()));
        assert_eq!(
            requests(&db).replay(&db.same_user, &r).await.unwrap(),
            Some(first.clone())
        );
    }
    db.clock.set(p.execution_permit().expires_at_ms());
    db.sessions.revoke(&db.session);
    let reopened = PgRepository::connect_existing(&db.url, &db.schema)
        .await
        .unwrap();
    let restarted =
        ExecutionRequestStore::for_trusted_backend(reopened.clone(), db.sessions.clone());
    let audit_before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        restarted.replay(&db.same_user, &r).await.unwrap(),
        Some(first)
    );
    assert_eq!(
        restarted
            .by_key(&db.same_user, &r.idempotency_key)
            .await
            .unwrap()
            .current_operation,
        current
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events")
            .fetch_one(&db.sql)
            .await
            .unwrap(),
        audit_before
    );
    assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
    reopened.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn callback_failure_rolls_back_mapping_consumption_submission_and_same_connection_sql() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    db.sentinel().await;
    let r = request(&v);
    let id = p.permit_id().clone();
    let b = p.binding().clone();
    let error = db
        .store
        .transaction(&db.session, &v.binding.task_id, move |unit| {
            Box::pin(async move {
                unit.consume(&id, &b).await?;
                unit.record_submission(&r).await?;
                sqlx::query("INSERT INTO foundation_atomicity_sentinel VALUES (1)")
                    .execute(unit.connection()?)
                    .await?;
                Err::<(), _>(StorageError::Invalid("isolated callback failure"))
            })
        })
        .await;
    assert_eq!(
        error,
        Err(StorageError::Invalid("isolated callback failure"))
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(mapping_count(&db).await, 0);
    assert_eq!(db.sentinel_count().await, 0);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    assert_eq!(db.count("EXECUTION_REQUEST_RESERVED").await, 0);
    assert_eq!(
        db.storage
            .recover(&db.session, &v.binding.task_id)
            .await
            .unwrap()
            .operations[0]
            .state,
        OperationState::Ready
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn audit_and_deferred_commit_failures_leave_no_partial_submission() {
    for deferred in [false, true] {
        let db = Db::new().await;
        let v = db.fixture().await;
        let p = db.save(&v).await;
        db.sql.execute("CREATE FUNCTION reject_public_submission() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated failure'; END $$").await.unwrap();
        let trigger = if deferred {
            "CREATE CONSTRAINT TRIGGER fail_public_submission AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.kind='OPERATION_TRANSITIONED') EXECUTE FUNCTION reject_public_submission()"
        } else {
            "CREATE TRIGGER fail_public_submission BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='EXECUTION_REQUEST_RESERVED') EXECUTE FUNCTION reject_public_submission()"
        };
        db.sql.execute(trigger).await.unwrap();
        assert_eq!(
            submit(&db, &p, &request(&v)).await,
            Err(StorageError::Database)
        );
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(mapping_count(&db).await, 0);
        assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn stale_versions_do_not_reserve_or_submit_but_historical_reads_remain_available() {
    for mode in 0..4 {
        let db = Db::new().await;
        let v = db.fixture().await;
        let p = db.save(&v).await;
        let r = request(&v);
        let reserved = requests(&db).reserve(&db.session, &r).await.unwrap();
        let update = match mode {
            0 => "UPDATE tasks SET revision=revision+1 WHERE task_id=$1",
            1 => "UPDATE tasks SET facts_version=facts_version+1 WHERE task_id=$1",
            2 => "UPDATE operations SET revision=revision+1 WHERE task_id=$1",
            _ => "UPDATE tasks SET plan_version=plan_version+1,plan=jsonb_set(plan,'{plan_version}',to_jsonb(plan_version+1)) WHERE task_id=$1",
        };
        sqlx::query(update)
            .bind(v.binding.task_id.as_str())
            .execute(&db.sql)
            .await
            .unwrap();
        assert!(submit(&db, &p, &r).await.is_err());
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(
            requests(&db)
                .by_key(&db.session, &r.idempotency_key)
                .await
                .unwrap()
                .request,
            reserved.request
        );
        assert_eq!(requests(&db).replay(&db.session, &r).await.unwrap(), None);
        let mut new = r.clone();
        new.idempotency_key = IdempotencyKey::parse("different_key").unwrap();
        new.business_operation_ref = BusinessOperationRef::parse("different_business").unwrap();
        assert_eq!(
            requests(&db).reserve(&db.session, &new).await,
            Err(StorageError::ExecutionOperationConflict)
        );
        db.finish().await;
    }
    let db = Db::new().await;
    let v = db.fixture().await;
    let r = request(&v);
    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE task_id=$1")
        .bind(v.binding.task_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        requests(&db).reserve(&db.session, &r).await,
        Err(StorageError::PermitBindingMismatch)
    );
    assert_eq!(mapping_count(&db).await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn lock_wait_rechecks_current_session_for_reservation_and_replay() {
    for replay in [false, true] {
        let db = Db::new().await;
        let v = db.fixture().await;
        let r = request(&v);
        if replay {
            requests(&db).reserve(&db.session, &r).await.unwrap();
        }
        let mut lock = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(v.binding.task_id.as_str())
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let store = requests(&db);
        let session = db.session.clone();
        let job = tokio::spawn(async move {
            if replay {
                store.replay(&session, &r).await.map(|_| ())
            } else {
                store.reserve(&session, &r).await.map(|_| ())
            }
        });
        wait_for_blocked(&db, pid).await;
        db.sessions.bind(&db.session, "user_001", 2);
        lock.commit().await.unwrap();
        assert_eq!(job.await.unwrap(), Err(StorageError::Session));
        assert_eq!(mapping_count(&db).await, if replay { 1 } else { 0 });
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn final_guard_and_immutable_first_receipt_reject_callback_changes() {
    for mode in 0..3 {
        let db = Db::new().await;
        let v = db.fixture().await;
        let p = db.save(&v).await;
        let id = p.permit_id().clone();
        let b = p.binding().clone();
        let r = request(&v);
        let sessions = db.sessions.clone();
        let session = db.session.clone();
        let result = db.store.transaction(&db.session, &v.binding.task_id, move |unit| Box::pin(async move {
            unit.consume(&id, &b).await?;
            unit.record_submission(&r).await?;
            match mode {
                0 => { sqlx::query("UPDATE operations SET facts_version=facts_version+1 WHERE operation_id=$1").bind(b.snapshot.operation_id.as_str()).execute(unit.connection()?).await?; }
                1 => { sqlx::query("UPDATE public_execution_requests SET first_operation=jsonb_set(first_operation,'{revision}',to_jsonb(99::bigint)) WHERE operation_id=$1").bind(b.snapshot.operation_id.as_str()).execute(unit.connection()?).await?; }
                _ => sessions.bind(&session, "user_001", 2),
            }
            Ok(())
        })).await;
        assert_eq!(
            result,
            Err(match mode {
                0 => StorageError::PermitBindingMismatch,
                1 => StorageError::Database,
                _ => StorageError::Session,
            })
        );
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(mapping_count(&db).await, 0);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn invalid_submission_poisoning_and_query_failure_never_report_bank_failure() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let p = db.save(&v).await;
    let r = request(&v);
    let result = db
        .store
        .transaction(&db.session, &v.binding.task_id, move |unit| {
            Box::pin(async move {
                assert_eq!(
                    unit.record_submission(&r).await,
                    Err(StorageError::TransactionAborted)
                );
                Ok(())
            })
        })
        .await;
    assert_eq!(result, Err(StorageError::TransactionAborted));
    assert_eq!(mapping_count(&db).await, 0);
    let id = p.permit_id().clone();
    let b = p.binding().clone();
    assert_eq!(
        db.store
            .submission_transaction(&db.session, &request(&v), move |unit| Box::pin(
                async move {
                    unit.consume(&id, &b).await?;
                    Ok(())
                }
            ))
            .await,
        Err(StorageError::TransactionAborted)
    );
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(mapping_count(&db).await, 0);
    let r = request(&v);
    let first = submit(&db, &p, &r).await.unwrap();
    assert_eq!(
        requests(&db)
            .by_business_operation(
                &db.session,
                &BusinessOperationRef::parse("missing_original").unwrap()
            )
            .await,
        Err(StorageError::NotFound)
    );
    let mut locked = db.sql.begin().await.unwrap();
    locked
        .execute("ALTER TABLE public_execution_requests RENAME TO isolated_temporarily_unavailable")
        .await
        .unwrap();
    // Make the rename visible, then restore it after checking the public error.
    locked.commit().await.unwrap();
    assert_eq!(
        requests(&db).by_key(&db.session, &r.idempotency_key).await,
        Err(StorageError::Database)
    );
    db.sql
        .execute("ALTER TABLE isolated_temporarily_unavailable RENAME TO public_execution_requests")
        .await
        .unwrap();
    let view = requests(&db)
        .by_key(&db.session, &r.idempotency_key)
        .await
        .unwrap();
    assert_eq!(view.current_operation, first.operation);
    assert_eq!(view.current_operation.state, OperationState::Submitted);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn competing_tasks_enforce_owner_key_and_business_uniqueness_before_callback() {
    for same_key in [true, false] {
        let db = Db::new().await;
        let a = db.fixture().await;
        let b = db.fixture().await;
        let original = request(&a);
        let mut competing = request(&b);
        if !same_key {
            competing.idempotency_key = IdempotencyKey::parse("another_original_key").unwrap();
        }
        let mut pending = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *pending)
            .await
            .unwrap();
        let task = lock_task(&mut pending, &a.binding.user_id, &a.binding.task_id)
            .await
            .unwrap();
        crate::execution_request::reserve_locked(&mut pending, &task, &original)
            .await
            .unwrap();
        let store = db.store.clone();
        let session = db.session.clone();
        let job = tokio::spawn(async move {
            store
                .submission_transaction(&session, &competing, |_| {
                    Box::pin(async { panic!("conflicting original must be rejected before work") })
                })
                .await
        });
        wait_for_blocked(&db, pid).await;
        pending.commit().await.unwrap();
        assert_eq!(
            job.await.unwrap(),
            Err(if same_key {
                StorageError::ExecutionKeyConflict
            } else {
                StorageError::ExecutionOperationConflict
            })
        );
        assert_eq!(mapping_count(&db).await, 1);
        assert_eq!(db.count("EXECUTION_REQUEST_RESERVED").await, 1);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn upgrade_from_thirteen_preserves_checksums_existing_records_and_active_permit() {
    let db = Db::unmigrated().await;
    let sql = [
        include_str!("../migrations/0001_recovery_points.sql"),
        include_str!("../migrations/0002_plan_identity_version.sql"),
        include_str!("../migrations/0003_server_confirmations.sql"),
        include_str!("../migrations/0004_execution_permits.sql"),
        include_str!("../migrations/0005_simulation_auth.sql"),
        include_str!("../migrations/0006_security_decisions.sql"),
        include_str!("../migrations/0007_permit_authentication_gates.sql"),
        include_str!("../migrations/0008_decision_microsecond_fence.sql"),
        include_str!("../migrations/0009_read_only_runtime_claims.sql"),
        include_str!("../migrations/0010_read_only_runtime_continuations.sql"),
        include_str!("../migrations/0011_planning_confirmation_preparations.sql"),
        include_str!("../migrations/0012_persistent_mock_draft_execution.sql"),
        include_str!("../migrations/0013_mock_draft_lifecycle_v2.sql"),
    ];
    let migrations = sql
        .iter()
        .enumerate()
        .map(|(i, sql)| {
            sqlx::migrate::Migration::new(
                i as i64 + 1,
                "unchanged historical migration".into(),
                sqlx::migrate::MigrationType::Simple,
                sql.replace("\r\n", "\n").into(),
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
    let v = db.fixture().await;
    // Write the actual pre-scope shape before upgrading, without the current writer.
    let b = &v.binding;
    let id = PermitId::parse(format!("permit_{}", uuid::Uuid::new_v4())).unwrap();
    let binding = PermitBinding {
        snapshot: b.clone(),
        fingerprint: compute_operation_fingerprint(&v.payload).unwrap(),
    };
    sqlx::query("INSERT INTO execution_permits (permit_id,contract_version,decision_id,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,operation_fingerprint,fingerprint_payload,session_digest,session_binding_version,confirmation_ref,auth_grant_ref,issued_at_ms,expires_at_ms,status) VALUES ($1,1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,'ACTIVE')")
        .bind(id.as_str()).bind(v.decision_id.as_str()).bind(b.user_id.as_str()).bind(b.task_id.as_str())
        .bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str()).bind(b.task_revision as i64)
        .bind(b.plan_version as i64).bind(b.operation_revision as i64).bind(b.facts_version as i64).bind(b.arguments_hash.as_str())
        .bind(hash(b).unwrap().as_str()).bind(binding.fingerprint.as_str()).bind(Json(&v.payload)).bind(&v.session_digest)
        .bind(v.session_version as i64).bind(v.confirmation_ref.as_str()).bind("legacy_auth_reference")
        .bind(db.clock.get()).bind(db.clock.get()+15_000).execute(&db.sql).await.unwrap();
    let original: Json<serde_json::Value> =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM execution_permits p WHERE permit_id=$1")
            .bind(id.as_str())
            .fetch_one(&db.sql)
            .await
            .unwrap();
    let before = db
        .storage
        .recover(&db.session, &v.binding.task_id)
        .await
        .unwrap();
    let checksums: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    assert_eq!(
        db.storage
            .recover(&db.session, &v.binding.task_id)
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT version,checksum FROM _sqlx_migrations WHERE version<=13 ORDER BY version"
        )
        .fetch_all(&db.sql)
        .await
        .unwrap(),
        checksums
    );
    db.assert_latest_migration_versions().await;
    // Migration 24 adds this column; it must not invent an anchor for old rows.
    let anchor: Option<String> =
        sqlx::query_scalar("SELECT lifecycle_anchor FROM execution_permits WHERE permit_id=$1")
            .bind(id.as_str())
            .fetch_one(&db.sql)
            .await
            .unwrap();
    assert_eq!(anchor, None);
    let retained: Json<serde_json::Value> = sqlx::query_scalar(
        "SELECT to_jsonb(p)-'execution_scope'-'lifecycle_anchor'-'card_attempt_no' FROM execution_permits p WHERE permit_id=$1",
    )
    .bind(id.as_str())
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert_eq!(retained, original);
    let p = db
        .store
        .read(&db.session, &v.binding.task_id, &id, &binding)
        .await
        .unwrap();
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(mapping_count(&db).await, 0);
    requests(&db)
        .reserve(&db.session, &request(&v))
        .await
        .unwrap();
    assert_eq!(db.count("EXECUTION_REQUEST_RESERVED").await, 1);
    // Unclassified historical common permits keep their original consumption behavior.
    db.consume(&p).await.unwrap();
    db.finish().await;
}
