#![cfg(feature = "postgres-mock-store")]
use safeflow_subscription_evidence::{
    confirmation_mock::*, managed_ledger_mock::*, ReadKind, ReadState,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::atomic::{AtomicU64, Ordering};
const NOW: &str = "2026-10-16T12:00:00+08:00";
const EXPIRY: &str = "2026-10-16T12:05:00+08:00";
static NEXT: AtomicU64 = AtomicU64::new(0);
fn owner() -> String {
    format!(
        "fictional-intent-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn fixture(user: &str) -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: user.into(),
        snapshot_id: Some("fictional-initial".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![LedgerEntry {
            subscription_id: "fictional-sub".into(),
            owner_user_id: user.into(),
            source_id: Some("fictional-source".into()),
            merchant_id: Some("fictional-merchant".into()),
            service_ref: Some("fictional-service".into()),
            status: LedgerStatus::Active,
        }],
    }
}
fn context(user: &str, id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: user.into(),
        session_ref: "fictional-session".into(),
        task_id: "fictional-task".into(),
        node_id: "fictional-node".into(),
        operation_id: id.into(),
        idempotency_key: format!("key-{id}"),
        plan_version: 1,
    }
}
async fn pool() -> PgPool {
    let url = std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL")
        .expect("DEDICATED_DATABASE_REQUIRED");
    let p = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap_or_else(|_| panic!("DATABASE_UNAVAILABLE"));
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&p)
        .await
        .expect("DATABASE_NAME_CHECK_FAILED");
    assert_eq!(db, "safeflow_subscription_mock_test");
    PostgresIntentSessionMock::initialize_local_schema(&p)
        .await
        .unwrap();
    p
}
async fn create(p: &PgPool, user: &str) -> PostgresIntentSessionMock {
    PostgresIntentSessionMock::create_fixture_mock(p.clone(), user, fixture(user))
        .await
        .unwrap()
}
async fn prepare(
    s: &mut PostgresIntentSessionMock,
    user: &str,
    op: &str,
    kind: MockIntentKind,
) -> MockIntentBinding {
    s.prepare_intent_mock(context(user, op), "fictional-sub", kind, NOW, EXPIRY)
        .await
        .unwrap()
}
struct Gate {
    binding: MockIntentBinding,
    calls: usize,
}
impl Gate {
    fn new(b: &MockIntentBinding) -> Self {
        Self {
            binding: b.clone(),
            calls: 0,
        }
    }
}
impl MockIntentGateVerifier for Gate {
    fn verify_and_consume_mock_intent(
        &mut self,
        b: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        if self.calls == 1 && *b == self.binding {
            MockGateOutcome::Confirmed
        } else {
            MockGateOutcome::Denied
        }
    }
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn record_revoke_and_original_retry_survive_reload() {
    let p = pool().await;
    let u = owner();
    let mut s = create(&p, &u).await;
    let b = prepare(&mut s, &u, "record", MockIntentKind::Record).await;
    let original = s
        .commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate::new(&b))
        .await
        .unwrap();
    drop(s);
    let mut s = PostgresIntentSessionMock::load_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(s.list_mock().unwrap()[0].status, LedgerStatus::Cancelling);
    let r = prepare(&mut s, &u, "revoke", MockIntentKind::Revoke).await;
    s.commit_intent_mock(&r.draft_id, &r.context, NOW, &mut Gate::new(&r))
        .await
        .unwrap();
    drop(s);
    let mut s = PostgresIntentSessionMock::load_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut gate = Gate::new(&b);
    assert_eq!(
        s.commit_intent_mock(&b.draft_id, &b.context, NOW, &mut gate)
            .await
            .unwrap(),
        original
    );
    assert_eq!(gate.calls, 0);
    assert_eq!(s.list_mock().unwrap()[0].status, LedgerStatus::Active);
    assert_eq!(s.intent_events_mock().unwrap().len(), 2);
    assert!(!original.merchant_cancellation_confirmed);
    let mut changed = b.context.clone();
    changed.plan_version += 1;
    assert_eq!(
        s.commit_intent_mock(&b.draft_id, &changed, NOW, &mut gate)
            .await,
        Err(IntentRecoveryIssue::Business(
            MockIssue::IdempotencyConflict
        ))
    );
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn failed_database_write_never_publishes_success_or_changes_local_rows() {
    let p = pool().await;
    let u = owner();
    let mut s = create(&p, &u).await;
    let b = prepare(&mut s, &u, "record", MockIntentKind::Record).await;
    p.close().await;
    assert_eq!(
        s.commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate::new(&b))
            .await,
        Err(IntentRecoveryIssue::Unavailable)
    );
    assert_eq!(s.list_mock().unwrap()[0].status, LedgerStatus::Active);
    assert!(s.intent_events_mock().unwrap().is_empty());
    let p = pool().await;
    let s = PostgresIntentSessionMock::load_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(s.list_mock().unwrap()[0].status, LedgerStatus::Active);
    assert!(s.intent_result_mock("record").is_err());
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn stale_sessions_cannot_overwrite_a_newer_saved_version() {
    let p = pool().await;
    let u = owner();
    let mut a = create(&p, &u).await;
    let ba = prepare(&mut a, &u, "a", MockIntentKind::Record).await;
    let mut b = PostgresIntentSessionMock::load_mock(p.clone(), &u)
        .await
        .unwrap();
    let bb = prepare(&mut b, &u, "b", MockIntentKind::Record).await;
    assert_eq!(
        a.commit_intent_mock(&ba.draft_id, &ba.context, NOW, &mut Gate::new(&ba))
            .await,
        Err(IntentRecoveryIssue::Conflict)
    );
    assert_eq!(a.list_mock().unwrap()[0].status, LedgerStatus::Active);
    b.commit_intent_mock(&bb.draft_id, &bb.context, NOW, &mut Gate::new(&bb))
        .await
        .unwrap();
    let loaded = PostgresIntentSessionMock::load_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(loaded.intent_result_mock("a").is_err());
    assert!(loaded.intent_result_mock("b").is_ok());
    assert_eq!(loaded.intent_events_mock().unwrap().len(), 1);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn pending_drafts_are_invalidated_without_reusing_their_ids() {
    let p = pool().await;
    let u = owner();
    let mut s = create(&p, &u).await;
    let old = prepare(&mut s, &u, "record", MockIntentKind::Record).await;
    drop(s);
    let mut s = PostgresIntentSessionMock::load_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut old_gate = Gate::new(&old);
    assert_eq!(
        s.commit_intent_mock(&old.draft_id, &old.context, NOW, &mut old_gate)
            .await,
        Err(IntentRecoveryIssue::Business(MockIssue::DraftUnavailable))
    );
    assert_eq!(old_gate.calls, 0);
    let new = prepare(&mut s, &u, "record", MockIntentKind::Record).await;
    assert_ne!(old.draft_id, new.draft_id);
    assert_eq!(
        s.commit_intent_mock(&new.draft_id, &new.context, NOW, &mut old_gate)
            .await,
        Err(IntentRecoveryIssue::Business(
            MockIssue::ConfirmationRequired
        ))
    );
    assert_eq!(s.list_mock().unwrap()[0].status, LedgerStatus::Active);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn unknown_owner_and_mixed_ownership_do_not_become_empty_or_replace_rows() {
    let p = pool().await;
    let u = owner();
    let _s = create(&p, &u).await;
    assert!(matches!(
        PostgresIntentSessionMock::load_mock(p.clone(), &format!("{u}-other")).await,
        Err(IntentRecoveryIssue::NotFound)
    ));
    let mut bad = fixture(&format!("{u}-bad"));
    bad.entries[0].owner_user_id = u.clone();
    assert!(matches!(
        PostgresIntentSessionMock::create_fixture_mock(p.clone(), &format!("{u}-bad"), bad).await,
        Err(IntentRecoveryIssue::Business(
            MockIssue::ResourceUnavailable
        ))
    ));
    assert!(matches!(
        PostgresIntentSessionMock::create_fixture_mock(p.clone(), &u, fixture(&u)).await,
        Err(IntentRecoveryIssue::Conflict)
    ));
    assert_eq!(
        PostgresIntentSessionMock::load_mock(p.clone(), &u)
            .await
            .unwrap()
            .list_mock()
            .unwrap()[0]
            .status,
        LedgerStatus::Active
    );
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn damaged_receipts_and_incomplete_history_are_rejected_on_restore() {
    let p = pool().await;
    for path in [
        "{completed,0,receipt,merchant_cancellation_confirmed}",
        "{events}",
    ] {
        let u = owner();
        let mut s = create(&p, &u).await;
        let b = prepare(&mut s, &u, "record", MockIntentKind::Record).await;
        s.commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate::new(&b))
            .await
            .unwrap();
        sqlx::query("UPDATE safeflow_subscription_mock_v1.intent_recovery SET checkpoint=jsonb_set(checkpoint,$1::text[],$2::jsonb) WHERE owner_id=$3")
            .bind(path).bind(if path=="{events}" {"[]"} else {"true"}).bind(&u).execute(&p).await.unwrap();
        assert!(matches!(
            PostgresIntentSessionMock::load_mock(p.clone(), &u).await,
            Err(IntentRecoveryIssue::InvalidCheckpoint)
        ));
    }
    p.close().await;
}
#[test]
#[ignore = "requires dedicated local PostgreSQL test database"]
fn completed_intent_survives_two_independent_processes() {
    let u = owner();
    let mut reports = Vec::new();
    for phase in ["write", "read"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "restart_child", "--nocapture"])
            .env("SAFEFLOW_INTENT_RESTART_PHASE", phase)
            .env("SAFEFLOW_INTENT_RESTART_OWNER", &u)
            .output()
            .unwrap();
        assert!(out.status.success(), "RESTART_CHILD_FAILED");
        let text = String::from_utf8(out.stdout).unwrap();
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("RESTART_PROBE_JSON:"))
            .expect("CHILD_REPORT_REQUIRED");
        let r: serde_json::Value = serde_json::from_str(line).unwrap();
        println!("RESTART_PROBE_JSON:{r}");
        reports.push(r);
    }
    assert_ne!(reports[0]["pid"], reports[1]["pid"]);
    assert_eq!(reports[1]["original_receipt_restored"], true);
    assert_eq!(reports[1]["repeat_gate_calls"], 0);
    assert_eq!(reports[1]["events"], 1);
}
#[tokio::test]
#[ignore = "subprocess helper; only called by the restart test"]
async fn restart_child() {
    let phase = std::env::var("SAFEFLOW_INTENT_RESTART_PHASE").expect("CHILD_PHASE_REQUIRED");
    let u = std::env::var("SAFEFLOW_INTENT_RESTART_OWNER").expect("CHILD_OWNER_REQUIRED");
    let p = pool().await;
    let mut s = if phase == "write" {
        create(&p, &u).await
    } else {
        PostgresIntentSessionMock::load_mock(p.clone(), &u)
            .await
            .unwrap()
    };
    let binding = MockIntentBinding {
        minimum_level: "YELLOW",
        kind: MockIntentKind::Record,
        context: context(&u, "record"),
        draft_id: "mock-intent-draft-1".into(),
        ledger_revision: 0,
        target: fixture(&u).entries.remove(0),
        prepared_at: NOW.into(),
        expires_at: EXPIRY.into(),
    };
    if phase == "write" {
        let actual = prepare(&mut s, &u, "record", MockIntentKind::Record).await;
        assert_eq!(actual, binding);
        s.commit_intent_mock(
            &binding.draft_id,
            &binding.context,
            NOW,
            &mut Gate::new(&binding),
        )
        .await
        .unwrap();
    }
    let original = s.intent_result_mock("record").unwrap().clone();
    let mut gate = Gate::new(&binding);
    assert_eq!(
        s.commit_intent_mock(&binding.draft_id, &binding.context, NOW, &mut gate)
            .await
            .unwrap(),
        original
    );
    assert_eq!(s.list_mock().unwrap()[0].status, LedgerStatus::Cancelling);
    assert_eq!(s.intent_events_mock().unwrap().len(), 1);
    assert!(!original.merchant_cancellation_confirmed);
    println!(
        "RESTART_PROBE_JSON:{}",
        serde_json::json!({"phase":phase,"pid":std::process::id(),
        "original_receipt_restored":true,"repeat_gate_calls":gate.calls,"events":1,"merchant_cancelled":false})
    );
    p.close().await;
}
