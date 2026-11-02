//! Public composition tests: real HTTP read settlement -> API-owned backend
//! factory -> Runtime final Kernel -> confirmation/permit/member/public/Outbox.
//! Original fictional member inputs remain unchanged; never shared-service wiring.
use super::*;
use safeflow_billing_source_reader::feedback::{FeedbackChoice, FeedbackIntent};
use safeflow_contracts::SessionRef;
use safeflow_storage::{BillingFeedbackService, ConfirmationConfig, PermitConfig, StorageError};
#[path = "feedback_browser_consumer_test.rs"]
mod browser_consumer_test;
#[path = "feedback_connection_adoption_tests.rs"]
mod connection_adoption_tests;
#[path = "card_feedback_tests.rs"]
mod card_feedback_tests;
#[path = "feedback_domain_audit_tests.rs"]
mod domain_audit_tests;
#[path = "feedback_recovery_pg_fault.rs"]
mod pg_fault;
#[path = "feedback_http_tests.rs"]
mod http_entry_tests;
#[path = "feedback_recovery_tests.rs"]
mod recovery_tests;
#[path = "feedback_typed_tests.rs"]
mod typed_query_tests;

fn service(server: &Server) -> BillingFeedbackService<'_> {
    service_state(&server.state)
}
fn service_state(state: &ApiState) -> BillingFeedbackService<'_> {
    state
        .billing_feedback_for_backend(
            ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),
            PermitConfig::new(Duration::from_secs(300)).unwrap(),
        )
        .unwrap()
}
fn session(cookie: &str) -> SessionRef {
    crate::session::token_reference(cookie::Cookie::parse(cookie.to_owned()).unwrap().value())
        .unwrap()
}
fn intent(report: &Value) -> FeedbackIntent {
    FeedbackIntent {
        report_id: report["metadata"]["report_id"].as_str().unwrap().into(),
        transaction: serde_json::from_value(report["evidence"][0]["reference"].clone()).unwrap(),
        choice: FeedbackChoice::NotMine,
    }
}
async fn count(db: &Db, table: &str) -> i64 {
    // All callsites below are fixed test-owned table names, never user input.
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}
async fn report(
    db: &Db,
    provider: &BillingProvider,
) -> (Server, Arc<fixture::Replay>, String, Value) {
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let server = billing_server_with_feedback(
        db,
        provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source.clone(), "8ff8c665:mixed")),
        false,
        true,
    )
    .await;
    let cookie = server.login().await;
    let view = initial(&server, &cookie, "report").await;
    let complete = ok(execute(&server, &cookie, &view).await);
    fixture::assert_report("mixed", &complete["results"][0]["data"]["report"]);
    (server, source, cookie, complete)
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; local fictional source and protocol only"]
async fn billing_feedback_api_factory_complete_confirmation_execute_query_and_concurrency() {
    let db = Db::new().await;
    db.repo.migrate().await.unwrap();
    let provider = BillingProvider::new("mixed").await;
    let (server, source, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    let identity = server
        .state
        .billing_feedback_session_for_backend(&headers)
        .unwrap();
    let feedback = service(&server);
    assert_eq!(count(&db, "public_billing_reports_v1").await, 1);
    assert_eq!(
        feedback
            .original_report(&identity, intent(original).report_id.as_str())
            .await
            .unwrap(),
        *original
    );
    let prepared = feedback.prepare(&identity, intent(original)).await.unwrap();
    assert!(feedback
        .execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref
        )
        .await
        .is_err());
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    feedback.confirm(&identity, &prepared).await.unwrap();
    let run = || {
        feedback.execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref,
        )
    };
    let results = tokio::join!(run(), run(), run(), run(), run());
    let values = [results.0, results.1, results.2, results.3, results.4]
        .into_iter()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values.iter().filter(|v| !v.replayed).count(), 1);
    for value in &values {
        assert_eq!(value.saved, values[0].saved);
    }
    assert_eq!(values[0].saved["lock_authorized"], false);
    for table in [
        "billing_report_bindings_v1",
        "billing_feedback_v1",
        "billing_feedback_events_v1",
        "public_billing_feedback_results_v1",
    ] {
        assert_eq!(count(&db, table).await, 1, "{table}");
    }
    let consumed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_permits WHERE status='CONSUMED'")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(consumed, 1);
    assert!(count(&db, "public_audit_outbox").await > 0);
    let queried = feedback
        .query(&identity, &prepared.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queried.saved, values[0].saved);
    assert!(feedback
        .execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &safeflow_contracts::ConfirmationRef::parse("wrong_confirm").unwrap()
        )
        .await
        .is_err());
    // A NEW public operation for the same original transaction remains D20-closed.
    let second = feedback.prepare(&identity, intent(original)).await.unwrap();
    feedback.confirm(&identity, &second).await.unwrap();
    assert!(matches!(
        feedback
            .execute(
                &identity,
                &second.task.task_id,
                &second.operation.operation_id,
                &second.confirmation.confirmation_ref
            )
            .await,
        Err(StorageError::ExecutionOperationConflict)
    ));
    assert_eq!(count(&db, "billing_feedback_v1").await, 1);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    if let Ok(directory) = std::env::var("SAFEFLOW_FEEDBACK_EVIDENCE") {
        std::fs::write(std::path::Path::new(&directory).join("api-factory-complete.json"),serde_json::to_vec_pretty(&json!({"original_http":complete,"preparation":prepared,"queried":queried,"five_calls_one_write":true,"source_calls":1,"lock_authorized":false})).unwrap()).unwrap();
    }
    server.stop();
    db.finish().await;
}

