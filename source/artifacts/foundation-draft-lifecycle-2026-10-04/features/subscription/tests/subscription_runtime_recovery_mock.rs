#![cfg(feature = "postgres-mock-store")]
use chrono::NaiveDate;
use safeflow_subscription_evidence::{
    confirmation_mock::*, managed_ledger_mock::*, recognition::Cycle, reminder_mock::*, *,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::atomic::{AtomicU64, Ordering};
const NOW: &str = "2026-10-14T12:00:00+08:00";
const EXPIRY: &str = "2026-10-14T12:05:00+08:00";
const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));
static NEXT: AtomicU64 = AtomicU64::new(0);
fn owner() -> String {
    format!(
        "fictional-runtime-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn fixture(u: &str) -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: u.into(),
        snapshot_id: Some("fictional-runtime-initial".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![],
    }
}
fn sample(u: &str) -> Sample {
    let mut s = billing_query_mock::load_case(
        INPUT,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap();
    s.trusted_context_fixture.user_id = u.into();
    for a in &mut s.snapshot.accounts {
        if a.user_id == "user_demo" {
            a.user_id = u.into();
        }
    }
    s
}
fn ctx(u: &str, id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: u.into(),
        session_ref: "fictional-session".into(),
        task_id: "fictional-task".into(),
        node_id: "fictional-node".into(),
        operation_id: id.into(),
        idempotency_key: format!("fictional-key-{id}"),
        plan_version: 1,
    }
}
fn ready_date_sample(user: &str) -> Sample {
    let root: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/discovery_ready_input.v1.json"
    )))
    .unwrap();
    assert_eq!(root["fictional"], true);
    assert_eq!(
        root["provenance"],
        "HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE"
    );
    let mut sample: Sample = serde_json::from_value(root["sample"].clone()).unwrap();
    sample.trusted_context_fixture.user_id = user.into();
    for account in &mut sample.snapshot.accounts {
        account.user_id = user.into();
    }
    sample
}
async fn register_ready_date(
    r: &mut SubscriptionRuntimeMock,
    user: &str,
) -> MockConfirmationBinding {
    let s = ready_date_sample(user);
    let binding = r
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(user, "register"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    r.register_mock(
        &binding.draft_id,
        &binding.context,
        NOW,
        &mut Gate { calls: 0 },
    )
    .await
    .unwrap();
    binding
}
struct Gate {
    calls: usize,
}
impl MockGateVerifier for Gate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        _: &MockConfirmationBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        MockGateOutcome::Confirmed
    }
}
impl MockIntentGateVerifier for Gate {
    fn verify_and_consume_mock_intent(
        &mut self,
        _: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        MockGateOutcome::Confirmed
    }
}
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
        self.calls += 1;
        MockGateOutcome::Confirmed
    }
}
struct Sink {
    calls: usize,
    outcome: DeliveryOutcome,
}
impl Sink {
    fn new(outcome: DeliveryOutcome) -> Self {
        Self { calls: 0, outcome }
    }
}
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
        self.calls += 1;
        self.outcome
    }
}
fn schedule() -> Schedule {
    Schedule {
        cycle: Cycle::CalendarMonth,
        anchor_date: NaiveDate::from_ymd_opt(2026, 9, 17).unwrap(),
        period_index: 1,
        expected_date: NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        enabled: true,
        days_before: 3,
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
    SubscriptionRuntimeMock::initialize_local_schema(&p)
        .await
        .unwrap();
    p
}
async fn create(p: &PgPool, u: &str) -> SubscriptionRuntimeMock {
    SubscriptionRuntimeMock::create_fixture_mock(p.clone(), u, fixture(u))
        .await
        .unwrap()
}
async fn register(r: &mut SubscriptionRuntimeMock, u: &str) -> MockConfirmationBinding {
    let s = sample(u);
    let b = r
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(u, "register"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    r.register_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
        .await
        .unwrap();
    b
}
async fn settings(r: &mut SubscriptionRuntimeMock, u: &str, s: Schedule) -> ScheduleBinding {
    let id = r.ledger_mock().list_mock(u).unwrap()[0]
        .subscription_id
        .clone();
    let b = r
        .prepare_settings_mock(ctx(u, "settings"), &id, s, NOW, EXPIRY)
        .await
        .unwrap();
    r.confirm_settings_mock(&b, NOW, &mut Gate { calls: 0 })
        .await
        .unwrap();
    b
}
async fn prepared(p: &PgPool, u: &str) -> SubscriptionRuntimeMock {
    let mut r = create(p, u).await;
    register(&mut r, u).await;
    settings(&mut r, u, schedule()).await;
    r
}

#[test]
#[ignore = "requires dedicated local PostgreSQL test database"]
fn registration_settings_delivery_and_actual_cli_survive_process_restart() {
    let u = owner();
    let mut reports = vec![];
    for phase in ["write", "read"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "runtime_child", "--nocapture"])
            .env("SAFEFLOW_RUNTIME_PHASE", phase)
            .env("SAFEFLOW_RUNTIME_OWNER", &u)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "RUNTIME_CHILD_FAILED: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("RUNTIME_PROBE_JSON:"))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        println!("RUNTIME_PROBE_JSON:{v}");
        reports.push(v);
    }
    assert_ne!(reports[0]["pid"], reports[1]["pid"]);
    assert_eq!(reports[1]["resend_calls"], 0);
    let exe = std::env::var("SAFEFLOW_SUBSCRIPTION_RUNTIME_STARTUP_EXE")
        .expect("BUILT_STARTUP_EXE_REQUIRED");
    let out = std::process::Command::new(exe)
        .args(["start", &u])
        .output()
        .unwrap();
    assert!(out.status.success(), "STARTUP_CLI_FAILED");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["subscription_runtime_ready"], true);
    assert_eq!(v["subscriptions"], 1);
    assert_eq!(v["registration_events"], 1);
    assert_eq!(v["full_banking_system_recovered"], false);
    println!("STARTUP_CLI_JSON:{v}");
}
#[test]
#[ignore = "requires dedicated local PostgreSQL test database"]
fn crash_after_sink_before_result_save_is_not_resent() {
    let u = owner();
    for phase in ["crash-write", "crash-read"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "runtime_child", "--nocapture"])
            .env("SAFEFLOW_RUNTIME_PHASE", phase)
            .env("SAFEFLOW_RUNTIME_OWNER", &u)
            .output()
            .unwrap();
        if phase == "crash-write" {
            assert_eq!(out.status.code(), Some(23));
        } else {
            assert!(
                out.status.success(),
                "CRASH_RECOVERY_CHILD_FAILED: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            println!("{}", String::from_utf8(out.stdout).unwrap());
        }
    }
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn successful_reminder_does_not_repeat_after_restart() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    r.scan_mock(NOW, &mut Sink::new(DeliveryOutcome::SimulatedSuccess))
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    let report = r.scan_mock(NOW, &mut sink).await.unwrap();
    assert_eq!(sink.calls, 0);
    assert_eq!(report.items[0].state, ItemState::AlreadySimulated);
    assert!(report.database_connected);
    assert!(!report.notification_sent);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn definite_failure_waits_until_next_day_across_restart() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    r.scan_mock(NOW, &mut Sink::new(DeliveryOutcome::DefiniteFailure))
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert_eq!(
        r.scan_mock(NOW, &mut sink).await.unwrap().items[0].state,
        ItemState::RetryWaiting
    );
    assert_eq!(sink.calls, 0);
    r.scan_mock("2026-10-15T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(sink.calls, 1);
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    r.scan_mock("2026-10-15T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(sink.calls, 1);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn unknown_reminder_stays_held_after_restart() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    r.scan_mock(NOW, &mut Sink::new(DeliveryOutcome::Unknown))
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    let report = r
        .scan_mock("2026-10-15T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(report.items[0].state, ItemState::UnknownNeedsReview);
    assert_eq!(sink.calls, 0);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn uncommitted_registration_and_settings_require_new_operations() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    let s = sample(&u);
    let old = r
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "old"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut gate = Gate { calls: 0 };
    assert_eq!(
        r.register_mock(&old.draft_id, &old.context, NOW, &mut gate)
            .await,
        Err(RuntimeRecoveryIssue::RestartRequiresNewOperation)
    );
    assert_eq!(gate.calls, 0);
    assert!(matches!(
        r.prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "old"),
            0,
            EXPIRY
        )
        .await,
        Err(RuntimeRecoveryIssue::RestartRequiresNewOperation)
    ));
    let b = register(&mut r, &u).await;
    assert_ne!(old.draft_id, b.draft_id);
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let old = r
        .prepare_settings_mock(ctx(&u, "old-settings"), &id, schedule(), NOW, EXPIRY)
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        r.confirm_settings_mock(&old, NOW, &mut gate).await,
        Err(RuntimeRecoveryIssue::RestartRequiresNewOperation)
    );
    let b = settings(&mut r, &u, schedule()).await;
    let mut gate = Gate { calls: 0 };
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    r.confirm_settings_mock(&b, NOW, &mut gate).await.unwrap();
    assert_eq!(gate.calls, 0);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn failed_reservation_never_calls_the_sink() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    p.close().await;
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert!(matches!(
        r.scan_mock(NOW, &mut sink).await,
        Err(RuntimeRecoveryIssue::Unavailable)
    ));
    assert_eq!(sink.calls, 0);
    let p = pool().await;
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    r.scan_mock(NOW, &mut sink).await.unwrap();
    assert_eq!(sink.calls, 1);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn missing_unavailable_and_damaged_startup_do_not_create_empty_state() {
    let p = pool().await;
    let u = owner();
    assert!(matches!(
        SubscriptionRuntimeMock::start_mock(p.clone(), &u).await,
        Err(RuntimeRecoveryIssue::NotFound)
    ));
    let mut r = create(&p, &u).await;
    register(&mut r, &u).await;
    drop(r);
    sqlx::query("UPDATE safeflow_subscription_mock_v1.runtime_recovery SET journal=jsonb_set(journal,'{commands,1,receipt,notification_sent}','true'::jsonb) WHERE owner_id=$1")
        .bind(&u).execute(&p).await.unwrap();
    assert!(matches!(
        SubscriptionRuntimeMock::start_mock(p.clone(), &u).await,
        Err(RuntimeRecoveryIssue::InvalidJournal)
    ));
    p.close().await;
    assert!(matches!(
        SubscriptionRuntimeMock::start_mock(p, &u).await,
        Err(RuntimeRecoveryIssue::Unavailable)
    ));
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn wrong_owner_and_existing_fixture_cannot_reset_saved_business() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    register(&mut r, &u).await;
    assert!(matches!(
        SubscriptionRuntimeMock::create_fixture_mock(p.clone(), &u, fixture(&u)).await,
        Err(RuntimeRecoveryIssue::Conflict)
    ));
    let mut wrong = fixture(&format!("{u}-other"));
    wrong.owner_user_id = u.clone();
    assert!(matches!(
        SubscriptionRuntimeMock::create_fixture_mock(p.clone(), &format!("{u}-other"), wrong).await,
        Err(RuntimeRecoveryIssue::Business(
            MockIssue::ResourceUnavailable
        ))
    ));
    let s = sample(&u);
    let mut context = ctx(&u, "foreign");
    context.user_id.push_str("-other");
    assert!(matches!(
        r.prepare_registration_mock(&s.trusted_context_fixture, &s.snapshot, context, 0, EXPIRY)
            .await,
        Err(RuntimeRecoveryIssue::Business(
            MockIssue::ResourceUnavailable
        ))
    ));
    assert_eq!(r.ledger_mock().list_mock(&u).unwrap().len(), 1);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn stale_process_cannot_overwrite_new_startup_generation() {
    let p = pool().await;
    let u = owner();
    let mut old = create(&p, &u).await;
    let _new = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let s = sample(&u);
    assert!(matches!(
        old.prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "old"),
            0,
            EXPIRY
        )
        .await,
        Err(RuntimeRecoveryIssue::Conflict)
    ));
    assert!(old.ledger_mock().list_mock(&u).unwrap().is_empty());
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn registration_intents_and_settings_keep_original_results_together() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    let registration = register(&mut r, &u).await;
    let settings = settings(&mut r, &u, schedule()).await;
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let intent = r
        .prepare_intent_mock(ctx(&u, "cancel"), &id, MockIntentKind::Record, NOW, EXPIRY)
        .await
        .unwrap();
    let original = r
        .commit_intent_mock(
            &intent.draft_id,
            &intent.context,
            NOW,
            &mut Gate { calls: 0 },
        )
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        r.ledger_mock().list_mock(&u).unwrap()[0].status,
        LedgerStatus::Cancelling
    );
    assert_eq!(
        r.ledger_mock()
            .confirmation_result_mock(&u, "register")
            .unwrap()
            .state,
        "ACTIVE"
    );
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "settings")
        .is_ok());
    let revoke = r
        .prepare_intent_mock(ctx(&u, "revoke"), &id, MockIntentKind::Revoke, NOW, EXPIRY)
        .await
        .unwrap();
    r.commit_intent_mock(
        &revoke.draft_id,
        &revoke.context,
        NOW,
        &mut Gate { calls: 0 },
    )
    .await
    .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut gate = Gate { calls: 0 };
    r.register_mock(
        &registration.draft_id,
        &registration.context,
        NOW,
        &mut gate,
    )
    .await
    .unwrap();
    r.confirm_settings_mock(&settings, NOW, &mut gate)
        .await
        .unwrap();
    assert_eq!(
        r.commit_intent_mock(&intent.draft_id, &intent.context, NOW, &mut gate)
            .await
            .unwrap(),
        original
    );
    assert_eq!(gate.calls, 0);
    assert_eq!(r.ledger_mock().list_mock(&u).unwrap().len(), 1);
    assert_eq!(r.ledger_mock().intent_events_mock(&u).unwrap().len(), 2);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn cancellation_late_check_remains_once_and_separate_from_charge_reminder() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    r.scan_mock(NOW, &mut sink).await.unwrap();
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let b = r
        .prepare_intent_mock(ctx(&u, "cancel"), &id, MockIntentKind::Record, NOW, EXPIRY)
        .await
        .unwrap();
    r.commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let report = r
        .scan_mock("2026-10-18T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(report.items[0].kind, ReminderKind::CancellationCheck);
    assert!(report.items[0].late);
    assert_eq!(sink.calls, 2);
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    r.scan_mock("2026-10-19T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(sink.calls, 2);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn disabled_settings_and_daily_period_progress_survive_restart() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    register(&mut r, &u).await;
    let mut s = schedule();
    s.enabled = false;
    settings(&mut r, &u, s).await;
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert_eq!(
        r.scan_mock(NOW, &mut sink).await.unwrap().items[0].state,
        ItemState::Disabled
    );
    assert_eq!(sink.calls, 0);
    let u = owner();
    let mut r = create(&p, &u).await;
    register(&mut r, &u).await;
    let date = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
    settings(
        &mut r,
        &u,
        Schedule {
            cycle: Cycle::Day,
            anchor_date: date,
            period_index: 1,
            expected_date: NaiveDate::from_ymd_opt(2026, 10, 14).unwrap(),
            enabled: true,
            days_before: 0,
        },
    )
    .await;
    r.scan_mock(NOW, &mut sink).await.unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    r.scan_mock("2026-10-15T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(sink.calls, 2);
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    r.scan_mock("2026-10-15T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(sink.calls, 2);
    p.close().await;
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn failed_registration_and_settings_saves_do_not_publish_success() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    let s = sample(&u);
    let b = r
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "register"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    p.close().await;
    assert_eq!(
        r.register_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
            .await,
        Err(RuntimeRecoveryIssue::Unavailable)
    );
    assert!(r.ledger_mock().list_mock(&u).unwrap().is_empty());
    let p = pool().await;
    let r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(r
        .ledger_mock()
        .confirmation_result_mock(&u, "register")
        .is_err());
    assert!(r.ledger_mock().list_mock(&u).unwrap().is_empty());
    let u = owner();
    let mut r = create(&p, &u).await;
    register(&mut r, &u).await;
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let b = r
        .prepare_settings_mock(ctx(&u, "settings"), &id, schedule(), NOW, EXPIRY)
        .await
        .unwrap();
    p.close().await;
    assert_eq!(
        r.confirm_settings_mock(&b, NOW, &mut Gate { calls: 0 })
            .await,
        Err(RuntimeRecoveryIssue::Unavailable)
    );
    assert!(r
        .reminders_mock()
        .current_schedule_mock(r.ledger_mock(), &u, &id)
        .is_err());
    let p = pool().await;
    let r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "settings")
        .is_err());
    assert_eq!(r.ledger_mock().list_mock(&u).unwrap().len(), 1);
    p.close().await;
}

#[tokio::test]
#[ignore = "subprocess helper"]
async fn runtime_child() {
    let phase = std::env::var("SAFEFLOW_RUNTIME_PHASE").expect("CHILD_PHASE_REQUIRED");
    let u = std::env::var("SAFEFLOW_RUNTIME_OWNER").expect("CHILD_OWNER_REQUIRED");
    let p = pool().await;
    if phase == "query-lost-write" {
        let runtime = create(&p, &u).await;
        let mut tx = p.begin().await.unwrap();
        let pending = stage_registration(&runtime, &mut tx, &u).await;
        let requests = pending.recovery_requests_mock().unwrap();
        println!(
            "LOST_REPLY_REQUEST:{}",
            serde_json::json!({"pid":std::process::id(),"request":requests[0]})
        );
        tx.commit().await.unwrap();
        assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
        // Terminate without publishing a successful business reply. The request
        // above is backend recovery evidence, not a receipt or a new permission.
        std::process::exit(23);
    }
    if phase == "db-cancel-write" || phase == "db-cancel-read" {
        let day = "2026-10-17T12:00:00+08:00";
        let mut r = if phase == "db-cancel-write" {
            prepared(&p, &u).await
        } else {
            SubscriptionRuntimeMock::start_mock(p.clone(), &u)
                .await
                .unwrap()
        };
        if phase == "db-cancel-write" {
            let id = r.ledger_mock().list_mock(&u).unwrap()[0]
                .subscription_id
                .clone();
            let b = r
                .prepare_intent_mock(
                    ctx(&u, "db-cancel"),
                    &id,
                    MockIntentKind::Record,
                    NOW,
                    EXPIRY,
                )
                .await
                .unwrap();
            let receipt = r
                .commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
                .await
                .unwrap();
            assert!(!receipt.merchant_cancellation_confirmed);
            let mut sink = Sink::new(DeliveryOutcome::Unknown);
            let report = r.scan_mock(day, &mut sink).await.unwrap();
            assert_eq!(report.items[0].kind, ReminderKind::CancellationCheck);
            assert_eq!(report.items[0].state, ItemState::UnknownNeedsReview);
            assert_eq!(sink.calls, 1);
        } else {
            for date in [day, "2026-10-18T12:00:00+08:00"] {
                let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
                let report = r.scan_mock(date, &mut sink).await.unwrap();
                assert_eq!(report.items[0].kind, ReminderKind::CancellationCheck);
                assert_eq!(report.items[0].state, ItemState::UnknownNeedsReview);
                assert_eq!(sink.calls, 0);
                assert!(!report.notification_sent);
            }
        }
        assert_eq!(
            r.ledger_mock().list_mock(&u).unwrap()[0].status,
            LedgerStatus::Cancelling
        );
        assert_eq!(r.ledger_mock().intent_events_mock(&u).unwrap().len(), 1);
        assert!(r
            .ledger_mock()
            .confirmation_result_mock(&u, "register")
            .is_ok());
        assert!(r
            .reminders_mock()
            .settings_result_mock(r.ledger_mock(), &u, "settings")
            .is_ok());
        println!(
            "CANCEL_DATABASE_PROBE_JSON:{}",
            serde_json::json!({"phase":phase,"pid":std::process::id(),"cancelling_retained":true,"unknown_retained":true,"notification_sent":false})
        );
        p.close().await;
        return;
    }
    if phase == "takeover-during-send" {
        let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        assert_eq!(
            r.scan_mock(
                &std::env::var("SAFEFLOW_TAKEOVER_NOW").unwrap_or_else(|_| NOW.into()),
                &mut sink
            )
            .await
            .unwrap()
            .items[0]
                .state,
            ItemState::UnknownNeedsReview
        );
        assert_eq!(sink.calls, 0);
        println!("TAKEOVER_PID:{}", std::process::id());
        p.close().await;
        return;
    }
    if phase.starts_with("journey-") {
        journey_phase(&p, &u, &phase).await;
        p.close().await;
        return;
    }
    if phase == "write" || phase == "crash-write" {
        let mut r = prepared(&p, &u).await;
        if phase == "crash-write" {
            struct Crash;
            impl MockInAppSink for Crash {
                fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
                    std::process::exit(23)
                }
            }
            let _ = r.scan_mock(NOW, &mut Crash).await;
            panic!("CRASH_NOT_TRIGGERED");
        }
        r.scan_mock(NOW, &mut Sink::new(DeliveryOutcome::SimulatedSuccess))
            .await
            .unwrap();
    } else {
        let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        assert_eq!(r.ledger_mock().list_mock(&u).unwrap().len(), 1);
        assert!(r
            .ledger_mock()
            .confirmation_result_mock(&u, "register")
            .is_ok());
        let id = r.ledger_mock().list_mock(&u).unwrap()[0]
            .subscription_id
            .clone();
        assert_eq!(
            r.reminders_mock()
                .current_schedule_mock(r.ledger_mock(), &u, &id)
                .unwrap(),
            &schedule()
        );
        assert!(r
            .reminders_mock()
            .settings_result_mock(r.ledger_mock(), &u, "settings")
            .is_ok());
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let report = r.scan_mock(NOW, &mut sink).await.unwrap();
        assert_eq!(sink.calls, 0);
        assert_eq!(
            report.items[0].state,
            if phase == "crash-read" {
                ItemState::UnknownNeedsReview
            } else {
                ItemState::AlreadySimulated
            }
        );
    }
    println!(
        "RUNTIME_PROBE_JSON:{}",
        serde_json::json!({"phase":phase,"pid":std::process::id(),
        "registration_restored":true,"settings_restored":true,"resend_calls":0,"full_banking_system_recovered":false})
    );
    p.close().await;
}

