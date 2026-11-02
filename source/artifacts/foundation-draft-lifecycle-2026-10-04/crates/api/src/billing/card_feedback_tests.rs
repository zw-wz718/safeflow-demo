//! Public original-feedback integration. Uses the unchanged fictional member
//! billing fixture, real public save/completion, and isolated card SQL only.
use super::*;
use safeflow_card_facts::{
    operation_records::StoredReasonCode, operation_replay::LockReplayParameters, LockSource,
};
use safeflow_card_postgres_store::{
    freeze::{FreezeCommand, FreezeRecordIds},
    LockedCardAccount,
};
use safeflow_contracts::{ContractFuture, OperationId, ResourceId};
use safeflow_runtime::SessionBinding;
use safeflow_storage::{
    BackendCardCatalog, BackendCardCatalogProvider, BackendCatalogCard, CardCatalogService,
    CardCatalogSourceBinding, CardReadService,
};
use std::sync::atomic::AtomicBool;

// Reuse the already reviewed member adapter's fictional transaction, not a
// modified basic_spending JSON (that original sample has no card). Source:
// c3688e5 billing-feedback-adapter/tests/support/mod.rs:93,236,295.
struct MemberAssociationSource {
    card: Option<&'static str>,
}
impl safeflow_billing_source_reader::query::PagedSource for MemberAssociationSource {
    fn source_id(&self) -> &str {
        "source_private_one"
    }
    fn read_page(
        &self,
        _: &safeflow_billing_source_reader::TrustedScope,
        request: &safeflow_billing_source_reader::query::Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<
        safeflow_billing_source_reader::query::Page,
        safeflow_billing_source_reader::query::PageError,
    > {
        Ok(safeflow_billing_source_reader::query::Page {
            snapshot_id:"original_snapshot_feedback_original_PRIVATE".into(),owner_user_id:"user_demo".into(),
            account_ids:request.account_ids.clone(),window:request.window.clone(),total_unique_rows:Some(1),
            items:vec![serde_json::from_value(json!({"transaction_id":"feedback_original_PRIVATE","account_id":"account_001",
                "operation_id":"original_purchase_operation_PRIVATE","amount":"30.00","currency":"CNY",
                "direction":"OUT","transaction_type":"CONSUMPTION","status":"COMPLETED",
                "completed_at":"2026-10-10T00:00:00Z","merchant_id":"fictional_merchant","card_id":self.card})).unwrap()],
            has_more:false,next_cursor:None,range_complete:true,
        })
    }
}

struct Catalog {
    revision: AtomicUsize,
    observation_calls: AtomicUsize,
    pause_observation: AtomicUsize,
    block: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl Catalog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            revision: AtomicUsize::new(1),
            observation_calls: AtomicUsize::new(0),
            pause_observation: AtomicUsize::new(0),
            block: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
}
impl BackendCardCatalogProvider for Catalog {
    fn authorized_catalog<'a>(
        &'a self,
        i: &'a SessionBinding,
        a: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        Box::pin(async move {
            if i.user_id.as_str() != "user_demo" || a.as_str() != "account_001" {
                return Ok(None);
            }
            Ok(Some(BackendCardCatalog {
                owner: i.user_id.clone(),
                account: a.clone(),
                source_revision: format!("isolated:{}", self.revision.load(Ordering::SeqCst)),
                cards: vec![BackendCatalogCard {
                    card_id: "card_target_PRIVATE".into(),
                    last_four: "1234".into(),
                }],
            }))
        })
    }
    fn current_revision<'a>(
        &'a self,
        _: &'a SessionBinding,
        _: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            let call = self.observation_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.block.swap(false, Ordering::SeqCst)
                || self.pause_observation.load(Ordering::SeqCst) == call
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(format!("isolated:{}", self.revision.load(Ordering::SeqCst)))
        })
    }
}
fn binding(catalog: Arc<Catalog>) -> CardCatalogSourceBinding {
    CardCatalogSourceBinding::for_reviewed_backend_provider(
        "isolated-feedback-cards",
        "isolated-public-r1",
        catalog,
    )
    .unwrap()
}
fn nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}
async fn setup(
    db: &Db,
) -> (
    Server,
    Arc<Catalog>,
    Arc<AccountAccess>,
    String,
    OperationId,
) {
    setup_case(db, FeedbackChoice::NotMine, Some("card_target_PRIVATE")).await
}
async fn setup_case(
    db: &Db,
    choice: FeedbackChoice,
    card: Option<&'static str>,
) -> (
    Server,
    Arc<Catalog>,
    Arc<AccountAccess>,
    String,
    OperationId,
) {
    let provider = BillingProvider::new("mixed").await;
    provider.arguments.lock().unwrap()["account_ids_json"] =
        json!("[\"account_001\",\"account_002\"]");
    let access = Arc::new(AccountAccess {
        denied: AtomicBool::new(false),
    });
    let catalog = Catalog::new();
    let card_source = binding(catalog.clone());
    let server = billing_server_with_feedback_and_cards(
        db,
        &provider,
        "user_demo",
        access.clone(),
        Some(fixture::approved(
            access.clone(),
            Arc::new(MemberAssociationSource { card }),
            "c3688e5:public-mechanism-only",
        )),
        false,
        true,
        Some(card_source.clone()),
    )
    .await;
    let cookie = server.login().await;
    let identity = session(&cookie);
    let plan = initial(&server, &cookie, "report").await;
    let complete = ok(execute(&server, &cookie, &plan).await);
    let report = &complete["results"][0]["data"]["report"];
    assert_eq!(report["evidence"][0]["transaction"]["card_id"], json!(card));
    let feedback = service(&server);
    let mut input = intent(report);
    input.choice = choice;
    let prepared = feedback.prepare(&identity, input).await.unwrap();
    feedback.confirm(&identity, &prepared).await.unwrap();
    feedback
        .execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    let mut tx = db.scoped.begin().await.unwrap();
    safeflow_transfer_integration::funds_store::FundsStore::provision_account(
        &mut tx,
        &UserId::parse("user_demo").unwrap(),
        &ResourceId::parse("account_001").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("100.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    CardCatalogService::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        Some(card_source),
    )
    .initialize(&identity, &ResourceId::parse("account_001").unwrap())
    .await
    .unwrap();
    let command = FreezeCommand {
        operation_id: "OP-public-feedback-history".into(),
        canonical_key: "KEY-public-feedback-history".into(),
        event_scope: None,
        ids: FreezeRecordIds {
            lock_id: "L-public-feedback-history".into(),
            operation_record_id: "R-public-feedback-history".into(),
            reason_history_id: "H-public-feedback-history".into(),
        },
        parameters: LockReplayParameters {
            card_id: "card_target_PRIVATE".into(),
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
    LockedCardAccount::acquire(&mut tx, "user_demo", "account_001")
        .await
        .unwrap()
        .save_freeze(&command)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(feedback);
    drop(provider);
    (
        server,
        catalog,
        access,
        cookie,
        prepared.operation.operation_id,
    )
}
async fn query_feedback(server: &Server, cookie: &str, op: &OperationId) -> Reply {
    command(
        server,
        cookie,
        "/api/card-read",
        json!({"kind":"QUERY_FEEDBACK","feedback_operation_id":op,"request_ref":nonce()}),
    )
    .await
}
async fn select(server: &Server, cookie: &str, view: &Value, position: usize) -> Reply {
    command(server,cookie,"/api/card-read",json!({"kind":"SELECT_RECORD","step_ref":view["step_ref"],"position":position,"request_ref":nonce()})).await
}
async fn all_business_rows(db: &Db) -> Value {
    // Backend observation uses the existing advisory owner boundary, not writes.
    let tables:Vec<String>=sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname=current_schema() AND tablename <> '_sqlx_migrations' ORDER BY tablename").fetch_all(&db.scoped).await.unwrap();
    let mut out = serde_json::Map::new();
    for table in tables {
        assert!(table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let rows:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(j ORDER BY j::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) j FROM \"{table}\" t) s")).fetch_one(&db.scoped).await.unwrap();
        out.insert(table, rows);
    }
    Value::Object(out)
}
#[tokio::test]
#[ignore = "isolated PG and actual loopback HTTP"]
async fn public_card_feedback_http_complete_private_projection_and_zero_writes() {
    let db = Db::new().await;
    let (server, _, _, cookie, op) = setup(&db).await;
    let before = all_business_rows(&db).await;
    let view = ok(query_feedback(&server, &cookie, &op).await);
    assert_eq!(view["cards"], json!([]));
    assert_eq!(view["feedback"]["state"], "CARD_ASSOCIATION_VERIFIED");
    assert!(view["original"].is_null());
    assert_eq!(
        view["history"]["locks"][0]["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let result = ok(select(&server, &cookie, &view, 1).await);
    assert_eq!(result["original"]["state"], "RECORDED_ACTIVE");
    assert!(result["original"]["original_action"].is_string());
    let text = result.to_string();
    for secret in [
        "card_target_PRIVATE",
        "user_demo",
        "account_001",
        "OP-public-feedback-history",
        "L-public-feedback-history",
        op.as_str(),
    ] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    assert_eq!(all_business_rows(&db).await, before);
    if let Ok(dir) = std::env::var("SAFEFLOW_FEEDBACK_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&dir).join("public-feedback-http.json"),
            serde_json::to_vec_pretty(
                &json!({"query":view,"selected":result,"all_business_rows_unchanged":true}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PG; old steps, cancellation, ownership and exact repository"]
async fn public_card_feedback_old_tickets_cancel_cross_path_and_namespace_rejected() {
    let db = Db::new().await;
    let (server, _, _, cookie, op) = setup(&db).await;
    let view = ok(query_feedback(&server, &cookie, &op).await);
    assert!(!command(
        &server,
        &cookie,
        "/api/card-read",
        json!({"kind":"SELECT_CARD","step_ref":view["step_ref"],"position":1,"request_ref":nonce()})
    )
    .await
    .status
    .is_success());
    assert!(!select(&server, &cookie, &view, 1).await.status.is_success());
    let view = ok(query_feedback(&server, &cookie, &op).await);
    let direct = ok(command(
        &server,
        &cookie,
        "/api/card-read",
        json!({"kind":"QUERY","account_id":"account_001","request_ref":nonce()}),
    )
    .await);
    assert!(!select(&server, &cookie, &view, 1).await.status.is_success());
    let feedback_view = ok(query_feedback(&server, &cookie, &op).await);
    assert!(!command(&server,&cookie,"/api/card-read",json!({"kind":"SELECT_CARD","step_ref":direct["step_ref"],"position":1,"request_ref":nonce()})).await.status.is_success());
    ok(command(
        &server,
        &cookie,
        "/api/card-read",
        json!({"kind":"CANCEL","step_ref":feedback_view["step_ref"]}),
    )
    .await);
    assert!(!select(&server, &cookie, &feedback_view, 1)
        .await
        .status
        .is_success());
    let id = session(&cookie);
    let nav = server.state.card_read_for_backend();
    let feedback = service(&server);
    let cancelled = nonce();
    nav.cancel_request(&id, &cancelled).unwrap();
    assert!(nav
        .query_feedback(&id, &feedback, &op, &cancelled)
        .await
        .is_err());
    let other = PgRepository::connect_existing(&db.url, &db.namespace)
        .await
        .unwrap();
    let foreign =
        CardReadService::for_trusted_backend(other.clone(), server.state.0.sessions.clone(), None);
    assert!(matches!(
        foreign.query_feedback(&id, &feedback, &op, &nonce()).await,
        Err(StorageError::FactsConflict)
    ));
    let foreign_issuer = CardReadService::for_trusted_backend(
        db.repo.clone(),
        Arc::new(
            crate::session::SimulationSessions::new(
                Zeroizing::new(SECRET.into()),
                UserId::parse("user_demo").unwrap(),
                Duration::from_secs(30),
            )
            .unwrap(),
        ),
        None,
    );
    assert!(foreign_issuer
        .query_feedback(&id, &feedback, &op, &nonce())
        .await
        .is_err());
    other.close().await;
    drop(feedback);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PG; all original accounts, source revision, missing source"]
async fn public_card_feedback_permissions_source_change_and_missing_provider_fail_closed() {
    let db = Db::new().await;
    let (server, catalog, access, cookie, op) = setup(&db).await;
    let view = ok(query_feedback(&server, &cookie, &op).await);
    access.denied.store(true, Ordering::SeqCst);
    assert!(!select(&server, &cookie, &view, 1).await.status.is_success());
    access.denied.store(false, Ordering::SeqCst);
    let view = ok(query_feedback(&server, &cookie, &op).await);
    catalog.revision.store(2, Ordering::SeqCst);
    assert!(!select(&server, &cookie, &view, 1).await.status.is_success());
    catalog.revision.store(1, Ordering::SeqCst);
    assert!(!select(&server, &cookie, &view, 1).await.status.is_success());
    let id = session(&cookie);
    let service = service(&server);
    let missing = CardReadService::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        None,
    );
    assert!(matches!(
        missing.query_feedback(&id, &service, &op, &nonce()).await,
        Err(StorageError::DependencyUnavailable)
    ));
    drop(service);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PG; every poll guard before provider future resumes"]
async fn public_card_feedback_revoke_cancel_and_new_request_during_actual_wait() {
    for mode in ["revoke", "cancel", "new-query"] {
        let db = Db::new().await;
        let (server, catalog, _, cookie, op) = setup(&db).await;
        let nav = server.state.card_read_for_backend();
        let service = service(&server);
        let id = session(&cookie);
        let request = nonce();
        catalog.block.store(true, Ordering::SeqCst);
        let mut pending = Box::pin(nav.query_feedback(&id, &service, &op, &request));
        tokio::select! {r=&mut pending=>panic!("did not wait {:?}",r.err()),_=catalog.entered.notified()=>{}}
        let new_request = nonce();
        let mut newer = Box::pin(nav.query_feedback(&id, &service, &op, &new_request));
        if mode == "revoke" {
            server.state.0.sessions.revoke(&id).unwrap();
        } else if mode == "cancel" {
            nav.cancel_request(&id, &request).unwrap();
        } else {
            use std::future::Future;
            std::future::poll_fn(|cx| {
                assert!(newer.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }
        catalog.release.notify_one();
        assert!(pending.await.is_err());
        if mode == "new-query" {
            newer.await.unwrap();
        } else {
            drop(newer);
        }
        drop(service);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PG; reviewed member Mine/Unsure/NoCard cases"]
async fn public_card_feedback_three_stop_states_never_offer_history_or_actions() {
    for (choice, card, state) in [
        (
            FeedbackChoice::Mine,
            Some("card_target_PRIVATE"),
            "STOP_MINE",
        ),
        (
            FeedbackChoice::Unsure,
            Some("card_target_PRIVATE"),
            "STOP_UNSURE",
        ),
        (FeedbackChoice::NotMine, None, "STOP_NO_CARD"),
    ] {
        let db = Db::new().await;
        let (server, _, _, cookie, op) = setup_case(&db, choice, card).await;
        let before = all_business_rows(&db).await;
        let v = ok(query_feedback(&server, &cookie, &op).await);
        assert_eq!(v["feedback"]["state"], state);
        assert_eq!(v["cards"], json!([]));
        assert!(v["history"].is_null() && v["original"].is_null());
        assert!(!select(&server, &cookie, &v, 1).await.status.is_success());
        assert_eq!(all_business_rows(&db).await, before);
        server.stop();
        db.finish().await;
    }
}
#[tokio::test]
#[ignore = "isolated PG; offline fact change only, no online release API"]
async fn public_card_feedback_current_inactive_and_missing_original_clear_old_action() {
    let db = Db::new().await;
    let (server, _, _, cookie, op) = setup(&db).await;
    let v = ok(query_feedback(&server, &cookie, &op).await);
    db.scoped
        .execute(
            "UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id='L-public-feedback-history'",
        )
        .await
        .unwrap();
    let after_release = all_business_rows(&db).await;
    let released_reply = select(&server, &cookie, &v, 1).await;
    assert_eq!(
        released_reply.status,
        StatusCode::OK,
        "after offline release: {}",
        released_reply.json
    );
    let selected = ok(released_reply);
    assert_eq!(selected["original"]["state"], "RECORDED_INACTIVE");
    assert!(selected["original"]["original_action"].is_string());
    assert_eq!(selected["history"]["card_locked"], false);
    assert_eq!(all_business_rows(&db).await, after_release);
    db.scoped.execute("ALTER TABLE card_freeze_receipts_v1 DISABLE TRIGGER USER; ALTER TABLE card_freeze_reasons_v1 DISABLE TRIGGER USER; ALTER TABLE card_freeze_operations_v1 DISABLE TRIGGER USER; DELETE FROM card_freeze_receipts_v1; DELETE FROM card_freeze_reasons_v1; DELETE FROM card_freeze_operations_v1; ALTER TABLE card_freeze_receipts_v1 ENABLE TRIGGER USER; ALTER TABLE card_freeze_reasons_v1 ENABLE TRIGGER USER; ALTER TABLE card_freeze_operations_v1 ENABLE TRIGGER USER").await.unwrap();
    let missing = all_business_rows(&db).await;
    let missing_reply = select(&server, &cookie, &selected, 1).await;
    assert_eq!(
        missing_reply.status,
        StatusCode::OK,
        "after original receipt missing: {}",
        missing_reply.json
    );
    let selected = ok(missing_reply);
    assert_eq!(selected["original"]["state"], "NEEDS_REVIEW");
    assert!(selected["original"]["original_action"].is_null());
    assert!(selected["history"]["message"]
        .as_str()
        .unwrap()
        .contains("上次完整清单"));
    assert_eq!(selected["history"]["locks"].as_array().unwrap().len(), 1);
    assert!(!select(&server, &cookie, &selected, 1)
        .await
        .status
        .is_success());
    assert_eq!(all_business_rows(&db).await, missing);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PG; immutable local row tamper and restore, not a live feedback contract"]
async fn public_card_feedback_member_reload_payload_choice_and_aba_rejected() {
    for mode in ["choice", "source", "aba"] {
        let db = Db::new().await;
        let (server, catalog, _, cookie, op) = setup(&db).await;
        let nav = server.state.card_read_for_backend();
        let service = service(&server);
        let id = session(&cookie);
        let request = nonce();
        let original: sqlx::types::Json<Value> =
            sqlx::query_scalar("SELECT payload FROM billing_feedback_v1")
                .fetch_one(&db.scoped)
                .await
                .unwrap();
        catalog.block.store(true, Ordering::SeqCst);
        let mut pending = Box::pin(nav.query_feedback(&id, &service, &op, &request));
        tokio::select! {r=&mut pending=>panic!("did not reach source barrier {:?}",r.err()),_=catalog.entered.notified()=>{}}
        let mut lock = db.scoped.begin().await.unwrap();
        lock.execute("LOCK TABLE billing_feedback_v1 IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        catalog.release.notify_one();
        // Actually poll member load until PG reports a blocked SELECT, rather
        // than claiming a fixed sleep is a member-reload race.
        tokio::select! {r=&mut pending=>panic!("returned before member SELECT barrier {:?}",r.err()),
        observed=tokio::time::timeout(Duration::from_secs(10),async {loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT f.public_operation_id%')").fetch_one(&db.scoped).await.unwrap();
            if waiting {break;} tokio::time::sleep(Duration::from_millis(10)).await;
        }})=>{observed.expect("no actual member reload lock wait observed");} }
        lock.execute("ALTER TABLE billing_feedback_v1 DISABLE TRIGGER USER")
            .await
            .unwrap();
        let mut changed = original.0.clone();
        if mode == "source" {
            changed["reference"]["source_id"] = json!("untrusted_private_source");
        } else {
            changed["choice"] = json!("MINE");
        }
        sqlx::query("UPDATE billing_feedback_v1 SET payload=$1")
            .bind(sqlx::types::Json(changed))
            .execute(&mut *lock)
            .await
            .unwrap();
        if mode == "aba" {
            sqlx::query("UPDATE billing_feedback_v1 SET payload=$1")
                .bind(&original)
                .execute(&mut *lock)
                .await
                .unwrap();
        }
        lock.execute("ALTER TABLE billing_feedback_v1 ENABLE TRIGGER USER")
            .await
            .unwrap();
        lock.commit().await.unwrap();
        assert!(pending.await.is_err(), "accepted {mode}");
        assert!(nav
            .is_feedback_step(&id, "00000000-0000-4000-8000-000000000000")
            .is_err());
        drop(service);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PG; actual HTTP perimeter and malformed/cross-owner refs"]
async fn public_card_feedback_http_forged_fields_csrf_owner_and_sql_fault_rejected() {
    let db = Db::new().await;
    let (server, _, _, cookie, op) = setup(&db).await;
    let input = json!({"kind":"QUERY_FEEDBACK","feedback_operation_id":op,"request_ref":nonce()});
    let before = all_business_rows(&db).await;
    for key in [
        "owner",
        "source_id",
        "card_id",
        "schema",
        "account_id",
        "scope",
        "saved",
    ] {
        let mut forged = input.clone();
        forged[key] = json!("forged");
        assert_eq!(
            command(&server, &cookie, "/api/card-read", forged)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/card-read",
                Some(input.to_string()),
                None,
                None,
                Some("http://127.0.0.1:5174"),
                true
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/card-read",
                Some(input.to_string()),
                Some(&cookie),
                None,
                Some("http://127.0.0.1:5174"),
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
                "/api/card-read",
                Some(input.to_string()),
                Some(&cookie),
                None,
                Some("https://foreign.invalid"),
                true
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(
        !query_feedback(&server, &cookie, &OperationId::parse(nonce()).unwrap())
            .await
            .status
            .is_success()
    );
    let other = Server::new(
        &db,
        "user_002",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    let other_cookie = other.login().await;
    let foreign_session = session(&other_cookie);
    let foreign_feedback = service(&other);
    assert!(foreign_feedback
        .query(&foreign_session, &op)
        .await
        .unwrap()
        .is_none());
    assert!(other
        .state
        .card_read_for_backend()
        .query_feedback(&foreign_session, &foreign_feedback, &op, &nonce())
        .await
        .is_err());
    assert_eq!(all_business_rows(&db).await, before);
    drop(foreign_feedback);
    other.stop();
    db.scoped
        .execute("ALTER TABLE billing_feedback_v1 RENAME TO unavailable_feedback_test")
        .await
        .unwrap();
    assert!(!query_feedback(&server, &cookie, &op)
        .await
        .status
        .is_success());
    db.scoped
        .execute("ALTER TABLE unavailable_feedback_test RENAME TO billing_feedback_v1")
        .await
        .unwrap();
    assert_eq!(all_business_rows(&db).await, before);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PG; corrupt partial receipt is an error, distinct from absent operation"]
async fn public_card_feedback_corrupt_receipt_is_not_needs_review_or_success() {
    let db = Db::new().await;
    let (server, _, _, cookie, op) = setup(&db).await;
    let v = ok(query_feedback(&server, &cookie, &op).await);
    db.scoped.execute("ALTER TABLE card_freeze_receipts_v1 DISABLE TRIGGER USER; DELETE FROM card_freeze_receipts_v1; ALTER TABLE card_freeze_receipts_v1 ENABLE TRIGGER USER").await.unwrap();
    let before = all_business_rows(&db).await;
    assert!(!select(&server, &cookie, &v, 1).await.status.is_success());
    assert_eq!(all_business_rows(&db).await, before);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PG; 64-slot eviction plus session renewal invalidation"]
async fn public_card_feedback_capacity_eviction_and_reissued_session_old_step_rejected() {
    let db = Db::new().await;
    let (server, _, _, cookie, op) = setup(&db).await;
    let nav = server.state.card_read_for_backend();
    let feedback = service(&server);
    let original_id = session(&cookie);
    let oldest = nav
        .query_feedback(&original_id, &feedback, &op, &nonce())
        .await
        .unwrap();
    for _ in 0..64 {
        let c = server.state.0.sessions.issue(SECRET, None).unwrap();
        let id = crate::session::token_reference(Cookie::parse(c).unwrap().value()).unwrap();
        nav.query_feedback(&id, &feedback, &op, &nonce())
            .await
            .unwrap();
    }
    assert!(nav
        .is_feedback_step(&original_id, oldest["step_ref"].as_str().unwrap())
        .is_err());
    let newest = nav
        .query_feedback(&original_id, &feedback, &op, &nonce())
        .await
        .unwrap();
    let cookie = server
        .state
        .0
        .sessions
        .issue(SECRET, Some(&original_id))
        .unwrap();
    let replacement =
        crate::session::token_reference(Cookie::parse(cookie).unwrap().value()).unwrap();
    assert!(nav
        .select_record_with_feedback(
            &original_id,
            &feedback,
            newest["step_ref"].as_str().unwrap(),
            1,
            &nonce()
        )
        .await
        .is_err());
    assert!(nav
        .select_record_with_feedback(
            &replacement,
            &feedback,
            newest["step_ref"].as_str().unwrap(),
            1,
            &nonce()
        )
        .await
        .is_err());
    drop(feedback);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "explicit local browser host; own PG/schema, bounded 180 seconds"]
async fn public_card_feedback_browser_host() {
    let Ok(dir) = std::env::var("SAFEFLOW_FEEDBACK_BROWSER_ROOT") else {
        return;
    };
    let root = std::path::Path::new(&dir);
    assert!(root.is_absolute());
    let db = Db::new().await;
    let (server, _, _, _, op) = setup(&db).await;
    let mut before = all_business_rows(&db).await;
    let state = json!({"address":server.address.to_string(),"feedback_operation_id":op,
        "fixture_only":true,"shared_provider_enabled":false});
    std::fs::write(
        root.join("browser-state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    let mut released = false;
    let mut removed = false;
    tokio::time::timeout(Duration::from_secs(180),async {
        loop {
            if root.join("browser-release.command").exists() && !released {
                assert_eq!(all_business_rows(&db).await,before,"browser wrote business before offline fixture change");
                db.scoped.execute("UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id='L-public-feedback-history'").await.unwrap();
                before=all_business_rows(&db).await;released=true;
                std::fs::write(root.join("browser-release.done"),b"own fixture offline change only").unwrap();
            }
            if root.join("browser-remove.command").exists() && !removed {
                assert_eq!(all_business_rows(&db).await,before);
                db.scoped.execute("ALTER TABLE card_freeze_receipts_v1 DISABLE TRIGGER USER; ALTER TABLE card_freeze_reasons_v1 DISABLE TRIGGER USER; ALTER TABLE card_freeze_operations_v1 DISABLE TRIGGER USER; DELETE FROM card_freeze_receipts_v1; DELETE FROM card_freeze_reasons_v1; DELETE FROM card_freeze_operations_v1; ALTER TABLE card_freeze_receipts_v1 ENABLE TRIGGER USER; ALTER TABLE card_freeze_reasons_v1 ENABLE TRIGGER USER; ALTER TABLE card_freeze_operations_v1 ENABLE TRIGGER USER").await.unwrap();
                before=all_business_rows(&db).await;removed=true;
                std::fs::write(root.join("browser-remove.done"),b"deliberate missing original fixture").unwrap();
            }
            if root.join("browser-finish.command").exists() {break;}
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("browser run timed out");
    assert_eq!(
        all_business_rows(&db).await,
        before,
        "browser changed business tables"
    );
    std::fs::write(root.join("browser-backend-result.json"),serde_json::to_vec_pretty(&json!({"business_rows_unchanged_between_explicit_fixture_controls":true,"released_fixture":released,"removed_fixture":removed})).unwrap()).unwrap();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PG; existing member Created/Reused and other active lock semantics"]
async fn public_card_feedback_reused_action_and_other_lock_still_restricts() {
    for other_lock in [false, true] {
        let db = Db::new().await;
        let (server, _, _, cookie, op) = setup(&db).await;
        let original = ok(query_feedback(&server, &cookie, &op).await);
        if other_lock {
            db.scoped.execute("UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id='L-public-feedback-history'").await.unwrap();
        }
        let command = FreezeCommand {
            operation_id: "OP-second-history".into(),
            canonical_key: "KEY-second-history".into(),
            // The reviewed member fixture uses a separate risk-control source
            // for another lock; a released UserSelf lock must never be reopened.
            event_scope: other_lock.then(|| "prior_provider_PRIVATE".into()),
            ids: FreezeRecordIds {
                lock_id: "L-second-history".into(),
                operation_record_id: "R-second-history".into(),
                reason_history_id: "H-second-history".into(),
            },
            parameters: LockReplayParameters {
                card_id: "card_target_PRIVATE".into(),
                source: if other_lock {
                    LockSource::RiskControl
                } else {
                    LockSource::UserSelf
                },
                reason: if other_lock {
                    StoredReasonCode::RiskControlFlag
                } else {
                    StoredReasonCode::UserSecurityConcern
                },
                event_id: other_lock.then(|| "prior_event_risk_PRIVATE".into()),
                transaction_id: None,
                feedback_id: None,
            },
        };
        let mut tx = db.scoped.begin().await.unwrap();
        sqlx::query("SELECT txid_current()")
            .execute(&mut *tx)
            .await
            .unwrap();
        LockedCardAccount::acquire(&mut tx, "user_demo", "account_001")
            .await
            .unwrap()
            .save_freeze(&command)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let before = all_business_rows(&db).await;
        if other_lock {
            let v = ok(select(&server, &cookie, &original, 1).await);
            assert_eq!(v["original"]["state"], "RECORDED_INACTIVE");
            assert_eq!(v["account_outgoing_restricted"], true);
            assert_eq!(v["original"]["account_outgoing_restricted"], true);
        } else {
            let current = ok(query_feedback(&server, &cookie, &op).await);
            let records = current["history"]["locks"][0]["records"]
                .as_array()
                .unwrap();
            assert_eq!(records.len(), 2);
            let reused = records
                .iter()
                .find(|r| r["original_action"].as_str().unwrap().contains("复用"))
                .unwrap();
            let v = ok(select(
                &server,
                &cookie,
                &current,
                reused["position"].as_u64().unwrap() as usize,
            )
            .await);
            assert_eq!(v["original"]["state"], "RECORDED_ACTIVE");
            assert!(v["original"]["original_action"]
                .as_str()
                .unwrap()
                .contains("复用"));
        }
        assert_eq!(all_business_rows(&db).await, before);
        server.stop();
        db.finish().await;
    }
}

// Regression for the real final provider wait, not a pre-request access toggle.
// The two observation calls bracket the unchanged typed/member read sequence.
async fn final_report_permission_cases(select_record: bool) -> Vec<Value> {
    let mut observations = Vec::new();
    for change in ["unchanged", "revoke-other-account", "revoke-session"] {
        let db = Db::new().await;
        let (server, catalog, access, cookie, operation) = setup(&db).await;
        let previous = if select_record {
            Some(ok(query_feedback(&server, &cookie, &operation).await))
        } else {
            None
        };
        let nav = server.state.card_read_for_backend();
        let feedback = service(&server);
        let identity = session(&cookie);
        let original_calls = catalog.observation_calls.load(Ordering::SeqCst);
        catalog
            .pause_observation
            .store(original_calls + 2, Ordering::SeqCst);
        let before = all_business_rows(&db).await;
        let request = nonce();
        let mut read: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Value, StorageError>> + '_>,
        > = if let Some(previous) = &previous {
            Box::pin(nav.select_record_with_feedback(
                &identity,
                &feedback,
                previous["step_ref"].as_str().unwrap(),
                1,
                &request,
            ))
        } else {
            Box::pin(nav.query_feedback(&identity, &feedback, &operation, &request))
        };
        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::select! {
                result = &mut read => panic!("read did not reach its final provider wait: {:?}", result.err()),
                _ = catalog.entered.notified() => {}
            }
        }).await.expect("final provider observation was not reached");
        assert_eq!(
            catalog.observation_calls.load(Ordering::SeqCst),
            original_calls + 2
        );
        if change == "revoke-other-account" {
            // Existing confirmed test gateway denies account_002 only; target
            // card account_001 and current catalog remain available.
            access.denied.store(true, Ordering::SeqCst);
        } else if change == "revoke-session" {
            server.state.0.sessions.revoke(&identity).unwrap();
        }
        catalog.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(20), read)
            .await
            .unwrap();
        let success = result.is_ok();
        let retained = result.as_ref().ok().is_some_and(|view| {
            nav.is_feedback_step(&identity, view["step_ref"].as_str().unwrap())
                .unwrap_or(false)
        });
        let current_query = feedback.query(&identity, &operation).await;
        let direct_query_denied = match change {
            "revoke-other-account" => matches!(current_query, Err(StorageError::NotFound)),
            "revoke-session" => matches!(current_query, Err(StorageError::Session)),
            _ => current_query.unwrap().is_none(),
        };
        if change != "unchanged" {
            assert!(direct_query_denied);
        }
        let mut old_selection_rejected_after_regrant = None;
        let mut fresh_query_after_regrant = None;
        if change == "revoke-other-account" {
            access.denied.store(false, Ordering::SeqCst);
            if let Some(previous) = &previous {
                old_selection_rejected_after_regrant = Some(
                    nav.select_record_with_feedback(
                        &identity,
                        &feedback,
                        previous["step_ref"].as_str().unwrap(),
                        1,
                        &nonce(),
                    )
                    .await
                    .is_err(),
                );
                assert_eq!(old_selection_rejected_after_regrant, Some(true));
            }
            fresh_query_after_regrant = Some(
                nav.query_feedback(&identity, &feedback, &operation, &nonce())
                    .await
                    .is_ok(),
            );
            assert_eq!(fresh_query_after_regrant, Some(true));
        }
        assert_eq!(all_business_rows(&db).await, before);
        observations.push(json!({
            "entry": if select_record { "select" } else { "query" }, "change":change,
            "blocked_at_final_provider_observation":true,"observation_number":2,
            "returned_success":success,"retained_feedback_ticket":retained,
            "same_session_direct_feedback_query_denied":direct_query_denied,
            "old_selection_rejected_after_regrant":old_selection_rejected_after_regrant,
            "fresh_query_after_regrant":fresh_query_after_regrant,
            "business_rows_unchanged":true,"expected_success":change=="unchanged"
        }));
        drop(feedback);
        server.stop();
        db.finish().await;
    }
    let name = if select_record {
        "repair1-late-select.json"
    } else {
        "repair1-late-query.json"
    };
    std::fs::write(
        std::path::Path::new(&std::env::var("SAFEFLOW_FEEDBACK_EVIDENCE").unwrap()).join(name),
        serde_json::to_vec_pretty(
            &json!({"mechanism_fixture_only":true,"observations":observations}),
        )
        .unwrap(),
    )
    .unwrap();
    observations
}

#[tokio::test]
#[ignore = "isolated PG; final provider wait report account revoke and legal control"]
async fn public_card_feedback_late_query_permission_is_rechecked() {
    let observations = final_report_permission_cases(false).await;
    assert!(
        observations
            .iter()
            .all(|o| o["returned_success"] == o["expected_success"]
                && o["retained_feedback_ticket"] == o["expected_success"]),
        "late query permission: {observations:?}"
    );
}

#[tokio::test]
#[ignore = "isolated PG; final selected-record wait report account revoke and legal control"]
async fn public_card_feedback_late_select_permission_is_rechecked() {
    let observations = final_report_permission_cases(true).await;
    assert!(
        observations
            .iter()
            .all(|o| o["returned_success"] == o["expected_success"]
                && o["retained_feedback_ticket"] == o["expected_success"]),
        "late select permission: {observations:?}"
    );
}