async fn snapshot(db: &Db) -> Vec<i64> {
    let mut result = vec![];
    for table in [
        "billing_report_bindings_v1",
        "billing_feedback_v1",
        "billing_feedback_events_v1",
        "public_billing_feedback_results_v1",
        "execution_permits",
        "public_execution_requests",
        "audit_events",
        "public_audit_outbox",
    ] {
        result.push(count(db, table).await);
    }
    result
}
async fn assert_ready(db: &Db, operation: &safeflow_contracts::OperationId) {
    let state: String = sqlx::query_scalar("SELECT state FROM operations WHERE operation_id=$1")
        .bind(operation.as_str())
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(state, "READY");
}
#[tokio::test]
#[ignore = "isolated PostgreSQL; injected real transaction failures"]
async fn billing_feedback_member_public_result_outbox_failures_roll_back_everything() {
    for table in [
        "billing_feedback_events_v1",
        "public_billing_feedback_results_v1",
        "public_audit_outbox",
    ] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        let identity = session(&cookie);
        let feedback = service(&server);
        let prepared = feedback
            .prepare(&identity, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        feedback.confirm(&identity, &prepared).await.unwrap();
        let before = snapshot(&db).await;
        // Outbox fails at the final billing result audit, AFTER member stage.
        let condition = if table == "public_audit_outbox" {
            "WHEN (NEW.envelope->'public_event'->'details'->>'source'='billing-feedback-local-v1')"
        } else {
            ""
        };
        db.scoped.execute("CREATE FUNCTION feedback_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated feedback fault'; END $$").await.unwrap();
        db.scoped.execute(format!("CREATE TRIGGER feedback_fault BEFORE INSERT ON {table} FOR EACH ROW {condition} EXECUTE FUNCTION feedback_fault()").as_str()).await.unwrap();
        assert!(
            feedback
                .execute(
                    &identity,
                    &prepared.task.task_id,
                    &prepared.operation.operation_id,
                    &prepared.confirmation.confirmation_ref
                )
                .await
                .is_err(),
            "{table}"
        );
        assert_eq!(snapshot(&db).await, before, "{table} partial commit");
        assert_ready(&db, &prepared.operation.operation_id).await;
        db.scoped
            .execute(format!("DROP TRIGGER feedback_fault ON {table}").as_str())
            .await
            .unwrap();
        // The SAME operation/confirmation remains usable after a known rollback.
        feedback
            .execute(
                &identity,
                &prepared.task.task_id,
                &prepared.operation.operation_id,
                &prepared.confirmation.confirmation_ref,
            )
            .await
            .unwrap();
        assert_eq!(count(&db, "billing_feedback_v1").await, 1);
        server.stop();
        db.finish().await;
    }
}

