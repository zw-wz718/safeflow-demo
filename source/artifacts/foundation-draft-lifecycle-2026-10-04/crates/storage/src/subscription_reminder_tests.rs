//! Own-PG boundary tests; original member initial and original settings calls.
#[path="subscription_reminder/calendar47_tests.rs"]
mod calendar47_tests;
#[path="subscription_reminder/monthly47_repair1_tests.rs"]
mod monthly47_repair1_tests;
use super::*;
use safeflow_contracts::{ContractFuture, SecurityError};
use safeflow_runtime::SessionResolver;
use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::LedgerFixture};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::sync::Mutex;
use std::{
    collections::HashMap,
    sync::{atomic::AtomicUsize, RwLock},
};
#[path = "subscription_reminder_pg_fault.rs"]
mod fault;
const NOW: &str = "2026-10-14T12:00:00+08:00";
#[derive(Default)]
pub(super) struct Probe {
    pub clock: Mutex<Option<String>>,
    point: Mutex<
        Option<(
            &'static str,
            Arc<tokio::sync::Notify>,
            Arc<tokio::sync::Notify>,
        )>,
    >,
}
impl Probe {
    pub async fn at(&self, name: &str) {
        let point = self.point.lock().unwrap().clone();
        if let Some((phase, entered, release)) = point {
            if phase == name {
                entered.notify_one();
                release.notified().await;
            }
        }
    }
}
#[derive(Default)]
struct Sessions(RwLock<HashMap<SessionRef, SessionBinding>>);
impl Sessions {
    fn bind(&self, s: &SessionRef, user: &str, version: u64) {
        self.0.write().unwrap().insert(
            s.clone(),
            SessionBinding {
                user_id: UserId::parse(user).unwrap(),
                binding_version: version,
            },
        );
    }
    fn revoke(&self, s: &SessionRef) {
        self.0.write().unwrap().remove(s);
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, s: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(s)?.user_id)
    }
    fn resolve_binding(&self, s: &SessionRef) -> Result<SessionBinding, SecurityError> {
        self.0.read().unwrap().get(s).cloned().ok_or(SecurityError {
            reason_code: safeflow_contracts::ReasonCode::parse("SESSION_MISSING").unwrap(),
            message: "missing".into(),
        })
    }
}
#[derive(Default)]
struct Bank {
    calls: AtomicUsize,
    pause_at: AtomicUsize,
    denied: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl BankGateway for Bank {
    fn get_account_balance<'a>(
        &'a self,
        u: &'a UserId,
        a: &'a ResourceId,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::GetAccountBalanceOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.pause_at.load(Ordering::SeqCst) == call {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let sample: Value = serde_json::from_str(include_str!(env!(
                "SAFEFLOW_SUBSCRIPTION_DISCOVERY_FIXTURE"
            )))
            .unwrap();
            if self.denied.load(Ordering::SeqCst)
                || !sample["sample"]["snapshot"]["accounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["user_id"] == u.as_str() && v["account_id"] == a.as_str())
            {
                return Err(safeflow_bank_gateway::BankError::ResourceNotOwned);
            }
            let bank: Value =
                serde_json::from_str(include_str!("../../mock-bank/src/fixtures.json")).unwrap();
            serde_json::from_value(
                bank["accounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["balance"]["account"]["account_id"] == a.as_str())
                    .unwrap()["balance"]
                    .clone(),
            )
            .map_err(|_| safeflow_bank_gateway::BankError::Unavailable)
        })
    }
    fn find_payees<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a str,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::FindPayeesOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async { Err(safeflow_bank_gateway::BankError::Unavailable) })
    }
}
struct Gate;
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
        MockGateOutcome::Confirmed
    }
}
impl MockIntentGateVerifier for Gate {
    fn verify_and_consume_mock_intent(
        &mut self,
        _: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        MockGateOutcome::Confirmed
    }
}
fn operation(id: &str) -> MockOperationContext {
    MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "fictional-session".into(),
        task_id: "fictional-task".into(),
        node_id: "fictional-node".into(),
        operation_id: id.into(),
        idempotency_key: format!("fictional-key-{id}"),
        plan_version: 1,
    }
}
fn schedule(daily: bool) -> Schedule {
    // Exact schedules already exercised by member shared_ledger_write_probe and
    // shared_ledger_reminder_cases; no new business JSON or date expectations.
    Schedule {
        cycle: if daily {
            safeflow_subscription_evidence::recognition::Cycle::Day
        } else {
            safeflow_subscription_evidence::recognition::Cycle::CalendarMonth
        },
        anchor_date: if daily { "2026-10-13" } else { "2026-09-17" }
            .parse()
            .unwrap(),
        period_index: 1,
        expected_date: if daily { "2026-10-14" } else { "2026-10-17" }
            .parse()
            .unwrap(),
        enabled: true,
        days_before: if daily { 0 } else { 3 },
    }
}
fn config() -> SubscriptionReminderConfig {
    SubscriptionReminderConfig {
        owner: UserId::parse("user_demo").unwrap(),
        accounts: vec![ResourceId::parse("account_001").unwrap()],
        max_messages: 16,
        max_ledger_entries: 64,
        scan_timeout: Duration::from_secs(8),
        cycles: 1,
        interval: Duration::from_secs(1),
    }
}
struct Db {
    schema: String,
    service: SubscriptionService,
    admin: PgPool,
    sessions: Arc<Sessions>,
    session: SessionRef,
    bank: Arc<Bank>,
}
impl Db {
    async fn empty() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let schema = format!("sf_reminder_test_{}", uuid::Uuid::new_v4().simple());
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let sessions = Arc::new(Sessions::default());
        let session = SessionRef::parse("reminder_current_session").unwrap();
        sessions.bind(&session, "user_demo", 1);
        let service = SubscriptionService::for_trusted_backend(repo, sessions.clone());
        Self {
            schema,
            service,
            admin,
            sessions,
            session,
            bank: Arc::new(Bank::default()),
        }
    }
    async fn new() -> Self {
        let db = Self::empty().await;
        db.initialize().await;
        db.settings(false, true).await;
        db
    }
    async fn initialize(&self) {
        let raw: Value =
            serde_json::from_str(include_str!(env!("SAFEFLOW_SUBSCRIPTION_LEDGER_FIXTURE")))
                .unwrap();
        let initial: LedgerFixture = serde_json::from_value(
            raw["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["case_id"] == "already-active")
                .unwrap()["ledger"]
                .clone(),
        )
        .unwrap();
        let store = self.service.store().unwrap();
        let request = store
            .prepare_initialization_mock("user_demo", initial)
            .unwrap();
        let mut c = self.service.repository.pool.acquire().await.unwrap();
        let mut tx = c.begin().await.unwrap();
        let pending = store
            .initialize_on_connection_mock(&mut tx, "user_demo", &request)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut other = self
            .service
            .repository
            .subscription_read_pool
            .acquire()
            .await
            .unwrap();
        pending
            .read_committed_on_connection_mock(&mut other)
            .await
            .unwrap();
    }
    async fn save(&self, work: ProposedLedgerWorkMock) {
        let store = self.service.store().unwrap();
        let mut c = self.service.repository.pool.acquire().await.unwrap();
        let mut tx = c.begin().await.unwrap();
        let p = store.save_on_connection_mock(&mut tx, work).await.unwrap();
        tx.commit().await.unwrap();
        let mut other = self
            .service
            .repository
            .subscription_read_pool
            .acquire()
            .await
            .unwrap();
        p.read_committed_on_connection_mock(&mut other)
            .await
            .unwrap();
    }
    async fn work(&self) -> ProposedLedgerWorkMock {
        let mut c = self.admin.acquire().await.unwrap();
        self.service
            .store()
            .unwrap()
            .load_work_on_connection_mock(&mut c, "user_demo")
            .await
            .unwrap()
    }
    async fn settings(&self, daily: bool, enabled: bool) {
        let mut work = self.work().await;
        let mut s = schedule(daily);
        s.enabled = enabled;
        let id = uuid::Uuid::new_v4().to_string();
        let b = work
            .prepare_settings_mock(
                operation(&id),
                "fictional-subscription-1",
                s,
                NOW,
                "2026-10-14T12:05:00+08:00",
            )
            .await
            .unwrap();
        work.confirm_settings_mock(&b, NOW, &mut Gate)
            .await
            .unwrap();
        self.save(work).await;
    }
    async fn cancel(&self) {
        let mut work = self.work().await;
        let op = operation("reminder-cancel");
        let b = work
            .prepare_intent_mock(
                op.clone(),
                "fictional-subscription-1",
                MockIntentKind::Record,
                NOW,
                "2026-10-14T12:05:00+08:00",
            )
            .await
            .unwrap();
        work.confirm_intent_mock(&b.draft_id, &op, NOW, &mut Gate)
            .await
            .unwrap();
        self.save(work).await;
    }
    async fn runner(&self) -> SubscriptionReminderHost {
        let host = self.service.startup_for_host(&self.session).await.unwrap();
        self.for_host(&host).await
    }
    async fn for_host(&self, host: &SubscriptionHost) -> SubscriptionReminderHost {
        let r = self
            .service
            .reminder_host_for_backend(&self.session, host, self.bank.clone(), config())
            .await
            .unwrap();
        *r.probe.clock.lock().unwrap() = Some(NOW.into());
        r
    }
    async fn journal(&self) -> Value {
        sqlx::query_scalar(&format!(
            "SELECT journal FROM \"{}\".subscription_runtime_records WHERE owner_id='user_demo'",
            self.schema
        ))
        .fetch_one(&self.admin)
        .await
        .unwrap()
    }
    async fn snapshot(&self) -> SubscriptionReadSnapshotMock {
        let mut c = self.admin.acquire().await.unwrap();
        self.service.load(&mut c, "user_demo").await.unwrap()
    }
    async fn facts(&self) -> Value {
        let mut all = serde_json::Map::new();
        for table in [
            "subscription_runtime_records",
            "subscription_host_generations_v1",
            "subscription_public_bindings_v1",
            "subscription_preparations_v1",
            "tasks",
            "operations",
            "server_confirmations",
            "execution_permits",
            "public_execution_requests",
            "audit_events",
            "public_audit_outbox",
        ] {
            let rows:Value=sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(v ORDER BY v::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) v FROM \"{}\".{table} t) q",self.schema)).fetch_one(&self.admin).await.unwrap();
            all.insert(table.into(), rows);
        }
        Value::Object(all)
    }
    fn record(&self, name: &str, data: Value) {
        let dir = evidence("faults");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}.json", self.schema));
        assert!(!path.exists());
        std::fs::write(path, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
    }
    async fn sql(&self, q: &str) {
        self.admin
            .execute(format!("SET search_path TO \"{}\";{q}", self.schema).as_str())
            .await
            .unwrap();
    }
    async fn finish(self) {
        self.service.repository.close().await;
        self.admin
            .execute(format!("DROP SCHEMA \"{}\" CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
struct Sink {
    outcome: DeliveryOutcome,
    calls: usize,
}
impl Sink {
    fn new(outcome: DeliveryOutcome) -> Self {
        Self { outcome, calls: 0 }
    }
}
impl MockInAppSink for Sink {
    fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
        self.calls += 1;
        self.outcome
    }
}
fn pause(
    r: &SubscriptionReminderHost,
    phase: &'static str,
) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let e = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    *r.probe.point.lock().unwrap() = Some((phase, e.clone(), go.clone()));
    (e, go)
}
fn clock(r: &SubscriptionReminderHost, time: &str) {
    *r.probe.clock.lock().unwrap() = Some(time.into());
}
async fn entered(e: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(5), e.notified())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn reminder_host_real_pg_two_kinds_saved_dedup_and_no_public_write() {
    let db = Db::new().await;
    let r = db.runner().await;
    let stop = SubscriptionReminderStop::default();
    let first = r.scan_once(&stop).await.unwrap();
    assert!(first.reserved && first.simulated && first.result_saved);
    assert_eq!(first.simulated_count, 1);
    assert!(!first.notification_sent);
    let original = db.journal().await;
    let next = r.scan_once(&stop).await.unwrap();
    assert_eq!(next.due_count, 0);
    assert!(!next.reserved);
    assert_eq!(db.journal().await, original);
    db.cancel().await;
    clock(&r, "2026-10-17T12:00:00+08:00");
    let cancelled = r.scan_once(&stop).await.unwrap();
    assert_eq!(cancelled.simulated_count, 1);
    let plan = db
        .snapshot()
        .await
        .reminder_plan_mock("fictional-subscription-1")
        .unwrap();
    assert_eq!(plan.saved_cycle_outcomes.len(), 2);
    let saved = db.journal().await;
    assert_eq!(r.scan_once(&stop).await.unwrap().due_count, 0);
    assert_eq!(saved, db.journal().await);
    for table in ["tasks", "operations", "public_audit_outbox", "audit_events"] {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM \"{}\".{table}", db.schema))
                .fetch_one(&db.admin)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }
    assert_eq!(
        db.service.require_write_adoption(&db.session),
        Err(StorageError::DependencyUnavailable)
    );
    drop(r);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn reminder_host_failure_next_day_retry_and_unknown_never_resends() {
    for outcome in [DeliveryOutcome::DefiniteFailure, DeliveryOutcome::Unknown] {
        let db = Db::new().await;
        let r = db.runner().await;
        let stop = SubscriptionReminderStop::default();
        let mut sink = Sink::new(outcome);
        assert!(
            r.scan_with_sink(&stop, &mut sink)
                .await
                .unwrap()
                .result_saved
        );
        assert_eq!(sink.calls, 1);
        assert_eq!(
            r.scan_with_sink(&stop, &mut sink).await.unwrap().due_count,
            0
        );
        clock(&r, "2026-10-15T12:00:00+08:00");
        sink.outcome = DeliveryOutcome::SimulatedSuccess;
        let next = r.scan_with_sink(&stop, &mut sink).await.unwrap();
        assert_eq!(
            next.simulated_count,
            usize::from(outcome == DeliveryOutcome::DefiniteFailure)
        );
        assert_eq!(
            sink.calls,
            if outcome == DeliveryOutcome::DefiniteFailure {
                2
            } else {
                1
            }
        );
        assert_eq!(
            r.scan_with_sink(&stop, &mut sink).await.unwrap().due_count,
            0
        );
        drop(r);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_cancel_reservation_rollback_and_postcommit_unknown() {
    for phase in [
        "reserve-staged",
        "reserve-committed",
        "batch-verified",
        "simulated",
        "results-staged",
        "results-committed",
    ] {
        let db = Db::new().await;
        let r = db.runner().await;
        let before = db.journal().await;
        let stop = SubscriptionReminderStop::default();
        let (e, go) = pause(&r, phase);
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let (result, ()) = tokio::join!(r.scan_with_sink(&stop, &mut sink), async {
            entered(&e).await;
            stop.stop();
            go.notify_one();
        });
        assert!(result.is_err());
        let after = db.journal().await;
        if phase == "reserve-staged" {
            assert_eq!(after, before);
            assert_eq!(sink.calls, 0);
        } else if phase != "results-committed" {
            assert_eq!(
                after["commands"].as_array().unwrap().last().unwrap()["kind"],
                "ReserveScan"
            );
        }
        if phase == "reserve-committed" || phase == "batch-verified" {
            assert_eq!(sink.calls, 0);
        }
        // Restart clears only pending control; Unknown original attempt stays.
        let newer = db.service.startup_for_host(&db.session).await.unwrap();
        let rr = db.for_host(&newer).await;
        let before_recheck = db.journal().await;
        assert_eq!(
            rr.scan_once(&SubscriptionReminderStop::default())
                .await
                .unwrap()
                .due_count,
            if phase == "reserve-staged" { 1 } else { 0 }
        );
        if phase != "reserve-staged" {
            assert_eq!(db.journal().await, before_recheck);
        }
        drop(rr);
        drop(r);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_session_revocation_at_every_bank_await_and_phase() {
    for at in 1..=8 {
        let db = Db::new().await;
        let r = db.runner().await;
        let before = db.facts().await;
        db.bank.pause_at.store(at, Ordering::SeqCst);
        let stop = SubscriptionReminderStop::default();
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let (result, ()) = tokio::join!(r.scan_with_sink(&stop, &mut sink), async {
            entered(&db.bank.entered).await;
            db.sessions.revoke(&db.session);
            db.bank.release.notify_one();
        });
        assert_eq!(result.err(), Some(StorageError::Session), "bank await {at}");
        if at <= 4 {
            assert_eq!(sink.calls, 0);
        }
        let journal = db.journal().await;
        let commands = journal["commands"].as_array().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|v| v["kind"] == "ReserveScan")
                .count(),
            usize::from(at >= 4)
        );
        assert_eq!(
            commands
                .iter()
                .filter(|v| v["kind"] == "ResolveScan")
                .count(),
            usize::from(at >= 8)
        );
        db.record(&format!("session-bank-{at}"),json!({"before":before,"after":db.facts().await,"mock_calls":sink.calls,"publication_rejected":true}));
        drop(r);
        db.finish().await;
    }
    for phase in [
        "reserve-staged",
        "reserve-committed",
        "batch-verified",
        "simulated",
        "results-staged",
        "results-committed",
    ] {
        let db = Db::new().await;
        let r = db.runner().await;
        let (e, go) = pause(&r, phase);
        let stop = SubscriptionReminderStop::default();
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let (result, ()) = tokio::join!(r.scan_with_sink(&stop, &mut sink), async {
            entered(&e).await;
            db.sessions.bind(&db.session, "user_demo", 2);
            go.notify_one();
        });
        assert_eq!(result.err(), Some(StorageError::Session));
        drop(r);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_account_withdrawal_at_all_fences_stops_publication() {
    for at in 1..=8 {
        let db = Db::new().await;
        let r = db.runner().await;
        let before = db.facts().await;
        db.bank.pause_at.store(at, Ordering::SeqCst);
        let stop = SubscriptionReminderStop::default();
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let (result, ()) = tokio::join!(r.scan_with_sink(&stop, &mut sink), async {
            entered(&db.bank.entered).await;
            db.bank.denied.store(true, Ordering::SeqCst);
            db.bank.release.notify_one();
        });
        assert_eq!(result.err(), Some(StorageError::NotFound));
        if at <= 4 {
            assert_eq!(sink.calls, 0);
        }
        let journal = db.journal().await;
        let commands = journal["commands"].as_array().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|v| v["kind"] == "ReserveScan")
                .count(),
            usize::from(at >= 4)
        );
        assert_eq!(
            commands
                .iter()
                .filter(|v| v["kind"] == "ResolveScan")
                .count(),
            usize::from(at >= 8)
        );
        db.record(&format!("account-bank-{at}"),json!({"before":before,"after":db.facts().await,"mock_calls":sink.calls,"publication_rejected":true}));
        drop(r);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_save_and_deferred_commit_failures_never_forge_results() {
    for phase in ["ReserveScan", "ResolveScan"] {
        for deferred in [false, true] {
            let db = Db::new().await;
            let r = db.runner().await;
            let before = db.journal().await;
            let before_facts = db.facts().await;
            db.sql(&format!("CREATE FUNCTION fail_reminder() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.journal->'commands'->-1->>'kind'='{phase}' THEN RAISE EXCEPTION 'owned injected reminder write failure'; END IF; RETURN NEW; END $$; CREATE {} TRIGGER reminder_failure {} UPDATE ON subscription_runtime_records {} FOR EACH ROW EXECUTE FUNCTION fail_reminder()",if deferred{"CONSTRAINT"}else{""},if deferred{"AFTER"}else{"BEFORE"},if deferred{"DEFERRABLE INITIALLY DEFERRED"}else{""})).await;
            let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
            assert!(r
                .scan_with_sink(&SubscriptionReminderStop::default(), &mut sink)
                .await
                .is_err());
            let after = db.journal().await;
            if phase == "ReserveScan" {
                assert_eq!(sink.calls, 0);
                assert_eq!(after, before);
            } else {
                assert_eq!(sink.calls, 1);
                assert_eq!(
                    after["commands"].as_array().unwrap().last().unwrap()["kind"],
                    "ReserveScan"
                );
            }
            db.record(&format!("save-{phase}-deferred-{deferred}"),json!({"before":before_facts,"after":db.facts().await,"mock_calls":sink.calls,"normal_success_not_returned":true}));
            db.sql("DROP TRIGGER reminder_failure ON subscription_runtime_records;DROP FUNCTION fail_reminder()").await;
            let rr = db.runner().await;
            let next = rr
                .scan_once(&SubscriptionReminderStop::default())
                .await
                .unwrap();
            assert_eq!(next.simulated_count, usize::from(phase == "ReserveScan"));
            drop(rr);
            drop(r);
            db.finish().await;
        }
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_independent_readback_unavailable_never_resimulates() {
    for phase in ["reserve-committed", "results-committed"] {
        let db = Db::new().await;
        let mut r = db.runner().await;
        let before = db.facts().await;
        r.config.scan_timeout = Duration::from_millis(500);
        let (e, go) = pause(&r, phase);
        let stop = SubscriptionReminderStop::default();
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let (result, ()) = tokio::join!(r.scan_with_sink(&stop, &mut sink), async {
            entered(&e).await;
            let hold = db
                .service
                .repository
                .subscription_read_pool
                .acquire()
                .await
                .unwrap();
            go.notify_one();
            tokio::time::sleep(Duration::from_millis(700)).await;
            drop(hold);
        });
        assert_eq!(result.err(), Some(StorageError::ReconcileRequired));
        assert_eq!(sink.calls, usize::from(phase == "results-committed"));
        db.record(&format!("readback-{phase}"),json!({"before":before,"after":db.facts().await,"mock_calls":sink.calls,"normal_success_not_returned":true}));
        let rr = db.runner().await;
        assert_eq!(
            rr.scan_once(&SubscriptionReminderStop::default())
                .await
                .unwrap()
                .due_count,
            0
        );
        drop(rr);
        drop(r);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_two_scanners_one_owner_and_real_startup_serialization() {
    let db = Db::new().await;
    let r = db.runner().await;
    let other = db.for_host(&r.host).await;
    let stop = SubscriptionReminderStop::default();
    let (a, b) = tokio::join!(r.scan_once(&stop), other.scan_once(&stop));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.simulated_count + b.simulated_count, 1);
    let before = db.journal().await;
    let new = db.service.startup_for_host(&db.session).await.unwrap();
    assert_eq!(
        r.scan_once(&stop).await.err(),
        Some(StorageError::FactsConflict)
    );
    assert_ne!(before, db.journal().await);
    let rr = db.for_host(&new).await;
    assert_eq!(rr.scan_once(&stop).await.unwrap().due_count, 0);
    drop(rr);
    drop(other);
    drop(r);
    db.finish().await;
    let db = Db::new().await;
    let r = db.runner().await;
    let (e, go) = pause(&r, "batch-verified");
    let stop = SubscriptionReminderStop::default();
    let (result, new) = tokio::join!(r.scan_once(&stop), async {
        entered(&e).await;
        let startup = db.service.startup_for_host(&db.session);
        tokio::pin!(startup);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut startup)
                .await
                .is_err()
        );
        go.notify_one();
        startup.await.unwrap()
    });
    assert!(result.unwrap().result_saved);
    assert_ne!(new.generation, r.host.generation);
    assert_eq!(
        r.scan_once(&stop).await.err(),
        Some(StorageError::FactsConflict)
    );
    drop(r);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn reminder_host_changed_generation_or_legacy_settings_invalidate_original_batch() {
    for changed_host in [false, true] {
        let db = Db::new().await;
        let r = db.runner().await;
        let (e, go) = pause(&r, "batch-verified");
        let stop = SubscriptionReminderStop::default();
        let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
        let (result, ()) = tokio::join!(r.scan_with_sink(&stop, &mut sink), async {
            entered(&e).await;
            if changed_host {
                db.sql("UPDATE subscription_host_generations_v1 SET generation=generation+1")
                    .await;
            } else {
                // Legacy member caller bypassing Echo's owner lock: legitimate
                // original settings method, complete Journal CAS, own backend.
                let store = db.service.store().unwrap();
                let mut c = db.admin.acquire().await.unwrap();
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(&db.schema)
                    .execute(&mut *c)
                    .await
                    .unwrap();
                let mut w = store
                    .load_work_on_connection_mock(&mut c, "user_demo")
                    .await
                    .unwrap();
                let mut s = schedule(false);
                s.enabled = false;
                let b = w
                    .prepare_settings_mock(
                        operation("intervening-settings"),
                        "fictional-subscription-1",
                        s,
                        NOW,
                        "2026-10-14T12:05:00+08:00",
                    )
                    .await
                    .unwrap();
                w.confirm_settings_mock(&b, NOW, &mut Gate).await.unwrap();
                let mut tx = c.begin().await.unwrap();
                let p = store.save_on_connection_mock(&mut tx, w).await.unwrap();
                tx.commit().await.unwrap();
                let mut other = db.admin.acquire().await.unwrap();
                p.read_committed_on_connection_mock(&mut other)
                    .await
                    .unwrap();
            }
            go.notify_one();
        });
        assert_eq!(result.err(), Some(StorageError::FactsConflict));
        assert_eq!(sink.calls, 0);
        drop(r);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn reminder_host_scope_session_missing_ledger_bad_codec_and_config_fail_closed() {
    let db = Db::new().await;
    let r = db.runner().await;
    let other = Db::empty().await;
    let before = db.journal().await;
    assert_eq!(
        other
            .service
            .reminder_host_for_backend(&other.session, &r.host, other.bank.clone(), config())
            .await
            .err()
            .map(|e| e),
        Some(StorageError::Session)
    );
    let foreign = SessionRef::parse("foreign_session").unwrap();
    db.sessions.bind(&foreign, "foreign_owner", 1);
    assert_eq!(
        db.service
            .reminder_host_for_backend(&foreign, &r.host, db.bank.clone(), config())
            .await
            .err()
            .map(|e| e),
        Some(StorageError::Session)
    );
    let mut bad = config();
    bad.max_messages = 0;
    assert!(db
        .service
        .reminder_host_for_backend(&db.session, &r.host, db.bank.clone(), bad)
        .await
        .is_err());
    let mut bad = config();
    bad.accounts.push(bad.accounts[0].clone());
    assert!(bad.validate().is_err());
    let mut bad = config();
    bad.cycles = 65;
    assert!(bad.validate().is_err());
    let mut bad = config();
    bad.interval = Duration::ZERO;
    assert!(bad.validate().is_err());
    assert_eq!(db.journal().await, before);
    // Own schema corruption fault only; production immutable-prefix trigger
    // correctly refuses this mutation when enabled. Never touches original JSON.
    db.sql("ALTER TABLE subscription_runtime_records DROP CONSTRAINT subscription_runtime_records_encoding_version_check;ALTER TABLE subscription_runtime_records DISABLE TRIGGER USER;UPDATE subscription_runtime_records SET encoding_version='unsupported';ALTER TABLE subscription_runtime_records ENABLE TRIGGER USER").await;
    assert_eq!(
        r.scan_once(&SubscriptionReminderStop::default())
            .await
            .err(),
        Some(StorageError::CorruptRecord)
    );
    assert!(other
        .service
        .startup_for_host(&other.session)
        .await
        .is_err());
    let mut switched = r;
    switched.service.repository = other.service.repository.clone();
    assert_eq!(
        switched
            .scan_once(&SubscriptionReminderStop::default())
            .await
            .err(),
        Some(StorageError::TransactionScope)
    );
    drop(switched);
    db.finish().await;
    other.finish().await;
}
#[tokio::test]
#[ignore]
async fn reminder_host_owner_lock_timeout_cancel_pool_cleanup_and_finite_interval() {
    let db = Db::new().await;
    let mut r = db.runner().await;
    r.config.scan_timeout = Duration::from_millis(250);
    let before = db.journal().await;
    let mut blocker = db.admin.acquire().await.unwrap();
    let mut tx = blocker.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1||$2,0))")
        .bind(&db.schema)
        .bind(json!(["public-owner-execution-v1", r.identity.user_id]).to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        r.scan_once(&SubscriptionReminderStop::default())
            .await
            .err(),
        Some(StorageError::ReconcileRequired)
    );
    tx.rollback().await.unwrap();
    drop(blocker);
    assert_eq!(before, db.journal().await);
    r.config.scan_timeout = Duration::from_secs(8);
    r.config.cycles = 2;
    let (e, go) = pause(&r, "results-committed");
    let stop = SubscriptionReminderStop::default();
    let (result, ()) = tokio::join!(r.run(&stop), async {
        entered(&e).await;
        go.notify_one();
        tokio::time::sleep(Duration::from_millis(150)).await;
        stop.stop();
    });
    assert_eq!(result.err(), Some(StorageError::ReconcileRequired));
    assert_eq!(
        db.journal().await["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["kind"] == "ReserveScan")
            .count(),
        1
    );
    let rr = db.runner().await;
    assert_eq!(
        rr.scan_once(&SubscriptionReminderStop::default())
            .await
            .unwrap()
            .due_count,
        0
    );
    drop(rr);
    drop(r);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn reminder_host_mock_sink_has_no_open_business_transaction() {
    struct CheckSink;
    impl MockInAppSink for CheckSink {
        fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
            let psql = std::env::var("SAFEFLOW_TEST_PSQL").unwrap();
            let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
            let out=std::process::Command::new(psql).args(["-X","-qAt","-v","ON_ERROR_STOP=1","-d",&url,"-c","SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND state='idle in transaction'"]).output().unwrap();
            assert!(out.status.success());
            assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "0");
            DeliveryOutcome::SimulatedSuccess
        }
    }
    let db = Db::new().await;
    let r = db.runner().await;
    assert!(
        r.scan_with_sink(&SubscriptionReminderStop::default(), &mut CheckSink)
            .await
            .unwrap()
            .result_saved
    );
    drop(r);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn reminder_host_actual_commit_lost_reply_and_not_forwarded_both_stages() {
    for which in [1, 2] {
        for forwarded in [false, true] {
            let db = Db::new().await;
            let host = db.service.startup_for_host(&db.session).await.unwrap();
            let before = db.facts().await;
            let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
            let authority = url.rsplit('@').next().unwrap().split('/').next().unwrap();
            let upstream: std::net::SocketAddr = authority.parse().unwrap();
            let proxy = fault::Proxy::start(upstream, which, forwarded).await;
            let proxy_url = url.replace(authority, &proxy.address.to_string());
            let repo = PgRepository::connect_existing(&proxy_url, &db.schema)
                .await
                .unwrap();
            let service =
                SubscriptionService::for_trusted_backend(repo.clone(), db.sessions.clone());
            let r = service
                .reminder_host_for_backend(&db.session, &host, db.bank.clone(), config())
                .await
                .unwrap();
            clock(&r, NOW);
            let mut sink = Sink::new(DeliveryOutcome::SimulatedSuccess);
            let result = r
                .scan_with_sink(&SubscriptionReminderStop::default(), &mut sink)
                .await;
            assert!(proxy.fired.load(Ordering::SeqCst));
            assert!(result.is_err());
            assert_eq!(sink.calls, usize::from(which == 2));
            let journal = db.journal().await;
            let commands = journal["commands"].as_array().unwrap();
            assert_eq!(
                commands
                    .iter()
                    .filter(|v| v["kind"] == "ReserveScan")
                    .count(),
                usize::from(which == 2 || forwarded)
            );
            assert_eq!(
                commands
                    .iter()
                    .filter(|v| v["kind"] == "ResolveScan")
                    .count(),
                usize::from(which == 2 && forwarded)
            );
            db.record(&format!("commit-stage-{which}-forwarded-{forwarded}"),json!({"before":before,"after":db.facts().await,"stage":which,"commit_forwarded":forwarded,"proxy_fired":true,"mock_calls":sink.calls,"normal_success_not_returned":true}));
            println!(
                "COMMIT_FAULT {}",
                json!({"stage":which,"commit_forwarded":forwarded,"proxy_fired":true,"mock_calls":sink.calls})
            );
            drop(r);
            repo.close().await;
            drop(proxy);
            let rr = db.runner().await;
            let scan = rr
                .scan_once(&SubscriptionReminderStop::default())
                .await
                .unwrap();
            assert_eq!(scan.due_count, usize::from(which == 1 && !forwarded));
            drop(rr);
            db.finish().await;
        }
    }
}
fn evidence(name: &str) -> std::path::PathBuf {
    std::path::Path::new(&std::env::var("SAFEFLOW_SIMULATION_EVIDENCE").unwrap()).join(name)
}
#[tokio::test]
#[ignore]
async fn reminder_host_restart_seed() {
    assert!(!evidence("reminder-restart-seed.json").exists());
    let mut states = Vec::new();
    for kind in ["ordinary", "cancellation", "uncompleted"] {
        let db = Db::empty().await;
        db.initialize().await;
        db.settings(kind == "ordinary", true).await;
        if kind != "ordinary" {
            db.cancel().await;
        }
        let r = db.runner().await;
        *r.probe.clock.lock().unwrap() = None;
        let stop = SubscriptionReminderStop::default();
        let outcome = if kind == "uncompleted" {
            let (e, go) = pause(&r, "reserve-committed");
            let (result, ()) = tokio::join!(r.scan_once(&stop), async {
                entered(&e).await;
                stop.stop();
                go.notify_one();
            });
            assert!(result.is_err());
            Value::Null
        } else {
            let report = r.scan_once(&stop).await.unwrap();
            assert_eq!(report.simulated_count, 1);
            assert!(report.result_saved);
            serde_json::to_value(report).unwrap()
        };
        let journal = db.journal().await;
        let plan = serde_json::to_value(
            db.snapshot()
                .await
                .reminder_plan_mock("fictional-subscription-1")
                .unwrap(),
        )
        .unwrap();
        states.push(json!({"schema":db.schema,"kind":kind,"journal":journal,"plan":plan,"report":outcome,"old_host":{"namespace":r.host.namespace,"owner":r.host.owner,"instance":r.host.instance,"generation":r.host.generation}}));
        drop(r);
        db.service.repository.close().await;
        db.admin.close().await;
    }
    std::fs::write(evidence("reminder-restart-seed.json"),serde_json::to_vec_pretty(&json!({"process":std::process::id(),"states":states,"clock":"actual database clock_timestamp; original member daily rollover / cancellation late check"})).unwrap()).unwrap();
}
#[tokio::test]
#[ignore]
async fn reminder_host_restart_verify() {
    assert!(!evidence("reminder-restart-verify.json").exists());
    let seed: Value =
        serde_json::from_slice(&std::fs::read(evidence("reminder-restart-seed.json")).unwrap())
            .unwrap();
    assert_ne!(seed["process"], std::process::id());
    let mut observations = Vec::new();
    for state in seed["states"].as_array().unwrap() {
        let schema = state["schema"].as_str().unwrap().to_string();
        assert!(schema.starts_with("sf_reminder_test_") && schema.len() == 49);
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
        let repository = PgRepository::connect_existing(&url, &schema).await.unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let sessions = Arc::new(Sessions::default());
        let session = SessionRef::parse("new_process_reminder_current_session").unwrap();
        sessions.bind(&session, "user_demo", 1);
        let db = Db {
            schema,
            service: SubscriptionService::for_trusted_backend(repository, sessions.clone()),
            sessions,
            session,
            bank: Arc::new(Bank::default()),
            admin,
        };
        assert_eq!(db.journal().await, state["journal"]);
        let old = &state["old_host"];
        let stale = SubscriptionHost {
            namespace: old["namespace"].as_str().unwrap().into(),
            owner: old["owner"].as_str().unwrap().into(),
            instance: old["instance"].as_str().unwrap().into(),
            generation: old["generation"].as_i64().unwrap(),
        };
        let host = db.service.startup_for_host(&db.session).await.unwrap();
        assert!(host.generation > stale.generation);
        assert_eq!(
            db.service
                .reminder_host_for_backend(&db.session, &stale, db.bank.clone(), config())
                .await
                .err()
                .map(|e| e),
            Some(StorageError::FactsConflict)
        );
        let r = db.for_host(&host).await;
        *r.probe.clock.lock().unwrap() = None;
        let report = r
            .scan_once(&SubscriptionReminderStop::default())
            .await
            .unwrap();
        assert_eq!(report.simulated_count, 0);
        assert!(!report.reserved);
        let after = db.journal().await;
        assert_eq!(after["initial"], state["journal"]["initial"]);
        let before_commands = state["journal"]["commands"].as_array().unwrap();
        let after_commands = after["commands"].as_array().unwrap();
        assert!(after_commands.starts_with(before_commands));
        assert_eq!(after_commands.len(), before_commands.len() + 1);
        let plan = serde_json::to_value(
            db.snapshot()
                .await
                .reminder_plan_mock("fictional-subscription-1")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(plan, state["plan"]);
        observations.push(json!({"schema":db.schema,"kind":state["kind"],"original_prefix_preserved":true,"old_host_rejected":true,"no_simulation_after_restart":true,"plan":plan,"report":report,"journal_after":after}));
        drop(r);
        db.finish().await;
    }
    std::fs::write(evidence("reminder-restart-verify.json"),serde_json::to_vec_pretty(&json!({"process":std::process::id(),"seed_process":seed["process"],"observations":observations,"reimported":false,"reinitialized":false,"real_notifications":false})).unwrap()).unwrap();
}
