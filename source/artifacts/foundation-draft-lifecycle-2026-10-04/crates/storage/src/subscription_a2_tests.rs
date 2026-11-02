//! Public-mechanism tests: invoke the unchanged real member transaction path.
use super::*;
use crate::subscription_local_audit::{checked_details, valid_business_time};
#[path = "subscription_a2_pg_fault.rs"]
mod pg_fault;
fn scope(db: &Db) -> SubscriptionA2CandidateScope {
    SubscriptionA2CandidateScope::for_isolated_backend(
        &db.schema,
        UserId::parse("user_demo").unwrap(),
        "subscription-a2-r1-20261025",
    )
    .unwrap()
}
#[tokio::test]
#[ignore]
async fn subscription_a2_exact_legacy_upgrade_and_writer_drain() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let a = db.assembly();
    let c = candidate(&db, a.runtime()).await;
    let p = change(&c, &db, &h, Action::RecordCancellation).await;
    run(&c, &db, &h, &p).await;
    let original = graph(&db).await;
    drop(c);
    // Upgrade fixture: keep the ACTUAL committed member/public/permit facts and
    // remove only this candidate's new graph in this OWNED throwaway schema.
    // This does not invent historical success or alter any product migration.
    let mut tx = db.read.repository.pool.begin().await.unwrap();
    tx.execute("DO $$ DECLARE r record; BEGIN FOR r IN SELECT t.tgname,c.relname FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid WHERE c.relnamespace=current_schema()::regnamespace AND t.tgname LIKE 'subscription_a2_%' AND NOT t.tgisinternal LOOP EXECUTE format('DROP TRIGGER %I ON %I',r.tgname,r.relname); END LOOP; END $$").await.unwrap();
    tx.execute("DROP TABLE public_subscription_a2_links_v1,public_subscription_a2_legacy_v1,public_subscription_a2_sources_v1").await.unwrap();
    tx.execute("ALTER TABLE public_audit_outbox DISABLE TRIGGER public_audit_outbox_immutable")
        .await
        .unwrap();
    tx.execute("DELETE FROM public_audit_outbox WHERE envelope->'public_event'->>'kind'='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'").await.unwrap();
    tx.execute("SET CONSTRAINTS ALL IMMEDIATE").await.unwrap();
    tx.execute("ALTER TABLE public_audit_outbox ENABLE TRIGGER public_audit_outbox_immutable")
        .await
        .unwrap();
    tx.execute("DELETE FROM audit_events WHERE kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'")
        .await
        .unwrap();
    tx.execute("ALTER TABLE audit_events DROP CONSTRAINT subscription_a2_details_check; DROP INDEX subscription_a2_one_domain; DROP FUNCTION subscription_a2_details_v1(jsonb),subscription_a2_install_only_v1(),verify_subscription_a2_v1(text),subscription_a2_association_v1(),subscription_a2_install_complete_v1()").await.unwrap();
    // Apply exact unchanged28 then exact29 DDL (not rewriting its checksum).
    let old_kind = include_str!("../migrations/0028_billing_domain_audit.sql")
        .split("ALTER TABLE audit_events ADD CONSTRAINT billing_domain_details_v1")
        .next()
        .unwrap();
    sqlx::raw_sql(old_kind).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0029_subscription_a2_audit.sql"))
        .execute(&db.read.repository.pool)
        .await
        .unwrap();
    // In-flight original writer holds the same shared install lock. Installation
    // must wait; timeout here would be a real error, never a fake legacy result.
    let mut writer = db.read.repository.pool.begin().await.unwrap();
    domain_audit::writer_lock(&mut writer).await.unwrap();
    let install_scope = scope(&db);
    let install = db
        .read
        .repository
        .install_subscription_a2_candidate_for_isolated_backend(&install_scope);
    let mut install = Box::pin(install);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut install)
            .await
            .is_err()
    );
    writer.rollback().await.unwrap();
    install.await.unwrap();
    let c = registration_tests::registration_writes(&db, a.runtime())
        .open_isolated_a2_candidate(&scope(&db))
        .await
        .unwrap();
    let result = c
        .query(&db.session, &p.operation.operation_id)
        .await
        .unwrap();
    assert_eq!(result.domain_audit["status"], "LEGACY_WITHOUT_DOMAIN_EVENT");
    let after = graph(&db).await;
    assert_eq!(
        after["public_subscription_a2_legacy_v1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        after["public_subscription_a2_links_v1"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    for table in [
        "subscription_runtime_records",
        "subscription_public_bindings_v1",
        "tasks",
        "operations",
        "execution_permits",
        "public_execution_requests",
    ] {
        assert_eq!(original[table], after[table]);
    }
    // A fresh post-install action is independently linked, never mistaken as old.
    let next = change(&c, &db, &h, Action::RevokeCancellation).await;
    let r = run(&c, &db, &h, &next).await;
    assert_eq!(r.domain_audit["status"], "VERIFIED_CANDIDATE_V1");
    save(
        "legacy-upgrade-and-drain",
        json!({"actual_saved_graph_reused":true,"upgrade_fixture_not_historical_release_claim":true,"writer_drained":true,"legacy_result":result,"new_result":r,"old_fact_rows_unchanged":true}),
    );
    drop(c);
    db.finish().await;
}
struct RevocableAccounts {
    calls: std::sync::atomic::AtomicUsize,
    at: std::sync::atomic::AtomicUsize,
    session_change: bool,
    sessions: Arc<Sessions>,
    session: SessionRef,
}
impl safeflow_bank_gateway::BankGateway for RevocableAccounts {
    fn get_account_balance<'a>(
        &'a self,
        u: &'a UserId,
        a: &'a ResourceId,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::GetAccountBalanceOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n >= self.at.load(Ordering::SeqCst) {
                if self.session_change {
                    self.sessions.bind(&self.session, "user_demo", 2);
                } else {
                    return Err(safeflow_bank_gateway::BankError::ResourceNotOwned);
                }
            }
            registration_tests::FixtureAccounts
                .get_account_balance(u, a)
                .await
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
        Box::pin(async { Err(safeflow_bank_gateway::BankError::ResourceNotOwned) })
    }
}
#[tokio::test]
#[ignore]
async fn subscription_a2_last_await_accounts_session_and_expiry_rollback() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for session_change in [false, true] {
        let db = Db::new().await;
        db.initialize("complete-empty").await;
        let h = db.read.startup_for_host(&db.session).await.unwrap();
        let a = db.assembly();
        let bank = Arc::new(RevocableAccounts {
            calls: AtomicUsize::new(0),
            at: AtomicUsize::new(usize::MAX),
            session_change,
            sessions: db.sessions.clone(),
            session: db.session.clone(),
        });
        let mut w = registration_tests::registration_writes(&db, a.runtime());
        w.bank = bank.clone();
        let c = w.into_isolated_a2_candidate(&scope(&db)).await.unwrap();
        let (_, request, source) = crate::billing_fixture::fixture("complete").unwrap();
        let capture = c
            .capture_registration(&db.session, &h, &request, &source, Default::default())
            .await
            .unwrap();
        let selected = c
            .select_registration_from_capture(&db.session, &h, capture, 0)
            .await
            .unwrap();
        let p = c
            .prepare_registration(
                &db.session,
                &h,
                &uuid::Uuid::new_v4().to_string(),
                &selected,
            )
            .await
            .unwrap();
        c.confirm(
            &db.session,
            &p.task.task_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
        let before = graph(&db).await;
        let count = before["subscription_public_bindings_v1"][0]["member_binding"]
            ["requested_account_ids"]
            .as_array()
            .unwrap()
            .len();
        assert!(count > 0);
        bank.at.store(
            bank.calls.load(Ordering::SeqCst) + 3 * count + 1,
            Ordering::SeqCst,
        );
        assert!(c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .is_err());
        assert_eq!(before, graph(&db).await);
        assert!(bank.calls.load(Ordering::SeqCst) >= bank.at.load(Ordering::SeqCst));
        drop(c);
        db.finish().await;
    }
    let db = Db::new().await;
    db.initialize("already-active").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let a = db.assembly();
    let mut w = db.writes(a.runtime());
    w.stage_observer = Some(repair_tests::cross_deadline);
    let c = w.into_isolated_a2_candidate(&scope(&db)).await.unwrap();
    let p = change(&c, &db, &h, Action::RecordCancellation).await;
    c.confirm(
        &db.session,
        &p.task.task_id,
        &p.confirmation.confirmation_ref,
    )
    .await
    .unwrap();
    let before = graph(&db).await;
    assert!(c
        .execute(
            &db.session,
            &h,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref
        )
        .await
        .is_err());
    assert_eq!(before, graph(&db).await);
    drop(c);
    db.finish().await;
    save(
        "final-guard-revocation",
        json!({"actual_original_accounts_rechecked":true,"account_denied_at_final_await":true,"session_renewed_at_final_await":true,"member_and_permit_deadline_enforced":true,"all_rolled_back":true}),
    );
}
#[tokio::test]
#[ignore]
async fn subscription_a2_commit_reply_loss_and_uncommitted_control() {
    use std::sync::atomic::Ordering;
    for committed in [false, true] {
        let mut db = Db::new().await;
        db.initialize("already-active").await;
        let h = db.read.startup_for_host(&db.session).await.unwrap();
        let a = db.assembly();
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let start = url.find('@').unwrap() + 1;
        let slash = start + url[start..].find('/').unwrap();
        let proxy =
            pg_fault::CommitLossProxy::start(url[start..slash].parse().unwrap(), true).await;
        let proxy_url = format!("{}{}{}", &url[..start], proxy.address, &url[slash..]);
        let direct = db.read.repository.clone();
        db.read.repository = PgRepository::connect(&proxy_url, &db.schema).await.unwrap();
        let c = candidate(&db, a.runtime()).await;
        let p = change(&c, &db, &h, Action::RecordCancellation).await;
        c.confirm(
            &db.session,
            &p.task.task_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
        let before = graph(&db).await;
        let error = {
            let running = c.execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            );
            tokio::pin!(running);
            tokio::select! {r=&mut running=>panic!("no real COMMIT barrier: {:?}",r.err()), r=tokio::time::timeout(Duration::from_secs(20),proxy.commit_seen.notified())=>r.unwrap()}
            assert!(proxy.fired.load(Ordering::SeqCst));
            if committed {
                proxy.release.notify_one();
                let e = running.await.err();
                assert_eq!(e, Some(StorageError::ReconcileRequired));
                format!("{e:?}")
            } else {
                "ABORTED_BEFORE_COMMIT".into()
            }
        };
        // Drop the pinned future/owned proxy closes writer without delivering COMMIT
        // in the false branch. Its database transaction rolls back on disconnect.
        drop(proxy);
        drop(c);
        db.read.repository = direct;
        let c = registration_tests::registration_writes(&db, a.runtime())
            .open_isolated_a2_candidate(&scope(&db))
            .await
            .unwrap();
        let result = c
            .query(&db.session, &p.operation.operation_id)
            .await
            .unwrap();
        if committed {
            assert_eq!(result.domain_audit["status"], "VERIFIED_CANDIDATE_V1");
        } else {
            assert_eq!(before, graph(&db).await);
            assert_eq!(result.domain_audit["status"], "UNEXECUTED_OR_NEEDS_REVIEW");
        }
        save(
            if committed {
                "commit-lost-response"
            } else {
                "commit-not-sent"
            },
            json!({"actual_protocol_fault":true,"committed":committed,"error":error,"query":result}),
        );
        drop(c);
        db.finish().await;
    }
}
fn transport_root() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("SAFEFLOW_SUBSCRIPTION_A2_EVIDENCE").unwrap())
        .parent()
        .unwrap()
        .join("transport")
}
fn transport_control(case: &str, mode: &str) {
    std::fs::write(
        transport_root().join("control.json"),
        json!({"case":case,"mode":mode}).to_string(),
    )
    .unwrap();
}
fn sender(db: &Db) -> crate::PublicAuditSender {
    sender_attempts(db, 2)
}
fn sender_attempts(db: &Db, attempts: u8) -> crate::PublicAuditSender {
    let ready: Value =
        serde_json::from_slice(&std::fs::read(transport_root().join("ready.json")).unwrap())
            .unwrap();
    assert_eq!(
        ready["endpoint"],
        "http://127.0.0.1:59539/api/audit/outbox/v1"
    );
    crate::PublicAuditSender::for_trusted_backend(
        crate::PublicAuditOutbox::for_trusted_backend(db.read.repository.clone()),
        crate::PublicAuditSenderConfig::new(
            ready["endpoint"].as_str().unwrap(),
            zeroize::Zeroizing::new(ready["token"].as_str().unwrap().into()),
            "fixed-408-isolated-compatibility",
            crate::PublicAuditRetryPolicy::new(
                Duration::from_secs(3),
                attempts,
                Duration::from_millis(100),
            )
            .unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}
#[tokio::test]
#[ignore]
async fn subscription_a2_restart_seed() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let a = db.assembly();
    let c = candidate(&db, a.runtime()).await;
    let p = change(&c, &db, &h, Action::RecordCancellation).await;
    let r = run(&c, &db, &h, &p).await;
    transport_control("subscription-restart", "lost-ack");
    let batch = sender_attempts(&db, 1).send_pending(1000).await.unwrap();
    let failed = batch
        .iter()
        .find(|v| v.event_id.as_str() == r.domain_audit["event_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(failed.outcome, Err(crate::AuditSendFailure::Transport));
    let pending = crate::PublicAuditOutbox::for_trusted_backend(db.read.repository.clone())
        .pending(1000)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    save(
        "restart-seed",
        json!({"pid":std::process::id(),"schema":db.schema,"operation":p.operation.operation_id,"business_ref":r.domain_audit["details"]["business_ref"],"result":r,"graph":graph(&db).await,"pending":{"event_id":pending[0].event_id,"envelope":pending[0].envelope,"digest":pending[0].digest}}),
    );
    drop(c);
    db.read.repository.close().await;
    db.admin.close().await;
}
#[tokio::test]
#[ignore]
async fn subscription_a2_restart_verify() {
    let file =
        std::path::PathBuf::from(std::env::var("SAFEFLOW_SUBSCRIPTION_A2_EVIDENCE").unwrap())
            .join("restart-seed.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    assert_ne!(saved["pid"], std::process::id());
    let schema = saved["schema"].as_str().unwrap().to_owned();
    assert!(schema.starts_with("sf_subscription_test_") && schema.len() == 53);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect_existing(&url, &schema).await.unwrap();
    let sessions = Arc::new(Sessions::default());
    let session = SessionRef::parse("a2_new_process_owner_read_only").unwrap();
    sessions.bind(&session, "user_demo", 2);
    let db = Db {
        schema,
        admin: PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap(),
        read: SubscriptionService::for_trusted_backend(repo, sessions.clone()),
        sessions,
        session,
    };
    let a = db.assembly();
    // No initial source or host is supplied; this can only inspect saved history.
    let c = db
        .writes(a.runtime())
        .open_isolated_a2_candidate(&scope(&db))
        .await
        .unwrap();
    let o = OperationId::parse(saved["operation"].as_str().unwrap()).unwrap();
    assert_eq!(
        saved["result"],
        json!(c.query(&db.session, &o).await.unwrap())
    );
    assert_eq!(saved["graph"], graph(&db).await);
    db.read
        .original(&db.session, saved["business_ref"].as_str().unwrap())
        .await
        .unwrap();
    let outbox = crate::PublicAuditOutbox::for_trusted_backend(db.read.repository.clone());
    let original = outbox.pending(1000).await.unwrap();
    assert_eq!(original.len(), 1);
    assert_eq!(saved["pending"]["event_id"], json!(original[0].event_id));
    assert_eq!(saved["pending"]["envelope"], original[0].envelope);
    assert_eq!(saved["pending"]["digest"], json!(original[0].digest));
    transport_control("subscription-restart-recovery", "good");
    let batch = sender(&db).send_pending(1000).await.unwrap();
    assert_eq!(batch.len(), 1);
    assert!(batch[0].outcome.is_ok());
    assert!(outbox.pending(1000).await.unwrap().is_empty());
    let after = graph(&db).await;
    for table in [
        "subscription_runtime_records",
        "subscription_public_bindings_v1",
        "public_subscription_a2_links_v1",
        "tasks",
        "operations",
        "execution_permits",
        "audit_events",
    ] {
        assert_eq!(saved["graph"][table], after[table], "{table}");
    }
    save(
        "restart-verified",
        json!({"seed_pid":saved["pid"],"verify_pid":std::process::id(),"exact_saved_result":true,"same_pending_after_restart":true,"original_event_id":original[0].event_id,"digest":original[0].digest,"no_business_reexecution":true,"original_ack_recovered":true,"result":saved["result"]}),
    );
    drop(c);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_a2_transport_existing_sender_strict_ack_and_fixed408() {
    use crate::{AuditSendFailure, PublicAuditOutbox};
    let mut results = vec![];
    for mode in [
        "good",
        "lost-ack",
        "transient",
        "unauthorized",
        "wrong-digest",
        "missing-field",
        "extra-field",
        "wrong-operation",
        "local-ack-fault",
        "conflict",
    ] {
        let db = Db::new().await;
        db.initialize("already-active").await;
        let h = db.read.startup_for_host(&db.session).await.unwrap();
        let a = db.assembly();
        let c = candidate(&db, a.runtime()).await;
        let p = change(&c, &db, &h, Action::RecordCancellation).await;
        let r = run(&c, &db, &h, &p).await;
        let outbox = PublicAuditOutbox::for_trusted_backend(db.read.repository.clone());
        let pending = outbox.pending(1000).await.unwrap();
        let original = pending
            .iter()
            .find(|v| v.event_id.as_str() == r.domain_audit["event_id"].as_str().unwrap())
            .unwrap()
            .clone();
        let case = format!("{mode}:{}", p.operation.operation_id);
        transport_control(&case, mode);
        if mode == "local-ack-fault" {
            db.read.repository.pool.execute("CREATE FUNCTION a2_ack_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'owned ACK failure'; END $$").await.unwrap();
            db.read.repository.pool.execute("CREATE TRIGGER a2_ack_fault BEFORE UPDATE ON public_audit_outbox FOR EACH ROW WHEN(NEW.envelope->'public_event'->>'kind'='SUBSCRIPTION_LOCAL_CHANGE_RECORDED') EXECUTE FUNCTION a2_ack_fault()").await.unwrap();
        }
        let sender = sender(&db);
        let batch = sender.send_pending(1000).await.unwrap();
        let got = batch
            .iter()
            .find(|x| x.event_id == original.event_id)
            .unwrap();
        let expected = match mode {
            "unauthorized" => Err(AuditSendFailure::Http(401)),
            "conflict" => Err(AuditSendFailure::Http(409)),
            "wrong-digest" | "missing-field" | "extra-field" | "wrong-operation" => {
                Err(AuditSendFailure::AckProtocol)
            }
            "local-ack-fault" => Err(AuditSendFailure::Acknowledge),
            _ => Ok(()),
        };
        assert_eq!(got.outcome, expected, "{mode}");
        assert_eq!(
            got.attempts,
            if matches!(mode, "lost-ack" | "transient") {
                2
            } else {
                1
            }
        );
        if expected.is_err() {
            let pending = outbox.pending(1000).await.unwrap();
            assert_eq!(
                pending.iter().find(|x| x.event_id == original.event_id),
                Some(&original)
            );
            if mode == "local-ack-fault" {
                db.read
                    .repository
                    .pool
                    .execute("DROP TRIGGER a2_ack_fault ON public_audit_outbox")
                    .await
                    .unwrap();
            }
            if mode != "conflict" {
                transport_control(&case, "good");
                let retry = sender.send_pending(1000).await.unwrap();
                assert_eq!(retry.len(), 1);
                assert!(retry[0].outcome.is_ok());
            }
        }
        assert_eq!(
            json!(r),
            json!(c
                .query(&db.session, &p.operation.operation_id)
                .await
                .unwrap())
        );
        results.push(json!({"mode":mode,"event_id":original.event_id,"envelope":original.envelope,"digest":original.digest,"attempts":got.attempts,"outcome":format!("{:?}",got.outcome),"formal_receiver_validator":false}));
        drop(c);
        db.finish().await;
    }
    save("sender-fixed408", json!(results));
}
async fn candidate<'a>(db: &Db, runtime: &'a Runtime) -> SubscriptionA2Candidate<'a> {
    registration_tests::registration_writes(db, runtime)
        .into_isolated_a2_candidate(&scope(db))
        .await
        .unwrap()
}
async fn change(
    c: &SubscriptionA2Candidate<'_>,
    db: &Db,
    h: &SubscriptionHost,
    a: Action,
) -> SubscriptionPreparation {
    c.prepare_change(
        &db.session,
        h,
        &uuid::Uuid::new_v4().to_string(),
        request(a),
    )
    .await
    .unwrap()
}
async fn run(
    c: &SubscriptionA2Candidate<'_>,
    db: &Db,
    h: &SubscriptionHost,
    p: &SubscriptionPreparation,
) -> SubscriptionA2Result {
    c.confirm(
        &db.session,
        &p.task.task_id,
        &p.confirmation.confirmation_ref,
    )
    .await
    .unwrap();
    c.execute(
        &db.session,
        h,
        &p.task.task_id,
        &p.operation.operation_id,
        &p.confirmation.confirmation_ref,
    )
    .await
    .unwrap()
}
fn save(name: &str, v: Value) {
    let d = std::env::var("SAFEFLOW_SUBSCRIPTION_A2_EVIDENCE").unwrap();
    std::fs::write(
        format!("{d}/{name}.json"),
        serde_json::to_vec_pretty(&v).unwrap(),
    )
    .unwrap();
}
#[test]
fn subscription_a2_strict_time_and_encoding() {
    for s in [
        "2024-02-29T23:59:59Z",
        "2026-10-25T12:02:03.123456789+08:00",
        "2026-10-25t12:00:00z",
    ] {
        assert!(valid_business_time(s), "{s}");
    }
    for s in [
        "",
        "0000-01-01T00:00:00Z",
        "2023-02-29T12:00:00Z",
        "2026-10-25T24:00:00Z",
        "2026-10-25T23:59:60Z",
        "2026-10-25T12:00:00",
        "2026-10-25T12:00:00+0800",
        "2026-10-25T12:00:00Z\n",
        "2026-10-25T12:00:00.Z",
        "2026-10-25T12:00:00+24:00",
    ] {
        assert!(!valid_business_time(s), "{s}");
    }
    let good = json!({"mapping_version":"safeflow.subscription.local-result.v1","action":"REGISTRATION","result":"ACTIVE","business_ref":"12345678-1111-2222-3333-123456789abc","business_occurred_at":"2026-10-25T12:00:00Z","business_time_source":"FICTIONAL_COMMAND_NOW","fictional":true,"production_registered":false,"notification_sent":false});
    checked_details(&good).unwrap();
    for k in good.as_object().unwrap().keys() {
        let mut bad = good.clone();
        bad.as_object_mut().unwrap().remove(k);
        assert!(checked_details(&bad).is_err(), "missing {k}");
        bad = good.clone();
        bad[k] = Value::Null;
        assert!(checked_details(&bad).is_err(), "null {k}");
    }
    let mut bad = good.clone();
    bad["private"] = json!("forbidden");
    assert!(checked_details(&bad).is_err());
    for (a, r) in [
        ("REGISTRATION", "ACTIVE"),
        ("REMINDER_SETTINGS", "SETTINGS_SAVED"),
        ("RECORD_CANCELLATION", "RECORDED"),
        ("REVOKE_CANCELLATION", "REVOKED"),
    ] {
        let mut v = good.clone();
        v["action"] = json!(a);
        v["result"] = json!(r);
        checked_details(&v).unwrap();
        v["result"] = json!("SUCCESS");
        assert!(checked_details(&v).is_err());
    }
}
#[tokio::test]
#[ignore]
async fn subscription_a2_four_actions_real_commit_independent_query_and_replay() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let c = candidate(&db, assembly.runtime()).await;
    let (_, request, source) = crate::billing_fixture::fixture("complete").unwrap();
    let capture = c
        .capture_registration(&db.session, &h, &request, &source, Default::default())
        .await
        .unwrap();
    let selected = c
        .select_registration_from_capture(&db.session, &h, capture, 0)
        .await
        .unwrap();
    let p = c
        .prepare_registration(
            &db.session,
            &h,
            &uuid::Uuid::new_v4().to_string(),
            &selected,
        )
        .await
        .unwrap();
    let mut results = vec![];
    let mut subscription = String::new();
    for (a, p) in [
        (Action::Registration, Some(p)),
        (Action::ReminderSettings, None),
        (Action::RecordCancellation, None),
        (Action::RevokeCancellation, None),
    ] {
        let p = match p {
            Some(p) => p,
            None => {
                let r = request_for(a, &subscription);
                c.prepare_change(&db.session, &h, &uuid::Uuid::new_v4().to_string(), r)
                    .await
                    .unwrap()
            }
        };
        let before = db.facts().await;
        assert!(c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .is_err());
        assert_eq!(before, db.facts().await);
        let result = run(&c, &db, &h, &p).await;
        assert_eq!(result.domain_audit["status"], "VERIFIED_CANDIDATE_V1");
        checked_details(&result.domain_audit["details"]).unwrap();
        assert_eq!(result.domain_audit["details"]["action"], json!(a));
        let after = db.facts().await;
        let nonce: String = sqlx::query_scalar(
            "SELECT nonce FROM subscription_preparations_v1 WHERE business_ref=$1",
        )
        .bind(
            result.domain_audit["details"]["business_ref"]
                .as_str()
                .unwrap(),
        )
        .fetch_one(&db.read.repository.pool)
        .await
        .unwrap();
        assert_eq!(
            c.recover_preparation(&db.session, &nonce).await.err(),
            Some(StorageError::Terminal)
        );
        let identity = db.read.identity(&db.session).unwrap();
        let aggregate = db
            .read
            .repository
            .recover(&identity.user_id, &p.task.task_id, &|| {
                db.read.check(&db.session, &identity)
            })
            .await
            .unwrap();
        assert!(aggregate
            .audit
            .iter()
            .any(|v| v.kind == PersistentAuditKind::SubscriptionLocalChangeRecorded));
        let replay = c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
            .await
            .unwrap();
        assert_eq!(json!(result), json!(replay));
        assert_eq!(after, db.facts().await);
        assert_eq!(
            json!(result),
            json!(c
                .query(&db.session, &p.operation.operation_id)
                .await
                .unwrap())
        );
        // Ordinary original read independently validates the same saved link.
        db.read
            .original(
                &db.session,
                result.domain_audit["details"]["business_ref"]
                    .as_str()
                    .unwrap(),
            )
            .await
            .unwrap();
        if a == Action::Registration {
            let j = db.journal().await;
            subscription = j["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["kind"] == "Register")
                .unwrap()["receipt"]["subscription_id"]
                .as_str()
                .unwrap()
                .into();
        }
        results.push(json!({"operation":p.operation.operation_id,"result":result}));
    }
    let f = db.facts().await;
    let events: Vec<_> = f["audit_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "SUBSCRIPTION_LOCAL_CHANGE_RECORDED")
        .collect();
    assert_eq!(events.len(), 4);
    for e in &events {
        assert_eq!(e.as_object().unwrap().len(), 9);
        assert_eq!(e["details"].as_object().unwrap().len(), 9);
        let raw = e["details"].to_string();
        for private in [
            "subscription_id",
            "session_ref",
            "member_binding",
            "requested_account_ids",
            "message",
            "settings_revision",
        ] {
            assert!(!raw.contains(private));
        }
    }
    // D1/D2: previous settings/register/cancellation events must survive later actions.
    for r in &results {
        let o = OperationId::parse(r["operation"].as_str().unwrap()).unwrap();
        assert_eq!(r["result"], json!(c.query(&db.session, &o).await.unwrap()));
    }
    // D2: unchanged member reservation -> committed capability -> mock sink ->
    // committed result. No fixture/Journal codec rewrite, no real notification.
    let store = db.read.store().unwrap();
    let mut writer = db.read.repository.pool.acquire().await.unwrap();
    use sqlx::Connection;
    let mut tx = writer.begin().await.unwrap();
    let mut work = store
        .load_work_on_connection_mock(&mut tx, "user_demo")
        .await
        .unwrap();
    work.reserve_reminders_mock(&now()).await.unwrap();
    let pending = store.save_on_connection_mock(&mut tx, work).await.unwrap();
    tx.commit().await.unwrap();
    let mut independent = db
        .read
        .repository
        .subscription_read_pool
        .acquire()
        .await
        .unwrap();
    let batch = pending
        .into_committed_reminder_batch_mock(&mut independent)
        .await
        .unwrap();
    let mut sink = SimulatedOnly(0);
    let simulated = batch
        .simulate_on_connection_mock(&mut independent, &mut sink)
        .await
        .unwrap();
    assert!(sink.0 > 0);
    let mut tx = writer.begin().await.unwrap();
    let mut work = store
        .load_work_on_connection_mock(&mut tx, "user_demo")
        .await
        .unwrap();
    work.record_reminder_results_mock(simulated).await.unwrap();
    let pending = store.save_on_connection_mock(&mut tx, work).await.unwrap();
    tx.commit().await.unwrap();
    pending
        .read_committed_on_connection_mock(&mut independent)
        .await
        .unwrap();
    drop(independent);
    drop(writer);
    // D1: a later saved setting differs from original; query must use historical O.
    let mut later = request_for(Action::ReminderSettings, &subscription);
    if let SubscriptionChangeRequestMock::UpdateReminderSettings { schedule, .. } = &mut later {
        schedule.enabled = false;
    }
    let p = c
        .prepare_change(&db.session, &h, &uuid::Uuid::new_v4().to_string(), later)
        .await
        .unwrap();
    run(&c, &db, &h, &p).await;
    for r in &results {
        let o = OperationId::parse(r["operation"].as_str().unwrap()).unwrap();
        assert_eq!(r["result"], json!(c.query(&db.session, &o).await.unwrap()));
    }
    save(
        "four-actions",
        json!({"results":results,"first_four_facts":f,"final_facts":graph(&db).await,"journal":db.journal().await,"simulated_attempts":sink.0,"later_settings_changed":true}),
    );
    drop(c);
    db.finish().await;
}
struct SimulatedOnly(usize);
impl safeflow_subscription_evidence::reminder_mock::MockInAppSink for SimulatedOnly {
    fn simulate_in_app(
        &mut self,
        _: &safeflow_subscription_evidence::reminder_mock::ReminderMessage,
    ) -> safeflow_subscription_evidence::reminder_mock::DeliveryOutcome {
        self.0 += 1;
        safeflow_subscription_evidence::reminder_mock::DeliveryOutcome::SimulatedSuccess
    }
}
fn request_for(a: Action, id: &str) -> SubscriptionChangeRequestMock {
    match request(a) {
        SubscriptionChangeRequestMock::CreateCancellationIntent { .. } => {
            SubscriptionChangeRequestMock::CreateCancellationIntent {
                subscription_id: id.into(),
            }
        }
        SubscriptionChangeRequestMock::RevokeCancellationIntent { .. } => {
            SubscriptionChangeRequestMock::RevokeCancellationIntent {
                subscription_id: id.into(),
            }
        }
        SubscriptionChangeRequestMock::UpdateReminderSettings { mut schedule, .. } => {
            // Actual public request parameters for this run, not edits to member fixture:
            // current-day schedule lets its real mock delivery algorithm produce results.
            let today = chrono::Utc::now()
                .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                .date_naive();
            schedule.anchor_date = today.checked_sub_months(chrono::Months::new(1)).unwrap();
            schedule.expected_date = today;
            schedule.period_index = 1;
            schedule.days_before = 0;
            SubscriptionChangeRequestMock::UpdateReminderSettings {
                subscription_id: id.into(),
                schedule,
            }
        }
    }
}
async fn graph(db: &Db) -> Value {
    let mut f = db.facts().await;
    for table in [
        "public_subscription_a2_sources_v1",
        "public_subscription_a2_legacy_v1",
        "public_subscription_a2_links_v1",
    ] {
        f[table]=sqlx::query_scalar::<_,Value>(&format!("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t")).fetch_one(&db.read.repository.pool).await.unwrap();
    }
    f
}
#[tokio::test]
#[ignore]
async fn subscription_a2_stage_faults_rollback_entire_graph() {
    let mut rows = vec![];
    for (name, table, when, condition) in [
        (
            "business_save",
            "subscription_runtime_records",
            "UPDATE",
            "true",
        ),
        ("submission", "public_execution_requests", "INSERT", "true"),
        (
            "public_result",
            "operations",
            "UPDATE",
            "NEW.state='SUCCEEDED'",
        ),
        ("terminal_task", "tasks", "UPDATE", "NEW.state='COMPLETED'"),
        (
            "domain_event",
            "audit_events",
            "INSERT",
            "NEW.kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'",
        ),
        (
            "domain_link",
            "public_subscription_a2_links_v1",
            "INSERT",
            "true",
        ),
        (
            "domain_outbox",
            "public_audit_outbox",
            "INSERT",
            "NEW.envelope->'public_event'->>'kind'='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'",
        ),
    ] {
        let db = Db::new().await;
        db.initialize("already-active").await;
        let h = db.read.startup_for_host(&db.session).await.unwrap();
        let a = db.assembly();
        let c = candidate(&db, a.runtime()).await;
        let p = change(&c, &db, &h, Action::RecordCancellation).await;
        c.confirm(
            &db.session,
            &p.task.task_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
        let before = graph(&db).await;
        db.read.repository.pool.execute("CREATE FUNCTION a2_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'a2 owned rollback fault'; END $$").await.unwrap();
        db.read.repository.pool.execute(format!("CREATE TRIGGER a2_fault BEFORE {when} ON {table} FOR EACH ROW WHEN ({condition}) EXECUTE FUNCTION a2_fault()").as_str()).await.unwrap();
        let error = c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(before, graph(&db).await, "{name}");
        db.read
            .repository
            .pool
            .execute(format!("DROP TRIGGER a2_fault ON {table}").as_str())
            .await
            .unwrap();
        let result = c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
            .await
            .unwrap();
        rows.push(json!({"stage":name,"error":format!("{error:?}"),"restored_original":true,"result":result}));
        drop(c);
        db.finish().await;
    }
    save("rollback-stages", json!(rows));
}
#[tokio::test]
#[ignore]
async fn subscription_a2_default_gates_and_candidate_scope_owner_generation() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let a = db.assembly();
    let w = db.writes(a.runtime());
    assert!(SubscriptionA2CandidateScope::for_isolated_backend(
        "public",
        UserId::parse("user_demo").unwrap(),
        "r1"
    )
    .is_err());
    let wrong = SubscriptionA2CandidateScope::for_isolated_backend(
        "sf_subscription_test_wrong",
        UserId::parse("user_demo").unwrap(),
        "r1",
    )
    .unwrap();
    assert!(db
        .read
        .repository
        .install_subscription_a2_candidate_for_isolated_backend(&wrong)
        .await
        .is_err());
    let before = graph(&db).await;
    let nonce = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        w.prepare_change(&db.session, &h, &nonce, request(Action::RecordCancellation))
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(
        w.recover_preparation(&db.session, &nonce).await.err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(before, graph(&db).await);
    let c = candidate(&db, a.runtime()).await;
    let p = change(&c, &db, &h, Action::RecordCancellation).await;
    let prepared = graph(&db).await;
    assert_eq!(
        w.confirm(
            &db.session,
            &p.task.task_id,
            &p.confirmation.confirmation_ref
        )
        .await
        .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(
        w.execute(
            &db.session,
            &h,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref
        )
        .await
        .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(prepared, graph(&db).await);
    let other = OperationId::parse("foreign-original").unwrap();
    assert!(c.query(&db.session, &other).await.is_err());
    db.sessions.bind(&db.session, "other_owner", 2);
    assert!(c
        .query(&db.session, &p.operation.operation_id)
        .await
        .is_err());
    db.sessions.bind(&db.session, "user_demo", 1);
    let other_revision = SubscriptionA2CandidateScope::for_isolated_backend(
        &db.schema,
        UserId::parse("user_demo").unwrap(),
        "different-generation",
    )
    .unwrap();
    assert!(registration_tests::registration_writes(&db, a.runtime())
        .open_isolated_a2_candidate(&other_revision)
        .await
        .is_err());
    c.confirm(
        &db.session,
        &p.task.task_id,
        &p.confirmation.confirmation_ref,
    )
    .await
    .unwrap();
    let h2 = db.read.startup_for_host(&db.session).await.unwrap();
    let before = graph(&db).await;
    assert!(c
        .execute(
            &db.session,
            &h,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref
        )
        .await
        .is_err());
    assert_eq!(before, graph(&db).await);
    // A new host cannot adopt an old prepared binding either. Reprepare explicitly.
    assert!(c
        .execute(
            &db.session,
            &h2,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref
        )
        .await
        .is_err());
    assert_eq!(before, graph(&db).await);
    drop(c);
    db.finish().await;
}
fn fence_after_save<'t>(
    u: &'t mut PermitTransaction,
) -> ContractFuture<'t, Result<(), StorageError>> {
    Box::pin(async move {
        sqlx::query("UPDATE subscription_host_generations_v1 SET generation=generation+1")
            .execute(u.connection()?)
            .await?;
        Ok(())
    })
}
#[tokio::test]
#[ignore]
async fn subscription_a2_final_host_guard_and_concurrent_original() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let a = db.assembly();
    let mut w = registration_tests::registration_writes(&db, a.runtime());
    w.stage_observer = Some(fence_after_save);
    let c = w.into_isolated_a2_candidate(&scope(&db)).await.unwrap();
    let p = change(&c, &db, &h, Action::RecordCancellation).await;
    c.confirm(
        &db.session,
        &p.task.task_id,
        &p.confirmation.confirmation_ref,
    )
    .await
    .unwrap();
    let before = graph(&db).await;
    assert!(c
        .execute(
            &db.session,
            &h,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref
        )
        .await
        .is_err());
    assert_eq!(before, graph(&db).await);
    drop(c);
    let c = registration_tests::registration_writes(&db, a.runtime())
        .open_isolated_a2_candidate(&scope(&db))
        .await
        .unwrap();
    let exec = || {
        c.execute(
            &db.session,
            &h,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
    };
    let (x, y, z) = tokio::join!(exec(), exec(), exec());
    let x = x.unwrap();
    assert_eq!(json!(x), json!(y.unwrap()));
    assert_eq!(json!(x), json!(z.unwrap()));
    let after = graph(&db).await;
    assert_eq!(
        after["public_subscription_a2_links_v1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Later original session generation is not a new write authorisation.
    db.sessions.bind(&db.session, "user_demo", 2);
    assert!(exec().await.is_err());
    assert_eq!(after, graph(&db).await);
    assert_eq!(
        json!(x),
        json!(c
            .query(&db.session, &p.operation.operation_id)
            .await
            .unwrap())
    );
    save(
        "concurrency-and-host",
        json!({"result":x,"links":after["public_subscription_a2_links_v1"],"original_reads_after_session_renew":true}),
    );
    drop(c);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_a2_sql_tamper_orphan_null_time_and_outbox_protection() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let a = db.assembly();
    let c = candidate(&db, a.runtime()).await;
    let p = change(&c, &db, &h, Action::RecordCancellation).await;
    let result = run(&c, &db, &h, &p).await;
    let before = graph(&db).await;
    let event = result.domain_audit["event_id"].as_str().unwrap();
    let mutations=[
  "UPDATE public_subscription_a2_links_v1 SET business_ref='11111111-1111-1111-1111-111111111111'",
  "UPDATE public_subscription_a2_sources_v1 SET source_revision='unadopted'",
  "UPDATE subscription_public_bindings_v1 SET owner_id='other'",
  "DELETE FROM public_subscription_a2_links_v1",
  "DELETE FROM public_audit_outbox WHERE envelope->'public_event'->>'kind'='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'",
  "UPDATE audit_events SET details=jsonb_set(details,'{notification_sent}','true') WHERE kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'",
  "UPDATE tasks SET revision=revision+1",
  "UPDATE execution_permits SET consumed_at_ms=consumed_at_ms+1",
  "UPDATE public_audit_outbox SET historical=true WHERE envelope->'public_event'->>'kind'='SUBSCRIPTION_LOCAL_CHANGE_RECORDED'",
  "UPDATE subscription_runtime_records SET journal=jsonb_set(journal,'{commands,0}','{}')",
 ];
    for sql in mutations {
        let mut tx = db.read.repository.pool.begin().await.unwrap();
        let update = tx.execute(sql).await;
        let rejected = if update.is_err() {
            tx.rollback().await.unwrap();
            true
        } else {
            tx.commit().await.is_err()
        };
        assert!(rejected, "{sql}");
        assert_eq!(before, graph(&db).await);
    }
    let good = result.domain_audit["details"].clone();
    for k in good.as_object().unwrap().keys() {
        let mut v = good.clone();
        v[k] = Value::Null;
        let accepted: bool = sqlx::query_scalar("SELECT subscription_a2_details_v1($1)")
            .bind(v)
            .fetch_one(&db.read.repository.pool)
            .await
            .unwrap();
        assert!(!accepted, "null {k}");
    }
    for stamp in [
        "2023-02-29T12:00:00Z",
        "2026-10-25T24:00:00Z",
        "2026-10-25T23:59:60Z",
        "2026-10-25T12:00:00Z\n",
        "0000-01-01T00:00:00Z",
    ] {
        let mut v = good.clone();
        v["business_occurred_at"] = json!(stamp);
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT subscription_a2_details_v1($1)")
                .bind(v)
                .fetch_one(&db.read.repository.pool)
                .await
                .unwrap()
        );
    }
    let outbox = crate::PublicAuditOutbox::for_trusted_backend(db.read.repository.clone());
    let pending = outbox.pending(1000).await.unwrap();
    let item = pending
        .iter()
        .find(|v| v.event_id.as_str() == event)
        .unwrap();
    outbox
        .acknowledge(
            &item.event_id,
            &item.digest,
            "isolated-test-receiver",
            "receipt-original",
        )
        .await
        .unwrap();
    outbox
        .acknowledge(
            &item.event_id,
            &item.digest,
            "isolated-test-receiver",
            "receipt-original",
        )
        .await
        .unwrap();
    assert!(outbox
        .acknowledge(
            &item.event_id,
            &item.digest,
            "isolated-test-receiver",
            "different-receipt"
        )
        .await
        .is_err());
    assert_eq!(
        json!(result),
        json!(c
            .query(&db.session, &p.operation.operation_id)
            .await
            .unwrap())
    );
    save(
        "tamper-and-local-ack",
        json!({"rejected_sql":mutations,"null_keys_rejected":9,"local_ack_only":true}),
    );
    drop(c);
    db.finish().await;
}

#[tokio::test]
#[ignore = "explicit audit744 owned PG seed; retain schema for original sender only"]
async fn audit744_seed_four_original_operations() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let h = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let c = candidate(&db, assembly.runtime()).await;
    let (_, request, source) = crate::billing_fixture::fixture("complete").unwrap();
    let capture = c
        .capture_registration(&db.session, &h, &request, &source, Default::default())
        .await
        .unwrap();
    let selected = c
        .select_registration_from_capture(&db.session, &h, capture, 0)
        .await
        .unwrap();
    let p = c
        .prepare_registration(
            &db.session,
            &h,
            &uuid::Uuid::new_v4().to_string(),
            &selected,
        )
        .await
        .unwrap();
    let mut results = vec![];
    let mut subscription = String::new();
    for (a, p) in [
        (Action::Registration, Some(p)),
        (Action::ReminderSettings, None),
        (Action::RecordCancellation, None),
        (Action::RevokeCancellation, None),
    ] {
        let p = match p {
            Some(p) => p,
            None => {
                let r = request_for(a, &subscription);
                c.prepare_change(&db.session, &h, &uuid::Uuid::new_v4().to_string(), r)
                    .await
                    .unwrap()
            }
        };
        let before = db.facts().await;
        assert!(c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .is_err());
        assert_eq!(before, db.facts().await);
        let result = run(&c, &db, &h, &p).await;
        assert_eq!(result.domain_audit["status"], "VERIFIED_CANDIDATE_V1");
        checked_details(&result.domain_audit["details"]).unwrap();
        assert_eq!(result.domain_audit["details"]["action"], json!(a));
        let after = db.facts().await;
        let nonce: String = sqlx::query_scalar(
            "SELECT nonce FROM subscription_preparations_v1 WHERE business_ref=$1",
        )
        .bind(
            result.domain_audit["details"]["business_ref"]
                .as_str()
                .unwrap(),
        )
        .fetch_one(&db.read.repository.pool)
        .await
        .unwrap();
        assert_eq!(
            c.recover_preparation(&db.session, &nonce).await.err(),
            Some(StorageError::Terminal)
        );
        let identity = db.read.identity(&db.session).unwrap();
        let aggregate = db
            .read
            .repository
            .recover(&identity.user_id, &p.task.task_id, &|| {
                db.read.check(&db.session, &identity)
            })
            .await
            .unwrap();
        assert!(aggregate
            .audit
            .iter()
            .any(|v| v.kind == PersistentAuditKind::SubscriptionLocalChangeRecorded));
        let replay = c
            .execute(
                &db.session,
                &h,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
            .await
            .unwrap();
        assert_eq!(json!(result), json!(replay));
        assert_eq!(after, db.facts().await);
        assert_eq!(
            json!(result),
            json!(c
                .query(&db.session, &p.operation.operation_id)
                .await
                .unwrap())
        );
        // Ordinary original read independently validates the same saved link.
        db.read
            .original(
                &db.session,
                result.domain_audit["details"]["business_ref"]
                    .as_str()
                    .unwrap(),
            )
            .await
            .unwrap();
        if a == Action::Registration {
            let j = db.journal().await;
            subscription = j["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["kind"] == "Register")
                .unwrap()["receipt"]["subscription_id"]
                .as_str()
                .unwrap()
                .into();
        }
        results.push(json!({"operation":p.operation.operation_id,"result":result}));
    }
    let f = db.facts().await;
    let events: Vec<_> = f["audit_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "SUBSCRIPTION_LOCAL_CHANGE_RECORDED")
        .collect();
    assert_eq!(events.len(), 4);
    for e in &events {
        assert_eq!(e.as_object().unwrap().len(), 9);
        assert_eq!(e["details"].as_object().unwrap().len(), 9);
        let raw = e["details"].to_string();
        for private in [
            "subscription_id",
            "session_ref",
            "member_binding",
            "requested_account_ids",
            "message",
            "settings_revision",
        ] {
            assert!(!raw.contains(private));
        }
    }
    // D1/D2: previous settings/register/cancellation events must survive later actions.
    for r in &results {
        let o = OperationId::parse(r["operation"].as_str().unwrap()).unwrap();
        assert_eq!(r["result"], json!(c.query(&db.session, &o).await.unwrap()));
    }

    let outbox = crate::PublicAuditOutbox::for_trusted_backend(db.read.repository.clone());
    let pending = outbox.pending(1000).await.unwrap();
    assert_eq!(pending.iter().filter(|p| p.envelope["public_event"]["kind"] == "SUBSCRIPTION_LOCAL_CHANGE_RECORDED").count(), 4);
    let seed = json!({"pid":std::process::id(),"schema":db.schema,"results":results,"graph":graph(&db).await});
    let file = std::env::var("AUD744_SEED").unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).open(file).unwrap().write_all(&serde_json::to_vec_pretty(&seed).unwrap()).unwrap();
    drop(c);
    db.read.repository.close().await;
    db.admin.close().await;
}
