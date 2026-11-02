//! Isolated PG verification using original member rows/initials, never new
//! invented business examples. Faults alter only this test's temporary schema.
use super::*;
use crate::billing_fixture::fixture;
use crate::simulation_source::SimulationCapture;
use crate::{PgSimulationSource, SimulationSourceSpec};
use registration_tests::{registration_writes, FixtureAccounts};
use safeflow_billing_source_reader::{query::Transaction, query::*, TrustedScope};
use std::sync::atomic::{AtomicUsize, Ordering};
#[path = "simulation_source_pg_fault.rs"]
mod fault;
fn input(case: &str) -> (SimulationSourceSpec, Request, Vec<Transaction>) {
    let (_, request, source) = fixture(case).unwrap();
    let rows = source
        .steps
        .iter()
        .filter_map(|s| s.get("page"))
        .flat_map(|p| p["items"].as_array().unwrap())
        .map(|r| serde_json::from_value(r.clone()).unwrap())
        .collect();
    (
        SimulationSourceSpec {
            source_id: source.source,
            owner: "user_demo".into(),
            revision: format!("original-{case}-v1"),
            accounts: request.account_ids.clone(),
            coverage: request.window.clone(),
            complete: true,
            coverage_evidence: format!(
                "member original consumption_pages.v1.json case {case}; isolated validation only"
            ),
            page_size: 2,
        },
        request,
        rows,
    )
}
async fn source(db: &Db, case: &str, initial_case: Option<&str>) -> (PgSimulationSource, Request) {
    let (spec, request, rows) = input(case);
    let s = PgSimulationSource::import(
        db.read.repository.clone(),
        spec,
        rows,
        initial_case.map(initial),
    )
    .await
    .unwrap();
    s.set_active(true).await.unwrap();
    (s, request)
}
fn scope(request: &Request) -> TrustedScope {
    TrustedScope::from_verified_context("user_demo", request.account_ids.iter().map(String::as_str))
        .unwrap()
}
fn owner() -> UserId {
    UserId::parse("user_demo").unwrap()
}
async fn capture(s: &PgSimulationSource, r: &Request) -> SimulationCapture {
    s.capture(&owner(), r).await.unwrap()
}
#[tokio::test]
#[ignore]
async fn simulation_source_sql_typed_multipage_binding_and_boundaries() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    let expected = input("complete").2;
    let c = capture(&s, &r).await;
    let reader = c.reader();
    let report = collect(&scope(&r), &r, &reader, Limits::default());
    assert!(report.is_complete);
    assert_eq!(report.pages_read, 3);
    let mut observed = report.observed_transactions.clone();
    observed.sort_by_key(|x| (x.account_id.clone(), x.transaction_id.clone()));
    let mut original = expected.clone();
    original.sort_by_key(|x| (x.account_id.clone(), x.transaction_id.clone()));
    assert_eq!(observed, original);
    assert!(observed
        .iter()
        .any(|x| x.transaction_type == TransactionType::Transfer));
    let first = reader.read_page(&scope(&r), &r, None, None).unwrap();
    for (cursor, snapshot) in [
        (first.next_cursor.clone(), Some("tampered".into())),
        (Some("bad".into()), Some(first.snapshot_id.clone())),
        (None, Some(first.snapshot_id.clone())),
    ] {
        assert!(reader
            .read_page(&scope(&r), &r, cursor.as_deref(), snapshot.as_deref())
            .is_err())
    }
    let mut other = r.clone();
    other.window.as_of = "2026-10-14T12:00:00+08:00".into();
    assert!(reader.read_page(&scope(&r), &other, None, None).is_err());
    let foreign =
        TrustedScope::from_verified_context("foreign", r.account_ids.iter().map(String::as_str))
            .unwrap();
    assert!(reader.read_page(&foreign, &r, None, None).is_err());
    drop(c);
    assert_eq!(local_four_month_window(&r.window.as_of).unwrap(), r.window);
    // Actual SQL half-open filtering, using only original transaction times.
    let boundary = expected
        .iter()
        .filter_map(|x| x.completed_at.clone())
        .min()
        .unwrap();
    let mut narrow = r.clone();
    narrow.window.end_exclusive = boundary.clone();
    if time_lt(&narrow.window.start_inclusive, &boundary) {
        let c = capture(&s, &narrow).await;
        let got = collect(&scope(&narrow), &narrow, &c.reader(), Limits::default());
        assert!(got.is_complete);
        assert!(got
            .observed_transactions
            .iter()
            .all(|x| x.completed_at.is_none()));
    }
    let mut wide = r.clone();
    wide.window.start_inclusive = "2026-01-01T00:00:00+08:00".into();
    assert!(s.capture(&owner(), &wide).await.is_err());
    let mut foreign_account = r.clone();
    foreign_account.account_ids = vec!["account_other".into()];
    assert!(s.capture(&owner(), &foreign_account).await.is_err());
    for alternate_source in [false, true] {
        let (mut spec, _, rows) = input("complete");
        if alternate_source {
            spec.source_id.push_str("-alternate")
        } else {
            spec.revision.push_str("-next")
        }
        let next = PgSimulationSource::import(
            db.read.repository.clone(),
            spec,
            rows,
            Some(initial("complete-empty")),
        )
        .await
        .unwrap();
        next.set_active(true).await.unwrap();
        let capture = capture(&next, &r).await;
        let reader = capture.reader();
        assert!(reader
            .read_page(
                &scope(&r),
                &r,
                first.next_cursor.as_deref(),
                Some(&first.snapshot_id)
            )
            .is_err());
        drop(capture);
    }
    let (spec, _, rows) = input("complete");
    PgSimulationSource::import(
        db.read.repository.clone(),
        spec.clone(),
        rows.clone(),
        Some(initial("complete-empty")),
    )
    .await
    .unwrap();
    assert!(PgSimulationSource::import(
        db.read.repository.clone(),
        spec,
        rows,
        Some(initial("already-active"))
    )
    .await
    .is_err());
    db.finish().await;
}
fn time_lt(a: &str, b: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(a).unwrap()
        < chrono::DateTime::parse_from_rfc3339(b).unwrap()
}
struct GateBank {
    calls: AtomicUsize,
    at: usize,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    deny: bool,
}
impl GateBank {
    fn new(at: usize, deny: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            at,
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            deny,
        })
    }
}
impl safeflow_bank_gateway::BankGateway for GateBank {
    fn get_account_balance<'a>(
        &'a self,
        u: &'a UserId,
        a: &'a ResourceId,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::GetAccountBalanceOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.at {
                self.entered.notify_one();
                self.release.notified().await;
                if self.deny {
                    return Err(safeflow_bank_gateway::BankError::ResourceNotOwned);
                }
            }
            FixtureAccounts.get_account_balance(u, a).await
        })
    }
    fn find_payees<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a str,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::FindPayeesOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async { Err(safeflow_bank_gateway::BankError::Unavailable) })
    }
}
#[tokio::test]
#[ignore]
async fn simulation_source_async_last_await_session_account_and_initial_rollback() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    for deny in [false, true] {
        db.sessions.bind(&db.session, "user_demo", 1);
        let bank = GateBank::new(2, deny);
        let service = db.read.clone();
        let src = s.clone();
        let request = r.clone();
        let session = db.session.clone();
        let b = bank.clone();
        let task = tokio::spawn(async move {
            service
                .capture_simulation_for_backend(&session, b.as_ref(), &src, &request)
                .await
        });
        bank.entered.notified().await;
        if !deny {
            db.sessions.bind(&db.session, "user_demo", 2)
        }
        bank.release.notify_one();
        assert!(task.await.unwrap().is_err());
    }
    db.sessions.bind(&db.session, "user_demo", 1);
    let bank = GateBank::new(1, true);
    let service = db.read.clone();
    let src = s.clone();
    let session = db.session.clone();
    let b = bank.clone();
    let task = tokio::spawn(async move {
        service
            .initialize_simulation_for_host(&session, b.as_ref(), &src)
            .await
    });
    bank.entered.notified().await;
    bank.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert!(db.journal_optional().await.is_none());
    assert!(!db
        .read
        .lookup_simulation_initial_for_backend(&db.session, &s)
        .await
        .unwrap());
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_generation_lock_cancel_timeout_and_connection_release() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    let c = capture(&s, &r).await;
    let src = s.clone();
    let update = tokio::spawn(async move { src.set_active(false).await });
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!update.is_finished());
    drop(c);
    update.await.unwrap().unwrap();
    assert!(s.capture(&owner(), &r).await.is_err());
    s.set_active(true).await.unwrap();
    for cancel in [true, false] {
        let mut lock = db.admin.begin().await.unwrap();
        sqlx::query(&format!(
            "SELECT * FROM \"{}\".simulation_source_heads_v1 FOR UPDATE",
            db.schema
        ))
        .fetch_all(&mut *lock)
        .await
        .unwrap();
        let src = s.clone();
        let request = r.clone();
        let task = tokio::spawn(async move { src.capture(&owner(), &request).await });
        let mut waited = false;
        for _ in 0..100 {
            let n:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FOR SHARE OF h%'").fetch_one(&db.admin).await.unwrap();
            if n > 0 {
                waited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(waited, "actual async SQL must be waiting");
        if cancel {
            task.abort();
            assert!(matches!(task.await,Err(e) if e.is_cancelled()))
        } else {
            assert!(task.await.unwrap().is_err())
        }
        lock.rollback().await.unwrap();
        let c = capture(&s, &r).await;
        drop(c);
    }
    // Incomplete/invalid query failures also release both pooled connections.
    let (a, b) = tokio::join!(
        db.read.repository.pool.acquire(),
        db.read.repository.pool.acquire()
    );
    drop(a.unwrap());
    drop(b.unwrap());
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_initial_fence_serializes_disable_and_host_restart() {
    let db = Db::new().await;
    let (s, _) = source(&db, "complete", Some("complete-empty")).await;
    let bank = GateBank::new(1, false);
    let service = db.read.clone();
    let src = s.clone();
    let session = db.session.clone();
    let b = bank.clone();
    let task = tokio::spawn(async move {
        service
            .initialize_simulation_for_host(&session, b.as_ref(), &src)
            .await
    });
    bank.entered.notified().await;
    let src = s.clone();
    let deactivate = tokio::spawn(async move { src.set_active(false).await });
    let service = db.read.clone();
    let session = db.session.clone();
    let restart = tokio::spawn(async move { service.startup_for_host(&session).await });
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!deactivate.is_finished());
    assert!(!restart.is_finished());
    bank.release.notify_one();
    task.await.unwrap().unwrap();
    deactivate.await.unwrap().unwrap();
    restart.await.unwrap().unwrap();
    assert!(db
        .read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .is_err());
    assert!(db
        .read
        .lookup_simulation_initial_for_backend(&db.session, &s)
        .await
        .unwrap());
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_seal_full_digest_index_corruption_and_import_rollback() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    // Version sealed after import, including attempts with a new primary key.
    let row = input("missing-merchant-and-service").2.remove(0);
    let added=sqlx::query("INSERT INTO simulation_source_rows_v1 SELECT source_id,owner_id,revision,account_id,'extra-original',completed_at,body,digest FROM simulation_source_rows_v1 LIMIT 1").execute(&db.read.repository.pool).await;
    assert!(added.is_err());
    // DDL bypass is test-only to probe read-time detection, not a permission claim.
    let queries=["DELETE FROM simulation_source_rows_v1 WHERE ctid IN(SELECT ctid FROM simulation_source_rows_v1 LIMIT 1)","UPDATE simulation_source_rows_v1 SET completed_at=completed_at + interval '1 day' WHERE completed_at IS NOT NULL", "INSERT INTO simulation_source_rows_v1 SELECT source_id,owner_id,revision,account_id,'extra-original',completed_at,body,digest FROM simulation_source_rows_v1 LIMIT 1"];
    for q in queries {
        let mut tx = db.read.repository.pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE simulation_source_rows_v1 DISABLE TRIGGER USER")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(q).execute(&mut *tx).await.unwrap();
        // verify_version observes this same corrupted transaction, before rollback.
        assert!(s.verify_version(&mut tx).await.is_err());
        tx.rollback().await.unwrap();
        let c = capture(&s, &r).await;
        assert!(collect(&scope(&r), &r, &c.reader(), Limits::default()).is_complete);
    }
    let (mut spec, _, rows) = input("complete");
    spec.revision = "rollback".into();
    let sql=format!("CREATE FUNCTION \"{}\".sf_import_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.transaction_id='{}' THEN RAISE EXCEPTION 'owned fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER sf_import_fail BEFORE INSERT ON simulation_source_rows_v1 FOR EACH ROW EXECUTE FUNCTION sf_import_fail()",db.schema,rows.last().unwrap().transaction_id);
    db.read.repository.pool.execute(sql.as_str()).await.unwrap();
    assert!(
        PgSimulationSource::import(db.read.repository.clone(), spec, rows, None)
            .await
            .is_err()
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM simulation_source_versions_v1 WHERE revision='rollback'",
    )
    .fetch_one(&db.read.repository.pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
    assert!(row.merchant_id.is_none());
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_missing_partial_scope_initial_and_immutable_journal() {
    let db = Db::new().await;
    let (mut spec, r, rows) = input("complete");
    spec.complete = false;
    assert!(PgSimulationSource::import(
        db.read.repository.clone(),
        spec.clone(),
        rows.clone(),
        None
    )
    .await
    .is_err());
    spec.complete = true;
    for name in ["ledger-unavailable", "ledger-partial"] {
        assert!(PgSimulationSource::import(
            db.read.repository.clone(),
            spec.clone(),
            rows.clone(),
            Some(initial(name))
        )
        .await
        .is_err())
    }
    let (mut bad_owner, _, _) = input("complete");
    bad_owner.owner = "other".into();
    assert!(PgSimulationSource::import(
        db.read.repository.clone(),
        bad_owner,
        rows.clone(),
        Some(initial("complete-empty"))
    )
    .await
    .is_err());
    let (s, _) = source(&db, "complete", None).await;
    assert!(db
        .read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .is_err());
    assert_eq!(db.journal_optional().await, None);
    let (mut spec, _, rows) = input("complete");
    spec.revision = "with-initial".into();
    let s = PgSimulationSource::import(
        db.read.repository.clone(),
        spec,
        rows,
        Some(initial("already-active")),
    )
    .await
    .unwrap();
    s.set_active(true).await.unwrap();
    let (a, b) = tokio::join!(
        db.read
            .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s),
        db.read
            .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
    );
    a.unwrap();
    b.unwrap();
    assert!(db
        .read
        .lookup_simulation_initial_for_backend(&db.session, &s)
        .await
        .unwrap());
    let journal = db.journal().await;
    db.read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .unwrap();
    assert_eq!(db.journal().await, journal);
    let (mut spec, _, rows) = input("complete");
    spec.revision = "different-initial".into();
    let different = PgSimulationSource::import(
        db.read.repository.clone(),
        spec,
        rows,
        Some(initial("complete-empty")),
    )
    .await
    .unwrap();
    different.set_active(true).await.unwrap();
    assert!(db
        .read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &different)
        .await
        .is_err());
    assert_eq!(db.journal().await, journal);
    assert!(s.capture(&owner(), &r).await.is_err());
    let foreign = PgRepository::connect(
        &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
        &format!("sf_source_foreign_{}", uuid::Uuid::new_v4().simple()),
    )
    .await
    .unwrap();
    foreign.migrate().await.unwrap();
    let service = SubscriptionService::for_trusted_backend(foreign.clone(), db.sessions.clone());
    assert!(service
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &different)
        .await
        .is_err());
    sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", foreign.namespace))
        .execute(&db.admin)
        .await
        .unwrap();
    foreign.close().await;
    db.finish().await;
}
impl Db {
    async fn journal_optional(&self) -> Option<Value> {
        sqlx::query_scalar(
            "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
        )
        .fetch_optional(&self.read.repository.pool)
        .await
        .unwrap()
    }
}
#[tokio::test]
#[ignore]
async fn simulation_source_initial_real_commit_reply_loss_and_uncommitted_control() {
    for committed in [true, false] {
        let db = Db::new().await;
        let (s, _) = source(&db, "complete", Some("complete-empty")).await;
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let proxy =
            fault::CommitLossProxy::start("127.0.0.1:59557".parse().unwrap(), !committed).await;
        let proxy_url = url.replace("127.0.0.1:59557", &proxy.address.to_string());
        let repository = PgRepository::connect_existing(&proxy_url, &db.schema)
            .await
            .unwrap();
        // Reopen with the actual SQL database identity, not the proxy TCP address.
        let src = PgSimulationSource::open(repository.clone(), s.spec().clone())
            .await
            .unwrap();
        let service =
            SubscriptionService::for_trusted_backend(repository.clone(), db.sessions.clone());
        if committed {
            assert!(matches!(
                service
                    .initialize_simulation_for_host(&db.session, &FixtureAccounts, &src)
                    .await,
                Err(StorageError::ReconcileRequired)
            ))
        } else {
            let session = db.session.clone();
            let task = tokio::spawn(async move {
                service
                    .initialize_simulation_for_host(&session, &FixtureAccounts, &src)
                    .await
            });
            proxy.commit_seen.notified().await;
            task.abort();
            let _ = task.await;
        }
        assert!(proxy.fired.load(Ordering::SeqCst));
        drop(proxy);
        repository.close().await;
        assert_eq!(
            db.read
                .lookup_simulation_initial_for_backend(&db.session, &s)
                .await
                .unwrap(),
            committed
        );
        let evidence = json!({"committed":committed,"observed":db.journal_optional().await,"pid":std::process::id(),"retry_write":false});
        std::fs::write(
            std::path::Path::new(&std::env::var("SAFEFLOW_SIMULATION_EVIDENCE").unwrap())
                .join(format!("initial-commit-{committed}.json")),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn simulation_source_same_capture_technical_fences_and_no_public_writes() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    db.read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .unwrap();
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let capture = writes
        .capture_simulation_registration_for_backend(&db.session, &host, &s, &r)
        .await
        .unwrap();
    assert!(capture.preview().to_string().contains("fictional"));
    let selected = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, capture, 0)
        .await
        .unwrap();
    let nonce = uuid::Uuid::new_v4().to_string();
    assert!(writes
        .prepare_registration_from_selection(&db.session, &host, &nonce, &selected)
        .await
        .is_err());
    let prepared = writes
        .prepare_registration_technical_for_trusted_backend(&db.session, &host, &nonce, &selected)
        .await
        .unwrap();
    let journal = db.journal().await;
    db.read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .unwrap();
    assert_eq!(db.journal().await, journal);
    let captured = writes
        .capture_simulation_registration_for_backend(&db.session, &host, &s, &r)
        .await
        .unwrap();
    s.set_active(false).await.unwrap();
    assert!(writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, captured, 0)
        .await
        .is_err());
    s.set_active(true).await.unwrap();
    let captured = writes
        .capture_simulation_registration_for_backend(&db.session, &host, &s, &r)
        .await
        .unwrap();
    let selected = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, captured, 0)
        .await
        .unwrap();
    s.set_active(true).await.unwrap();
    let before = db.facts().await;
    assert!(writes
        .prepare_registration_technical_for_trusted_backend(
            &db.session,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            &selected
        )
        .await
        .is_err());
    assert_eq!(db.facts().await, before);
    assert!(db.read.require_write_adoption(&db.session).is_err());
    let _ = prepared;
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_selection_and_preparation_final_gateway_rejection() {
    for preparing in [false, true] {
        let db = Db::new().await;
        let (s, r) = source(&db, "complete", Some("complete-empty")).await;
        db.read
            .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
            .await
            .unwrap();
        let host = db.read.startup_for_host(&db.session).await.unwrap();
        let assembly = db.assembly();
        let mut writes = registration_writes(&db, assembly.runtime());
        let capture = writes
            .capture_simulation_registration_for_backend(&db.session, &host, &s, &r)
            .await
            .unwrap();
        let bank = GateBank::new(2, true);
        let b = bank.clone();
        let before = db.facts().await;
        if preparing {
            let selected = writes
                .select_registration_from_capture_for_trusted_backend(
                    &db.session,
                    &host,
                    capture,
                    0,
                )
                .await
                .unwrap();
            writes.bank = b;
            let work = async {
                writes
                    .prepare_registration_technical_for_trusted_backend(
                        &db.session,
                        &host,
                        &uuid::Uuid::new_v4().to_string(),
                        &selected,
                    )
                    .await
            };
            let fault = async {
                bank.entered.notified().await;
                bank.release.notify_one();
            };
            let (result, _) = tokio::join!(work, fault);
            assert!(result.is_err());
        } else {
            writes.bank = b;
            let work = writes.select_registration_from_capture_for_trusted_backend(
                &db.session,
                &host,
                capture,
                0,
            );
            let fault = async {
                bank.entered.notified().await;
                bank.release.notify_one();
            };
            let (result, _) = tokio::join!(work, fault);
            assert!(result.is_err());
        }
        assert_eq!(db.facts().await, before);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn simulation_source_actual_database_scope_nullable_and_capacity() {
    let db = Db::new().await;
    let (s, r) = source(&db, "missing-merchant-and-service", None).await;
    let c = capture(&s, &r).await;
    let report = collect(&scope(&r), &r, &c.reader(), Limits::default());
    assert!(report.is_complete);
    assert!(report.observed_transactions[0].merchant_id.is_none());
    assert!(report.observed_transactions[0].service_ref.is_none());
    let name = format!("billing_test_source_{}", uuid::Uuid::new_v4().simple());
    db.admin
        .execute(format!("CREATE DATABASE {name}").as_str())
        .await
        .unwrap();
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let url = format!("{}/{}", url.rsplit_once('/').unwrap().0, name);
    let other = PgRepository::connect(&url, &db.schema).await.unwrap();
    other.migrate().await.unwrap();
    let mut other_connection = other.pool.acquire().await.unwrap();
    assert_eq!(
        c.fence.lock(&mut other_connection).await.err(),
        Some(StorageError::TransactionScope)
    );
    drop(other_connection);
    other.close().await;
    db.admin
        .execute(format!("DROP DATABASE {name}").as_str())
        .await
        .unwrap();
    drop(c);
    let (spec, _, rows) = input("complete");
    let oversized = vec![rows[0].clone(); 10001];
    assert!(
        PgSimulationSource::import(db.read.repository.clone(), spec, oversized, None)
            .await
            .is_err()
    );
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_concurrent_real_capture_consumers_and_query_failure_release() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    db.read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .unwrap();
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            writes.capture_simulation_registration_for_backend(&db.session, &host, &s, &r),
            writes.capture_simulation_registration_for_backend(&db.session, &host, &s, &r)
        )
    })
    .await
    .unwrap();
    a.unwrap();
    b.unwrap();
    sqlx::query("ALTER TABLE simulation_source_rows_v1 RENAME COLUMN body TO owned_fault_body")
        .execute(&db.read.repository.pool)
        .await
        .unwrap();
    assert!(s.capture(&owner(), &r).await.is_err());
    sqlx::query("ALTER TABLE simulation_source_rows_v1 RENAME COLUMN owned_fault_body TO body")
        .execute(&db.read.repository.pool)
        .await
        .unwrap();
    let c = capture(&s, &r).await;
    drop(c);
    let (a, b) = tokio::join!(
        db.read.repository.simulation_source_pool.acquire(),
        db.read.repository.simulation_source_pool.acquire()
    );
    drop(a.unwrap());
    drop(b.unwrap());
    db.finish().await;
}
fn evidence_path(file: &str) -> std::path::PathBuf {
    std::path::Path::new(&std::env::var("SAFEFLOW_SIMULATION_EVIDENCE").unwrap()).join(file)
}
#[tokio::test]
#[ignore]
async fn simulation_source_final_gateway_deadline_and_local_capture_expiration() {
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    let bank = GateBank::new(2, false);
    let service = db.read.clone();
    let src = s.clone();
    let request = r.clone();
    let session = db.session.clone();
    let b = bank.clone();
    let task = tokio::spawn(async move {
        service
            .capture_simulation_for_backend(&session, b.as_ref(), &src, &request)
            .await
    });
    bank.entered.notified().await;
    assert!(task.await.unwrap().is_err());
    bank.release.notify_one();
    s.set_active(false).await.unwrap();
    s.set_active(true).await.unwrap();
    let c = capture(&s, &r).await;
    let reader = c.reader();
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(c.check_live().is_err());
    assert!(matches!(
        reader.read_page(&scope(&r), &r, None, None),
        Err(PageError::SnapshotExpired)
    ));
    drop(c);
    s.set_active(false).await.unwrap();
    db.finish().await;
}
fn report_value(s: &PgSimulationSource, r: &Request, c: &SimulationCapture) -> Value {
    let metadata = safeflow_billing_source_reader::spending::ReportMetadata {
        report_id: "source_restart_report".into(),
        calculated_at: r.window.as_of.clone(),
    };
    let (report, _) =
        crate::capture_billing_report(&scope(r), r, &c.reader(), &metadata, &s.spec().revision)
            .unwrap();
    serde_json::to_value(report).unwrap()
}
#[tokio::test]
#[ignore]
async fn simulation_source_restart_seed() {
    assert!(!evidence_path("source-restart-seed.json").exists());
    let db = Db::new().await;
    let (s, r) = source(&db, "complete", Some("complete-empty")).await;
    db.read
        .initialize_simulation_for_host(&db.session, &FixtureAccounts, &s)
        .await
        .unwrap();
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let captured = writes
        .capture_simulation_registration_for_backend(&db.session, &host, &s, &r)
        .await
        .unwrap();
    let selected = writes
        .select_registration_from_capture_for_trusted_backend(&db.session, &host, captured, 0)
        .await
        .unwrap();
    writes
        .prepare_registration_technical_for_trusted_backend(
            &db.session,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            &selected,
        )
        .await
        .unwrap();
    let c = capture(&s, &r).await;
    let report = report_value(&s, &r, &c);
    drop(c);
    let state = json!({"schema":db.schema,"spec":s.spec(),"request":r,"journal":db.journal().await,"facts":db.facts().await,"report":report,"process":std::process::id()});
    std::fs::write(
        evidence_path("source-restart-seed.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    db.read.repository.close().await;
    db.admin.close().await;
}
#[tokio::test]
#[ignore]
async fn simulation_source_restart_verify() {
    let state: Value =
        serde_json::from_slice(&std::fs::read(evidence_path("source-restart-seed.json")).unwrap())
            .unwrap();
    assert_ne!(state["process"], std::process::id());
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let schema = state["schema"].as_str().unwrap();
    assert!(schema.starts_with("sf_subscription_test_"));
    let repository = PgRepository::connect_existing(&url, schema).await.unwrap();
    let sessions = Arc::new(Sessions::default());
    let session = SessionRef::parse("new_process_read_session").unwrap();
    sessions.bind(&session, "user_demo", 1);
    let service = SubscriptionService::for_trusted_backend(repository.clone(), sessions);
    let source = PgSimulationSource::open(
        repository.clone(),
        serde_json::from_value(state["spec"].clone()).unwrap(),
    )
    .await
    .unwrap();
    let r: Request = serde_json::from_value(state["request"].clone()).unwrap();
    let c = capture(&source, &r).await;
    assert_eq!(report_value(&source, &r, &c), state["report"]);
    drop(c);
    assert!(service
        .lookup_simulation_initial_for_backend(&session, &source)
        .await
        .unwrap());
    let journal: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&repository.pool)
    .await
    .unwrap();
    assert_eq!(journal, state["journal"]);
    for (name, expected) in state["facts"].as_object().unwrap() {
        assert!([
            "subscription_runtime_records",
            "subscription_public_bindings_v1",
            "subscription_preparations_v1",
            "subscription_host_generations_v1",
            "tasks",
            "operations",
            "server_confirmations",
            "execution_permits",
            "public_execution_requests",
            "audit_events",
            "public_audit_outbox"
        ]
        .contains(&name.as_str()));
        let actual:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(v ORDER BY v::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) v FROM {name} t) q")).fetch_one(&repository.pool).await.unwrap();
        assert_eq!(&actual, expected);
    }
    std::fs::write(evidence_path("source-restart-verify.json"),serde_json::to_vec_pretty(&json!({"process":std::process::id(),"seed_process":state["process"],"all_original_facts_unchanged":true,"reimported":false,"reinitialized":false})).unwrap()).unwrap();
    repository.close().await;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    admin
        .execute(format!("DROP SCHEMA \"{schema}\" CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}
