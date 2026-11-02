//! Foundation HTTP/SQL mechanism fixtures, not member business sample JSON.
use super::payment_execution_tests::{persistent_state, submit_isolated};
use super::*;
use safeflow_contracts::ResourceId;
use safeflow_transfer_integration::{funds_store::FundsStore, mock_execution::MockBehavior};

fn observations(v: &Value) -> &Vec<Value> {
    v["business_preflight"]["observations"].as_array().unwrap()
}
fn latest(v: &Value) -> &Value {
    observations(v).last().unwrap()
}
fn reason(v: &Value) -> &str {
    latest(v)["findings"][0]["reason_code"].as_str().unwrap()
}
async fn evaluate(server: &Server, cookie: &str, p: &Value) -> Reply {
    command(
        server,
        cookie,
        &format!("{}/evaluate", path(p)),
        decision(p, "CONFIRM"),
    )
    .await
}
async fn all_state(db: &Db) -> Vec<Value> {
    let mut state = persistent_state(db).await;
    state.push(sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(o) ORDER BY ordinal),'[]') FROM payment_business_observations o")
        .fetch_one(&db.scoped).await.unwrap());
    state
}
async fn no_execution(db: &Db) {
    let counts: (i64,i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM public_execution_requests),(SELECT count(*) FROM execution_permits),(SELECT count(*) FROM transfer_business_orders_v1),(SELECT count(*) FROM transfer_fund_orders_v1),(SELECT count(*) FROM transfer_ledger_v1)")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(counts, (0, 0, 0, 0, 0));
}
async fn route(db: &Db, p: &Value, same: bool) {
    let mut tx = db.scoped.begin().await.unwrap();
    let owner = UserId::parse("user_001").unwrap();
    let recipient = ResourceId::parse(if same {
        "account_001"
    } else {
        "preflight_recipient"
    })
    .unwrap();
    if !same {
        FundsStore::provision_account(
            &mut tx,
            &UserId::parse("recipient_owner").unwrap(),
            &recipient,
            &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
        )
        .await
        .unwrap();
    }
    FundsStore::provision_payee(
        &mut tx,
        &owner,
        &serde_json::from_value(p["snapshot"]["transfer"]["payee_id"].clone()).unwrap(),
        &recipient,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn expiring_server(db: &Db) -> Server {
    Server::with_draft(
        db,
        "user_001",
        Duration::from_secs(60),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(2),
                Duration::from_secs(300),
                600,
                "payment-observation-expiry-regression",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await
}

async fn wait_confirmation_expiry(db: &Db, payment: &Value) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let expired: bool = sqlx::query_scalar(
                "SELECT clock_timestamp()>=expires_at FROM server_confirmations WHERE confirmation_ref=$1",
            )
            .bind(payment["confirmation_ref"].as_str().unwrap())
            .fetch_one(&db.scoped)
            .await
            .unwrap();
            if expired {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("confirmation must expire according to the database clock");
}

async fn forbid_new_observations(db: &Db) {
    db.scoped.execute("CREATE FUNCTION forbid_unusable_observation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'UNUSABLE_OBSERVATION_MUST_NOT_BE_ATTEMPTED'; END $$; CREATE TRIGGER forbid_unusable_observation BEFORE INSERT ON payment_business_observations FOR EACH ROW EXECUTE FUNCTION forbid_unusable_observation(); CREATE TRIGGER forbid_unusable_observation_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.details ? 'payment_business_preflight') EXECUTE FUNCTION forbid_unusable_observation()").await.unwrap();
}

async fn saved_invalidation(db: &Db, payment: &Value) -> (String, Option<String>, i64) {
    sqlx::query_as("SELECT c.status,p.invalid_reason,(SELECT count(*) FROM audit_events a WHERE a.operation_id=p.operation_id AND a.kind='CONFIRMATION_REVOKED') FROM payment_preparations p JOIN server_confirmations c USING(confirmation_ref) WHERE p.operation_id=$1")
        .bind(payment["operation"]["operation_id"].as_str().unwrap())
        .fetch_one(&db.scoped).await.unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual confirmation expiry"]
async fn payment_business_http_expired_first_confirmation_saves_revocation_without_observation() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = expiring_server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    forbid_new_observations(&db).await;
    wait_confirmation_expiry(&db, &payment).await;
    let before = all_state(&db).await;
    let projected = ok(query(&server, &cookie, &path(&payment)).await);
    assert_eq!(projected["invalid_reason"], "EXPIRED");
    assert_eq!(all_state(&db).await, before);
    assert_eq!(
        saved_invalidation(&db, &payment).await,
        ("PREPARED".into(), None, 0)
    );
    let funds = funds_state(&db).await;
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        saved_invalidation(&db, &payment).await,
        ("REVOKED".into(), Some("EXPIRED".into()), 1)
    );
    let persisted = ok(query(&server, &cookie, &path(&payment)).await);
    assert!(observations(&persisted).is_empty());
    assert_eq!(persisted["confirmation_status"], "REVOKED");
    assert_eq!(persisted["invalid_reason"], "EXPIRED");
    assert!(persisted["security_decisions"]
        .as_array()
        .unwrap()
        .is_empty());
    let stable = all_state(&db).await;
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        evaluate(&server, &cookie, &payment).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        ok(query(&server, &cookie, &path(&payment)).await),
        persisted
    );
    assert_eq!(all_state(&db).await, stable);
    assert_eq!(funds_state(&db).await, funds);
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual confirmation expiry"]
async fn payment_business_http_expired_confirm_and_evaluate_preserve_observation_and_invalidation()
{
    for action in ["confirm", "evaluate"] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = expiring_server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let payment = prepared(&server, &cookie, &created).await;
        let confirmed = decided(&server, &cookie, &payment, "CONFIRM").await;
        assert_eq!(observations(&confirmed).len(), 1);
        forbid_new_observations(&db).await;
        wait_confirmation_expiry(&db, &payment).await;
        let before = all_state(&db).await;
        let projected = ok(query(&server, &cookie, &path(&payment)).await);
        assert_eq!(
            projected["business_preflight"]["freshness"],
            "PREPARATION_INVALIDATED"
        );
        assert_eq!(observations(&projected), observations(&confirmed));
        assert_eq!(all_state(&db).await, before);
        assert_eq!(
            saved_invalidation(&db, &payment).await,
            ("CONFIRMED".into(), None, 0)
        );
        let funds = funds_state(&db).await;
        assert_eq!(
            command(
                &server,
                &cookie,
                &format!("{}/{action}", path(&payment)),
                decision(&payment, "CONFIRM")
            )
            .await
            .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            saved_invalidation(&db, &payment).await,
            ("REVOKED".into(), Some("EXPIRED".into()), 1)
        );
        let persisted = ok(query(&server, &cookie, &path(&payment)).await);
        assert_eq!(observations(&persisted), observations(&confirmed));
        assert_eq!(persisted["invalid_reason"], "EXPIRED");
        assert_eq!(persisted["security_decisions"].as_array().unwrap().len(), 1);
        assert_eq!(persisted["security_decisions"][0]["status"], "INVALIDATED");
        let stable = all_state(&db).await;
        for retry in ["confirm", "evaluate"] {
            assert_eq!(
                command(
                    &server,
                    &cookie,
                    &format!("{}/{retry}", path(&payment)),
                    decision(&payment, "CONFIRM")
                )
                .await
                .status,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            ok(query(&server, &cookie, &path(&payment)).await),
            persisted
        );
        assert_eq!(all_state(&db).await, stable);
        assert_eq!(funds_state(&db).await, funds);
        no_execution(&db).await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_rejected_confirmation_never_appends_observation_or_revives() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    let rejected = decided(&server, &cookie, &payment, "REJECT").await;
    forbid_new_observations(&db).await;
    let before = all_state(&db).await;
    for action in ["confirm", "evaluate"] {
        assert_eq!(
            command(
                &server,
                &cookie,
                &format!("{}/{action}", path(&payment)),
                decision(&payment, "CONFIRM")
            )
            .await
            .status,
            StatusCode::CONFLICT
        );
    }
    assert_eq!(ok(query(&server, &cookie, &path(&payment)).await), rejected);
    assert_eq!(all_state(&db).await, before);
    assert_eq!(
        saved_invalidation(&db, &payment).await,
        ("REVOKED".into(), Some("REJECTED".into()), 1)
    );
    assert!(observations(&rejected).is_empty());
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_real_policy_sources_replay_and_explicit_refresh() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    assert!(observations(&p).is_empty());
    let before = funds_state(&db).await;
    let a = decided(&server, &cookie, &p, "CONFIRM").await;
    assert_eq!(reason(&a), "PAYEE_ROUTE_UNAVAILABLE");
    assert_eq!(latest(&a)["facts"]["funds"]["available"], "1288.00");
    assert_eq!(
        latest(&a)["context"]["normalized_arguments"],
        p["snapshot"]["transfer"]
    );
    assert_eq!(latest(&a)["context"]["authenticated_user_id"], "user_001");
    assert_eq!(latest(&a)["facts"]["key_scope"], "INTERNAL_COMPARISON_ONLY");
    assert_eq!(
        a["business_preflight"]["formal_execution_binding_adopted"],
        false
    );
    assert_eq!(
        a["security_decisions"][0]["assembly_metadata"]["kernel"]["domain_policy_evaluated"],
        false
    );
    assert_eq!(funds_state(&db).await, before);
    assert_eq!(decided(&server, &cookie, &p, "CONFIRM").await, a);
    assert_eq!(ok(evaluate(&server, &cookie, &p).await), a);
    route(&db, &p, false).await;
    let before = all_state(&db).await;
    let historical = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(
        historical["business_preflight"]["freshness"],
        "HISTORICAL_REEVALUATION_REQUIRED"
    );
    assert_eq!(observations(&historical), observations(&a));
    assert_eq!(all_state(&db).await, before);
    let replay = decided(&server, &cookie, &p, "CONFIRM").await;
    assert_eq!(observations(&replay).len(), 1);
    let funds = funds_state(&db).await;
    let refreshed = ok(evaluate(&server, &cookie, &p).await);
    assert_eq!(reason(&refreshed), "OUTGOING_STATE_UNKNOWN");
    assert_eq!(observations(&refreshed).len(), 2);
    assert_eq!(
        latest(&refreshed)["facts"]["daily"]["scope"],
        "OWNER_ALL_ACCOUNTS_PUBLIC_FIRST_SUBMISSION"
    );
    assert_eq!(
        latest(&refreshed)["facts"]["outgoing"]["missing_reason"],
        "CARD_TRANSACTION_REPOSITORY_NOT_CONNECTED"
    );
    assert_eq!(latest(&refreshed)["findings"].as_array().unwrap().len(), 1);
    assert_eq!(ok(evaluate(&server, &cookie, &p).await), refreshed);
    assert_eq!(ok(query(&server, &cookie, &path(&p)).await), refreshed);
    assert_eq!(funds_state(&db).await, funds);
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_same_account_and_unknown_card_are_different_actual_findings() {
    for same in [true, false] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let p = prepared(&server, &cookie, &created).await;
        route(&db, &p, same).await;
        // Real low balance is visible, but the member Policy stops at Unknown first.
        sqlx::query(
            "UPDATE transfer_accounts_v1 SET booked_cents=1 WHERE account_id='account_001'",
        )
        .execute(&db.scoped)
        .await
        .unwrap();
        let before = funds_state(&db).await;
        let result = decided(&server, &cookie, &p, "CONFIRM").await;
        assert_eq!(
            reason(&result),
            if same {
                "INVALID_SELF_ROUTE"
            } else {
                "OUTGOING_STATE_UNKNOWN"
            }
        );
        assert_eq!(latest(&result)["facts"]["funds"]["available"], "0.01");
        assert_eq!(funds_state(&db).await, before);
        no_execution(&db).await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_observation_save_and_audit_failures_rollback_everything() {
    for failure in ["save", "audit", "outbox", "commit"] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let p = prepared(&server, &cookie, &created).await;
        db.scoped.execute("CREATE FUNCTION reject_preflight() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'ISOLATED_PREFLIGHT_FAILURE'; END $$").await.unwrap();
        let trigger = match failure {
            "save" => "CREATE TRIGGER reject_preflight BEFORE INSERT ON payment_business_observations FOR EACH ROW EXECUTE FUNCTION reject_preflight()",
            "audit" => "CREATE TRIGGER reject_preflight BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.details ? 'payment_business_preflight') EXECUTE FUNCTION reject_preflight()",
            "outbox" => "CREATE TRIGGER reject_preflight BEFORE INSERT ON public_audit_outbox FOR EACH ROW WHEN (NEW.envelope->'public_event'->'details' ? 'payment_business_preflight') EXECUTE FUNCTION reject_preflight()",
            _ => "CREATE CONSTRAINT TRIGGER reject_preflight AFTER INSERT ON payment_business_observations DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_preflight()",
        };
        db.scoped.execute(trigger).await.unwrap();
        let before = all_state(&db).await;
        assert_ne!(
            command(
                &server,
                &cookie,
                &format!("{}/confirm", path(&p)),
                decision(&p, "CONFIRM")
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(all_state(&db).await, before, "{failure}");
        no_execution(&db).await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_changed_draft_denied_and_new_preparation_can_modify_cancel() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    let first = decided(&server, &cookie, &p, "CONFIRM").await;
    let selected = select(&server, &cookie, &created, "MODIFY_MOCK_DRAFT",
        json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"remark":"new preflight version"}})).await;
    let modified = run(&server, &cookie, &selected).await.1;
    let before = all_state(&db).await;
    let history = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(
        history["business_preflight"]["freshness"],
        "PREPARATION_INVALIDATED"
    );
    assert_eq!(observations(&history), observations(&first));
    assert_eq!(all_state(&db).await, before);
    assert_eq!(
        evaluate(&server, &cookie, &p).await.status,
        StatusCode::CONFLICT
    );
    let denied = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(reason(&denied), "FUNDS_DRAFT_NOT_EXECUTABLE");
    assert_eq!(observations(&denied).len(), 2);
    let next = prepared(&server, &cookie, &modified).await;
    let next = decided(&server, &cookie, &next, "CONFIRM").await;
    assert_ne!(
        next["operation"]["operation_id"],
        p["operation"]["operation_id"]
    );
    assert_eq!(next["snapshot"]["transfer"]["draft_version"], 2);
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
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_original_order_remains_query_only_with_actual_key() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    let confirmed = decided(&server, &cookie, &p, "CONFIRM").await;
    // Existing member settlement fixture is separate from HTTP authority.
    submit_isolated(&db, &p, MockBehavior::Immediate).await;
    let before = all_state(&db).await;
    let history = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(observations(&history), observations(&confirmed));
    assert_eq!(all_state(&db).await, before);
    assert_eq!(
        evaluate(&server, &cookie, &p).await.status,
        StatusCode::CONFLICT
    );
    let result = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(reason(&result), "FUNDS_ORIGINAL_REQUIRES_QUERY");
    assert_eq!(
        latest(&result)["facts"]["key_scope"],
        "OBSERVED_ORIGINAL_KEY_QUERY_ONLY"
    );
    assert_eq!(
        latest(&result)["facts"]["original_key"],
        "isolated-original-key"
    );
    assert_eq!(result["funds_execution_enabled"], false);
    let before_reads = all_state(&db).await;
    ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(all_state(&db).await, before_reads);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and deterministic SQL barriers"]
async fn payment_business_http_concurrent_confirmation_retries_snapshot_and_saves_once() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    let key = 600_000
        + i64::from(u16::from_le_bytes(
            *uuid::Uuid::new_v4().as_bytes().first_chunk::<2>().unwrap(),
        ));
    db.scoped.execute(format!("CREATE FUNCTION preflight_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'payment_business_preflight' THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$; CREATE TRIGGER preflight_barrier BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION preflight_barrier()").as_str()).await.unwrap();
    let mut held = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    let endpoint = format!("{}/confirm", path(&p));
    let first = spawn_post(&server, &cookie, endpoint.clone(), decision(&p, "CONFIRM"));
    wait_advisory(&db, key).await;
    let second = spawn_post(&server, &cookie, endpoint, decision(&p, "CONFIRM"));
    let mut waited = false;
    for _ in 0..200 {
        waited = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE '%current_schema() || $1%' AND cardinality(pg_blocking_pids(pid))>0)")
            .fetch_one(&db.admin).await.unwrap();
        if waited {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        waited,
        "second confirmation must actually wait for the owner"
    );
    held.rollback().await.unwrap();
    let first = ok(first.await.unwrap());
    assert_eq!(ok(second.await.unwrap()), first);
    assert_eq!(observations(&first).len(), 1);
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM payment_business_observations),(SELECT count(*) FROM security_decisions WHERE source_mode='TRUSTED_PAYMENT_MINIMUM_V1'),(SELECT count(*) FROM audit_events WHERE details ? 'payment_business_preflight')")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(counts, (1, 1, 1));
    route(&db, &p, false).await;
    let mut held = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    let endpoint = format!("{}/evaluate", path(&p));
    let a = spawn_post(&server, &cookie, endpoint.clone(), decision(&p, "CONFIRM"));
    wait_advisory(&db, key).await;
    let b = spawn_post(&server, &cookie, endpoint, decision(&p, "CONFIRM"));
    let mut waited = false;
    for _ in 0..200 {
        waited = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE '%current_schema() || $1%' AND cardinality(pg_blocking_pids(pid))>0)")
            .fetch_one(&db.admin).await.unwrap();
        if waited {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waited, "second evaluation must wait for the owner");
    held.rollback().await.unwrap();
    let a = ok(a.await.unwrap());
    assert_eq!(ok(b.await.unwrap()), a);
    assert_eq!(observations(&a).len(), 2);
    assert_eq!(reason(&a), "OUTGOING_STATE_UNKNOWN");
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_identity_and_version_inputs_cannot_supply_policy_facts() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    let stranger = Server::with_draft(
        &db,
        "other_owner",
        Duration::from_secs(60),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "preflight-foreign-owner",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let foreign = stranger.login().await;
    let before = all_state(&db).await;
    assert_eq!(
        query(&stranger, &foreign, &path(&p)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        command(
            &stranger,
            &foreign,
            &format!("{}/confirm", path(&p)),
            decision(&p, "CONFIRM")
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    for (field, value) in [
        ("owner_id", json!("other_owner")),
        ("balance", json!("999999.00")),
        ("outgoing", json!("ALLOWED")),
        ("findings", json!([])),
        ("draft_version", json!(99)),
        ("permit", json!("forged")),
    ] {
        let mut forged = decision(&p, "CONFIRM");
        forged[field] = value;
        assert_eq!(
            command(&server, &cookie, &format!("{}/confirm", path(&p)), forged)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(all_state(&db).await, before);
    no_execution(&db).await;
    stranger.stop();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real HTTP process restart"]
async fn payment_business_http_restart_retains_history_without_restoring_confirmation_authority() {
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
    let p = ok(first
        .call(
            Method::POST,
            &root(&created),
            Some(prepare_input(&created)),
            Some(&cookie),
        )
        .await);
    let p = ok(first
        .call(
            Method::POST,
            &format!("{}/confirm", path(&p)),
            Some(decision(&p, "CONFIRM")),
            Some(&cookie),
        )
        .await);
    assert_eq!(reason(&p), "PAYEE_ROUTE_UNAVAILABLE");
    first.kill();
    let before = all_state(&db).await;
    let second = ProcessServer::start(&db).await;
    assert_eq!(
        second
            .call(Method::GET, &path(&p), None, Some(&cookie))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = second.login().await;
    let recovered = ok(second
        .call(Method::GET, &path(&p), None, Some(&fresh))
        .await);
    assert_eq!(observations(&recovered), observations(&p));
    assert_eq!(
        recovered["business_preflight"]["freshness"],
        "PREPARATION_INVALIDATED"
    );
    assert_eq!(all_state(&db).await, before);
    no_execution(&db).await;
    second.kill();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_original_content_conflict_is_a_real_policy_finding() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    decided(&server, &cookie, &p, "CONFIRM").await;
    let selected = select(&server,&cookie,&created,"MODIFY_MOCK_DRAFT",
        json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"remark":"different original content"}})).await;
    let modified = run(&server, &cookie, &selected).await.1;
    let next = prepared(&server, &cookie, &modified).await;
    submit_isolated(&db, &next, MockBehavior::Immediate).await;
    let funds = funds_state(&db).await;
    assert_eq!(
        evaluate(&server, &cookie, &p).await.status,
        StatusCode::CONFLICT
    );
    let denied = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(reason(&denied), "FUNDS_IDEMPOTENCY_CONFLICT");
    assert_eq!(
        latest(&denied)["facts"]["original_request"]["remark"],
        "different original content"
    );
    assert_eq!(
        latest(&denied)["context"]["normalized_arguments"]["remark"],
        ""
    );
    assert_eq!(funds_state(&db).await, funds);
    let formal: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM public_execution_requests),(SELECT count(*) FROM execution_permits)")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(formal, (0, 0));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and deterministic SQL barrier"]
async fn payment_business_http_minimum_and_policy_share_one_snapshot_despite_concurrent_source_change(
) {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    let key = 700_000
        + i64::from(u16::from_le_bytes(
            *uuid::Uuid::new_v4().as_bytes().first_chunk::<2>().unwrap(),
        ));
    db.scoped.execute(format!("CREATE FUNCTION minimum_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}::bigint); RETURN NEW; END $$; CREATE TRIGGER minimum_barrier BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='SECURITY_DECISION_RECORDED') EXECUTE FUNCTION minimum_barrier()").as_str()).await.unwrap();
    let mut held = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    let attempt = spawn_post(
        &server,
        &cookie,
        format!("{}/confirm", path(&p)),
        decision(&p, "CONFIRM"),
    );
    wait_advisory(&db, key).await;
    route(&db, &p, false).await;
    sqlx::query("UPDATE transfer_accounts_v1 SET booked_cents=1 WHERE account_id='account_001'")
        .execute(&db.scoped)
        .await
        .unwrap();
    held.rollback().await.unwrap();
    let first = ok(attempt.await.unwrap());
    assert_eq!(reason(&first), "PAYEE_ROUTE_UNAVAILABLE");
    assert_eq!(latest(&first)["facts"]["funds"]["available"], "1288.00");
    let before = all_state(&db).await;
    let history = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(
        history["business_preflight"]["freshness"],
        "HISTORICAL_REEVALUATION_REQUIRED"
    );
    assert_eq!(all_state(&db).await, before);
    let refreshed = ok(evaluate(&server, &cookie, &p).await);
    assert_eq!(reason(&refreshed), "OUTGOING_STATE_UNKNOWN");
    assert_eq!(latest(&refreshed)["facts"]["funds"]["available"], "0.01");
    assert_eq!(observations(&refreshed).len(), 2);
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_business_http_foreign_draft_source_produces_unavailable_without_disclosure() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &created).await;
    sqlx::query("UPDATE transfer_drafts_v1 SET owner_id='other_owner' WHERE draft_id=$1")
        .bind(p["snapshot"]["transfer"]["draft_id"].as_str().unwrap())
        .execute(&db.scoped)
        .await
        .unwrap();
    let funds = funds_state(&db).await;
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&p)),
            decision(&p, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let denied = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(reason(&denied), "FUNDS_DRAFT_UNAVAILABLE");
    assert!(latest(&denied)["facts"]["draft"].is_null());
    assert_eq!(
        latest(&denied)["context"]["authenticated_user_id"],
        "user_001"
    );
    assert!(!latest(&denied).to_string().contains("other_owner"));
    assert_eq!(funds_state(&db).await, funds);
    no_execution(&db).await;
    server.stop();
    db.finish().await;
}
