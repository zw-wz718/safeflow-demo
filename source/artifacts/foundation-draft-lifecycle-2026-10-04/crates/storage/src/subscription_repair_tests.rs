//! Repair regression tests. Scheduling/clock faults only; original member facts,
//! real preparation, Kernel, consumed permit and gate remain in use. No A2 event.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct StaleFirstIdentity {
    current: Arc<Sessions>,
    calls: AtomicUsize,
}
impl SessionResolver for StaleFirstIdentity {
    fn resolve(&self,s:&SessionRef)->Result<UserId,SecurityError>{Ok(self.resolve_binding(s)?.user_id)}
    fn resolve_binding(&self,s:&SessionRef)->Result<SessionBinding,SecurityError>{
        let mut binding=self.current.resolve_binding(s)?;
        if self.calls.fetch_add(1,Ordering::SeqCst)==0 {binding.binding_version=1;}
        Ok(binding)
    }
}
#[tokio::test]
#[ignore]
async fn subscription_repair_stale_identity_before_cache_admission_cannot_clear_new_binding() {
    let db=Db::new().await;db.initialize("already-active").await;
    db.sessions.bind(&db.session,"user_demo",2);
    let latest=db.read.read(&db.session).await.unwrap();
    let choice=latest["items"][0]["selection_ref"].as_str().unwrap();
    let before=db.facts().await;
    let mut old=db.read.clone();
    old.sessions=Arc::new(StaleFirstIdentity{current:db.sessions.clone(),calls:AtomicUsize::new(0)});
    assert_eq!(old.read(&db.session).await.err(),Some(StorageError::Session));
    db.read.details(&db.session,choice).await.unwrap();
    assert_eq!(before,db.facts().await);
    db.finish().await;
}
struct OffsetClock(i64);
impl crate::ConfirmationClock for OffsetClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::seconds(self.0)
    }
}
pub(super) fn confirmation(db: &Db, offset: i64) -> ConfirmationService {
    ConfirmationService::for_trusted_backend_with_clock(
        db.read.repository.clone(),
        db.sessions.clone(),
        ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
        Arc::new(OffsetClock(offset)),
    )
}
async fn prepare(
    writes: &SubscriptionWriteService<'_>,
    db: &Db,
    host: &SubscriptionHost,
    nonce: &str,
) -> Result<SubscriptionPreparation, StorageError> {
    writes
        .prepare_technical(
            &db.session,
            host,
            nonce,
            Preparation::Change(request(Action::RecordCancellation)),
        )
        .await
}
#[tokio::test]
#[ignore]
async fn subscription_repair_nonce_concurrency_response_loss_and_conflicts() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let mut writes = db.writes(assembly.runtime());
    let nonce = uuid::Uuid::new_v4().to_string();
    let (a, b) = tokio::join!(
        prepare(&writes, &db, &host, &nonce),
        prepare(&writes, &db, &host, &nonce)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(json!(a), json!(b));
    let before = db.facts().await;
    assert_eq!(before["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(
        before["subscription_preparations_v1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Successful response discarded: the caller retains ONLY its original nonce.
    assert_eq!(
        json!(a),
        json!(prepare(&writes, &db, &host, &nonce).await.unwrap())
    );
    assert_eq!(before, db.facts().await);
    assert_eq!(
        writes
            .prepare_technical(
                &db.session,
                &host,
                &nonce,
                Preparation::Change(request(Action::ReminderSettings))
            )
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(
        writes
            .prepare_technical(
                &db.session,
                &host,
                &nonce,
                Preparation::Change(SubscriptionChangeRequestMock::CreateCancellationIntent {
                    subscription_id: "different-original-input".into()
                })
            )
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        prepare(&writes, &db, &host, &nonce).await.err(),
        Some(StorageError::FactsConflict)
    );
    db.sessions.bind(&db.session, "user_demo", 1);
    writes.confirmations = confirmation(&db, 61);
    assert_eq!(
        prepare(&writes, &db, &host, &nonce).await.err(),
        Some(StorageError::ConfirmationExpired)
    );
    assert_eq!(
        before,
        db.facts().await,
        "expiry/conflicts cannot refresh originals or add effects"
    );
    for statement in [
        "UPDATE subscription_preparations_v1 SET nonce=nonce",
        "DELETE FROM subscription_preparations_v1",
    ] {
        let mut tx = db.read.repository.pool.begin().await.unwrap();
        assert!(sqlx::query(statement).execute(&mut *tx).await.is_err());
        tx.rollback().await.unwrap();
    }
    drop(writes);
    drop(assembly);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_repair_nonce_recovers_committed_readback_and_confirmation_faults() {
    for readback_fault in [true, false] {
        let db = Db::new().await;
        db.initialize("already-active").await;
        let host = db.read.startup_for_host(&db.session).await.unwrap();
        let assembly = db.assembly();
        let mut writes = db.writes(assembly.runtime());
        let nonce = uuid::Uuid::new_v4().to_string();
        if readback_fault {
            writes.store.repository.subscription_read_pool.close().await;
        } else {
            writes.confirmations = confirmation(&db, -120);
        }
        assert!(prepare(&writes, &db, &host, &nonce).await.is_err());
        let committed = db.facts().await;
        assert_eq!(committed["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(
            committed["subscription_public_bindings_v1"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(committed["server_confirmations"], json!([]));
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let restored = PgRepository::connect_existing(&url, &db.schema)
            .await
            .unwrap();
        writes.store.repository = restored.clone();
        writes.confirmations = ConfirmationService::for_trusted_backend(
            restored.clone(),
            db.sessions.clone(),
            ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
        );
        let saved_task: TaskRecord = serde_json::from_value(committed["subscription_preparations_v1"][0]["prepared_task"].clone()).unwrap();
        let saved_operation: OperationRecord = serde_json::from_value(committed["subscription_preparations_v1"][0]["prepared_operation"].clone()).unwrap();
        // Even a generic trusted confirmation retry must derive the original cap.
        let generic = writes.confirmations.prepare(&db.session,&saved_task.task_id,&saved_operation.operation_id).await.unwrap();
        let p = prepare(&writes, &db, &host, &nonce).await.unwrap();
        assert_eq!(generic,p.confirmation);
        let recovered = db.facts().await;
        for name in [
            "subscription_runtime_records",
            "subscription_public_bindings_v1",
            "subscription_preparations_v1",
            "tasks",
            "operations",
        ] {
            assert_eq!(committed[name], recovered[name], "{name}");
        }
        assert_eq!(
            recovered["server_confirmations"].as_array().unwrap().len(),
            1
        );
        let member = &recovered["subscription_public_bindings_v1"][0]["member_binding"];
        let expires =
            chrono::DateTime::parse_from_rfc3339(member["expires_at"].as_str().unwrap()).unwrap();
        assert!(p.confirmation.expires_at <= expires);
        assert_eq!(
            json!(p),
            json!(prepare(&writes, &db, &host, &nonce).await.unwrap())
        );
        assert_eq!(recovered, db.facts().await);
        restored.close().await;
        drop(writes);
        drop(assembly);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn subscription_repair_nonce_preserves_complete_reminder_schedule_input() {
    let db=Db::new().await;db.initialize("already-active").await;
    let host=db.read.startup_for_host(&db.session).await.unwrap();
    let assembly=db.assembly();let writes=db.writes(assembly.runtime());
    let nonce=uuid::Uuid::new_v4().to_string();
    let first=writes.prepare_technical(&db.session,&host,&nonce,Preparation::Change(request(Action::ReminderSettings))).await.unwrap();
    let before=db.facts().await;
    assert_eq!(json!(first),json!(writes.prepare_technical(&db.session,&host,&nonce,Preparation::Change(request(Action::ReminderSettings))).await.unwrap()));
    let mut changed=request(Action::ReminderSettings);
    if let SubscriptionChangeRequestMock::UpdateReminderSettings{schedule,..}=&mut changed {schedule.days_before+=1;}
    assert_eq!(writes.prepare_technical(&db.session,&host,&nonce,Preparation::Change(changed)).await.err(),Some(StorageError::FactsConflict));
    assert_eq!(before,db.facts().await);
    drop(writes);drop(assembly);db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_repair_real_backend_list_out_of_order_preserves_latest() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let original = db.journal().await;
    // Same original facts; view only schedules the first read behind two locks.
    let ddl="ALTER TABLE subscription_runtime_records RENAME TO repair_original_records; CREATE FUNCTION repair_delay(v JSONB) RETURNS JSONB LANGUAGE plpgsql VOLATILE AS $$ BEGIN IF pg_try_advisory_xact_lock(93873999) THEN PERFORM pg_advisory_xact_lock(93873888); END IF; RETURN v; END $$; CREATE VIEW subscription_runtime_records AS SELECT owner_id,storage_version,encoding_version,repair_delay(journal) AS journal FROM repair_original_records;";
    sqlx::raw_sql(ddl)
        .execute(&db.read.repository.pool)
        .await
        .unwrap();
    let mut blocker = db.admin.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(93873888)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let svc = db.read.clone();
    let s = db.session.clone();
    let mut a = tokio::spawn(async move { svc.read(&s).await });
    let limit = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory' AND query LIKE 'SELECT storage_version,encoding_version,journal FROM%'").fetch_one(&db.admin).await.unwrap();
        if n > 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < limit);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Actual caller timeout: the spawned backend is STILL waiting in SQL.
    assert!(tokio::time::timeout(Duration::from_millis(5), &mut a).await.is_err());
    let b = db.read.read(&db.session).await.unwrap();
    let choice = b["items"][0]["selection_ref"].as_str().unwrap();
    db.read.details(&db.session, choice).await.unwrap();
    blocker.rollback().await.unwrap();
    assert_eq!(a.await.unwrap().err(), Some(StorageError::FactsConflict));
    db.read.details(&db.session, choice).await.unwrap();
    let actual: Value = sqlx::query_scalar("SELECT journal FROM repair_original_records")
        .fetch_one(&db.read.repository.pool)
        .await
        .unwrap();
    assert_eq!(original, actual);
    db.finish().await;
}
// Three earlier guard clock reads are still before the original cutoff; only
// the final member-deadline read crosses it. No successful domain event is forged.
struct TailClock {
    before: i64,
    expiry: i64,
    calls: AtomicUsize,
}
impl crate::ConfirmationClock for TailClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        chrono::DateTime::from_timestamp_millis(if n < 3 { self.before } else { self.expiry })
            .unwrap()
    }
}
pub(super) fn cross_deadline(unit: &mut PermitTransaction) -> ContractFuture<'_, Result<(), StorageError>> {
    Box::pin(async move {
        let deadline = deadline_for_permit(unit).await?.unwrap();
        assert_eq!(deadline.check(deadline.prepared.timestamp_millis()-1).err(),Some(StorageError::Invalid("subscription clock moved backwards")));
        assert!(unit.consumed.as_ref().unwrap().expires <= deadline.expires.timestamp_millis());
        unit.store.clock = Arc::new(TailClock {
            before: unit.consumed.as_ref().unwrap().expires - 1,
            expiry: deadline.expires.timestamp_millis(),
            calls: AtomicUsize::new(0),
        });
        unit.final_guard(true).await
    })
}
#[tokio::test]
#[ignore]
async fn subscription_repair_real_stage_last_member_deadline_rejects_and_rolls_back() {
    let db = Db::new().await;
    db.initialize("already-active").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let mut writes = db.writes(assembly.runtime());
    let p = prepare(&writes, &db, &host, &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap();
    writes
        .confirmations
        .record_explicit_confirmation(
            &db.session,
            &p.task.task_id,
            &p.confirmation.confirmation_ref,
            &p.confirmation.binding,
        )
        .await
        .unwrap();
    let before = db.facts().await;
    writes.stage_observer = Some(cross_deadline);
    assert_eq!(
        writes
            .execute_technical(
                &db.session,
                &host,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    assert_eq!(
        before,
        db.facts().await,
        "real member CAS, consumed permit, submission, audit and outbox all rollback"
    );
    drop(writes);
    drop(assembly);
    db.finish().await;
}
