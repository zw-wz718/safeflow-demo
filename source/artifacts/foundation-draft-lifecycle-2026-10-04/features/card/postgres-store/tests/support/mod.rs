//! Only this test/example fixture owns a pool, schema and transactions.
//! The actual store above never creates/commits a transaction or installs DDL.
use safeflow_card_facts::account_restrictions::{AccountUse, UnlockRoute};
use safeflow_card_postgres_store::{CatalogCard, LockedCardAccount, StoreError};
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
use tokio::{sync::oneshot, time::timeout};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const WAIT: Duration = Duration::from_secs(10);
static SERIAL: AtomicU64 = AtomicU64::new(1);
pub const CASES: [&str; 14] = [
    "catalog_persists_and_is_idempotent",
    "missing_catalog_differs_from_complete_empty",
    "ownership_and_missing_account_rejected",
    "invalid_catalog_has_no_partial_initialization",
    "register_card_advances_version_and_rejects_conflicts",
    "multiple_sources_and_released_history",
    "lock_change_then_restore_advances_version",
    "failed_save_rolls_back_catalog_and_version",
    "account_guard_blocks_a_later_lock_insert",
    "database_failure_is_not_empty",
    "mapping_and_owner_rewrite_are_disabled",
    "caller_transaction_and_isolation_required",
    "all_cards_and_locks_without_truncation",
    "active_lock_unique_constraints",
];
fn check(ok: bool, label: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(label.into())
    }
}
fn card(id: &str) -> CatalogCard {
    CatalogCard {
        card_id: id.into(),
        last_four: "1234".into(),
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
            .map_err(|_| "dedicated card store test URL required")?;
        let options = PgConnectOptions::from_str(&url).map_err(|_| "invalid test URL")?;
        check(
            matches!(options.get_host(), "127.0.0.1" | "localhost" | "::1")
                && options
                    .get_database()
                    .is_some_and(|v| v.starts_with("safeflow_card_store_test")),
            "dedicated loopback test database required",
        )?;
        let schema = format!(
            "card_store_test_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        check(
            schema.len() < 64
                && schema
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid private schema",
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
        let setup = db.initialize().await;
        if let Err(error) = setup {
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
        // Same adopted account columns/constraints; only fictitious test rows.
        conn.execute("CREATE TABLE transfer_accounts_v1(account_id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, opening_cents BIGINT NOT NULL CHECK(opening_cents>=0), booked_cents BIGINT NOT NULL CHECK(booked_cents>=0)); INSERT INTO transfer_accounts_v1 VALUES('A','U1',128800,128800),('B','U2',5000,5000),('C','U1',0,0);").await?;
        conn.execute(include_str!("../../sql/card_catalog_v1.proposed.sql"))
            .await?;
        Ok(())
    }
    async fn cleanup(self) -> Result<()> {
        timeout(Duration::from_secs(15), self.pool.close())
            .await
            .map_err(|_| "fixture connections still active")?;
        if self.created {
            let mut conn = PgConnection::connect_with(&self.options).await?;
            conn.execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
                .await?;
            conn.close().await?;
        }
        Ok(())
    }
    async fn begin(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        // In production public writes/consume already assign the transaction.
        let _: i64 = sqlx::query_scalar("SELECT txid_current()::bigint")
            .fetch_one(&mut *tx)
            .await?;
        Ok(tx)
    }
    async fn seed(&self) -> Result<()> {
        let mut tx = self.begin().await?;
        LockedCardAccount::acquire(&mut tx, "U1", "A")
            .await?
            .initialize_catalog(&[card("K")])
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn count(pool: &PgPool, table: &str) -> Result<i64> {
    check(
        matches!(
            table,
            "card_catalog_v1" | "card_locks_v1" | "card_account_versions_v1"
        ),
        "fixture table not allowed",
    )?;
    Ok(
        sqlx::query_scalar(format!("SELECT count(*) FROM {table}").as_str())
            .fetch_one(pool)
            .await?,
    )
}
async fn revision(pool: &PgPool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT revision FROM card_account_versions_v1 WHERE account_id='A'")
            .fetch_one(pool)
            .await?,
    )
}
async fn insert_self(conn: &mut PgConnection, id: &str) -> Result<()> {
    sqlx::query("INSERT INTO card_locks_v1(lock_id,card_id,source,state) VALUES($1,'K','USER_SELF','ACTIVE')").bind(id).execute(conn).await?;
    Ok(())
}

async fn exercise(db: &Database, case: &str) -> Result<Value> {
    match case {
        "catalog_persists_and_is_idempotent" => {
            db.seed().await?;
            let original = revision(&db.pool).await?;
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            let replay = store.initialize_catalog(&[card("K")]).await?;
            let facts = store.read_facts().await?;
            check(
                facts.cards.len() == 1
                    && facts.cards[0].card_id == "K"
                    && facts.account_id == "A"
                    && facts.revision == replay,
                "persisted card facts differ",
            )?;
            drop(store);
            tx.commit().await?;
            check(
                revision(&db.pool).await? == original,
                "catalog retry changed version",
            )?;
            Ok(
                json!({"cards":1,"original_version":replay,"new_connection_read":true,"same_catalog_no_new_rows_or_version":true}),
            )
        }
        "missing_catalog_differs_from_complete_empty" => {
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            check(
                matches!(
                    store.read_facts().await,
                    Err(StoreError::CatalogUninitialized)
                ),
                "unknown catalog treated as empty",
            )?;
            check(
                matches!(
                    store.register_card(&card("K")).await,
                    Err(StoreError::CatalogUninitialized)
                ),
                "one mapping initialized unknown catalog",
            )?;
            store.initialize_catalog(&[]).await?;
            let facts = store.read_facts().await?;
            check(
                facts.cards.is_empty() && !facts.outgoing_restricted(),
                "complete empty catalog invalid",
            )?;
            drop(store);
            tx.commit().await?;
            Ok(
                json!({"missing":"CatalogUninitialized","complete_empty_cards":0,"payment_permission_granted":false}),
            )
        }
        "ownership_and_missing_account_rejected" => {
            for (owner, account) in [("U2", "A"), ("U1", "B"), ("U1", "absent"), ("U1", "1234")] {
                let mut tx = db.begin().await?;
                check(
                    matches!(
                        LockedCardAccount::acquire(&mut tx, owner, account).await,
                        Err(StoreError::ResourceUnavailable)
                    ),
                    "foreign/missing resource leaked",
                )?;
                tx.rollback().await?;
            }
            check(
                count(&db.pool, "card_account_versions_v1").await? == 0,
                "rejected owner wrote catalog",
            )?;
            Ok(json!({"foreign_missing_and_tail_rejected":true,"new_accounts_created":0}))
        }
        "invalid_catalog_has_no_partial_initialization" => {
            for input in [
                vec![card("K"), card("K")],
                vec![card("K"), card(" ")],
                vec![CatalogCard {
                    card_id: "K".into(),
                    last_four: "12x4".into(),
                }],
            ] {
                let mut tx = db.begin().await?;
                let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
                check(
                    matches!(
                        store.initialize_catalog(&input).await,
                        Err(StoreError::InvalidInput)
                    ),
                    "invalid complete catalog accepted",
                )?;
                drop(store);
                tx.commit().await?; // even swallowing preflight errors writes nothing
            }
            check(
                count(&db.pool, "card_catalog_v1").await? == 0
                    && count(&db.pool, "card_account_versions_v1").await? == 0,
                "partial catalog persisted",
            )?;
            Ok(json!({"invalid_inputs_rejected_before_writes":true}))
        }
        "register_card_advances_version_and_rejects_conflicts" => {
            db.seed().await?;
            let before = revision(&db.pool).await?;
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "A").await?;
            let added = store.register_card(&card("K2")).await?;
            check(
                store.register_card(&card("K2")).await? == added,
                "duplicate new mapping changed version",
            )?;
            check(
                matches!(
                    store
                        .register_card(&CatalogCard {
                            card_id: "K2".into(),
                            last_four: "9999".into()
                        })
                        .await,
                    Err(StoreError::Conflict)
                ),
                "silent tail replacement",
            )?;
            check(
                matches!(
                    store.initialize_catalog(&[card("K")]).await,
                    Err(StoreError::Conflict)
                ),
                "incomplete replacement accepted",
            )?;
            drop(store);
            tx.commit().await?;
            let mut tx = db.begin().await?;
            let mut store = LockedCardAccount::acquire(&mut tx, "U1", "C").await?;
            store.initialize_catalog(&[]).await?;
            check(
                matches!(
                    store.register_card(&card("K")).await,
                    Err(StoreError::Conflict)
                ),
                "card moved across accounts",
            )?;
            drop(store);
            tx.commit().await?;
            check(
                revision(&db.pool).await? == before + 1,
                "mapping version count differs",
            )?;
            Ok(
                json!({"new_mapping_version_increments":1,"duplicate_noop":true,"tail_and_account_rebind_rejected":true}),
            )
        }
        "multiple_sources_and_released_history" => {
            db.seed().await?;
            let mut tx = db.begin().await?;
            insert_self(&mut tx, "L1").await?;
            sqlx::query("INSERT INTO card_locks_v1 VALUES('L2','K','ANOMALY_DISPUTE','ACTIVE','feedback-fixture','E1'),('L3','K','RISK_CONTROL','ACTIVE','risk-fixture','E2'),('old','K','RISK_CONTROL','RELEASED','risk-fixture','E0')").execute(&mut *tx).await?;
            let facts = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .read_facts()
                .await?;
            check(
                facts.active_card_lock_ids.len() == 3 && facts.cards[0].active_locks.len() == 3,
                "active locks missing",
            )?;
            for usage in [
                AccountUse::Transfer,
                AccountUse::CardSpend,
                AccountUse::AutomaticDebit,
                AccountUse::OtherOutgoing,
            ] {
                check(facts.restricts(usage), "outgoing rule changed")?;
            }
            check(
                !facts.restricts(AccountUse::Receive)
                    && !facts.restricts(AccountUse::Query)
                    && facts.unlock_route == UnlockRoute::OfflineBank,
                "D02 read/receive/offline rule changed",
            )?;
            check(
                !format!("{facts:?}").contains("risk-fixture"),
                "private event scope leaked",
            )?;
            tx.commit().await?;
            Ok(
                json!({"stored_locks":4,"active_locks":3,"outgoing_restricted":true,"query_and_receive_restricted":false,"unlock_route":"OfflineBank"}),
            )
        }
        "lock_change_then_restore_advances_version" => {
            db.seed().await?;
            let before = revision(&db.pool).await?;
            let mut tx = db.begin().await?;
            insert_self(&mut tx, "L1").await?;
            sqlx::query("UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id='L1'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE card_locks_v1 SET state='ACTIVE' WHERE lock_id='L1'")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            check(
                revision(&db.pool).await? == before + 3,
                "ABA did not advance stable version",
            )?;
            let mut tx = db.begin().await?;
            let reset =
                sqlx::query("UPDATE card_account_versions_v1 SET revision=1 WHERE account_id='A'")
                    .execute(&mut *tx)
                    .await;
            check(reset.is_err(), "version reset allowed")?;
            tx.rollback().await?;
            Ok(
                json!({"version_advances":3,"same_final_lock_state_does_not_restore_old_version":true,"reset_rejected":true,"production_release_entry_used":false}),
            )
        }
        "failed_save_rolls_back_catalog_and_version" => {
            let mut tx = db.begin().await?;
            LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .initialize_catalog(&[card("K"), card("K2")])
                .await?;
            let failed = sqlx::query("INSERT INTO card_catalog_v1 VALUES('bad','A','12x4')")
                .execute(&mut *tx)
                .await;
            check(failed.is_err(), "fault injection did not fail")?;
            tx.rollback().await?;
            check(
                count(&db.pool, "card_catalog_v1").await? == 0
                    && count(&db.pool, "card_account_versions_v1").await? == 0,
                "partial save remained",
            )?;
            Ok(json!({"injected_failure":true,"caller_rollback_removed_cards_and_revision":true}))
        }
        "account_guard_blocks_a_later_lock_insert" => {
            db.seed().await?;
            let mut read = db.begin().await?;
            let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *read)
                .await?;
            let mut store = LockedCardAccount::acquire(&mut read, "U1", "A").await?;
            check(
                !store.read_facts().await?.outgoing_restricted(),
                "fixture initially locked",
            )?;
            let (signal, ready) = oneshot::channel();
            let pool = db.pool.clone();
            let writer = tokio::spawn(async move {
                let mut tx = pool.begin().await?;
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await?;
                signal.send(pid).map_err(|_| "fixture signal dropped")?;
                insert_self(&mut tx, "L1").await?;
                tx.commit().await?;
                Ok::<(), Box<dyn Error + Send + Sync>>(())
            });
            let waiting = timeout(WAIT, ready).await??;
            let observed=timeout(WAIT,async {loop {
                let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND $2=ANY(pg_blocking_pids(pid)))").bind(waiting).bind(blocker).fetch_one(&db.pool).await?;
                if blocked {return Ok::<_,sqlx::Error>(());}tokio::time::sleep(Duration::from_millis(10)).await;
            }}).await;
            if observed.is_err() {
                writer.abort();
                return Err("actual account guard wait not observed".into());
            }
            observed??;
            check(
                !store.read_facts().await?.outgoing_restricted(),
                "uncommitted waiting lock became visible",
            )?;
            drop(store);
            read.commit().await?;
            timeout(WAIT, writer).await???;
            let mut tx = db.begin().await?;
            let facts = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .read_facts()
                .await?;
            check(facts.outgoing_restricted(), "committed new lock not read")?;
            tx.commit().await?;
            Ok(
                json!({"database_lock_wait_verified":true,"zero_locks_then_new_lock_protected":true,"later_read_outgoing_restricted":true}),
            )
        }
        "database_failure_is_not_empty" => {
            db.seed().await?;
            let mut tx = db.begin().await?;
            sqlx::query("ALTER TABLE card_locks_v1 RENAME TO unavailable_locks_fixture")
                .execute(&mut *tx)
                .await?;
            let result = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .read_facts()
                .await;
            check(
                matches!(result, Err(StoreError::Database(_))),
                "database error became normal/empty facts",
            )?;
            tx.rollback().await?;
            Ok(json!({"database_error_propagated":true,"empty_facts_returned":false}))
        }
        "mapping_and_owner_rewrite_are_disabled" => {
            db.seed().await?;
            for sql in [
                "UPDATE card_catalog_v1 SET account_id='C' WHERE card_id='K'",
                "UPDATE transfer_accounts_v1 SET owner_id='U2' WHERE account_id='A'",
                "DELETE FROM card_account_versions_v1 WHERE account_id='A'",
            ] {
                let mut tx = db.begin().await?;
                check(
                    sqlx::query(sql).execute(&mut *tx).await.is_err(),
                    "unsupported identity rewrite accepted",
                )?;
                tx.rollback().await?;
            }
            Ok(
                json!({"cross_account_and_cross_owner_changes_disabled":true,"revision_history_deletion_disabled":true}),
            )
        }
        "caller_transaction_and_isolation_required" => {
            let mut conn = db.pool.acquire().await?;
            check(
                matches!(
                    LockedCardAccount::acquire(&mut conn, "U1", "A").await,
                    Err(StoreError::TransactionRequired)
                ),
                "autocommit accepted",
            )?;
            drop(conn);
            let mut tx = db.pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
            let _: i64 = sqlx::query_scalar("SELECT txid_current()::bigint")
                .fetch_one(&mut *tx)
                .await?;
            check(
                matches!(
                    LockedCardAccount::acquire(&mut tx, "U1", "A").await,
                    Err(StoreError::TransactionRequired)
                ),
                "stale snapshot isolation accepted",
            )?;
            tx.rollback().await?;
            check(
                count(&db.pool, "card_account_versions_v1").await? == 0,
                "invalid transaction wrote data",
            )?;
            Ok(
                json!({"autocommit_and_repeatable_read_rejected":true,"transaction_check_is_not_authentication":true}),
            )
        }
        "all_cards_and_locks_without_truncation" => {
            let cards: Vec<_> = (0..40).map(|n| card(&format!("K{n:02}"))).collect();
            let mut tx = db.begin().await?;
            LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .initialize_catalog(&cards)
                .await?;
            for (n, c) in cards.iter().enumerate() {
                sqlx::query("INSERT INTO card_locks_v1 VALUES($1,$2,'RISK_CONTROL','ACTIVE','risk-fixture',$3)").bind(format!("L{n:02}")).bind(&c.card_id).bind(format!("E{n:02}")).execute(&mut *tx).await?;
            }
            let facts = LockedCardAccount::acquire(&mut tx, "U1", "A")
                .await?
                .read_facts()
                .await?;
            check(
                facts.cards.len() == 40 && facts.active_card_lock_ids.len() == 40,
                "database read truncated cards or locks",
            )?;
            tx.commit().await?;
            Ok(json!({"cards":40,"active_locks":40,"later_cards_and_locks_preserved":true}))
        }
        "active_lock_unique_constraints" => {
            db.seed().await?;
            let mut tx = db.begin().await?;
            insert_self(&mut tx, "L1").await?;
            tx.commit().await?;
            let mut tx = db.begin().await?;
            check(
                insert_self(&mut tx, "duplicate-self").await.is_err(),
                "two active self locks accepted",
            )?;
            tx.rollback().await?;
            let mut tx = db.begin().await?;
            sqlx::query("INSERT INTO card_locks_v1 VALUES('L2','K','RISK_CONTROL','ACTIVE','risk-fixture','E1')").execute(&mut *tx).await?;
            tx.commit().await?;
            let mut tx = db.begin().await?;
            check(sqlx::query("INSERT INTO card_locks_v1 VALUES('duplicate-event','K','RISK_CONTROL','ACTIVE','risk-fixture','E1')").execute(&mut *tx).await.is_err(),"duplicate active event lock accepted")?;
            tx.rollback().await?;
            check(
                count(&db.pool, "card_locks_v1").await? == 2,
                "failed duplicate changed persisted locks",
            )?;
            Ok(
                json!({"active_self_unique":true,"same_scoped_event_unique":true,"persisted_locks":2}),
            )
        }
        _ => Err("unknown store test case".into()),
    }
}

pub async fn run_case(case: &str) -> Result<Value> {
    check(CASES.contains(&case), "unknown case")?;
    let db = Database::create().await?;
    let result = timeout(Duration::from_secs(40), exercise(&db, case))
        .await
        .map_err(|_| "store test timeout")
        .and_then(|r| r.map_err(|_| "store test failed"));
    db.cleanup().await?;
    Ok(
        json!({"case":case,"verified":true,"actual_postgresql":true,"fictional_source_data":true,"detail":result?}),
    )
}
