//! Genuine loopback HTTP + SQL. Reviewed catalog fixture, never shared startup.
use super::*;
use safeflow_card_facts::{
    operation_records::StoredReasonCode, operation_replay::LockReplayParameters, LockSource,
};
use safeflow_card_postgres_store::{
    freeze::{FreezeCommand, FreezeRecordIds},
    LockedCardAccount,
};
use safeflow_contracts::{ContractFuture, ResourceId};
use safeflow_storage::{
    BackendCardCatalog, BackendCardCatalogProvider, BackendCatalogCard, CardCatalogService,
    CardCatalogSourceBinding,
};
use safeflow_transfer_integration::funds_store::FundsStore;
struct Catalog;
impl BackendCardCatalogProvider for Catalog {
    fn authorized_catalog<'a>(
        &'a self,
        i: &'a SessionBinding,
        a: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        Box::pin(async move {
            if i.user_id.as_str() != "user_001" || a.as_str() != "account_001" {
                return Ok(None);
            }
            Ok(Some(BackendCardCatalog {
                owner: i.user_id.clone(),
                account: a.clone(),
                source_revision: "isolated-pr23-v1".into(),
                cards: vec![
                    BackendCatalogCard {
                        card_id: "isolated-a".into(),
                        last_four: "1234".into(),
                    },
                    BackendCatalogCard {
                        card_id: "isolated-b".into(),
                        last_four: "1234".into(),
                    },
                ],
            }))
        })
    }
    fn current_revision<'a>(
        &'a self,
        i: &'a SessionBinding,
        a: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            if i.user_id.as_str() != "user_001" || a.as_str() != "account_001" {
                return Err(StorageError::NotFound);
            }
            Ok("isolated-pr23-v1".into())
        })
    }
}
async fn server(db: &Db, enabled: bool) -> Server {
    configured_server(db, enabled, Duration::from_secs(30)).await
}
async fn configured_server(db: &Db, enabled: bool, timeout: Duration) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut c = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    c.bind = address;
    c.request_timeout = timeout;
    if let Ok(origin) = std::env::var("SAFEFLOW_CARD_BROWSER_ORIGIN") {
        c.frontend_origin = origin;
    }
    c.card_read_source = enabled.then(|| {
        CardCatalogSourceBinding::for_reviewed_backend_provider(
            "isolated-pr23",
            "isolated-impl",
            Arc::new(Catalog),
        )
        .unwrap()
    });
    let source = c.card_read_source.clone();
    let state = ApiState::for_project_simulation(db.repo.clone(), c).unwrap();
    if let Some(source) = source {
        let cookie = state.0.sessions.issue(SECRET, None).unwrap();
        let r = crate::session::token_reference(Cookie::parse(cookie).unwrap().value()).unwrap();
        let mut tx = db.scoped.begin().await.unwrap();
        FundsStore::provision_account(
            &mut tx,
            &UserId::parse("user_001").unwrap(),
            &ResourceId::parse("account_001").unwrap(),
            &safeflow_bank_gateway::DecimalAmount::parse("100.00").unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        CardCatalogService::for_trusted_backend(
            db.repo.clone(),
            state.0.sessions.clone(),
            Some(source),
        )
        .initialize(&r, &ResourceId::parse("account_001").unwrap())
        .await
        .unwrap();
        let c = FreezeCommand {
            operation_id: "OP-readonly-fixture".into(),
            canonical_key: "KEY-readonly-fixture".into(),
            event_scope: None,
            ids: FreezeRecordIds {
                lock_id: "L-readonly-fixture".into(),
                operation_record_id: "R-readonly-fixture".into(),
                reason_history_id: "H-readonly-fixture".into(),
            },
            parameters: LockReplayParameters {
                card_id: "isolated-a".into(),
                source: LockSource::UserSelf,
                reason: StoredReasonCode::UserReportLost,
                event_id: None,
                transaction_id: None,
                feedback_id: None,
            },
        };
        let mut tx = db.scoped.begin().await.unwrap();
        sqlx::query("SELECT txid_current()")
            .execute(&mut *tx)
            .await
            .unwrap();
        LockedCardAccount::acquire(&mut tx, "user_001", "account_001")
            .await
            .unwrap()
            .save_freeze(&c)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let app = router(state.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        state,
        address,
        handle,
    }
}
async fn command(live: &Server, cookie: &str, mut value: Value) -> Reply {
    if value["kind"] != "CANCEL" && value["kind"] != "CANCEL_REQUEST" {
        value["request_ref"] = Value::String(uuid::Uuid::new_v4().to_string());
    }
    live.call(
        Method::POST,
        "/api/card-read",
        Some(value.to_string()),
        Some(cookie),
        None,
        None,
        true,
    )
    .await
}
async fn snapshot(db: &Db) -> Value {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname=current_schema() ORDER BY tablename",
    )
    .fetch_all(&db.scoped)
    .await
    .unwrap();
    let mut out = serde_json::Map::new();
    for t in names {
        assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        let rows:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(x ORDER BY x::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) x FROM {t} t) a")).fetch_one(&db.scoped).await.unwrap();
        out.insert(t, rows);
    }
    Value::Object(out)
}
#[tokio::test]
#[ignore = "isolated PG and real HTTP"]
async fn public_card_http_chain_cookie_csrf_cross_session_original_restart_and_no_writes() {
    let db = Db::new().await;
    let live = server(&db, true).await;
    let cookie = live.login().await;
    let before = snapshot(&db).await;
    let input = json!({"kind":"QUERY","account_id":"account_001"});
    assert_eq!(
        command(&live, "", input.clone()).await.status,
        StatusCode::UNAUTHORIZED
    );
    let raw=json!({"kind":"QUERY","account_id":"account_001","request_ref":uuid::Uuid::new_v4().to_string()}).to_string();
    assert_eq!(
        live.call(
            Method::POST,
            "/api/card-read",
            Some(raw.clone()),
            Some(&cookie),
            None,
            Some("http://evil.example"),
            true
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        live.call(
            Method::POST,
            "/api/card-read",
            Some(raw),
            Some(&cookie),
            None,
            None,
            false
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        command(
            &live,
            &cookie,
            json!({"kind":"QUERY","account_id":"account_001","owner":"user_001"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert!(!command(
        &live,
        &cookie,
        json!({"kind":"QUERY","account_id":"foreign"})
    )
    .await
    .status
    .is_success());
    let v = command(&live, &cookie, input.clone()).await;
    assert_eq!(v.status, StatusCode::OK);
    let v = v.json;
    assert_eq!(v["cards"].as_array().unwrap().len(), 2);
    assert_eq!(v["cards"][0]["ambiguous_tail"], true);
    let other = live.login().await;
    assert!(!command(
        &live,
        &other,
        json!({"kind":"SELECT_CARD","step_ref":v["step_ref"],"position":1})
    )
    .await
    .status
    .is_success());
    let h = command(
        &live,
        &cookie,
        json!({"kind":"SELECT_CARD","step_ref":v["step_ref"],"position":1}),
    )
    .await;
    assert_eq!(h.status, StatusCode::OK);
    let o = command(
        &live,
        &cookie,
        json!({"kind":"SELECT_RECORD","step_ref":h.json["step_ref"],"position":1}),
    )
    .await;
    assert_eq!(o.status, StatusCode::OK);
    assert_eq!(o.json["original"]["state"], "RECORDED_ACTIVE");
    assert!(!o.json.to_string().contains("KEY-readonly"));
    let stale = command(
        &live,
        &cookie,
        json!({"kind":"SELECT_CARD","step_ref":v["step_ref"],"position":1}),
    )
    .await;
    assert!(!stale.status.is_success());
    let context = live
        .call(
            Method::GET,
            "/api/card-read/session",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await
        .json;
    let rotated = live
        .call(
            Method::POST,
            "/api/sim/session",
            Some(json!({"login_secret":SECRET}).to_string()),
            Some(&cookie),
            None,
            None,
            true,
        )
        .await;
    let new_cookie = Cookie::parse(rotated.headers[header::SET_COOKIE].to_str().unwrap()).unwrap();
    let new_cookie = format!("{}={}", new_cookie.name(), new_cookie.value());
    let next = live
        .call(
            Method::GET,
            "/api/card-read/session",
            None,
            Some(&new_cookie),
            None,
            None,
            false,
        )
        .await
        .json;
    assert_ne!(context["context_ref"], next["context_ref"]);
    assert_eq!(
        command(&live, &cookie, input.clone()).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(snapshot(&db).await, before);
    live.stop();
    let missing = server(&db, false).await;
    let cookie = missing.login().await;
    assert_eq!(
        command(&missing, &cookie, input).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    missing.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "manual isolated browser harness; explicit ready file required"]
async fn public_card_browser_harness() {
    let path = std::env::var("SAFEFLOW_CARD_BROWSER_READY_FILE")
        .expect("explicit isolated ready path required");
    let db = Db::new().await;
    let live = server(&db, true).await;
    let before = snapshot(&db).await;
    std::fs::write(&path,json!({"address":live.address.to_string(),"fixture":"isolated-public-card-only","ready":true}).to_string()).unwrap();
    let stop = format!("{path}.stop");
    for _ in 0..1200 {
        if std::path::Path::new(&stop).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        std::path::Path::new(&stop).exists(),
        "browser did not finish within isolated harness window"
    );
    assert_eq!(snapshot(&db).await, before);
    live.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PG and real HTTP SQL timeout"]
async fn public_card_http_member_sql_timeout_discards_step_without_business_writes() {
    let db = Db::new().await;
    let live = configured_server(&db, true, Duration::from_secs(1)).await;
    let cookie = live.login().await;
    let before = snapshot(&db).await;
    let list = command(
        &live,
        &cookie,
        json!({"kind":"QUERY","account_id":"account_001"}),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    let mut lock = db.scoped.begin().await.unwrap();
    lock.execute("LOCK TABLE card_freeze_operations_v1 IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let address = live.address;
    let c = cookie.clone();
    let old = list.json["step_ref"].clone();
    let old2 = old.clone();
    let task = tokio::spawn(async move {
        Server::call_at(address,Method::POST,"/api/card-read",
        Some(json!({"kind":"SELECT_CARD","step_ref":old2,"position":1,"request_ref":uuid::Uuid::new_v4().to_string()}).to_string()),Some(&c),None,None,true).await
    });
    let mut waited = false;
    for _ in 0..40 {
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid<>pg_backend_pid() AND wait_event_type='Lock' AND query LIKE '%SELECT owner_id,account_id,operation_id FROM card_freeze_operations_v1%')").fetch_one(&db.admin).await.unwrap();
        if active {
            waited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(waited, "HTTP did not reach member SQL Lock wait");
    let reply = task.await.unwrap();
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    lock.rollback().await.unwrap();
    assert!(!command(
        &live,
        &cookie,
        json!({"kind":"SELECT_CARD","step_ref":old,"position":1})
    )
    .await
    .status
    .is_success());
    assert_eq!(
        command(
            &live,
            &cookie,
            json!({"kind":"QUERY","account_id":"account_001"})
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(snapshot(&db).await, before);
    live.stop();
    db.finish().await;
}
