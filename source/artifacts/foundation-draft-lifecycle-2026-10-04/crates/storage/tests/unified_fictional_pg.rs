//! Requires a NEW database for each exact test invocation. Never skips a test.
use safeflow_bank_gateway::{BankError, BankGateway};
use safeflow_contracts::{ResourceId, UserId};
use safeflow_runtime::SessionBinding;
use safeflow_storage::*;
use sqlx::{postgres::PgPoolOptions, Row};
fn doc() -> UnifiedFictionalDocument {
    UnifiedFictionalDocument::read_adopted_v1(
        &std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").unwrap()).unwrap(),
    )
    .unwrap()
}
fn url() -> String {
    std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("dedicated new owned PG URL required")
}
async fn repo() -> PgRepository {
    let r = PgRepository::connect(&url(), "sf_unified_contract")
        .await
        .unwrap();
    r.migrate().await.unwrap();
    r
}
async fn sql() -> sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET search_path=sf_unified_contract")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect(&url())
        .await
        .unwrap()
}
#[tokio::test]
async fn adopted_original_sql_and_exact_scope() {
    let s = PgUnifiedFictionalSource::provision_new_database(&url(), "sf_unified_contract", doc())
        .await
        .unwrap();
    let db = sql().await;
    let rows=sqlx::query("SELECT account_id,owner_id,opening_cents,booked_cents FROM transfer_accounts_v1 ORDER BY account_id").fetch_all(&db).await.unwrap();
    assert_eq!(rows.len(), 3);
    for (owner, id, amount, cents) in [
        ("user_001", "account_001", "1288.00", 128800_i64),
        ("user_002", "account_002", "50.00", 5000),
        ("demo_user_li_v1", "demo_account_li_v1", "0.00", 0),
    ] {
        let owner = UserId::parse(owner).unwrap();
        let id = ResourceId::parse(id).unwrap();
        assert_eq!(
            s.get_account_balance(&owner, &id)
                .await
                .unwrap()
                .available_balance
                .as_str(),
            amount
        );
        let r = rows
            .iter()
            .find(|r| r.get::<String, _>("account_id") == id.as_str())
            .unwrap();
        assert_eq!(r.get::<i64, _>("opening_cents"), cents);
        assert_eq!(r.get::<i64, _>("booked_cents"), cents);
        assert_eq!(s.complete_accounts(&owner).await.unwrap().len(), 1);
        let identity = SessionBinding {
            user_id: owner,
            binding_version: 1,
        };
        let c = s.authorized_catalog(&identity, &id).await.unwrap().unwrap();
        assert_eq!(c.cards.len(), 1);
    }
    assert_eq!(
        s.get_account_balance(
            &UserId::parse("user_002").unwrap(),
            &ResourceId::parse("account_001").unwrap()
        )
        .await,
        Err(BankError::ResourceNotOwned)
    );
    assert!(s
        .complete_accounts(&UserId::parse("user_demo").unwrap())
        .await
        .is_err());
    assert_eq!(
        s.find_payees(&UserId::parse("user_001").unwrap(), "小李")
            .await
            .unwrap()
            .candidates[0]
            .payee_id
            .as_str(),
        "payee_008"
    );
    assert!(PgUnifiedFictionalSource::open(
        s.repository_for_isolated_backend(),
        UNIFIED_FICTIONAL_SOURCE,
        "wrong"
    )
    .await
    .is_err());
    assert_eq!(
        s.consumption_source(),
        Err(StorageError::DependencyUnavailable)
    );
    assert_eq!(
        s.initial_subscription_ledger(),
        Err(StorageError::DependencyUnavailable)
    );
    let raw: Vec<u8> =
        sqlx::query_scalar("SELECT original_bytes FROM public_unified_fictional_source_v1")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        raw,
        std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").unwrap()).unwrap()
    );
    assert!(PgUnifiedFictionalSource::initialize_new_isolated(
        s.repository_for_isolated_backend(),
        doc()
    )
    .await
    .is_err());
    s.repository_for_isolated_backend().close().await;
    db.close().await;
}
#[tokio::test]
async fn failed_initialization_rolls_back_every_account_and_route() {
    let r = repo().await;
    let db = sql().await;
    sqlx::raw_sql("CREATE FUNCTION reject_source() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'TEST_SOURCE_INSERT_FAIL'; END; $$; CREATE TRIGGER test_source_fail BEFORE INSERT ON public_unified_fictional_source_v1 FOR EACH ROW EXECUTE FUNCTION reject_source();").execute(&db).await.unwrap();
    assert!(
        PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
            .await
            .is_err()
    );
    for table in [
        "transfer_accounts_v1",
        "transfer_payee_routes_v1",
        "public_unified_fictional_source_v1",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
    assert!(PgUnifiedFictionalSource::open(
        r.clone(),
        UNIFIED_FICTIONAL_SOURCE,
        UNIFIED_FICTIONAL_REVISION
    )
    .await
    .is_err());
    sqlx::query("DROP TRIGGER test_source_fail ON public_unified_fictional_source_v1")
        .execute(&db)
        .await
        .unwrap();
    // Explicit retry only after independently proving transaction was rolled back.
    PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
        .await
        .unwrap();
    r.close().await;
    db.close().await;
}
#[tokio::test]
async fn preexisting_state_and_preflight_never_reset_or_migrate() {
    let db = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url())
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE public.existing_state(value bigint);INSERT INTO public.existing_state VALUES(783);").execute(&db).await.unwrap();
    assert!(
        PgUnifiedFictionalSource::provision_new_database(&url(), "sf_unified_contract", doc())
            .await
            .is_err()
    );
    let schema: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='sf_unified_contract')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(!schema);
    let value: i64 = sqlx::query_scalar("SELECT value FROM public.existing_state")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(value, 783);
    db.close().await;
}
#[tokio::test]
async fn occupied_schema_blocks_bootstrap_and_preserves_state() {
    let r = repo().await;
    let db = sql().await;
    sqlx::raw_sql("INSERT INTO transfer_accounts_v1 VALUES('old_account','old_owner',9876,9876)")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
            .await
            .is_err()
    );
    let rows = sqlx::query("SELECT account_id,booked_cents FROM transfer_accounts_v1")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("account_id"), "old_account");
    assert_eq!(rows[0].get::<i64, _>("booked_cents"), 9876);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM public_unified_fictional_source_v1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    r.close().await;
    db.close().await;
}
#[tokio::test]
async fn withdrawal_is_one_way_and_cached_provider_fails_closed() {
    let r = repo().await;
    let db = sql().await;
    let s = PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
        .await
        .unwrap();
    let owner = UserId::parse("user_001").unwrap();
    let account = ResourceId::parse("account_001").unwrap();
    let identity = SessionBinding {
        user_id: owner.clone(),
        binding_version: 1,
    };
    assert!(s.current_revision(&identity, &account).await.is_ok());
    s.deactivate_for_isolated_backend().await.unwrap();
    assert!(s.get_account_balance(&owner, &account).await.is_err());
    assert!(s.find_payees(&owner, "小李").await.is_err());
    assert!(s.complete_accounts(&owner).await.is_err());
    assert!(s.current_revision(&identity, &account).await.is_err());
    assert!(PgUnifiedFictionalSource::open(
        r.clone(),
        UNIFIED_FICTIONAL_SOURCE,
        UNIFIED_FICTIONAL_REVISION
    )
    .await
    .is_err());
    assert!(
        sqlx::query("UPDATE public_unified_fictional_source_v1 SET active=true")
            .execute(&db)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE public_unified_fictional_source_v1 SET source_revision='other'")
            .execute(&db)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM public_unified_fictional_source_v1")
            .execute(&db)
            .await
            .is_err()
    );
    r.close().await;
    db.close().await;
}
#[tokio::test]
async fn concurrent_bootstrap_has_one_atomic_winner() {
    let r = repo().await;
    let db = sql().await;
    let (a, b) = tokio::join!(
        PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc()),
        PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
    );
    assert_ne!(a.is_ok(), b.is_ok());
    for (table, expected) in [
        ("transfer_accounts_v1", 3_i64),
        ("transfer_payee_routes_v1", 1),
        ("public_unified_fictional_source_v1", 1),
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, expected);
    }
    r.close().await;
    db.close().await;
}
#[tokio::test]
async fn waited_revision_rechecks_withdrawal_after_database_wait() {
    let r = repo().await;
    let db = sql().await;
    let s = PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
        .await
        .unwrap();
    let identity = SessionBinding {
        user_id: UserId::parse("user_001").unwrap(),
        binding_version: 1,
    };
    let account = ResourceId::parse("account_001").unwrap();
    let mut change = db.begin().await.unwrap();
    sqlx::query("UPDATE public_unified_fictional_source_v1 SET active=false WHERE singleton=1")
        .execute(&mut *change)
        .await
        .unwrap();
    let waiting = s.current_revision(&identity, &account);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
            .await
            .is_err()
    );
    change.commit().await.unwrap();
    assert!(waiting.await.is_err());
    r.close().await;
    db.close().await;
}
#[tokio::test]
async fn waited_balance_rechecks_owner_instead_of_cached_metadata() {
    let r = repo().await;
    let db = sql().await;
    let s = PgUnifiedFictionalSource::initialize_new_isolated(r.clone(), doc())
        .await
        .unwrap();
    let owner = UserId::parse("user_001").unwrap();
    let account = ResourceId::parse("account_001").unwrap();
    let mut change = db.begin().await.unwrap();
    sqlx::query("LOCK TABLE transfer_accounts_v1 IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *change)
        .await
        .unwrap();
    let waiting = s.get_account_balance(&owner, &account);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
            .await
            .is_err()
    );
    // Technical fault only in this owned test DB, never source redefinition.
    sqlx::query(
        "UPDATE transfer_accounts_v1 SET owner_id='fault_owner' WHERE account_id='account_001'",
    )
    .execute(&mut *change)
    .await
    .unwrap();
    change.commit().await.unwrap();
    assert!(waiting.await.is_err());
    r.close().await;
    db.close().await;
}
