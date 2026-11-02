//! Actual loopback HTTP, adopted source and independent PostgreSQL logins.
use super::*;
use safeflow_contracts::{OperationId, ResourceId, SessionRef};
use safeflow_storage::{
    PaymentBackgroundActor, PaymentBackgroundDeployment, PaymentBackgroundRole,
    PaymentSimulationDecision, PgUnifiedFictionalSource, UnifiedFictionalDocument,
};
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;
#[path="background_acceptance_tests/final_completion_tests.rs"]
mod final_completion_tests;

struct Setup {
    db: Db,
    server: Server,
    source: PgUnifiedFictionalSource,
    cookie: String,
    p: Value,
    control: Zeroizing<String>,
    api_pool: sqlx::PgPool,
}
async fn restricted_source(
    db: &Db,
    source: &PgUnifiedFictionalSource,
) -> (PgUnifiedFictionalSource, sqlx::PgPool) {
    use sqlx::ConnectOptions;
    let role = format!("sf_api_{}", uuid::Uuid::new_v4().simple());
    let password = Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ));
    PaymentBackgroundDeployment::for_isolated_backend(source.clone())
        .register_new_api_login(&role, password.clone())
        .await
        .unwrap();
    let options = PgConnectOptions::from_str(&db.url)
        .unwrap()
        .username(&role)
        .password(&password);
    let mut url = options.to_url_lossy();
    url.set_password(Some(&password)).unwrap();
    let repo = PgRepository::connect_existing(url.as_str(), &db.namespace)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let source = PgUnifiedFictionalSource::open(
        repo,
        safeflow_storage::UNIFIED_FICTIONAL_SOURCE,
        safeflow_storage::UNIFIED_FICTIONAL_REVISION,
    )
    .await
    .unwrap();
    (source, pool)
}
async fn setup() -> Setup {
    let db = Db::new().await;
    let raw = std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").unwrap()).unwrap();
    let source = PgUnifiedFictionalSource::initialize_new_isolated(
        db.repo.clone(),
        UnifiedFictionalDocument::read_adopted_v1(&raw).unwrap(),
    )
    .await
    .unwrap();
    let (api_source, api_pool) = restricted_source(&db, &source).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.request_timeout = Duration::from_secs(60);
    let control = Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ));
    let state = ApiState::for_unified_payment_background(
        api_source,
        config,
        UserId::parse("user_001").unwrap(),
        control.clone(),
    )
    .await
    .unwrap();
    let app = router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let server = Server {
        state,
        address,
        handle,
    };
    let cookie = server.login().await;
    let s = session(&cookie);
    server
        .state
        .initialize_unified_catalog_for_backend(&s, &ResourceId::parse("account_001").unwrap())
        .await
        .unwrap();
    let d = draft(&server, &cookie).await;
    let p = prepared(&server, &cookie, &d).await;
    let p = decided(&server, &cookie, &p, "CONFIRM").await;
    Setup {
        db,
        server,
        source,
        cookie,
        p,
        control,
        api_pool,
    }
}
fn session(cookie: &str) -> SessionRef {
    crate::session::token_reference(cookie.split(';').next().unwrap().split_once('=').unwrap().1)
        .unwrap()
}
fn task(p: &Value) -> TaskId {
    serde_json::from_value(p["task_id"].clone()).unwrap()
}
fn prep(p: &Value) -> OperationId {
    serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap()
}
async fn command_bg(v: &Setup, action: Value) -> Reply {
    command(
        &v.server,
        &v.cookie,
        &format!("{}/background", path(&v.p)),
        json!({"contract_version":1,"scope":"EXPLICIT_PAYMENT_BACKGROUND_V1","action":action}),
    )
    .await
}
async fn authorize(v: &Setup) -> Value {
    let display = ok(command_bg(v, json!({"kind":"DISPLAY"})).await);
    ok(command_bg(v, json!({"kind":"CONFIRM","shown":display})).await);
    let code = v
        .server
        .state
        .payment_background_challenge_for_native_controller(
            &v.control,
            &session(&v.cookie),
            &task(&v.p),
            &prep(&v.p),
            display["consent_id"].as_str().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    let r=ok(command_bg(v,json!({"kind":"VERIFY_CODE","consent_id":display["consent_id"],"code":code.expose_for_native_display()})).await);
    assert_eq!(r["verified"], true);
    display
}
fn execution(v: &Setup, display: &Value) -> Value {
    json!({"kind":"EXECUTE","consent_id":display["consent_id"],"confirmation_ref":v.p["confirmation_ref"],"display_ticket":v.p["display_ticket"]})
}
async fn grant(v: &Setup) -> String {
    sqlx::query_scalar("SELECT grant_id FROM payment_completion_candidate_grants WHERE preparation_operation_id=$1").bind(prep(&v.p).as_str()).fetch_one(&v.db.scoped).await.unwrap()
}
async fn snapshot(v: &Setup) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('accounts',(SELECT jsonb_agg(to_jsonb(t) ORDER BY account_id) FROM transfer_accounts_v1 t),'orders',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM transfer_fund_orders_v1 t),'ledger',(SELECT jsonb_agg(to_jsonb(t)) FROM transfer_ledger_v1 t),'grants',(SELECT jsonb_agg(to_jsonb(t) ORDER BY grant_id) FROM payment_completion_candidate_grants t),'business',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM transfer_business_orders_v1 t),'public',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM public_execution_requests t))").fetch_one(&v.db.scoped).await.unwrap()
}
async fn actor(
    v: &Setup,
    kind: PaymentBackgroundRole,
) -> (PaymentBackgroundActor, sqlx::PgPool, String) {
    let role = format!("sf_bg_{}", uuid::Uuid::new_v4().simple());
    let password = Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ));
    PaymentBackgroundDeployment::for_isolated_backend(v.source.clone())
        .register_new_login(&role, kind, password.clone())
        .await
        .unwrap();
    let options = PgConnectOptions::from_str(&v.db.url)
        .unwrap()
        .username(&role)
        .password(&password);
    let wrong = options.clone().password("incorrect-random-secret");
    assert!(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(wrong)
            .await
            .is_err(),
        "actual host auth must reject wrong credentials"
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let actor = PaymentBackgroundActor::authenticate(options, &v.db.namespace, kind)
        .await
        .unwrap();
    (actor, pool, role)
}
async fn denied(pool: &sqlx::PgPool, namespace: &str, sql: &str) {
    let q = sql.replace("@SCHEMA@", &format!("\"{namespace}\""));
    let e = sqlx::query(&q).execute(pool).await.unwrap_err();
    assert_eq!(
        e.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501"),
        "permission check must be the rejection"
    );
}
#[tokio::test]
#[ignore = "requires owned isolated PG with scram host authentication"]
async fn background_acceptance_http_atomic_replay_and_real_principals() {
    let v = setup().await;
    for sql in [
        "DELETE FROM @SCHEMA@.payment_completion_candidate_grants",
        "DELETE FROM @SCHEMA@.payment_completion_candidate_decisions",
        "UPDATE @SCHEMA@.payment_completion_candidate_controls SET revoked=true",
        "UPDATE @SCHEMA@.payment_completion_candidate_jobs SET lease_epoch=lease_epoch+1",
        "UPDATE @SCHEMA@.payment_background_api_logins_v1 SET active=true",
        "UPDATE @SCHEMA@.transfer_ledger_v1 SET amount_cents=0",
        "UPDATE @SCHEMA@.billing_feedback_v1 SET payload=payload",
        "DELETE FROM @SCHEMA@.tasks",
    ] {
        denied(&v.api_pool, &v.db.namespace, sql).await;
    }
    let paths:Vec<(String,Vec<String>)>=sqlx::query_as("SELECT proname,proconfig FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname=$1 AND proname LIKE 'payment_background_%'").bind(&v.db.namespace).fetch_all(&v.db.scoped).await.unwrap();
    assert!(paths.len() >= 8);
    for (name, path) in paths {
        assert_eq!(
            path,
            vec![format!(
                "search_path=pg_catalog, {}, pg_temp",
                v.db.namespace
            )],
            "{name}"
        );
    }
    let create: bool = sqlx::query_scalar("SELECT has_schema_privilege(current_user,$1,'CREATE')")
        .bind(&v.db.namespace)
        .fetch_one(&v.api_pool)
        .await
        .unwrap();
    assert!(!create);
    let before = snapshot(&v).await;
    let old=command(&v.server,&v.cookie,&format!("{}/execute",path(&v.p)),json!({"contract_version":1,"scope":"PAYMENT_EXECUTION_ONLY","confirmation_ref":v.p["confirmation_ref"],"display_ticket":v.p["display_ticket"],"background":true})).await;
    assert_eq!(old.status, StatusCode::BAD_REQUEST);
    assert_eq!(before, snapshot(&v).await);
    let display = ok(command_bg(&v, json!({"kind":"DISPLAY"})).await);
    assert_eq!(display["display"]["default_delegated"], false);
    assert_ne!(
        command_bg(&v, execution(&v, &display)).await.status,
        StatusCode::OK
    );
    assert_eq!(before, snapshot(&v).await);
    ok(command_bg(&v, json!({"kind":"CONFIRM","shown":display})).await);
    assert!(v
        .server
        .state
        .payment_background_challenge_for_native_controller(
            "wrong",
            &session(&v.cookie),
            &task(&v.p),
            &prep(&v.p),
            display["consent_id"].as_str().unwrap()
        )
        .await
        .is_err());
    let code = v
        .server
        .state
        .payment_background_challenge_for_native_controller(
            &v.control,
            &session(&v.cookie),
            &task(&v.p),
            &prep(&v.p),
            display["consent_id"].as_str().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    let wrong = if code.expose_for_native_display() == "000000" {
        "000001"
    } else {
        "000000"
    };
    assert_eq!(
        ok(command_bg(
            &v,
            json!({"kind":"VERIFY_CODE","consent_id":display["consent_id"],"code":wrong})
        )
        .await)["verified"],
        false
    );
    assert_eq!(ok(command_bg(&v,json!({"kind":"VERIFY_CODE","consent_id":display["consent_id"],"code":code.expose_for_native_display()})).await)["verified"],true);
    let (a, b) = tokio::join!(
        command_bg(&v, execution(&v, &display)),
        command_bg(&v, execution(&v, &display))
    );
    let a = ok(a);
    let b = ok(b);
    assert_eq!(a["operation"], b["operation"]);
    assert_eq!(a["operation"]["state"], "PROCESSING");
    let gid = grant(&v).await;
    let first = snapshot(&v).await;
    let body: Value = sqlx::query_scalar(
        "SELECT evidence FROM payment_completion_candidate_grants WHERE grant_id=$1",
    )
    .bind(&gid)
    .fetch_one(&v.db.scoped)
    .await
    .unwrap();
    for field in [
        "amount_cents",
        "owner_id",
        "source_id",
        "request_payload",
        "catalog_adoption",
    ] {
        let mut missing = body.clone();
        missing.as_object_mut().unwrap().remove(field);
        let error = sqlx::query(&format!(
            "SELECT \"{}\".payment_background_accept_v1($1::uuid,$2)",
            v.db.namespace
        ))
        .bind(display["consent_id"].as_str().unwrap())
        .bind(sqlx::types::Json(missing))
        .execute(&v.api_pool)
        .await
        .unwrap_err();
        let message = error.as_database_error().unwrap().message();
        assert!(
            message.starts_with("BACKGROUND_"),
            "missing {field} must reject before unique conflict: {message}"
        );
    }
    assert_eq!(first, snapshot(&v).await);
    let counts:(i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM payment_completion_candidate_grants),(SELECT count(*) FROM payment_completion_candidate_jobs),(SELECT count(*) FROM transfer_fund_orders_v1),(SELECT count(*) FROM transfer_ledger_v1)").fetch_one(&v.db.scoped).await.unwrap();
    assert_eq!(counts, (1, 1, 1, 0));
    let duration:bool=sqlx::query_scalar("SELECT completion_until=first_submitted_at+interval '30 minutes' FROM payment_completion_candidate_grants").fetch_one(&v.db.scoped).await.unwrap();
    assert!(duration);
    ok(command_bg(&v, execution(&v, &display)).await);
    assert_eq!(first, snapshot(&v).await);
    let (producer, pp, pr) = actor(&v, PaymentBackgroundRole::Producer).await;
    let (worker, wp, _) = actor(&v, PaymentBackgroundRole::Worker).await;
    for sql in [
        "UPDATE @SCHEMA@.transfer_accounts_v1 SET booked_cents=0",
        "UPDATE @SCHEMA@.transfer_fund_orders_v1 SET state='FAILED'",
        "INSERT INTO @SCHEMA@.payment_background_consents_v1(consent_id) VALUES(NULL)",
        "UPDATE @SCHEMA@.payment_completion_candidate_controls SET revoked=true",
        "UPDATE @SCHEMA@.operations SET state='SUCCEEDED'",
        "DELETE FROM @SCHEMA@.transfer_ledger_v1",
    ] {
        denied(&pp, &v.db.namespace, sql).await;
    }
    denied(
        &wp,
        &v.db.namespace,
        "DELETE FROM @SCHEMA@.payment_completion_candidate_decisions",
    )
    .await;
    denied(
        &wp,
        &v.db.namespace,
        "SELECT @SCHEMA@.payment_background_decide_v1('x','x','SETTLE_LOCAL_SIMULATION')",
    )
    .await;
    denied(
        &pp,
        &v.db.namespace,
        "SELECT @SCHEMA@.payment_background_worker_v1('x')",
    )
    .await;
    assert_eq!(
        worker.inspect_and_claim_original(&gid).await.unwrap()["state"],
        "WAITING_TRUSTED_DECISION"
    );
    assert!(producer
        .record_explicit_decision(
            "missing",
            "result-1",
            PaymentSimulationDecision::SettleLocalSimulation
        )
        .await
        .is_err());
    let d = producer
        .record_explicit_decision(
            &gid,
            "result-1",
            PaymentSimulationDecision::SettleLocalSimulation,
        )
        .await
        .unwrap();
    assert_eq!(
        d,
        producer
            .record_explicit_decision(
                &gid,
                "result-1",
                PaymentSimulationDecision::SettleLocalSimulation
            )
            .await
            .unwrap()
    );
    assert!(producer
        .record_explicit_decision(
            &gid,
            "result-1",
            PaymentSimulationDecision::DefiniteSimulatedDecline
        )
        .await
        .is_err());
    assert!(producer
        .record_explicit_decision(
            &gid,
            "result-2",
            PaymentSimulationDecision::SettleLocalSimulation
        )
        .await
        .is_err());
    let r = worker.inspect_and_claim_original(&gid).await.unwrap();
    assert_eq!(r["state"], "READY_FOR_STEP08");
    assert_eq!(r["business_executed"], false);
    assert_eq!(
        worker.inspect_and_claim_original(&gid).await.unwrap()["state"],
        "LEASE_BUSY"
    );
    assert_eq!(first, snapshot(&v).await);
    PaymentBackgroundDeployment::for_isolated_backend(v.source.clone())
        .revoke_login(&pr)
        .await
        .unwrap();
    assert!(producer
        .record_explicit_decision(
            &gid,
            "result-1",
            PaymentSimulationDecision::SettleLocalSimulation
        )
        .await
        .is_err());
    assert!(worker.inspect_and_claim_original(&gid).await.is_err());
    producer.close().await;
    worker.close().await;
    pp.close().await;
    wp.close().await;
    v.server.stop();
    v.db.finish().await;
}

#[tokio::test]
#[ignore = "requires owned isolated PG with scram host authentication"]
async fn background_acceptance_rollback_logout_revocation_and_legacy_closed() {
    let v = setup().await;
    let display = authorize(&v).await;
    let before = payment_execution_tests::persistent_state(&v.db).await;
    sqlx::raw_sql("CREATE FUNCTION fail_bg_test() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'INJECT_GRANT_JOB_FAILURE'; END $$; CREATE TRIGGER fail_bg BEFORE INSERT ON payment_completion_candidate_jobs FOR EACH ROW EXECUTE FUNCTION fail_bg_test()").execute(&v.db.scoped).await.unwrap();
    assert_ne!(
        command_bg(&v, execution(&v, &display)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        before,
        payment_execution_tests::persistent_state(&v.db).await
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM payment_completion_candidate_grants")
        .fetch_one(&v.db.scoped)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::raw_sql(
        "DROP TRIGGER fail_bg ON payment_completion_candidate_jobs; DROP FUNCTION fail_bg_test()",
    )
    .execute(&v.db.scoped)
    .await
    .unwrap();
    ok(command_bg(&v, execution(&v, &display)).await);
    let gid = grant(&v).await;
    let original = snapshot(&v).await;
    // Expire old confirmation/auth by trusted administrator boundary fixture only.
    // Accepted grant must not depend on their current validity or old session.
    v.server
        .state
        .0
        .sessions
        .revoke(&session(&v.cookie))
        .unwrap();
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT first_submitted_at+interval '6 minutes' FROM payment_completion_candidate_grants $$").execute(&v.db.scoped).await.unwrap();
    let expired:bool=sqlx::query_scalar("SELECT payment_background_clock_v1()>confirmed_until AND payment_background_clock_v1()>auth_until FROM payment_background_consents_v1").fetch_one(&v.db.scoped).await.unwrap();
    assert!(expired);
    let (producer, pp, _) = actor(&v, PaymentBackgroundRole::Producer).await;
    producer
        .record_explicit_decision(
            &gid,
            "after-logout",
            PaymentSimulationDecision::DefiniteSimulatedDecline,
        )
        .await
        .unwrap();
    let new_cookie = v.server.login().await;
    let result = ok(query(
        &v.server,
        &new_cookie,
        v.p["original_result_path"].as_str().unwrap(),
    )
    .await);
    assert_eq!(result["state"], "PROCESSING");
    assert_eq!(original, snapshot(&v).await);
    let op: OperationId =
        serde_json::from_value(result["binding"]["execution_operation_id"].clone()).unwrap();
    v.server
        .state
        .0
        .repository
        .revoke_payment_background(
            v.server.state.0.sessions.clone(),
            &session(&new_cookie),
            &op,
        )
        .await
        .unwrap();
    assert!(producer
        .record_explicit_decision(
            &gid,
            "after-logout",
            PaymentSimulationDecision::DefiniteSimulatedDecline
        )
        .await
        .is_err());
    assert_eq!(original, snapshot(&v).await);
    // A missing original/grant cannot be activated by producer or GET.
    assert!(producer
        .record_explicit_decision(
            "legacy_without_grant",
            "legacy",
            PaymentSimulationDecision::SettleLocalSimulation
        )
        .await
        .is_err());
    producer.close().await;
    pp.close().await;
    v.server.stop();
    v.db.finish().await;
}

#[tokio::test]
#[ignore = "requires owned isolated PG; trusted DB clock boundary and withdrawal"]
async fn background_acceptance_db_deadline_and_source_boundaries() {
    for mode in [
        "before",
        "equal",
        "after",
        "source",
        "catalog",
        "account",
        "epoch",
        "scope-owner",
        "scope-account",
        "unknown",
    ] {
        let v = setup().await;
        let display = authorize(&v).await;
        ok(command_bg(&v, execution(&v, &display)).await);
        let gid = grant(&v).await;
        let (producer, pp, role) = actor(&v, PaymentBackgroundRole::Producer).await;
        let before = snapshot(&v).await;
        if ["before", "equal", "after"].contains(&mode) {
            let offset = match mode {
                "before" => "-interval '1 microsecond'",
                "after" => "+interval '1 microsecond'",
                _ => "",
            };
            // Only this dedicated test schema's trusted clock changes. No grant
            // mutation, long sleep, wall clock, or input JSON modification.
            let q=format!("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT completion_until {offset} FROM payment_completion_candidate_grants $$");
            sqlx::raw_sql(&q).execute(&v.db.scoped).await.unwrap();
        } else if mode == "source" {
            v.source.deactivate_for_isolated_backend().await.unwrap();
        } else if mode == "catalog" {
            sqlx::query("INSERT INTO public_card_catalog_adoptions_v1(account_id,owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest) SELECT account_id,owner_id,source_id,implementation_revision,ordinal+1,source_revision||'-new-revision',catalog_digest FROM public_card_catalog_adoptions_v1 ORDER BY ordinal DESC LIMIT 1").execute(&v.db.scoped).await.unwrap();
        } else if mode == "account" {
            sqlx::query("UPDATE card_safety_account_fences_v1 SET revoked=true,epoch=epoch+1 WHERE account_id='account_001'").execute(&v.db.scoped).await.unwrap();
        } else if mode == "epoch" {
            PaymentBackgroundDeployment::for_isolated_backend(v.source.clone())
                .revoke_login(&role)
                .await
                .unwrap();
        } else if mode.starts_with("scope-") {
            let scope = if mode == "scope-owner" {
                json!({"accounts":[{"owner_id":"user_002","account_id":"account_001"}]})
            } else {
                json!({"accounts":[{"owner_id":"user_001","account_id":"account_002"}]})
            };
            sqlx::query("UPDATE payment_completion_candidate_fences SET epoch=epoch+1,scope=$2 WHERE authority_id=$1").bind(&role).bind(sqlx::types::Json(scope)).execute(&v.db.scoped).await.unwrap();
            sqlx::query(
                "UPDATE payment_background_principals_v1 SET epoch=epoch+1 WHERE role_name=$1",
            )
            .bind(&role)
            .execute(&v.db.scoped)
            .await
            .unwrap();
        }
        let result = if mode == "unknown" {
            sqlx::query(&format!("SELECT \"{}\".payment_background_decide_v1($1,'unknown','EXTERNAL_BANK_ALREADY_SUCCEEDED')",v.db.namespace)).bind(&gid).fetch_one(&pp).await.map(|_|json!({}))
        } else {
            producer
                .record_explicit_decision(
                    &gid,
                    "boundary",
                    PaymentSimulationDecision::SettleLocalSimulation,
                )
                .await
                .map_err(|_| sqlx::Error::RowNotFound)
        };
        if mode == "before" {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err(), "{mode}");
        }
        assert_eq!(
            before,
            snapshot(&v).await,
            "all modes preserve hold and complete original: {mode}"
        );
        if mode == "source" {
            assert!(
                sqlx::query("UPDATE public_unified_fictional_source_v1 SET active=true")
                    .execute(&v.db.scoped)
                    .await
                    .is_err(),
                "source ABA is impossible"
            );
        }
        producer.close().await;
        pp.close().await;
        v.server.stop();
        v.db.finish().await;
    }
}

async fn blocked(db: &Db, holder: i32) {
    let until = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let yes:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))").bind(holder).fetch_one(&db.admin).await.unwrap();
        if yes {
            println!("observed actual pg_blocking_pids for background revocation root");
            return;
        }
        assert!(std::time::Instant::now() < until, "expected real SQL wait");
        tokio::task::yield_now().await;
    }
}
#[tokio::test]
#[ignore = "requires owned isolated PG; real revocation/decision lock ordering"]
async fn background_acceptance_real_wait_revocation_serialization() {
    for mode in ["control", "account", "source"] {
        let v = setup().await;
        let display = authorize(&v).await;
        ok(command_bg(&v, execution(&v, &display)).await);
        let gid = grant(&v).await;
        let (producer, pp, _) = actor(&v, PaymentBackgroundRole::Producer).await;
        let before = snapshot(&v).await;
        let mut tx = v.db.scoped.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let q=match mode{"control"=>"UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1","account"=>"UPDATE card_safety_account_fences_v1 SET revoked=true,epoch=epoch+1 WHERE account_id='account_001'",_=>"UPDATE public_unified_fictional_source_v1 SET active=false"};
        sqlx::query(q).execute(&mut *tx).await.unwrap();
        let future = producer.record_explicit_decision(
            &gid,
            "wait-revoke",
            PaymentSimulationDecision::SettleLocalSimulation,
        );
        let (_, r) = tokio::join!(
            async {
                blocked(&v.db, pid).await;
                tx.commit().await.unwrap();
            },
            future
        );
        assert!(r.is_err());
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM payment_completion_candidate_decisions")
                .fetch_one(&v.db.scoped)
                .await
                .unwrap();
        assert_eq!(count, 0);
        assert_eq!(before, snapshot(&v).await);
        producer.close().await;
        pp.close().await;
        v.server.stop();
        v.db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires owned isolated PG; actual legacy and closed binding checks"]
async fn background_acceptance_legacy_and_cross_original_binding() {
    let mut v = setup().await;
    let shown = ok(command_bg(&v, json!({"kind":"DISPLAY"})).await);
    for field in ["owner_id", "snapshot", "catalog_adoption"] {
        let mut wrong = shown.clone();
        wrong["display"][field] = json!({"untrusted":"different"});
        use sha2::Digest;
        wrong["display_digest"] = json!(format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&wrong["display"]).unwrap())
        ));
        assert_ne!(
            command_bg(&v, json!({"kind":"CONFIRM","shown":wrong}))
                .await
                .status,
            StatusCode::OK
        );
    }
    // Invoke the real pre-delegation payment composer with its old Yellow profile.
    // This creates an actual accepted PROCESSING original, not a nonexistent ID.
    let config = v.server.state.0.payment_execution.as_ref().unwrap();
    let mut profile = config.profile.clone();
    profile.minimum_level = safeflow_contracts::SecurityLevel::Yellow;
    let runtime = safeflow_runtime::RuntimeBootstrap::build(
        safeflow_runtime::ToolRegistry::new(),
        safeflow_runtime::DomainPolicyRegistry::new(),
        safeflow_runtime::ToolSecurityCatalog::new(),
        v.server.state.0.sessions.clone(),
        Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
    )
    .unwrap()
    .with_final_payment_policy_for_backend(
        safeflow_storage::payment_final_registration_for_backend(profile.clone()).unwrap(),
    )
    .unwrap();
    let service = safeflow_storage::PaymentExecutionService::for_project_simulation(
        v.server.state.0.repository.clone(),
        v.server.state.0.sessions.clone(),
        runtime.runtime(),
        config.source.clone(),
        profile,
        config.permit.clone(),
        safeflow_transfer_integration::mock_execution::MockBehavior::Pending,
    )
    .unwrap();
    let old = service
        .execute(
            &session(&v.cookie),
            &task(&v.p),
            &prep(&v.p),
            &serde_json::from_value(v.p["confirmation_ref"].clone()).unwrap(),
            v.p["display_ticket"].as_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        old.operation.state,
        safeflow_contracts::OperationState::Processing
    );
    let before = snapshot(&v).await;
    assert_ne!(
        command_bg(&v, json!({"kind":"DISPLAY"})).await.status,
        StatusCode::OK
    );
    assert_eq!(
        ok(query(
            &v.server,
            &v.cookie,
            v.p["original_result_path"].as_str().unwrap()
        )
        .await)["state"],
        "PROCESSING"
    );
    let (producer, pp, _) = actor(&v, PaymentBackgroundRole::Producer).await;
    assert!(producer
        .record_explicit_decision(
            &old.operation.operation_id.to_string(),
            "legacy",
            PaymentSimulationDecision::SettleLocalSimulation
        )
        .await
        .is_err());
    assert_eq!(before, snapshot(&v).await);
    let zero: i64 = sqlx::query_scalar("SELECT count(*) FROM payment_completion_candidate_grants")
        .fetch_one(&v.db.scoped)
        .await
        .unwrap();
    assert_eq!(zero, 0);
    let mut grants = vec![];
    for _ in 0..2 {
        let d = draft(&v.server, &v.cookie).await;
        let p = prepared(&v.server, &v.cookie, &d).await;
        v.p = decided(&v.server, &v.cookie, &p, "CONFIRM").await;
        let display = authorize(&v).await;
        ok(command_bg(&v, execution(&v, &display)).await);
        grants.push(grant(&v).await);
    }
    producer
        .record_explicit_decision(
            &grants[0],
            "one-original-result",
            PaymentSimulationDecision::SettleLocalSimulation,
        )
        .await
        .unwrap();
    let first = snapshot(&v).await;
    assert!(producer
        .record_explicit_decision(
            &grants[1],
            "one-original-result",
            PaymentSimulationDecision::SettleLocalSimulation
        )
        .await
        .is_err());
    assert_eq!(first, snapshot(&v).await);
    producer.close().await;
    pp.close().await;
    v.server.stop();
    v.db.finish().await;
}

