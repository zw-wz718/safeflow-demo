//! Actual same-capture -> public selection -> original technical preparation.
//! All inputs are the unchanged member fictional pages. Public A2 stays closed.
use super::*;
use crate::billing_fixture::{fixture, FixtureSource};
use safeflow_billing_source_reader::{query as billing, TrustedScope};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counted {
    inner: FixtureSource,
    calls: AtomicUsize,
    rebind: Option<(Arc<Sessions>, SessionRef)>,
}
impl billing::PagedSource for Counted {
    fn source_id(&self) -> &str {
        self.inner.source_id()
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &billing::Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<billing::Page, billing::PageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((sessions, session)) = &self.rebind {
            sessions.bind(session, "user_demo", 2);
        }
        self.inner.read_page(scope, request, cursor, snapshot)
    }
}
fn input(case: &str) -> (billing::Request, Counted) {
    let (_, request, source) = fixture(case).unwrap();
    (
        request,
        Counted {
            inner: source,
            calls: AtomicUsize::new(0),
            rebind: None,
        },
    )
}
async fn capture(
    writes: &SubscriptionWriteService<'_>,
    db: &Db,
    host: &SubscriptionHost,
) -> crate::SubscriptionRegistrationCapture {
    let (request, source) = input("complete");
    let capture = writes
        .capture_registration_for_trusted_backend(
            &db.session,
            host,
            &request,
            &source,
            billing::Limits::default(),
        )
        .await
        .unwrap();
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    capture
}
#[tokio::test]
#[ignore]
async fn subscription_capture_preview_select_nonce_and_public_a2_rejection() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let before = db.facts().await;
    let (request, mut source) = input("complete");
    let expected_ids: Vec<Value> = source
        .inner
        .steps
        .iter()
        .flat_map(|step| step["page"]["items"].as_array().unwrap())
        .map(|row| row["transaction_id"].clone())
        .collect();
    let c = writes
        .capture_registration_for_trusted_backend(
            &db.session,
            &host,
            &request,
            &source,
            billing::Limits::default(),
        )
        .await
        .unwrap();
    assert_eq!(c.preview()["read"]["pages_read"], 2);
    assert_eq!(c.preview()["read"]["observed_rows"], expected_ids.len());
    // Make a second collect impossible after preview. Selection must use owned facts.
    source.inner.steps.clear();
    let selection = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
        .await
        .unwrap();
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        selection.request["source_contract"],
        "36cdc62-same-capture-v1"
    );
    assert_eq!(selection.request["context"]["user_id"], "user_demo");
    let selected_ids: Vec<Value> = selection.request["input"]["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["transaction_id"].clone())
        .collect();
    // Includes transfer/refund/pending/failed rows, not only eligible consumption.
    assert_eq!(selected_ids, expected_ids);
    assert_eq!(selection.request["input"]["window"], json!(request.window));
    assert_eq!(selection.request["index"], 0);
    assert_eq!(before, db.facts().await);
    let nonce = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        writes
            .prepare_registration_from_selection(&db.session, &host, &nonce, &selection)
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(before, db.facts().await);
    let (a, b) = tokio::join!(
        prepare(&writes, &db, &host, &nonce, &selection),
        prepare(&writes, &db, &host, &nonce, &selection)
    );
    let a = a.unwrap();
    assert_eq!(json!(a), json!(b.unwrap()));
    let after = db.facts().await;
    assert_eq!(after["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["subscription_public_bindings_v1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        after["subscription_preparations_v1"][0]["request_identity"]["request"],
        selection.request
    );
    assert_eq!(
        json!(a),
        json!(writes.recover_technical(&db.session, &nonce).await.unwrap())
    );
    assert_eq!(
        writes.recover_preparation(&db.session, &nonce).await.err(),
        Some(StorageError::DependencyUnavailable)
    );
    let other = capture(&writes, &db, &host).await;
    let other = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, other, 0)
        .await
        .unwrap();
    assert_eq!(
        prepare(&writes, &db, &host, &nonce, &other).await.err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(after, db.facts().await);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_failed_pages_scope_and_index_leave_no_facts() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let before = db.facts().await;
    for case in [
        "second-page-failed",
        "snapshot-changed",
        "no-completion-proof",
        "source-unavailable",
        "foreign-account-row",
    ] {
        let (request, source) = input(case);
        assert!(
            writes
                .capture_registration_for_trusted_backend(
                    &db.session,
                    &host,
                    &request,
                    &source,
                    billing::Limits::default()
                )
                .await
                .is_err(),
            "{case}"
        );
        assert_eq!(before, db.facts().await);
    }
    let (request, source) = input("complete");
    assert_eq!(
        writes
            .capture_registration_for_trusted_backend(
                &db.session,
                &host,
                &request,
                &source,
                billing::Limits {
                    max_pages: 1,
                    max_rows: 100
                }
            )
            .await
            .err()
            .map(|_| true),
        Some(true)
    );
    let c = capture(&writes, &db, &host).await;
    assert!(writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, usize::MAX)
        .await
        .is_err());
    db.sessions.bind(&db.session, "different_owner", 1);
    let (request, source) = input("complete");
    assert_eq!(
        writes
            .capture_registration_for_trusted_backend(
                &db.session,
                &host,
                &request,
                &source,
                billing::Limits::default()
            )
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    assert_eq!(before, db.facts().await);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_identity_changes_during_collect_are_rejected() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let before = db.facts().await;
    let (request, mut source) = input("complete");
    source.rebind = Some((db.sessions.clone(), db.session.clone()));
    assert_eq!(
        writes
            .capture_registration_for_trusted_backend(
                &db.session,
                &host,
                &request,
                &source,
                billing::Limits::default()
            )
            .await
            .err(),
        Some(StorageError::Session)
    );
    assert_eq!(before, db.facts().await);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_session_service_and_host_binding_are_not_transferable() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let before = db.facts().await;
    let c = capture(&writes, &db, &host).await;
    let other = SessionRef::parse("same_owner_another_capture_session").unwrap();
    db.sessions.bind(&other, "user_demo", 1);
    assert_eq!(
        writes
            .select_registration_from_capture_for_trusted_backend(&other, &host, c, 0)
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    let c = capture(&writes, &db, &host).await;
    let another = registration_writes(&db, assembly.runtime());
    assert_eq!(
        another
            .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(before, db.facts().await);
    let c = capture(&writes, &db, &host).await;
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        writes
            .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(before, db.facts().await);
    db.sessions.bind(&db.session, "user_demo", 1);
    let c = capture(&writes, &db, &host).await;
    let new_host = db.read.startup_for_host(&db.session).await.unwrap();
    let after = db.facts().await;
    assert_eq!(
        writes
            .select_registration_from_capture_for_trusted_backend(&db.session, &new_host, c, 0)
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(after, db.facts().await);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_journal_change_after_preview_rejects_old_index() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let c = capture(&writes, &db, &host).await;
    let store = db.read.store().unwrap();
    let mut tx = db.read.repository.pool.begin().await.unwrap();
    let p = store
        .stage_startup_recovery_on_connection_mock(&mut tx, "user_demo")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut reader = db
        .read
        .repository
        .subscription_read_pool
        .acquire()
        .await
        .unwrap();
    p.read_committed_startup_summary_on_connection_mock(&mut reader, &crate::subscription::now())
        .await
        .unwrap();
    drop(reader);
    let before = db.facts().await;
    assert_eq!(
        writes
            .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(before, db.facts().await);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_deadline_is_shared_and_not_refreshed_at_selection() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let before = db.facts().await;
    let mut c = capture(&writes, &db, &host).await;
    c.expire_for_test();
    assert_eq!(
        writes
            .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    assert_eq!(before, db.facts().await);
    let c = capture(&writes, &db, &host).await;
    let selected = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
        .await
        .unwrap();
    let reviewed = chrono::DateTime::parse_from_rfc3339(
        selected.request["capture_reviewed_at"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(selected.expires, reviewed + chrono::Duration::seconds(60));
    let p = prepare(
        &writes,
        &db,
        &host,
        &uuid::Uuid::new_v4().to_string(),
        &selected,
    )
    .await
    .unwrap();
    // PostgreSQL stores microseconds; the original deadline must not extend.
    assert_eq!(
        p.confirmation.expires_at.timestamp_micros(),
        selected.expires.timestamp_micros()
    );
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_does_not_initialize_missing_ledger() {
    let db = Db::new().await;
    let other = Db::new().await;
    other.initialize("complete-empty").await;
    let host = other.read.startup_for_host(&other.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let before = db.facts().await;
    let (request, source) = input("complete");
    assert!(writes
        .capture_registration_for_trusted_backend(
            &db.session,
            &host,
            &request,
            &source,
            billing::Limits::default()
        )
        .await
        .is_err());
    assert_eq!(before, db.facts().await);
    db.finish().await;
    other.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_restart_seed() {
    let root = std::path::PathBuf::from(
        std::env::var("SAFEFLOW_SUBSCRIPTION_CAPTURE_RESTART_ROOT").unwrap(),
    );
    let path = root.join("capture-restart-state.json");
    assert!(!path.exists());
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let mut writes = registration_writes(&db, assembly.runtime());
    writes.config = ConfirmationConfig::new(Duration::from_secs(600)).unwrap();
    writes.confirmations = ConfirmationService::for_trusted_backend(
        db.read.repository.clone(),
        db.sessions.clone(),
        writes.config.clone(),
    );
    let c = capture(&writes, &db, &host).await;
    let selected = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, c, 0)
        .await
        .unwrap();
    let nonce = uuid::Uuid::new_v4().to_string();
    let preparation = prepare(&writes, &db, &host, &nonce, &selected)
        .await
        .unwrap();
    let legacy = select(&writes, &db, &host).await;
    assert_eq!(
        legacy.request["source_contract"],
        "c163-trusted-selection-v1"
    );
    let legacy_nonce = uuid::Uuid::new_v4().to_string();
    let legacy_preparation = prepare(&writes, &db, &host, &legacy_nonce, &legacy)
        .await
        .unwrap();
    let uncommitted = capture(&writes, &db, &host).await;
    drop(uncommitted);
    std::fs::write(path,serde_json::to_vec_pretty(&json!({"schema":db.schema,"nonce":nonce,"preparation":preparation,
        "legacy_nonce":legacy_nonce,"legacy_preparation":legacy_preparation,"unknown_nonce":uuid::Uuid::new_v4().to_string(),"facts":db.facts().await})).unwrap()).unwrap();
    drop(writes);
    drop(assembly);
    db.read.repository.close().await;
    db.admin.close().await;
}
#[tokio::test]
#[ignore]
async fn subscription_capture_restart_verify() {
    let root = std::path::PathBuf::from(
        std::env::var("SAFEFLOW_SUBSCRIPTION_CAPTURE_RESTART_ROOT").unwrap(),
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.join("capture-restart-state.json")).unwrap())
            .unwrap();
    let schema = saved["schema"].as_str().unwrap().to_string();
    assert!(schema.starts_with("sf_subscription_test_") && schema.len() == 53);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect_existing(&url, &schema).await.unwrap();
    let sessions = Arc::new(Sessions::default());
    let session = SessionRef::parse("http_sim_isolated_non_replay_reference").unwrap();
    sessions.bind(&session, "user_demo", 1);
    let db = Db {
        schema,
        admin: PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap(),
        read: SubscriptionService::for_trusted_backend(repo, sessions.clone()),
        sessions,
        session,
    };
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    for (key, value) in [
        ("nonce", "preparation"),
        ("legacy_nonce", "legacy_preparation"),
    ] {
        let p = writes
            .recover_technical(&db.session, saved[key].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(json!(p), saved[value]);
        assert_eq!(
            writes
                .recover_preparation(&db.session, saved[key].as_str().unwrap())
                .await
                .err(),
            Some(StorageError::DependencyUnavailable)
        );
    }
    assert_eq!(
        writes
            .recover_technical(&db.session, saved["unknown_nonce"].as_str().unwrap())
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(db.facts().await, saved["facts"]);
    std::fs::write(root.join("capture-restart-verified.json"),serde_json::to_vec_pretty(&json!({"new_same_capture_nonce_only":true,"legacy_c163_nonce_only":true,"no_old_handle_or_recollect":true,"facts_unchanged":true,"public_a2_rejected":true})).unwrap()).unwrap();
    drop(writes);
    drop(assembly);
    db.finish().await;
}