/// One subscription, chronological business dates, five independent OS processes.
#[test]
#[ignore = "requires dedicated local PostgreSQL test database"]
fn full_subscription_journey_across_five_processes() {
    let u = owner();
    let mut seen = std::collections::HashSet::new();
    for phase in [
        "journey-register",
        "journey-remind",
        "journey-cancel",
        "journey-check",
        "journey-revoke",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "runtime_child", "--nocapture"])
            .env("SAFEFLOW_RUNTIME_PHASE", phase)
            .env("SAFEFLOW_RUNTIME_OWNER", &u)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {} {}",
            phase,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            text.lines()
                .find_map(|l| l.strip_prefix("JOURNEY_JSON:"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["phase"], phase);
        assert!(seen.insert(v["pid"].as_u64().unwrap()));
        println!("JOURNEY_JSON:{v}");
    }
}

async fn journey_phase(p: &PgPool, u: &str, phase: &str) {
    let mut r = if phase == "journey-register" {
        create(p, u).await
    } else {
        SubscriptionRuntimeMock::start_mock(p.clone(), u)
            .await
            .unwrap()
    };
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    if phase == "journey-register" {
        let s = sample(u);
        let recognition = r
            .ledger_mock()
            .discover_mock(&s.trusted_context_fixture, &s.snapshot)
            .unwrap();
        assert!(recognition.matching_executed);
        assert_eq!(recognition.groups[0].disposition, Disposition::NotInLedger);
        assert!(!recognition.groups.is_empty());
        register(&mut r, u).await;
        settings(&mut r, u, schedule()).await;
    }
    let id = r.ledger_mock().list_mock(u).unwrap()[0]
        .subscription_id
        .clone();
    assert_eq!(r.ledger_mock().list_mock(u).unwrap().len(), 1);
    assert_eq!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), u, &id)
            .unwrap(),
        &schedule()
    );
    assert!(r
        .ledger_mock()
        .list_mock("another-fictional-owner")
        .is_err());
    let day = "2026-10-17T12:00:00+08:00";
    if phase == "journey-remind" {
        let report = r.scan_mock(NOW, &mut sink).await.unwrap();
        assert_eq!(report.items[0].kind, ReminderKind::ChargeAdvance);
        assert_eq!(report.items[0].state, ItemState::SimulatedSuccess);
        assert!(!report.notification_sent);
        assert_eq!(sink.calls, 1);
    } else if phase == "journey-cancel" {
        assert_eq!(
            r.scan_mock(NOW, &mut sink).await.unwrap().items[0].state,
            ItemState::AlreadySimulated
        );
        let b = r
            .prepare_intent_mock(
                ctx(u, "journey-cancel"),
                &id,
                MockIntentKind::Record,
                NOW,
                EXPIRY,
            )
            .await
            .unwrap();
        let receipt = r
            .commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
            .await
            .unwrap();
        assert!(!receipt.merchant_cancellation_confirmed);
        assert!(!receipt.merchant_action_executed);
        let report = r.scan_mock(NOW, &mut sink).await.unwrap();
        assert_eq!(report.items[0].kind, ReminderKind::CancellationCheck);
        assert_eq!(report.items[0].state, ItemState::NotDue);
        assert_eq!(sink.calls, 0);
    } else if phase == "journey-check" {
        assert_eq!(
            r.ledger_mock().list_mock(u).unwrap()[0].status,
            LedgerStatus::Cancelling
        );
        let report = r.scan_mock(day, &mut sink).await.unwrap();
        assert_eq!(report.items[0].kind, ReminderKind::CancellationCheck);
        assert_eq!(report.items[0].state, ItemState::SimulatedSuccess);
        assert!(!report.items[0].late);
        assert!(!report.notification_sent);
        assert_eq!(sink.calls, 1);
        // A reminder to check a charge is not evidence of a charge or cancellation.
        assert_eq!(
            r.ledger_mock().list_mock(u).unwrap()[0].status,
            LedgerStatus::Cancelling
        );
    } else if phase == "journey-revoke" {
        assert_eq!(
            r.scan_mock(day, &mut sink).await.unwrap().items[0].state,
            ItemState::AlreadySimulated
        );
        let b = r
            .prepare_intent_mock(
                ctx(u, "journey-revoke"),
                &id,
                MockIntentKind::Revoke,
                day,
                "2026-10-17T12:05:00+08:00",
            )
            .await
            .unwrap();
        let receipt = r
            .commit_intent_mock(&b.draft_id, &b.context, day, &mut Gate { calls: 0 })
            .await
            .unwrap();
        assert!(!receipt.merchant_service_restored);
        assert_eq!(
            r.ledger_mock().list_mock(u).unwrap()[0].status,
            LedgerStatus::Active
        );
        let report = r.scan_mock(day, &mut sink).await.unwrap();
        assert_eq!(report.items[0].kind, ReminderKind::ChargeAdvance);
        assert_eq!(report.items[0].state, ItemState::AlreadySimulated);
        assert_eq!(sink.calls, 0);
        assert_eq!(r.ledger_mock().intent_events_mock(u).unwrap().len(), 2);
    }
    println!(
        "JOURNEY_JSON:{}",
        serde_json::json!({"phase":phase,"pid":std::process::id(),
        "fictional":true,"mock_deliveries_this_process":sink.calls,"notification_sent":false,
        "full_banking_system_recovered":false,"status":r.ledger_mock().list_mock(u).unwrap()[0].status})
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn replaced_runtime_cannot_start_reminder_delivery() {
    let p = pool().await;
    let u = owner();
    let mut old = prepared(&p, &u).await;
    let mut current = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut stale_sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert!(matches!(
        old.scan_mock(NOW, &mut stale_sink).await,
        Err(RuntimeRecoveryIssue::Conflict)
    ));
    assert_eq!(stale_sink.calls, 0);
    let mut current_sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert_eq!(
        current
            .scan_mock(NOW, &mut current_sink)
            .await
            .unwrap()
            .items[0]
            .state,
        ItemState::SimulatedSuccess
    );
    assert_eq!(current_sink.calls, 1);
    drop(current);
    let mut restarted = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        restarted
            .scan_mock(NOW, &mut current_sink)
            .await
            .unwrap()
            .items[0]
            .state,
        ItemState::AlreadySimulated
    );
    assert!(matches!(
        old.scan_mock(NOW, &mut stale_sink).await,
        Err(RuntimeRecoveryIssue::Conflict)
    ));
    assert_eq!(stale_sink.calls, 0);
    assert_eq!(current_sink.calls, 1);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn takeover_during_send_preserves_unknown_without_resending() {
    check_takeover_during_send(false).await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn cancellation_check_takeover_preserves_intent_and_unknown() {
    check_takeover_during_send(true).await;
}

async fn check_takeover_during_send(cancellation: bool) {
    struct TakeoverSink {
        owner: String,
        calls: usize,
        now: &'static str,
        kind: ReminderKind,
    }
    impl MockInAppSink for TakeoverSink {
        fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome {
            assert_eq!(message.kind, self.kind);
            assert!(message.fictional);
            self.calls += 1;
            // Reserve is durable but the old sender has not returned its result.
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "runtime_child", "--nocapture"])
                .env("SAFEFLOW_RUNTIME_PHASE", "takeover-during-send")
                .env("SAFEFLOW_RUNTIME_OWNER", &self.owner)
                .env("SAFEFLOW_TAKEOVER_NOW", self.now)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8(out.stdout).unwrap();
            let pid: u32 = text
                .lines()
                .find_map(|l| l.strip_prefix("TAKEOVER_PID:"))
                .unwrap()
                .parse()
                .unwrap();
            assert_ne!(pid, std::process::id());
            println!("TAKEOVER_PID:{pid}");
            DeliveryOutcome::SimulatedSuccess
        }
    }
    let p = pool().await;
    let u = owner();
    let mut old = prepared(&p, &u).await;
    if cancellation {
        let id = old.ledger_mock().list_mock(&u).unwrap()[0]
            .subscription_id
            .clone();
        let b = old
            .prepare_intent_mock(
                ctx(&u, "cancel-before-takeover"),
                &id,
                MockIntentKind::Record,
                NOW,
                EXPIRY,
            )
            .await
            .unwrap();
        let receipt = old
            .commit_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
            .await
            .unwrap();
        assert!(!receipt.merchant_cancellation_confirmed);
        assert!(!receipt.merchant_action_executed);
    }
    let dates = if cancellation {
        ["2026-10-17T12:00:00+08:00", "2026-10-18T12:00:00+08:00"]
    } else {
        [NOW, "2026-10-15T12:00:00+08:00"]
    };
    let kind = if cancellation {
        ReminderKind::CancellationCheck
    } else {
        ReminderKind::ChargeAdvance
    };
    let mut sink = TakeoverSink {
        owner: u.clone(),
        calls: 0,
        now: dates[0],
        kind,
    };
    assert!(matches!(
        old.scan_mock(dates[0], &mut sink).await,
        Err(RuntimeRecoveryIssue::Conflict)
    ));
    assert_eq!(sink.calls, 1);
    drop(old);
    for now in dates {
        let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let report = r.scan_mock(now, &mut retry).await.unwrap();
        assert_eq!(report.items[0].state, ItemState::UnknownNeedsReview);
        assert_eq!(report.items[0].kind, kind);
        if cancellation {
            assert_eq!(
                r.ledger_mock().list_mock(&u).unwrap()[0].status,
                LedgerStatus::Cancelling
            );
            assert_eq!(r.ledger_mock().intent_events_mock(&u).unwrap().len(), 1);
        }
        assert_eq!(retry.calls, 0);
        assert!(!report.notification_sent);
    }
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn unknown_attempt_survives_disable_restart_and_reenable() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut first = Sink::new(DeliveryOutcome::Unknown);
    assert_eq!(
        r.scan_mock(NOW, &mut first).await.unwrap().items[0].state,
        ItemState::UnknownNeedsReview
    );
    assert_eq!(first.calls, 1);
    for (operation, enabled) in [
        ("disable-after-unknown", false),
        ("reenable-after-unknown", true),
    ] {
        drop(r);
        r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        let mut configuration = schedule();
        configuration.enabled = enabled;
        let binding = r
            .prepare_settings_mock(ctx(&u, operation), &id, configuration, NOW, EXPIRY)
            .await
            .unwrap();
        let receipt = r
            .confirm_settings_mock(&binding, NOW, &mut Gate { calls: 0 })
            .await
            .unwrap();
        assert!(!receipt.notification_sent);
        drop(r);
        r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        assert_eq!(
            r.reminders_mock()
                .current_schedule_mock(r.ledger_mock(), &u, &id)
                .unwrap()
                .enabled,
            enabled
        );
        assert!(r
            .reminders_mock()
            .settings_result_mock(r.ledger_mock(), &u, operation)
            .is_ok());
        let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let report = r.scan_mock(NOW, &mut retry).await.unwrap();
        assert_eq!(
            report.items[0].state,
            if enabled {
                ItemState::UnknownNeedsReview
            } else {
                ItemState::Disabled
            }
        );
        assert_eq!(retry.calls, 0);
        assert!(!report.notification_sent);
    }
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert_eq!(
        r.scan_mock("2026-10-15T12:00:00+08:00", &mut retry)
            .await
            .unwrap()
            .items[0]
            .state,
        ItemState::UnknownNeedsReview
    );
    assert_eq!(retry.calls, 0);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn replayed_old_settings_cannot_override_newer_disabled_settings() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    register(&mut r, &u).await;
    let original = settings(&mut r, &u, schedule()).await;
    let original_receipt = r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "settings")
        .unwrap()
        .clone();
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut disabled = schedule();
    disabled.enabled = false;
    let b = r
        .prepare_settings_mock(ctx(&u, "newer-disable"), &id, disabled, NOW, EXPIRY)
        .await
        .unwrap();
    let disabled_receipt = r
        .confirm_settings_mock(&b, NOW, &mut Gate { calls: 0 })
        .await
        .unwrap();
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut gate = Gate { calls: 0 };
    // Retry an already-completed original request after its draft expiry.
    let retry_time = "2026-10-15T12:00:00+08:00";
    assert_eq!(
        r.confirm_settings_mock(&original, retry_time, &mut gate)
            .await
            .unwrap(),
        original_receipt
    );
    assert_eq!(gate.calls, 0);
    assert!(
        !r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap()
            .enabled
    );
    assert_eq!(
        r.reminders_mock()
            .settings_result_mock(r.ledger_mock(), &u, "newer-disable")
            .unwrap(),
        &disabled_receipt
    );
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(
        !r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap()
            .enabled
    );
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    let report = r.scan_mock(retry_time, &mut sink).await.unwrap();
    assert_eq!(report.items[0].state, ItemState::Disabled);
    assert_eq!(sink.calls, 0);
    assert!(!report.notification_sent);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn failed_disable_preserves_enabled_setting_and_unknown_attempt() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut initial = Sink::new(DeliveryOutcome::Unknown);
    assert_eq!(
        r.scan_mock(NOW, &mut initial).await.unwrap().items[0].state,
        ItemState::UnknownNeedsReview
    );
    assert_eq!(initial.calls, 1);
    let mut disabled = schedule();
    disabled.enabled = false;
    let binding = r
        .prepare_settings_mock(ctx(&u, "disable-save-fails"), &id, disabled, NOW, EXPIRY)
        .await
        .unwrap();
    p.close().await;
    assert_eq!(
        r.confirm_settings_mock(&binding, NOW, &mut Gate { calls: 0 })
            .await,
        Err(RuntimeRecoveryIssue::Unavailable)
    );
    assert!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap()
            .enabled
    );
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "disable-save-fails")
        .is_err());
    drop(r);
    let p = pool().await;
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap()
            .enabled
    );
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "disable-save-fails")
        .is_err());
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "settings")
        .is_ok());
    let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
    let report = r.scan_mock(NOW, &mut retry).await.unwrap();
    assert_eq!(report.items[0].state, ItemState::UnknownNeedsReview);
    assert_eq!(retry.calls, 0);
    assert!(!report.notification_sent);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn same_operation_ids_keep_two_owners_recovery_isolated() {
    let p = pool().await;
    let a = owner();
    let b = owner();
    // The helpers intentionally use identical operation IDs and idempotency keys.
    let mut first = prepared(&p, &a).await;
    let mut second = prepared(&p, &b).await;
    let first_id = first.ledger_mock().list_mock(&a).unwrap()[0]
        .subscription_id
        .clone();
    let mut disabled = schedule();
    disabled.enabled = false;
    let binding = first
        .prepare_settings_mock(ctx(&a, "disable"), &first_id, disabled, NOW, EXPIRY)
        .await
        .unwrap();
    first
        .confirm_settings_mock(&binding, NOW, &mut Gate { calls: 0 })
        .await
        .unwrap();
    let mut unknown = Sink::new(DeliveryOutcome::Unknown);
    assert_eq!(
        second.scan_mock(NOW, &mut unknown).await.unwrap().items[0].state,
        ItemState::UnknownNeedsReview
    );
    assert_eq!(unknown.calls, 1);
    drop(first);
    drop(second);
    for (user, other, expected) in [
        (&a, &b, ItemState::Disabled),
        (&b, &a, ItemState::UnknownNeedsReview),
    ] {
        let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), user)
            .await
            .unwrap();
        assert!(r.ledger_mock().list_mock(other).is_err());
        assert!(r
            .ledger_mock()
            .confirmation_result_mock(other, "register")
            .is_err());
        assert!(r
            .reminders_mock()
            .settings_result_mock(r.ledger_mock(), other, "settings")
            .is_err());
        let entries = r.ledger_mock().list_mock(user).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(&entries[0].owner_user_id, user);
        assert!(r
            .ledger_mock()
            .confirmation_result_mock(user, "register")
            .is_ok());
        assert!(r
            .reminders_mock()
            .settings_result_mock(r.ledger_mock(), user, "settings")
            .is_ok());
        assert_eq!(
            r.reminders_mock()
                .settings_result_mock(r.ledger_mock(), user, "disable")
                .is_ok(),
            user == &a
        );
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let report = r.scan_mock(NOW, &mut sink).await.unwrap();
        assert_eq!(report.items[0].state, expected);
        assert_eq!(sink.calls, 0);
        assert!(!report.notification_sent);
    }
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn damaged_owner_does_not_reset_records_or_block_other_owner() {
    let p = pool().await;
    let damaged = owner();
    let healthy = owner();
    let bad = prepared(&p, &damaged).await;
    let mut good = prepared(&p, &healthy).await;
    let mut first = Sink::new(DeliveryOutcome::SimulatedSuccess);
    good.scan_mock(NOW, &mut first).await.unwrap();
    assert_eq!(first.calls, 1);
    drop(bad);
    drop(good);
    // Deliberately corrupt only a fictional owner's persisted receipt.
    assert_eq!(sqlx::query("UPDATE safeflow_subscription_mock_v1.runtime_recovery SET journal=jsonb_set(journal,'{commands,1,receipt,notification_sent}','true'::jsonb) WHERE owner_id=$1")
        .bind(&damaged).execute(&p).await.unwrap().rows_affected(), 1);
    let saved: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&damaged).fetch_one(&p).await.unwrap();
    for _ in 0..2 {
        assert!(matches!(
            SubscriptionRuntimeMock::start_mock(p.clone(), &damaged).await,
            Err(RuntimeRecoveryIssue::InvalidJournal)
        ));
        let mut current = SubscriptionRuntimeMock::start_mock(p.clone(), &healthy)
            .await
            .unwrap();
        assert_eq!(current.ledger_mock().list_mock(&healthy).unwrap().len(), 1);
        assert!(current
            .ledger_mock()
            .confirmation_result_mock(&healthy, "register")
            .is_ok());
        assert!(current
            .reminders_mock()
            .settings_result_mock(current.ledger_mock(), &healthy, "settings")
            .is_ok());
        let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
        assert_eq!(
            current.scan_mock(NOW, &mut retry).await.unwrap().items[0].state,
            ItemState::AlreadySimulated
        );
        assert_eq!(retry.calls, 0);
        let after: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
            .bind(&damaged).fetch_one(&p).await.unwrap();
        assert_eq!(after, saved);
    }
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn actual_startup_cli_rejects_missing_and_corrupt_records_without_reset() {
    let p = pool().await;
    let missing = owner();
    let damaged = owner();
    drop(prepared(&p, &damaged).await);
    sqlx::query("UPDATE safeflow_subscription_mock_v1.runtime_recovery SET journal=jsonb_set(journal,'{commands,1,receipt,notification_sent}','true'::jsonb) WHERE owner_id=$1")
        .bind(&damaged).execute(&p).await.unwrap();
    let before: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&damaged).fetch_one(&p).await.unwrap();
    let exe = std::env::var("SAFEFLOW_SUBSCRIPTION_RUNTIME_STARTUP_EXE")
        .expect("BUILT_STARTUP_EXE_REQUIRED");
    for (user, expected) in [(&missing, "NotFound"), (&damaged, "InvalidJournal")] {
        let output = std::process::Command::new(&exe)
            .args(["start", user])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), expected);
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1",
    )
    .bind(&missing)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(count, 0);
    let after: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&damaged).fetch_one(&p).await.unwrap();
    assert_eq!(after, before);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn actual_cli_reinitialization_cannot_erase_existing_subscription() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    r.scan_mock(NOW, &mut sink).await.unwrap();
    assert_eq!(sink.calls, 1);
    drop(r);
    let before: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    let path = std::env::temp_dir().join(format!("safeflow-reinit-{u}.json"));
    // Create only this uniquely named fictional fixture; do not overwrite files.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, &serde_json::to_vec(&fixture(&u)).unwrap()).unwrap();
    drop(file);
    let exe = std::env::var("SAFEFLOW_SUBSCRIPTION_RUNTIME_STARTUP_EXE")
        .expect("BUILT_STARTUP_EXE_REQUIRED");
    let output = std::process::Command::new(exe)
        .arg("init")
        .arg(&path)
        .output();
    std::fs::remove_file(&path).unwrap();
    let output = output.unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), "Conflict");
    let after: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    assert_eq!(after, before);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let entries = r.ledger_mock().list_mock(&u).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &entries[0].subscription_id)
            .unwrap(),
        &schedule()
    );
    assert!(r
        .ledger_mock()
        .confirmation_result_mock(&u, "register")
        .is_ok());
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "settings")
        .is_ok());
    let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert_eq!(
        r.scan_mock(NOW, &mut retry).await.unwrap().items[0].state,
        ItemState::AlreadySimulated
    );
    assert_eq!(retry.calls, 0);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn clock_rollback_after_restart_cannot_change_saved_reminders() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let mut first = Sink::new(DeliveryOutcome::SimulatedSuccess);
    r.scan_mock(NOW, &mut first).await.unwrap();
    assert_eq!(first.calls, 1);
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let before: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert!(matches!(
        r.scan_mock("2026-10-13T12:00:00+08:00", &mut retry).await,
        Err(RuntimeRecoveryIssue::Reminder("CLOCK_MOVED_BACKWARD"))
    ));
    assert_eq!(retry.calls, 0);
    let after: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    assert_eq!(after, before);
    assert!(r
        .ledger_mock()
        .confirmation_result_mock(&u, "register")
        .is_ok());
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "settings")
        .is_ok());
    assert_eq!(
        r.scan_mock(NOW, &mut retry).await.unwrap().items[0].state,
        ItemState::AlreadySimulated
    );
    assert_eq!(retry.calls, 0);
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        r.scan_mock("2026-10-15T12:00:00+08:00", &mut retry)
            .await
            .unwrap()
            .items[0]
            .state,
        ItemState::AlreadySimulated
    );
    assert_eq!(retry.calls, 0);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn concurrent_startups_leave_only_one_reminder_writer() {
    let p = pool().await;
    let u = owner();
    drop(prepared(&p, &u).await);
    let (left, right) = tokio::join!(
        SubscriptionRuntimeMock::start_mock(p.clone(), &u),
        SubscriptionRuntimeMock::start_mock(p.clone(), &u)
    );
    let mut active = Vec::new();
    for result in [left, right] {
        match result {
            Ok(runtime) => active.push(runtime),
            Err(issue) => assert_eq!(issue, RuntimeRecoveryIssue::Conflict),
        }
    }
    assert!(!active.is_empty());
    let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
    let mut completed = 0;
    for runtime in &mut active {
        match runtime.scan_mock(NOW, &mut sink).await {
            Ok(report) => {
                assert_eq!(report.items[0].state, ItemState::SimulatedSuccess);
                assert!(!report.notification_sent);
                completed += 1;
            }
            Err(issue) => assert_eq!(issue, RuntimeRecoveryIssue::Conflict),
        }
    }
    assert_eq!(completed, 1);
    assert_eq!(sink.calls, 1);
    drop(active);
    let mut restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(restored.ledger_mock().list_mock(&u).unwrap().len(), 1);
    assert_eq!(
        restored.scan_mock(NOW, &mut sink).await.unwrap().items[0].state,
        ItemState::AlreadySimulated
    );
    assert_eq!(sink.calls, 1);
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn mixed_delivery_results_remain_separate_for_one_owners_subscriptions() {
    struct MixedSink {
        calls: Vec<String>,
    }
    impl MockInAppSink for MixedSink {
        fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome {
            self.calls.push(message.subscription_id.clone());
            if message.subscription_id == "fictional-sub-unknown" {
                DeliveryOutcome::Unknown
            } else {
                DeliveryOutcome::SimulatedSuccess
            }
        }
    }
    let p = pool().await;
    let u = owner();
    let mut initial = fixture(&u);
    for id in ["fictional-sub-unknown", "fictional-sub-success"] {
        initial.entries.push(LedgerEntry {
            subscription_id: id.into(),
            owner_user_id: u.clone(),
            source_id: Some("fictional-source".into()),
            merchant_id: Some(format!("fictional-merchant-{id}")),
            service_ref: Some(format!("fictional-service-{id}")),
            status: LedgerStatus::Active,
        });
    }
    let mut r = SubscriptionRuntimeMock::create_fixture_mock(p.clone(), &u, initial)
        .await
        .unwrap();
    for id in ["fictional-sub-unknown", "fictional-sub-success"] {
        let b = r
            .prepare_settings_mock(
                ctx(&u, &format!("settings-{id}")),
                id,
                schedule(),
                NOW,
                EXPIRY,
            )
            .await
            .unwrap();
        r.confirm_settings_mock(&b, NOW, &mut Gate { calls: 0 })
            .await
            .unwrap();
    }
    let mut sink = MixedSink { calls: vec![] };
    let first = r.scan_mock(NOW, &mut sink).await.unwrap();
    assert_eq!(first.items.len(), 2);
    sink.calls.sort();
    assert_eq!(
        sink.calls,
        vec!["fictional-sub-success", "fictional-sub-unknown"]
    );
    for item in &first.items {
        assert_eq!(
            item.state,
            if item.subscription_id == "fictional-sub-unknown" {
                ItemState::UnknownNeedsReview
            } else {
                ItemState::SimulatedSuccess
            }
        );
    }
    drop(r);
    for now in [NOW, "2026-10-15T12:00:00+08:00"] {
        let mut restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        assert_eq!(restored.ledger_mock().list_mock(&u).unwrap().len(), 2);
        let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let report = restored.scan_mock(now, &mut retry).await.unwrap();
        assert_eq!(report.items.len(), 2);
        for item in report.items {
            assert_eq!(
                item.state,
                if item.subscription_id == "fictional-sub-unknown" {
                    ItemState::UnknownNeedsReview
                } else {
                    ItemState::AlreadySimulated
                }
            );
        }
        assert_eq!(retry.calls, 0);
        assert!(!report.notification_sent);
    }
    p.close().await;
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn rejected_settings_confirmation_after_restart_preserves_state() {
    struct DeniedGate {
        calls: usize,
    }
    impl MockScheduleGate for DeniedGate {
        fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
            self.calls += 1;
            MockGateOutcome::Denied
        }
    }
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let mut unknown = Sink::new(DeliveryOutcome::Unknown);
    r.scan_mock(NOW, &mut unknown).await.unwrap();
    assert_eq!(unknown.calls, 1);
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut disabled = schedule();
    disabled.enabled = false;
    let b = r
        .prepare_settings_mock(ctx(&u, "denied-disable"), &id, disabled, NOW, EXPIRY)
        .await
        .unwrap();
    let before: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    let mut gate = DeniedGate { calls: 0 };
    assert_eq!(
        r.confirm_settings_mock(&b, NOW, &mut gate).await,
        Err(RuntimeRecoveryIssue::Reminder("CONFIRMATION_REQUIRED"))
    );
    assert_eq!(gate.calls, 1);
    let after: (i64, serde_json::Value) = sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    assert_eq!(after, before);
    assert!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap()
            .enabled
    );
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "denied-disable")
        .is_err());
    drop(r);
    let mut r = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap()
            .enabled
    );
    assert!(r
        .reminders_mock()
        .settings_result_mock(r.ledger_mock(), &u, "denied-disable")
        .is_err());
    let mut retry = Sink::new(DeliveryOutcome::SimulatedSuccess);
    assert_eq!(
        r.scan_mock(NOW, &mut retry).await.unwrap().items[0].state,
        ItemState::UnknownNeedsReview
    );
    assert_eq!(retry.calls, 0);
    p.close().await;
}

