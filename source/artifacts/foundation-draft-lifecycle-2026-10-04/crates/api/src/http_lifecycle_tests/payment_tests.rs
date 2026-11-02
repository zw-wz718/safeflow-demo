use super::process_tests::ProcessServer;
use super::*;
#[path = "payment_tests/payment_agent_tests.rs"]
mod payment_agent_tests;
#[path = "payment_tests/payment_business_preflight_tests.rs"]
mod payment_business_preflight_tests;
#[path = "payment_tests/payment_decision_tests.rs"]
mod payment_decision_tests;
#[path = "payment_tests/payment_execution_tests.rs"]
mod payment_execution_tests;
#[path = "payment_tests/payment_link_tests.rs"]
mod payment_link_tests;
#[path = "payment_tests/card_payment_composition_tests.rs"]
mod card_payment_composition_tests;
#[path = "payment_tests/background_acceptance_tests.rs"]
mod background_acceptance_tests;

fn root(view: &Value) -> String {
    format!(
        "/api/sim/payment-preparations/{}",
        view["task_id"].as_str().unwrap()
    )
}
fn path(view: &Value) -> String {
    format!(
        "{}/{}",
        root(view),
        view["operation"]["operation_id"].as_str().unwrap()
    )
}
fn prepare_input(view: &Value) -> Value {
    json!({"contract_version":1,"scope":"PAYMENT_INTENT_ONLY","source":{
        "selection_ticket":view["selection_ticket"],"task_revision":view["task_revision"],
        "plan_version":view["plan_version"],"facts_version":view["facts_version"],
        "draft_id":view["receipt"]["draft_id"],"draft_version":view["receipt"]["draft_version"]}})
}
fn decision(view: &Value, action: &str) -> Value {
    json!({"contract_version":1,"scope":"PAYMENT_INTENT_ONLY","confirmation_ref":view["confirmation_ref"],
        "display_ticket":view["display_ticket"],"action":action})
}
async fn prepared(server: &Server, cookie: &str, draft: &Value) -> Value {
    ok(command(server, cookie, &root(draft), prepare_input(draft)).await)
}
async fn decided(server: &Server, cookie: &str, payment: &Value, action: &str) -> Value {
    ok(command(
        server,
        cookie,
        &format!("{}/confirm", path(payment)),
        decision(payment, action),
    )
    .await)
}
async fn draft(server: &Server, cookie: &str) -> Value {
    let initial = create(server, cookie).await;
    run(server, cookie, &initial).await.1
}
async fn provision_payer(db: &Db) {
    let mut tx = db.scoped.begin().await.unwrap();
    safeflow_transfer_integration::funds_store::FundsStore::provision_account(
        &mut tx,
        &UserId::parse("user_001").unwrap(),
        &safeflow_contracts::ResourceId::parse("account_001").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("1288.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}
async fn payment_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM payment_preparations")
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}
async fn funds_state(db: &Db) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('accounts',(SELECT coalesce(jsonb_agg(to_jsonb(a)),'[]') FROM transfer_accounts_v1 a),'funds',(SELECT coalesce(jsonb_agg(to_jsonb(f)),'[]') FROM transfer_fund_orders_v1 f),'ledger',(SELECT coalesce(jsonb_agg(to_jsonb(l)),'[]') FROM transfer_ledger_v1 l),'business',(SELECT coalesce(jsonb_agg(to_jsonb(b)),'[]') FROM transfer_business_orders_v1 b),'permits',(SELECT coalesce(jsonb_agg(to_jsonb(p)),'[]') FROM execution_permits p))")
        .fetch_one(&db.scoped).await.unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_http_full_content_distinct_confirmation_and_concurrent_replay() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let mut funds = db.scoped.begin().await.unwrap();
    for (owner, account, amount) in [
        ("user_001", "account_001", "1288.00"),
        ("user_002", "payment_recipient", "100.00"),
    ] {
        safeflow_transfer_integration::funds_store::FundsStore::provision_account(
            &mut funds,
            &UserId::parse(owner).unwrap(),
            &safeflow_contracts::ResourceId::parse(account).unwrap(),
            &safeflow_bank_gateway::DecimalAmount::parse(amount).unwrap(),
        )
        .await
        .unwrap();
    }
    funds.commit().await.unwrap();
    let before = funds_state(&db).await;
    let input = prepare_input(&created);
    let endpoint = root(&created);
    let (a, b) = tokio::join!(
        command(&server, &cookie, &endpoint, input.clone()),
        command(&server, &cookie, &endpoint, input.clone())
    );
    let a = ok(a);
    let b = ok(b);
    let old_confirmation: String = sqlx::query_scalar(
        "SELECT confirmation_ref FROM server_confirmations WHERE operation_id=$1",
    )
    .bind(created["operation_id"].as_str().unwrap())
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(a["operation"], b["operation"]);
    assert_eq!(a["confirmation_ref"], b["confirmation_ref"]);
    assert_ne!(a["confirmation_ref"], old_confirmation);
    assert_ne!(a["operation"]["operation_id"], created["operation_id"]);
    assert_eq!(a["operation"]["state"], "DRAFT");
    let payment = &a["snapshot"]["transfer"];
    assert_eq!(
        payment["operation_id"],
        created["receipt"]["business_operation_id"]
    );
    assert_eq!(payment["payer_account_id"], "account_001");
    assert_eq!(payment["payee_id"], "payee_008");
    assert_eq!(payment["amount"], "200.00");
    assert_eq!(payment["currency"], "CNY");
    assert_eq!(payment["remark"], "");
    assert!(a["snapshot"]["display_details"].is_object());
    let mut old = decision(&a, "CONFIRM");
    old["confirmation_ref"] = json!(old_confirmation);
    assert_eq!(
        command(&server, &cookie, &format!("{}/confirm", path(&a)), old)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let endpoint = format!("{}/confirm", path(&a));
    let (x, y) = tokio::join!(
        command(&server, &cookie, &endpoint, decision(&a, "CONFIRM")),
        command(&server, &cookie, &endpoint, decision(&a, "CONFIRM"))
    );
    let confirmed = ok(x);
    let retry = ok(y);
    assert_eq!(confirmed["confirmed_at"], retry["confirmed_at"]);
    assert_eq!(
        confirmed["state"],
        "CONFIRMED_AWAITING_EXECUTION_CONDITIONS"
    );
    assert_eq!(confirmed["funds_execution_enabled"], false);
    let mut legacy = execution(&created);
    legacy["confirmation_ref"] = confirmed["confirmation_ref"].clone();
    legacy["operation_revision"] = json!(1);
    let legacy_path = format!(
        "{}/operations/{}/execute",
        task_path(&created),
        a["operation"]["operation_id"].as_str().unwrap()
    );
    assert_ne!(
        command(&server, &cookie, &legacy_path, legacy).await.status,
        StatusCode::OK
    );
    let query = ok(query(&server, &cookie, &path(&a)).await);
    assert_eq!(query, confirmed);
    assert_eq!(payment_count(&db).await, 1);
    let confirms:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE operation_id=$1 AND kind='EXPLICIT_CONFIRMATION_RECORDED'")
        .bind(a["operation"]["operation_id"].as_str().unwrap()).fetch_one(&db.scoped).await.unwrap();
    assert_eq!(confirms, 1);
    assert_eq!(funds_state(&db).await, before);
    assert_eq!(server.calls(), 1, "only the original draft Tool ran");
    let unchanged = ok(super::query(&server, &cookie, &task_path(&created)).await);
    assert_eq!(unchanged["task_revision"], created["task_revision"]);
    assert_eq!(unchanged["operation_id"], created["operation_id"]);
    assert_eq!(
        super::query(
            &server,
            &cookie,
            &format!("/api/tasks/{}", created["task_id"].as_str().unwrap())
        )
        .await
        .status,
        StatusCode::OK
    );
    let plan: Value = sqlx::query_scalar("SELECT preparation_plan FROM payment_preparations")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(plan["plan_version"], created["plan_version"]);
    assert_eq!(plan["nodes"][0]["arguments"], a["snapshot"]);
    assert_eq!(plan["nodes"][1]["type"], "USER_DECISION");
    eprintln!("payment HTTP preparation/confirmation/query: {}", confirmed);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_http_rejects_forgery_ownership_stale_versions_and_never_revives_rejection() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    for (field, value) in [
        ("owner", json!("user_002")),
        ("amount", json!("1.00")),
        ("status", json!("CONFIRMED")),
    ] {
        let mut input = prepare_input(&created);
        input["source"][field] = value;
        assert_eq!(
            command(&server, &cookie, &root(&created), input)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    for (field, value) in [
        ("task_revision", json!(1)),
        ("plan_version", json!(2)),
        ("facts_version", json!(99)),
        ("draft_id", json!("not_this_draft")),
        ("draft_version", json!(2)),
        ("selection_ticket", json!("forged_ticket")),
    ] {
        let mut input = prepare_input(&created);
        input["source"][field] = value;
        assert_ne!(
            command(&server, &cookie, &root(&created), input)
                .await
                .status,
            StatusCode::OK
        );
    }
    assert_eq!(payment_count(&db).await, 0);
    let payment = prepared(&server, &cookie, &created).await;
    let mut changed = prepare_input(&created);
    changed["source"]["draft_version"] = json!(99);
    assert_eq!(
        command(&server, &cookie, &root(&created), changed)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let mut wrong = decision(&payment, "CONFIRM");
    wrong["display_ticket"] = json!("wrong");
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            wrong
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let other = Server::with_draft(
        &db,
        "user_002",
        Duration::from_secs(30),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "payment-other",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let stranger = other.login().await;
    assert_eq!(
        query(&other, &stranger, &path(&payment)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        command(&other, &stranger, &root(&created), prepare_input(&created))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        command(
            &other,
            &stranger,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let rejected = decided(&server, &cookie, &payment, "REJECT").await;
    assert_eq!(rejected["state"], "REJECTED");
    assert_eq!(
        decided(&server, &cookie, &payment, "REJECT").await,
        rejected
    );
    let retry = prepared(&server, &cookie, &created).await;
    assert_eq!(retry["state"], "REJECTED");
    assert_eq!(retry["confirmation_ref"], payment["confirmation_ref"]);
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
    assert_eq!(payment_count(&db).await, 1);
    other.stop();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual member modification/cancellation"]
async fn payment_http_real_draft_modify_cancel_invalidates_old_intents_and_preserves_original_id() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let first = prepared(&server, &cookie, &created).await;
    decided(&server, &cookie, &first, "CONFIRM").await;
    let modify=select(&server,&cookie,&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00","remark":"  payment note  "}})).await;
    let invalid = ok(query(&server, &cookie, &path(&first)).await);
    assert_eq!(invalid["state"], "INVALIDATED");
    assert_eq!(invalid["invalid_reason"], "SOURCE_CHANGED");
    let modified = run(&server, &cookie, &modify).await.1;
    let second = prepared(&server, &cookie, &modified).await;
    assert_eq!(second["snapshot"]["transfer"]["amount"], "300.00");
    assert_eq!(second["snapshot"]["transfer"]["remark"], "  payment note  ");
    assert_eq!(
        first["snapshot"]["transfer"]["operation_id"],
        second["snapshot"]["transfer"]["operation_id"]
    );
    decided(&server, &cookie, &second, "CONFIRM").await;
    let cancel = select(
        &server,
        &cookie,
        &modified,
        "CANCEL_MOCK_DRAFT",
        json!({"draft_id":modified["receipt"]["draft_id"],"expected_version":2}),
    )
    .await;
    let cancelled = run(&server, &cookie, &cancel).await.1;
    assert_eq!(cancelled["task_state"], "CANCELLED");
    let invalid = ok(query(&server, &cookie, &path(&second)).await);
    assert_eq!(invalid["state"], "INVALIDATED");
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&second)),
            decision(&second, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        prepared(&server, &cookie, &created).await["state"],
        "INVALIDATED"
    );
    assert_eq!(payment_count(&db).await, 2);
    assert_eq!(count(&db, "execution_permits").await, 0);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL, actual expiry and session revocation"]
async fn payment_http_actual_draft_expiry_and_session_revocation_invalidate_confirmation() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let short = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(30),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                2,
                "payment-expiry",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let cookie = short.login().await;
    let created = draft(&short, &cookie).await;
    let payment = prepared(&short, &cookie, &created).await;
    decided(&short, &cookie, &payment, "CONFIRM").await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(
        ok(query(&short, &cookie, &path(&payment)).await)["state"],
        "INVALIDATED"
    );
    assert_eq!(
        command(
            &short,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    short.stop();
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    decided(&server, &cookie, &payment, "CONFIRM").await;
    let reply = server
        .call(
            Method::DELETE,
            "/api/sim/session",
            Some("{}".into()),
            Some(&cookie),
            None,
            None,
            true,
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        query(&server, &cookie, &path(&payment)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = server.login().await;
    let invalid = ok(query(&server, &fresh, &path(&payment)).await);
    assert_eq!(invalid["invalid_reason"], "SESSION_CHANGED");
    assert_eq!(
        command(
            &server,
            &fresh,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        prepared(&server, &fresh, &created).await["confirmation_ref"],
        payment["confirmation_ref"]
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and transactional fault injection"]
async fn payment_http_sql_failures_roll_back_operation_confirmation_audit_and_outbox() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let before:(i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM operations),(SELECT count(*) FROM server_confirmations),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM public_audit_outbox)")
        .fetch_one(&db.scoped).await.unwrap();
    db.scoped.execute("CREATE FUNCTION fail_payment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'payment_preparation' THEN RAISE EXCEPTION 'payment injected SQL failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_payment BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_payment()").await.unwrap();
    assert_eq!(
        command(&server, &cookie, &root(&created), prepare_input(&created))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(payment_count(&db).await, 0);
    let after:(i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM operations),(SELECT count(*) FROM server_confirmations),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM public_audit_outbox)")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(before, after);
    db.scoped
        .execute("DROP TRIGGER fail_payment ON audit_events")
        .await
        .unwrap();
    let payment = prepared(&server, &cookie, &created).await;
    db.scoped.execute("CREATE FUNCTION fail_payment_confirm() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='EXPLICIT_CONFIRMATION_RECORDED' THEN RAISE EXCEPTION 'payment confirm injected failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_payment_confirm BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_payment_confirm()").await.unwrap();
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let current = ok(query(&server, &cookie, &path(&payment)).await);
    assert_eq!(current["confirmation_status"], "PREPARED");
    assert!(current["confirmed_at"].is_null());
    db.scoped
        .execute("DROP TRIGGER fail_payment_confirm ON audit_events")
        .await
        .unwrap();
    decided(&server, &cookie, &payment, "CONFIRM").await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual new service processes"]
async fn payment_http_new_process_recovers_original_intent_without_reviving_session() {
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
    let confirmed = ok(first
        .call(
            Method::POST,
            &format!("{}/confirm", path(&payment)),
            Some(decision(&payment, "CONFIRM")),
            Some(&cookie),
        )
        .await);
    first.kill();
    let second = ProcessServer::start(&db).await;
    assert_eq!(
        second
            .call(Method::GET, &path(&payment), None, Some(&cookie))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = second.login().await;
    let recovered_task = ok(second
        .call(Method::GET, &root(&created), None, Some(&fresh))
        .await);
    assert_eq!(recovered_task.as_array().unwrap().len(), 1);
    assert_eq!(recovered_task[0]["operation"], payment["operation"]);
    let recovered = ok(second
        .call(Method::GET, &path(&payment), None, Some(&fresh))
        .await);
    for field in [
        "operation",
        "snapshot",
        "snapshot_digest",
        "confirmation_ref",
        "display_ticket",
        "confirmed_at",
    ] {
        assert_eq!(recovered[field], confirmed[field]);
    }
    assert_eq!(recovered["state"], "INVALIDATED");
    assert_eq!(recovered["invalid_reason"], "SESSION_CHANGED");
    assert_eq!(
        recovered["decision_freshness"],
        "HISTORICAL_REEVALUATION_REQUIRED"
    );
    assert_eq!(
        recovered["security_decisions"][0]["stored_decision_id"],
        confirmed["security_decisions"][0]["stored_decision_id"]
    );
    assert_eq!(recovered["security_decisions"][0]["status"], "INVALIDATED");
    let replay = ok(second
        .call(
            Method::POST,
            &root(&created),
            Some(prepare_input(&created)),
            Some(&fresh),
        )
        .await);
    assert_eq!(replay["operation"], payment["operation"]);
    assert_eq!(payment_count(&db).await, 1);
    assert_eq!(count(&db, "execution_permits").await, 0);
    second.kill();
    db.finish().await;
}

fn spawn_post(
    server: &Server,
    cookie: &str,
    path: String,
    input: Value,
) -> tokio::task::JoinHandle<Reply> {
    let address = server.address;
    let cookie = cookie.to_owned();
    tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &path,
            Some(input.to_string()),
            Some(&cookie),
            None,
            None,
            true,
        )
        .await
    })
}
async fn wait_advisory(db: &Db, key: i64) {
    for _ in 0..200 {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=0 AND objid::bigint=$1 AND NOT granted)")
            .bind(key).fetch_one(&db.admin).await.unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("request did not reach the database barrier");
}
async fn wait_task_lock(db: &Db) {
    for _ in 0..200 {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE '%FROM tasks%FOR UPDATE%' AND cardinality(pg_blocking_pids(pid))>0)")
            .fetch_one(&db.admin).await.unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("concurrent request did not wait on the task lock");
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and deterministic transaction barriers"]
async fn payment_http_prepare_confirm_and_real_modification_serialize_in_both_orders() {
    for mode in ["PREPARE_FIRST", "CONFIRM_FIRST", "MODIFY_FIRST"] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let payment = if mode == "CONFIRM_FIRST" {
            Some(prepared(&server, &cookie, &created).await)
        } else {
            None
        };
        let key = 10_000
            + u16::from_le_bytes(*uuid::Uuid::new_v4().as_bytes().first_chunk::<2>().unwrap())
                as i64;
        let mut held = db.scoped.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *held)
            .await
            .unwrap();
        let trigger = if mode == "MODIFY_FIRST" {
            format!("CREATE FUNCTION payment_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='MODIFY_MOCK_DRAFT' THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$; CREATE TRIGGER payment_barrier BEFORE INSERT ON mock_draft_lifecycle_actions FOR EACH ROW EXECUTE FUNCTION payment_barrier()")
        } else {
            let condition = if mode == "PREPARE_FIRST" {
                "NEW.details ? 'payment_preparation'"
            } else {
                "NEW.kind='EXPLICIT_CONFIRMATION_RECORDED'"
            };
            format!("CREATE FUNCTION payment_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF {condition} THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$; CREATE TRIGGER payment_barrier BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION payment_barrier()")
        };
        db.scoped.execute(trigger.as_str()).await.unwrap();
        let modification = selection(
            &created,
            "MODIFY_MOCK_DRAFT",
            json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"301.00"}}),
        );
        let modify_path = format!("{}/actions", task_path(&created));
        let (first_path, first_input, second_path, second_input) = if mode == "MODIFY_FIRST" {
            (
                modify_path,
                modification,
                root(&created),
                prepare_input(&created),
            )
        } else if let Some(payment) = payment.as_ref() {
            (
                format!("{}/confirm", path(payment)),
                decision(payment, "CONFIRM"),
                modify_path,
                modification,
            )
        } else {
            (
                root(&created),
                prepare_input(&created),
                modify_path,
                modification,
            )
        };
        let first = spawn_post(&server, &cookie, first_path, first_input);
        wait_advisory(&db, key).await;
        let second = spawn_post(&server, &cookie, second_path, second_input);
        wait_task_lock(&db).await;
        held.rollback().await.unwrap();
        let first = ok(first.await.unwrap());
        let second = second.await.unwrap();
        let modified = if mode == "MODIFY_FIRST" {
            assert_eq!(second.status, StatusCode::CONFLICT);
            assert_eq!(payment_count(&db).await, 0);
            first
        } else {
            let selected = ok(second);
            let payment = payment.as_ref().unwrap_or(&first);
            let view = ok(query(&server, &cookie, &path(payment)).await);
            assert_eq!(view["state"], "INVALIDATED");
            assert_eq!(view["confirmation_status"], "REVOKED");
            assert_eq!(
                command(
                    &server,
                    &cookie,
                    &format!("{}/confirm", path(payment)),
                    decision(payment, "CONFIRM")
                )
                .await
                .status,
                StatusCode::CONFLICT
            );
            selected
        };
        let actual = run(&server, &cookie, &modified).await.1;
        assert_eq!(actual["receipt"]["draft_version"], 2);
        let amount: String = sqlx::query_scalar(
            "SELECT details->>'amount' FROM transfer_drafts_v1 WHERE draft_id=$1",
        )
        .bind(actual["receipt"]["draft_id"].as_str().unwrap())
        .fetch_one(&db.scoped)
        .await
        .unwrap();
        assert_eq!(amount, "301.00");
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual original-operation protection"]
async fn payment_http_submitted_original_draft_blocks_new_intent_and_invalidates_prepared_one() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    // Isolated persisted submission marker exercises the existing original-ID
    // protection, not a successful bank execution or a card authorization.
    sqlx::query("UPDATE transfer_drafts_v1 SET submitted=true WHERE draft_id=$1")
        .bind(created["receipt"]["draft_id"].as_str().unwrap())
        .execute(&db.scoped)
        .await
        .unwrap();
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
        ok(query(&server, &cookie, &path(&payment)).await)["state"],
        "INVALIDATED"
    );
    let replay = prepared(&server, &cookie, &created).await;
    assert_eq!(replay["operation"], payment["operation"]);
    assert_eq!(payment_count(&db).await, 1);
    let other = draft(&server, &cookie).await;
    sqlx::query("UPDATE transfer_drafts_v1 SET submitted=true WHERE draft_id=$1")
        .bind(other["receipt"]["draft_id"].as_str().unwrap())
        .execute(&db.scoped)
        .await
        .unwrap();
    assert_eq!(
        command(&server, &cookie, &root(&other), prepare_input(&other))
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(payment_count(&db).await, 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and a session change during SQL writes"]
async fn payment_http_session_revocation_while_sql_waits_rolls_back_before_commit() {
    for confirming in [false, true] {
        let db = Db::new().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let payment = if confirming {
            Some(prepared(&server, &cookie, &created).await)
        } else {
            None
        };
        let key = 10_000
            + u16::from_le_bytes(*uuid::Uuid::new_v4().as_bytes().first_chunk::<2>().unwrap())
                as i64;
        let mut held = db.scoped.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *held)
            .await
            .unwrap();
        let condition = if confirming {
            "NEW.kind='EXPLICIT_CONFIRMATION_RECORDED'"
        } else {
            "NEW.details ? 'payment_preparation'"
        };
        db.scoped.execute(format!("CREATE FUNCTION payment_session_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF {condition} THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$; CREATE TRIGGER payment_session_barrier BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION payment_session_barrier()").as_str()).await.unwrap();
        let request = if let Some(payment) = payment.as_ref() {
            spawn_post(
                &server,
                &cookie,
                format!("{}/confirm", path(payment)),
                decision(payment, "CONFIRM"),
            )
        } else {
            spawn_post(&server, &cookie, root(&created), prepare_input(&created))
        };
        wait_advisory(&db, key).await;
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
        held.rollback().await.unwrap();
        assert_eq!(request.await.unwrap().status, StatusCode::UNAUTHORIZED);
        if let Some(payment) = payment {
            let status: String = sqlx::query_scalar(
                "SELECT status FROM server_confirmations WHERE confirmation_ref=$1",
            )
            .bind(payment["confirmation_ref"].as_str().unwrap())
            .fetch_one(&db.scoped)
            .await
            .unwrap();
            assert_eq!(status, "PREPARED");
        } else {
            assert_eq!(payment_count(&db).await, 0);
        }
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events a JOIN payment_preparations p USING(operation_id) WHERE a.kind='EXPLICIT_CONFIRMATION_RECORDED'")
            .fetch_one(&db.scoped).await.unwrap();
        assert_eq!(count, 0);
        server.stop();
        db.finish().await;
    }
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires isolated PostgreSQL, actual HTTP and PowerShell example"]
async fn payment_http_powershell_example_displays_and_confirms_existing_persisted_draft() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let before = funds_state(&db).await;
    let url = format!("http://{}", server.address);
    let id = created["task_id"].as_str().unwrap().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-File"])
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/payment-preparation-http.ps1"),
            )
            .args(["-Url", &url, "-TaskId", &id, "-Decision", "CONFIRM"])
            .env("SAFEFLOW_SIM_LOGIN_SECRET", SECRET)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("CONFIRMED_AWAITING_EXECUTION_CONDITIONS")
    );
    assert_eq!(payment_count(&db).await, 1);
    assert_eq!(funds_state(&db).await, before);
    server.stop();
    db.finish().await;
}