async fn install_wait(db: &Db) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    db.scoped.execute("CREATE FUNCTION feedback_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER feedback_wait BEFORE INSERT ON billing_feedback_events_v1 FOR EACH ROW EXECUTE FUNCTION feedback_wait()").await.unwrap();
    let mut blocker = db.scoped.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker
}
async fn wait_for_stage(db: &Db) {
    wait_for_query(db, "INSERT INTO billing_feedback_events_v1%").await;
}
async fn wait_for_query(db: &Db, pattern: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event='advisory' AND query LIKE $1)").bind(pattern).fetch_one(&db.scoped).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("actual member stage did not wait");
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; actual read settlement waits AFTER archive insertion"]
async fn billing_feedback_original_settlement_revoke_after_archive_insert_leaves_no_orphan() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let server = billing_server(
        &db,
        &provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source.clone(), "8ff8c665:mixed")),
        false,
    )
    .await;
    let cookie = server.login().await;
    let identity = session(&cookie);
    let prepared = initial(&server, &cookie, "report").await;
    db.scoped.execute("CREATE FUNCTION read_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER read_wait BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.details ? 'runtime_read_execution') EXECUTE FUNCTION read_wait()").await.unwrap();
    let mut blocker = db.scoped.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut running = Box::pin(execute(&server, &cookie, &prepared));
    tokio::select! { _=&mut running => panic!("unexpected settle"), _=wait_for_query(&db,"INSERT INTO audit_events%") => {} }
    server.state.0.sessions.revoke(&identity).unwrap();
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    assert!(!running.await.status.is_success());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&db, "public_billing_reports_v1").await, 0);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL; actual waiting member SAVEPOINT cancellation/session change"]
async fn billing_feedback_cancel_or_revoke_during_stage_rolls_back_outer_transaction() {
    for revoke in [false, true] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        let identity = session(&cookie);
        let feedback = service(&server);
        let prepared = feedback
            .prepare(&identity, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        feedback.confirm(&identity, &prepared).await.unwrap();
        let before = snapshot(&db).await;
        let mut blocker = install_wait(&db).await;
        let mut running = Box::pin(feedback.execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref,
        ));
        tokio::select! {
            result=&mut running => panic!("stage returned before barrier: {:?}",result.err()),
            _=wait_for_stage(&db) => {}
        }
        if revoke {
            server.state.0.sessions.revoke(&identity).unwrap();
            sqlx::query("SELECT pg_advisory_unlock(8021021)")
                .execute(&mut *blocker)
                .await
                .unwrap();
            assert!(matches!(running.await, Err(StorageError::Session)));
        } else {
            // Drop the ENTIRE composer future, never just the member inner call.
            drop(running);
            sqlx::query("SELECT pg_advisory_unlock(8021021)")
                .execute(&mut *blocker)
                .await
                .unwrap();
        }
        drop(blocker);
        // A task lock round trip waits for SQLx's queued ROLLBACK to complete.
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(prepared.task.task_id.as_str())
            .fetch_one(&db.scoped)
            .await
            .unwrap();
        assert_eq!(snapshot(&db).await, before);
        assert_ready(&db, &prepared.operation.operation_id).await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; failed original settlement must not leave selectable evidence"]
async fn billing_feedback_archive_is_atomic_with_original_read_and_incomplete_stays_unselectable() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let server = billing_server(
        &db,
        &provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source, "8ff8c665:mixed")),
        false,
    )
    .await;
    let cookie = server.login().await;
    let prepared = initial(&server, &cookie, "report").await;
    // All settlement audit failures happen inside the original transaction.
    db.scoped.execute("CREATE FUNCTION read_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated read settle fault'; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER read_fault BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.details ? 'runtime_read_execution') EXECUTE FUNCTION read_fault()").await.unwrap();
    assert_ne!(
        execute(&server, &cookie, &prepared).await.status,
        StatusCode::OK
    );
    assert_eq!(count(&db, "public_billing_reports_v1").await, 0);
    db.scoped
        .execute("DROP TRIGGER read_fault ON audit_events")
        .await
        .unwrap();
    server.stop();
    let incomplete_provider = BillingProvider::new("page-failed").await;
    let source = fixture::replay("page-failed");
    let bank = fixture::bank();
    let server = billing_server(
        &db,
        &incomplete_provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source, "8ff8c665:page-failed")),
        false,
    )
    .await;
    let cookie = server.login().await;
    let view = initial(&server, &cookie, "report").await;
    let complete = ok(execute(&server, &cookie, &view).await);
    let report = &complete["results"][0]["data"]["report"];
    fixture::assert_report("page-failed", report);
    assert_eq!(count(&db, "public_billing_reports_v1").await, 0);
    assert!(matches!(
        service(&server)
            .original_report(
                &session(&cookie),
                report["metadata"]["report_id"].as_str().unwrap()
            )
            .await,
        Err(StorageError::NotFound)
    ));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "invoked only by separate-process recovery test with explicit existing namespace"]
