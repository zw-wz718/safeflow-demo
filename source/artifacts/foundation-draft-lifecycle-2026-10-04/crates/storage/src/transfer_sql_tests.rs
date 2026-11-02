//! Received business SQL plus public storage fixtures, never a formal funds permit.
use super::*;
use crate::TransferSqlReader;
use safeflow_bank_gateway::{CnyCurrency, DecimalAmount, MaskedAccountView, PayeeView};
use safeflow_contracts::{BusinessOperationRef, IdempotencyKey, OperationId};
use safeflow_transfer_integration::{
    funds_store::{FundsError, FundsStore},
    mock_execution::{MockBehavior, MockOutgoingState, MockResolution, MockTransferRequest},
    DraftDetails, DraftStatus, StoredDraft,
};

const SAMPLES: &str = include_str!("../../../fixtures/transfer/postgres_business_actual_v1.json");

fn resource(value: &str) -> ResourceId {
    ResourceId::parse(value).unwrap()
}
fn amount(value: &str) -> DecimalAmount {
    DecimalAmount::parse(value).unwrap()
}
fn reader(db: &Db) -> TransferSqlReader {
    TransferSqlReader::for_trusted_backend(db.repo.clone(), db.sessions.clone())
}
fn public_request(v: &VerifiedPermitSnapshot) -> crate::ExecutionRequest {
    crate::ExecutionRequest {
        idempotency_key: IdempotencyKey::parse("isolated-public-original-key").unwrap(),
        business_operation_ref: BusinessOperationRef::parse("business-original-id").unwrap(),
        business_version: 1,
        binding: PermitBinding {
            snapshot: v.binding.clone(),
            fingerprint: compute_operation_fingerprint(&v.payload).unwrap(),
        },
        payload: v.payload.clone(),
    }
}
async fn db_now(db: &Db) -> u64 {
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
            .fetch_one(&db.sql)
            .await
            .unwrap();
    now as u64
}
fn request(draft: &StoredDraft) -> MockTransferRequest {
    let details = draft.details.as_ref().unwrap();
    MockTransferRequest {
        task_id: draft.task_id.clone(),
        operation_id: draft.operation_id.clone(),
        draft_id: draft.draft_id.clone(),
        draft_version: draft.plan_version,
        payer_account_id: details.account.account_id.clone(),
        payee_id: details.payee.payee_id.clone(),
        amount: details.amount.clone(),
        currency: details.currency,
        remark: details.remark.clone(),
    }
}
async fn provision(
    db: &Db,
    draft: &StoredDraft,
    receiver: &UserId,
    recipient: &ResourceId,
    opening: &str,
) {
    let details = draft.details.as_ref().unwrap();
    let mut tx = db.sql.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &draft.user_id,
        &details.account.account_id,
        &amount(opening),
    )
    .await
    .unwrap();
    FundsStore::provision_account(&mut tx, receiver, recipient, &amount("0.00"))
        .await
        .unwrap();
    FundsStore::provision_payee(&mut tx, &draft.user_id, &details.payee.payee_id, recipient)
        .await
        .unwrap();
    FundsStore::save_draft(&mut tx, &draft.user_id, None, draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16 and built transfer_sql_fixture_read example"]
async fn transfer_sql_seven_original_json_cases_and_new_process_reads() {
    let db = Db::new().await;
    let samples: serde_json::Value = serde_json::from_str(SAMPLES).unwrap();
    assert_eq!(
        samples["scope"],
        "ISOLATED_POSTGRES_BUSINESS_SQL_NOT_RUNTIME_AUTHORIZATION"
    );
    let cases = samples["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 7);
    let mut snapshots = Vec::new();
    for case in cases {
        let name = case["case"].as_str().unwrap();
        let owner = UserId::parse(format!("sql-owner-{name}")).unwrap();
        let receiver = UserId::parse(format!("sql-recipient-{name}")).unwrap();
        let recipient = resource(&format!("sql-recipient-account-{name}"));
        let expected_draft = &case["draft"];
        let draft = StoredDraft {
            draft_id: serde_json::from_value(expected_draft["draft_id"].clone()).unwrap(),
            operation_id: serde_json::from_value(expected_draft["operation_id"].clone()).unwrap(),
            task_id: TaskId::parse(format!("sql-task-{name}")).unwrap(),
            user_id: owner.clone(),
            plan_version: expected_draft["business_version"].as_u64().unwrap() as u32,
            status: DraftStatus::ReadyToConfirm,
            expires_at: db_now(&db).await + 3600,
            details: Some(serde_json::from_value(expected_draft["details"].clone()).unwrap()),
        };
        provision(&db, &draft, &receiver, &recipient, "1288.00").await;
        let key = IdempotencyKey::parse(format!("sql-key-{name}")).unwrap();
        let request = request(&draft);
        let behavior = match name {
            "failure" => MockBehavior::Decline,
            "pending" | "settled" | "released" => MockBehavior::Pending,
            "lost_reply" => MockBehavior::LostReplyAfterCommit,
            _ => MockBehavior::Immediate,
        };
        let mut tx = db.sql.begin().await.unwrap();
        FundsStore::submit(
            &mut tx,
            &owner,
            &key,
            &request,
            behavior,
            MockOutgoingState::Allowed,
        )
        .await
        .unwrap();
        if name == "rollback" {
            tx.rollback().await.unwrap();
        } else {
            tx.commit().await.unwrap();
        }
        if matches!(name, "settled" | "released") {
            let mut tx = db.sql.begin().await.unwrap();
            FundsStore::resolve(
                &mut tx,
                &owner,
                &request.operation_id,
                if name == "settled" {
                    MockResolution::Succeed
                } else {
                    MockResolution::Fail
                },
                MockOutgoingState::Allowed,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        db.sessions.bind(&db.session, owner.as_str(), 1);
        let read = reader(&db);
        let before_replay = read
            .business(&db.session, &draft.operation_id)
            .await
            .unwrap();
        if name != "rollback" {
            let mut tx = db.sql.begin().await.unwrap();
            let replay = FundsStore::submit(
                &mut tx,
                &owner,
                &key,
                &request,
                behavior,
                MockOutgoingState::Unknown,
            )
            .await
            .unwrap();
            assert_eq!(Some(replay), before_replay);
            tx.commit().await.unwrap();
            let mut conflicting = request.clone();
            conflicting.amount = amount("201.00");
            let mut tx = db.sql.begin().await.unwrap();
            assert!(matches!(
                FundsStore::submit(
                    &mut tx,
                    &owner,
                    &key,
                    &conflicting,
                    behavior,
                    MockOutgoingState::Allowed
                )
                .await,
                Err(FundsError::Receipt(
                    safeflow_transfer_integration::business_store::StoreError::IdempotencyConflict
                ))
            ));
            tx.rollback().await.unwrap();
        }
        let current = read
            .business(&db.session, &draft.operation_id)
            .await
            .unwrap();
        assert_eq!(current, before_replay);
        let mut actual_record = serde_json::to_value(&current).unwrap();
        let mut expected_record = case["record"].clone();
        for record in [&mut actual_record, &mut expected_record] {
            if record.is_object() {
                for field in ["first_receipt", "latest_receipt"] {
                    if let Some(time) = record[field]["completed_at"].as_str() {
                        chrono::DateTime::parse_from_rfc3339(time).unwrap();
                        record[field]["completed_at"] = json!("VALID_RFC3339");
                    }
                }
            }
        }
        assert_eq!(actual_record, expected_record, "{name} receipts");
        let payer = read
            .funds(&db.session, &request.payer_account_id)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(&payer).unwrap(), case["payer_funds"]);
        let debit = read.transactions(&db.session).await.unwrap();
        db.sessions.bind(&db.same_user, receiver.as_str(), 1);
        let credited = read.funds(&db.same_user, &recipient).await.unwrap();
        assert_eq!(
            serde_json::to_value(&credited).unwrap(),
            case["recipient_funds"]
        );
        let credit = read.transactions(&db.same_user).await.unwrap();
        for (actual, field) in [
            (&debit, "payer_transactions"),
            (&credit, "recipient_transactions"),
        ] {
            let mut actual = serde_json::to_value(actual).unwrap();
            let mut expected = case[field].clone();
            for rows in [&mut actual, &mut expected] {
                for row in rows.as_array_mut().unwrap() {
                    assert!(row["transaction_id"]
                        .as_str()
                        .unwrap()
                        .starts_with("transfer-ledger-"));
                    chrono::DateTime::parse_from_rfc3339(row["completed_at"].as_str().unwrap())
                        .unwrap();
                    row.as_object_mut().unwrap().remove("transaction_id");
                    row.as_object_mut().unwrap().remove("completed_at");
                }
            }
            assert_eq!(actual, expected, "{name} {field}");
        }
        assert_eq!(
            read.draft(&db.session, &draft.draft_id).await.unwrap(),
            draft
        );
        assert!(read
            .business(&db.same_user, &draft.operation_id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            read.funds(&db.same_user, &request.payer_account_id)
                .await
                .unwrap_err(),
            StorageError::NotFound
        );
        snapshots.push(json!({"case":name,"record":current,"payer":payer,"recipient":credited,"debit":debit,"credit":credit}));
    }
    let example = std::env::var("SAFEFLOW_TRANSFER_SQL_READ_EXAMPLE_BIN")
        .expect("build the isolated read example; no test skipped");
    let output = std::process::Command::new(example)
        .env("SAFEFLOW_DATABASE_URL", &db.url)
        .env("SAFEFLOW_STORAGE_SCHEMA", &db.schema)
        .output()
        .expect("start fresh fixture-reader process");
    assert!(
        output.status.success(),
        "fresh process failed without logging private environment"
    );
    let restarted: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        restarted["scope"],
        "ISOLATED_TRANSFER_SQL_FIXTURE_READ_ONLY"
    );
    assert_eq!(restarted["cases"], json!(snapshots));
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn transfer_sql_upgrade_from_fourteen_preserves_history_and_original_mapping() {
    let db = Db::unmigrated().await;
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let source = sqlx::migrate::Migrator::new(directory).await.unwrap();
    let historical = source
        .iter()
        .filter(|m| m.version <= 14)
        .map(|m| {
            sqlx::migrate::Migration::new(
                m.version,
                m.description.clone(),
                m.migration_type,
                m.sql.replace("\r\n", "\n").into(),
                false,
            )
        })
        .collect::<Vec<_>>();
    let migrator = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(historical),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator.run(&db.sql).await.unwrap();
    let v = db.fixture().await;
    let r = public_request(&v);
    // The original request/receipt fixture is public metadata, not money evidence.
    let requests =
        crate::ExecutionRequestStore::for_trusted_backend(db.repo.clone(), db.sessions.clone());
    requests.reserve(&db.session, &r).await.unwrap();
    let old: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.sql)
            .await
            .unwrap();
    db.repo.migrate().await.unwrap();
    db.repo.migrate().await.unwrap();
    let retained: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=14 ORDER BY version",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    assert_eq!(retained, old);
    db.assert_latest_migration_versions().await;
    assert_eq!(
        requests
            .by_key(&db.session, &r.idempotency_key)
            .await
            .unwrap()
            .request,
        r
    );
    let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM information_schema.tables WHERE table_schema=$1 AND table_name=ANY($2)")
        .bind(&db.schema).bind(vec!["transfer_business_orders_v1","transfer_drafts_v1","transfer_accounts_v1","transfer_payee_routes_v1","transfer_fund_orders_v1","transfer_ledger_v1"])
        .fetch_one(&db.sql).await.unwrap();
    assert_eq!(tables, 6);
    db.finish().await;
}

pub(super) async fn public_draft(db: &Db, v: &VerifiedPermitSnapshot) -> StoredDraft {
    let draft = StoredDraft {
        draft_id: resource("public-contract-draft"),
        operation_id: OperationId::parse("business-original-id").unwrap(),
        task_id: v.binding.task_id.clone(),
        user_id: v.binding.user_id.clone(),
        plan_version: 1,
        status: DraftStatus::ReadyToConfirm,
        expires_at: db_now(db).await + 3600,
        details: Some(DraftDetails {
            account: MaskedAccountView {
                account_id: resource("public-contract-payer"),
                display_name: "Fixture".into(),
                masked_account_no: "**** 0001".into(),
                currency: CnyCurrency::Cny,
            },
            payee: PayeeView {
                payee_id: resource("public-contract-payee"),
                display_name: "Fixture".into(),
                bank_name: "Fixture".into(),
                masked_account_no: "**** 0002".into(),
            },
            amount: amount("2.00"),
            currency: CnyCurrency::Cny,
            remark: String::new(),
            available_balance: amount("10.00"),
            balance_as_of: "2026-10-17T00:00:00Z".into(),
        }),
    };
    provision(
        db,
        &draft,
        &UserId::parse("fixture-recipient").unwrap(),
        &resource("public-contract-recipient"),
        "10.00",
    )
    .await;
    draft
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn transfer_sql_projection_uses_session_current_draft_and_separate_business_version() {
    let db = Db::new().await;
    let v = db.fixture().await;
    let mut draft = public_draft(&db, &v).await;
    draft.plan_version = 2;
    draft.details.as_mut().unwrap().remark = "revised".into();
    let mut tx = db.sql.begin().await.unwrap();
    FundsStore::save_draft(&mut tx, &draft.user_id, Some(1), &draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let read = reader(&db);
    let projected = read
        .project_draft(&db.session, &v.binding.operation_id, &draft.draft_id, 2)
        .await
        .unwrap();
    assert_eq!(projected.runtime_plan_version, 1);
    assert_eq!(projected.request.draft_version, 2);
    assert_eq!(projected.request, request(&draft));
    assert_ne!(
        projected.runtime_operation_id,
        projected.request.operation_id
    );
    assert!(matches!(
        read.project_draft(&db.session, &v.binding.operation_id, &draft.draft_id, 1)
            .await,
        Err(StorageError::ConfirmationBindingMismatch)
    ));
    assert!(matches!(
        read.project_draft(&db.other, &v.binding.operation_id, &draft.draft_id, 2)
            .await,
        Err(StorageError::NotFound)
    ));
    let foreign = db.fixture().await;
    assert!(matches!(
        read.project_draft(
            &db.session,
            &foreign.binding.operation_id,
            &draft.draft_id,
            2
        )
        .await,
        Err(StorageError::ConfirmationBindingMismatch)
    ));
    sqlx::query("UPDATE transfer_drafts_v1 SET expires_at=1 WHERE owner_id=$1")
        .bind(draft.user_id.as_str())
        .execute(&db.sql)
        .await
        .unwrap();
    assert!(matches!(
        read.project_draft(&db.session, &v.binding.operation_id, &draft.draft_id, 2)
            .await,
        Err(StorageError::ConfirmationBindingMismatch)
    ));
    db.sessions.revoke(&db.session);
    assert_eq!(
        read.draft(&db.session, &draft.draft_id).await.unwrap_err(),
        StorageError::Session
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn transfer_sql_real_business_writes_rollback_with_public_callback_and_final_guard() {
    // These permits verify only existing public storage mechanics. There is no
    // approved funds Tool/Profile, funding fingerprint or domain Audit/Outbox here.
    for mode in 0..6 {
        let db = Db::new().await;
        let v = db.fixture().await;
        let draft = public_draft(&db, &v).await;
        let p = db.save(&v).await;
        let id = p.permit_id().clone();
        let binding = p.binding().clone();
        let r = public_request(&v);
        let business = request(&draft);
        let owner = draft.user_id.clone();
        let original_key = IdempotencyKey::parse("isolated-business-key").unwrap();
        if mode >= 4 {
            db.sql.execute("CREATE FUNCTION reject_transfer_test_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated injected transfer failure'; END $$").await.unwrap();
            db.sql.execute(if mode == 4 {
                "CREATE TRIGGER reject_second_transfer_ledger BEFORE INSERT ON transfer_ledger_v1 FOR EACH ROW WHEN (NEW.direction='CREDIT') EXECUTE FUNCTION reject_transfer_test_write()"
            } else {
                "CREATE CONSTRAINT TRIGGER reject_transfer_public_audit AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.kind='OPERATION_TRANSITIONED') EXECUTE FUNCTION reject_transfer_test_write()"
            }).await.unwrap();
        }
        let result=db.store.submission_transaction(&db.session,&r.clone(),move |unit| Box::pin(async move {
            assert!(unit.connection().is_err());
            unit.consume(&id,&binding).await?;
            let tx=unit.connection()?;
            let business_result=FundsStore::submit(tx,&owner,&original_key,&business,MockBehavior::Immediate,
                if mode==3 {MockOutgoingState::Unknown} else {MockOutgoingState::Allowed}).await;
            if mode==3 {
                assert!(matches!(business_result,Err(FundsError::OutgoingUnknown)));
                return Err(StorageError::DependencyUnavailable);
            }
            if mode==4 {
                assert!(matches!(business_result,Err(FundsError::Database(_))));
                return Err(StorageError::Database);
            }
            let record=business_result.map_err(|_|StorageError::CorruptRecord)?;
            assert_eq!(record.latest_receipt.status,safeflow_contracts::ToolResultStatus::Succeeded);
            unit.record_submission(&r).await?;
            match mode {
                0=>Err(StorageError::DependencyUnavailable), // Missing domain Audit/Outbox.
                1=>{
                    sqlx::query("UPDATE operations SET facts_version=facts_version+1 WHERE operation_id=$1").bind(binding.snapshot.operation_id.as_str()).execute(unit.connection()?).await?;
                    Ok(())
                },
                2=>{
                    // Contract28 must not silently accept a business SUCCEEDED terminal.
                    sqlx::query("UPDATE operations SET state='SUCCEEDED' WHERE operation_id=$1").bind(binding.snapshot.operation_id.as_str()).execute(unit.connection()?).await?;
                    Ok(())
                },
                _=>Ok(()), // The deferred audit failure must reject COMMIT.
            }
        })).await;
        assert!(result.is_err());
        if mode >= 4 {
            assert_eq!(result, Err(StorageError::Database));
        }
        assert_eq!(db.status(&p).await, "ACTIVE");
        assert_eq!(db.count("PERMIT_CONSUMED").await, 0);
        let mappings: i64 = sqlx::query_scalar("SELECT count(*) FROM public_execution_requests")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(mappings, 0);
        assert!(reader(&db)
            .business(&db.session, &draft.operation_id)
            .await
            .unwrap()
            .is_none());
        let funds = reader(&db)
            .funds(
                &db.session,
                &draft.details.as_ref().unwrap().account.account_id,
            )
            .await
            .unwrap();
        assert_eq!(funds.booked.as_str(), "10.00");
        assert_eq!(funds.reserved.as_str(), "0.00");
        assert!(reader(&db)
            .transactions(&db.session)
            .await
            .unwrap()
            .is_empty());
        let recipient_funds: i64 = sqlx::query_scalar("SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='public-contract-recipient'").fetch_one(&db.sql).await.unwrap();
        let ledger_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_ledger_v1")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(recipient_funds, 0);
        assert_eq!(ledger_rows, 0);
        assert_eq!(
            reader(&db)
                .draft(&db.session, &draft.draft_id)
                .await
                .unwrap(),
            draft
        );
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn transfer_sql_read_wait_rechecks_versioned_session_without_returning_old_identity() {
    let db = Db::new().await;
    let read = reader(&db);
    let mut locked = db.sql.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *locked)
        .await
        .unwrap();
    locked
        .execute("LOCK TABLE transfer_business_orders_v1 IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let session = db.session.clone();
    let job = tokio::spawn(async move {
        read.business(
            &session,
            &OperationId::parse("not-an-existing-business-id").unwrap(),
        )
        .await
    });
    wait_for_blocked(&db, pid).await;
    db.sessions.bind(&db.session, "user_002", 2);
    locked.commit().await.unwrap();
    assert!(matches!(job.await.unwrap(), Err(StorageError::Session)));
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn transfer_sql_reader_failure_never_becomes_empty_or_bank_failed() {
    let db = Db::new().await;
    let read = reader(&db);
    db.sql
        .execute(
            "ALTER TABLE transfer_business_orders_v1 RENAME TO isolated_missing_business_source",
        )
        .await
        .unwrap();
    assert!(matches!(
        read.business(&db.session, &OperationId::parse("absent").unwrap())
            .await,
        Err(StorageError::Database)
    ));
    db.sql
        .execute(
            "ALTER TABLE isolated_missing_business_source RENAME TO transfer_business_orders_v1",
        )
        .await
        .unwrap();
    assert!(read
        .business(&db.session, &OperationId::parse("absent").unwrap())
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        db.store.request_issuance(&db.session, &new_task_id()).await,
        Err(StorageError::PermitIssuanceUnavailable)
    ));
    db.finish().await;
}