async fn stage_registration(
    runtime: &SubscriptionRuntimeMock,
    connection: &mut sqlx::PgConnection,
    user: &str,
) -> PendingRuntimeWriteMock<MockConfirmationReceipt> {
    let user = user.to_owned();
    runtime
        .stage_on_connection_mock(connection, move |candidate| {
            Box::pin(async move {
                let s = sample(&user);
                let b = candidate
                    .prepare_registration_mock(
                        &s.trusted_context_fixture,
                        &s.snapshot,
                        ctx(&user, "register"),
                        0,
                        EXPIRY,
                    )
                    .await?;
                candidate
                    .register_mock(&b.draft_id, &b.context, NOW, &mut Gate { calls: 0 })
                    .await
            })
        })
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_commit_publishes_registration_and_restores_original() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let pending = stage_registration(&runtime, &mut tx, &u).await;
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
    let outside: i64 = sqlx::query_scalar(
        "SELECT version FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1",
    )
    .bind(&u)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(outside, 1);
    tx.commit().await.unwrap();
    let receipt = pending
        .publish_after_commit_mock(&mut runtime)
        .await
        .unwrap();
    assert_eq!(runtime.ledger_mock().list_mock(&u).unwrap().len(), 1);
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let original = restored
        .ledger_mock()
        .confirmation_result_mock(&u, "register")
        .unwrap();
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(receipt).unwrap()
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_audit_failure_rolls_back_registration_and_memory() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let pending = stage_registration(&runtime, &mut tx, &u).await;
    assert!(sqlx::query("SELECT 1/0").execute(&mut *tx).await.is_err());
    tx.rollback().await.unwrap();
    assert_eq!(
        pending
            .publish_after_commit_mock(&mut runtime)
            .await
            .unwrap_err(),
        RuntimeRecoveryIssue::CommitNotObserved
    );
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert!(restored.ledger_mock().list_mock(&u).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_final_commit_failure_cannot_publish_success() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    sqlx::query("CREATE TEMP TABLE subscription_commit_probe (id int UNIQUE DEFERRABLE INITIALLY DEFERRED) ON COMMIT DROP")
        .execute(&mut *tx).await.unwrap();
    let pending = stage_registration(&runtime, &mut tx, &u).await;
    sqlx::query("INSERT INTO subscription_commit_probe VALUES (1),(1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(tx.commit().await.is_err());
    assert_eq!(
        pending
            .publish_after_commit_mock(&mut runtime)
            .await
            .unwrap_err(),
        RuntimeRecoveryIssue::CommitNotObserved
    );
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_open_transaction_cannot_publish_receipt() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let pending = stage_registration(&runtime, &mut tx, &u).await;
    assert_eq!(
        pending
            .publish_after_commit_mock(&mut runtime)
            .await
            .unwrap_err(),
        RuntimeRecoveryIssue::CommitNotObserved
    );
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_stale_version_rejects_overwrite() {
    let p = pool().await;
    let u = owner();
    let runtime = create(&p, &u).await;
    let active = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let mut tx = p.begin().await.unwrap();
    let user = u.clone();
    let result = runtime
        .stage_on_connection_mock(&mut tx, move |candidate| {
            Box::pin(async move {
                let s = sample(&user);
                candidate
                    .prepare_registration_mock(
                        &s.trusted_context_fixture,
                        &s.snapshot,
                        ctx(&user, "register"),
                        0,
                        EXPIRY,
                    )
                    .await
            })
        })
        .await;
    assert!(matches!(result, Err(RuntimeRecoveryIssue::Conflict)));
    tx.rollback().await.unwrap();
    assert!(active.ledger_mock().list_mock(&u).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_settings_cancel_revoke_share_caller_commit() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    register(&mut runtime, &u).await;
    let user = u.clone();
    let mut tx = p.begin().await.unwrap();
    let pending = runtime
        .stage_on_connection_mock(&mut tx, move |candidate| {
            Box::pin(async move {
                let id = candidate.ledger_mock().list_mock(&user).unwrap()[0]
                    .subscription_id
                    .clone();
                let binding = candidate
                    .prepare_settings_mock(ctx(&user, "settings"), &id, schedule(), NOW, EXPIRY)
                    .await?;
                let settings = candidate
                    .confirm_settings_mock(&binding, NOW, &mut Gate { calls: 0 })
                    .await?;
                let cancel = candidate
                    .prepare_intent_mock(
                        ctx(&user, "cancel"),
                        &id,
                        MockIntentKind::Record,
                        NOW,
                        EXPIRY,
                    )
                    .await?;
                candidate
                    .commit_intent_mock(
                        &cancel.draft_id,
                        &cancel.context,
                        NOW,
                        &mut Gate { calls: 0 },
                    )
                    .await?;
                let revoke = candidate
                    .prepare_intent_mock(
                        ctx(&user, "revoke"),
                        &id,
                        MockIntentKind::Revoke,
                        NOW,
                        EXPIRY,
                    )
                    .await?;
                candidate
                    .commit_intent_mock(
                        &revoke.draft_id,
                        &revoke.context,
                        NOW,
                        &mut Gate { calls: 0 },
                    )
                    .await?;
                Ok(settings)
            })
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    pending
        .publish_after_commit_mock(&mut runtime)
        .await
        .unwrap();
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        restored.ledger_mock().list_mock(&u).unwrap()[0].status,
        LedgerStatus::Active
    );
    assert!(restored
        .ledger_mock()
        .intent_result_mock(&u, "cancel")
        .is_ok());
    assert!(restored
        .ledger_mock()
        .intent_result_mock(&u, "revoke")
        .is_ok());
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_connection_rejects_sender_before_any_external_call() {
    let p = pool().await;
    let u = owner();
    let runtime = prepared(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let result = runtime
        .stage_on_connection_mock(&mut tx, |candidate| {
            Box::pin(async move {
                let mut sink = Sink {
                    calls: 0,
                    outcome: DeliveryOutcome::SimulatedSuccess,
                };
                let result = candidate.scan_mock(NOW, &mut sink).await;
                assert_eq!(sink.calls, 0);
                result
            })
        })
        .await;
    assert!(matches!(
        result,
        Err(RuntimeRecoveryIssue::StagedOperationUnsupported)
    ));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_readonly_snapshot_queries_original_receipts_without_writes() {
    let p = pool().await;
    let u = owner();
    let runtime = prepared(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    let snapshot = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut tx, &u)
        .await
        .unwrap();
    assert_eq!(snapshot.ledger_mock().list_mock(&u).unwrap().len(), 1);
    assert_eq!(
        serde_json::to_value(
            snapshot
                .ledger_mock()
                .confirmation_result_mock(&u, "register")
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(
            runtime
                .ledger_mock()
                .confirmation_result_mock(&u, "register")
                .unwrap()
        )
        .unwrap()
    );
    assert!(snapshot
        .reminders_mock()
        .settings_result_mock(snapshot.ledger_mock(), &u, "settings")
        .is_ok());
    assert!(snapshot.ledger_mock().list_mock("another-owner").is_err());
    tx.commit().await.unwrap();
    let saved: i64 = sqlx::query_scalar(
        "SELECT version FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1",
    )
    .bind(&u)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(snapshot.version_mock(), saved);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_readonly_query_does_not_invalidate_pending_confirmation() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let s = sample(&u);
    let binding = runtime
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "register"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    let mut connection = p.acquire().await.unwrap();
    let snapshot = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &u)
        .await
        .unwrap();
    assert!(snapshot.ledger_mock().list_mock(&u).unwrap().is_empty());
    assert_eq!(snapshot.version_mock(), 2);
    runtime
        .register_mock(
            &binding.draft_id,
            &binding.context,
            NOW,
            &mut Gate { calls: 0 },
        )
        .await
        .unwrap();
    assert_eq!(runtime.ledger_mock().list_mock(&u).unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_readonly_missing_owner_is_not_initialized() {
    let p = pool().await;
    let u = owner();
    let mut connection = p.acquire().await.unwrap();
    let result = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &u).await;
    assert!(matches!(result, Err(RuntimeRecoveryIssue::NotFound)));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1",
    )
    .bind(&u)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn borrowed_readonly_corrupt_owner_is_rejected_without_reset() {
    let p = pool().await;
    let u = owner();
    let _runtime = prepared(&p, &u).await;
    sqlx::query("UPDATE safeflow_subscription_mock_v1.runtime_recovery SET journal=jsonb_set(journal,'{commands,1,receipt,notification_sent}',$2) WHERE owner_id=$1")
        .bind(&u).bind(serde_json::json!(true)).execute(&p).await.unwrap();
    let mut connection = p.acquire().await.unwrap();
    let result = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &u).await;
    assert!(matches!(result, Err(RuntimeRecoveryIssue::InvalidJournal)));
    let saved: String = sqlx::query_scalar("SELECT journal #>> '{commands,1,receipt,notification_sent}' FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(&u).fetch_one(&p).await.unwrap();
    assert_eq!(saved, "true");
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn write_handoff_rollback_returns_review_without_success_or_memory_change() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let handoff = stage_registration(&runtime, &mut tx, &u)
        .await
        .into_handoff_mock()
        .unwrap();
    assert_eq!(handoff.original_requests_mock().len(), 1);
    tx.rollback().await.unwrap();
    let result = handoff.complete_mock(&mut runtime, Err::<(), ()>(())).await;
    let recovery = match result {
        SubscriptionWriteCompletionMock::ReadOnlyRecovery(recovery) => recovery,
        _ => panic!("ROLLED_BACK_WRITE_REPORTED_SUCCESS"),
    };
    assert!(!recovery.continue_writes);
    assert!(!recovery.business_reexecuted);
    assert!(!recovery.runtime_refreshed);
    assert!(matches!(
        &recovery.original_results[0],
        OriginalLookupAttemptMock::Read {
            result: OriginalResultMock::NeedsReview
        }
    ));
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
}
#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn write_handoff_lost_commit_ack_queries_original_without_publishing_or_repeating() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let handoff = stage_registration(&runtime, &mut tx, &u)
        .await
        .into_handoff_mock()
        .unwrap();
    tx.commit().await.unwrap();
    // Actual DB commit succeeded, but simulate loss of its acknowledgement.
    let result = handoff.complete_mock(&mut runtime, Err::<(), ()>(())).await;
    let recovery = match result {
        SubscriptionWriteCompletionMock::ReadOnlyRecovery(recovery) => recovery,
        _ => panic!("UNCERTAIN_ACK_PUBLISHED_LIVE_STATE"),
    };
    assert!(matches!(
        &recovery.original_results[0],
        OriginalLookupAttemptMock::Read {
            result: OriginalResultMock::Recorded { .. }
        }
    ));
    assert!(!recovery.continue_writes);
    assert!(!recovery.business_reexecuted);
    assert!(!recovery.runtime_refreshed);
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
    let mut connection = p.acquire().await.unwrap();
    let saved = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &u)
        .await
        .unwrap();
    assert_eq!(saved.ledger_mock().list_mock(&u).unwrap().len(), 1);
    assert_eq!(saved.business_records_mock().len(), 1);
}

fn query_process(user: &str, query: serde_json::Value) -> std::process::Output {
    query_process_as(user, query, false, None)
}
fn query_process_as(
    user: &str,
    query: serde_json::Value,
    user_view: bool,
    url: Option<&str>,
) -> std::process::Output {
    let exe = std::env::var(if user_view {
        "SAFEFLOW_SUBSCRIPTION_USER_VIEW_EXE"
    } else {
        "SAFEFLOW_SUBSCRIPTION_RUNTIME_QUERY_EXE"
    })
    .expect("BUILT_READ_ONLY_QUERY_EXE_REQUIRED");
    let dir = std::path::PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap())
        .join("fictional-query-inputs");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!(
        "query-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&file, serde_json::to_vec(&query).unwrap()).unwrap();
    let mut command = std::process::Command::new(exe);
    command.arg("--run-fictional").arg(user).arg(&file);
    if let Some(url) = url {
        command.env("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL", url);
    }
    let output = command.output();
    std::fs::remove_file(file).unwrap();
    output.unwrap()
}

fn query_ok(user: &str, query: serde_json::Value) -> serde_json::Value {
    let output = query_process(user, query);
    assert!(output.status.success(), "{:?}", output.stderr);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["database_written"], false);
    assert_eq!(result["business_reexecuted"], false);
    assert_eq!(result["notification_sent"], false);
    result
}

async fn persisted_query_state(p: &PgPool, user: &str) -> serde_json::Value {
    sqlx::query_scalar("SELECT jsonb_build_object('version',version,'journal',journal) FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
        .bind(user).fetch_one(p).await.unwrap()
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn readonly_cli_reads_registration_settings_and_cancellation_without_restart() {
    let p = pool().await;
    let u = owner();
    let mut runtime = prepared(&p, &u).await;
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let binding = runtime
        .prepare_intent_mock(
            ctx(&u, "query-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    runtime
        .commit_intent_mock(
            &binding.draft_id,
            &binding.context,
            NOW,
            &mut Gate { calls: 0 },
        )
        .await
        .unwrap();
    let before = persisted_query_state(&p, &u).await;
    let mut connection = p.acquire().await.unwrap();
    let saved = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &u)
        .await
        .unwrap();
    let mut registration = None;
    for record in saved.business_records_mock() {
        let record = serde_json::to_value(record).unwrap();
        let request = serde_json::json!({"command_kind":record["original_command"]["kind"],
            "original_binding":record["original_binding"]});
        let result = query_ok(&u, serde_json::json!({"kind":"Original","request":request}));
        assert_eq!(result["result"]["state"], "RECORDED");
        assert_eq!(
            result["result"]["original_receipt"],
            record["original_command"]["receipt"]
        );
        if record["original_command"]["kind"] == "Register" {
            registration = Some(request);
        }
    }
    assert_eq!(saved.business_records_mock().len(), 3);
    let request = registration.unwrap();
    let other = owner();
    create(&p, &other).await;
    let foreign = query_process(
        &other,
        serde_json::json!({"kind":"Original","request":request}),
    );
    assert!(!foreign.status.success());
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("OriginalRequestConflict"));
    let mut changed = request.clone();
    changed["original_binding"]["context"]["plan_version"] = serde_json::json!(999);
    assert!(
        !query_process(&u, serde_json::json!({"kind":"Original","request":changed}))
            .status
            .success()
    );
    assert!(!query_process(
        "fictional-missing-query-owner",
        serde_json::json!({"kind":"Original","request":request})
    )
    .status
    .success());
    assert_eq!(before, persisted_query_state(&p, &u).await);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn readonly_cli_pagination_resumes_after_exit_and_append_without_missing_or_cross_owner() {
    let p = pool().await;
    let u = owner();
    let mut runtime = prepared(&p, &u).await;
    let first = query_ok(
        &u,
        serde_json::json!({"kind":"BusinessPage","cursor":null,"limit":1}),
    );
    assert_eq!(first["result"]["records"].as_array().unwrap().len(), 1);
    let cursor = first["result"]["next_cursor"].clone();
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let binding = runtime
        .prepare_intent_mock(
            ctx(&u, "page-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    runtime
        .commit_intent_mock(
            &binding.draft_id,
            &binding.context,
            NOW,
            &mut Gate { calls: 0 },
        )
        .await
        .unwrap();
    let before = persisted_query_state(&p, &u).await;
    let second = query_ok(
        &u,
        serde_json::json!({"kind":"BusinessPage","cursor":cursor,"limit":10}),
    );
    assert_ne!(first["pid"], second["pid"]);
    assert_eq!(second["result"]["has_more"], false);
    let mut joined = first["result"]["records"].as_array().unwrap().clone();
    joined.extend(second["result"]["records"].as_array().unwrap().clone());
    let full = query_ok(
        &u,
        serde_json::json!({"kind":"BusinessPage","cursor":null,"limit":10}),
    );
    assert_eq!(serde_json::json!(joined), full["result"]["records"]);
    assert_eq!(joined.len(), 3);
    // An unacknowledged page is deliberately replayable. Saving the returned
    // backend cursor belongs to the receiver; this is not exactly-once delivery.
    let repeat = query_ok(
        &u,
        serde_json::json!({"kind":"BusinessPage","cursor":cursor,"limit":10}),
    );
    assert_eq!(repeat["result"], second["result"]);
    let other = owner();
    create(&p, &other).await;
    assert!(!query_process(
        &other,
        serde_json::json!({"kind":"BusinessPage","cursor":cursor,"limit":10})
    )
    .status
    .success());
    let mut damaged = cursor;
    damaged["exported_prefix"][0]["source_command_position"] = serde_json::json!(999);
    assert!(!query_process(
        &u,
        serde_json::json!({"kind":"BusinessPage","cursor":damaged,"limit":10})
    )
    .status
    .success());
    assert_eq!(before, persisted_query_state(&p, &u).await);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn readonly_cli_recovers_after_writer_exits_without_publishing_commit_reply() {
    let p = pool().await;
    let u = owner();
    let writer = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "runtime_child", "--nocapture"])
        .env("SAFEFLOW_RUNTIME_PHASE", "query-lost-write")
        .env("SAFEFLOW_RUNTIME_OWNER", &u)
        .output()
        .unwrap();
    assert_eq!(writer.status.code(), Some(23));
    let text = String::from_utf8(writer.stdout).unwrap();
    let line = text
        .lines()
        .find_map(|s| s.split_once("LOST_REPLY_REQUEST:").map(|(_, v)| v))
        .unwrap();
    let evidence: serde_json::Value = serde_json::from_str(line).unwrap();
    let before = persisted_query_state(&p, &u).await;
    let query = serde_json::json!({"kind":"Original","request":evidence["request"]});
    let read = query_ok(&u, query.clone());
    assert_ne!(read["pid"], evidence["pid"]);
    assert_eq!(read["result"]["state"], "RECORDED");
    let again = query_ok(&u, query);
    assert_eq!(read["result"], again["result"]);
    let full = query_ok(
        &u,
        serde_json::json!({"kind":"BusinessPage","cursor":null,"limit":10}),
    );
    assert_eq!(full["result"]["records"].as_array().unwrap().len(), 1);
    assert_eq!(
        read["result"]["original_receipt"],
        full["result"]["records"][0]["original_command"]["receipt"]
    );
    assert_eq!(before, persisted_query_state(&p, &u).await);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn user_view_cli_with_select_only_role_rejects_writes_and_reports_revoked_read_permission() {
    use std::str::FromStr;
    let p = pool().await;
    let u = owner();
    let runtime = prepared(&p, &u).await;
    let before = persisted_query_state(&p, &u).await;
    let mut connection = p.acquire().await.unwrap();
    let saved = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &u)
        .await
        .unwrap();
    let record = serde_json::to_value(&saved.business_records_mock()[0]).unwrap();
    let request = serde_json::json!({"command_kind":record["original_command"]["kind"],"original_binding":record["original_binding"]});
    // Identifiers and credentials are generated locally from test-only ASCII IDs.
    // This test is run in the disposable dedicated database container.
    let role = format!("reader_{}", u.replace('-', "_"));
    let password = format!("fictional_{}", owner().replace('-', "_"));
    sqlx::query(&format!("CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD '{password}'"))
        .execute(&p).await.unwrap();
    sqlx::query(&format!(
        "GRANT USAGE ON SCHEMA safeflow_subscription_mock_v1 TO {role}"
    ))
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(&format!(
        "GRANT SELECT ON safeflow_subscription_mock_v1.runtime_recovery TO {role}"
    ))
    .execute(&p)
    .await
    .unwrap();
    let options = sqlx::postgres::PgConnectOptions::from_str(
        &std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL").unwrap(),
    )
    .unwrap();
    let url = format!(
        "postgres://{role}:{password}@{}:{}/safeflow_subscription_mock_test",
        options.get_host(),
        options.get_port()
    );
    let reader = PgPoolOptions::new()
        .connect_with(options.username(&role).password(&password))
        .await
        .unwrap();
    for statement in [
        "UPDATE safeflow_subscription_mock_v1.runtime_recovery SET version=version WHERE false",
        "DELETE FROM safeflow_subscription_mock_v1.runtime_recovery WHERE false",
        "INSERT INTO safeflow_subscription_mock_v1.runtime_recovery SELECT * FROM safeflow_subscription_mock_v1.runtime_recovery WHERE false",
    ] {
        let error=sqlx::query(statement).execute(&reader).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(),Some("42501"));
    }
    let output = query_process_as(&u, request.clone(), true, Some(&url));
    assert!(output.status.success());
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["status"], "REGISTRATION_RECORDED");
    assert_eq!(view.as_object().unwrap().len(), 3);
    assert!(output.stderr.is_empty());
    let backend = query_process_as(
        &u,
        serde_json::json!({"kind":"Original","request":request}),
        false,
        Some(&url),
    );
    assert!(backend.status.success());
    sqlx::query(&format!(
        "REVOKE SELECT ON safeflow_subscription_mock_v1.runtime_recovery FROM {role}"
    ))
    .execute(&p)
    .await
    .unwrap();
    let denied = query_process_as(&u, request, true, Some(&url));
    assert!(!denied.status.success());
    let view: serde_json::Value = serde_json::from_slice(&denied.stdout).unwrap();
    assert_eq!(view["status"], "UNAVAILABLE");
    assert_eq!(view.as_object().unwrap().len(), 3);
    for secret in [&role, &password, &u] {
        assert!(!String::from_utf8_lossy(&denied.stdout).contains(secret));
    }
    assert!(denied.stderr.is_empty());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    assert_eq!(runtime.ledger_mock().list_mock(&u).unwrap().len(), 1);
    reader.close().await;
    sqlx::query(&format!(
        "REVOKE USAGE ON SCHEMA safeflow_subscription_mock_v1 FROM {role}"
    ))
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(&p)
        .await
        .unwrap();
}

fn current_view_ok(user: &str, query: serde_json::Value) -> serde_json::Value {
    let output = query_process_as(user, query, true, None);
    assert!(output.status.success(), "{:?}", output.stderr);
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for field in [
        "pid",
        "version",
        "original_binding",
        "original_receipt",
        "owner_user_id",
        "session_ref",
        "idempotency_key",
    ] {
        assert!(!view.to_string().contains(field));
    }
    view
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn current_business_queries_read_empty_list_cancel_history_and_revoke_after_restart() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let empty = current_view_ok(&u, serde_json::json!({"kind":"ListSubscriptions"}));
    assert!(empty["items"].as_array().unwrap().is_empty());
    register(&mut runtime, &u).await;
    let list = current_view_ok(&u, serde_json::json!({"kind":"ListSubscriptions"}));
    assert_eq!(list["items"][0]["current_state"], "ACTIVE");
    assert_eq!(list["items"][0]["service_data_state"], "AVAILABLE");
    let id = list["items"][0]["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let plan = current_view_ok(
        &u,
        serde_json::json!({"kind":"ReminderPlan","subscription_id":id}),
    );
    assert_eq!(plan["data_state"], "NOT_CONFIGURED");
    assert_eq!(plan["schedule"], serde_json::Value::Null);
    for (name, kind, count, state) in [
        ("current-cancel", MockIntentKind::Record, 1, "CANCELLING"),
        ("current-revoke", MockIntentKind::Revoke, 2, "ACTIVE"),
    ] {
        let binding = runtime
            .prepare_intent_mock(ctx(&u, name), &id, kind, NOW, EXPIRY)
            .await
            .unwrap();
        runtime
            .commit_intent_mock(
                &binding.draft_id,
                &binding.context,
                NOW,
                &mut Gate { calls: 0 },
            )
            .await
            .unwrap();
        let progress = current_view_ok(
            &u,
            serde_json::json!({"kind":"CancellationStatus","subscription_id":id}),
        );
        assert_eq!(progress["current_state"], state);
        assert_eq!(progress["history"].as_array().unwrap().len(), count);
        assert_eq!(progress["merchant_cancellation_confirmed"], false);
    }
    drop(runtime);
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let before = persisted_query_state(&p, &u).await;
    let progress = current_view_ok(
        &u,
        serde_json::json!({"kind":"CancellationStatus","subscription_id":id}),
    );
    assert_eq!(progress["history"][0]["kind"], "RECORD");
    assert_eq!(progress["history"][1]["kind"], "REVOKE");
    assert_eq!(progress["current_state"], "ACTIVE");
    let other = owner();
    create(&p, &other).await;
    for kind in ["CancellationStatus", "ReminderPlan"] {
        let denied = query_process_as(
            &other,
            serde_json::json!({"kind":kind,"subscription_id":id}),
            true,
            None,
        );
        assert!(!denied.status.success());
        assert!(!String::from_utf8_lossy(&denied.stdout).contains(&id));
    }
    let missing = query_process_as(
        "fictional-no-current-query-owner",
        serde_json::json!({"kind":"ListSubscriptions"}),
        true,
        None,
    );
    assert!(!missing.status.success());
    let missing: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["status"], "NEEDS_REVIEW");
    assert!(missing.get("items").is_none());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    assert_eq!(restored.ledger_mock().list_mock(&u).unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn current_reminder_queries_preserve_disabled_and_cancellation_unknown_cycles_after_restart()
{
    let p = pool().await;
    let u = owner();
    let mut runtime = prepared(&p, &u).await;
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let query = serde_json::json!({"kind":"ReminderPlan","subscription_id":id});
    let initial = current_view_ok(&u, query.clone());
    assert_eq!(initial["schedule"]["days_before"], 3);
    assert_eq!(initial["current_attempt"], "NOT_ATTEMPTED");
    let mut sink = Sink::new(DeliveryOutcome::Unknown);
    runtime.scan_mock(NOW, &mut sink).await.unwrap();
    assert_eq!(sink.calls, 1);
    let mut disabled = schedule();
    disabled.enabled = false;
    let binding = runtime
        .prepare_settings_mock(ctx(&u, "query-disable"), &id, disabled, NOW, EXPIRY)
        .await
        .unwrap();
    runtime
        .confirm_settings_mock(&binding, NOW, &mut Gate { calls: 0 })
        .await
        .unwrap();
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["schedule"]["enabled"], false);
    assert_eq!(view["current_attempt"], "NEEDS_REVIEW");
    let binding = runtime
        .prepare_intent_mock(
            ctx(&u, "query-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    runtime
        .commit_intent_mock(
            &binding.draft_id,
            &binding.context,
            NOW,
            &mut Gate { calls: 0 },
        )
        .await
        .unwrap();
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["active_kind"], "CANCELLATION_CHECK");
    assert_eq!(view["current_attempt"], "NOT_ATTEMPTED");
    assert_eq!(view["saved_cycle_outcomes"][0]["state"], "NEEDS_REVIEW");
    let mut sink = Sink::new(DeliveryOutcome::Unknown);
    runtime
        .scan_mock("2026-10-17T12:00:00+08:00", &mut sink)
        .await
        .unwrap();
    assert_eq!(sink.calls, 1);
    drop(runtime);
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let before = persisted_query_state(&p, &u).await;
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["current_attempt"], "NEEDS_REVIEW");
    assert_eq!(view["saved_cycle_outcomes"].as_array().unwrap().len(), 2);
    assert_eq!(view["notification_sent"], false);
    assert_eq!(view, current_view_ok(&u, query));
    assert_eq!(before, persisted_query_state(&p, &u).await);
    assert_eq!(
        restored.ledger_mock().list_mock(&u).unwrap()[0].status,
        LedgerStatus::Cancelling
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn current_reminder_queries_separate_failed_and_simulated_success_without_sending() {
    let p = pool().await;
    let u = owner();
    let mut runtime = prepared(&p, &u).await;
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let query = serde_json::json!({"kind":"ReminderPlan","subscription_id":id});
    let mut failure = Sink::new(DeliveryOutcome::DefiniteFailure);
    runtime.scan_mock(NOW, &mut failure).await.unwrap();
    assert_eq!(failure.calls, 1);
    assert_eq!(
        current_view_ok(&u, query.clone())["current_attempt"],
        "FAILED"
    );
    let mut success = Sink::new(DeliveryOutcome::SimulatedSuccess);
    runtime
        .scan_mock("2026-10-15T12:00:00+08:00", &mut success)
        .await
        .unwrap();
    assert_eq!(success.calls, 1);
    let before = persisted_query_state(&p, &u).await;
    let view = current_view_ok(&u, query);
    assert_eq!(view["current_attempt"], "SIMULATED_SUCCESS");
    assert_eq!(
        view["saved_cycle_outcomes"][0]["last_attempt_date"],
        "2026-10-15"
    );
    assert_eq!(view["notification_sent"], false);
    assert_eq!(before, persisted_query_state(&p, &u).await);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn registration_preview_reads_persisted_draft_and_rejects_replaced_or_restarted_confirmation()
{
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let s = sample(&u);
    let binding = runtime
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "preview-register"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    let query = serde_json::json!({"kind":"RegistrationPreview","draft_id":binding.draft_id,"context":binding.context,"now":NOW});
    let before = persisted_query_state(&p, &u).await;
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["action"], "REGISTER_SUBSCRIPTION");
    assert_eq!(
        view["charges"].as_array().unwrap().len(),
        binding.candidate.actual_charges.len()
    );
    assert_eq!(view["service_display_name"], serde_json::Value::Null);
    assert!(runtime.ledger_mock().list_mock(&u).unwrap().is_empty());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    let mut operation = binding.context.clone();
    operation.plan_version += 1;
    let next = runtime
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            operation,
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    assert!(!query_process_as(&u, query, true, None).status.success());
    let next_query = serde_json::json!({"kind":"RegistrationPreview","draft_id":next.draft_id,"context":next.context,"now":NOW});
    current_view_ok(&u, next_query.clone());
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    let before = persisted_query_state(&p, &u).await;
    assert!(!query_process_as(&u, next_query, true, None)
        .status
        .success());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    assert!(restored.ledger_mock().list_mock(&u).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn settings_preview_reads_persisted_difference_without_changing_settings_or_accepting_old_plan(
) {
    let p = pool().await;
    let u = owner();
    let mut runtime = prepared(&p, &u).await;
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut changed = schedule();
    changed.enabled = false;
    changed.days_before = 0;
    let first = runtime
        .prepare_settings_mock(ctx(&u, "preview-edit"), &id, changed.clone(), NOW, EXPIRY)
        .await
        .unwrap();
    let query = serde_json::json!({"kind":"SettingsPreview","context":first.context,"now":NOW});
    let before = persisted_query_state(&p, &u).await;
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["before"]["enabled"], true);
    assert_eq!(view["after"]["enabled"], false);
    assert_eq!(
        view["changed_fields"],
        serde_json::json!(["enabled", "days_before"])
    );
    assert_eq!(
        runtime
            .reminders_mock()
            .current_schedule_mock(runtime.ledger_mock(), &u, &id)
            .unwrap(),
        &schedule()
    );
    assert_eq!(before, persisted_query_state(&p, &u).await);
    let mut operation = first.context.clone();
    operation.plan_version += 1;
    changed.days_before = 1;
    let next = runtime
        .prepare_settings_mock(operation, &id, changed, NOW, EXPIRY)
        .await
        .unwrap();
    assert!(!query_process_as(&u, query, true, None).status.success());
    let mut gate = Gate { calls: 0 };
    assert!(runtime
        .confirm_settings_mock(&first, NOW, &mut gate)
        .await
        .is_err());
    assert_eq!(gate.calls, 0);
    let next_query = serde_json::json!({"kind":"SettingsPreview","context":next.context,"now":NOW});
    assert_eq!(current_view_ok(&u, next_query)["after"]["days_before"], 1);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn intent_preview_reads_local_effect_without_execution_and_old_preview_fails_after_commit() {
    let p = pool().await;
    let u = owner();
    let mut runtime = prepared(&p, &u).await;
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let binding = runtime
        .prepare_intent_mock(
            ctx(&u, "preview-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    let query = serde_json::json!({"kind":"IntentPreview","draft_id":binding.draft_id,"context":binding.context,"now":NOW});
    let before = persisted_query_state(&p, &u).await;
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["before"], "ACTIVE");
    assert_eq!(view["after"], "CANCELLING");
    assert!(view["message"]
        .as_str()
        .unwrap()
        .contains("不替你取消商户订阅"));
    assert!(runtime
        .ledger_mock()
        .intent_events_mock(&u)
        .unwrap()
        .is_empty());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    runtime
        .commit_intent_mock(
            &binding.draft_id,
            &binding.context,
            NOW,
            &mut Gate { calls: 0 },
        )
        .await
        .unwrap();
    assert!(!query_process_as(&u, query, true, None).status.success());
    let revoke = runtime
        .prepare_intent_mock(
            ctx(&u, "preview-revoke"),
            &id,
            MockIntentKind::Revoke,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    let view = current_view_ok(
        &u,
        serde_json::json!({"kind":"IntentPreview","draft_id":revoke.draft_id,"context":revoke.context,"now":NOW}),
    );
    assert_eq!(view["before"], "CANCELLING");
    assert_eq!(view["after"], "ACTIVE");
    assert!(view["message"]
        .as_str()
        .unwrap()
        .contains("不表示恢复商户服务"));
    assert_eq!(
        runtime.ledger_mock().list_mock(&u).unwrap()[0].status,
        LedgerStatus::Cancelling
    );
    assert_eq!(
        runtime.ledger_mock().intent_events_mock(&u).unwrap().len(),
        1
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn discovery_cli_accepts_consumption_and_current_ledger_without_writes_or_foreign_scope() {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let s = sample(&u);
    let query = serde_json::json!({"kind":"IdentifySubscriptions","context":s.trusted_context_fixture,"input":s.snapshot});
    let before = persisted_query_state(&p, &u).await;
    let view = current_view_ok(&u, query.clone());
    assert_eq!(view["matching_executed"], true);
    assert_eq!(view["groups"][0]["disposition"], "NOT_IN_LEDGER");
    assert_eq!(before, persisted_query_state(&p, &u).await);
    register(&mut runtime, &u).await;
    assert_eq!(
        current_view_ok(&u, query.clone())["groups"][0]["disposition"],
        "ALREADY_MANAGED"
    );
    let before = persisted_query_state(&p, &u).await;
    let mut incomplete = query.clone();
    incomplete["input"]["transactions_read"]["state"] = serde_json::json!("UNAVAILABLE");
    incomplete["input"]["transactions_read"]["is_complete"] = serde_json::json!(false);
    assert_eq!(current_view_ok(&u, incomplete)["matching_executed"], false);
    let mut foreign = query;
    foreign["context"]["user_id"] = serde_json::json!("other");
    assert!(!query_process_as(&u, foreign, true, None).status.success());
    assert_eq!(before, persisted_query_state(&p, &u).await);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn date_suggestion_cli_reads_candidate_and_registered_dates_but_does_not_guess_expired_dates()
{
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let s = ready_date_sample(&u);
    let query = serde_json::json!({"kind":"SuggestReminderDate","context":s.trusted_context_fixture,"input":s.snapshot,"candidate_index":0,"subscription_id":null,"now":NOW});
    let before = persisted_query_state(&p, &u).await;
    let candidate = current_view_ok(&u, query.clone());
    assert_eq!(candidate["state"], "READY");
    assert_eq!(candidate["prediction"]["expected_date"], "2026-10-17");
    assert_eq!(before, persisted_query_state(&p, &u).await);
    register_ready_date(&mut runtime, &u).await;
    let id = runtime.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut existing = query;
    existing["candidate_index"] = serde_json::Value::Null;
    existing["subscription_id"] = serde_json::json!(id);
    assert_eq!(
        current_view_ok(&u, existing.clone())["prediction"],
        candidate["prediction"]
    );
    let before = persisted_query_state(&p, &u).await;
    existing["now"] = serde_json::json!("2026-10-20T12:00:00+08:00");
    let expired = current_view_ok(&u, existing);
    assert_eq!(expired["state"], "NEEDS_INPUT");
    assert_eq!(expired["prediction"], serde_json::Value::Null);
    assert_eq!(before, persisted_query_state(&p, &u).await);
    assert!(runtime
        .reminders_mock()
        .current_schedule_mock(runtime.ledger_mock(), &u, &id)
        .is_err());
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn registration_to_reminder_preparation_keeps_registration_and_requires_separate_confirmation(
) {
    let p = pool().await;
    let u = owner();
    let mut runtime = create(&p, &u).await;
    let registration = register_ready_date(&mut runtime, &u).await;
    let s = ready_date_sample(&u);
    let mut unavailable = s.snapshot.clone();
    unavailable.transactions_read.state = ReadKind::Unavailable;
    unavailable.transactions_read.is_complete = false;
    let before = persisted_query_state(&p, &u).await;
    let missing = runtime
        .prepare_reminder_after_registration_mock(
            &s.trusted_context_fixture,
            &unavailable,
            &registration,
            ctx(&u, "auto-reminder"),
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    assert_eq!(missing.state, "NEEDS_INPUT");
    assert!(missing.confirmation_binding.is_none());
    assert_eq!(runtime.ledger_mock().list_mock(&u).unwrap().len(), 1);
    assert_eq!(before, persisted_query_state(&p, &u).await);
    let ready = runtime
        .prepare_reminder_after_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            &registration,
            ctx(&u, "auto-reminder"),
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    assert_eq!(ready.state, "AWAITING_CONFIRMATION");
    assert!(!ready.settings_saved);
    assert_eq!(ready.preview.as_ref().unwrap().after.days_before, 3);
    let safe = serde_json::to_value(&ready).unwrap();
    assert!(safe.get("confirmation_binding").is_none());
    assert!(!safe.to_string().contains("session_ref"));
    assert!(runtime
        .reminders_mock()
        .current_schedule_mock(runtime.ledger_mock(), &u, &ready.subscription_id)
        .is_err());
    let binding = ready.confirmation_binding.unwrap();
    struct Deny;
    impl MockScheduleGate for Deny {
        fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
            MockGateOutcome::Denied
        }
    }
    let before = persisted_query_state(&p, &u).await;
    assert!(runtime
        .confirm_settings_mock(&binding, NOW, &mut Deny)
        .await
        .is_err());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    let mut gate = Gate { calls: 0 };
    runtime
        .confirm_settings_mock(&binding, NOW, &mut gate)
        .await
        .unwrap();
    assert_eq!(gate.calls, 1);
    let before = persisted_query_state(&p, &u).await;
    assert!(runtime
        .prepare_reminder_after_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            &registration,
            ctx(&u, "second-initialization"),
            NOW,
            EXPIRY
        )
        .await
        .is_err());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        restored
            .reminders_mock()
            .current_schedule_mock(restored.ledger_mock(), &u, &ready.subscription_id)
            .unwrap(),
        &binding.schedule
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn pending_list_cli_is_readonly_and_preserves_expired_and_restarted_drafts() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    let s = ready_date_sample(&u);
    r.prepare_registration_mock(
        &s.trusted_context_fixture,
        &s.snapshot,
        ctx(&u, "pending"),
        0,
        EXPIRY,
    )
    .await
    .unwrap();
    let before = persisted_query_state(&p, &u).await;
    for (now, status) in [(NOW, "READY"), (EXPIRY, "EXPIRED")] {
        let result = current_view_ok(
            &u,
            serde_json::json!({"kind":"PendingConfirmations","now":now}),
        );
        assert_eq!(result["items"][0]["state"], status);
    }
    assert_eq!(before, persisted_query_state(&p, &u).await);
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        restored.pending_confirmations_mock(NOW).unwrap().items[0].state,
        "INVALIDATED"
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn abandonment_rolls_back_then_persists_and_rejects_old_confirmation_without_gate() {
    let p = pool().await;
    let u = owner();
    let mut r = create(&p, &u).await;
    let s = ready_date_sample(&u);
    let b = r
        .prepare_registration_mock(
            &s.trusted_context_fixture,
            &s.snapshot,
            ctx(&u, "abandon"),
            0,
            EXPIRY,
        )
        .await
        .unwrap();
    let position = r.pending_confirmations_mock(NOW).unwrap().items[0].source_position;
    let before = persisted_query_state(&p, &u).await;
    let mut tx = p.begin().await.unwrap();
    let original = b.context.clone();
    let pending = r
        .stage_on_connection_mock(&mut tx, move |candidate| {
            Box::pin(async move {
                candidate
                    .abandon_pending_mock(position, &original, NOW)
                    .await
            })
        })
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(pending.publish_after_commit_mock(&mut r).await.is_err());
    assert_eq!(before, persisted_query_state(&p, &u).await);
    assert!(r
        .registration_preview_mock(&b.draft_id, &b.context, NOW)
        .is_ok());
    let mut foreign = b.context.clone();
    foreign.user_id = "different-owner".into();
    assert!(r
        .abandon_pending_mock(position, &foreign, NOW)
        .await
        .is_err());
    let receipt = r
        .abandon_pending_mock(position, &b.context, NOW)
        .await
        .unwrap();
    assert!(!receipt.business_changed);
    let saved = persisted_query_state(&p, &u).await;
    assert_eq!(
        r.abandon_pending_mock(position, &b.context, EXPIRY)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(saved, persisted_query_state(&p, &u).await);
    let mut gate = Gate { calls: 0 };
    assert_eq!(
        r.register_mock(&b.draft_id, &b.context, NOW, &mut gate)
            .await
            .unwrap_err(),
        RuntimeRecoveryIssue::OperationAbandoned
    );
    assert_eq!(gate.calls, 0);
    assert!(r.ledger_mock().list_mock(&u).unwrap().is_empty());
    let mut restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        restored.pending_confirmations_mock(NOW).unwrap().items[0].state,
        "ABANDONED"
    );
    assert!(restored
        .register_mock(&b.draft_id, &b.context, NOW, &mut gate)
        .await
        .is_err());
    assert_eq!(gate.calls, 0);
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn abandoning_settings_cancel_and_revoke_preserves_saved_business() {
    let p = pool().await;
    let u = owner();
    let mut r = prepared(&p, &u).await;
    let id = r.ledger_mock().list_mock(&u).unwrap()[0]
        .subscription_id
        .clone();
    let mut disabled = schedule();
    disabled.enabled = false;
    let settings = r
        .prepare_settings_mock(ctx(&u, "disable-draft"), &id, disabled, NOW, EXPIRY)
        .await
        .unwrap();
    let cancel = r
        .prepare_intent_mock(
            ctx(&u, "cancel-draft"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    for (action, context) in [
        ("SETTINGS", &settings.context),
        ("CANCEL_INTENT", &cancel.context),
    ] {
        let list = r.pending_confirmations_mock(NOW).unwrap();
        let pos = list
            .items
            .iter()
            .find(|item| item.action == action && item.state == "READY")
            .unwrap()
            .source_position;
        r.abandon_pending_mock(pos, context, NOW).await.unwrap();
    }
    assert_eq!(
        r.reminders_mock()
            .current_schedule_mock(r.ledger_mock(), &u, &id)
            .unwrap(),
        &schedule()
    );
    let mut gate = Gate { calls: 0 };
    assert!(r
        .confirm_settings_mock(&settings, NOW, &mut gate)
        .await
        .is_err());
    assert!(r
        .commit_intent_mock(&cancel.draft_id, &cancel.context, NOW, &mut gate)
        .await
        .is_err());
    assert_eq!(gate.calls, 0);
    let actual = r
        .prepare_intent_mock(
            ctx(&u, "actual-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    let receipt = r
        .commit_intent_mock(&actual.draft_id, &actual.context, NOW, &mut gate)
        .await
        .unwrap();
    let revoke = r
        .prepare_intent_mock(
            ctx(&u, "revoke-draft"),
            &id,
            MockIntentKind::Revoke,
            NOW,
            EXPIRY,
        )
        .await
        .unwrap();
    let pos = r
        .pending_confirmations_mock(NOW)
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.action == "REVOKE_INTENT")
        .unwrap()
        .source_position;
    r.abandon_pending_mock(pos, &revoke.context, NOW)
        .await
        .unwrap();
    assert!(r
        .commit_intent_mock(&revoke.draft_id, &revoke.context, NOW, &mut gate)
        .await
        .is_err());
    assert_eq!(gate.calls, 1);
    let list = r.pending_confirmations_mock(NOW).unwrap();
    assert!(list.items.iter().all(|i| i.state == "ABANDONED"));
    let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(
            restored
                .ledger_mock()
                .intent_result_mock(&u, "actual-cancel")
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(receipt).unwrap()
    );
}

#[tokio::test]
#[ignore = "requires dedicated local PostgreSQL test database"]
async fn reselection_invalidates_old_plan_and_preserves_completed_original_after_restart() {
    let p = pool().await;
    for completed in [false, true] {
        let u = owner();
        let mut r = create(&p, &u).await;
        let mut s = ready_date_sample(&u);
        let mut other = s.snapshot.transactions.clone();
        for t in &mut other {
            t.transaction_id.push_str("-b");
            t.mock_service_ref = Some("fictional-hannah-video-service-b".into());
        }
        s.snapshot.transactions.extend(other);
        let a = r
            .prepare_registration_mock(
                &s.trusted_context_fixture,
                &s.snapshot,
                ctx(&u, "a"),
                0,
                EXPIRY,
            )
            .await
            .unwrap();
        let original = if completed {
            Some(
                r.register_mock(&a.draft_id, &a.context, NOW, &mut Gate { calls: 0 })
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let next = if completed {
            ctx(&u, "b")
        } else {
            let mut c = a.context.clone();
            c.plan_version += 1;
            c
        };
        let changed = r
            .reselect_registration_mock(
                &a,
                &s.trusted_context_fixture,
                &s.snapshot,
                1,
                next,
                NOW,
                EXPIRY,
            )
            .await
            .unwrap();
        assert_eq!(
            changed.mode,
            if completed {
                "NEW_REGISTRATION_KEEP_ORIGINAL"
            } else {
                "REPLACE_UNCOMMITTED"
            }
        );
        assert!(serde_json::to_value(&changed)
            .unwrap()
            .get("confirmation_binding")
            .is_none());
        assert_eq!(
            r.ledger_mock().list_mock(&u).unwrap().len(),
            usize::from(completed)
        );
        let mut gate = Gate { calls: 0 };
        if !completed {
            assert!(r
                .register_mock(&a.draft_id, &a.context, NOW, &mut gate)
                .await
                .is_err());
            let pending = r.pending_confirmations_mock(NOW).unwrap();
            assert_eq!(pending.items[0].state, "SUPERSEDED");
            assert_eq!(pending.items[1].state, "READY");
        }
        assert_eq!(gate.calls, 0);
        let b = changed.confirmation_binding;
        assert_ne!(a.candidate.mock_service_ref, b.candidate.mock_service_ref);
        r.register_mock(&b.draft_id, &b.context, NOW, &mut gate)
            .await
            .unwrap();
        assert_eq!(gate.calls, 1);
        assert!(r.pending_confirmations_mock(NOW).unwrap().items.is_empty());
        let restored = SubscriptionRuntimeMock::start_mock(p.clone(), &u)
            .await
            .unwrap();
        assert_eq!(
            restored.ledger_mock().list_mock(&u).unwrap().len(),
            if completed { 2 } else { 1 }
        );
        if let Some(receipt) = original {
            assert_eq!(
                serde_json::to_value(
                    restored
                        .ledger_mock()
                        .confirmation_result_mock(&u, "a")
                        .unwrap()
                )
                .unwrap(),
                serde_json::to_value(receipt).unwrap()
            );
        }
    }
}
