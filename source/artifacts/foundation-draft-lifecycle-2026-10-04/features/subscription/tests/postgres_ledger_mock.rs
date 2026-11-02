#![cfg(feature = "postgres-mock-store")]
use safeflow_subscription_evidence::{
    managed_ledger_mock::{LedgerEntry, LedgerFixture, LedgerStatus},
    postgres_ledger_mock::*,
    ReadKind, ReadState,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
fn fixture(label: &str, empty: bool) -> LedgerFixture {
    let unique = format!(
        "fictional-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let owner = format!("owner-{unique}");
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: owner.clone(),
        snapshot_id: Some(unique),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: if empty {
            vec![]
        } else {
            vec![LedgerEntry {
                subscription_id: "fictional-subscription".into(),
                owner_user_id: owner,
                source_id: Some("fictional-source".into()),
                merchant_id: Some("fictional-merchant".into()),
                service_ref: Some("fictional-service".into()),
                status: LedgerStatus::Active,
            }]
        },
    }
}
async fn connect() -> PgPool {
    let url = std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL")
        .expect("DEDICATED_LOCAL_TEST_DATABASE_URL_REQUIRED");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap_or_else(|_| panic!("LOCAL_TEST_DATABASE_UNAVAILABLE"));
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .expect("DATABASE_NAME_CHECK_FAILED");
    assert_eq!(
        database, "safeflow_subscription_mock_test",
        "DEDICATED_TEST_DATABASE_REQUIRED"
    );
    pool
}
async fn store() -> (PgPool, PostgresLedgerMock) {
    let pool = connect().await;
    let store = PostgresLedgerMock::from_pool_for_local_simulation(pool.clone());
    store.initialize_local_schema().await.unwrap();
    (pool, store)
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn closed_and_reopened_connections_read_the_saved_snapshot() {
    let (pool, store) = store().await;
    let f = fixture("reopen", false);
    assert_eq!(
        store.save_fixture_mock(&f.owner_user_id, &f).await.unwrap(),
        MockSnapshotSave::Stored
    );
    pool.close().await;
    drop(store);
    let reopened = connect().await;
    let store = PostgresLedgerMock::from_pool_for_local_simulation(reopened.clone());
    let actual = store
        .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(&f).unwrap()
    );
    reopened.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn known_empty_missing_and_unavailable_are_distinct() {
    let (pool, store) = store().await;
    let f = fixture("empty", true);
    store.save_fixture_mock(&f.owner_user_id, &f).await.unwrap();
    assert!(store
        .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
        .await
        .unwrap()
        .entries
        .is_empty());
    assert!(matches!(
        store
            .read_owned_fixture_mock(&f.owner_user_id, "missing")
            .await,
        Err(MockPostgresIssue::NotFound)
    ));
    pool.close().await;
    assert!(matches!(
        store
            .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
            .await,
        Err(MockPostgresIssue::Unavailable)
    ));
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn identical_snapshot_ids_remain_scoped_to_the_owner() {
    let (pool, store) = store().await;
    let a = fixture("owner", false);
    let mut b = a.clone();
    b.owner_user_id.push_str("-other");
    b.entries[0].owner_user_id = b.owner_user_id.clone();
    b.entries[0].service_ref = Some("other-fictional-service".into());
    store.save_fixture_mock(&a.owner_user_id, &a).await.unwrap();
    store.save_fixture_mock(&b.owner_user_id, &b).await.unwrap();
    for f in [&a, &b] {
        let r = store
            .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
            .await
            .unwrap();
        assert_eq!(r.entries, f.entries);
    }
    assert!(matches!(
        store
            .read_owned_fixture_mock("foreign", a.snapshot_id.as_deref().unwrap())
            .await,
        Err(MockPostgresIssue::NotFound)
    ));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn repeated_save_is_idempotent_and_conflicts_do_not_overwrite() {
    let (pool, store) = store().await;
    let f = fixture("conflict", false);
    store.save_fixture_mock(&f.owner_user_id, &f).await.unwrap();
    assert_eq!(
        store.save_fixture_mock(&f.owner_user_id, &f).await.unwrap(),
        MockSnapshotSave::AlreadyStored
    );
    let mut changed = f.clone();
    changed.entries[0].status = LedgerStatus::Cancelling;
    assert_eq!(
        store.save_fixture_mock(&f.owner_user_id, &changed).await,
        Err(MockPostgresIssue::Conflict)
    );
    assert_eq!(
        store
            .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
            .await
            .unwrap()
            .entries,
        f.entries
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn invalid_snapshots_are_rejected_before_any_row_is_written() {
    let (pool, store) = store().await;
    for variant in 0..4 {
        let mut f = fixture("invalid", false);
        match variant {
            0 => f.entries[0].owner_user_id = "foreign".into(),
            1 => f.read.is_complete = false,
            2 => f.production_registered = true,
            _ => f.fictional = false,
        }
        assert_eq!(
            store.save_fixture_mock(&f.owner_user_id, &f).await,
            Err(MockPostgresIssue::InvalidSnapshot)
        );
        assert!(matches!(
            store
                .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
                .await,
            Err(MockPostgresIssue::NotFound)
        ));
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn concurrent_same_snapshot_saves_leave_one_row() {
    let (pool, store) = store().await;
    let f = fixture("concurrent", false);
    let (a, b) = tokio::join!(
        store.save_fixture_mock(&f.owner_user_id, &f),
        store.save_fixture_mock(&f.owner_user_id, &f)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(matches!(
        (a, b),
        (MockSnapshotSave::Stored, MockSnapshotSave::AlreadyStored)
            | (MockSnapshotSave::AlreadyStored, MockSnapshotSave::Stored)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM safeflow_subscription_mock_v1.ledger_snapshots WHERE owner_id=$1 AND snapshot_id=$2")
        .bind(&f.owner_user_id).bind(&f.snapshot_id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn invalid_persisted_read_metadata_is_not_trusted() {
    let (pool, store) = store().await;
    let f = fixture("corruption", false);
    store.save_fixture_mock(&f.owner_user_id, &f).await.unwrap();
    sqlx::query("UPDATE safeflow_subscription_mock_v1.ledger_snapshots SET snapshot=jsonb_set(snapshot,'{read,is_complete}','false'::jsonb) WHERE owner_id=$1 AND snapshot_id=$2")
        .bind(&f.owner_user_id).bind(&f.snapshot_id).execute(&pool).await.unwrap();
    assert!(matches!(
        store
            .read_owned_fixture_mock(&f.owner_user_id, f.snapshot_id.as_deref().unwrap())
            .await,
        Err(MockPostgresIssue::InvalidSnapshot)
    ));
    pool.close().await;
}