async fn billing_feedback_restart_probe() {
    let Ok(namespace) = std::env::var("SAFEFLOW_FEEDBACK_RESTART_SCHEMA") else {
        return;
    };
    assert!(namespace.starts_with("sf_http_test_") && namespace.len() == 45);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect(&url, &namespace).await.unwrap();
    let state = ApiState::build_with_bank(
        repo.clone(),
        ApiConfig::local_simulation(Zeroizing::new(SECRET.into())),
        UserId::parse("user_demo").unwrap(),
        fixture::bank(),
        &[],
    )
    .unwrap();
    // A new authenticated session in a NEW OS process. No source is registered.
    let cookie = state.0.sessions.issue(SECRET, None).unwrap();
    let feedback = service_state(&state);
    let identity = session(&cookie);
    let report_id = std::env::var("SAFEFLOW_FEEDBACK_RESTART_REPORT").unwrap();
    let operation = safeflow_contracts::OperationId::parse(
        std::env::var("SAFEFLOW_FEEDBACK_RESTART_OPERATION").unwrap(),
    )
    .unwrap();
    let original = feedback
        .original_report(&identity, &report_id)
        .await
        .unwrap();
    let saved = feedback
        .query(&identity, &operation)
        .await
        .unwrap()
        .unwrap();
    std::fs::write(std::env::var("SAFEFLOW_FEEDBACK_RESTART_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({"pid":std::process::id(),"report":original,"result":saved,"registered_source":false})).unwrap()).unwrap();
    repo.close().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; launches a separate OS process without any source"]
async fn billing_feedback_new_process_original_report_and_original_operation_recovery() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, source, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let identity = session(&cookie);
    let feedback = service(&server);
    let prepared = feedback.prepare(&identity, intent(original)).await.unwrap();
    feedback.confirm(&identity, &prepared).await.unwrap();
    let saved = feedback
        .execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    let path = std::env::var("SAFEFLOW_FEEDBACK_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("restart-{}.json", db.namespace));
    server.stop();
    let helper = format!(
        "{}::billing_feedback_restart_probe",
        module_path!().split_once("::").unwrap().1
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", &helper, "--nocapture"])
        .env("SAFEFLOW_FEEDBACK_RESTART_SCHEMA", &db.namespace)
        .env(
            "SAFEFLOW_FEEDBACK_RESTART_REPORT",
            intent(original).report_id,
        )
        .env(
            "SAFEFLOW_FEEDBACK_RESTART_OPERATION",
            prepared.operation.operation_id.as_str(),
        )
        .env("SAFEFLOW_FEEDBACK_RESTART_OUTPUT", &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child recovery failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_ne!(probe["pid"], std::process::id());
    assert_eq!(probe["registered_source"], false);
    assert_eq!(probe["report"], *original);
    assert_eq!(probe["result"]["saved"], saved.saved);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; current owner/session and database absence versus failure"]
async fn billing_feedback_owner_session_tampering_and_database_error_are_distinct() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let identity = session(&cookie);
    let feedback = service(&server);
    let mut changed = intent(original);
    changed.transaction.transaction_id = "absent_transaction".into();
    assert!(feedback.prepare(&identity, changed).await.is_err());
    let prepared = feedback.prepare(&identity, intent(original)).await.unwrap();
    feedback.confirm(&identity, &prepared).await.unwrap();
    let fresh = session(&server.login().await);
    assert!(feedback
        .execute(
            &fresh,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref
        )
        .await
        .is_err());
    let foreign = billing_server(&db, &provider, "other_owner", fixture::bank(), None, false).await;
    let foreign_identity = session(&foreign.login().await);
    assert!(matches!(
        service(&foreign)
            .original_report(&foreign_identity, &intent(original).report_id)
            .await,
        Err(StorageError::NotFound)
    ));
    assert!(service(&foreign)
        .query(&foreign_identity, &prepared.operation.operation_id)
        .await
        .unwrap()
        .is_none());
    assert!(feedback
        .query(&identity, &prepared.operation.operation_id)
        .await
        .unwrap()
        .is_none());
    // No result was committed; a DB query failure must never turn into None.
    db.repo.close().await;
    assert!(matches!(
        feedback
            .query(&identity, &prepared.operation.operation_id)
            .await,
        Err(StorageError::Database)
    ));
    server.stop();
    foreign.stop();
    db.finish().await;
}

// Public authorization-mechanism fixture only. No new business rows/results:
// copy original transaction items; adapt test page's account-scope metadata to
// two accounts to prove a non-target ORIGINAL account remains required.
struct AccountSource(Arc<fixture::Replay>);
impl safeflow_billing_source_reader::query::PagedSource for AccountSource {
    fn source_id(&self) -> &str {
        safeflow_billing_source_reader::query::PagedSource::source_id(&*self.0)
    }
    fn read_page(
        &self,
        scope: &safeflow_billing_source_reader::TrustedScope,
        request: &safeflow_billing_source_reader::query::Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<
        safeflow_billing_source_reader::query::Page,
        safeflow_billing_source_reader::query::PageError,
    > {
        let mut page = safeflow_billing_source_reader::query::PagedSource::read_page(
            &*self.0, scope, request, cursor, snapshot,
        )?;
        page.account_ids = request.account_ids.clone();
        Ok(page)
    }
}
struct AccountAccess {
    denied: std::sync::atomic::AtomicBool,
}
impl BankGateway for AccountAccess {
    fn get_account_balance<'a>(
        &'a self,
        owner: &'a UserId,
        account: &'a safeflow_contracts::ResourceId,
    ) -> safeflow_contracts::ContractFuture<
        'a,
        Result<safeflow_bank_gateway::GetAccountBalanceOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async move {
            if account.as_str() == "account_002" && self.denied.load(Ordering::SeqCst) {
                return Err(safeflow_bank_gateway::BankError::ResourceNotOwned);
            }
            let mut result = fixture::bank()
                .get_account_balance(
                    owner,
                    &safeflow_contracts::ResourceId::parse("account_001").unwrap(),
                )
                .await?;
            result.account.account_id = account.clone();
            Ok(result)
        })
    }
    fn find_payees<'a>(
        &'a self,
        owner: &'a UserId,
        query: &'a str,
    ) -> safeflow_contracts::ContractFuture<
        'a,
        Result<safeflow_bank_gateway::FindPayeesOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async move { fixture::bank().find_payees(owner, query).await })
    }
}
#[tokio::test]
#[ignore = "isolated PostgreSQL; public authorization-mechanism fixture, unchanged member rows"]
async fn billing_feedback_all_original_accounts_required_before_and_after_stage() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    provider.arguments.lock().unwrap()["account_ids_json"] =
        json!("[\"account_001\",\"account_002\"]");
    let source = fixture::replay("mixed");
    let access = Arc::new(AccountAccess {
        denied: std::sync::atomic::AtomicBool::new(false),
    });
    let server = billing_server(
        &db,
        &provider,
        "user_demo",
        access.clone(),
        Some(fixture::approved(
            access.clone(),
            Arc::new(AccountSource(source.clone())),
            "8ff8c665:auth-scope-test",
        )),
        false,
    )
    .await;
    let cookie = server.login().await;
    let identity = session(&cookie);
    let view = initial(&server, &cookie, "report").await;
    let completed = ok(execute(&server, &cookie, &view).await);
    let original = &completed["results"][0]["data"]["report"];
    let selection = intent(original);
    assert_eq!(selection.transaction.account_id, "account_001");
    let feedback = service(&server);
    access.denied.store(true, Ordering::SeqCst);
    assert!(matches!(
        feedback
            .original_report(&identity, &selection.report_id)
            .await,
        Err(StorageError::NotFound)
    ));
    access.denied.store(false, Ordering::SeqCst);
    let prepared = feedback.prepare(&identity, selection).await.unwrap();
    feedback.confirm(&identity, &prepared).await.unwrap();
    let before = snapshot(&db).await;
    let mut blocker = install_wait(&db).await;
    let mut running = Box::pin(feedback.execute(
        &identity,
        &prepared.task.task_id,
        &prepared.operation.operation_id,
        &prepared.confirmation.confirmation_ref,
    ));
    tokio::select! { result=&mut running => panic!("unexpected finish {:?}",result.err()), _=wait_for_stage(&db) => {} }
    access.denied.store(true, Ordering::SeqCst);
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    assert!(matches!(running.await, Err(StorageError::NotFound)));
    assert_eq!(snapshot(&db).await, before);
    access.denied.store(false, Ordering::SeqCst);
    feedback
        .execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    access.denied.store(true, Ordering::SeqCst);
    assert!(matches!(
        feedback
            .query(&identity, &prepared.operation.operation_id)
            .await,
        Err(StorageError::NotFound)
    ));
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; root/continuation reports bind separate settled receipts"]
async fn billing_feedback_continuation_archives_specific_report_and_detects_receipt_tampering() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let server = billing_server(
        &db,
        &provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source.clone(), "8ff8c665:mixed")),
        false,
    )
    .await;
    let cookie = server.login().await;
    let identity = session(&cookie);
    let view = initial(&server, &cookie, "wait").await;
    let waiting = ok(execute(&server, &cookie, &view).await);
    let mut input = version(&waiting);
    input["waiting_node_id"] = json!("choice");
    let id = waiting["task"]["task_id"].as_str().unwrap();
    let finished = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/continue"),
        input,
    )
    .await);
    assert_eq!(count(&db, "public_billing_reports_v1").await, 2);
    let feedback = service(&server);
    for item in finished["results"].as_array().unwrap() {
        let original = &item["data"]["report"];
        assert_eq!(
            feedback
                .original_report(&identity, &intent(original).report_id)
                .await
                .unwrap(),
            *original
        );
    }
    let second = &finished["results"][1]["data"]["report"];
    // A current/new report never substitutes for the original first snapshot.
    assert_ne!(
        second["metadata"]["report_id"],
        finished["results"][0]["data"]["report"]["metadata"]["report_id"]
    );
    sqlx::query("UPDATE runtime_read_continuations SET receipt=jsonb_set(receipt,'{results,1,data,source_revision}', '\"tampered\"'::jsonb) WHERE task_id=$1").bind(id).execute(&db.scoped).await.unwrap();
    assert!(matches!(
        feedback
            .original_report(&identity, &intent(second).report_id)
            .await,
        Err(StorageError::CorruptRecord)
    ));
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; exact closed terminal proof rejects post-result corruption/failure"]
async fn billing_feedback_terminal_proof_and_terminal_audit_failure_roll_back_all() {
    for corrupt in [true, false] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        let identity = session(&cookie);
        let feedback = service(&server);
        let prepared = feedback
            .prepare(&identity, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        feedback.confirm(&identity, &prepared).await.unwrap();
        let before = snapshot(&db).await;
        let body = if corrupt {
            "UPDATE tasks SET completed_nodes='[]'::jsonb WHERE task_id=NEW.task_id; RETURN NEW;"
        } else {
            "RAISE EXCEPTION 'isolated terminal audit failure';"
        };
        db.scoped.execute(format!("CREATE FUNCTION terminal_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN {body} END $$").as_str()).await.unwrap();
        db.scoped.execute("CREATE TRIGGER terminal_fault AFTER INSERT ON audit_events FOR EACH ROW WHEN (NEW.details->>'source'='billing-feedback-local-v1' AND NEW.details->>'event'='COMPLETE') EXECUTE FUNCTION terminal_fault()").await.unwrap();
        assert!(feedback
            .execute(
                &identity,
                &prepared.task.task_id,
                &prepared.operation.operation_id,
                &prepared.confirmation.confirmation_ref
            )
            .await
            .is_err());
        assert_eq!(snapshot(&db).await, before);
        assert_ready(&db, &prepared.operation.operation_id).await;
        db.scoped
            .execute("DROP TRIGGER terminal_fault ON audit_events")
            .await
            .unwrap();
        let result = feedback
            .execute(
                &identity,
                &prepared.task.task_id,
                &prepared.operation.operation_id,
                &prepared.confirmation.confirmation_ref,
            )
            .await
            .unwrap();
        assert_eq!(result.task_state, safeflow_contracts::TaskState::Completed);
        server.stop();
        db.finish().await;
    }
}