#[tokio::test]
#[ignore = "requires owned isolated PG; reverse writer waits and post-wait deadline"]
async fn background_acceptance_reverse_wait_and_deadline_after_wait() {
    for mode in [
        "control",
        "account",
        "source",
        "deadline",
        "deadline-replay",
        "principal-expiry",
        "worker-expiry",
        "worker-producer-expiry",
    ] {
        let v = setup().await;
        let display = authorize(&v).await;
        ok(command_bg(&v, execution(&v, &display)).await);
        let gid = grant(&v).await;
        let (producer, pp, role) = actor(&v, PaymentBackgroundRole::Producer).await;
        let (worker, wp, worker_role) = actor(&v, PaymentBackgroundRole::Worker).await;
        if mode.starts_with("deadline") {
            // Install the trusted test clock BEFORE the statement starts. DDL
            // replacement during an already-running statement may retain its
            // cached SQL-function body, so advance data instead of replacing code.
            sqlx::raw_sql("CREATE TABLE background_wait_clock_v1(stamp timestamptz NOT NULL); INSERT INTO background_wait_clock_v1 VALUES(clock_timestamp()); CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT stamp FROM background_wait_clock_v1 $$").execute(&v.db.scoped).await.unwrap();
        }
        if mode == "deadline-replay" {
            producer
                .record_explicit_decision(
                    &gid,
                    "after-wait-deadline",
                    PaymentSimulationDecision::SettleLocalSimulation,
                )
                .await
                .unwrap();
        }
        if mode.starts_with("worker-") {
            producer
                .record_explicit_decision(
                    &gid,
                    "before-worker-wait",
                    PaymentSimulationDecision::SettleLocalSimulation,
                )
                .await
                .unwrap();
        }
        let before = snapshot(&v).await;
        if mode.starts_with("worker-") {
            let mut tx = v.db.scoped.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            sqlx::query("SELECT grant_id FROM payment_completion_candidate_jobs WHERE grant_id=$1 FOR UPDATE").bind(&gid).fetch_one(&mut *tx).await.unwrap();
            let future = worker.inspect_and_claim_original(&gid);
            let (_, r) = tokio::join!(
                async {
                    blocked(&v.db, pid).await;
                    let expired = if mode == "worker-expiry" {
                        &worker_role
                    } else {
                        &role
                    };
                    sqlx::query("UPDATE payment_background_principals_v1 SET valid_until=clock_timestamp()-interval '1 microsecond' WHERE role_name=$1").bind(expired).execute(&v.db.scoped).await.unwrap();
                    tx.commit().await.unwrap();
                },
                future
            );
            assert!(r.is_err(), "post-job-wait must reject {mode}");
        } else if mode.starts_with("deadline") || mode == "principal-expiry" {
            let mut tx = v.db.scoped.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            sqlx::query("SELECT grant_id FROM payment_completion_candidate_grants WHERE grant_id=$1 FOR UPDATE").bind(&gid).fetch_one(&mut *tx).await.unwrap();
            let future = producer.record_explicit_decision(
                &gid,
                "after-wait-deadline",
                PaymentSimulationDecision::SettleLocalSimulation,
            );
            let (_, result) = tokio::join!(
                async {
                    blocked(&v.db, pid).await;
                    if mode == "principal-expiry" {
                        sqlx::query("UPDATE payment_background_principals_v1 SET valid_until=clock_timestamp()-interval '1 microsecond' WHERE role_name=$1").bind(&role).execute(&v.db.scoped).await.unwrap();
                    } else {
                        sqlx::query("UPDATE background_wait_clock_v1 SET stamp=(SELECT completion_until FROM payment_completion_candidate_grants)").execute(&v.db.scoped).await.unwrap();
                    }
                    tx.commit().await.unwrap();
                },
                future
            );
            assert!(
                result.is_err(),
                "post-wait authorization must reject mode={mode}"
            );
        } else {
            let mut tx = pp.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            sqlx::query(&format!("SELECT \"{}\".payment_background_decide_v1($1,'decision-before-revoke','SETTLE_LOCAL_SIMULATION')",v.db.namespace)).bind(&gid).execute(&mut *tx).await.unwrap();
            let q=match mode{"control"=>"UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1","account"=>"UPDATE card_safety_account_fences_v1 SET revoked=true,epoch=epoch+1 WHERE account_id='account_001'",_=>"UPDATE public_unified_fictional_source_v1 SET active=false"};
            let (_, r) = tokio::join!(
                async {
                    blocked(&v.db, pid).await;
                    tx.commit().await.unwrap();
                },
                sqlx::query(q).execute(&v.db.scoped)
            );
            assert_eq!(r.unwrap().rows_affected(), 1);
            assert!(worker.inspect_and_claim_original(&gid).await.is_err());
        }
        assert_eq!(before, snapshot(&v).await);
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM payment_completion_candidate_decisions")
                .fetch_one(&v.db.scoped)
                .await
                .unwrap();
        assert_eq!(
            count,
            if mode == "deadline" || mode == "principal-expiry" {
                0
            } else {
                1
            }
        );
        producer.close().await;
        worker.close().await;
        pp.close().await;
        wp.close().await;
        v.server.stop();
        v.db.finish().await;
    }
}

