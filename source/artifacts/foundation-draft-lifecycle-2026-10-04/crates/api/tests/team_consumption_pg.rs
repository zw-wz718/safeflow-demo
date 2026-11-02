//! Each exact invocation requires its own NEW local database. No silent skips.
use safeflow_api::{ApiConfig, ApiState};
use safeflow_billing_source_reader::{query::*, TrustedScope};
use safeflow_contracts::UserId;
use safeflow_storage::*;
use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, Executor, Row};
use std::sync::atomic::Ordering;
#[path = "../../storage/src/simulation_source_pg_fault.rs"]
mod fault;
const SCHEMA: &str = "sf_unified_team";
const SECRET: &str = "team-consumption-owned-test-login-only";
fn owner(s: &str) -> UserId {
    UserId::parse(s).unwrap()
}
fn url() -> String {
    std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("new owned database required")
}
async fn setup() -> (PgUnifiedFictionalSource, sqlx::PgPool) {
    let identity = PgUnifiedFictionalSource::provision_new_database(
        &url(),
        SCHEMA,
        UnifiedFictionalDocument::read_adopted_v1(include_bytes!(
            "../../../../../fixtures/transfer/unified_demo_source_v1.json"
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    let db = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET search_path=sf_unified_team")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect(&url())
        .await
        .unwrap();
    let doc = TeamConsumptionDocument::fixed_adopted().unwrap();
    for s in ["user_001", "user_002", "demo_user_li_v1"] {
        let source = TeamConsumptionSource::import(&identity, &owner(s), &doc)
            .await
            .unwrap();
        source
            .persistent_for_backend()
            .set_active(true)
            .await
            .unwrap();
    }
    (identity, db)
}
async fn state(identity: &PgUnifiedFictionalSource, s: &str) -> ApiState {
    ApiState::for_unified_team_consumption(
        identity.clone(),
        ApiConfig::local_simulation(zeroize::Zeroizing::new(SECRET.to_owned())),
        owner(s),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn team_sql_original_rows_pagination_scope_and_half_open() {
    let (identity, db) = setup().await;
    let raw: Value = serde_json::from_slice(include_bytes!(
        "../../../fixtures/team-consumption-initial/team_demo_consumption.proposed.v1.json"
    ))
    .unwrap();
    let mut total = 0;
    let mut pages = 0;
    for (name, count) in [("user_001", 21), ("user_002", 2), ("demo_user_li_v1", 0)] {
        let team = TeamConsumptionSource::open(&identity, &owner(name))
            .await
            .unwrap();
        let pg = team.persistent_for_backend();
        let r = team.request();
        let c = pg.capture(&owner(name), &r).await.unwrap();
        let scope =
            TrustedScope::from_verified_context(name, r.account_ids.iter().map(String::as_str))
                .unwrap();
        let report = collect(&scope, &r, &c.reader(), Limits::default());
        assert!(report.is_complete);
        assert_eq!(report.observed_transactions.len(), count);
        total += count;
        pages += report.pages_read;
        let original = raw["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["owner_user_id"] == name)
            .unwrap();
        let mut rows: Vec<Transaction> = original["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p["page"]["items"].as_array().unwrap())
            .map(|v| serde_json::from_value(v.clone()).unwrap())
            .collect();
        rows.sort_by_key(|v| (v.account_id.clone(), v.transaction_id.clone()));
        let mut actual = report.observed_transactions;
        actual.sort_by_key(|v| (v.account_id.clone(), v.transaction_id.clone()));
        assert_eq!(actual, rows);
        assert!(
            safeflow_storage::subscription_billing_reader::collect_from_billing_mock(
                &scope,
                &r,
                &c.reader(),
                Limits::default()
            )
            .is_err()
        ); // old guard is still closed
        let foreign = TrustedScope::from_verified_context(
            "foreign",
            r.account_ids.iter().map(String::as_str),
        )
        .unwrap();
        assert!(c.reader().read_page(&foreign, &r, None, None).is_err());
        drop(c);
        assert!(pg.capture(&owner("foreign"), &r).await.is_err());
        let mut wrong = r.clone();
        wrong.account_ids = vec!["foreign_account".into()];
        assert!(pg.capture(&owner(name), &wrong).await.is_err());
        wrong = r.clone();
        wrong.window.end_exclusive = "2026-10-28T00:00:00+08:00".into();
        wrong.window.as_of = wrong.window.end_exclusive.clone();
        assert!(pg.capture(&owner(name), &wrong).await.is_err());
        if let Some(first) = rows.iter().filter_map(|x| x.completed_at.as_ref()).min() {
            let mut narrow = r.clone();
            narrow.window.end_exclusive = first.clone();
            let cap = pg.capture(&owner(name), &narrow).await.unwrap();
            let got = collect(&scope, &narrow, &cap.reader(), Limits::default());
            assert!(got.is_complete);
            assert!(got
                .observed_transactions
                .iter()
                .all(|r| r.completed_at.is_none()));
        }
    }
    assert_eq!((total, pages), (23, 7));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM simulation_source_rows_v1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 23);
    db.close().await;
    identity.repository_for_isolated_backend().close().await;
}
#[tokio::test]
async fn team_initial_three_owners_idempotence_journal_conflict_and_reopen() {
    let (identity, db) = setup().await;
    for name in ["user_001", "user_002", "demo_user_li_v1"] {
        let state = state(&identity, name).await;
        let s = state.simulation_session_for_native_host(SECRET).unwrap();
        state.initialize_team_initial_for_backend(&s).await.unwrap();
        assert!(state.lookup_team_initial_for_backend(&s).await.unwrap());
        let read = state.subscription_for_backend();
        read.startup_for_host(&s).await.unwrap();
        let before: Value = sqlx::query_scalar(
            "SELECT journal FROM subscription_runtime_records WHERE owner_id=$1",
        )
        .bind(name)
        .fetch_one(&db)
        .await
        .unwrap();
        state.initialize_team_initial_for_backend(&s).await.unwrap();
        let after: Value = sqlx::query_scalar(
            "SELECT journal FROM subscription_runtime_records WHERE owner_id=$1",
        )
        .bind(name)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(before, after);
        let view = read.read(&s).await.unwrap();
        assert_eq!(view["items"].as_array().unwrap().len(), 0);
        assert_eq!(view["writes_enabled"], false);
        assert!(read.require_write_adoption(&s).is_err());
        let team = TeamConsumptionSource::open(&identity, &owner(name))
            .await
            .unwrap();
        let mut spec = team.persistent_for_backend().spec().clone();
        spec.revision.push_str("-conflict");
        let raw: Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/team-consumption-initial/team_demo_initial_ledgers.v1.json"
        ))
        .unwrap();
        let mut initial = raw["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["owner_user_id"] == name)
            .unwrap()["ledger"]
            .clone();
        initial["snapshot_id"] = Value::from("different-initial-snapshot");
        let changed = PgSimulationSource::import(
            identity.repository_for_isolated_backend(),
            spec,
            vec![],
            Some(serde_json::from_value(initial).unwrap()),
        )
        .await
        .unwrap();
        changed.set_active(true).await.unwrap();
        assert!(state
            .initialize_simulation_source_for_backend(&s, &changed)
            .await
            .is_err());
        assert_eq!(
            before,
            sqlx::query_scalar::<_, Value>(
                "SELECT journal FROM subscription_runtime_records WHERE owner_id=$1"
            )
            .bind(name)
            .fetch_one(&db)
            .await
            .unwrap()
        );
        team.persistent_for_backend()
            .set_active(true)
            .await
            .unwrap();
        let reopen = PgUnifiedFictionalSource::open(
            PgRepository::connect_existing(&url(), SCHEMA)
                .await
                .unwrap(),
            UNIFIED_FICTIONAL_SOURCE,
            UNIFIED_FICTIONAL_REVISION,
        )
        .await
        .unwrap();
        let next = state_for_reopen(&reopen, name).await;
        let ns = next.simulation_session_for_native_host(SECRET).unwrap();
        assert!(next.lookup_team_initial_for_backend(&ns).await.unwrap());
        reopen.repository_for_isolated_backend().close().await;
    }
    for (name, cents) in [
        ("user_001", 128800_i64),
        ("user_002", 5000),
        ("demo_user_li_v1", 0),
    ] {
        let v: i64 =
            sqlx::query_scalar("SELECT booked_cents FROM transfer_accounts_v1 WHERE owner_id=$1")
                .bind(name)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(v, cents);
    }
    db.close().await;
    identity.repository_for_isolated_backend().close().await;
}
async fn state_for_reopen(i: &PgUnifiedFictionalSource, n: &str) -> ApiState {
    state(i, n).await
}
#[tokio::test]
async fn team_initial_failure_rolls_back_and_revoked_session_source_reject() {
    let (identity, db) = setup().await;
    let state = state(&identity, "user_001").await;
    let s = state.simulation_session_for_native_host(SECRET).unwrap();
    db.execute("CREATE FUNCTION reject_initial() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'owned failure'; END $$; CREATE TRIGGER reject_initial BEFORE INSERT ON subscription_runtime_records FOR EACH ROW EXECUTE FUNCTION reject_initial()").await.unwrap();
    assert!(state.initialize_team_initial_for_backend(&s).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM subscription_runtime_records")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.execute("DROP TRIGGER reject_initial ON subscription_runtime_records")
        .await
        .unwrap();
    let team = TeamConsumptionSource::open(&identity, &owner("user_001"))
        .await
        .unwrap();
    team.persistent_for_backend()
        .set_active(false)
        .await
        .unwrap();
    assert!(state.initialize_team_initial_for_backend(&s).await.is_err());
    assert!(!state.lookup_team_initial_for_backend(&s).await.unwrap());
    team.persistent_for_backend()
        .set_active(true)
        .await
        .unwrap();
    state.initialize_team_initial_for_backend(&s).await.unwrap();
    team.persistent_for_backend()
        .set_active(false)
        .await
        .unwrap();
    assert!(state.lookup_team_initial_for_backend(&s).await.unwrap());
    state.stop_native_simulation_session(&s).unwrap();
    assert!(state.initialize_team_initial_for_backend(&s).await.is_err());
    assert!(state
        .team_subscription_preview_for_backend(&s)
        .await
        .is_err());
    assert!(state.lookup_team_initial_for_backend(&s).await.is_err());
    db.close().await;
    identity.repository_for_isolated_backend().close().await;
}
#[tokio::test]
async fn team_same_capture_preview_select_prepare_and_generation_reject() {
    let (identity, db) = setup().await;
    let state = state(&identity, "user_001").await;
    let s = state.simulation_session_for_native_host(SECRET).unwrap();
    state.initialize_team_initial_for_backend(&s).await.unwrap();
    let preview = state
        .team_subscription_preview_for_backend(&s)
        .await
        .unwrap();
    assert_eq!(preview["read"]["observed_rows"], 21);
    assert_eq!(preview["read"]["pages_read"], 5);
    let text = serde_json::to_string(&preview).unwrap();
    assert!(text.contains("demo_service_music_v1") && text.contains("demo_service_video_v1"));
    let evidence = &preview["service_calendar_evidence"];
    assert_eq!(evidence["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["notification_sent"], false);
    let host = state
        .subscription_for_backend()
        .startup_for_host(&s)
        .await
        .unwrap();
    // Candidate index follows the ORIGINAL result; no recreated business candidate.
    let index = preview["recognition"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| {
            g["disposition"] == "NOT_IN_LEDGER"
                && !g["matching_cycles"].as_array().unwrap().is_empty()
        })
        .expect("member-owned eligible selection")["selection_index"]
        .as_u64()
        .unwrap() as usize;
    let team = TeamConsumptionSource::open(&identity, &owner("user_001"))
        .await
        .unwrap();
    let mut bank_config = ApiConfig::local_simulation(zeroize::Zeroizing::new(SECRET.to_owned()));
    bank_config.agent_fixed_plan_enabled = true;
    assert!(ApiState::for_unified_team_consumption(
        identity.clone(),
        bank_config,
        owner("user_001")
    )
    .await
    .is_err());
    state
        .prepare_team_subscription_for_backend(&s, &host, index, &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM subscription_preparations_v1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    team.persistent_for_backend()
        .set_active(false)
        .await
        .unwrap();
    assert!(state
        .prepare_team_subscription_for_backend(&s, &host, index, &uuid::Uuid::new_v4().to_string())
        .await
        .is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM subscription_preparations_v1")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    for table in [
        "transfer_fund_orders_v1",
        "transfer_ledger_v1",
        "card_locks_v1",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
    db.close().await;
    identity.repository_for_isolated_backend().close().await;
}
#[tokio::test]
async fn team_commit_reply_unknown_lookup_original_without_reinitialize() {
    let (identity, db) = setup().await;
    let upstream = std::env::var("SAFEFLOW_TEAM_PG_ADDRESS")
        .expect("owned loopback address")
        .parse()
        .unwrap();
    let proxy = fault::CommitLossProxy::start(upstream, false).await;
    let _ = &proxy.commit_seen;
    let proxy_url = format!(
        "postgresql://sf_team@{}/{}?sslmode=disable",
        proxy.address,
        std::env::var("SAFEFLOW_TEAM_DATABASE").unwrap()
    );
    let repo = PgRepository::connect_existing(&proxy_url, SCHEMA)
        .await
        .unwrap();
    let proxied = PgUnifiedFictionalSource::open(
        repo.clone(),
        UNIFIED_FICTIONAL_SOURCE,
        UNIFIED_FICTIONAL_REVISION,
    )
    .await
    .unwrap();
    let st = state(&proxied, "user_001").await;
    let s = st.simulation_session_for_native_host(SECRET).unwrap();
    assert_eq!(
        st.initialize_team_initial_for_backend(&s).await,
        Err(StorageError::ReconcileRequired)
    );
    assert!(proxy.fired.load(Ordering::SeqCst));
    let direct = state(&identity, "user_001").await;
    let ds = direct.simulation_session_for_native_host(SECRET).unwrap();
    assert!(direct.lookup_team_initial_for_backend(&ds).await.unwrap());
    let rows = sqlx::query("SELECT storage_version,journal FROM subscription_runtime_records")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i64, _>("storage_version"), 1);
    repo.close().await;
    drop(proxy);
    db.close().await;
    identity.repository_for_isolated_backend().close().await;
}
#[tokio::test]
async fn team_final_account_await_revocation_rolls_back_initial() {
    let (identity, db) = setup().await;
    let state = state(&identity, "user_001").await;
    let s = state.simulation_session_for_native_host(SECRET).unwrap();
    let mut lock = db.begin().await.unwrap();
    sqlx::query("LOCK TABLE transfer_accounts_v1 IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let st = state.clone();
    let ss = s.clone();
    let running = tokio::spawn(async move { st.initialize_team_initial_for_backend(&ss).await });
    tokio::time::timeout(std::time::Duration::from_secs(3),async{
        loop{let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%transfer_accounts_v1%' AND pid<>pg_backend_pid())").fetch_one(&db).await.unwrap();if waiting{break}tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
    }).await.expect("real final bank account await must block");
    state.stop_native_simulation_session(&s).unwrap();
    lock.commit().await.unwrap();
    assert_eq!(running.await.unwrap(), Err(StorageError::Session));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM subscription_runtime_records")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await;
    identity.repository_for_isolated_backend().close().await;
}
