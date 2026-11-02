//! Isolated real PostgreSQL, unchanged original member archive and public composer.
use super::*;
use safeflow_storage::{BillingRecoveryConfig, BillingRecoveryNonce};
fn config(db: &Db, source: &str) -> BillingRecoveryConfig {
    BillingRecoveryConfig::new(
        &uuid::Uuid::new_v4().to_string(),
        &db.namespace,
        source,
        "8ff8c665:mixed",
        "isolated-candidate-continuity-1",
        8,
        32,
        1000,
        2000,
        1000,
        2000,
        true,
    )
    .unwrap()
}
fn nonce() -> BillingRecoveryNonce {
    BillingRecoveryNonce::parse(&uuid::Uuid::new_v4().to_string()).unwrap()
}
fn targets(
    v: &Value,
) -> (
    safeflow_contracts::TaskId,
    safeflow_contracts::OperationId,
    safeflow_contracts::ConfirmationRef,
) {
    (
        serde_json::from_value(v["task_id"].clone()).unwrap(),
        serde_json::from_value(v["operation_id"].clone()).unwrap(),
        serde_json::from_value(v["confirmation"]["confirmation_ref"].clone()).unwrap(),
    )
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL and original member protocol fixtures"]
async fn billing_recovery_v2_real_prepare_replay_close_execute() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let input = intent(&complete["results"][0]["data"]["report"]);
    let cfg = config(&db, &input.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let feedback = service(&server);
    let s = session(&cookie);
    let n = nonce();
    let realm = cfg.realm();
    let prepared = feedback
        .prepare_v2(&s, &cfg, &realm, &n, input.clone())
        .await
        .unwrap();
    assert_eq!(prepared["kind"], "CREATED");
    let (task, operation, reference) = targets(&prepared);
    let repeat = feedback
        .prepare_v2(&s, &cfg, &realm, &n, input.clone())
        .await
        .unwrap();
    assert_eq!(repeat["kind"], "RECOVERY_ONLY");
    assert!(repeat.get("confirmation").is_none());
    assert_eq!(count(&db, "billing_prepare_requests_v2").await, 1);
    feedback
        .confirm_preparation_v2(&s, &cfg, &realm, &n, &task, &operation, &reference)
        .await
        .unwrap();
    let (closing_a, closing_b) = tokio::join!(
        feedback.abandon_preparation_v2(&s, &cfg, &realm, &n),
        feedback.abandon_preparation_v2(&s, &cfg, &realm, &n)
    );
    let closed = closing_a.unwrap();
    assert_eq!(closing_b.unwrap(), closed);
    assert_eq!(closed["state"], "ABANDONED");
    assert_eq!(
        feedback
            .abandon_preparation_v2(&s, &cfg, &realm, &n)
            .await
            .unwrap(),
        closed
    );
    assert!(feedback
        .execute_preparation_v2(&s, &cfg, &realm, &n, &task, &operation, &reference)
        .await
        .is_err());
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    let n2 = nonce();
    let p = feedback
        .prepare_v2(&s, &cfg, &realm, &n2, input.clone())
        .await
        .unwrap();
    let (t, o, r) = targets(&p);
    feedback
        .confirm_preparation_v2(&s, &cfg, &realm, &n2, &t, &o, &r)
        .await
        .unwrap();
    let (execute_a, execute_b) = tokio::join!(
        feedback.execute_preparation_v2(&s, &cfg, &realm, &n2, &t, &o, &r),
        feedback.execute_preparation_v2(&s, &cfg, &realm, &n2, &t, &o, &r)
    );
    assert_eq!(
        usize::from(execute_a.is_ok()) + usize::from(execute_b.is_ok()),
        1
    );
    let saved = execute_a.or(execute_b).unwrap();
    assert_eq!(saved["state"], "SAVED");
    assert_eq!(count(&db, "billing_feedback_v1").await, 1);
    assert_eq!(count(&db, "billing_prepare_attempts_v2").await, 1);
    let read = feedback
        .read_preparation_v2(&s, &cfg, &realm, &n2)
        .await
        .unwrap();
    assert_eq!(read["state"], "SAVED");
    assert!(read.get("confirmation").is_none());
    assert!(feedback
        .prepare_v2(&s, &cfg, &realm, &nonce(), input)
        .await
        .is_err());
    server.stop();
    db.finish().await;
}

async fn v2_report(
    db: &Db,
    provider: &BillingProvider,
) -> (Server, String, Value, BillingRecoveryConfig) {
    use safeflow_billing_source_reader::query::PagedSource;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let cfg = config(db, source.source_id());
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let server = billing_server_with_recovery(
        db,
        provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source, "8ff8c665:mixed")),
        false,
        true,
        None,
        Some(cfg.clone()),
    )
    .await;
    let cookie = server.login().await;
    let view = initial(&server, &cookie, "report").await;
    let complete = ok(execute(&server, &cookie, &view).await);
    (server, cookie, complete, cfg)
}
fn request(cfg: &BillingRecoveryConfig, n: &BillingRecoveryNonce, i: &FeedbackIntent) -> Value {
    json!({"contract_version":2,"recovery_realm":cfg.realm(),"nonce":n.as_string(),"intent":{"input_version":1,"report_id":i.report_id,"transaction":i.transaction,"choice":i.choice}})
}
fn action(cfg: &BillingRecoveryConfig, p: &Value) -> Value {
    json!({"contract_version":2,"recovery_realm":cfg.realm(),"task_id":p["task_id"],"operation_id":p["operation_id"],"confirmation_ref":p["confirmation"]["confirmation_ref"]})
}
fn path(n: &BillingRecoveryNonce, action: &str) -> String {
    format!(
        "/api/billing-feedback/v2/preparations/{}/{}",
        n.as_string(),
        action
    )
}
async fn read(
    server: &Server,
    cookie: &str,
    cfg: &BillingRecoveryConfig,
    n: &BillingRecoveryNonce,
) -> Reply {
    server
        .call(
            Method::GET,
            &format!(
                "/api/billing-feedback/v2/preparations/{}?recovery_realm={}",
                n.as_string(),
                cfg.realm()
            ),
            None,
            Some(cookie),
            None,
            None,
            false,
        )
        .await
}
/// A real loopback proxy forwards the exact request, consumes the committed HTTP
/// response, then closes without delivering any response byte to its client.
async fn lose_response(server: &Server, cookie: &str, path: &str, body: Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let json = body.to_string();
    let raw=format!("POST {path} HTTP/1.1\r\nHost: {}\r\nCookie: {cookie}\r\nX-SafeFlow-Request: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",server.address,json.len());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = server.address;
    let len = raw.len();
    let proxy = tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; len];
        client.read_exact(&mut bytes).await.unwrap();
        let mut api = tokio::net::TcpStream::connect(upstream).await.unwrap();
        api.write_all(&bytes).await.unwrap();
        let mut response = vec![];
        api.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200") || response.starts_with(b"HTTP/1.1 201"));
        client.shutdown().await.unwrap();
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(raw.as_bytes()).await.unwrap();
    let mut response = vec![];
    client.read_to_end(&mut response).await.unwrap();
    assert!(response.is_empty());
    proxy.await.unwrap();
}
#[tokio::test]
#[ignore = "owned isolated PG and real in-flight loopback HTTP request gates"]
async fn billing_recovery_v2_404_late_prepare_close_both_commit_orders() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, cookie, complete, cfg) = v2_report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let n = nonce();
    let body = request(&cfg, &n, &i).to_string();
    let raw=format!("POST /api/billing-feedback/v2/preparations HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nX-SafeFlow-Request: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",server.address,cookie,body.len(),body);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = server.address;
    let len = raw.len();
    let (release, gate) = tokio::sync::oneshot::channel();
    let (ready, received) = tokio::sync::oneshot::channel();
    let proxy = tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut raw = vec![0; len];
        client.read_exact(&mut raw).await.unwrap();
        ready.send(()).unwrap();
        gate.await.unwrap();
        let mut api = tokio::net::TcpStream::connect(upstream).await.unwrap();
        api.write_all(&raw).await.unwrap();
        let mut response = vec![];
        api.read_to_end(&mut response).await.unwrap();
        client.write_all(&response).await.unwrap();
        client.shutdown().await.unwrap();
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(raw.as_bytes()).await.unwrap();
    received.await.unwrap();
    assert_eq!(
        read(&server, &cookie, &cfg, &n).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &path(&n, "abandon"),
            json!({"contract_version":2,"recovery_realm":cfg.realm()})
        )
        .await
        .status,
        StatusCode::OK
    );
    release.send(()).unwrap();
    let mut late = vec![];
    client.read_to_end(&mut late).await.unwrap();
    proxy.await.unwrap();
    assert!(late.starts_with(b"HTTP/1.1 410"));
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    // Opposite order: prepare is inside its transaction but has not committed.
    db.scoped.execute("CREATE FUNCTION recovery_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER recovery_wait BEFORE INSERT ON billing_prepare_requests_v2 FOR EACH ROW EXECUTE FUNCTION recovery_wait()").await.unwrap();
    let mut blocker = db.scoped.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let n = nonce();
    let address = server.address;
    let pc = cookie.clone();
    let raw = request(&cfg, &n, &i);
    let preparing = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            "/api/billing-feedback/v2/preparations",
            Some(raw.to_string()),
            Some(&pc),
            None,
            None,
            true,
        )
        .await
    });
    wait_for_query(&db, "INSERT INTO billing_prepare_requests_v2%").await;
    assert_eq!(
        read(&server, &cookie, &cfg, &n).await.status,
        StatusCode::NOT_FOUND
    );
    let address = server.address;
    let pc = cookie.clone();
    let cp = path(&n, "abandon");
    let cb = json!({"contract_version":2,"recovery_realm":cfg.realm()});
    let closing = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &cp,
            Some(cb.to_string()),
            Some(&pc),
            None,
            None,
            true,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!closing.is_finished());
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    assert_eq!(preparing.await.unwrap().status, StatusCode::CREATED);
    assert_eq!(closing.await.unwrap().status, StatusCode::OK);
    assert_eq!(
        read(&server, &cookie, &cfg, &n).await.json["state"],
        "ABANDONED"
    );
    assert_eq!(count(&db, "billing_prepare_requests_v2").await, 2);
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL and real HTTP response loss proxy"]
async fn billing_recovery_v2_lost_prepare_execute_abandon_responses_reconcile_only() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, cookie, complete, cfg) = v2_report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let n = nonce();
    lose_response(
        &server,
        &cookie,
        "/api/billing-feedback/v2/preparations",
        request(&cfg, &n, &i),
    )
    .await;
    let recovered = read(&server, &cookie, &cfg, &n).await;
    assert_eq!(recovered.status, StatusCode::OK);
    assert_eq!(recovered.json["state"], "PREPARED_OPEN");
    assert!(!recovered.json.to_string().contains("confirmation_ref"));
    lose_response(
        &server,
        &cookie,
        &path(&n, "abandon"),
        json!({"contract_version":2,"recovery_realm":cfg.realm()}),
    )
    .await;
    assert_eq!(
        read(&server, &cookie, &cfg, &n).await.json["state"],
        "ABANDONED"
    );
    let n = nonce();
    let reply = command(
        &server,
        &cookie,
        "/api/billing-feedback/v2/preparations",
        request(&cfg, &n, &i),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let p = reply.json;
    command(&server, &cookie, &path(&n, "confirm"), action(&cfg, &p)).await;
    lose_response(&server, &cookie, &path(&n, "execute"), action(&cfg, &p)).await;
    assert_eq!(
        read(&server, &cookie, &cfg, &n).await.json["state"],
        "SAVED"
    );
    assert_eq!(count(&db, "billing_feedback_v1").await, 1);
    assert_eq!(
        command(&server, &cookie, &path(&n, "execute"), action(&cfg, &p))
            .await
            .status,
        StatusCode::CONFLICT
    );
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "task-owned plaintext PostgreSQL protocol acknowledgement-loss proxy"]
async fn billing_recovery_v2_admission_commit_ack_loss_mints_no_execution_token() {
    let mut db = Db::new().await;
    let start = db
        .url
        .find("@127.0.0.1:")
        .expect("explicit isolated loopback PostgreSQL URL")
        + 1;
    let slash = db.url[start..].find('/').unwrap() + start;
    let upstream: std::net::SocketAddr = db.url[start..slash].parse().unwrap();
    let proxy = pg_fault::CommitLossProxy::start(upstream, false).await;
    let proxy_url = format!("{}{}{}", &db.url[..start], proxy.address, &db.url[slash..]);
    db.repo.close().await;
    db.repo = PgRepository::connect(&proxy_url, &db.namespace)
        .await
        .unwrap();
    let provider = BillingProvider::new("mixed").await;
    let (server, cookie, complete, cfg) = v2_report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let s = session(&cookie);
    let n = nonce();
    let f = service(&server);
    let p = f.prepare_v2(&s, &cfg, &cfg.realm(), &n, i).await.unwrap();
    let (t, o, r) = targets(&p);
    f.confirm_preparation_v2(&s, &cfg, &cfg.realm(), &n, &t, &o, &r)
        .await
        .unwrap();
    assert!(matches!(
        f.execute_preparation_v2(&s, &cfg, &cfg.realm(), &n, &t, &o, &r)
            .await,
        Err(StorageError::ReconcileRequired)
    ));
    assert!(proxy.fired.load(Ordering::SeqCst));
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    assert_eq!(count(&db, "public_execution_requests").await, 0);
    assert_eq!(
        f.read_preparation_v2(&s, &cfg, &cfg.realm(), &n)
            .await
            .unwrap()["state"],
        "EXECUTION_ADMITTED"
    );
    assert!(f
        .execute_preparation_v2(&s, &cfg, &cfg.realm(), &n, &t, &o, &r)
        .await
        .is_err());
    assert!(f
        .abandon_preparation_v2(&s, &cfg, &cfg.realm(), &n)
        .await
        .is_err());
    server.stop();
    db.finish().await;
    drop(proxy);
}
#[tokio::test]
#[ignore = "owned PostgreSQL BUSINESS COMMIT protocol acknowledgement loss, original GET only"]
async fn billing_recovery_v2_business_commit_ack_loss_preserves_atomic_saved_and_never_rewrites() {
    let mut db = Db::new().await;
    let start = db.url.find("@127.0.0.1:").expect("owned loopback PG") + 1;
    let slash = db.url[start..].find('/').unwrap() + start;
    let upstream = db.url[start..slash].parse().unwrap();
    let proxy = pg_fault::CommitLossProxy::start(upstream, true).await;
    let proxy_url = format!("{}{}{}", &db.url[..start], proxy.address, &db.url[slash..]);
    db.repo.close().await;
    db.repo = PgRepository::connect(&proxy_url, &db.namespace)
        .await
        .unwrap();
    let provider = BillingProvider::new("mixed").await;
    let (server, cookie, complete, cfg) = v2_report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let n = nonce();
    let prepared = command(
        &server,
        &cookie,
        "/api/billing-feedback/v2/preparations",
        request(&cfg, &n, &i),
    )
    .await;
    assert_eq!(prepared.status, StatusCode::CREATED);
    let p = prepared.json;
    assert_eq!(
        command(&server, &cookie, &path(&n, "confirm"), action(&cfg, &p))
            .await
            .status,
        StatusCode::OK
    );
    let address = server.address;
    let c = cookie.clone();
    let execute_path = path(&n, "execute");
    let execute_body = action(&cfg, &p).to_string();
    let pending = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &execute_path,
            Some(execute_body),
            Some(&c),
            None,
            None,
            true,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), proxy.commit_seen.notified())
        .await
        .unwrap();
    // The exact business COMMIT is held before reaching PG: none of its member,
    // public, final-state or Outbox effects may be visible on another connection.
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    assert_eq!(count(&db, "billing_feedback_events_v1").await, 0);
    assert_eq!(count(&db, "public_billing_feedback_results_v1").await, 0);
    let visible: String =
        sqlx::query_scalar("SELECT state FROM billing_prepare_requests_v2 WHERE nonce=$1::uuid")
            .bind(n.as_string())
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(visible, "EXECUTION_ADMITTED");
    let audit_before = count(&db, "audit_events").await;
    let outbox_before = count(&db, "public_audit_outbox").await;
    proxy.release.notify_one();
    let outcome = pending.await.unwrap();
    assert_eq!(outcome.status, StatusCode::CONFLICT);
    assert_eq!(outcome.json["code"], "RECONCILE_REQUIRED");
    assert!(proxy.fired.load(Ordering::SeqCst));
    // Direct upstream queries prove the server committed even though SQLx did not
    // receive COMMIT's acknowledgement. Public final rows share the parent xid;
    // original member SAVEPOINT rows have a PG subtransaction xid, not that xid.
    let business_txid:i64=sqlx::query_scalar("SELECT business_txid FROM billing_prepare_attempts_v2 WHERE nonce=$1::uuid AND state='BUSINESS_COMMITTED'")
        .bind(n.as_string()).fetch_one(&db.scoped).await.unwrap();
    for sql in [
        "SELECT xmin::text::bigint FROM billing_prepare_requests_v2 WHERE state='SAVED'",
        "SELECT xmin::text::bigint FROM billing_prepare_attempts_v2 WHERE state='BUSINESS_COMMITTED'",
        "SELECT xmin::text::bigint FROM public_billing_feedback_results_v1",
    ] {
        let xids:Vec<i64>=sqlx::query_scalar(sql).fetch_all(&db.scoped).await.unwrap();
        assert!(!xids.is_empty(),"{sql}");
        assert!(xids.iter().all(|x|*x==business_txid),"{sql}");
    }
    let task_xid: i64 = sqlx::query_scalar(
        "SELECT xmin::text::bigint FROM tasks WHERE task_id=$1 AND state='COMPLETED'",
    )
    .bind(p["task_id"].as_str().unwrap())
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(task_xid, business_txid);
    let operation_xid: i64 = sqlx::query_scalar(
        "SELECT xmin::text::bigint FROM operations WHERE operation_id=$1 AND state='SUCCEEDED'",
    )
    .bind(p["operation_id"].as_str().unwrap())
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(operation_xid, business_txid);
    let audit_xids:Vec<(i64,i64)>=sqlx::query_as("SELECT a.xmin::text::bigint,o.xmin::text::bigint FROM audit_events a JOIN public_audit_outbox o USING(event_id) WHERE a.task_id=$1 AND a.xmin::text::bigint=$2")
        .bind(p["task_id"].as_str().unwrap()).bind(business_txid).fetch_all(&db.scoped).await.unwrap();
    assert!(audit_xids.len() >= 3);
    assert!(audit_xids
        .iter()
        .all(|(a, o)| *a == business_txid && *o == business_txid));
    assert_eq!(count(&db, "billing_feedback_events_v1").await, 1);
    assert_eq!(
        count(&db, "audit_events").await - audit_before,
        audit_xids.len() as i64
    );
    assert_eq!(
        count(&db, "public_audit_outbox").await - outbox_before,
        audit_xids.len() as i64
    );
    let before = snapshot(&db).await;
    assert_eq!(
        read(&server, &cookie, &cfg, &n).await.json["state"],
        "SAVED"
    );
    let original = server
        .call(
            Method::GET,
            &format!(
                "/api/billing-feedback/operations/{}",
                p["operation_id"].as_str().unwrap()
            ),
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(original.status, StatusCode::OK);
    assert_eq!(original.json["task_state"], "COMPLETED");
    assert_eq!(
        command(&server, &cookie, &path(&n, "execute"), action(&cfg, &p))
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &path(&n, "abandon"),
            json!({"contract_version":2,"recovery_realm":cfg.realm()})
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(count(&db, "billing_feedback_v1").await, 1);
    assert_eq!(count(&db, "billing_prepare_attempts_v2").await, 1);
    server.stop();
    db.finish().await;
    drop(proxy);
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL and loopback HTTP"]
async fn billing_recovery_v2_http_strict_dto_perimeter_default_and_saved_get() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, cookie, complete, cfg) = v2_report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let n = nonce();
    let raw = request(&cfg, &n, &i);
    for (field, value) in [
        ("owner_id", json!("forged")),
        ("contract_version", json!(1)),
        ("nonce", json!(n.as_string().to_uppercase())),
    ] {
        let mut invalid = raw.clone();
        invalid[field] = value;
        assert_eq!(
            command(
                &server,
                &cookie,
                "/api/billing-feedback/v2/preparations",
                invalid
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    let mut nested = raw.clone();
    nested["intent"]["transaction"]["extra"] = json!(true);
    assert_eq!(
        command(
            &server,
            &cookie,
            "/api/billing-feedback/v2/preparations",
            nested
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let duplicate = raw.to_string().replacen(
        "\"contract_version\":2",
        "\"contract_version\":2,\"contract_version\":2",
        1,
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/billing-feedback/v2/preparations",
                Some(duplicate),
                Some(&cookie),
                None,
                None,
                true
            )
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/billing-feedback/v2/preparations",
                Some(raw.to_string()),
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
        server
            .call(
                Method::POST,
                "/api/billing-feedback/v2/preparations",
                Some(raw.to_string()),
                None,
                None,
                None,
                true
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let reply = command(
        &server,
        &cookie,
        "/api/billing-feedback/v2/preparations",
        raw,
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let p = reply.json;
    assert_eq!(
        command(&server, &cookie, &path(&n, "execute"), action(&cfg, &p))
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        command(&server, &cookie, &path(&n, "confirm"), action(&cfg, &p))
            .await
            .status,
        StatusCode::OK
    );
    let get = read(&server, &cookie, &cfg, &n).await;
    assert_eq!(get.status, StatusCode::OK);
    assert!(!get.json.to_string().contains("confirmation_ref"));
    assert_eq!(get.json["confirmation_observation"]["status"], "CONFIRMED");
    let saved = command(&server, &cookie, &path(&n, "execute"), action(&cfg, &p)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.json);
    let saved_get = server
        .call(
            Method::GET,
            &format!(
                "/api/billing-feedback/operations/{}",
                p["operation_id"].as_str().unwrap()
            ),
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(saved_get.status, StatusCode::OK);
    assert_eq!(saved_get.json["saved"], saved.json["result"]["saved"]);
    assert_eq!(
        command(
            &server,
            &cookie,
            &path(&n, "abandon"),
            json!({"contract_version":2,"recovery_realm":cfg.realm()})
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let disabled = Server::new(
        &db,
        "user_demo",
        Duration::from_secs(5),
        Duration::from_secs(60),
    )
    .await;
    let dc = disabled.login().await;
    let c = disabled
        .call(
            Method::GET,
            "/api/billing-feedback/v2/context",
            None,
            Some(&dc),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(c.status, StatusCode::OK);
    assert_eq!(c.json["available"], false);
    assert_eq!(
        command(
            &disabled,
            &dc,
            "/api/billing-feedback/v2/preparations",
            request(&cfg, &nonce(), &i)
        )
        .await
        .status,
        StatusCode::NOT_IMPLEMENTED
    );
    disabled.stop();
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL concurrent real transactions"]
async fn billing_recovery_v2_nonce_concurrency_conflicts_fences_and_realms() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let f = service(&server);
    let s = session(&cookie);
    let n = nonce();
    let realm = cfg.realm();
    let run = || f.prepare_v2(&s, &cfg, &realm, &n, i.clone());
    let results = tokio::join!(run(), run(), run(), run());
    let values = [
        results.0.unwrap(),
        results.1.unwrap(),
        results.2.unwrap(),
        results.3.unwrap(),
    ];
    assert_eq!(values.iter().filter(|v| v["kind"] == "CREATED").count(), 1);
    assert!(values
        .windows(2)
        .all(|v| v[0]["operation_id"] == v[1]["operation_id"]));
    assert_eq!(count(&db, "billing_prepare_requests_v2").await, 1);
    let later = initial(
        &server,
        &cookie,
        "same transaction in a distinct original report",
    )
    .await;
    let later_complete = ok(execute(&server, &cookie, &later).await);
    let later_intent = intent(&later_complete["results"][0]["data"]["report"]);
    assert_ne!(later_intent.report_id, i.report_id);
    assert_eq!(later_intent.transaction, i.transaction);
    assert!(matches!(
        f.prepare_v2(&s, &cfg, &realm, &n, later_intent.clone())
            .await,
        Err(StorageError::BillingRecovery("PREPARATION_KEY_CONFLICT"))
    ));
    assert!(matches!(
        f.prepare_v2(&s, &cfg, &realm, &nonce(), later_intent).await,
        Err(StorageError::BillingRecovery("PREPARATION_ALREADY_OPEN"))
    ));
    let mut changed = i.clone();
    changed.choice = FeedbackChoice::Mine;
    assert!(matches!(
        f.prepare_v2(&s, &cfg, &realm, &n, changed).await,
        Err(StorageError::BillingRecovery("PREPARATION_KEY_CONFLICT"))
    ));
    for field in ["source", "account", "transaction"] {
        let mut changed = i.clone();
        match field {
            "source" => changed.transaction.source_id.push_str("-different"),
            "account" => changed.transaction.account_id.push_str("-different"),
            _ => changed.transaction.transaction_id.push_str("-different"),
        }
        assert!(matches!(
            f.prepare_v2(&s, &cfg, &realm, &n, changed).await,
            Err(StorageError::BillingRecovery("PREPARATION_KEY_CONFLICT"))
        ));
    }
    assert!(matches!(
        f.prepare_v2(&s, &cfg, &realm, &nonce(), i.clone()).await,
        Err(StorageError::BillingRecovery("PREPARATION_ALREADY_OPEN"))
    ));
    let wrong = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        f.read_preparation_v2(&s, &cfg, &wrong, &n).await,
        Err(StorageError::BillingRecovery("RECOVERY_REALM_MISMATCH"))
    ));
    let original = values.iter().find(|v| v["kind"] == "CREATED").unwrap();
    let (wt, wo, wr) = targets(original);
    let before_realm = snapshot(&db).await;
    for result in [
        f.read_operation_preparation_v2(&s, &cfg, &wrong, &wo).await,
        f.confirm_preparation_v2(&s, &cfg, &wrong, &n, &wt, &wo, &wr)
            .await,
        f.execute_preparation_v2(&s, &cfg, &wrong, &n, &wt, &wo, &wr)
            .await,
        f.abandon_preparation_v2(&s, &cfg, &wrong, &n).await,
        f.prepare_v2(&s, &cfg, &wrong, &nonce(), i.clone()).await,
    ] {
        assert!(matches!(
            result,
            Err(StorageError::BillingRecovery("RECOVERY_REALM_MISMATCH"))
        ));
    }
    assert_eq!(snapshot(&db).await, before_realm);
    assert_eq!(count(&db, "billing_prepare_attempts_v2").await, 0);
    let unknown = nonce();
    assert!(matches!(
        f.read_preparation_v2(&s, &cfg, &realm, &unknown).await,
        Err(StorageError::NotFound)
    ));
    let closed = f
        .abandon_preparation_v2(&s, &cfg, &realm, &unknown)
        .await
        .unwrap();
    assert!(closed["task_id"].is_null());
    assert!(matches!(
        f.prepare_v2(&s, &cfg, &realm, &unknown, i.clone()).await,
        Err(StorageError::BillingRecovery("PREPARATION_CLOSED"))
    ));
    let other = billing_server_with_feedback(
        &db,
        &provider,
        "different_owner",
        fixture::bank(),
        None,
        false,
        true,
    )
    .await;
    let oc = other.login().await;
    let os = session(&oc);
    let of = service(&other);
    assert!(matches!(
        of.read_preparation_v2(&os, &cfg, &realm, &n).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        of.abandon_preparation_v2(&os, &cfg, &realm, &n).await,
        Err(StorageError::NotFound)
    ));
    let global = nonce();
    of.abandon_preparation_v2(&os, &cfg, &realm, &global)
        .await
        .unwrap();
    assert!(f
        .prepare_v2(&s, &cfg, &realm, &global, i.clone())
        .await
        .is_err());
    // Current same-owner new session reads or closes, but cannot consume an old ref.
    let nc = server.login().await;
    let ns = session(&nc);
    let p = values.iter().find(|v| v["kind"] == "CREATED").unwrap();
    let (t, o, r) = targets(p);
    assert!(f
        .confirm_preparation_v2(&ns, &cfg, &realm, &n, &t, &o, &r)
        .await
        .is_err());
    assert_eq!(
        f.read_preparation_v2(&ns, &cfg, &realm, &n).await.unwrap()["state"],
        "PREPARED_OPEN"
    );
    f.abandon_preparation_v2(&ns, &cfg, &realm, &n)
        .await
        .unwrap();
    assert!(f.execute(&s, &t, &o, &r).await.is_err());
    assert!(f.prepare(&s, i).await.is_err());
    // Raw SQL cannot mutate nonce/owner or erase a permanent fence.
    assert!(
        sqlx::query("DELETE FROM billing_prepare_requests_v2 WHERE nonce=$1::uuid")
            .bind(n.as_string())
            .execute(&db.scoped)
            .await
            .is_err()
    );
    assert!(sqlx::query(
        "UPDATE billing_prepare_requests_v2 SET owner_id='forged' WHERE nonce=$1::uuid"
    )
    .bind(n.as_string())
    .execute(&db.scoped)
    .await
    .is_err());
    assert!(sqlx::query(
        "UPDATE billing_prepare_requests_v2 SET nonce=$1::uuid WHERE nonce=$2::uuid"
    )
    .bind(nonce().as_string())
    .bind(n.as_string())
    .execute(&db.scoped)
    .await
    .is_err());
    other.stop();
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL explicit small quota boundaries"]
async fn billing_recovery_v2_capacity_retained_fences_and_rate_survive_new_pool() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let cfg = BillingRecoveryConfig::new(
        &uuid::Uuid::new_v4().to_string(),
        &db.namespace,
        &i.transaction.source_id,
        "8ff8c665:mixed",
        "owned-quota-test",
        1,
        2,
        100,
        200,
        1,
        200,
        true,
    )
    .unwrap();
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let f = service(&server);
    let s = session(&cookie);
    let realm = cfg.realm();
    let first = nonce();
    f.prepare_v2(&s, &cfg, &realm, &first, i.clone())
        .await
        .unwrap();
    let a = nonce();
    let b = nonce();
    let (ar, br) = tokio::join!(
        f.abandon_preparation_v2(&s, &cfg, &realm, &a),
        f.abandon_preparation_v2(&s, &cfg, &realm, &b)
    );
    assert_eq!(usize::from(ar.is_ok()) + usize::from(br.is_ok()), 1);
    assert_eq!(count(&db, "billing_prepare_requests_v2").await, 2);
    // Closing an existing root may commit even if its final observation is rate-limited.
    let _ = f.abandon_preparation_v2(&s, &cfg, &realm, &first).await;
    let state: String =
        sqlx::query_scalar("SELECT state FROM billing_prepare_requests_v2 WHERE nonce=$1::uuid")
            .bind(first.as_string())
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(state, "ABANDONED");
    assert!(matches!(
        f.prepare_v2(&s, &cfg, &realm, &nonce(), i).await,
        Err(StorageError::BillingRecovery("PREPARATION_CAPACITY"))
    ));
    let other_pool = PgRepository::connect(&db.url, &db.namespace).await.unwrap();
    other_pool
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let sf = BillingFeedbackService::for_trusted_backend(
        other_pool.clone(),
        server.state.0.sessions.clone(),
        server.state.0.bank.clone(),
        server.state.0.runtime.runtime(),
        ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),
        PermitConfig::new(Duration::from_secs(300)).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        sf.read_preparation_v2(&s, &cfg, &realm, &first).await,
        Err(StorageError::BillingRecovery("PREPARATION_CAPACITY"))
    ));
    other_pool.close().await;
    server.stop();
    db.finish().await;
}
struct RecoveryClock(std::sync::atomic::AtomicU64);
impl safeflow_storage::ConfirmationClock for RecoveryClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.0.load(Ordering::SeqCst) as i64).unwrap()
    }
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL explicit public clock injection"]
async fn billing_recovery_v2_original_expiry_backward_clock_and_installation_continuity() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let clock = Arc::new(RecoveryClock(std::sync::atomic::AtomicU64::new(now)));
    let f = BillingFeedbackService::for_trusted_backend_with_clock(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        server.state.0.bank.clone(),
        server.state.0.runtime.runtime(),
        ConfirmationConfig::new(Duration::from_secs(10)).unwrap(),
        PermitConfig::new(Duration::from_secs(10)).unwrap(),
        clock.clone(),
    )
    .unwrap();
    let s = session(&cookie);
    let realm = cfg.realm();
    let n = nonce();
    let p = f.prepare_v2(&s, &cfg, &realm, &n, i.clone()).await.unwrap();
    let (t, o, r) = targets(&p);
    clock.0.store(now - 1, Ordering::SeqCst);
    assert!(f
        .confirm_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
        .await
        .is_err());
    assert!(f
        .abandon_preparation_v2(&s, &cfg, &realm, &n)
        .await
        .is_err());
    clock.0.store(now + 10_000, Ordering::SeqCst);
    assert!(matches!(
        f.confirm_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
            .await,
        Err(StorageError::ConfirmationExpired)
    ));
    let before = f.read_preparation_v2(&s, &cfg, &realm, &n).await.unwrap();
    assert_eq!(before["confirmation_observation"]["status"], "EXPIRED");
    assert_eq!(
        f.prepare_v2(&s, &cfg, &realm, &n, i).await.unwrap()["kind"],
        "RECOVERY_ONLY"
    );
    assert_eq!(
        f.read_preparation_v2(&s, &cfg, &realm, &n).await.unwrap()["confirmation_observation"]
            ["original_expires_at"],
        p["confirmation"]["expires_at"]
    );
    db.repo
        .set_billing_recovery_enabled_for_reviewed_backend(&cfg, false)
        .await
        .unwrap();
    assert_eq!(
        f.recovery_context_v2(&s, &cfg).await.unwrap()["available"],
        false
    );
    assert!(f
        .abandon_preparation_v2(&s, &cfg, &realm, &n)
        .await
        .is_err());
    let alternate = config(
        &db,
        &p["parameters"]["transaction"]["source_id"]
            .as_str()
            .unwrap(),
    );
    assert!(db
        .repo
        .set_billing_recovery_enabled_for_reviewed_backend(&alternate, true)
        .await
        .is_err());
    db.repo
        .set_billing_recovery_enabled_for_reviewed_backend(&cfg, true)
        .await
        .unwrap();
    f.abandon_preparation_v2(&s, &cfg, &realm, &n)
        .await
        .unwrap();
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL cross-owner actual nonce races"]
async fn billing_recovery_v2_cross_owner_nonce_close_prepare_race() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let other = billing_server_with_feedback(
        &db,
        &provider,
        "other_owner",
        fixture::bank(),
        None,
        false,
        true,
    )
    .await;
    let oc = other.login().await;
    let s = session(&cookie);
    let os = session(&oc);
    let f = service(&server);
    let of = service(&other);
    let realm = cfg.realm();
    for _ in 0..6 {
        let n = nonce();
        let (a, b) = tokio::join!(
            f.prepare_v2(&s, &cfg, &realm, &n, i.clone()),
            of.abandon_preparation_v2(&os, &cfg, &realm, &n)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_prepare_requests_v2 WHERE nonce=$1::uuid",
        )
        .bind(n.as_string())
        .fetch_one(&db.scoped)
        .await
        .unwrap();
        assert_eq!(rows, 1);
        if a.is_ok() {
            f.abandon_preparation_v2(&s, &cfg, &realm, &n)
                .await
                .unwrap();
        } else {
            assert!(f.read_preparation_v2(&s, &cfg, &realm, &n).await.is_err());
        }
    }
    other.stop();
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL deterministic opposite lock orders"]
async fn billing_recovery_v2_confirmation_close_and_close_execute_opposite_races() {
    for mode in ["confirm-first", "close-confirm", "close-execute"] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        let i = intent(&complete["results"][0]["data"]["report"]);
        let cfg = config(&db, &i.transaction.source_id);
        db.repo
            .install_billing_recovery_for_reviewed_backend(&cfg)
            .await
            .unwrap();
        let s = session(&cookie);
        let n = nonce();
        let p = service(&server)
            .prepare_v2(&s, &cfg, &cfg.realm(), &n, i)
            .await
            .unwrap();
        let (t, o, r) = targets(&p);
        if mode == "close-execute" {
            service(&server)
                .confirm_preparation_v2(&s, &cfg, &cfg.realm(), &n, &t, &o, &r)
                .await
                .unwrap();
        }
        db.scoped.execute("CREATE FUNCTION recovery_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
        let table = if mode == "confirm-first" {
            "server_confirmations"
        } else {
            "billing_prepare_requests_v2"
        };
        db.scoped.execute(format!("CREATE TRIGGER recovery_wait BEFORE UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION recovery_wait()").as_str()).await.unwrap();
        let mut blocker = db.scoped.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(8021021)")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let state = server.state.clone();
        let c = cfg.clone();
        let ss = s.clone();
        let nn = n.clone();
        let tt = t.clone();
        let oo = o.clone();
        let rr = r.clone();
        let first = tokio::spawn(async move {
            if mode == "confirm-first" {
                service_state(&state)
                    .confirm_preparation_v2(&ss, &c, &c.realm(), &nn, &tt, &oo, &rr)
                    .await
            } else {
                service_state(&state)
                    .abandon_preparation_v2(&ss, &c, &c.realm(), &nn)
                    .await
            }
        });
        wait_for_query(&db, &format!("UPDATE {table}%")).await;
        let state = server.state.clone();
        let c = cfg.clone();
        let ss = s.clone();
        let nn = n.clone();
        let tt = t.clone();
        let oo = o.clone();
        let rr = r.clone();
        let second = tokio::spawn(async move {
            match mode {
                "confirm-first" => {
                    service_state(&state)
                        .abandon_preparation_v2(&ss, &c, &c.realm(), &nn)
                        .await
                }
                "close-confirm" => {
                    service_state(&state)
                        .confirm_preparation_v2(&ss, &c, &c.realm(), &nn, &tt, &oo, &rr)
                        .await
                }
                _ => {
                    service_state(&state)
                        .execute_preparation_v2(&ss, &c, &c.realm(), &nn, &tt, &oo, &rr)
                        .await
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!second.is_finished());
        sqlx::query("SELECT pg_advisory_unlock(8021021)")
            .execute(&mut *blocker)
            .await
            .unwrap();
        drop(blocker);
        first.await.unwrap().unwrap();
        let result = second.await.unwrap();
        assert_eq!(result.is_ok(), mode == "confirm-first");
        assert_eq!(
            service(&server)
                .read_preparation_v2(&s, &cfg, &cfg.realm(), &n)
                .await
                .unwrap()["state"],
            "ABANDONED"
        );
        assert_eq!(count(&db, "billing_feedback_v1").await, 0);
        server.stop();
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL real pre-cutover legacy operation and generic writers"]
async fn billing_recovery_v2_legacy_original_retained_generic_confirmation_and_runtime_fail_closed()
{
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let s = session(&cookie);
    let f = service(&server);
    let legacy = f.prepare(&s, i.clone()).await.unwrap();
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let before = snapshot(&db).await;
    let generic = safeflow_storage::ConfirmationService::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),
    );
    assert!(generic
        .prepare(&s, &legacy.task.task_id, &legacy.operation.operation_id)
        .await
        .is_err());
    assert!(generic
        .record_explicit_confirmation(
            &s,
            &legacy.task.task_id,
            &legacy.confirmation.confirmation_ref,
            &legacy.confirmation.binding
        )
        .await
        .is_err());
    assert!(f
        .execute(
            &s,
            &legacy.task.task_id,
            &legacy.operation.operation_id,
            &legacy.confirmation.confirmation_ref
        )
        .await
        .is_err());
    assert!(matches!(
        f.read_operation_preparation_v2(&s, &cfg, &cfg.realm(), &legacy.operation.operation_id)
            .await,
        Err(StorageError::BillingRecovery("LEGACY_UNFENCED"))
    ));
    let response = command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{}/execute", legacy.task.task_id),
        json!({"task_revision":legacy.task.revision}),
    )
    .await;
    assert!(!response.status.is_success());
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(count(&db, "billing_prepare_requests_v2").await, 0);
    assert_ready(&db, &legacy.operation.operation_id).await;
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL real current complete account scope"]
async fn billing_recovery_v2_all_accounts_revoked_after_stage_redacts_and_never_saves() {
    use safeflow_billing_source_reader::query::PagedSource;
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    provider.arguments.lock().unwrap()["account_ids_json"] =
        json!("[\"account_001\",\"account_002\"]");
    let source = fixture::replay("mixed");
    let access = Arc::new(AccountAccess {
        denied: std::sync::atomic::AtomicBool::new(false),
    });
    let cfg = BillingRecoveryConfig::new(
        &uuid::Uuid::new_v4().to_string(),
        &db.namespace,
        source.source_id(),
        "8ff8c665:recovery-scope",
        "owned-scope",
        8,
        32,
        1000,
        2000,
        1000,
        2000,
        true,
    )
    .unwrap();
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let server = billing_server_with_recovery(
        &db,
        &provider,
        "user_demo",
        access.clone(),
        Some(fixture::approved(
            access.clone(),
            Arc::new(AccountSource(source)),
            "8ff8c665:recovery-scope",
        )),
        false,
        true,
        None,
        Some(cfg.clone()),
    )
    .await;
    let cookie = server.login().await;
    let v = initial(&server, &cookie, "report").await;
    let complete = ok(execute(&server, &cookie, &v).await);
    let i = intent(&complete["results"][0]["data"]["report"]);
    let f = service(&server);
    let s = session(&cookie);
    let realm = cfg.realm();
    let n = nonce();
    let p = f.prepare_v2(&s, &cfg, &realm, &n, i).await.unwrap();
    let (t, o, r) = targets(&p);
    f.confirm_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
        .await
        .unwrap();
    let mut blocker = install_wait(&db).await;
    let mut running = Box::pin(f.execute_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r));
    tokio::select! {result=&mut running=>panic!("unexpected {:?}",result.err()),_=wait_for_stage(&db)=>{}}
    access.denied.store(true, Ordering::SeqCst);
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    assert!(running.await.is_err());
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    let redacted = f.read_preparation_v2(&s, &cfg, &realm, &n).await.unwrap();
    assert!(
        redacted["parameters"].is_null()
            && redacted["operation_id"].is_null()
            && redacted["task_id"].is_null()
    );
    assert!(matches!(
        f.read_operation_preparation_v2(&s, &cfg, &realm, &o).await,
        Err(StorageError::NotFound)
    ));
    assert_eq!(
        f.abandon_preparation_v2(&s, &cfg, &realm, &n)
            .await
            .unwrap()["state"],
        "ABANDONED"
    );
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL real single-transaction fault injection"]
async fn billing_recovery_v2_prepare_all_write_points_rollback() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let f = service(&server);
    let s = session(&cookie);
    let realm = cfg.realm();
    db.scoped.execute("CREATE FUNCTION recovery_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'candidate atomicity fault'; END $$").await.unwrap();
    for (table, event, condition) in [
        ("tasks", "INSERT", ""),
        ("operations", "INSERT", ""),
        ("tasks", "UPDATE", "WHEN (NEW.revision=3)"),
        ("operations", "UPDATE", "WHEN (NEW.state='READY')"),
        ("server_confirmations", "INSERT", ""),
        (
            "audit_events",
            "INSERT",
            "WHEN (NEW.kind='CONFIRMATION_PREPARED')",
        ),
        ("public_audit_outbox", "INSERT", ""),
        ("billing_prepare_requests_v2", "INSERT", ""),
    ] {
        let before = snapshot(&db).await;
        let tasks = count(&db, "tasks").await;
        let n = nonce();
        db.scoped.execute(format!("CREATE TRIGGER recovery_fault BEFORE {event} ON {table} FOR EACH ROW {condition} EXECUTE FUNCTION recovery_fault()").as_str()).await.unwrap();
        assert!(
            f.prepare_v2(&s, &cfg, &realm, &n, i.clone()).await.is_err(),
            "{table}/{event}/{condition}"
        );
        assert_eq!(snapshot(&db).await, before);
        assert_eq!(count(&db, "tasks").await, tasks);
        assert_eq!(count(&db, "billing_prepare_requests_v2").await, 0);
        db.scoped
            .execute(format!("DROP TRIGGER recovery_fault ON {table}").as_str())
            .await
            .unwrap();
    }
    f.prepare_v2(&s, &cfg, &realm, &nonce(), i).await.unwrap();
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL explicit rollback positive proof"]
async fn billing_recovery_v2_business_faults_require_explicit_rollback_then_abandon() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let i = intent(&complete["results"][0]["data"]["report"]);
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let f = service(&server);
    let s = session(&cookie);
    let realm = cfg.realm();
    db.scoped.execute("CREATE FUNCTION recovery_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'candidate business fault'; END $$").await.unwrap();
    for (table, condition) in [
        ("billing_feedback_events_v1", ""),
        ("public_billing_feedback_results_v1", ""),
        (
            "public_audit_outbox",
            "WHEN (NEW.envelope->'public_event'->'details'->>'source'='billing-feedback-local-v1')",
        ),
    ] {
        let n = nonce();
        let p = f.prepare_v2(&s, &cfg, &realm, &n, i.clone()).await.unwrap();
        let (t, o, r) = targets(&p);
        f.confirm_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
            .await
            .unwrap();
        let before = snapshot(&db).await;
        db.scoped.execute(format!("CREATE TRIGGER recovery_fault BEFORE INSERT ON {table} FOR EACH ROW {condition} EXECUTE FUNCTION recovery_fault()").as_str()).await.unwrap();
        assert!(f
            .execute_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
            .await
            .is_err());
        assert_eq!(snapshot(&db).await, before);
        assert_eq!(
            f.read_preparation_v2(&s, &cfg, &realm, &n).await.unwrap()["state"],
            "CLOSED_ROLLBACK"
        );
        assert!(f
            .execute_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
            .await
            .is_err());
        db.scoped
            .execute(format!("DROP TRIGGER recovery_fault ON {table}").as_str())
            .await
            .unwrap();
        f.abandon_preparation_v2(&s, &cfg, &realm, &n)
            .await
            .unwrap();
    }
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "owned isolated PostgreSQL real admission/abandon and coroutine crash"]
async fn billing_recovery_v2_admitted_crash_and_cancel_races_never_reexecute() {
    for crash in [false, true] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        let i = intent(&complete["results"][0]["data"]["report"]);
        let cfg = config(&db, &i.transaction.source_id);
        db.repo
            .install_billing_recovery_for_reviewed_backend(&cfg)
            .await
            .unwrap();
        let s = session(&cookie);
        let realm = cfg.realm();
        let n = nonce();
        let p = service(&server)
            .prepare_v2(&s, &cfg, &realm, &n, i)
            .await
            .unwrap();
        let (t, o, r) = targets(&p);
        service(&server)
            .confirm_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
            .await
            .unwrap();
        let mut blocker = install_wait(&db).await;
        let state = server.state.clone();
        let cc = cfg.clone();
        let nn = n.clone();
        let ss = s.clone();
        let tt = t.clone();
        let oo = o.clone();
        let rr = r.clone();
        let pending = tokio::spawn(async move {
            service_state(&state)
                .execute_preparation_v2(&ss, &cc, &cc.realm(), &nn, &tt, &oo, &rr)
                .await
        });
        wait_for_stage(&db).await;
        assert_eq!(
            service(&server)
                .read_preparation_v2(&s, &cfg, &realm, &n)
                .await
                .unwrap()["state"],
            "EXECUTION_ADMITTED"
        );
        let waiting_state = server.state.clone();
        let waiting_cfg = cfg.clone();
        let waiting_session = s.clone();
        let waiting_nonce = n.clone();
        let closing = tokio::spawn(async move {
            service_state(&waiting_state)
                .abandon_preparation_v2(
                    &waiting_session,
                    &waiting_cfg,
                    &waiting_cfg.realm(),
                    &waiting_nonce,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !closing.is_finished(),
            "close must wait for the active owner transaction"
        );
        if crash {
            pending.abort();
        }
        sqlx::query("SELECT pg_advisory_unlock(8021021)")
            .execute(&mut *blocker)
            .await
            .unwrap();
        drop(blocker);
        if !crash {
            assert_eq!(pending.await.unwrap().unwrap()["state"], "SAVED");
            assert_eq!(count(&db, "billing_feedback_v1").await, 1);
        } else {
            assert!(pending.await.unwrap_err().is_cancelled());
            assert_eq!(
                service(&server)
                    .read_preparation_v2(&s, &cfg, &realm, &n)
                    .await
                    .unwrap()["state"],
                "EXECUTION_ADMITTED"
            );
            assert_eq!(count(&db, "billing_feedback_v1").await, 0);
            assert!(service(&server)
                .abandon_preparation_v2(&s, &cfg, &realm, &n)
                .await
                .is_err());
        }
        assert!(closing.await.unwrap().is_err());
        assert!(service(&server)
            .execute_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r)
            .await
            .is_err());
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "explicit task-owned browser host; separate opt-in run"]
async fn billing_recovery_browser_host() {
    let root = std::path::PathBuf::from(std::env::var("SAFEFLOW_RECOVERY_BROWSER_ROOT").unwrap());
    assert!(
        root.is_absolute()
            && !root.join("browser-state.json").exists()
            && !root.join("stop-browser").exists()
    );
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, cookie, complete, cfg) = v2_report(&db, &provider).await;
    std::fs::write(root.join("browser-state.json"),serde_json::to_vec_pretty(&json!({"api":format!("http://{}",server.address),"report_task_id":complete["task"]["task_id"],"realm":cfg.realm(),"namespace":db.namespace,"fixture":"unchanged Jonathan mixed / 8ff8c665:mixed"})).unwrap()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while !root.join("stop-browser").exists() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let roots: Vec<(String, String, i16)> = sqlx::query_as(
        "SELECT nonce::text,state,attempt_count FROM billing_prepare_requests_v2 ORDER BY nonce",
    )
    .fetch_all(&db.scoped)
    .await
    .unwrap();
    let saved: i64 = count(&db, "billing_feedback_v1").await;
    std::fs::write(root.join("browser-db-verification.json"),serde_json::to_vec_pretty(&json!({"roots":roots,"saved":saved,"attempts":count(&db,"billing_prepare_attempts_v2").await,"real_http":true,"real_pg":true})).unwrap()).unwrap();
    let _ = cookie;
    server.stop();
    db.finish().await;
    std::fs::write(
        root.join("browser-cleanup.json"),
        b"{\"owned_schema_dropped\":true}",
    )
    .unwrap();
}

fn restart_root() -> std::path::PathBuf {
    let path = std::path::PathBuf::from(std::env::var("SAFEFLOW_RECOVERY_RESTART_ROOT").unwrap());
    assert!(path.starts_with("C:/SafeFlowDevRuns/billing-prepare-recovery-candidate-20261024"));
    path
}
#[tokio::test]
#[ignore = "owned PG; phase one of an actual test-process and PostgreSQL restart"]
async fn billing_recovery_restart_seed() {
    let root = restart_root();
    assert!(!root.join("restart-seed.json").exists());
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    assert!(original["evidence"].as_array().unwrap().len() > 1);
    let i = intent(original);
    let cfg = config(&db, &i.transaction.source_id);
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let s = session(&cookie);
    let open = nonce();
    let mut other = i.clone();
    other.transaction =
        serde_json::from_value(original["evidence"][1]["reference"].clone()).unwrap();
    service(&server)
        .prepare_v2(&s, &cfg, &cfg.realm(), &open, other)
        .await
        .unwrap();
    let admitted = nonce();
    let p = service(&server)
        .prepare_v2(&s, &cfg, &cfg.realm(), &admitted, i.clone())
        .await
        .unwrap();
    let (t, o, r) = targets(&p);
    service(&server)
        .confirm_preparation_v2(&s, &cfg, &cfg.realm(), &admitted, &t, &o, &r)
        .await
        .unwrap();
    let mut blocker = install_wait(&db).await;
    let state = server.state.clone();
    let cc = cfg.clone();
    let nn = admitted.clone();
    let ss = s.clone();
    let pending = tokio::spawn(async move {
        service_state(&state)
            .execute_preparation_v2(&ss, &cc, &cc.realm(), &nn, &t, &o, &r)
            .await
    });
    wait_for_stage(&db).await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    let closed = nonce();
    service(&server)
        .abandon_preparation_v2(&s, &cfg, &cfg.realm(), &closed)
        .await
        .unwrap();
    let states = [
        service(&server)
            .read_preparation_v2(&s, &cfg, &cfg.realm(), &open)
            .await
            .unwrap(),
        service(&server)
            .read_preparation_v2(&s, &cfg, &cfg.realm(), &admitted)
            .await
            .unwrap(),
        service(&server)
            .read_preparation_v2(&s, &cfg, &cfg.realm(), &closed)
            .await
            .unwrap(),
    ];
    assert_eq!(states[1]["state"], "EXECUTION_ADMITTED");
    std::fs::write(root.join("restart-seed.json"),serde_json::to_vec_pretty(&json!({"pid":std::process::id(),"namespace":db.namespace,"url":db.url,"realm":cfg.realm(),"source":i.transaction.source_id,"states":states,"open":open.as_string(),"admitted":admitted.as_string(),"closed":closed.as_string(),"rate_count":count(&db,"billing_prepare_rate_v2").await})).unwrap()).unwrap();
    server.stop();
    db.repo.close().await;
    db.scoped.close().await;
    db.admin.close().await;
    // The next, separate process owns cleanup after verifying the same namespace.
}
#[tokio::test]
#[ignore = "owned PG; phase two after verified task-owned PG restart"]
async fn billing_recovery_restart_verify() {
    let root = restart_root();
    let seed: Value =
        serde_json::from_slice(&std::fs::read(root.join("restart-seed.json")).unwrap()).unwrap();
    assert_ne!(seed["pid"], json!(std::process::id()));
    let namespace = seed["namespace"].as_str().unwrap().to_owned();
    assert!(namespace.starts_with("sf_http_test_") && namespace.len() == 45);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    assert_eq!(seed["url"], url);
    let repo = PgRepository::connect(&url, &namespace).await.unwrap();
    repo.migrate().await.unwrap();
    let path = namespace.clone();
    let scoped = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |connection, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(path)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let db = Db {
        namespace,
        url,
        repo,
        scoped,
        admin,
    };
    let cfg = BillingRecoveryConfig::new(
        seed["realm"].as_str().unwrap(),
        &db.namespace,
        seed["source"].as_str().unwrap(),
        "8ff8c665:mixed",
        "isolated-candidate-continuity-1",
        8,
        32,
        1000,
        2000,
        1000,
        2000,
        true,
    )
    .unwrap();
    db.repo
        .install_billing_recovery_for_reviewed_backend(&cfg)
        .await
        .unwrap();
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let server = billing_server_with_recovery(
        &db,
        &provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source, "8ff8c665:mixed")),
        false,
        true,
        None,
        Some(cfg.clone()),
    )
    .await;
    let cookie = server.login().await;
    let s = session(&cookie);
    let f = service(&server);
    let persisted_rate = count(&db, "billing_prepare_rate_v2").await;
    assert!(persisted_rate >= seed["rate_count"].as_i64().unwrap());
    let mut results = vec![];
    for key in ["open", "admitted", "closed"] {
        let nonce = BillingRecoveryNonce::parse(seed[key].as_str().unwrap()).unwrap();
        let v = f
            .read_preparation_v2(&s, &cfg, &cfg.realm(), &nonce)
            .await
            .unwrap();
        assert!(!v.to_string().contains("confirmation_ref"));
        results.push(v);
    }
    assert_eq!(results[0]["state"], "PREPARED_OPEN");
    assert_eq!(results[1]["state"], "EXECUTION_ADMITTED");
    assert_eq!(results[2]["state"], "ABANDONED");
    assert_eq!(
        results[2]["fence_revision"],
        seed["states"][2]["fence_revision"]
    );
    let admitted = BillingRecoveryNonce::parse(seed["admitted"].as_str().unwrap()).unwrap();
    assert!(matches!(
        f.abandon_preparation_v2(&s, &cfg, &cfg.realm(), &admitted)
            .await,
        Err(StorageError::BillingRecovery("RECONCILE_REQUIRED"))
    ));
    let open = BillingRecoveryNonce::parse(seed["open"].as_str().unwrap()).unwrap();
    f.abandon_preparation_v2(&s, &cfg, &cfg.realm(), &open)
        .await
        .unwrap();
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    std::fs::write(root.join("restart-verification.json"),serde_json::to_vec_pretty(&json!({"seed_pid":seed["pid"],"verification_pid":std::process::id(),"same_namespace":db.namespace,"same_realm":cfg.realm(),"after_restart":results,"no_ref_recovered":true,"admitted_not_closed":true,"no_member_write":true,"rate_records_before_new_requests":persisted_rate})).unwrap()).unwrap();
    server.stop();
    db.finish().await;
}