#[tokio::test]
#[ignore = "only controlled by owned recovery runner; never ordinary full test"]
async fn background_acceptance_recovery_child() {
    use std::io::{BufRead, Write};
    assert_eq!(
        std::env::var("SAFEFLOW_BACKGROUND_RECOVERY").unwrap(),
        "OWNED_CLUSTER_ONLY"
    );
    let resume = std::env::var("SAFEFLOW_BACKGROUND_RESUME").ok();
    let v = if let Some(h) = resume.as_ref() {
        let hints: Value = serde_json::from_str(h).unwrap();
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let namespace = hints["namespace"].as_str().unwrap().to_owned();
        let repo = PgRepository::connect_existing(&url, &namespace)
            .await
            .unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let ns = namespace.clone();
        let scoped = PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |c, _| {
                let ns = ns.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(ns)
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let source = PgUnifiedFictionalSource::open(
            repo.clone(),
            safeflow_storage::UNIFIED_FICTIONAL_SOURCE,
            safeflow_storage::UNIFIED_FICTIONAL_REVISION,
        )
        .await
        .unwrap();
        let db = Db {
            namespace,
            url,
            admin,
            scoped,
            repo,
        };
        let (api_source, api_pool) = restricted_source(&db, &source).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
        config.bind = address;
        config.request_timeout = Duration::from_secs(60);
        let control = Zeroizing::new(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ));
        let state = ApiState::for_unified_payment_background(
            api_source,
            config,
            UserId::parse("user_001").unwrap(),
            control.clone(),
        )
        .await
        .unwrap();
        let app = router(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let server = Server {
            state,
            address,
            handle,
        };
        let cookie = server.login().await;
        Setup {
            db,
            server,
            source,
            cookie,
            p: hints["payment"].clone(),
            control,
            api_pool,
        }
    } else {
        setup().await
    };
    let display = if resume.is_none() {
        authorize(&v).await
    } else {
        json!({"consent_id":"00000000-0000-0000-0000-000000000000"})
    };
    let emit = |value: Value| {
        println!("BACKGROUND_RECOVERY_REPLY:{value}");
        std::io::stdout().flush().unwrap();
    };
    emit(
        json!({"ready":true,"pid":std::process::id(),"namespace":v.db.namespace,"payment":v.p,"address":v.server.address.to_string()}),
    );
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match value["kind"].as_str().unwrap() {
            "EXECUTE" => {
                let r = command_bg(&v, execution(&v, &display)).await;
                emit(json!({"status":r.status.as_u16(),"result":r.json}));
            }
            "QUERY" => {
                let r = query(
                    &v.server,
                    &v.cookie,
                    v.p["original_result_path"].as_str().unwrap(),
                )
                .await;
                emit(
                    json!({"status":r.status.as_u16(),"result":r.json,"snapshot":snapshot(&v).await}),
                );
            }
            "DECIDE_AND_POLL" => {
                let gid = grant(&v).await;
                let (producer, pp, _) = actor(&v, PaymentBackgroundRole::Producer).await;
                let (worker, wp, _) = actor(&v, PaymentBackgroundRole::Worker).await;
                let d = producer
                    .record_explicit_decision(
                        &gid,
                        "recovery-original-result",
                        PaymentSimulationDecision::SettleLocalSimulation,
                    )
                    .await
                    .unwrap();
                let r = worker.inspect_and_claim_original(&gid).await.unwrap();
                emit(
                    json!({"decision_id":d["decision_id"],"state":r["state"],"business_executed":r["business_executed"],"snapshot":snapshot(&v).await}),
                );
                producer.close().await;
                worker.close().await;
                pp.close().await;
                wp.close().await;
            }
            "POLL_PERSISTED" => {
                // Trusted test DB clock moves past the old 30s lease; not a
                // production timer, fake decision, or changed grant deadline.
                sqlx::raw_sql("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT clock_timestamp()+interval '31 seconds' $$").execute(&v.db.scoped).await.unwrap();
                let gid = grant(&v).await;
                let (worker, wp, _) = actor(&v, PaymentBackgroundRole::Worker).await;
                let r = worker.inspect_and_claim_original(&gid).await.unwrap();
                emit(
                    json!({"decision_id":r["decision"]["decision_id"],"state":r["state"],"business_executed":r["business_executed"],"snapshot":snapshot(&v).await}),
                );
                worker.close().await;
                wp.close().await;
            }
            "STOP" => {
                emit(json!({"stopping":true}));
                break;
            }
            _ => panic!("unknown parent command"),
        }
    }
    v.server.stop();
    v.db.repo.close().await;
    v.db.scoped.close().await;
    v.db.admin.close().await;
}
