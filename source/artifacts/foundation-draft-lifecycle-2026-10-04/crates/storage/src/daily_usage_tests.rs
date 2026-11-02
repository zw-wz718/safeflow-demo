//! Public mechanism fixtures with sealed permits, not final funds authorization.
use super::*;
use crate::{DailyUsageSnapshot, ExecutionRequest, ExecutionRequestStore, PublicAuditOutbox};
use safeflow_bank_gateway::{
    BankGateway, CnyCurrency, DecimalAmount, MaskedAccountView, PayeeView,
};
use safeflow_contracts::{BusinessOperationRef, IdempotencyKey, OperationId};
use safeflow_transfer_integration::{
    daily_usage::{calculate_daily_usage, DailyUsageInput},
    funds_store::FundsStore,
    mock_execution::{MockBehavior, MockOutgoingState, MockResolution, MockTransferRequest},
    shared_demo::{DemoRecipient, SharedDemoPlan},
    DraftDetails, DraftStatus, StoredDraft,
};

fn resource(value: &str) -> ResourceId {
    ResourceId::parse(value).unwrap()
}
fn amount(value: &str) -> DecimalAmount {
    DecimalAmount::parse(value).unwrap()
}
fn requests(db: &Db) -> ExecutionRequestStore {
    ExecutionRequestStore::for_trusted_backend(db.repo.clone(), db.sessions.clone())
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn scope_nonfund_permits_ignore_broken_transfer_statistics_but_funding_refuses() {
    for unavailable in [false, true] {
        let db = Db::new().await;
        let (funding, transfer, business) = setup(&db, "scope", "scope-payer").await;
        let common = db.fixture().await;
        let common_permit = db.save(&common).await;
        let submitted = db.fixture().await;
        let submitted_permit = db.save(&submitted).await;
        // A real persisted member original, intentionally missing its public submission.
        let mut tx = db.sql.begin().await.unwrap();
        FundsStore::submit(
            &mut tx,
            &transfer.binding.snapshot.user_id,
            &transfer.idempotency_key,
            &business,
            MockBehavior::Pending,
            MockOutgoingState::Allowed,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        if unavailable {
            db.sql
                .execute("ALTER TABLE transfer_fund_orders_v1 RENAME TO isolated_scope_unavailable")
                .await
                .unwrap();
        }
        assert!(requests(&db).daily_usage(&db.session).await.is_err());
        db.consume(&common_permit).await.unwrap();
        let request = ExecutionRequest {
            idempotency_key: IdempotencyKey::parse("scope-common-key").unwrap(),
            business_operation_ref: BusinessOperationRef::parse("scope-common-business").unwrap(),
            business_version: 1,
            binding: submitted_permit.binding().clone(),
            payload: submitted.payload,
        };
        let id = submitted_permit.permit_id().clone();
        let recorded = request.clone();
        let receipt = db
            .store
            .submission_transaction(&db.session, &request, move |unit| {
                Box::pin(async move {
                    unit.consume(&id, &recorded.binding).await?;
                    assert!(unit.daily_usage_snapshot().is_none());
                    unit.record_submission(&recorded).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(receipt.operation.state, OperationState::Submitted);
        let id = funding.permit_id().clone();
        let expected = transfer.binding.clone();
        assert!(db
            .store
            .submission_transaction(&db.session, &transfer, move |unit| Box::pin(async move {
                unit.consume(&id, &expected).await?;
                panic!("funding may not bypass unavailable/inconsistent transfer facts")
            }))
            .await
            .is_err());
        assert_eq!(db.status(&funding).await, "ACTIVE");
        assert_eq!(db.status(&common_permit).await, "CONSUMED");
        assert_eq!(db.status(&submitted_permit).await, "CONSUMED");
        assert_eq!(db.count("PERMIT_CONSUMED").await, 2);
        db.store
            .submission_transaction(&db.session, &request, |_| {
                Box::pin(async { panic!("no replay callback") })
            })
            .await
            .unwrap();
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn scope_transfer_persists_and_requires_full_projection_even_without_payer() {
    let db = Db::new().await;
    for reserved in [false, true] {
        let mut v = db.fixture().await;
        // Model fields do not select the scope, even when none looks like funding.
        assert!(!crate::daily_usage::funding_shape(
            &v.payload.normalized_arguments
        ));
        v.execution_scope = VerifiedPermitScope::Transfer;
        let p = db.save(&v).await;
        v.execution_scope = VerifiedPermitScope::Common;
        assert_eq!(
            db.store.save_verified(&db.session, &v).await.err(),
            Some(StorageError::PermitConflict)
        );
        assert!(sqlx::query(
            "UPDATE execution_permits SET execution_scope='COMMON' WHERE permit_id=$1"
        )
        .bind(p.permit_id().as_str())
        .execute(&db.sql)
        .await
        .is_err());
        if reserved {
            requests(&db)
                .reserve(
                    &db.session,
                    &ExecutionRequest {
                        idempotency_key: IdempotencyKey::parse("scope-missing-payer-key").unwrap(),
                        business_operation_ref: BusinessOperationRef::parse(
                            "scope-missing-payer-operation",
                        )
                        .unwrap(),
                        business_version: 1,
                        binding: p.binding().clone(),
                        payload: v.payload.clone(),
                    },
                )
                .await
                .unwrap();
        }
        let reopened = PgRepository::connect_existing(&db.url, &db.schema)
            .await
            .unwrap();
        let store = Db::store(reopened.clone(), db.sessions.clone(), db.clock.clone());
        assert_eq!(
            consume(&store, &db.session, p.permit_id(), p.binding()).await,
            Err(StorageError::DependencyUnavailable)
        );
        reopened.close().await;
        assert_eq!(db.status(&p).await, "ACTIVE");
    }
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16, public mechanisms only"]
async fn daily_payment_minimum_real_multi_account_900_plus_200_and_original_replay() {
    let db = Db::new().await;
    let (prior, request, business) =
        setup_amount(&db, "payment-prior", "payment-payer-a", "900.00", "2000.00").await;
    let (_unused, current, projection) = setup_amount(
        &db,
        "payment-current",
        "payment-payer-b",
        "200.00",
        "2000.00",
    )
    .await;
    // Existing sealed public-mechanism fixtures, never an enabled product issuer/card source.
    submit(&db, &prior, &request, &business, MockBehavior::Pending).await;
    let before = usage(&db).await;
    assert_eq!(before.usage.used_cents, 90_000);
    assert_eq!(before.payer_accounts.len(), 2);
    let runtime = safeflow_runtime::RuntimeBootstrap::build(
        safeflow_runtime::ToolRegistry::new(),
        safeflow_runtime::DomainPolicyRegistry::new(),
        safeflow_runtime::ToolSecurityCatalog::new(),
        db.sessions.clone(),
        Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
    )
    .unwrap();
    for replay in [false, true] {
        if replay {
            submit(&db, &prior, &request, &business, MockBehavior::Pending).await;
        }
        let mut tx = db.sql.begin().await.unwrap();
        crate::daily_usage::protect_owner(&mut tx, &current.binding.snapshot.user_id)
            .await
            .unwrap();
        let task = crate::repository::lock_task(
            &mut tx,
            &current.binding.snapshot.user_id,
            &current.binding.snapshot.task_id,
        )
        .await
        .unwrap();
        let saved = FundsStore::draft(&mut tx, &task.owner_id, &projection.draft_id)
            .await
            .unwrap();
        let snapshot = crate::PaymentSnapshot {
            transfer: projection.clone(),
            display_details: serde_json::to_value(&saved.details).unwrap(),
            draft_expires_at: saved.expires_at,
        };
        let facts = crate::decision::payment::facts(&mut tx, &task, &snapshot)
            .await
            .unwrap();
        let result = runtime
            .runtime()
            .review_payment_minimum_for_backend(
                &db.session,
                safeflow_runtime::PaymentMinimumInput {
                    operation_id: current.binding.snapshot.operation_id.clone(),
                    facts_version: current.binding.snapshot.facts_version as u32,
                    used_cents: facts["used_cents"].as_u64().unwrap(),
                    current_cents: facts["current_cents"].as_u64().unwrap(),
                },
            )
            .unwrap();
        assert_eq!(facts["combined_cents"], 110_000);
        assert_eq!(
            result.decision().minimum_level,
            safeflow_contracts::SecurityLevel::Red
        );
        assert_eq!(result.decision().outcome, SecurityOutcome::WaitingAuth);
        tx.rollback().await.unwrap();
    }
    assert_eq!(usage(&db).await.usage, before.usage);
    db.finish().await;
}

async fn usage(db: &Db) -> DailyUsageSnapshot {
    requests(db).daily_usage(&db.session).await.unwrap()
}
async fn setup(
    db: &Db,
    label: &str,
    payer: &str,
) -> (StoredExecutionPermit, ExecutionRequest, MockTransferRequest) {
    setup_amount(db, label, payer, "2.00", "10.00").await
}
async fn setup_amount(
    db: &Db,
    label: &str,
    payer: &str,
    value: &str,
    opening: &str,
) -> (StoredExecutionPermit, ExecutionRequest, MockTransferRequest) {
    let b = MockTransferRequest {
        task_id: new_task_id(),
        operation_id: OperationId::parse(format!("daily-operation-{label}")).unwrap(),
        draft_id: resource(&format!("daily-draft-{label}")),
        draft_version: 1,
        payer_account_id: resource(payer),
        payee_id: resource("daily-payee"),
        amount: amount(value),
        currency: CnyCurrency::Cny,
        remark: String::new(),
    };
    let mut v = db
        .fixture_parameters(
            1,
            b.task_id.clone(),
            serde_json::to_value(&b).unwrap(),
            b.draft_id.clone(),
        )
        .await;
    v.execution_scope = VerifiedPermitScope::Transfer;
    let mut tx = db.sql.begin().await.unwrap();
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    FundsStore::provision_account(
        &mut tx,
        &v.binding.user_id,
        &b.payer_account_id,
        &amount(opening),
    )
    .await
    .unwrap();
    let recipient = resource("daily-isolated-recipient-account");
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("daily-isolated-recipient").unwrap(),
        &recipient,
        &amount("0.00"),
    )
    .await
    .unwrap();
    FundsStore::provision_payee(&mut tx, &v.binding.user_id, &b.payee_id, &recipient)
        .await
        .unwrap();
    FundsStore::save_draft(
        &mut tx,
        &v.binding.user_id,
        None,
        &StoredDraft {
            draft_id: b.draft_id.clone(),
            operation_id: b.operation_id.clone(),
            task_id: b.task_id.clone(),
            user_id: v.binding.user_id.clone(),
            plan_version: 1,
            status: DraftStatus::ReadyToConfirm,
            expires_at: now as u64 + 3600,
            details: Some(DraftDetails {
                account: MaskedAccountView {
                    account_id: b.payer_account_id.clone(),
                    display_name: "Public mechanism fixture".into(),
                    masked_account_no: "**** 0001".into(),
                    currency: CnyCurrency::Cny,
                },
                payee: PayeeView {
                    payee_id: b.payee_id.clone(),
                    display_name: "Fixture".into(),
                    bank_name: "Fixture".into(),
                    masked_account_no: "**** 0002".into(),
                },
                amount: b.amount.clone(),
                currency: b.currency,
                remark: b.remark.clone(),
                available_balance: amount(opening),
                balance_as_of: chrono::DateTime::from_timestamp(now, 0)
                    .unwrap()
                    .to_rfc3339(),
            }),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let r = ExecutionRequest {
        idempotency_key: IdempotencyKey::parse(format!("daily-key-{label}")).unwrap(),
        business_operation_ref: BusinessOperationRef::parse(b.operation_id.as_str()).unwrap(),
        business_version: 1,
        binding: PermitBinding {
            snapshot: v.binding.clone(),
            fingerprint: compute_operation_fingerprint(&v.payload).unwrap(),
        },
        payload: v.payload.clone(),
    };
    (db.save(&v).await, r, b)
}
async fn write(
    unit: &mut PermitTransaction,
    r: &ExecutionRequest,
    b: &MockTransferRequest,
    behavior: MockBehavior,
) -> Result<(), StorageError> {
    FundsStore::submit(
        unit.connection()?,
        &r.binding.snapshot.user_id,
        &r.idempotency_key,
        b,
        behavior,
        MockOutgoingState::Allowed,
    )
    .await
    .map_err(crate::transfer_sql::business_error)?;
    unit.record_submission(r).await?;
    unit.record_transfer_result().await?;
    Ok(())
}
async fn submit(
    db: &Db,
    p: &StoredExecutionPermit,
    r: &ExecutionRequest,
    b: &MockTransferRequest,
    behavior: MockBehavior,
) -> crate::PublicSubmissionReceipt {
    let id = p.permit_id().clone();
    let r2 = r.clone();
    let b = b.clone();
    db.store
        .submission_transaction(&db.session, r, move |unit| {
            Box::pin(async move {
                unit.consume(&id, &r2.binding).await?;
                assert!(unit.daily_usage_snapshot().is_some());
                write(unit, &r2, &b, behavior).await
            })
        })
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_real_public_time_multi_account_replay_and_fresh_process_example() {
    let db = Db::new().await;
    let (p, a, ba) = setup(&db, "a", "daily-payer-a").await;
    let (q, b, bb) = setup(&db, "b", "daily-payer-b").await;
    assert_eq!(usage(&db).await.usage.used_cents, 0); // Only drafts/accounts exist.
    requests(&db).reserve(&db.session, &b).await.unwrap();
    assert_eq!(usage(&db).await.usage.used_cents, 0); // Reservation is not submission.
    let first = submit(&db, &p, &a, &ba, MockBehavior::Pending).await;
    let second = submit(&db, &q, &b, &bb, MockBehavior::Immediate).await;
    let snapshot = usage(&db).await;
    assert_eq!(
        snapshot.payer_accounts,
        vec![resource("daily-payer-a"), resource("daily-payer-b")]
    );
    assert_eq!(
        (
            snapshot.usage.used_cents,
            snapshot.usage.successful_cents,
            snapshot.usage.pending_today_cents
        ),
        (400, 200, 200)
    );
    assert_eq!(
        snapshot.submitted[0].submitted_at,
        Some(first.recorded_at.to_rfc3339())
    );
    assert_eq!(
        snapshot.submitted[1].submitted_at,
        Some(second.recorded_at.to_rfc3339())
    );
    let persisted: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT submitted_at FROM public_execution_requests WHERE idempotency_key=$1",
    )
    .bind(a.idempotency_key.as_str())
    .fetch_one(&db.sql)
    .await
    .unwrap();
    assert_eq!(persisted, first.recorded_at);
    assert!(sqlx::query("UPDATE public_execution_requests SET submitted_at=clock_timestamp() WHERE idempotency_key=$1")
        .bind(a.idempotency_key.as_str()).execute(&db.sql).await.is_err());
    let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
    let before = outbox.pending(100).await.unwrap();
    assert_eq!(
        db.store
            .submission_transaction(&db.same_user, &a, |_| Box::pin(async {
                panic!("replay must not execute")
            }))
            .await
            .unwrap(),
        first
    );
    assert_eq!(outbox.pending(100).await.unwrap(), before);
    assert_eq!(usage(&db).await.usage.used_cents, 400);
    assert_eq!(db.count("PERMIT_CONSUMED").await, 2);
    let output = std::process::Command::new(
        std::env::var("SAFEFLOW_DAILY_EXAMPLE_BIN").expect("build public_daily_usage"),
    )
    .arg("user_001")
    .env("SAFEFLOW_DATABASE_URL", &db.url)
    .env("SAFEFLOW_STORAGE_SCHEMA", &db.schema)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "example failed without printing connection configuration"
    );
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        actual["snapshot"]["submitted"],
        serde_json::to_value(&snapshot.submitted).unwrap()
    );
    assert_eq!(
        actual["snapshot"]["usage"],
        serde_json::to_value(&snapshot.usage).unwrap()
    );
    println!(
        "PUBLIC_DAILY_DB_ACTUAL={}",
        serde_json::to_string(&actual).unwrap()
    );
    assert_eq!(
        db.store
            .request_issuance(&db.session, &ba.task_id)
            .await
            .err(),
        Some(StorageError::PermitIssuanceUnavailable)
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_deterministic_two_tasks_two_accounts_commit_or_rollback() {
    for rollback in [false, true] {
        let db = Db::new().await;
        let (p, a, ba) = setup(&db, "a", "daily-payer-a").await;
        let (q, b, bb) = setup(&db, "b", "daily-payer-b").await;
        assert_ne!(a.binding.snapshot.task_id, b.binding.snapshot.task_id);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let store = db.store.clone();
        let session = db.session.clone();
        let r = a.clone();
        let id = p.permit_id().clone();
        let first = tokio::spawn(async move {
            store
                .submission_transaction(&session, &a, move |unit| {
                    Box::pin(async move {
                        unit.consume(&id, &r.binding).await?;
                        assert_eq!(unit.daily_usage_snapshot().unwrap().usage.used_cents, 0);
                        if rollback {
                            write(unit, &r, &ba, MockBehavior::Immediate).await?;
                        }
                        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                            .fetch_one(unit.connection()?)
                            .await?;
                        entered_tx.send(pid).unwrap();
                        release_rx.await.unwrap();
                        if rollback {
                            return Err(StorageError::DependencyUnavailable);
                        }
                        write(unit, &r, &ba, MockBehavior::Pending).await
                    })
                })
                .await
        });
        let pid = entered_rx.await.unwrap();
        let (seen_tx, mut seen_rx) = tokio::sync::oneshot::channel();
        let store = db.store.clone();
        let session = db.session.clone();
        let r = b.clone();
        let id = q.permit_id().clone();
        let second = tokio::spawn(async move {
            store
                .submission_transaction(&session, &b, move |unit| {
                    Box::pin(async move {
                        unit.consume(&id, &r.binding).await?;
                        seen_tx
                            .send(unit.daily_usage_snapshot().unwrap().usage.used_cents)
                            .unwrap();
                        write(unit, &r, &bb, MockBehavior::Pending).await
                    })
                })
                .await
        });
        wait_for_blocked(&db, pid).await;
        assert!(matches!(
            seen_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .unwrap()
            .unwrap();
        if rollback {
            assert_eq!(result.unwrap_err(), StorageError::DependencyUnavailable);
        } else {
            result.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(seen_rx.await.unwrap(), if rollback { 0 } else { 200 });
        assert_eq!(
            usage(&db).await.usage.used_cents,
            if rollback { 200 } else { 400 }
        );
        assert_eq!(
            db.status(&p).await,
            if rollback { "ACTIVE" } else { "CONSUMED" }
        );
        let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_ledger_v1")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(ledger, 0);
        let mappings: i64 = sqlx::query_scalar("SELECT count(*) FROM public_execution_requests")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(mappings, if rollback { 1 } else { 2 });
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_result_release_wait_and_cross_day_synthetic_clock() {
    for resolution in [MockResolution::Succeed, MockResolution::Fail] {
        let db = Db::new().await;
        let (p, r, b) = setup(&db, "a", "daily-payer-a").await;
        let first = submit(&db, &p, &r, &b, MockBehavior::Pending).await;
        let later = (first.recorded_at + chrono::Duration::days(1)).to_rfc3339();
        let snapshot = usage(&db).await;
        // Synthetic NEXT-day observation only; submitted_at remains the real public timestamp.
        let next = calculate_daily_usage(DailyUsageInput {
            owner_id: &snapshot.owner_id,
            as_of: &later,
            complete: true,
            submitted: &snapshot.submitted,
        })
        .unwrap();
        assert_eq!((next.carried_pending_cents, next.used_cents), (200, 200));
        let mut tx = db.sql.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        FundsStore::resolve(
            &mut tx,
            &r.binding.snapshot.user_id,
            &b.operation_id,
            resolution,
            MockOutgoingState::Allowed,
        )
        .await
        .unwrap();
        let reader = requests(&db);
        let session = db.session.clone();
        let job = tokio::spawn(async move { reader.daily_usage(&session).await });
        wait_for_blocked(&db, pid).await;
        tx.commit().await.unwrap();
        let current = tokio::time::timeout(Duration::from_secs(10), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            current.submitted[0].submitted_at,
            Some(first.recorded_at.to_rfc3339())
        );
        let next = calculate_daily_usage(DailyUsageInput {
            owner_id: &current.owner_id,
            as_of: &later,
            complete: true,
            submitted: &current.submitted,
        })
        .unwrap();
        assert_eq!(next.used_cents, 0);
        assert_eq!(
            current.usage.used_cents,
            if matches!(resolution, MockResolution::Succeed) {
                200
            } else {
                0
            }
        );
        let recovered = requests(&db)
            .reconcile_transfer(&db.session, &r.idempotency_key)
            .await
            .unwrap();
        assert_eq!(recovered.public.first_submission, Some(first));
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_orphan_business_and_reservation_are_not_zero_or_formal_submission() {
    for reserve in [false, true] {
        let db = Db::new().await;
        let (_p, r, b) = setup(&db, "a", "daily-payer-a").await;
        if reserve {
            requests(&db).reserve(&db.session, &r).await.unwrap();
        }
        let mut tx = db.sql.begin().await.unwrap();
        FundsStore::submit(
            &mut tx,
            &r.binding.snapshot.user_id,
            &r.idempotency_key,
            &b,
            MockBehavior::Pending,
            MockOutgoingState::Allowed,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            requests(&db).daily_usage(&db.session).await.unwrap_err(),
            StorageError::ReconcileRequired
        );
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_incomplete_or_conflicting_persisted_records_refuse() {
    for damage in [
        "business_missing",
        "fund_missing",
        "owner",
        "account",
        "amount",
        "request",
        "operation",
        "query_failure",
        "future_time",
    ] {
        let db = Db::new().await;
        let (p, r, b) = setup(&db, "a", "daily-payer-a").await;
        submit(&db, &p, &r, &b, MockBehavior::Pending).await;
        let (next_permit, next, _) = setup(&db, "next", "daily-payer-b").await;
        let sql=match damage {
            "business_missing"=>"DELETE FROM transfer_fund_orders_v1; DELETE FROM transfer_business_orders_v1",
            "fund_missing"=>"DELETE FROM transfer_fund_orders_v1",
            "owner"=>"UPDATE transfer_accounts_v1 SET owner_id='wrong-owner' WHERE account_id='daily-payer-a'",
            "account"=>"UPDATE transfer_fund_orders_v1 SET payer_account_id='daily-payer-b'",
            "amount"=>"UPDATE transfer_fund_orders_v1 SET amount_cents=201",
            "request"=>"UPDATE transfer_business_orders_v1 SET request=jsonb_set(request,'{remark}','\"tampered\"')",
            "operation"=>"UPDATE transfer_business_orders_v1 SET request=jsonb_set(request,'{operation_id}','\"wrong-operation\"')",
            "query_failure"=>"ALTER TABLE transfer_fund_orders_v1 RENAME TO isolated_unavailable_facts",
            // Isolated corruption injection only; production trigger rejects this update.
            _=>"ALTER TABLE public_execution_requests DISABLE TRIGGER USER; UPDATE public_execution_requests SET submitted_at=clock_timestamp()+interval '1 day'; ALTER TABLE public_execution_requests ENABLE TRIGGER USER",
        };
        db.sql.execute(sql).await.unwrap();
        assert!(
            requests(&db).daily_usage(&db.session).await.is_err(),
            "{damage}"
        );
        let id = next_permit.permit_id().clone();
        let expected = next.binding.clone();
        let before = PublicAuditOutbox::for_trusted_backend(db.repo.clone())
            .pending(100)
            .await
            .unwrap();
        assert!(
            db.store
                .submission_transaction(&db.session, &next, move |unit| Box::pin(async move {
                    unit.consume(&id, &expected).await?;
                    panic!("incomplete source must reject before obtaining a writable connection");
                }))
                .await
                .is_err(),
            "{damage}"
        );
        assert_eq!(db.status(&next_permit).await, "ACTIVE");
        assert_eq!(
            PublicAuditOutbox::for_trusted_backend(db.repo.clone())
                .pending(100)
                .await
                .unwrap(),
            before
        );
        // Original request replay remains read-only even when statistics are unavailable.
        if damage != "future_time" {
            db.store
                .submission_transaction(&db.session, &r, |_| {
                    Box::pin(async { panic!("no recovery execution") })
                })
                .await
                .unwrap();
        }
        assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_consumed_snapshot_protects_pending_release_through_commit() {
    let db = Db::new().await;
    let (p, a, ba) = setup(&db, "a", "daily-payer-a").await;
    submit(&db, &p, &a, &ba, MockBehavior::Pending).await;
    let (q, b, bb) = setup(&db, "b", "daily-payer-b").await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let store = db.store.clone();
    let session = db.session.clone();
    let r = b.clone();
    let id = q.permit_id().clone();
    let first = tokio::spawn(async move {
        store
            .submission_transaction(&session, &b, move |unit| {
                Box::pin(async move {
                    unit.consume(&id, &r.binding).await?;
                    assert_eq!(unit.daily_usage_snapshot().unwrap().usage.used_cents, 200);
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(unit.connection()?)
                        .await?;
                    entered_tx.send(pid).unwrap();
                    release_rx.await.unwrap();
                    write(unit, &r, &bb, MockBehavior::Pending).await
                })
            })
            .await
    });
    let pid = entered_rx.await.unwrap();
    let pool = db.sql.clone();
    let owner = a.binding.snapshot.user_id.clone();
    let release = tokio::spawn(async move {
        let mut tx = pool.begin().await.unwrap();
        FundsStore::resolve(
            &mut tx,
            &owner,
            &ba.operation_id,
            MockResolution::Fail,
            MockOutgoingState::Allowed,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    });
    wait_for_blocked(&db, pid).await;
    assert!(!release.is_finished());
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), release)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage(&db).await.usage.used_cents, 200);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_wait_revalidates_session_and_does_not_consume() {
    let db = Db::new().await;
    let (p, r, _) = setup(&db, "a", "daily-payer-a").await;
    let mut tx = db.sql.begin().await.unwrap();
    crate::daily_usage::protect_owner(&mut tx, &r.binding.snapshot.user_id)
        .await
        .unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let store = db.store.clone();
    let session = db.session.clone();
    let job = tokio::spawn(async move {
        store
            .submission_transaction(&session, &r, |_| {
                Box::pin(async { panic!("revoked wait must not enter work") })
            })
            .await
    });
    wait_for_blocked(&db, pid).await;
    db.sessions.revoke(&db.session);
    tx.commit().await.unwrap();
    assert_eq!(job.await.unwrap().unwrap_err(), StorageError::Session);
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_post_write_partial_funding_submission_is_rolled_back() {
    let db = Db::new().await;
    let (p, r, _) = setup(&db, "a", "daily-payer-a").await;
    let id = p.permit_id().clone();
    let r2 = r.clone();
    let before = PublicAuditOutbox::for_trusted_backend(db.repo.clone())
        .pending(100)
        .await
        .unwrap();
    let result = db
        .store
        .submission_transaction(&db.session, &r, move |unit| {
            Box::pin(async move {
                unit.consume(&id, &r2.binding).await?;
                unit.record_submission(&r2).await?;
                Ok(()) // No member original: final complete-snapshot check must fail.
            })
        })
        .await;
    assert_eq!(result.unwrap_err(), StorageError::ReconcileRequired);
    assert_eq!(db.status(&p).await, "ACTIVE");
    assert_eq!(
        PublicAuditOutbox::for_trusted_backend(db.repo.clone())
            .pending(100)
            .await
            .unwrap(),
        before
    );
    assert_eq!(usage(&db).await.usage.used_cents, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn daily_explicit_shared_demo_atomic_idempotent_sql_gateway_no_balance_reset() {
    let db = Db::new().await;
    let recipient = || DemoRecipient {
        owner_id: UserId::parse("explicit-test-recipient").unwrap(),
        account_id: resource("explicit-test-recipient-account"),
    };
    let plan = SharedDemoPlan::prepare(recipient()).unwrap();
    db.repo.initialize_shared_demo(&plan).await.unwrap();
    let balances: Vec<(String, i64)> = sqlx::query_as(
        "SELECT account_id,booked_cents FROM transfer_accounts_v1 ORDER BY account_id",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(
        balances,
        vec![
            ("account_001".into(), 128800),
            ("account_002".into(), 5000),
            ("explicit-test-recipient-account".into(), 0)
        ]
    );
    // Simulate an already changed balance; reinitialization must never reset it.
    db.sql
        .execute(
            "UPDATE transfer_accounts_v1 SET booked_cents=120000 WHERE account_id='account_001'",
        )
        .await
        .unwrap();
    let reopened = PgRepository::connect_existing(&db.url, &db.schema)
        .await
        .unwrap();
    reopened.initialize_shared_demo(&plan).await.unwrap();
    let bank = crate::TransferSqlBankGateway::for_trusted_backend(
        reopened.clone(),
        Arc::new(safeflow_mock_bank::MockBankGateway::new()),
    );
    assert_eq!(
        bank.get_account_balance(
            &UserId::parse("user_001").unwrap(),
            &resource("account_001")
        )
        .await
        .unwrap()
        .available_balance,
        amount("1200.00")
    );
    assert_eq!(
        bank.get_account_balance(
            &UserId::parse("user_002").unwrap(),
            &resource("account_002")
        )
        .await
        .unwrap()
        .available_balance,
        amount("50.00")
    );
    let conflict = SharedDemoPlan::prepare(DemoRecipient {
        owner_id: UserId::parse("other-explicit-recipient").unwrap(),
        account_id: resource("other-explicit-recipient-account"),
    })
    .unwrap();
    assert!(reopened.initialize_shared_demo(&conflict).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_accounts_v1")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(count, 3);
    reopened.close().await;
    db.finish().await;
}
