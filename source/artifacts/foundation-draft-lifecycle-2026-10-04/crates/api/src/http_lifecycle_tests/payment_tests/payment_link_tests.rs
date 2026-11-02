use super::payment_execution_tests::{persistent_state, submit_isolated};
use super::*;
use safeflow_transfer_integration::mock_execution::MockBehavior;

async fn count(db: &Db, table: &str) -> i64 {
    assert!([
        "public_execution_requests",
        "execution_permits",
        "transfer_business_orders_v1",
        "transfer_fund_orders_v1",
        "transfer_ledger_v1",
        "payment_execution_links",
        "operations",
        "audit_events",
        "public_audit_outbox"
    ]
    .contains(&table));
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}

fn endpoint(payment: &Value) -> String {
    format!("{}/execution-link", path(payment))
}
fn input(payment: &Value) -> Value {
    json!({"contract_version":1,"scope":"PAYMENT_REGISTRATION_ONLY",
        "confirmation_ref":payment["confirmation_ref"],"display_ticket":payment["display_ticket"]})
}
async fn register(server: &Server, cookie: &str, payment: &Value) -> Reply {
    command(server, cookie, &endpoint(payment), input(payment)).await
}
async fn confirmed_payment(server: &Server, cookie: &str) -> (Value, Value) {
    let draft = draft(server, cookie).await;
    let payment = prepared(server, cookie, &draft).await;
    let payment = decided(server, cookie, &payment, "CONFIRM").await;
    (draft, payment)
}
async fn assert_no_funds_identity(db: &Db) {
    for table in [
        "public_execution_requests",
        "execution_permits",
        "transfer_business_orders_v1",
        "transfer_fund_orders_v1",
        "transfer_ledger_v1",
    ] {
        assert_eq!(count(db, table).await, 0, "registration changed {table}");
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual registration HTTP"]
async fn payment_link_modified_version_reconfirms_registers_and_cancels_same_business() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (created, payment) = confirmed_payment(&server, &cookie).await;
    let before = funds_state(&db).await;
    let first = ok(register(&server, &cookie, &payment).await);
    assert_eq!(first["registration_operation"]["state"], "DRAFT");
    assert!(first["registration_operation"]["business_operation_ref"].is_null());
    assert_no_funds_identity(&db).await;
    let selected = select(
        &server,
        &cookie,
        &created,
        "MODIFY_MOCK_DRAFT",
        json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,
            "changes":{"amount":"300.00","remark":"new registration version"}}),
    )
    .await;
    let modified = run(&server, &cookie, &selected).await.1;
    let next = prepared(&server, &cookie, &modified).await;
    let next = decided(&server, &cookie, &next, "CONFIRM").await;
    let second = ok(register(&server, &cookie, &next).await);
    assert_eq!(
        second["business_operation_id"],
        first["business_operation_id"]
    );
    assert_eq!(second["normalized_arguments"]["draft_version"], 2);
    assert_eq!(second["normalized_arguments"]["amount"], "300.00");
    assert_ne!(
        second["registration_operation"],
        first["registration_operation"]
    );
    assert_eq!(ok(register(&server, &cookie, &payment).await), first);
    assert_eq!(ok(register(&server, &cookie, &next).await), second);
    let old = ok(query(&server, &cookie, &path(&payment)).await);
    assert_eq!(old["state"], "INVALIDATED");
    assert_eq!(old["execution_link"], first);
    assert_eq!(
        ok(query(&server, &cookie, &path(&next)).await)["execution_link"],
        second
    );
    let cancel = select(
        &server,
        &cookie,
        &modified,
        "CANCEL_MOCK_DRAFT",
        json!({"draft_id":modified["receipt"]["draft_id"],"expected_version":2}),
    )
    .await;
    assert_eq!(
        run(&server, &cookie, &cancel).await.1["task_state"],
        "CANCELLED"
    );
    let before_reads = persistent_state(&db).await;
    for p in [&payment, &next] {
        let result = ok(query(&server, &cookie, &endpoint(p)).await);
        assert_eq!(result["state"], "REGISTERED_UNSUBMITTED");
        assert!(result["binding"]["execution_operation_id"].is_null());
        assert!(result["binding"]["idempotency_key"].is_null());
    }
    assert_eq!(persistent_state(&db).await, before_reads);
    assert_eq!(count(&db, "payment_execution_links").await, 2);
    assert_no_funds_identity(&db).await;
    assert_eq!(funds_state(&db).await, before);
    assert_eq!(server.calls(), 3);
    server.stop();
    db.finish().await;
}

