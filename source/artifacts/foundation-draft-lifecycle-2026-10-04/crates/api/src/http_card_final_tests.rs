//! Real HTTP final-card path; no native challenge secret enters HTTP logs.
use super::*;
use safeflow_contracts::{OperationId, ResourceId, SessionRef};
use safeflow_storage::{CardFinalDeployment, PgUnifiedFictionalSource, UnifiedFictionalDocument};
const CONTROL: &str = "step07-native-card-controller-secret-only";
fn session(cookie: &str) -> SessionRef {
    crate::session::token_reference(cookie.split_once('=').unwrap().1).unwrap()
}
async fn start(source: PgUnifiedFictionalSource, timeout: Duration) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.request_timeout = timeout;
    let state = ApiState::for_unified_card_safety(
        source,
        config,
        UserId::parse("user_001").unwrap(),
        Zeroizing::new(CONTROL.into()),
    )
    .await
    .unwrap();
    let app = router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        state,
        address,
        handle,
    }
}
async fn restricted(
    source: &PgUnifiedFictionalSource,
    admin_url: &str,
    namespace: &str,
) -> PgUnifiedFictionalSource {
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    let role = format!("sf_card_http_{}", uuid::Uuid::new_v4().simple());
    let password = Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ));
    CardFinalDeployment::for_isolated_backend(source.clone())
        .register_new_login(&role, password.clone())
        .await
        .unwrap();
    let opts = sqlx::postgres::PgConnectOptions::from_str(admin_url)
        .unwrap()
        .username(&role)
        .password(&password);
    let mut url = opts.to_url_lossy();
    url.set_password(Some(&password)).unwrap();
    let repo = PgRepository::connect_existing(url.as_str(), namespace)
        .await
        .unwrap();
    PgUnifiedFictionalSource::open(
        repo,
        safeflow_storage::UNIFIED_FICTIONAL_SOURCE,
        safeflow_storage::UNIFIED_FICTIONAL_REVISION,
    )
    .await
    .unwrap()
}
async fn fresh() -> (PgUnifiedFictionalSource, Server, String, String) {
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let raw = std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").unwrap()).unwrap();
    let namespace = "sf_unified_card_http".to_owned();
    let admin = PgUnifiedFictionalSource::provision_new_database(
        &url,
        &namespace,
        UnifiedFictionalDocument::read_adopted_v1(&raw).unwrap(),
    )
    .await
    .unwrap();
    let initial = start(admin.clone(), Duration::from_secs(60)).await;
    let s = initial
        .state
        .simulation_session_for_native_host(SECRET)
        .unwrap();
    initial
        .state
        .initialize_unified_catalog_for_backend(&s, &ResourceId::parse("account_001").unwrap())
        .await
        .unwrap();
    initial.stop();
    CardFinalDeployment::for_isolated_backend(admin.clone())
        .activate_exact(&[], &[])
        .await
        .unwrap();
    let api = restricted(&admin, &url, &namespace).await;
    let server = start(api, Duration::from_secs(2)).await;
    let cookie = server.login().await;
    (admin, server, cookie, namespace)
}
async fn action(server: &Server, cookie: &str, v: Value) -> Reply {
    server
        .call(
            Method::POST,
            "/api/card-safety",
            Some(v.to_string()),
            Some(cookie),
            None,
            None,
            true,
        )
        .await
}
async fn authorized(server: &Server, cookie: &str) -> Value {
    let r=action(server,cookie,json!({"kind":"PREPARE","account_id":"account_001","card_id":"demo_card_salary_v1","nonce":uuid::Uuid::new_v4().to_string()})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let v = r.json;
    let r=action(server,cookie,json!({"kind":"CONFIRM","operation_id":v["operation_id"],"confirmation_ref":v["confirmation_ref"]})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let op: OperationId = serde_json::from_value(v["operation_id"].clone()).unwrap();
    let mut delivery = server
        .state
        .card_challenge_for_native_controller(CONTROL, &session(cookie), &op)
        .await
        .unwrap();
    let secret = delivery.take_secret().unwrap();
    let r=action(server,cookie,json!({"kind":"VERIFY_CODE","operation_id":op,"challenge_id":delivery.challenge_id,"code":secret.expose_once_for_native_display()})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["verified"], true);
    let r = action(
        server,
        cookie,
        json!({"kind":"ISSUE_PERMIT","operation_id":op}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    v
}
#[tokio::test]
#[ignore="owned v40 native registered card principal before/after recipient upgrade; no registration or seeding"]
async fn existing_card_principal_recipient_upgrade_http() {
    let url=std::env::var("SAFEFLOW_EXISTING_CARD_API_URL").unwrap();
    let ns=std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let repo=PgRepository::connect_existing(&url,&ns).await.unwrap();
    let source=PgUnifiedFictionalSource::open(repo.clone(),safeflow_storage::UNIFIED_FICTIONAL_SOURCE,safeflow_storage::UNIFIED_FICTIONAL_REVISION).await.unwrap();
    let server=start(source,Duration::from_secs(5)).await;let cookie=server.login().await;
    let prepared=action(&server,&cookie,json!({"kind":"PREPARE","account_id":"account_001","card_id":"demo_card_salary_v1","nonce":uuid::Uuid::new_v4().to_string()})).await;
    assert_eq!(prepared.status,StatusCode::OK,"{}",prepared.json);
    let original=action(&server,&cookie,json!({"kind":"QUERY_ORIGINAL","operation_id":prepared.json["operation_id"]})).await;
    assert_eq!(original.status,StatusCode::OK,"{}",original.json);
    let denied=action(&server,&cookie,json!({"kind":"EXECUTE_FREEZE","operation_id":prepared.json["operation_id"]})).await;
    assert!(!denied.status.is_success());
    assert!(original.json.to_string().find("9900000000000001024").is_none());
    if std::env::var("SAFEFLOW_UPGRADE_CARD_EXECUTE").as_deref()==Ok("OWNED_ONLY") {
        let confirmed=authorized(&server,&cookie).await;
        let result=action(&server,&cookie,json!({"kind":"EXECUTE_FREEZE","operation_id":confirmed["operation_id"]})).await;
        assert_eq!(result.status,StatusCode::OK,"{}",result.json);assert_eq!(result.json["state"],"RECORDED");
        for kind in ["EXECUTE_FREEZE","QUERY_ORIGINAL","RETRY_ROLLED_BACK_FREEZE"] {
            let replay=action(&server,&cookie,json!({"kind":kind,"operation_id":confirmed["operation_id"]})).await;
            assert_eq!(replay.status,StatusCode::OK,"{}",replay.json);assert_eq!(replay.json,result.json);
        }
    }
    server.stop();repo.close().await;
}
#[tokio::test]
#[ignore="owned maintenance split-phase failure/recovery driver; no adoption or role registration"]
async fn recipient_upgrade_migration_phase_only() {
    let url=std::env::var("SAFEFLOW_BANK_CHAT_ADMIN_URL").unwrap();
    let ns=std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let repo=PgRepository::connect_existing(&url,&ns).await.unwrap();
    let source=PgUnifiedFictionalSource::open(repo.clone(),safeflow_storage::UNIFIED_FICTIONAL_SOURCE,safeflow_storage::UNIFIED_FICTIONAL_REVISION).await.unwrap();
    source.validate_recipient_profile_maintenance("sf_bank_llm_api").await.unwrap();
    repo.migrate().await.unwrap();repo.close().await;
}
#[tokio::test]
async fn final_card_real_http_and_strict_requests() {
    let (admin, server, cookie, _) = fresh().await;
    let v = authorized(&server, &cookie).await;
    for bad in [
        json!({"kind":"EXECUTE_FREEZE","operation_id":v["operation_id"],"owner_id":"user_002"}),
        json!({"kind":"EXECUTE_FREEZE","operation_id":v["operation_id"],"result":"SUCCESS"}),
        json!({"kind":"NATIVE_CHALLENGE","operation_id":v["operation_id"]}),
        json!({"kind":"UNLOCK","operation_id":v["operation_id"]}),
    ] {
        assert_eq!(
            action(&server, &cookie, bad).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    let bad = server
        .call(
            Method::POST,
            "/api/card-safety",
            Some(json!({"kind":"EXECUTE_FREEZE","operation_id":v["operation_id"]}).to_string()),
            None,
            None,
            None,
            true,
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    let result = action(
        &server,
        &cookie,
        json!({"kind":"EXECUTE_FREEZE","operation_id":v["operation_id"]}),
    )
    .await;
    assert_eq!(result.status, StatusCode::OK, "{}", result.json);
    assert_eq!(result.json["state"], "RECORDED");
    for kind in [
        "EXECUTE_FREEZE",
        "QUERY_ORIGINAL",
        "RETRY_ROLLED_BACK_FREEZE",
    ] {
        let r = action(
            &server,
            &cookie,
            json!({"kind":kind,"operation_id":v["operation_id"]}),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        assert_eq!(r.json, result.json);
    }
    server.stop();
    admin.repository_for_isolated_backend().close().await;
}
#[tokio::test]
#[ignore = "only owned loopback commit-fault/OS-restart driver"]
async fn final_card_recovery_child() {
    use std::io::{BufRead, Write};
    assert_eq!(
        std::env::var("SAFEFLOW_CARD_RECOVERY").unwrap(),
        "OWNED_CLUSTER_ONLY"
    );
    let resume = std::env::var("SAFEFLOW_CARD_RESUME").ok();
    let (admin, server, cookie, namespace) = if let Some(h) = resume.as_ref() {
        let h: Value = serde_json::from_str(h).unwrap();
        let namespace = h["namespace"].as_str().unwrap().to_owned();
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let repo = PgRepository::connect_existing(&url, &namespace)
            .await
            .unwrap();
        let admin = PgUnifiedFictionalSource::open(
            repo,
            safeflow_storage::UNIFIED_FICTIONAL_SOURCE,
            safeflow_storage::UNIFIED_FICTIONAL_REVISION,
        )
        .await
        .unwrap();
        let api = restricted(&admin, &url, &namespace).await;
        let server = start(api, Duration::from_secs(2)).await;
        let cookie = server.login().await;
        (admin, server, cookie, namespace)
    } else {
        fresh().await
    };
    let v = if let Some(h) = resume {
        serde_json::from_str::<Value>(&h).unwrap()["card"].clone()
    } else {
        authorized(&server, &cookie).await
    };
    let emit = |v: Value| {
        println!("CARD_RECOVERY_REPLY:{v}");
        std::io::stdout().flush().unwrap();
    };
    emit(
        json!({"ready":true,"pid":std::process::id(),"namespace":namespace,"card":{"operation_id":v["operation_id"]},"address":server.address.to_string()}),
    );
    for line in std::io::stdin().lock().lines() {
        let q: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match q["kind"].as_str().unwrap() {
            "NEW_EXECUTE" => {
                let next=authorized(&server,&cookie).await;
                let r=action(&server,&cookie,json!({"kind":"EXECUTE_FREEZE","operation_id":next["operation_id"]})).await;
                emit(json!({"status":r.status.as_u16(),"result":r.json,"new_operation_id":next["operation_id"]}));
            }
            "EXECUTE" | "QUERY" | "RETRY" => {
                let kind = match q["kind"].as_str().unwrap() {
                    "EXECUTE" => "EXECUTE_FREEZE",
                    "RETRY" => "RETRY_ROLLED_BACK_FREEZE",
                    _ => "QUERY_ORIGINAL",
                };
                let r = action(
                    &server,
                    &cookie,
                    json!({"kind":kind,"operation_id":v["operation_id"]}),
                )
                .await;
                emit(json!({"status":r.status.as_u16(),"result":r.json}));
            }
            "STOP" => {
                emit(json!({"stopping":true}));
                break;
            }
            _ => panic!("unknown owned parent command"),
        }
    }
    server.stop();
    admin.repository_for_isolated_backend().close().await;
}
