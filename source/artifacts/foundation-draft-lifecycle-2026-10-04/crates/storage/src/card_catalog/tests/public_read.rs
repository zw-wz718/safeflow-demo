//! Public composition fixtures only. No member JSON is fabricated or modified.
use super::*;
use crate::CardReadService;
use safeflow_card_facts::{
    operation_records::StoredReasonCode, operation_replay::LockReplayParameters, LockSource,
};
use safeflow_card_postgres_store::card_selection::ReadOnlyCardList;
use safeflow_card_postgres_store::freeze::{FreezeCommand, FreezeRecordIds};
use serde_json::Value;
use tokio::sync::Notify;
fn nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn token(v: &Value) -> &str {
    v["step_ref"].as_str().unwrap()
}
fn navigation(db: &Db) -> CardReadService {
    CardReadService::for_trusted_backend(db.repo.clone(), db.sessions.clone(), Some(db.binding()))
}
async fn setup() -> Db {
    let db = Db::new().await;
    db.provider
        .set(&[("public_card_a", "0022"), ("public_card_b", "0022")]);
    db.service()
        .initialize(&db.session, &db.account)
        .await
        .unwrap();
    db
}
async fn read(db: &Db, s: &CardReadService) -> Value {
    s.query(&db.session, &db.account, &nonce()).await.unwrap()
}
async fn snapshot(db: &Db) -> Value {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname=current_schema() ORDER BY tablename",
    )
    .fetch_all(&db.sql)
    .await
    .unwrap();
    let mut out = serde_json::Map::new();
    for t in tables {
        assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        let rows:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(x ORDER BY x::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) x FROM {t} t) a"))
            .fetch_one(&db.sql).await.unwrap();
        out.insert(t, rows);
    }
    Value::Object(out)
}
async fn seed(db: &Db, n: &str, card: &str, source: LockSource, event: Option<&str>) {
    // Already reviewed member command shape, TEST SETUP ONLY, never constructed
    // by navigation or by HTTP. Each read is compared with a full DB snapshot.
    let c = FreezeCommand {
        operation_id: format!("OP-{n}"),
        canonical_key: format!("KEY-{n}"),
        event_scope: (source != LockSource::UserSelf).then(|| "fictional-provider-v1".into()),
        ids: FreezeRecordIds {
            lock_id: format!("L-{n}"),
            operation_record_id: format!("R-{n}"),
            reason_history_id: format!("H-{n}"),
        },
        parameters: LockReplayParameters {
            card_id: card.into(),
            source,
            reason: match source {
                LockSource::UserSelf => StoredReasonCode::UserReportLost,
                LockSource::AnomalyDispute => StoredReasonCode::NonOwnerTransaction,
                LockSource::RiskControl => StoredReasonCode::RiskControlFlag,
            },
            event_id: event.map(str::to_owned),
            transaction_id: (source == LockSource::AnomalyDispute).then(|| "PRIVATE-TX".into()),
            feedback_id: (source == LockSource::AnomalyDispute).then(|| "PRIVATE-FEEDBACK".into()),
        },
    };
    let mut tx = db.sql.begin().await.unwrap();
    sqlx::query("SELECT txid_current()")
        .execute(&mut *tx)
        .await
        .unwrap();
    LockedCardAccount::acquire(&mut tx, "catalog_test_owner", db.account.as_str())
        .await
        .unwrap()
        .save_freeze(&c)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
struct Hook {
    base: Arc<Provider>,
    calls: AtomicU64,
    pause: AtomicU64,
    entered: Notify,
    release: Notify,
    deny: AtomicBool,
}
impl Hook {
    fn new(db: &Db) -> Arc<Self> {
        Arc::new(Self {
            base: db.provider.clone(),
            calls: AtomicU64::new(0),
            pause: AtomicU64::new(0),
            entered: Notify::new(),
            release: Notify::new(),
            deny: AtomicBool::new(false),
        })
    }
}
impl BackendCardCatalogProvider for Hook {
    fn authorized_catalog<'a>(
        &'a self,
        i: &'a SessionBinding,
        a: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        self.base.authorized_catalog(i, a)
    }
    fn current_revision<'a>(
        &'a self,
        i: &'a SessionBinding,
        a: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == self.pause.load(Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if self.deny.load(Ordering::SeqCst)
                || self.base.fail.load(Ordering::SeqCst)
                || !self.base.known.load(Ordering::SeqCst)
            {
                return Err(StorageError::DependencyUnavailable);
            }
            self.base.current_revision(i, a).await
        })
    }
}
fn hooked(db: &Db, h: Arc<Hook>) -> CardReadService {
    CardReadService::for_trusted_backend(
        db.repo.clone(),
        db.sessions.clone(),
        Some(
            CardCatalogSourceBinding::for_reviewed_backend_provider(
                "isolated_test_provider",
                "test_impl_1",
                h,
            )
            .unwrap(),
        ),
    )
}
async fn append_adoption(db: &Db, source: &str) {
    let revision = db.provider.revision.fetch_add(1, Ordering::SeqCst) + 1;
    sqlx::query("INSERT INTO public_card_catalog_adoptions_v1 SELECT account_id,owner_id,ordinal+1,$1,implementation_revision,$3,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$2 ORDER BY ordinal DESC LIMIT 1")
    .bind(source).bind(db.account.as_str()).bind(format!("revision_{revision}")).execute(&db.sql).await.unwrap();
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_same_tail_explicit_selection_history_original_and_zero_business_writes() {
    let db = setup().await;
    seed(&db, "a", "public_card_a", LockSource::UserSelf, None).await;
    seed(&db, "b", "public_card_a", LockSource::UserSelf, None).await;
    seed(
        &db,
        "c",
        "public_card_a",
        LockSource::RiskControl,
        Some("private-event"),
    )
    .await;
    let before = snapshot(&db).await;
    let s = navigation(&db);
    let v = read(&db, &s).await;
    assert_eq!(v["cards"].as_array().unwrap().len(), 2);
    assert_eq!(v["cards"][0]["ambiguous_tail"], true);
    assert!(v["history"].is_null());
    assert_eq!(v["account_outgoing_restricted"], true);
    let b = s
        .select_card(&db.session, token(&v), 2, &nonce())
        .await
        .unwrap();
    assert_eq!(b["history"]["card_locked"], false);
    assert_eq!(b["history"]["account_outgoing_restricted"], true);
    assert_eq!(b["history"]["locks"], serde_json::json!([]));
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    let a = s
        .select_card(&db.session, token(&b), 1, &nonce())
        .await
        .unwrap();
    assert_eq!(a["history"]["locks"].as_array().unwrap().len(), 2);
    assert!(s
        .select_record(&db.session, token(&b), 1, &nonce())
        .await
        .is_err());
    for pos in 1..=3 {
        let fresh = read(&db, &s).await;
        let a = s
            .select_card(&db.session, token(&fresh), 1, &nonce())
            .await
            .unwrap();
        let original = s
            .select_record(&db.session, token(&a), pos, &nonce())
            .await
            .unwrap();
        assert_eq!(original["original"]["state"], "RECORDED_ACTIVE");
        let text = original.to_string();
        for secret in [
            "PRIVATE-TX",
            "private-event",
            "KEY-",
            "L-a",
            "catalog_test_owner",
            "public_card_a",
        ] {
            assert!(!text.contains(secret));
        }
        assert!(s
            .select_record(&db.session, token(&a), pos, &nonce())
            .await
            .is_err());
    }
    assert_eq!(snapshot(&db).await, before);
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_original_inactive_kept_separate_from_other_current_locks() {
    let db = setup().await;
    seed(&db, "a", "public_card_a", LockSource::UserSelf, None).await;
    seed(&db, "b", "public_card_b", LockSource::UserSelf, None).await;
    db.sql
        .execute("UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id='L-a'")
        .await
        .unwrap();
    let before = snapshot(&db).await;
    let s = navigation(&db);
    let v = read(&db, &s).await;
    let h = s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .unwrap();
    assert_eq!(h["history"]["card_locked"], false);
    assert_eq!(h["history"]["account_outgoing_restricted"], true);
    let o = s
        .select_record(&db.session, token(&h), 1, &nonce())
        .await
        .unwrap();
    assert_eq!(o["original"]["state"], "RECORDED_INACTIVE");
    assert_eq!(o["original"]["account_outgoing_restricted"], true);
    assert_eq!(snapshot(&db).await, before);
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_missing_foreign_complete_empty_and_restart_requery() {
    let db = Db::new().await;
    let s = navigation(&db);
    assert!(s.query(&db.session, &db.account, &nonce()).await.is_err());
    db.service()
        .initialize(&db.session, &db.account)
        .await
        .unwrap();
    let v = read(&db, &s).await;
    assert_eq!(v["cards"], serde_json::json!([]));
    let missing = CardReadService::for_trusted_backend(db.repo.clone(), db.sessions.clone(), None);
    assert!(missing
        .query(&db.session, &db.account, &nonce())
        .await
        .is_err());
    let mut tx = db.sql.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("foreign_owner").unwrap(),
        &ResourceId::parse("foreign_account").unwrap(),
        &DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(s
        .query(
            &db.session,
            &ResourceId::parse("foreign_account").unwrap(),
            &nonce()
        )
        .await
        .is_err());
    let restarted = navigation(&db);
    assert!(restarted
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    assert_eq!(read(&db, &restarted).await["cards"], serde_json::json!([]));
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_tokens_cross_session_generation_refresh_error_and_cancel() {
    let db = setup().await;
    let s = navigation(&db);
    let v = read(&db, &s).await;
    let other = SessionRef::parse("other_isolated_session").unwrap();
    assert!(s.select_card(&other, token(&v), 1, &nonce()).await.is_err());
    let fresh = read(&db, &s).await;
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    assert!(s
        .select_card(&db.session, token(&fresh), 0, &nonce())
        .await
        .is_err());
    assert!(s
        .select_card(&db.session, token(&fresh), 1, &nonce())
        .await
        .is_err());
    let v = read(&db, &s).await;
    s.cancel(&db.session, token(&v)).unwrap();
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    let v = read(&db, &s).await;
    db.sessions.0.fetch_add(1, Ordering::SeqCst);
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    let v = read(&db, &s).await;
    db.sessions.0.store(0, Ordering::SeqCst);
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_db_facts_and_source_adoption_aba_reject_old_selection() {
    let db = setup().await;
    let s = navigation(&db);
    let v = read(&db, &s).await;
    db.sql.execute("UPDATE card_catalog_v1 SET last_four='9999' WHERE card_id='public_card_a'; UPDATE card_catalog_v1 SET last_four='0022' WHERE card_id='public_card_a'").await.unwrap();
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    let v = read(&db, &s).await;
    let revision = db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap()
        .revision;
    append_adoption(&db, "foreign_source").await;
    assert!(s.query(&db.session, &db.account, &nonce()).await.is_err());
    append_adoption(&db, "isolated_test_provider").await;
    assert_eq!(
        db.service()
            .observe(&db.session, &db.account)
            .await
            .unwrap()
            .revision,
        revision
    );
    // A separately retained object must fail even after source identity restored.
    let s2 = navigation(&db);
    let old = read(&db, &s2).await;
    append_adoption(&db, "foreign_source").await;
    append_adoption(&db, "isolated_test_provider").await;
    assert!(s2
        .select_card(&db.session, token(&old), 1, &nonce())
        .await
        .is_err());
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    read(&db, &s).await;
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_provider_changed_revoked_failed_and_return_boundary_changes() {
    for mode in [
        "changed",
        "revoked",
        "failed",
        "return_session",
        "return_adoption",
    ] {
        let db = setup().await;
        let hook = Hook::new(&db);
        let s = hooked(&db, hook.clone());
        if mode.starts_with("return_") {
            hook.pause.store(2, Ordering::SeqCst);
            let task = {
                let s = s.clone();
                let session = db.session.clone();
                let a = db.account.clone();
                tokio::spawn(async move { s.query(&session, &a, &nonce()).await })
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), hook.entered.notified())
                .await
                .unwrap();
            if mode == "return_session" {
                db.sessions.0.fetch_add(1, Ordering::SeqCst);
            } else {
                append_adoption(&db, "isolated_test_provider").await;
            }
            hook.release.notify_one();
            assert!(task.await.unwrap().is_err());
        } else {
            let v = read(&db, &s).await;
            match mode {
                "changed" => {
                    db.provider.revision.fetch_add(1, Ordering::SeqCst);
                }
                "revoked" => hook.deny.store(true, Ordering::SeqCst),
                _ => db.provider.fail.store(true, Ordering::SeqCst),
            }
            assert!(s
                .select_card(&db.session, token(&v), 1, &nonce())
                .await
                .is_err());
            hook.deny.store(false, Ordering::SeqCst);
            db.provider.fail.store(false, Ordering::SeqCst);
            assert!(s
                .select_card(&db.session, token(&v), 1, &nonce())
                .await
                .is_err());
        }
        db.close().await;
    }
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_late_query_cancelled_request_cannot_publish_or_remove_new_step() {
    let db = setup().await;
    let hook = Hook::new(&db);
    hook.pause.store(1, Ordering::SeqCst);
    let s = hooked(&db, hook.clone());
    let old_request = nonce();
    let task = {
        let s = s.clone();
        let session = db.session.clone();
        let a = db.account.clone();
        let id = old_request.clone();
        tokio::spawn(async move { s.query(&session, &a, &id).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), hook.entered.notified())
        .await
        .unwrap();
    s.cancel_request(&db.session, &old_request).unwrap();
    hook.release.notify_one();
    assert!(task.await.unwrap().is_err());
    let fresh = read(&db, &s).await;
    let cancelled_before_start = nonce();
    s.cancel_request(&db.session, &cancelled_before_start)
        .unwrap();
    assert!(s
        .query(&db.session, &db.account, &cancelled_before_start)
        .await
        .is_err());
    s.cancel_request(&db.session, &old_request).unwrap();
    s.select_card(&db.session, token(&fresh), 1, &nonce())
        .await
        .unwrap();
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_capacity_evicts_only_navigation_and_refresh_failure_discards_old() {
    let db = setup().await;
    let hook = Hook::new(&db);
    let s = hooked(&db, hook.clone());
    let first = read(&db, &s).await;
    let before = snapshot(&db).await;
    for i in 0..64 {
        let session = SessionRef::parse(format!("capacity_session_{i}")).unwrap();
        s.query(&session, &db.account, &nonce()).await.unwrap();
    }
    assert!(s
        .select_card(&db.session, token(&first), 1, &nonce())
        .await
        .is_err());
    let last = read(&db, &s).await;
    hook.deny.store(true, Ordering::SeqCst);
    assert!(s.query(&db.session, &db.account, &nonce()).await.is_err());
    hook.deny.store(false, Ordering::SeqCst);
    assert!(s
        .select_card(&db.session, token(&last), 1, &nonce())
        .await
        .is_err());
    let fresh = read(&db, &s).await;
    s.select_card(&db.session, token(&fresh), 1, &nonce())
        .await
        .unwrap();
    assert_eq!(snapshot(&db).await, before);
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_corrupt_or_missing_original_denies_without_reexecution() {
    for mode in [
        "missing_receipt",
        "conflicting_receipt",
        "missing_operation",
    ] {
        let db = setup().await;
        seed(&db, "a", "public_card_a", LockSource::UserSelf, None).await;
        let s = navigation(&db);
        let v = read(&db, &s).await;
        let h = s
            .select_card(&db.session, token(&v), 1, &nonce())
            .await
            .unwrap();
        // Isolated fault injection into otherwise immutable rows. Never production.
        let mut tx = db.sql.begin().await.unwrap();
        tx.execute("ALTER TABLE card_freeze_receipts_v1 DISABLE TRIGGER ALL; ALTER TABLE card_freeze_operations_v1 DISABLE TRIGGER ALL").await.unwrap();
        match mode {
            "missing_receipt" => {
                tx.execute("DELETE FROM card_freeze_receipts_v1")
                    .await
                    .unwrap();
            }
            "conflicting_receipt" => {
                tx.execute("UPDATE card_freeze_receipts_v1 SET original_effect='REUSED_ACTIVE'")
                    .await
                    .unwrap();
            }
            _ => {
                tx.execute("DELETE FROM card_freeze_operations_v1")
                    .await
                    .unwrap();
            }
        }
        tx.execute("ALTER TABLE card_freeze_receipts_v1 ENABLE TRIGGER ALL; ALTER TABLE card_freeze_operations_v1 ENABLE TRIGGER ALL").await.unwrap();
        tx.commit().await.unwrap();
        let before = snapshot(&db).await;
        let result = s.select_record(&db.session, token(&h), 1, &nonce()).await;
        if let Ok(v) = result {
            assert_eq!(v["original"]["state"], "NEEDS_REVIEW");
        }
        assert_eq!(snapshot(&db).await, before);
        db.close().await;
    }
}
async fn wait_for_member_lock(db: &Db) {
    for _ in 0..100 {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid<>pg_backend_pid() AND wait_event_type='Lock' AND query LIKE '%SELECT owner_id,account_id,operation_id FROM card_freeze_operations_v1%')").fetch_one(&db.admin).await.unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("member SQL never reached actual Lock wait");
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_actual_member_sql_wait_future_drop_invalidates_and_rolls_back() {
    let db = setup().await;
    let before = snapshot(&db).await;
    let s = navigation(&db);
    let v = read(&db, &s).await;
    let mut lock = db.sql.begin().await.unwrap();
    lock.execute("LOCK TABLE card_freeze_operations_v1 IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let task = {
        let s = s.clone();
        let session = db.session.clone();
        let t = token(&v).to_owned();
        tokio::spawn(async move { s.select_card(&session, &t, 1, &nonce()).await })
    };
    wait_for_member_lock(&db).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lock.rollback().await.unwrap();
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    let fresh = read(&db, &s).await;
    s.select_card(&db.session, token(&fresh), 1, &nonce())
        .await
        .unwrap();
    assert_eq!(snapshot(&db).await, before);
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_actual_member_wait_source_revocation_denies_response() {
    let db = setup().await;
    let hook = Hook::new(&db);
    let s = hooked(&db, hook.clone());
    let v = read(&db, &s).await;
    let mut lock = db.sql.begin().await.unwrap();
    lock.execute("LOCK TABLE card_freeze_operations_v1 IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let task = {
        let s = s.clone();
        let session = db.session.clone();
        let t = token(&v).to_owned();
        tokio::spawn(async move { s.select_card(&session, &t, 1, &nonce()).await })
    };
    wait_for_member_lock(&db).await;
    hook.deny.store(true, Ordering::SeqCst);
    lock.rollback().await.unwrap();
    assert!(task.await.unwrap().is_err());
    assert!(s
        .select_card(&db.session, token(&v), 1, &nonce())
        .await
        .is_err());
    db.close().await;
}
#[tokio::test]
#[ignore = "isolated PostgreSQL required"]
async fn public_cards_wrong_read_boundary_member_rejected_and_shared_borrowed_path_works() {
    let db = setup().await;
    let before = snapshot(&db).await;
    let mut tx = db.sql.begin().await.unwrap();
    assert!(ReadOnlyCardList::read_on_connection(
        &mut tx,
        "catalog_test_owner",
        db.account.as_str()
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
    let mut tx = db.sql.begin().await.unwrap();
    tx.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .unwrap();
    assert!(ReadOnlyCardList::read_on_connection(
        &mut tx,
        "catalog_test_owner",
        db.account.as_str()
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
    let facts = db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap();
    assert_eq!(facts.cards.len(), 2);
    assert_eq!(snapshot(&db).await, before);
    db.close().await;
}

#[tokio::test]
#[ignore = "isolated PG; launches two sequential disposable test processes"]
async fn public_cards_process_restart_loses_reference_and_rereads_same_database() {
    let db = setup().await;
    let before = snapshot(&db).await;
    let mut previous = None;
    let mut first_cards = None;
    let mut pids = Vec::new();
    for _ in 0..2 {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "card_catalog::tests::public_read::public_cards_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("SAFEFLOW_CARD_RESTART_NAMESPACE", &db.schema);
        if let Some(ref token) = previous {
            command.env("SAFEFLOW_CARD_RESTART_PREVIOUS", token);
        }
        let output = command.output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            output.status.success(),
            "child failed: {} {}",
            text,
            String::from_utf8_lossy(&output.stderr)
        );
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("READ_PROCESS_PROBE="))
            .expect("child evidence missing");
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["old_reference_rejected"], previous.is_some());
        if let Some(ref cards) = first_cards {
            assert_eq!(&value["view"]["cards"], cards);
        } else {
            first_cards = Some(value["view"]["cards"].clone());
        }
        previous = Some(token(&value["view"]).to_owned());
        pids.push(value["pid"].clone());
    }
    assert_ne!(pids[0], pids[1]);
    println!("READ_PROCESS_RESTART_PIDS={}", serde_json::json!(pids));
    assert_eq!(snapshot(&db).await, before);
    db.close().await;
}
#[tokio::test]
#[ignore = "child-only harness: explicit isolated namespace required"]
async fn public_cards_process_child() {
    let schema =
        std::env::var("SAFEFLOW_CARD_RESTART_NAMESPACE").expect("child namespace required");
    assert!(
        schema.starts_with("sf_card_public_")
            && schema.len() == 47
            && schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    );
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    assert_eq!(options.get_host(), "127.0.0.1");
    assert_eq!(options.get_database(), Some("card_public_20261021"));
    let path = schema.clone();
    let sql = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |c, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(path)
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let repo = PgRepository::connect(&url, &schema).await.unwrap();
    let db = Db {
        repo,
        sql,
        admin: PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap(),
        schema,
        sessions: Arc::new(Sessions(AtomicU64::new(1))),
        provider: Provider::new(),
        session: SessionRef::parse("catalog_isolated_session").unwrap(),
        account: ResourceId::parse("catalog_test_account").unwrap(),
    };
    db.provider
        .set(&[("public_card_a", "0022"), ("public_card_b", "0022")]);
    let s = navigation(&db);
    let previous = std::env::var("SAFEFLOW_CARD_RESTART_PREVIOUS").ok();
    if let Some(ref old) = previous {
        assert!(s.select_card(&db.session, old, 1, &nonce()).await.is_err());
    }
    let view = read(&db, &s).await;
    println!(
        "READ_PROCESS_PROBE={}",
        serde_json::json!({"pid":std::process::id(),"old_reference_rejected":previous.is_some(),"view":view})
    );
    db.repo.pool.close().await;
    db.sql.close().await;
    db.admin.close().await;
}
