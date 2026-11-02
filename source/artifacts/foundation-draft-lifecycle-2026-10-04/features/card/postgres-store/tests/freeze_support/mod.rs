//! Isolated acceptance fixture: fictional principals/evidence, actual store SQL.
//! Only this fixture owns pools/transactions/schema/fault injection. No public
//! permit, FundsStore, live bank, network response loss or process restart here.
use safeflow_card_facts::{
    account_restrictions::{AccountUse, UnlockRoute},
    account_service::AccountCardFacts,
    operation_records::{OriginalLockEffect, StoredReasonCode},
    operation_replay::LockReplayParameters,
    LockSource,
};
use safeflow_card_postgres_store::{
    freeze::{FreezeCommand, FreezeRecordIds, FreezeSaveDisposition, FreezeSaveResult},
    CatalogCard, LockedCardAccount, StoreError,
};
use serde_json::{json, Value};
use sqlx::{
    postgres::PgConnectOptions, Connection, Executor, PgConnection, PgPool, Postgres, Transaction,
};
use std::{
    error::Error,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const WAIT: Duration = Duration::from_secs(10);
static SERIAL: AtomicU64 = AtomicU64::new(1);
pub const SAVE_CASES: [&str; 8] = [
    "first_freeze_saves_complete_records",
    "same_operation_replays_without_writes",
    "same_event_new_operation_reuses_lock",
    "distinct_events_and_scopes_keep_multiple_locks",
    "self_new_reason_keeps_one_active_lock",
    "three_sources_preserve_private_evidence",
    "parameter_and_key_conflicts_write_nothing",
    "foreign_or_missing_card_writes_nothing",
];
pub const RECOVERY_CASES: [&str; 7] = [
    "discarded_result_new_connection_recovers_original",
    "released_original_is_history_not_current_state",
    "history_is_owner_and_account_scoped",
    "missing_original_lookup_is_read_only",
    "database_error_is_not_missing_history",
    "broken_join_is_not_missing_history",
    "original_operation_reason_and_receipt_are_immutable",
];
pub const TRANSACTION_CASES: [&str; 8] = [
    "failure_after_lock_insert_rolls_back_all",
    "failure_after_reason_insert_rolls_back_all",
    "caller_rollback_removes_successful_save",
    "incomplete_operation_cannot_commit",
    "freeze_first_account_reader_waits",
    "account_reader_first_freeze_waits",
    "concurrent_same_operation_keeps_one_result",
    "concurrent_same_event_keeps_one_lock",
];
fn check(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn command(n: &str, source: LockSource, event: Option<&str>) -> FreezeCommand {
    FreezeCommand {
        operation_id: format!("OP-{n}"),
        canonical_key: format!("KEY-{n}"),
        event_scope: (source != LockSource::UserSelf).then(|| "fixture-provider-v1".into()),
        ids: FreezeRecordIds {
            lock_id: format!("L-{n}"),
            operation_record_id: format!("R-{n}"),
            reason_history_id: format!("H-{n}"),
        },
        parameters: LockReplayParameters {
            card_id: "K".into(),
            source,
            reason: match source {
                LockSource::UserSelf => StoredReasonCode::UserReportLost,
                LockSource::AnomalyDispute => StoredReasonCode::NonOwnerTransaction,
                LockSource::RiskControl => StoredReasonCode::RiskControlFlag,
            },
            event_id: event.map(str::to_owned),
            transaction_id: (source == LockSource::AnomalyDispute).then(|| "PRIVATE-TX-1".into()),
            feedback_id: (source == LockSource::AnomalyDispute)
                .then(|| "PRIVATE-FEEDBACK-1".into()),
        },
    }
}

struct Database {
    pool: PgPool,
    options: PgConnectOptions,
    schema: String,
    created: bool,
}
impl Database {
    async fn create() -> Result<Self> {
        let url = std::env::var("SAFEFLOW_CARD_STORE_TEST_DATABASE_URL")
            .map_err(|_| "dedicated freeze test database URL required")?;
        let options =
            PgConnectOptions::from_str(&url).map_err(|_| "invalid freeze test database URL")?;
        check(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1")
                && options
                    .get_database()
                    .is_some_and(|n| n.starts_with("safeflow_card_store_test")),
            "dedicated loopback freeze database required",
        )?;
        let schema = format!(
            "card_freeze_test_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        check(
            schema.len() < 64
                && schema
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid fixture schema",
        )?;
        let namespace = schema.clone();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(6)
            .acquire_timeout(WAIT)
            .after_connect(move |conn, _| {
                let schema = namespace.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(schema)
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("SET statement_timeout='10s'")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options.clone())
            .await?;
        let mut db = Self {
            pool,
            options,
            schema,
            created: false,
        };
        if let Err(error) = db.initialize().await {
            db.cleanup().await?;
            return Err(error);
        }
        Ok(db)
    }
    async fn initialize(&mut self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        conn.execute(format!("CREATE SCHEMA {}", self.schema).as_str())
            .await?;
        self.created = true;
        // Adopted public account columns, fictional rows in a PRIVATE schema.
        conn.execute("CREATE TABLE transfer_accounts_v1(account_id TEXT PRIMARY KEY,owner_id TEXT NOT NULL,opening_cents BIGINT NOT NULL CHECK(opening_cents>=0),booked_cents BIGINT NOT NULL CHECK(booked_cents>=0)); INSERT INTO transfer_accounts_v1 VALUES('A','U1',128800,128800),('B','U2',5000,5000),('C','U1',0,0);").await?;
        conn.execute(include_str!("../../sql/card_catalog_v1.proposed.sql"))
            .await?;
        conn.execute(include_str!("../../sql/card_freeze_v1.proposed.sql"))
            .await?;
        drop(conn);
        let mut tx = self.begin().await?;
        for (owner, account, cards) in [
            (
                "U1",
                "A",
                vec![
                    CatalogCard {
                        card_id: "K".into(),
                        last_four: "1234".into(),
                    },
                    CatalogCard {
                        card_id: "K2".into(),
                        last_four: "5678".into(),
                    },
                ],
            ),
            (
                "U2",
                "B",
                vec![CatalogCard {
                    card_id: "FOREIGN".into(),
                    last_four: "1234".into(),
                }],
            ),
            ("U1", "C", vec![]),
        ] {
            LockedCardAccount::acquire(&mut tx, owner, account)
                .await?
                .initialize_catalog(&cards)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    async fn begin(&self) -> Result<Transaction<'_, Postgres>> {
        begin(&self.pool).await
    }
    async fn save(&self, command: &FreezeCommand) -> Result<FreezeSaveResult> {
        let mut tx = self.begin().await?;
        let result = LockedCardAccount::acquire(&mut tx, "U1", "A")
            .await?
            .save_freeze(command)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    async fn facts(&self) -> Result<AccountCardFacts> {
        let mut tx = self.begin().await?;
        let facts = LockedCardAccount::acquire(&mut tx, "U1", "A")
            .await?
            .read_facts()
            .await?;
        tx.commit().await?;
        Ok(facts)
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        Ok(sqlx::query_as("SELECT (SELECT count(*) FROM card_locks_v1),(SELECT count(*) FROM card_freeze_operations_v1),(SELECT count(*) FROM card_freeze_reasons_v1),(SELECT count(*) FROM card_freeze_receipts_v1),(SELECT revision FROM card_account_versions_v1 WHERE account_id='A')").fetch_one(&self.pool).await?)
    }
    async fn cleanup(self) -> Result<()> {
        timeout(Duration::from_secs(15), self.pool.close())
            .await
            .map_err(|_| "freeze fixture connections still active")?;
        if self.created {
            let mut conn = PgConnection::connect_with(&self.options).await?;
            // Only the random schema successfully created by THIS fixture.
            conn.execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
                .await?;
            conn.close().await?;
        }
        Ok(())
    }
}
type Snapshot = (i64, i64, i64, i64, i64);
async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    // Fixture only. Real public write must already consume its correct permit.
    sqlx::query("SELECT txid_current()")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
fn complete_counts(s: Snapshot, locks: i64, operations: i64) -> Result<()> {
    check(
        (s.0, s.1, s.2, s.3) == (locks, operations, operations, operations),
        "unexpected lock/operation/reason/receipt counts",
    )
}
fn blocked(f: &AccountCardFacts) -> Result<()> {
    check(
        f.outgoing_restricted()
            && !f.restricts(AccountUse::Query)
            && !f.restricts(AccountUse::Receive)
            && f.restricts(AccountUse::Transfer)
            && f.restricts(AccountUse::CardSpend)
            && f.restricts(AccountUse::AutomaticDebit)
            && f.restricts(AccountUse::OtherOutgoing)
            && f.unlock_route == UnlockRoute::OfflineBank,
        "D02 or offline unlock facts differ",
    )
}

async fn save_case(db: &Database, name: &str) -> Result<Value> {
    match name {
        "first_freeze_saves_complete_records" => {
            let before = db.snapshot().await?;
            let result = db.save(&command("1", LockSource::UserSelf, None)).await?;
            let after = db.snapshot().await?;
            complete_counts(after, 1, 1)?;
            check(
                after.4 > before.4
                    && result.disposition == FreezeSaveDisposition::PersistedInCallerTransaction
                    && result.historical_receipt.original_effect == OriginalLockEffect::Created,
                "first freeze effect or revision differs",
            )?;
            blocked(&db.facts().await?)?;
            Ok(
                json!({"locks":1,"operations":1,"reasons":1,"receipts":1,"version_advanced":true,"d02_and_offline_facts_checked":true}),
            )
        }
        "same_operation_replays_without_writes" => {
            let first = db.save(&command("1", LockSource::UserSelf, None)).await?;
            let before = db.snapshot().await?;
            let mut retry = command("1", LockSource::UserSelf, None);
            retry.ids = FreezeRecordIds {
                lock_id: "unused-L".into(),
                operation_record_id: "unused-R".into(),
                reason_history_id: "unused-H".into(),
            };
            let replay = db.save(&retry).await?;
            check(
                replay.disposition == FreezeSaveDisposition::OriginalReplayed
                    && replay.historical_receipt == first.historical_receipt
                    && db.snapshot().await? == before,
                "same operation changed original result or database",
            )?;
            Ok(json!({"original_ids_retained":true,"writes":0,"version_unchanged":true}))
        }
        "same_event_new_operation_reuses_lock" => {
            let first = db
                .save(&command(
                    "1",
                    LockSource::AnomalyDispute,
                    Some("PRIVATE-E1"),
                ))
                .await?;
            let before = db.snapshot().await?;
            let second = db
                .save(&command(
                    "2",
                    LockSource::AnomalyDispute,
                    Some("PRIVATE-E1"),
                ))
                .await?;
            let after = db.snapshot().await?;
            complete_counts(after, 1, 2)?;
            check(
                first.historical_receipt.lock_id == second.historical_receipt.lock_id
                    && second.historical_receipt.original_effect
                        == OriginalLockEffect::ReusedActive
                    && after.4 > before.4,
                "same event did not retain one lock and new reason",
            )?;
            Ok(
                json!({"locks":1,"operations":2,"reasons":2,"receipts":2,"reused_original_lock":true,"version_advanced":true}),
            )
        }
        "distinct_events_and_scopes_keep_multiple_locks" => {
            db.save(&command("1", LockSource::RiskControl, Some("PRIVATE-E1")))
                .await?;
            db.save(&command("2", LockSource::RiskControl, Some("PRIVATE-E2")))
                .await?;
            let mut third = command("3", LockSource::RiskControl, Some("PRIVATE-E1"));
            third.event_scope = Some("other-fixture-provider-v1".into());
            db.save(&third).await?;
            complete_counts(db.snapshot().await?, 3, 3)?;
            let f = db.facts().await?;
            blocked(&f)?;
            check(
                f.cards
                    .iter()
                    .find(|c| c.card_id == "K")
                    .is_some_and(|c| c.active_locks.len() == 3),
                "separate event locks hidden",
            )?;
            Ok(
                json!({"independent_events_and_scopes":true,"active_locks":3,"old_locks_preserved":true}),
            )
        }
        "self_new_reason_keeps_one_active_lock" => {
            let first = db.save(&command("1", LockSource::UserSelf, None)).await?;
            let mut second = command("2", LockSource::UserSelf, None);
            second.parameters.reason = StoredReasonCode::UserSecurityConcern;
            let next = db.save(&second).await?;
            complete_counts(db.snapshot().await?, 1, 2)?;
            let reasons: Vec<String> = sqlx::query_scalar(
                "SELECT reason FROM card_freeze_reasons_v1 ORDER BY reason_history_id",
            )
            .fetch_all(&db.pool)
            .await?;
            check(
                first.historical_receipt.lock_id == next.historical_receipt.lock_id
                    && reasons == ["USER_REPORT_LOST", "USER_SECURITY_CONCERN"],
                "D21 duplicated lock or overwrote reason",
            )?;
            Ok(
                json!({"active_self_locks":1,"distinct_operations_and_reasons":2,"original_reason_retained":true}),
            )
        }
        "three_sources_preserve_private_evidence" => {
            db.save(&command("1", LockSource::UserSelf, None)).await?;
            db.save(&command(
                "2",
                LockSource::AnomalyDispute,
                Some("PRIVATE-E1"),
            ))
            .await?;
            db.save(&command("3", LockSource::RiskControl, Some("PRIVATE-E2")))
                .await?;
            complete_counts(db.snapshot().await?, 3, 3)?;
            let bound:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM card_freeze_operations_v1 WHERE source='ANOMALY_DISPUTE' AND event_scope='fixture-provider-v1' AND event_id='PRIVATE-E1' AND transaction_id='PRIVATE-TX-1' AND feedback_id='PRIVATE-FEEDBACK-1')").fetch_one(&db.pool).await?;
            let f = db.facts().await?;
            blocked(&f)?;
            let output = format!("{f:?}");
            check(
                bound
                    && ![
                        "PRIVATE-E1",
                        "PRIVATE-TX-1",
                        "PRIVATE-FEEDBACK-1",
                        "fixture-provider-v1",
                        "KEY-",
                    ]
                    .iter()
                    .any(|v| output.contains(v)),
                "private evidence missing or publicly exposed",
            )?;
            Ok(
                json!({"source_count":3,"anomaly_evidence_persisted":true,"public_projection_hides_private_evidence":true,"source_authority_is_fictional":true}),
            )
        }
        "parameter_and_key_conflicts_write_nothing" => {
            db.save(&command("1", LockSource::UserSelf, None)).await?;
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            let mut changed = command("1", LockSource::UserSelf, None);
            changed.parameters.reason = StoredReasonCode::UserSecurityConcern;
            check(
                matches!(store.save_freeze(&changed).await, Err(StoreError::Conflict)),
                "changed parameters allowed",
            )?;
            let mut alias = command("1", LockSource::UserSelf, None);
            alias.canonical_key = "unapproved-alias".into();
            check(
                matches!(
                    store.save_freeze(&alias).await,
                    Err(StoreError::RetryKeyNeedsCoordination)
                ),
                "unknown alias adopted",
            )?;
            let mut reused_key = command("2", LockSource::UserSelf, None);
            reused_key.canonical_key = "KEY-1".into();
            check(
                matches!(
                    store.save_freeze(&reused_key).await,
                    Err(StoreError::Conflict)
                ),
                "other operation reused original key",
            )?;
            drop(store);
            tx.commit().await?;
            check(
                db.snapshot().await? == before,
                "conflict path changed database even after commit",
            )?;
            Ok(
                json!({"parameter_conflict":true,"unknown_key_needs_coordination":true,"different_operation_same_key_conflict":true,"writes":0}),
            )
        }
        "foreign_or_missing_card_writes_nothing" => {
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            check(
                matches!(
                    LockedCardAccount::acquire(&mut tx, "U2", "A").await,
                    Err(StoreError::ResourceUnavailable)
                ),
                "foreign owner acquired account",
            )?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            for card in ["FOREIGN", "MISSING", "1234"] {
                let mut c = command("1", LockSource::UserSelf, None);
                c.parameters.card_id = card.into();
                check(
                    matches!(
                        store.save_freeze(&c).await,
                        Err(StoreError::ResourceUnavailable)
                    ),
                    "foreign/missing/tail reference saved",
                )?;
            }
            let mut invalid = command("1", LockSource::AnomalyDispute, Some("PRIVATE-E1"));
            invalid.parameters.feedback_id = None;
            check(
                matches!(
                    store.save_freeze(&invalid).await,
                    Err(StoreError::InvalidInput)
                ),
                "missing evidence shape accepted",
            )?;
            drop(store);
            tx.commit().await?;
            check(
                db.snapshot().await? == before,
                "rejected input left changes",
            )?;
            Ok(
                json!({"foreign_owner_rejected":true,"foreign_missing_and_tail_card_rejected":true,"incomplete_evidence_rejected":true,"writes":0}),
            )
        }
        _ => Err("unknown freeze save case".into()),
    }
}

async fn recovery_case(db: &Database, name: &str) -> Result<Value> {
    match name {
        "discarded_result_new_connection_recovers_original" => {
            let mut first = db.begin().await?;
            let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *first)
                .await?;
            // Intentionally discard only the returned Rust value. No simulated
            // TCP loss or server crash is claimed; commit succeeds normally.
            LockedCardAccount::acquire(&mut first, "U1", "A")
                .await?
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await?;
            first.commit().await?;
            let before = db.snapshot().await?;
            let mut connection = PgConnection::connect_with(&db.options).await?;
            sqlx::query("SELECT set_config('search_path',$1,false)")
                .bind(&db.schema)
                .execute(&mut connection)
                .await?;
            sqlx::query("SET statement_timeout='10s'")
                .execute(&mut connection)
                .await?;
            let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut connection)
                .await?;
            let mut tx = connection.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SELECT txid_current()")
                .execute(&mut *tx)
                .await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            let r = store
                .read_original_freeze("OP-1")
                .await?
                .ok_or("original not recovered")?;
            check(
                first_pid != second_pid
                    && r.operation_record_id == "R-1"
                    && r.lock_id == "L-1"
                    && r.reason_history_id == "H-1"
                    && r.original_effect == OriginalLockEffect::Created,
                "new connection did not recover original IDs",
            )?;
            let replay = store
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await?;
            check(
                replay.disposition == FreezeSaveDisposition::OriginalReplayed
                    && replay.historical_receipt == r,
                "recovery reexecuted operation",
            )?;
            drop(store);
            tx.commit().await?;
            connection.close().await?;
            check(
                db.snapshot().await? == before,
                "recovery changed stored rows or version",
            )?;
            Ok(
                json!({"returned_value_discarded":true,"new_physical_connection":true,"new_store_instance":true,"original_ids_recovered":true,"recovery_writes":0,"process_or_database_restart_tested":false,"network_response_loss_tested":false}),
            )
        }
        "released_original_is_history_not_current_state" => {
            let first = db.save(&command("1", LockSource::UserSelf, None)).await?;
            // Fixture-only history setup; never a production release entry.
            let mut setup = db.begin().await?;
            sqlx::query("UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id='L-1'")
                .execute(&mut *setup)
                .await?;
            setup.commit().await?;
            let mut other = command("2", LockSource::RiskControl, Some("PRIVATE-E2"));
            other.parameters.card_id = "K2".into();
            db.save(&other).await?;
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            let r = store
                .read_original_freeze("OP-1")
                .await?
                .ok_or("released original disappeared")?;
            check(
                r == first.historical_receipt,
                "release rewrote historical effect",
            )?;
            let replay = store
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await?;
            check(
                replay.disposition == FreezeSaveDisposition::OriginalReplayed,
                "released history re-froze",
            )?;
            check(
                matches!(
                    store
                        .save_freeze(&command("3", LockSource::UserSelf, None))
                        .await,
                    Err(StoreError::NeedsCoordination)
                ),
                "new operation implicitly reopened released lock",
            )?;
            let facts = store.read_facts().await?;
            blocked(&facts)?;
            check(
                facts.active_card_lock_ids == ["L-2"]
                    && facts
                        .cards
                        .iter()
                        .find(|c| c.card_id == "K")
                        .is_some_and(|c| c.active_locks.is_empty()),
                "old original treated as current or other card ignored",
            )?;
            drop(store);
            tx.commit().await?;
            check(
                db.snapshot().await? == before,
                "history read/replay wrote changes",
            )?;
            Ok(
                json!({"original_created_effect_retained":true,"old_lock_not_active":true,"other_card_lock_still_blocks_account":true,"new_reopening_needs_coordination":true,"writes":0,"production_release_entry_used":false}),
            )
        }
        "history_is_owner_and_account_scoped" => {
            db.save(&command("1", LockSource::UserSelf, None)).await?;
            let before = db.snapshot().await?;
            for (owner, account) in [("U1", "C"), ("U2", "B")] {
                let mut tx = db.begin().await?;
                check(
                    LockedCardAccount::acquire(&mut tx, owner, account)
                        .await?
                        .read_original_freeze("OP-1")
                        .await?
                        .is_none(),
                    "history escaped owner/account scope",
                )?;
                tx.commit().await?;
            }
            let mut tx = db.begin().await?;
            check(
                matches!(
                    LockedCardAccount::acquire(&mut tx, "U2", "A").await,
                    Err(StoreError::ResourceUnavailable)
                ),
                "foreign account acquired",
            )?;
            tx.commit().await?;
            check(
                db.snapshot().await? == before,
                "scoped history lookup wrote",
            )?;
            Ok(
                json!({"foreign_owner_history_hidden":true,"same_owner_other_account_history_hidden":true,"writes":0}),
            )
        }
        "missing_original_lookup_is_read_only" => {
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            for _ in 0..2 {
                check(
                    store.read_original_freeze("MISSING").await?.is_none(),
                    "missing original invented",
                )?;
            }
            check(
                matches!(
                    store.read_original_freeze(" ").await,
                    Err(StoreError::InvalidInput)
                ),
                "blank original accepted",
            )?;
            drop(store);
            tx.commit().await?;
            check(
                db.snapshot().await? == before,
                "missing lookup created records",
            )?;
            Ok(
                json!({"complete_missing_lookup":true,"no_new_operation_or_retry":true,"writes":0,"missing_does_not_prove_uncertain_operation_failed":true}),
            )
        }
        "database_error_is_not_missing_history" => {
            db.save(&command("1", LockSource::UserSelf, None)).await?;
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            sqlx::query(
                "ALTER TABLE card_freeze_receipts_v1 RENAME TO temporarily_unavailable_receipts",
            )
            .execute(&mut *tx)
            .await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            check(
                matches!(
                    store.read_original_freeze("OP-1").await,
                    Err(StoreError::Database(_))
                ),
                "SQL error became missing history",
            )?;
            drop(store);
            tx.rollback().await?;
            check(
                db.snapshot().await? == before,
                "fault setup was not rolled back",
            )?;
            Ok(
                json!({"database_error_propagated":true,"not_returned_as_none":true,"fixture_fault_rolled_back":true}),
            )
        }
        "broken_join_is_not_missing_history" => {
            db.save(&command("1", LockSource::UserSelf, None)).await?;
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            // Deliberately corrupt our PRIVATE fixture, roll it all back below.
            // This is not a supported write path or permission bypass in code.
            sqlx::query("ALTER TABLE card_freeze_receipts_v1 DISABLE TRIGGER USER")
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM card_freeze_receipts_v1 WHERE operation_record_id='R-1'")
                .execute(&mut *tx)
                .await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            check(
                matches!(
                    store.read_original_freeze("OP-1").await,
                    Err(StoreError::InconsistentRecord)
                ),
                "broken join disappeared as missing",
            )?;
            drop(store);
            tx.rollback().await?;
            check(
                db.snapshot().await? == before,
                "fixture corruption remained",
            )?;
            Ok(
                json!({"broken_association_rejected":true,"left_join_does_not_hide_operation":true,"fixture_corruption_rolled_back":true}),
            )
        }
        "original_operation_reason_and_receipt_are_immutable" => {
            db.save(&command("1", LockSource::UserSelf, None)).await?;
            let before = db.snapshot().await?;
            for statement in [
                "UPDATE card_freeze_operations_v1 SET original_key='changed' WHERE record_id='R-1'",
                "UPDATE card_freeze_reasons_v1 SET reason='USER_SECURITY_CONCERN' WHERE reason_history_id='H-1'",
                "UPDATE card_freeze_receipts_v1 SET original_effect='REUSED_ACTIVE' WHERE operation_record_id='R-1'",
            ] {
                let mut tx=db.begin().await?;
                check(sqlx::query(statement).execute(&mut *tx).await.is_err(),"historical record was rewritten")?;
                tx.rollback().await?;
            }
            check(
                db.snapshot().await? == before,
                "rejected history rewrite changed rows",
            )?;
            Ok(
                json!({"operation_immutable":true,"reason_immutable":true,"receipt_immutable":true,"writes":0}),
            )
        }
        _ => Err("unknown freeze recovery case".into()),
    }
}

// Dropping a case on error/timeout also cancels its waiter, so fixture cleanup
// cannot silently leave a background database writer alive.
struct Worker<T>(JoinHandle<Result<T>>);
impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl<T> Worker<T> {
    async fn finish(mut self) -> Result<T> {
        timeout(WAIT, &mut self.0).await??
    }
}
async fn observe_wait(pool: &PgPool, waiter: i32, blocker: i32) -> Result<()> {
    timeout(WAIT,async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND $2=ANY(pg_blocking_pids(pid)))")
                .bind(waiter).bind(blocker).fetch_one(pool).await?;
            if waiting { return Ok::<(),sqlx::Error>(()); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.map_err(|_|"actual PostgreSQL blocking wait not observed")??;
    Ok(())
}
async fn write_worker(
    pool: PgPool,
    c: FreezeCommand,
    signal: oneshot::Sender<i32>,
) -> Result<FreezeSaveResult> {
    let mut tx = begin(&pool).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await?;
    signal.send(pid).map_err(|_| "writer signal dropped")?;
    let result = LockedCardAccount::acquire(&mut tx, "U1", "A")
        .await?
        .save_freeze(&c)
        .await?;
    tx.commit().await?;
    Ok(result)
}
async fn concurrent_writes(db: &Database, same_operation: bool) -> Result<Value> {
    let before = db.snapshot().await?;
    let mut first = db.begin().await?;
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first)
        .await?;
    let original = LockedCardAccount::acquire(&mut first, "U1", "A")
        .await?
        .save_freeze(&command("1", LockSource::RiskControl, Some("PRIVATE-E1")))
        .await?;
    let mut second = command(
        if same_operation { "1" } else { "2" },
        LockSource::RiskControl,
        Some("PRIVATE-E1"),
    );
    if same_operation {
        second.ids = FreezeRecordIds {
            lock_id: "unused-concurrent-L".into(),
            operation_record_id: "unused-concurrent-R".into(),
            reason_history_id: "unused-concurrent-H".into(),
        };
    }
    let (signal, ready) = oneshot::channel();
    let worker = Worker(tokio::spawn(write_worker(db.pool.clone(), second, signal)));
    let waiter = timeout(WAIT, ready).await??;
    observe_wait(&db.pool, waiter, blocker).await?;
    first.commit().await?;
    let result = worker.finish().await?;
    let after = db.snapshot().await?;
    complete_counts(after, 1, if same_operation { 1 } else { 2 })?;
    check(
        original.historical_receipt.lock_id == result.historical_receipt.lock_id,
        "concurrent write duplicated event lock",
    )?;
    if same_operation {
        check(
            result.disposition == FreezeSaveDisposition::OriginalReplayed
                && result.historical_receipt == original.historical_receipt
                && after.4 == before.4 + 2,
            "concurrent replay changed original/version",
        )?;
    } else {
        check(
            result.disposition == FreezeSaveDisposition::PersistedInCallerTransaction
                && result.historical_receipt.original_effect == OriginalLockEffect::ReusedActive
                && after.4 == before.4 + 3,
            "concurrent new operation did not reuse lock and retain reason",
        )?;
    }
    Ok(
        json!({"database_lock_wait_verified":true,"fresh_read_after_wait":true,"locks":1,"operations":if same_operation {1}else{2},"original_replayed":same_operation,"public_owner_or_permit_integration_tested":false}),
    )
}
async fn transaction_case(db: &Database, name: &str) -> Result<Value> {
    match name {
        "failure_after_lock_insert_rolls_back_all" => {
            // Fault at the next insert AFTER the real function created a lock.
            db.pool.execute("CREATE FUNCTION fixture_fail_operation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'FIXTURE_FAIL_AFTER_LOCK'; END; $$; CREATE TRIGGER fixture_fail_operation BEFORE INSERT ON card_freeze_operations_v1 FOR EACH ROW EXECUTE FUNCTION fixture_fail_operation();").await?;
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            let result = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await;
            check(
                matches!(result, Err(StoreError::Database(_))),
                "operation insertion fault was not propagated",
            )?;
            tx.rollback().await?;
            check(
                db.snapshot().await? == before,
                "partial lock/version remained after operation failure",
            )?;
            Ok(
                json!({"fault_after_actual_lock_insert":true,"caller_rollback":true,"all_rows_and_version_restored":true}),
            )
        }
        "failure_after_reason_insert_rolls_back_all" => {
            db.pool.execute("CREATE FUNCTION fixture_fail_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'FIXTURE_FAIL_AFTER_REASON'; END; $$; CREATE TRIGGER fixture_fail_receipt BEFORE INSERT ON card_freeze_receipts_v1 FOR EACH ROW EXECUTE FUNCTION fixture_fail_receipt();").await?;
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            sqlx::query("SAVEPOINT fixture_before_freeze")
                .execute(&mut *tx)
                .await?;
            let result = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await;
            check(
                matches!(result, Err(StoreError::Database(_))),
                "receipt insertion fault was not propagated",
            )?;
            // Test-only recovery to prove statement atomicity even after caller
            // recovers a savepoint and commits. Production must propagate error.
            sqlx::query("ROLLBACK TO SAVEPOINT fixture_before_freeze")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            check(
                db.snapshot().await? == before,
                "partial lock/operation/reason/version survived failed statement",
            )?;
            Ok(
                json!({"fault_after_actual_reason_insert":true,"test_only_savepoint_recovery_and_commit":true,"all_rows_and_version_restored":true}),
            )
        }
        "caller_rollback_removes_successful_save" => {
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            let result = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await?;
            check(
                result.disposition == FreezeSaveDisposition::PersistedInCallerTransaction
                    && db.snapshot().await? == before,
                "uncommitted save was visible from another connection",
            )?;
            tx.rollback().await?;
            check(
                db.snapshot().await? == before,
                "successful transaction-local save survived caller rollback",
            )?;
            Ok(
                json!({"save_returned_before_commit":true,"other_connection_cannot_see_uncommitted_rows":true,"caller_rollback_removed_rows_and_versions":true}),
            )
        }
        "incomplete_operation_cannot_commit" => {
            let before = db.snapshot().await?;
            let mut tx = db.begin().await?;
            sqlx::query(
                "SELECT account_id FROM transfer_accounts_v1 WHERE account_id='A' FOR UPDATE",
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO card_locks_v1(lock_id,card_id,source,state) VALUES('L-incomplete','K','USER_SELF','ACTIVE')").execute(&mut *tx).await?;
            sqlx::query("INSERT INTO card_freeze_operations_v1(record_id,owner_id,account_id,operation_id,card_id,lock_id,source,reason,original_key) VALUES('R-incomplete','U1','A','OP-incomplete','K','L-incomplete','USER_SELF','USER_REPORT_LOST','KEY-incomplete')").execute(&mut *tx).await?;
            let committed = tx.commit().await;
            check(
                committed
                    .as_ref()
                    .err()
                    .and_then(|e| e.as_database_error())
                    .is_some_and(|e| {
                        e.message()
                            .contains("CARD_FREEZE_ORIGINAL_RESULT_INCOMPLETE")
                    }),
                "incomplete operation commit was not rejected by completeness guard",
            )?;
            check(
                db.snapshot().await? == before,
                "failed commit left incomplete operation",
            )?;
            Ok(
                json!({"missing_reason_and_receipt_rejected_at_commit":true,"failed_commit_restored_all_rows_and_version":true}),
            )
        }
        "freeze_first_account_reader_waits" => {
            let mut freeze = db.begin().await?;
            let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *freeze)
                .await?;
            LockedCardAccount::acquire(&mut freeze, "U1", "A")
                .await?
                .save_freeze(&command("1", LockSource::UserSelf, None))
                .await?;
            let (signal, ready) = oneshot::channel();
            let pool = db.pool.clone();
            let reader = Worker(tokio::spawn(async move {
                let mut tx = begin(&pool).await?;
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await?;
                signal.send(pid).map_err(|_| "reader signal dropped")?;
                let facts = LockedCardAccount::acquire(&mut tx, "U1", "A")
                    .await?
                    .read_facts()
                    .await?;
                tx.commit().await?;
                Ok(facts)
            }));
            let waiter = timeout(WAIT, ready).await??;
            observe_wait(&db.pool, waiter, blocker).await?;
            freeze.commit().await?;
            blocked(&reader.finish().await?)?;
            Ok(
                json!({"database_lock_wait_verified":true,"freeze_committed_before_reader_continued":true,"reader_observed_outgoing_restricted":true,"real_payment_or_funds_store_executed":false}),
            )
        }
        "account_reader_first_freeze_waits" => {
            let mut read = db.begin().await?;
            let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *read)
                .await?;
            let mut store = LockedCardAccount::acquire(&mut read, "U1", "A").await?;
            check(
                !store.read_facts().await?.outgoing_restricted(),
                "fixture was not initially unlocked",
            )?;
            let (signal, ready) = oneshot::channel();
            let writer = Worker(tokio::spawn(write_worker(
                db.pool.clone(),
                command("1", LockSource::UserSelf, None),
                signal,
            )));
            let waiter = timeout(WAIT, ready).await??;
            observe_wait(&db.pool, waiter, blocker).await?;
            check(
                !store.read_facts().await?.outgoing_restricted(),
                "waiting freeze became effective before account reader commit",
            )?;
            drop(store);
            read.commit().await?;
            writer.finish().await?;
            blocked(&db.facts().await?)?;
            Ok(
                json!({"database_lock_wait_verified":true,"account_guard_retained_until_commit":true,"freeze_saved_after_reader_commit":true,"later_reader_observed_restriction":true,"real_payment_or_funds_store_executed":false}),
            )
        }
        "concurrent_same_operation_keeps_one_result" => concurrent_writes(db, true).await,
        "concurrent_same_event_keeps_one_lock" => concurrent_writes(db, false).await,
        _ => Err("unknown freeze transaction case".into()),
    }
}

pub async fn run_case(name: &str) -> Result<Value> {
    check(
        SAVE_CASES.contains(&name)
            || RECOVERY_CASES.contains(&name)
            || TRANSACTION_CASES.contains(&name),
        "unknown freeze acceptance case",
    )?;
    let db = Database::create().await?;
    let exercise = async {
        if SAVE_CASES.contains(&name) {
            save_case(&db, name).await
        } else if RECOVERY_CASES.contains(&name) {
            recovery_case(&db, name).await
        } else {
            transaction_case(&db, name).await
        }
    };
    let outcome = timeout(Duration::from_secs(45), exercise).await;
    // All transaction/worker futures have been dropped before schema cleanup.
    db.cleanup().await?;
    let detail = outcome.map_err(|_| "freeze acceptance case timeout")??;
    Ok(
        json!({"case":name,"verified":true,"actual_postgresql":true,"fictional_source_data":true,"detail":detail}),
    )
}
