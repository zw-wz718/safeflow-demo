//! Public candidate tests use the unchanged original member fixture and actual
//! read->prepare->confirmation->member SAVEPOINT->public completion path.
use super::*;
use safeflow_storage::{
    BillingDomainAuditInstallation, BillingDomainAuditStatus, PublicAuditOutbox,
};
use sha2::{Digest, Sha256};

#[path = "step09_original_seed.rs"]
mod step09_original_seed;

async fn install(db: &Db, original: &Value) -> String {
    db.repo
        .install_billing_domain_audit_for_reviewed_backend(
            &BillingDomainAuditInstallation::for_reviewed_backend(
                &db.namespace,
                "owned-loopback-pg-59447",
                intent(original).transaction.source_id.as_str(),
                "8ff8c665:mixed",
            )
            .unwrap(),
        )
        .await
        .unwrap()
}
async fn domain_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE kind='BILLING_FEEDBACK_SAVED'")
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}
async fn all_counts(db: &Db) -> Vec<i64> {
    let mut v = snapshot(db).await;
    v.push(count(db, "public_billing_audit_links_v1").await);
    v.push(domain_count(db).await);
    v
}
fn evidence(name: &str, value: Value) {
    if let Ok(root) = std::env::var("SAFEFLOW_DOMAIN_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&root).join(name),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "owned PostgreSQL; real three-choice composition, original envelope/digest and readers"]
async fn billing_domain_three_choices_atomic_original_replay_readers_and_digest() {
    for choice in [
        FeedbackChoice::Mine,
        FeedbackChoice::NotMine,
        FeedbackChoice::Unsure,
    ] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, source, cookie, complete) = report(&db, &provider).await;
        let original = &complete["results"][0]["data"]["report"];
        assert_eq!(count(&db, "public_billing_audit_sources_v1").await, 0);
        install(&db, original).await;
        let f = service(&server);
        let s = session(&cookie);
        let mut i = intent(original);
        i.choice = choice.clone();
        let p = f.prepare(&s, i.clone()).await.unwrap();
        assert!(f
            .query(&s, &p.operation.operation_id)
            .await
            .unwrap()
            .is_none());
        assert!(f
            .execute(
                &s,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .is_err());
        assert_eq!(domain_count(&db).await, 0);
        f.confirm(&s, &p).await.unwrap();
        let run = || {
            f.execute(
                &s,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
        };
        let (a, b, c) = tokio::join!(run(), run(), run());
        let views = [a.unwrap(), b.unwrap(), c.unwrap()];
        assert_eq!(views.iter().filter(|v| !v.replayed).count(), 1);
        assert!(views
            .iter()
            .all(|v| v.domain_audit == BillingDomainAuditStatus::VerifiedV1));
        assert_eq!(views[0].saved["lock_authorized"], false);
        assert_eq!(domain_count(&db).await, 1);
        assert_eq!(count(&db, "public_billing_audit_links_v1").await, 1);
        // Same composite transaction under a NEW O stays D20 closed.
        let second = f.prepare(&s, i).await.unwrap();
        f.confirm(&s, &second).await.unwrap();
        assert!(matches!(
            f.execute(
                &s,
                &second.task.task_id,
                &second.operation.operation_id,
                &second.confirmation.confirmation_ref
            )
            .await,
            Err(StorageError::ExecutionOperationConflict)
        ));
        assert_eq!(domain_count(&db).await, 1);
        let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
        let pending = outbox
            .pending(1000)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.envelope["public_event"]["kind"] == "BILLING_FEEDBACK_SAVED")
            .unwrap();
        let exact = json!({"mapping_version":"billing-feedback-saved-v1","schema_version":1,"action":"SAVE_BILLING_FEEDBACK","result":"SAVED","lock_authorized":false});
        assert_eq!(pending.envelope["public_event"]["details"], exact);
        assert_eq!(
            pending.envelope["public_event"].as_object().unwrap().len(),
            9
        );
        assert_eq!(pending.envelope.as_object().unwrap().len(), 3);
        let independent = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&pending.envelope).unwrap())
        );
        assert_eq!(independent, pending.digest.as_str());
        let read = safeflow_storage::PersistenceService::for_trusted_backend(
            db.repo.clone(),
            server.state.0.sessions.clone(),
        )
        .recover(&s, &p.task.task_id)
        .await;
        // Reader below uses the public backend factory, not a handcrafted event DTO.
        assert!(read.is_ok());
        assert!(read
            .unwrap()
            .audit
            .iter()
            .any(|e| e.kind == safeflow_storage::PersistentAuditKind::BillingFeedbackSaved));
        evidence(
            &format!("choice-{choice:?}.json"),
            json!({"pid":std::process::id(),"namespace":db.namespace,
          "saved":views[0],"envelope":pending.envelope,"digest":independent,"event_id":pending.event_id,
          "historical":pending.historical,"enqueued_at":pending.enqueued_at,"counts":all_counts(&db).await}),
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "owned PostgreSQL; event/link/outbox/deferred error must roll back entire feedback"]
async fn billing_domain_faults_roll_back_every_success_row() {
    for stage in [
        "member",
        "result",
        "event",
        "link",
        "outbox",
        "deferred",
        "skip-link",
        "wrong-link-owner",
        "wrong-link-source",
    ] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        install(&db, &complete["results"][0]["data"]["report"]).await;
        let f = service(&server);
        let s = session(&cookie);
        let p = f
            .prepare(&s, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        f.confirm(&s, &p).await.unwrap();
        let before = all_counts(&db).await;
        let function=match stage{
            "skip-link"=>"CREATE FUNCTION domain_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
            "wrong-link-owner"=>"CREATE FUNCTION domain_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.owner_id='different_owner'; RETURN NEW; END $$",
            "wrong-link-source"=>"CREATE FUNCTION domain_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.source_binding_id='00000000-0000-4000-8000-000000000001'::uuid; RETURN NEW; END $$",
            _=>"CREATE FUNCTION domain_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'owned domain fault'; END $$"};
        db.scoped.execute(function).await.unwrap();
        let (table, when) = match stage {
            "member" => ("billing_feedback_events_v1", ""),
            "result" => ("public_billing_feedback_results_v1", ""),
            "event" => ("audit_events", "WHEN (NEW.kind='BILLING_FEEDBACK_SAVED')"),
            "outbox" => (
                "public_audit_outbox",
                "WHEN (NEW.envelope->'public_event'->>'kind'='BILLING_FEEDBACK_SAVED')",
            ),
            _ => ("public_billing_audit_links_v1", ""),
        };
        let ddl = if stage == "deferred" {
            format!("CREATE CONSTRAINT TRIGGER domain_fault AFTER INSERT ON {table} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION domain_fault()")
        } else {
            format!("CREATE TRIGGER domain_fault BEFORE INSERT ON {table} FOR EACH ROW {when} EXECUTE FUNCTION domain_fault()")
        };
        db.scoped.execute(ddl.as_str()).await.unwrap();
        assert!(
            f.execute(
                &s,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .is_err(),
            "{stage}"
        );
        assert_eq!(all_counts(&db).await, before, "{stage}");
        assert_ready(&db, &p.operation.operation_id).await;
        db.scoped
            .execute(format!("DROP TRIGGER domain_fault ON {table}").as_str())
            .await
            .unwrap();
        assert_eq!(
            f.execute(
                &s,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .unwrap()
            .domain_audit,
            BillingDomainAuditStatus::VerifiedV1
        );
        evidence(
            &format!("fault-{stage}.json"),
            json!({"rollback_counts":before,"same_o_after_known_rollback":p.operation.operation_id,"after":all_counts(&db).await}),
        );
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "owned PG; exclusive installation waits for actual in-flight writer then exact legacy enumeration"]
async fn billing_domain_installation_drains_inflight_writer_before_cutover() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let f = service(&server);
    let s = session(&cookie);
    let p = f.prepare(&s, intent(original)).await.unwrap();
    f.confirm(&s, &p).await.unwrap();
    let mut blocker = install_wait(&db).await;
    let mut writing = Box::pin(f.execute(
        &s,
        &p.task.task_id,
        &p.operation.operation_id,
        &p.confirmation.confirmation_ref,
    ));
    tokio::select! {r=&mut writing=>panic!("no writer barrier {:?}",r.err()),_=wait_for_stage(&db)=>{}}
    let config = BillingDomainAuditInstallation::for_reviewed_backend(
        &db.namespace,
        "owned-loopback-pg-59447",
        &intent(original).transaction.source_id,
        "8ff8c665:mixed",
    )
    .unwrap();
    let mut installing = Box::pin(
        db.repo
            .install_billing_domain_audit_for_reviewed_backend(&config),
    );
    tokio::select! {r=&mut installing=>panic!("installation passed active writer {:?}",r),_=wait_for_query(&db,"SELECT pg_advisory_xact_lock(1680000028%")=>{}}
    assert_eq!(count(&db, "public_billing_audit_sources_v1").await, 0);
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    let (saved, installed) = tokio::join!(writing, installing);
    saved.unwrap();
    installed.unwrap();
    assert_eq!(domain_count(&db).await, 0);
    assert_eq!(count(&db, "public_billing_audit_legacy_v1").await, 1);
    assert_eq!(
        f.query(&s, &p.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            .domain_audit,
        BillingDomainAuditStatus::LegacyWithoutDomainEvent
    );
    evidence(
        "installation-drain.json",
        json!({"original_O":p.operation.operation_id,"waited_for_actual_writer":true,"exact_legacy_count":1,"no_backfill":true}),
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "owned PostgreSQL; exact per-O legacy installation and default-off compatibility"]
async fn billing_domain_legacy_exact_enumeration_no_backfill_and_source_spoof_refused() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let f = service(&server);
    let s = session(&cookie);
    let p = f.prepare(&s, intent(original)).await.unwrap();
    f.confirm(&s, &p).await.unwrap();
    let v = f
        .execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    assert_eq!(v.domain_audit, BillingDomainAuditStatus::NotActivated);
    assert_eq!(domain_count(&db).await, 0);
    let id = install(&db, original).await;
    let legacy = f
        .query(&s, &p.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        legacy.domain_audit,
        BillingDomainAuditStatus::LegacyWithoutDomainEvent
    );
    assert_eq!(legacy.saved, v.saved);
    assert_eq!(domain_count(&db).await, 0);
    assert_eq!(count(&db, "public_billing_audit_legacy_v1").await, 1);
    let row: (String, String, String) = sqlx::query_as(
        "SELECT operation_id,owner_id,request_digest FROM public_billing_audit_legacy_v1",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(row.0, p.operation.operation_id.as_str());
    assert!(
        sqlx::query("UPDATE public_billing_audit_sources_v1 SET namespace='spoof'")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    assert!(sqlx::query("DELETE FROM public_billing_audit_legacy_v1")
        .execute(&db.scoped)
        .await
        .is_err());
    assert!(sqlx::query("INSERT INTO public_billing_audit_activations_v1(source_binding_id,legacy_count) VALUES($1::uuid,0)").bind(&id).execute(&db.scoped).await.is_err());
    assert!(db
        .repo
        .install_billing_domain_audit_for_reviewed_backend(
            &BillingDomainAuditInstallation::for_reviewed_backend("other_schema", "x", "x", "x")
                .unwrap()
        )
        .await
        .is_err());
    evidence(
        "legacy.json",
        json!({"source_binding":id,"legacy":legacy,"exact_legacy_O":row.0,"domain_events":0}),
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "owned PostgreSQL; revoke after NEW domain event/link SQL await"]
async fn billing_domain_session_revoke_after_new_await_rolls_back() {
    for table in ["audit_events", "public_billing_audit_links_v1"] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        install(&db, &complete["results"][0]["data"]["report"]).await;
        let f = service(&server);
        let s = session(&cookie);
        let p = f
            .prepare(&s, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        f.confirm(&s, &p).await.unwrap();
        let before = all_counts(&db).await;
        db.scoped.execute("CREATE FUNCTION domain_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
        let when = if table == "audit_events" {
            "WHEN (NEW.kind='BILLING_FEEDBACK_SAVED')"
        } else {
            ""
        };
        db.scoped.execute(format!("CREATE TRIGGER domain_wait BEFORE INSERT ON {table} FOR EACH ROW {when} EXECUTE FUNCTION domain_wait()").as_str()).await.unwrap();
        let mut blocker = db.scoped.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(8021021)")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let mut running = Box::pin(f.execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        ));
        let pattern = format!("INSERT INTO {table}%");
        tokio::select! {r=&mut running=>panic!("completed before new barrier: {:?}",r.err()),_=wait_for_query(&db,&pattern)=>{}}
        server.state.0.sessions.revoke(&s).unwrap();
        sqlx::query("SELECT pg_advisory_unlock(8021021)")
            .execute(&mut *blocker)
            .await
            .unwrap();
        assert!(matches!(running.await, Err(StorageError::Session)));
        assert_eq!(all_counts(&db).await, before);
        evidence(
            &format!("revoke-{table}.json"),
            json!({"rollback_counts":before,"stage":table}),
        );
        drop(blocker);
        server.stop();
        db.finish().await;
    }
}

struct DomainClock(std::sync::atomic::AtomicU64);
impl safeflow_storage::ConfirmationClock for DomainClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.0.load(Ordering::SeqCst) as i64).unwrap()
    }
}
#[tokio::test]
#[ignore = "owned PostgreSQL; expiry at newly added domain SQL wait, explicit clock"]
async fn billing_domain_expiry_after_new_await_rolls_back() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    install(&db, &complete["results"][0]["data"]["report"]).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let clock = Arc::new(DomainClock(std::sync::atomic::AtomicU64::new(now)));
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
    let p = f
        .prepare(&s, intent(&complete["results"][0]["data"]["report"]))
        .await
        .unwrap();
    f.confirm(&s, &p).await.unwrap();
    let before = all_counts(&db).await;
    db.scoped.execute("CREATE FUNCTION domain_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER domain_wait BEFORE INSERT ON public_billing_audit_links_v1 FOR EACH ROW EXECUTE FUNCTION domain_wait()").await.unwrap();
    let mut blocker = db.scoped.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut running = Box::pin(f.execute(
        &s,
        &p.task.task_id,
        &p.operation.operation_id,
        &p.confirmation.confirmation_ref,
    ));
    tokio::select! {r=&mut running=>panic!("no expiry barrier {:?}",r.err()),_=wait_for_query(&db,"INSERT INTO public_billing_audit_links_v1%")=>{}}
    clock.0.store(now + 10_000, Ordering::SeqCst);
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    assert!(matches!(
        running.await,
        Err(StorageError::PermitExpired) | Err(StorageError::ConfirmationExpired)
    ));
    assert_eq!(all_counts(&db).await, before);
    drop(blocker);
    evidence(
        "expiry-new-await.json",
        json!({"original_now":now,"after":now+10_000,"rollback_counts":before}),
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "owned PostgreSQL; actual business COMMIT response loss versus pre-COMMIT disconnect"]
async fn billing_domain_commit_unknown_only_original_query_committed_and_uncommitted() {
    for committed in [true, false] {
        let mut db = Db::new().await;
        let start = db.url.find("@127.0.0.1:").unwrap() + 1;
        let slash = db.url[start..].find('/').unwrap() + start;
        let proxy =
            pg_fault::CommitLossProxy::start(db.url[start..slash].parse().unwrap(), true).await;
        let proxy_url = format!("{}{}{}", &db.url[..start], proxy.address, &db.url[slash..]);
        db.repo.close().await;
        db.repo = PgRepository::connect(&proxy_url, &db.namespace)
            .await
            .unwrap();
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        install(&db, &complete["results"][0]["data"]["report"]).await;
        let i = intent(&complete["results"][0]["data"]["report"]);
        let cfg = safeflow_storage::BillingRecoveryConfig::new(
            &uuid::Uuid::new_v4().to_string(),
            &db.namespace,
            &i.transaction.source_id,
            "8ff8c665:mixed",
            "owned-domain-audit-continuity",
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
        let n = safeflow_storage::BillingRecoveryNonce::parse(&uuid::Uuid::new_v4().to_string())
            .unwrap();
        let f = service(&server);
        let s = session(&cookie);
        let p = f.prepare_v2(&s, &cfg, &cfg.realm(), &n, i).await.unwrap();
        let t: safeflow_contracts::TaskId = serde_json::from_value(p["task_id"].clone()).unwrap();
        let o: safeflow_contracts::OperationId =
            serde_json::from_value(p["operation_id"].clone()).unwrap();
        let r: safeflow_contracts::ConfirmationRef =
            serde_json::from_value(p["confirmation"]["confirmation_ref"].clone()).unwrap();
        f.confirm_preparation_v2(&s, &cfg, &cfg.realm(), &n, &t, &o, &r)
            .await
            .unwrap();
        let before = all_counts(&db).await;
        let realm = cfg.realm();
        let mut running = Box::pin(f.execute_preparation_v2(&s, &cfg, &realm, &n, &t, &o, &r));
        tokio::select! {r=&mut running=>panic!("no COMMIT barrier {:?}",r.err()),_=tokio::time::timeout(Duration::from_secs(10),proxy.commit_seen.notified())=>{}}
        assert_eq!(domain_count(&db).await, 0);
        assert_eq!(count(&db, "billing_feedback_v1").await, 0);
        assert!(proxy.fired.load(Ordering::SeqCst));
        if committed {
            proxy.release.notify_one();
        } else {
            drop(proxy);
        }
        assert!(matches!(
            running.await,
            Err(StorageError::ReconcileRequired)
        ));
        // Build an independent primary reader, with no source adapter/write retry.
        let direct = PgRepository::connect(&db.url, &db.namespace).await.unwrap();
        let state = ApiState::build_with_bank(
            direct.clone(),
            ApiConfig::local_simulation(Zeroizing::new(SECRET.into())),
            UserId::parse("user_demo").unwrap(),
            fixture::bank(),
            &[],
        )
        .unwrap();
        let cookie = state.0.sessions.issue(SECRET, None).unwrap();
        let original = service_state(&state)
            .query(&session(&cookie), &o)
            .await
            .unwrap();
        assert_eq!(original.is_some(), committed);
        if committed {
            assert_eq!(
                original.unwrap().domain_audit,
                BillingDomainAuditStatus::VerifiedV1
            );
            let xids:Vec<i64>=sqlx::query_scalar("SELECT xmin::text::bigint FROM public_billing_feedback_results_v1 UNION ALL SELECT xmin::text::bigint FROM public_billing_audit_links_v1 UNION ALL SELECT xmin::text::bigint FROM audit_events WHERE kind='BILLING_FEEDBACK_SAVED' UNION ALL SELECT xmin::text::bigint FROM public_audit_outbox WHERE envelope->'public_event'->>'kind'='BILLING_FEEDBACK_SAVED'")
                .fetch_all(&db.scoped).await.unwrap();
            assert_eq!(xids.len(), 4);
            assert!(xids.iter().all(|x| *x == xids[0]));
            evidence(
                "commit-unknown-committed.json",
                json!({"original_O":o,"same_parent_xid":xids,"domain_events":domain_count(&db).await}),
            );
        } else {
            assert_eq!(all_counts(&db).await, before);
            evidence(
                "commit-unknown-uncommitted.json",
                json!({"original_O":o,"query":"NONE","counts":before}),
            );
        }
        direct.close().await;
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "owned PostgreSQL; default constraints reject orphan/spoof, query rejects admin-corrupted originals"]
async fn billing_domain_missing_link_outbox_source_and_full_fact_corruptions_rejected() {
    // Schema-owner fault injection only. Every disabled trigger is restored using
    // transaction-local replica mode. Each mutation has a fresh isolated schema.
    let corruptions=[
      ("missing-link","DELETE FROM public_billing_audit_links_v1"),
      ("missing-outbox","DELETE FROM public_audit_outbox WHERE envelope->'public_event'->>'kind'='BILLING_FEEDBACK_SAVED'"),
      ("source-namespace","UPDATE public_billing_audit_sources_v1 SET namespace='cross_namespace'"),
      ("source-database","UPDATE public_billing_audit_sources_v1 SET database_name='other_primary'"),
      ("member-choice","UPDATE billing_feedback_v1 SET payload=jsonb_set(payload,'{choice}','\"MINE\"')"),
      ("member-window","UPDATE billing_feedback_v1 SET payload=jsonb_set(payload,'{request,window}','{}')"),
      ("member-event","UPDATE billing_feedback_events_v1 SET recorded_at=recorded_at+interval '1 second'"),
      ("staged-replay","UPDATE public_billing_feedback_results_v1 SET staged=jsonb_set(staged,'{replayed}','true')"),
      ("facts-digest","UPDATE public_billing_audit_links_v1 SET facts_digest=repeat('0',64)"),
      ("cross-source","UPDATE billing_feedback_v1 SET source_id='other_source'"),
      ("public-kind","UPDATE audit_events SET kind='TASK_CREATED' WHERE kind='BILLING_FEEDBACK_SAVED'"),
      ("public-detail","UPDATE audit_events SET details=details||'{\"choice\":\"NOT_MINE\"}' WHERE kind='BILLING_FEEDBACK_SAVED'"),
    ];
    for (name, sql) in corruptions {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        install(&db, &complete["results"][0]["data"]["report"]).await;
        let f = service(&server);
        let s = session(&cookie);
        let p = f
            .prepare(&s, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        f.confirm(&s, &p).await.unwrap();
        f.execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
        let mut admin = db.scoped.begin().await.unwrap();
        admin
            .execute("SET LOCAL session_replication_role=replica")
            .await
            .unwrap();
        // Constraint checks still guard numeric/exact details even for replica;
        // those cases are rejected before committing deliberately corrupt rows.
        let result = admin.execute(sql).await;
        let database_rejection = name == "public-detail" || name == "source-namespace";
        if database_rejection {
            assert!(result.is_err());
            admin.rollback().await.unwrap();
        } else {
            result.unwrap();
            admin.commit().await.unwrap();
            assert!(
                f.query(&s, &p.operation.operation_id).await.is_err(),
                "{name}"
            );
        }
        evidence(
            &format!("corrupt-{name}.json"),
            json!({"original_O":p.operation.operation_id,"rejection_layer":if database_rejection{"DATABASE_CHECK"}else{"ORIGINAL_QUERY"}}),
        );
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "owned PG; complete ordered non-target accounts and revoke at new domain await"]
async fn billing_domain_ordered_all_accounts_and_final_non_target_revocation() {
    for revoke in [false, true] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        provider.arguments.lock().unwrap()["account_ids_json"] =
            json!("[\"account_001\",\"account_002\"]");
        let source = fixture::replay("mixed");
        let access = Arc::new(AccountAccess {
            denied: std::sync::atomic::AtomicBool::new(false),
        });
        let server = billing_server_with_feedback(
            &db,
            &provider,
            "user_demo",
            access.clone(),
            Some(fixture::approved(
                access.clone(),
                Arc::new(AccountSource(source)),
                "8ff8c665:mixed",
            )),
            false,
            true,
        )
        .await;
        let cookie = server.login().await;
        let view = initial(&server, &cookie, "ordered original accounts").await;
        let complete = ok(execute(&server, &cookie, &view).await);
        let original = &complete["results"][0]["data"]["report"];
        assert_eq!(
            original["request"]["account_ids"],
            json!(["account_001", "account_002"])
        );
        install(&db, original).await;
        let f = service(&server);
        let s = session(&cookie);
        let p = f.prepare(&s, intent(original)).await.unwrap();
        f.confirm(&s, &p).await.unwrap();
        let before = all_counts(&db).await;
        if revoke {
            db.scoped.execute("CREATE FUNCTION domain_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8021021); RETURN NEW; END $$").await.unwrap();
            db.scoped.execute("CREATE TRIGGER domain_wait BEFORE INSERT ON public_billing_audit_links_v1 FOR EACH ROW EXECUTE FUNCTION domain_wait()").await.unwrap();
            let mut blocker = db.scoped.acquire().await.unwrap();
            sqlx::query("SELECT pg_advisory_lock(8021021)")
                .execute(&mut *blocker)
                .await
                .unwrap();
            let mut running = Box::pin(f.execute(
                &s,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            ));
            tokio::select! {r=&mut running=>panic!("no account barrier {:?}",r.err()),_=wait_for_query(&db,"INSERT INTO public_billing_audit_links_v1%")=>{}}
            access.denied.store(true, Ordering::SeqCst);
            sqlx::query("SELECT pg_advisory_unlock(8021021)")
                .execute(&mut *blocker)
                .await
                .unwrap();
            assert!(running.await.is_err());
            assert_eq!(all_counts(&db).await, before);
            drop(blocker);
        } else {
            f.execute(
                &s,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref,
            )
            .await
            .unwrap();
            // Keep header and payload mutually consistent; only order changes.
            let mut tx = db.scoped.begin().await.unwrap();
            tx.execute("SET LOCAL session_replication_role=replica")
                .await
                .unwrap();
            tx.execute("UPDATE billing_feedback_v1 SET payload=jsonb_set(payload,'{request,account_ids}','[\"account_002\",\"account_001\"]'); UPDATE billing_report_bindings_v1 SET header=jsonb_set(header,'{request,account_ids}','[\"account_002\",\"account_001\"]')").await.unwrap();
            tx.commit().await.unwrap();
            assert!(f.query(&s, &p.operation.operation_id).await.is_err());
        }
        evidence(
            &format!("all-accounts-revoke-{revoke}.json"),
            json!({"original_order":["account_001","account_002"],"non_target_revoke":revoke,"negative_rejected":true}),
        );
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "explicit owned transport-harness.mjs over unchanged fixed408; NOT formal domain adoption"]
async fn billing_domain_transport_actual_pending_existing_sender_strict_ack_and_retry() {
    use safeflow_storage::{
        AuditSendFailure, PublicAuditRetryPolicy, PublicAuditSender, PublicAuditSenderConfig,
    };
    let root = std::env::var("SAFEFLOW_DOMAIN_TRANSPORT").expect("explicit owned transport root");
    let ready: Value = serde_json::from_slice(
        &std::fs::read(std::path::Path::new(&root).join("ready.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        ready["endpoint"],
        "http://127.0.0.1:59449/api/audit/outbox/v1"
    );
    for mode in [
        "good",
        "unauthorized",
        "transient",
        "lost-ack",
        "wrong-digest",
        "missing-field",
        "extra-field",
        "wrong-operation",
        "local-ack-fault",
        "conflict",
    ] {
        let db = Db::new().await;
        let provider = BillingProvider::new("mixed").await;
        let (server, _, cookie, complete) = report(&db, &provider).await;
        install(&db, &complete["results"][0]["data"]["report"]).await;
        let f = service(&server);
        let s = session(&cookie);
        let p = f
            .prepare(&s, intent(&complete["results"][0]["data"]["report"]))
            .await
            .unwrap();
        f.confirm(&s, &p).await.unwrap();
        f.execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
        let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
        let original = outbox
            .pending(1000)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.envelope["public_event"]["kind"] == "BILLING_FEEDBACK_SAVED")
            .unwrap();
        let sender = PublicAuditSender::for_trusted_backend(
            outbox.clone(),
            PublicAuditSenderConfig::new(
                ready["endpoint"].as_str().unwrap(),
                Zeroizing::new(ready["token"].as_str().unwrap().into()),
                "fixed-408-isolated-compatibility",
                PublicAuditRetryPolicy::new(Duration::from_secs(3), 2, Duration::from_millis(100))
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let control = std::path::Path::new(&root).join("control.json");
        let case = format!("{mode}:{}", p.operation.operation_id);
        std::fs::write(&control, json!({"case":case,"mode":mode}).to_string()).unwrap();
        if mode == "local-ack-fault" {
            db.scoped.execute("CREATE FUNCTION ack_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'owned local ACK fault'; END $$").await.unwrap();
            db.scoped.execute("CREATE TRIGGER ack_fault BEFORE UPDATE ON public_audit_outbox FOR EACH ROW WHEN (NEW.envelope->'public_event'->>'kind'='BILLING_FEEDBACK_SAVED') EXECUTE FUNCTION ack_fault()").await.unwrap();
        }
        let batch = sender.send_pending(1000).await.unwrap();
        let r = batch
            .iter()
            .find(|r| r.event_id == original.event_id)
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
        assert_eq!(r.outcome, expected, "{mode}");
        assert_eq!(
            r.attempts,
            if mode == "transient" || mode == "lost-ack" {
                2
            } else {
                1
            },
            "{mode}"
        );
        let pending = outbox.pending(1000).await.unwrap();
        assert_eq!(
            pending.iter().any(|p| p.event_id == original.event_id),
            expected.is_err()
        );
        if expected.is_err() {
            let unchanged = pending
                .iter()
                .find(|p| p.event_id == original.event_id)
                .unwrap();
            assert_eq!(unchanged, &original);
            if mode == "local-ack-fault" {
                db.scoped
                    .execute("DROP TRIGGER ack_fault ON public_audit_outbox")
                    .await
                    .unwrap();
            }
            if mode != "conflict" {
                // A new poll still selects original pending; no business execution.
                std::fs::write(&control, json!({"case":case,"mode":"good"}).to_string()).unwrap();
                let second = sender.send_pending(1000).await.unwrap();
                assert_eq!(second.len(), 1);
                assert!(second[0].outcome.is_ok());
            } else {
                let second = sender.send_pending(1000).await.unwrap();
                assert_eq!(second.len(), 1);
                assert_eq!(second[0].outcome, Err(AuditSendFailure::Http(409)));
            }
        }
        assert_eq!(domain_count(&db).await, 1);
        assert_eq!(count(&db, "billing_feedback_v1").await, 1);
        let ack:Option<(String,String)>=sqlx::query_as("SELECT receiver,receipt_ref FROM public_audit_outbox WHERE event_id=$1 AND acknowledged_at IS NOT NULL")
            .bind(original.event_id.as_str()).fetch_optional(&db.scoped).await.unwrap();
        evidence(
            &format!("sender-{mode}.json"),
            json!({"original_event_id":original.event_id,"envelope":original.envelope,
            "digest":original.digest,"attempts":r.attempts,"outcome":format!("{:?}",r.outcome),"ack":ack,
            "remaining_pending":outbox.pending(1000).await.unwrap().len(),"domain_events":1,"business_saves":1,
            "receiver_fixed":"408e4df2b3aa250fc800371de0519b36d9117426","formal_adoption":false}),
        );
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "explicit seed process; own namespace retained for real PG and receiver restart"]
async fn billing_domain_restart_seed() {
    use safeflow_storage::{
        AuditSendFailure, PublicAuditRetryPolicy, PublicAuditSender, PublicAuditSenderConfig,
    };
    let root = std::env::var("SAFEFLOW_DOMAIN_RESTART").expect("explicit fresh restart directory");
    let path = std::path::Path::new(&root);
    assert!(!path.join("seed.json").exists());
    std::fs::create_dir_all(path).unwrap();
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let source = install(&db, original).await;
    let f = service(&server);
    let s = session(&cookie);
    let p = f.prepare(&s, intent(original)).await.unwrap();
    f.confirm(&s, &p).await.unwrap();
    let saved = f
        .execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
    let pending = outbox
        .pending(1000)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.envelope["public_event"]["kind"] == "BILLING_FEEDBACK_SAVED")
        .unwrap();
    let transport = std::env::var("SAFEFLOW_DOMAIN_TRANSPORT").unwrap();
    std::fs::write(
        std::path::Path::new(&transport).join("control.json"),
        json!({"case":format!("restart:{}",p.operation.operation_id),"mode":"lost-ack"})
            .to_string(),
    )
    .unwrap();
    let sender = PublicAuditSender::for_trusted_backend(
        outbox.clone(),
        PublicAuditSenderConfig::new(
            "http://127.0.0.1:59449/api/audit/outbox/v1",
            Zeroizing::new("isolated-domain-compatibility-token".into()),
            "fixed-408-isolated-compatibility",
            PublicAuditRetryPolicy::new(Duration::from_secs(3), 1, Duration::from_millis(100))
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let results = sender.send_pending(1000).await.unwrap();
    let r = results
        .iter()
        .find(|r| r.event_id == pending.event_id)
        .unwrap();
    assert_eq!(r.outcome, Err(AuditSendFailure::Transport));
    assert_eq!(outbox.pending(1000).await.unwrap(), vec![pending.clone()]);
    let seed = json!({"pid":std::process::id(),"namespace":db.namespace,"source_binding":source,"operation":p.operation.operation_id,
        "report":original,"saved":saved,"envelope":pending.envelope,"event_id":pending.event_id,"digest":pending.digest,
        "historical":pending.historical,"enqueued_at":pending.enqueued_at,"counts":all_counts(&db).await});
    std::fs::write(
        path.join("seed.json"),
        serde_json::to_vec_pretty(&seed).unwrap(),
    )
    .unwrap();
    server.stop();
    db.repo.close().await;
    db.scoped.close().await;
    db.admin.close().await;
    println!(
        "restart seed PID {} namespace {}",
        std::process::id(),
        db.namespace
    );
}

#[tokio::test]
#[ignore = "separate verify process after explicit own PG and fixed408 receiver restart"]
async fn billing_domain_restart_verify() {
    use safeflow_storage::{PublicAuditRetryPolicy, PublicAuditSender, PublicAuditSenderConfig};
    let root = std::env::var("SAFEFLOW_DOMAIN_RESTART").expect("explicit restart root");
    let path = std::path::Path::new(&root);
    assert!(!path.join("verify.json").exists());
    let seed: Value =
        serde_json::from_slice(&std::fs::read(path.join("seed.json")).unwrap()).unwrap();
    assert_ne!(seed["pid"], std::process::id());
    let namespace = seed["namespace"].as_str().unwrap();
    assert!(namespace.starts_with("sf_http_test_") && namespace.len() == 45);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect(&url, namespace).await.unwrap();
    let state = ApiState::build_with_bank(
        repo.clone(),
        ApiConfig::local_simulation(Zeroizing::new(SECRET.into())),
        UserId::parse("user_demo").unwrap(),
        fixture::bank(),
        &[],
    )
    .unwrap();
    let cookie = state.0.sessions.issue(SECRET, None).unwrap();
    let s = session(&cookie);
    let operation = serde_json::from_value(seed["operation"].clone()).unwrap();
    let f = service_state(&state);
    let saved = f.query(&s, &operation).await.unwrap().unwrap();
    assert_eq!(saved.domain_audit, BillingDomainAuditStatus::VerifiedV1);
    assert_eq!(saved.saved, seed["saved"]["saved"]);
    let report = f
        .original_report(
            &s,
            seed["report"]["metadata"]["report_id"].as_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(report, seed["report"]);
    let outbox = PublicAuditOutbox::for_trusted_backend(repo.clone());
    let pending = outbox.pending(1000).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].event_id.as_str(),
        seed["event_id"].as_str().unwrap()
    );
    assert_eq!(pending[0].envelope, seed["envelope"]);
    assert_eq!(pending[0].digest.as_str(), seed["digest"].as_str().unwrap());
    assert_eq!(json!(pending[0].enqueued_at), seed["enqueued_at"]);
    let transport = std::env::var("SAFEFLOW_DOMAIN_TRANSPORT").unwrap();
    std::fs::write(
        std::path::Path::new(&transport).join("control.json"),
        json!({"case":"restart-recovery","mode":"good"}).to_string(),
    )
    .unwrap();
    let sender = PublicAuditSender::for_trusted_backend(
        outbox.clone(),
        PublicAuditSenderConfig::new(
            "http://127.0.0.1:59449/api/audit/outbox/v1",
            Zeroizing::new("isolated-domain-compatibility-token".into()),
            "fixed-408-isolated-compatibility",
            PublicAuditRetryPolicy::new(Duration::from_secs(3), 2, Duration::from_millis(100))
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let result = sender.send_pending(1000).await.unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].outcome.is_ok());
    assert!(outbox.pending(1000).await.unwrap().is_empty());
    std::fs::write(path.join("verify.json"),serde_json::to_vec_pretty(&json!({"pid":std::process::id(),"seed_pid":seed["pid"],"same_original":true,
        "event_id":seed["event_id"],"digest":seed["digest"],"saved":saved,"source_adapter":false,"sender_recovered":true})).unwrap()).unwrap();
    repo.close().await;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    admin
        .execute(format!("DROP SCHEMA {namespace} CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
#[ignore = "explicit audit744 owned PG seed; retain schema for original sender only"]
async fn audit744_seed_original_billing_feedback() {
    let root = std::env::var("AUD744_BILL_ROOT").unwrap();
    let path = std::path::Path::new(&root);
    assert!(!path.join("seed.json").exists());
    std::fs::create_dir_all(path).unwrap();
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, _, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let source = install(&db, original).await;
    let f = service(&server);
    let s = session(&cookie);
    let p = f.prepare(&s, intent(original)).await.unwrap();
    f.confirm(&s, &p).await.unwrap();
    let saved = f
        .execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
    let pending = outbox
        .pending(1000)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.envelope["public_event"]["kind"] == "BILLING_FEEDBACK_SAVED")
        .unwrap();

    let seed = json!({"pid":std::process::id(),"schema":db.namespace,"source_binding":source,"operation":p.operation.operation_id,"report":original,"saved":saved,"pending_event":pending.event_id,"counts":all_counts(&db).await});
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).open(std::env::var("AUD744_SEED").unwrap()).unwrap().write_all(&serde_json::to_vec_pretty(&seed).unwrap()).unwrap();
    server.stop();
    db.repo.close().await;
    db.scoped.close().await;
    db.admin.close().await;
}