// This checks SQL coexistence only; it does not authorize a funds Tool or submit.
async fn assert_public_slot_available(db: &Db, payment: &Value) {
    let mut tx = db.scoped.begin().await.unwrap();
    let row = sqlx::query("INSERT INTO public_execution_requests(contract_version,owner_id,idempotency_key,business_operation_ref,business_version,task_id,operation_id,node_id,tool_name,task_revision,operation_revision,plan_version,facts_version,arguments_hash,operation_fingerprint,fingerprint_payload,request_digest) SELECT 1,'user_001','isolated-public-key',$1,1,o.task_id,o.operation_id,o.node_id,'isolated-public-contract',1,o.revision,o.plan_version,o.facts_version,o.arguments_hash,repeat('a',64),'{}',repeat('b',64) FROM operations o WHERE o.operation_id=$2")
        .bind(payment["snapshot"]["transfer"]["operation_id"].as_str().unwrap())
        .bind(payment["operation"]["operation_id"].as_str().unwrap())
        .execute(&mut *tx).await.unwrap();
    assert_eq!(row.rows_affected(), 1);
    // Existing unique and immutable protections must still reject conflicts/rewrites.
    sqlx::query("SAVEPOINT public_protection")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(sqlx::query("UPDATE public_execution_requests SET tool_name='rewritten' WHERE idempotency_key='isolated-public-key'").execute(&mut *tx).await.is_err());
    sqlx::query("ROLLBACK TO SAVEPOINT public_protection")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(sqlx::query(
        "INSERT INTO public_execution_requests SELECT * FROM public_execution_requests"
    )
    .execute(&mut *tx)
    .await
    .is_err());
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual registration HTTP"]
async fn payment_link_internal_identity_does_not_reserve_public_funds_slot() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (_, payment) = confirmed_payment(&server, &cookie).await;
    let before = persistent_state(&db).await;
    let link = ok(register(&server, &cookie, &payment).await);
    assert_eq!(link["scope"], "PAYMENT_REGISTRATION_ONLY");
    assert!(link.get("execution_operation").is_none());
    assert!(link.get("idempotency_key").is_none());
    assert!(link["preparation_binding"]["lifecycle_anchor"].is_string());
    assert_eq!(link["confirmation_migration_authorized"], false);
    assert_eq!(link["funds_execution_enabled"], false);
    assert_eq!(
        link["normalized_arguments"],
        payment["snapshot"]["transfer"]
    );
    assert_ne!(
        link["preparation_binding"]["arguments_hash"],
        link["registration_binding"]["transfer_arguments_hash"]
    );
    assert_public_slot_available(&db, &payment).await;
    assert_no_funds_identity(&db).await;
    let after = persistent_state(&db).await;
    assert_ne!(after, before);
    for target in [
        endpoint(&payment),
        path(&payment),
        root(&payment),
        format!("{}/original-result", path(&payment)),
        task_path(&payment),
    ] {
        assert_eq!(
            query(&server, &cookie, &target).await.status,
            StatusCode::OK
        );
    }
    assert_eq!(persistent_state(&db).await, after);
    let result = ok(query(&server, &cookie, &endpoint(&payment)).await);
    assert_eq!(result["execution_link"], link);
    assert_eq!(result["state"], "REGISTERED_UNSUBMITTED");
    assert!(result["first_public_submission"].is_null());
    assert!(result["public_submitted_at"].is_null());
    assert_eq!(server.calls(), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual registration HTTP"]
async fn payment_link_existing_business_and_unknown_submission_only_query_original() {
    for behavior in [
        Some(MockBehavior::Immediate),
        Some(MockBehavior::Pending),
        None,
    ] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (_, payment) = confirmed_payment(&server, &cookie).await;
        if let Some(behavior) = behavior {
            submit_isolated(&db, &payment, behavior).await;
        } else {
            sqlx::query("UPDATE transfer_drafts_v1 SET submitted=true WHERE draft_id=$1")
                .bind(
                    payment["snapshot"]["transfer"]["draft_id"]
                        .as_str()
                        .unwrap(),
                )
                .execute(&db.scoped)
                .await
                .unwrap();
        }
        let before = persistent_state(&db).await;
        assert_eq!(
            register(&server, &cookie, &payment).await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(count(&db, "payment_execution_links").await, 0);
        let result = ok(query(&server, &cookie, &endpoint(&payment)).await);
        assert_eq!(result["state"], "RECONCILE_REQUIRED");
        assert!(result["execution_link"].is_null());
        assert_eq!(persistent_state(&db).await, before);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual registration HTTP"]
async fn payment_link_strict_input_unconfirmed_rejected_and_foreign_owner() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    let before = persistent_state(&db).await;
    assert_eq!(
        register(&server, &cookie, &payment).await.status,
        StatusCode::CONFLICT
    );
    for field in [
        "owner_id",
        "amount",
        "operation_id",
        "business_operation_ref",
        "idempotency_key",
        "registration_key",
        "outcome",
    ] {
        let mut forged = input(&payment);
        forged[field] = json!("forged");
        assert_eq!(
            command(&server, &cookie, &endpoint(&payment), forged)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    for field in ["confirmation_ref", "display_ticket"] {
        let mut forged = input(&payment);
        forged[field] = json!("forged");
        assert_ne!(
            command(&server, &cookie, &endpoint(&payment), forged)
                .await
                .status,
            StatusCode::OK
        );
    }
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}?amount=1", endpoint(&payment)),
            input(&payment)
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(persistent_state(&db).await, before);
    let other = Server::with_draft(
        &db,
        "user_002",
        Duration::from_secs(60),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "link-other-owner",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let stranger = other.login().await;
    assert_eq!(
        register(&other, &stranger, &payment).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        query(&other, &stranger, &endpoint(&payment)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                &endpoint(&payment),
                Some(input(&payment).to_string()),
                Some(&cookie),
                None,
                None,
                false
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let rejected = decided(&server, &cookie, &payment, "REJECT").await;
    assert_eq!(rejected["state"], "REJECTED");
    let before = persistent_state(&db).await;
    assert_eq!(
        register(&server, &cookie, &payment).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(persistent_state(&db).await, before);
    assert_no_funds_identity(&db).await;
    other.stop();
    server.stop();
    db.finish().await;
}

async fn install_barrier(db: &Db, key: i64) {
    db.scoped.execute(format!("CREATE FUNCTION registration_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'payment_registration' THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$; CREATE TRIGGER registration_barrier BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION registration_barrier()").as_str()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and deterministic registration barriers"]
async fn payment_link_concurrent_replay_one_operation_audit_and_outbox() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (_, payment) = confirmed_payment(&server, &cookie).await;
    let before = funds_state(&db).await;
    let audits = count(&db, "audit_events").await;
    let outbox = count(&db, "public_audit_outbox").await;
    let key = 200_000
        + i64::from(u16::from_le_bytes(
            *uuid::Uuid::new_v4().as_bytes().first_chunk::<2>().unwrap(),
        ));
    let mut held = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    install_barrier(&db, key).await;
    let first = spawn_post(&server, &cookie, endpoint(&payment), input(&payment));
    wait_advisory(&db, key).await;
    let second = spawn_post(&server, &cookie, endpoint(&payment), input(&payment));
    // The second registration must actually wait behind the first owner's transaction.
    let mut waited = false;
    for _ in 0..200 {
        waited = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE '%current_schema() || $1%' AND cardinality(pg_blocking_pids(pid))>0)")
            .fetch_one(&db.admin).await.unwrap();
        if waited {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waited, "second registration never waited on the owner lock");
    held.rollback().await.unwrap();
    let first = ok(first.await.unwrap());
    assert_eq!(ok(second.await.unwrap()), first);
    let state = persistent_state(&db).await;
    assert_eq!(ok(register(&server, &cookie, &payment).await), first);
    assert_eq!(persistent_state(&db).await, state);
    assert_eq!(count(&db, "payment_execution_links").await, 1);
    assert_eq!(count(&db, "operations").await, 3); // draft, preparation, registration
    assert_eq!(count(&db, "audit_events").await, audits + 1);
    assert_eq!(count(&db, "public_audit_outbox").await, outbox + 1);
    let audit: Value = sqlx::query_scalar("SELECT envelope->'public_event'->'details'->'payment_registration' FROM public_audit_outbox WHERE envelope->'public_event'->'details' ? 'payment_registration'")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(audit["preparation_binding"], first["preparation_binding"]);
    assert_eq!(audit["registration_binding"], first["registration_binding"]);
    assert_eq!(funds_state(&db).await, before);
    assert_no_funds_identity(&db).await;
    eprintln!("registered immutable internal association: {first}");
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and transaction fault injection"]
async fn payment_link_outbox_error_rolls_back_link_operation_and_audit() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (_, payment) = confirmed_payment(&server, &cookie).await;
    db.scoped.execute("CREATE FUNCTION fail_registration_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.envelope->'public_event'->'details' ? 'payment_registration' THEN RAISE EXCEPTION 'injected registration Outbox failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_registration_outbox BEFORE INSERT ON public_audit_outbox FOR EACH ROW EXECUTE FUNCTION fail_registration_outbox()").await.unwrap();
    let before = persistent_state(&db).await;
    assert_eq!(
        register(&server, &cookie, &payment).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(persistent_state(&db).await, before);
    assert_eq!(count(&db, "payment_execution_links").await, 0);
    db.scoped
        .execute("DROP TRIGGER fail_registration_outbox ON public_audit_outbox")
        .await
        .unwrap();
    ok(register(&server, &cookie, &payment).await);
    assert_no_funds_identity(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL, actual expiry and deterministic SQL barrier"]
async fn payment_link_expiry_or_session_revocation_during_save_rolls_back_everything() {
    for revoke in [false, true] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = Server::with_draft(
            &db,
            "user_001",
            Duration::from_secs(30),
            Duration::from_secs(600),
            Some(
                DraftConfirmationConfig::new(
                    Duration::from_secs(300),
                    Duration::from_secs(300),
                    if revoke { 600 } else { 3 },
                    "link-expiry-barrier",
                )
                .unwrap()
                .with_lifecycle_v2()
                .unwrap(),
            ),
        )
        .await;
        let cookie = server.login().await;
        let (_, payment) = confirmed_payment(&server, &cookie).await;
        let before = persistent_state(&db).await;
        let key = 300_000
            + i64::from(u16::from_le_bytes(
                *uuid::Uuid::new_v4().as_bytes().first_chunk::<2>().unwrap(),
            ));
        let mut held = db.scoped.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *held)
            .await
            .unwrap();
        install_barrier(&db, key).await;
        let request = spawn_post(&server, &cookie, endpoint(&payment), input(&payment));
        wait_advisory(&db, key).await;
        if revoke {
            assert_eq!(
                server
                    .call(
                        Method::DELETE,
                        "/api/sim/session",
                        Some("{}".into()),
                        Some(&cookie),
                        None,
                        None,
                        true
                    )
                    .await
                    .status,
                StatusCode::NO_CONTENT
            );
        } else {
            let mut expired = false;
            for _ in 0..500 {
                expired = sqlx::query_scalar("SELECT clock_timestamp()>=expires_at FROM server_confirmations WHERE confirmation_ref=$1")
                    .bind(payment["confirmation_ref"].as_str().unwrap()).fetch_one(&db.scoped).await.unwrap();
                if expired {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(expired, "server confirmation did not expire");
        }
        held.rollback().await.unwrap();
        assert_eq!(
            request.await.unwrap().status,
            if revoke {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::CONFLICT
            }
        );
        assert_eq!(persistent_state(&db).await, before);
        let fresh = if revoke {
            server.login().await
        } else {
            cookie.clone()
        };
        let state = persistent_state(&db).await;
        assert_ne!(
            register(&server, &fresh, &payment).await.status,
            StatusCode::OK
        );
        let history = ok(query(&server, &fresh, &path(&payment)).await);
        assert_eq!(history["state"], "INVALIDATED");
        assert_eq!(
            persistent_state(&db).await,
            state,
            "GET must not persist invalidations"
        );
        assert_no_funds_identity(&db).await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and trusted source comparisons"]
async fn payment_link_changed_amount_payee_or_business_version_cannot_register_old_snapshot() {
    for sql in [
        "UPDATE transfer_drafts_v1 SET details=jsonb_set(details,'{amount}','\"201.00\"')",
        "UPDATE transfer_drafts_v1 SET details=jsonb_set(details,'{payee,payee_id}','\"payee_009\"')",
        "UPDATE transfer_drafts_v1 SET business_version=2",
    ] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let (_, payment) = confirmed_payment(&server, &cookie).await;
        db.scoped.execute(sql).await.unwrap();
        let before = persistent_state(&db).await;
        assert_eq!(
            register(&server, &cookie, &payment).await.status,
            StatusCode::CONFLICT,
            "changed source: {sql}"
        );
        assert_eq!(persistent_state(&db).await, before);
        assert_eq!(count(&db, "payment_execution_links").await, 0);
        assert_no_funds_identity(&db).await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real restarted HTTP processes"]
async fn payment_link_restart_recovers_same_registration_without_reviving_confirmation() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let first = ProcessServer::start(&db).await;
    let cookie = first.login().await;
    let confirmed = first.confirmed(&cookie).await;
    let created = ok(first
        .call(
            Method::POST,
            &format!("{}/execute", operation_path(&confirmed)),
            Some(execution(&confirmed)),
            Some(&cookie),
        )
        .await);
    let payment = ok(first
        .call(
            Method::POST,
            &root(&created),
            Some(prepare_input(&created)),
            Some(&cookie),
        )
        .await);
    ok(first
        .call(
            Method::POST,
            &format!("{}/confirm", path(&payment)),
            Some(decision(&payment, "CONFIRM")),
            Some(&cookie),
        )
        .await);
    let link = ok(first
        .call(
            Method::POST,
            &endpoint(&payment),
            Some(input(&payment)),
            Some(&cookie),
        )
        .await);
    first.kill();
    let second = ProcessServer::start(&db).await;
    assert_eq!(
        second
            .call(Method::GET, &endpoint(&payment), None, Some(&cookie))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = second.login().await;
    let before = persistent_state(&db).await;
    let result = ok(second
        .call(Method::GET, &endpoint(&payment), None, Some(&fresh))
        .await);
    assert_eq!(result["state"], "REGISTERED_UNSUBMITTED");
    assert_eq!(result["execution_link"], link);
    assert!(result["binding"]["execution_operation_id"].is_null());
    let all = ok(second
        .call(Method::GET, &root(&payment), None, Some(&fresh))
        .await);
    assert_eq!(all[0]["execution_link"], link);
    assert_eq!(all[0]["invalid_reason"], "SESSION_CHANGED");
    assert_eq!(
        ok(second
            .call(
                Method::POST,
                &endpoint(&payment),
                Some(input(&payment)),
                Some(&fresh)
            )
            .await),
        link
    );
    assert_eq!(persistent_state(&db).await, before);
    assert_no_funds_identity(&db).await;
    second.kill();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and immutable registration SQL"]
async fn payment_link_metadata_and_registration_operation_cannot_be_promoted_or_rewritten() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (_, payment) = confirmed_payment(&server, &cookie).await;
    let link = ok(register(&server, &cookie, &payment).await);
    let before = persistent_state(&db).await;
    for sql in [
        "UPDATE payment_execution_links SET business_version=2",
        "DELETE FROM payment_execution_links",
        "UPDATE operations SET state='READY_TO_EXECUTE' WHERE operation_id IN (SELECT registration_operation_id FROM payment_execution_links)",
        "UPDATE operations SET state='SUBMITTED',revision=revision+1 WHERE operation_id IN (SELECT registration_operation_id FROM payment_execution_links)",
        "DELETE FROM operations WHERE operation_id IN (SELECT registration_operation_id FROM payment_execution_links)",
    ] { assert!(db.scoped.execute(sql).await.is_err(), "must reject {sql}"); }
    assert_eq!(persistent_state(&db).await, before);
    let uri = format!(
        "{}/operations/{}/execute",
        task_path(&payment),
        link["registration_operation"]["operation_id"]
            .as_str()
            .unwrap()
    );
    assert_ne!(
        command(&server, &cookie, &uri, input(&payment))
            .await
            .status,
        StatusCode::OK
    );
    assert_no_funds_identity(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and original member SQL recovery"]
async fn payment_link_later_business_original_takes_precedence_over_internal_registration() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (_, payment) = confirmed_payment(&server, &cookie).await;
    let link = ok(register(&server, &cookie, &payment).await);
    // Existing isolated member submission helper, never product registration authority.
    submit_isolated(&db, &payment, MockBehavior::Pending).await;
    let before = persistent_state(&db).await;
    let result = ok(query(&server, &cookie, &endpoint(&payment)).await);
    assert_eq!(result["state"], "RECONCILE_REQUIRED");
    assert_eq!(result["reason"], "BUSINESS_RESULT_WITHOUT_PUBLIC_ORIGINAL");
    assert_eq!(
        result["verified_business_result"]["latest_receipt"]["status"],
        "PROCESSING"
    );
    assert_eq!(
        result["binding"]["idempotency_key"],
        "isolated-original-key"
    );
    assert_eq!(result["execution_link"], link);
    assert_eq!(
        ok(query(&server, &cookie, &path(&payment)).await)["business_recovery_required"],
        true
    );
    assert_eq!(persistent_state(&db).await, before);
    assert_eq!(count(&db, "public_execution_requests").await, 0);
    assert_eq!(count(&db, "execution_permits").await, 0);
    server.stop();
    db.finish().await;
}
