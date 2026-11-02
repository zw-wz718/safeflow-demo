//! Local, fictional database acceptance only. Owns its disposable fixture setup.
//! Uses actual foundation migrations/demo; never opens a public permit issuer.
use safeflow_contracts::{ResourceId, UserId};
use safeflow_storage::PgRepository;
use safeflow_subscription_evidence::{
    confirmation_mock::*, managed_ledger_mock::LedgerFixture, recognition::LOCAL_RULE_VERSION,
    reminder_mock::*, Sample,
};
use safeflow_transfer_integration::shared_demo::{DemoRecipient, SharedDemoPlan};
use serde_json::{json, Map, Value};
use sqlx::{postgres::PgConnectOptions, Connection, Executor, PgConnection};
use std::{error::Error, str::FromStr};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
#[track_caller]
fn ensure(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(format!(
            "ACCEPTANCE_ASSERTION_FAILED:{}",
            std::panic::Location::caller().line()
        )
        .into())
    }
}
fn initial(status: Option<&str>) -> Value {
    let entries = status.map(|s| vec![json!({"subscription_id":"fictional-subscription-1", "owner_user_id":"user_demo", "source_id":"fictional-hannah-ready-v1", "merchant_id":"fictional-hannah-video", "service_ref":"fictional-hannah-video-service", "status":s})]).unwrap_or_default();
    json!({"schema_version":1,"fictional":true,"production_registered":false,"owner_user_id":"user_demo","snapshot_id":"fictional-complete-1","read":{"state":"COMPLETE","is_complete":true,"next_cursor":null,"reason":null},"entries":entries})
}
fn journal(status: Option<&str>) -> Value {
    json!({"schema_version":1,"rule_version":LOCAL_RULE_VERSION,"fictional":true,"production_registered":false,"initial":initial(status),"commands":[]})
}
async fn connect(options: &PgConnectOptions) -> Result<PgConnection> {
    let mut conn = PgConnection::connect_with(options).await?;
    conn.execute("SET statement_timeout='15s'").await?;
    Ok(conn)
}
async fn snapshot(conn: &mut PgConnection, schema: &str) -> Result<Value> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname=$1 ORDER BY tablename",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    let mut values = Map::new();
    for t in tables {
        ensure(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))?;
        let data:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb) FROM \"{schema}\".\"{t}\" t")).fetch_one(&mut *conn).await?;
        values.insert(t, data);
    }
    Ok(Value::Object(values))
}
async fn seed(
    conn: &mut PgConnection,
    schema: &str,
    owner: &str,
    encoding: &str,
    data: Value,
) -> Result<()> {
    let version = data["commands"]
        .as_array()
        .ok_or("COMMAND_ARRAY_REQUIRED")?
        .len() as i64
        + 1;
    sqlx::query(&format!("INSERT INTO \"{schema}\".subscription_runtime_records(owner_id,storage_version,encoding_version,journal) VALUES($1,$2,$3,$4) ON CONFLICT(owner_id) DO UPDATE SET storage_version=EXCLUDED.storage_version,encoding_version=EXCLUDED.encoding_version,journal=EXCLUDED.journal"))
        .bind(owner).bind(version).bind(encoding).bind(data).execute(conn).await?;
    Ok(())
}
fn ddl(schema: &str) -> Result<String> {
    // Explicit TEST SETUP of the existing commented proposal, not a migration.
    let text = include_str!(env!("SAFEFLOW_SUBSCRIPTION_PROPOSED_SQL"));
    let body = text
        .split_once("/*")
        .ok_or("DDL_COMMENT_REQUIRED")?
        .1
        .split_once("*/")
        .ok_or("DDL_COMMENT_REQUIRED")?
        .0;
    ensure(
        body.matches("CREATE TABLE").count() == 1
            && body.contains("__APPROVED_SCHEMA__.subscription_runtime_records"),
    )?;
    Ok(body.replace("__APPROVED_SCHEMA__", &format!("\"{schema}\"")))
}

const NOW: &str = "2026-10-14T12:00:00+08:00";
const EXPIRES: &str = "2026-10-14T12:05:00+08:00";
fn op(id: &str) -> MockOperationContext {
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
struct Gate {
    calls: usize,
    outcome: MockGateOutcome,
}
impl Gate {
    fn yes() -> Self {
        Self {
            calls: 0,
            outcome: MockGateOutcome::Confirmed,
        }
    }
}
impl MockGateVerifier for Gate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        _: &MockConfirmationBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        self.outcome
    }
}
impl MockIntentGateVerifier for Gate {
    fn verify_and_consume_mock_intent(
        &mut self,
        _: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        self.calls += 1;
        self.outcome
    }
}
impl MockScheduleGate for Gate {
    fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
        self.calls += 1;
        self.outcome
    }
}
fn fail<T>(r: std::result::Result<T, impl std::fmt::Debug>) -> Result<T> {
    r.map_err(|e| format!("{e:?}").into())
}
async fn load(
    store: &ProposedLedgerStoreMock,
    c: &mut PgConnection,
) -> Result<ProposedLedgerWorkMock> {
    fail(store.load_work_on_connection_mock(c, "user_demo").await)
}
async fn view(
    reader: &ProposedLedgerReaderMock,
    c: &mut PgConnection,
) -> Result<SubscriptionReadSnapshotMock> {
    fail(reader.load_on_connection_mock(c, "user_demo").await)
}
async fn save(
    store: &ProposedLedgerStoreMock,
    c: &mut PgConnection,
    w: ProposedLedgerWorkMock,
) -> Result<PendingProposedLedgerWriteMock> {
    fail(store.save_on_connection_mock(c, w).await)
}
async fn record(
    rows: &mut Vec<Value>,
    name: &str,
    c: &mut PgConnection,
    schema: &str,
    base: &Value,
) -> Result<()> {
    let mut current = snapshot(c, schema).await?;
    current
        .as_object_mut()
        .unwrap()
        .remove("subscription_runtime_records");
    ensure(&current == base)?;
    rows.push(json!({"case":name,"verified":true,"fictional":true,"actual_postgresql":true,"foundation_rows_unchanged":true}));
    Ok(())
}
async fn reg(
    w: &mut ProposedLedgerWorkMock,
    s: &Sample,
    id: &str,
) -> Result<MockConfirmationBinding> {
    let b = fail(
        w.prepare_registration_mock(&s.trusted_context_fixture, &s.snapshot, op(id), 0, EXPIRES)
            .await,
    )?;
    fail(
        w.confirm_registration_mock(&b.draft_id, &b.context, NOW, &mut Gate::yes())
            .await,
    )?;
    Ok(b)
}
async fn intent(
    w: &mut ProposedLedgerWorkMock,
    id: &str,
    subscription: &str,
    kind: MockIntentKind,
) -> Result<MockIntentBinding> {
    let b = fail(
        w.prepare_intent_mock(op(id), subscription, kind, NOW, EXPIRES)
            .await,
    )?;
    fail(
        w.confirm_intent_mock(&b.draft_id, &b.context, NOW, &mut Gate::yes())
            .await,
    )?;
    Ok(b)
}
fn schedule() -> Schedule {
    Schedule {
        cycle: safeflow_subscription_evidence::recognition::Cycle::CalendarMonth,
        anchor_date: "2026-09-17".parse().unwrap(),
        period_index: 1,
        expected_date: "2026-10-17".parse().unwrap(),
        enabled: true,
        days_before: 3,
    }
}
async fn settings(
    w: &mut ProposedLedgerWorkMock,
    id: &str,
    subscription: &str,
    s: Schedule,
) -> Result<ScheduleBinding> {
    let b = fail(
        w.prepare_settings_mock(op(id), subscription, s, NOW, EXPIRES)
            .await,
    )?;
    fail(w.confirm_settings_mock(&b, NOW, &mut Gate::yes()).await)?;
    Ok(b)
}
// Existing Memory business constructs this older, explicitly fictional journal.
// ReserveScan represents an unknown attempt; it calls no real notification.
fn unknown_fixture() -> Result<Value> {
    let initial: LedgerFixture = serde_json::from_value(initial(Some("ACTIVE")))?;
    let ledger = MemoryConfirmationMock::from_fixture(initial.clone());
    let mut reminders = MemoryReminderMock::default();
    let s = schedule();
    let b = fail(reminders.prepare_mock(
        &ledger,
        op("fixture-settings"),
        "fictional-subscription-1",
        s.clone(),
        NOW,
        EXPIRES,
    ))?;
    let receipt = fail(reminders.confirm_mock(&ledger, &b, NOW, &mut Gate::yes()))?;
    struct Unknown {
        messages: Vec<Value>,
    }
    impl MockInAppSink for Unknown {
        fn simulate_in_app(&mut self, m: &ReminderMessage) -> DeliveryOutcome {
            self.messages.push(serde_json::to_value(m).unwrap());
            DeliveryOutcome::Unknown
        }
    }
    let mut sink = Unknown { messages: vec![] };
    let report = fail(reminders.scan_mock(&ledger, "user_demo", NOW, &mut sink))?;
    ensure(sink.messages.len() == 1)?;
    Ok(
        json!({"schema_version":1,"rule_version":LOCAL_RULE_VERSION,"fictional":true,"production_registered":false,"initial":initial,"commands":[
            {"kind":"PrepareSettings","context":b.context,"id":"fictional-subscription-1","schedule":{"cycle":s.cycle,"anchor":s.anchor_date.to_string(),"index":s.period_index,"expected":s.expected_date.to_string(),"enabled":true,"days_before":3},"now":NOW,"expires":EXPIRES,"binding":b},
            {"kind":"Settings","operation":b.context.operation_id,"binding":b,"now":NOW,"receipt":receipt},
            {"kind":"ReserveScan","now":NOW,"messages":sink.messages,"report":report}
        ]}),
    )
}
async fn cases(
    c: &mut PgConnection,
    other: &mut PgConnection,
    options: &PgConnectOptions,
    schema: &str,
    role: &str,
    sample: &Sample,
    base: &Value,
) -> Result<Vec<Value>> {
    let store = fail(ProposedLedgerStoreMock::new(schema))?;
    let reader = fail(ProposedLedgerReaderMock::new(schema))?;
    let mut rows = Vec::new();
    ensure(matches!(
        store.load_work_on_connection_mock(c, "missing-owner").await,
        Err(ProposedLedgerWriteIssue::Read(
            ProposedLedgerReadIssue::NotFound
        ))
    ))?;
    record(&mut rows, "missing_owner_not_initialized", c, schema, base).await?;
    seed(
        c,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(None),
    )
    .await?;
    let mut w = load(&store, c).await?;
    let b = fail(
        w.prepare_registration_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            op("draft"),
            0,
            EXPIRES,
        )
        .await,
    )?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    ensure(matches!(
        p.read_committed_on_connection_mock(c).await,
        Err(ProposedLedgerWriteIssue::VerificationConnectionRequired)
    ))?;
    ensure(matches!(
        p.read_committed_on_connection_mock(other).await,
        Err(ProposedLedgerWriteIssue::CommitNotObserved)
    ))?;
    ensure(
        view(&reader, other)
            .await?
            .pending_confirmations_mock(NOW)?
            .items
            .is_empty(),
    )?;
    c.execute("ROLLBACK").await?;
    ensure(matches!(
        p.read_committed_on_connection_mock(other).await,
        Err(ProposedLedgerWriteIssue::CommitNotObserved)
    ))?;
    record(
        &mut rows,
        "draft_rollback_and_uncommitted_visibility",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let rolled = reg(&mut w, sample, "rolled-registration").await?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    ensure(c.execute("SELECT 1/0").await.is_err())?;
    c.execute("ROLLBACK").await?;
    ensure(p.read_committed_on_connection_mock(other).await.is_err())?;
    let check = view(&reader, c).await?;
    ensure(
        check.list_subscriptions_mock()?.items.is_empty()
            && matches!(
                check
                    .original_result_mock(&OriginalOperationRequestMock::registration(&rolled)?)?,
                OriginalResultMock::NeedsReview
            ),
    )?;
    record(
        &mut rows,
        "registration_rollback_has_no_saved_success",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let registration = reg(&mut w, sample, "register").await?;
    c.execute("BEGIN").await?;
    let pending_reg = save(&store, c, w).await?;
    c.execute("COMMIT").await?;
    let saved = fail(pending_reg.read_committed_on_connection_mock(other).await)?;
    let id = saved.list_subscriptions_mock()?.items[0]
        .subscription_id
        .clone();
    let request = OriginalOperationRequestMock::registration(&registration)?;
    let original = saved.original_result_mock(&request)?;
    ensure(
        matches!(original, OriginalResultMock::Recorded { .. })
            && saved.list_subscriptions_mock()?.items.len() == 1,
    )?;
    record(
        &mut rows,
        "registration_committed_original_fact_readback",
        c,
        schema,
        base,
    )
    .await?;
    let before = saved.version_mock();
    // Identical fictional rows in another database cannot authenticate a save
    // or a commit readback. Only this run's disposable server is touched.
    c.execute("CREATE DATABASE safeflow_subscription_write_other_test")
        .await?;
    let mut wrong = PgConnection::connect_with(
        &options
            .clone()
            .database("safeflow_subscription_write_other_test"),
    )
    .await?;
    wrong
        .execute(format!("CREATE SCHEMA \"{schema}\"").as_str())
        .await?;
    wrong.execute(ddl(schema)?.as_str()).await?;
    let cloned: Value = sqlx::query_scalar(&format!(
        "SELECT journal FROM \"{schema}\".subscription_runtime_records WHERE owner_id='user_demo'"
    ))
    .fetch_one(&mut *c)
    .await?;
    seed(
        &mut wrong,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        cloned,
    )
    .await?;
    let work = load(&store, c).await?;
    ensure(matches!(
        store.save_on_connection_mock(&mut wrong, work).await,
        Err(ProposedLedgerWriteIssue::VerificationDatabaseMismatch)
    ))?;
    ensure(matches!(
        pending_reg
            .read_committed_on_connection_mock(&mut wrong)
            .await,
        Err(ProposedLedgerWriteIssue::VerificationDatabaseMismatch)
    ))?;
    wrong.close().await?;
    c.execute("DROP DATABASE safeflow_subscription_write_other_test")
        .await?;
    record(
        &mut rows,
        "identical_fixture_in_wrong_database_cannot_save_or_verify",
        c,
        schema,
        base,
    )
    .await?;
    let mut replay = load(&store, c).await?;
    let mut gate = Gate::yes();
    fail(
        replay
            .confirm_registration_mock(
                &registration.draft_id,
                &registration.context,
                NOW,
                &mut gate,
            )
            .await,
    )?;
    let p = save(&store, c, replay).await?;
    let replayed = fail(p.read_committed_on_connection_mock(other).await)?;
    ensure(
        gate.calls == 0
            && replayed.version_mock() == before
            && replayed.original_result_mock(&request)? == original,
    )?;
    record(
        &mut rows,
        "registration_exact_replay_no_duplicate_version_or_gate",
        c,
        schema,
        base,
    )
    .await?;
    let mut bad = load(&store, c).await?;
    let mut changed = registration.context.clone();
    changed.plan_version += 1;
    ensure(
        bad.confirm_registration_mock(&registration.draft_id, &changed, NOW, &mut gate)
            .await
            .is_err(),
    )?;
    ensure(matches!(
        store.save_on_connection_mock(c, bad).await,
        Err(ProposedLedgerWriteIssue::Poisoned)
    ))?;
    record(
        &mut rows,
        "changed_original_parameters_poison_work",
        c,
        schema,
        base,
    )
    .await?;
    let mut bad = load(&store, c).await?;
    let mut foreign = op("foreign");
    foreign.user_id = "not-owner".into();
    ensure(
        bad.prepare_intent_mock(foreign, &id, MockIntentKind::Record, NOW, EXPIRES)
            .await
            .is_err(),
    )?;
    ensure(store.save_on_connection_mock(c, bad).await.is_err())?;
    record(
        &mut rows,
        "foreign_owner_rejected_without_saving",
        c,
        schema,
        base,
    )
    .await?;
    let different = fail(ProposedLedgerStoreMock::new("sub_write_other_fixture"))?;
    ensure(matches!(
        different
            .save_on_connection_mock(c, load(&store, other).await?)
            .await,
        Err(ProposedLedgerWriteIssue::Conflict)
    ))?;
    record(
        &mut rows,
        "work_cannot_be_saved_into_other_namespace",
        c,
        schema,
        base,
    )
    .await?;
    let mut stale = load(&store, c).await?;
    let mut current = load(&store, other).await?;
    let cancellation = intent(&mut current, "cancel", &id, MockIntentKind::Record).await?;
    let cancel_pending = save(&store, other, current).await?;
    let cancelled = fail(cancel_pending.read_committed_on_connection_mock(c).await)?;
    let status = cancelled.cancellation_status_mock(&id)?;
    ensure(
        status.current_state
            == safeflow_subscription_evidence::managed_ledger_mock::LedgerStatus::Cancelling
            && status.history.len() == 1
            && !status.merchant_cancellation_confirmed,
    )?;
    record(
        &mut rows,
        "cancellation_local_intent_and_history_committed",
        c,
        schema,
        base,
    )
    .await?;
    intent(&mut stale, "stale-cancel", &id, MockIntentKind::Record).await?;
    ensure(matches!(
        store.save_on_connection_mock(c, stale).await,
        Err(ProposedLedgerWriteIssue::Conflict)
    ))?;
    record(
        &mut rows,
        "concurrent_stale_version_cannot_overwrite",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let mut gate = Gate::yes();
    fail(
        w.confirm_intent_mock(
            &cancellation.draft_id,
            &cancellation.context,
            NOW,
            &mut gate,
        )
        .await,
    )?;
    let p = save(&store, c, w).await?;
    let again = fail(p.read_committed_on_connection_mock(other).await)?;
    ensure(
        gate.calls == 0
            && again.version_mock() == cancelled.version_mock()
            && again.cancellation_status_mock(&id)?.history.len() == 1,
    )?;
    record(
        &mut rows,
        "cancellation_replay_preserves_single_history",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let rolled_revoke = intent(&mut w, "rolled-revoke", &id, MockIntentKind::Revoke).await?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    c.execute("ROLLBACK").await?;
    ensure(matches!(
        p.read_committed_on_connection_mock(other).await,
        Err(ProposedLedgerWriteIssue::CommitNotObserved)
    ))?;
    ensure(matches!(
        view(&reader, c)
            .await?
            .original_result_mock(&OriginalOperationRequestMock::intent(&rolled_revoke)?)?,
        OriginalResultMock::NeedsReview
    ))?;
    ensure(
        view(&reader, c)
            .await?
            .cancellation_status_mock(&id)?
            .history
            .len()
            == 1,
    )?;
    record(
        &mut rows,
        "revoke_rollback_keeps_cancelling_and_old_history",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let revoke = intent(&mut w, "revoke", &id, MockIntentKind::Revoke).await?;
    let p = save(&store, c, w).await?;
    let saved = fail(p.read_committed_on_connection_mock(other).await)?;
    ensure(
        saved.cancellation_status_mock(&id)?.history.len() == 2
            && saved.cancellation_status_mock(&id)?.current_state
                == safeflow_subscription_evidence::managed_ledger_mock::LedgerStatus::Active,
    )?;
    record(
        &mut rows,
        "revoke_commit_preserves_both_history_entries",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let mut gate = Gate::yes();
    fail(
        w.confirm_intent_mock(&revoke.draft_id, &revoke.context, NOW, &mut gate)
            .await,
    )?;
    let p = save(&store, c, w).await?;
    ensure(
        fail(p.read_committed_on_connection_mock(other).await)?.version_mock()
            == saved.version_mock()
            && gate.calls == 0,
    )?;
    record(
        &mut rows,
        "revoke_exact_replay_no_extra_event",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let configured = settings(&mut w, "settings", &id, schedule()).await?;
    let p = save(&store, c, w).await?;
    let configured_saved = fail(p.read_committed_on_connection_mock(other).await)?;
    ensure(configured_saved.reminder_plan_mock(&id)?.schedule == Some(schedule()))?;
    record(
        &mut rows,
        "reminder_settings_committed_with_original_binding",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let mut gate = Gate::yes();
    fail(w.confirm_settings_mock(&configured, NOW, &mut gate).await)?;
    let p = save(&store, c, w).await?;
    ensure(
        fail(p.read_committed_on_connection_mock(other).await)?.version_mock()
            == configured_saved.version_mock()
            && gate.calls == 0,
    )?;
    record(
        &mut rows,
        "settings_replay_no_duplicate_version_or_gate",
        c,
        schema,
        base,
    )
    .await?;
    let mut disabled = schedule();
    disabled.enabled = false;
    let mut w = load(&store, c).await?;
    let change = fail(
        w.prepare_settings_mock(op("denied-settings"), &id, disabled.clone(), NOW, EXPIRES)
            .await,
    )?;
    let mut deny = Gate {
        calls: 0,
        outcome: MockGateOutcome::Denied,
    };
    ensure(
        w.confirm_settings_mock(&change, NOW, &mut deny)
            .await
            .is_err(),
    )?;
    ensure(
        store.save_on_connection_mock(c, w).await.is_err()
            && view(&reader, c).await?.reminder_plan_mock(&id)?.schedule == Some(schedule()),
    )?;
    record(
        &mut rows,
        "denied_confirmation_cannot_save_partial_settings",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    settings(&mut w, "rolled-settings", &id, disabled.clone()).await?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    ensure(c.execute("SELECT 1/0").await.is_err())?;
    c.execute("ROLLBACK").await?;
    ensure(
        p.read_committed_on_connection_mock(other).await.is_err()
            && view(&reader, c).await?.reminder_plan_mock(&id)?.schedule == Some(schedule()),
    )?;
    record(
        &mut rows,
        "later_transaction_failure_rolls_back_settings",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    settings(&mut w, "readonly-settings", &id, disabled.clone()).await?;
    c.execute("BEGIN READ ONLY").await?;
    ensure(store.save_on_connection_mock(c, w).await.is_err())?;
    c.execute("ROLLBACK").await?;
    ensure(view(&reader, c).await?.reminder_plan_mock(&id)?.schedule == Some(schedule()))?;
    record(
        &mut rows,
        "read_only_transaction_does_not_commit_settings",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    settings(&mut w, "disabled-settings", &id, disabled.clone()).await?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    c.execute("COMMIT").await?;
    let final_settings = fail(p.read_committed_on_connection_mock(other).await)?;
    ensure(final_settings.reminder_plan_mock(&id)?.schedule == Some(disabled))?;
    ensure(final_settings.original_result_mock(&request)? == original)?;
    record(
        &mut rows,
        "settings_edit_keeps_registration_and_original_receipts",
        c,
        schema,
        base,
    )
    .await?;
    // Reopening the connection is a process-style reload, NOT a Docker restart.
    let mut reload = load(&store, c).await?;
    fail(reload.invalidate_pending_for_restart_mock().await)?;
    let p = save(&store, c, reload).await?;
    let reread = fail(p.read_committed_on_connection_mock(other).await)?;
    ensure(
        reread.reminder_plan_mock(&id)?.schedule
            == final_settings.reminder_plan_mock(&id)?.schedule
            && reread.cancellation_status_mock(&id)?.history.len() == 2,
    )?;
    ensure(
        fail(pending_reg.read_committed_on_connection_mock(other).await)?
            .original_result_mock(&request)?
            == original,
    )?;
    record(
        &mut rows,
        "restart_invalidation_preserves_saved_business_and_prefix_readback",
        c,
        schema,
        base,
    )
    .await?;
    // Same version with a changed journal must not defeat the full-base CAS.
    let mut stale = load(&store, c).await?;
    let raw = view(&reader, c).await?;
    let version = raw.version_mock();
    sqlx::query(&format!("UPDATE \"{schema}\".subscription_runtime_records SET journal=jsonb_set(journal,'{{initial,snapshot_id}}','\"fictional-tamper\"'::jsonb) WHERE owner_id='user_demo'" )).execute(&mut *c).await?;
    fail(stale.invalidate_pending_for_restart_mock().await)?;
    ensure(matches!(
        store.save_on_connection_mock(c, stale).await,
        Err(ProposedLedgerWriteIssue::Conflict)
    ))?;
    ensure(view(&reader, c).await?.version_mock() == version)?;
    ensure(
        pending_reg
            .read_committed_on_connection_mock(other)
            .await
            .is_err(),
    )?;
    record(
        &mut rows,
        "same_version_journal_tamper_rejected_by_cas_and_readback",
        c,
        schema,
        base,
    )
    .await?;
    c.execute(format!("SET ROLE \"{role}\"").as_str()).await?;
    let mut w = load(&store, c).await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    ensure(store.save_on_connection_mock(c, w).await.is_err())?;
    c.execute("RESET ROLE").await?;
    record(&mut rows, "select_only_role_cannot_save", c, schema, base).await?;
    seed(c, schema, "user_demo", "unsupported", journal(None)).await?;
    ensure(matches!(
        store.load_work_on_connection_mock(c, "user_demo").await,
        Err(ProposedLedgerWriteIssue::Read(
            ProposedLedgerReadIssue::UnsupportedEncoding
        ))
    ))?;
    record(
        &mut rows,
        "unknown_encoding_rejected_before_work",
        c,
        schema,
        base,
    )
    .await?;
    seed(
        c,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        unknown_fixture()?,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let mut s = schedule();
    s.enabled = false;
    settings(
        &mut w,
        "disable-unknown",
        "fictional-subscription-1",
        s.clone(),
    )
    .await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    let p = save(&store, c, w).await?;
    let read = fail(p.read_committed_on_connection_mock(other).await)?;
    let plan = read.reminder_plan_mock("fictional-subscription-1")?;
    ensure(
        plan.schedule == Some(s)
            && plan.saved_cycle_outcomes.len() == 1
            && plan.saved_cycle_outcomes[0].state == ReminderAttemptStateMock::NeedsReview
            && !plan.notification_sent,
    )?;
    record(
        &mut rows,
        "settings_and_startup_preserve_prior_unknown_attempt",
        c,
        schema,
        base,
    )
    .await?;
    // Leave this independent seed for an actual container restart and second run.
    let mut w = load(&store, c).await?;
    intent(
        &mut w,
        "restart-cancel",
        "fictional-subscription-1",
        MockIntentKind::Record,
    )
    .await?;
    let _pending = fail(
        w.prepare_intent_mock(
            op("restart-unfinished-revoke"),
            "fictional-subscription-1",
            MockIntentKind::Revoke,
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    let p = save(&store, c, w).await?;
    let _committed = fail(p.read_committed_on_connection_mock(other).await)?;
    record(
        &mut rows,
        "restart_seed_committed_settings_intent_unknown_and_pending",
        c,
        schema,
        base,
    )
    .await?;
    let _ = b;
    Ok(rows)
}
async fn borrowed_type_check(
    transaction: &mut safeflow_storage::PermitTransaction,
    store: &ProposedLedgerStoreMock,
    owner: &str,
) -> std::result::Result<ProposedLedgerWorkMock, String> {
    let connection = transaction.connection().map_err(|e| format!("{e:?}"))?;
    store
        .load_work_on_connection_mock(connection, owner)
        .await
        .map_err(|e| format!("{e:?}"))
}
// Windows defaults to a small main-thread stack. The acceptance program holds
// many independent failure scenarios; this is a test-only stack allocation.
fn main() -> Result<()> {
    std::thread::Builder::new()
        .name("fictional-subscription-acceptance".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| -> std::result::Result<(), String> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(run())
                .map_err(|e| e.to_string())
        })?
        .join()
        .map_err(|_| "PROBE_THREAD_FAILED")?
        .map_err(Into::into)
}
async fn run() -> Result<()> {
    let _ = borrowed_type_check;
    let options =
        PgConnectOptions::from_str(&std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL")?)?;
    ensure(
        options.get_host() == "127.0.0.1"
            && options.get_database() == Some("safeflow_subscription_write_test"),
    )?;
    let schema = std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_SCHEMA")?;
    ensure(
        schema.starts_with("sub_write_check_")
            && schema.len() < 42
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
    )?;
    let role = format!("{schema}_select");
    let reader = fail(ProposedLedgerReaderMock::new(&schema))?;
    let mut c = connect(&options).await?;
    let mut other = connect(&options).await?;
    if std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_PHASE").as_deref() == Ok("after-restart") {
        let read = view(&reader, &mut c).await?;
        let plan = read.reminder_plan_mock("fictional-subscription-1")?;
        ensure(
            plan.schedule.as_ref().is_some_and(|s| !s.enabled)
                && plan.saved_cycle_outcomes[0].state == ReminderAttemptStateMock::NeedsReview,
        )?;
        ensure(
            read.cancellation_status_mock("fictional-subscription-1")?
                .history
                .len()
                == 1
                && !read
                    .cancellation_status_mock("fictional-subscription-1")?
                    .merchant_cancellation_confirmed,
        )?;
        ensure(
            read.pending_confirmations_mock(NOW)?
                .items
                .iter()
                .any(|i| i.action == "REVOKE_INTENT" && i.state == "READY"),
        )?;
        let store = fail(ProposedLedgerStoreMock::new(&schema))?;
        let mut w = load(&store, &mut c).await?;
        fail(w.invalidate_pending_for_restart_mock().await)?;
        let p = save(&store, &mut c, w).await?;
        let read = fail(p.read_committed_on_connection_mock(&mut other).await)?;
        ensure(
            read.pending_confirmations_mock(NOW)?
                .items
                .iter()
                .any(|i| i.action == "REVOKE_INTENT" && i.state == "INVALIDATED"),
        )?;
        let plan = read.reminder_plan_mock("fictional-subscription-1")?;
        ensure(
            plan.saved_cycle_outcomes[0].state == ReminderAttemptStateMock::NeedsReview
                && !plan.notification_sent,
        )?;
        c.execute(format!("DROP SCHEMA \"{schema}\" CASCADE; DROP ROLE \"{role}\"").as_str())
            .await?;
        println!(
            "{}",
            json!({"phase":"after-restart","verified":true,"settings_restored":true,"cancellation_restored":true,"unknown_retained":true,"pending_invalidated_explicitly":true,"notification_sent":false,"cleanup_completed":true})
        );
        return Ok(());
    }
    let repo = PgRepository::connect(
        &std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL")?,
        &schema,
    )
    .await?;
    repo.migrate().await?;
    let plan = SharedDemoPlan::prepare(DemoRecipient {
        owner_id: UserId::parse("fixture_subscription_recipient")?,
        account_id: ResourceId::parse("fixture_subscription_account")?,
    })?;
    repo.initialize_shared_demo(&plan).await?;
    let versions: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT version FROM \"{schema}\"._sqlx_migrations WHERE success ORDER BY version"
    ))
    .fetch_all(&mut c)
    .await?;
    ensure(versions == (1..=21).collect::<Vec<_>>())?;
    let base = snapshot(&mut c, &schema).await?;
    c.execute(ddl(&schema)?.as_str()).await?;
    c.execute(format!("CREATE ROLE \"{role}\" NOLOGIN; GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\"; GRANT SELECT ON \"{schema}\".subscription_runtime_records TO \"{role}\"").as_str()).await?;
    let fixture: Value = serde_json::from_str(include_str!(env!(
        "SAFEFLOW_SUBSCRIPTION_DISCOVERY_FIXTURE"
    )))?;
    let sample: Sample = serde_json::from_value(fixture["sample"].clone())?;
    let records = cases(&mut c, &mut other, &options, &schema, &role, &sample, &base).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace WHERE nspname='safeflow_subscription_mock_v1'",
    )
    .fetch_one(&mut c)
    .await?;
    ensure(count == 0)?;
    repo.close().await;
    println!(
        "{}",
        json!({"scope":"ACTUAL_FOUNDATION_PROPOSED_FICTIONAL_SUBSCRIPTION_WRITE","verified":true,"cases":records,"migration_versions":versions,"old_mock_schema_installed":false,"public_permit_transaction_executed":false,"formal_subscription_integrated":false,"formal_codec_adopted":false,"notification_sent":false,"merchant_cancellation_confirmed":false,"actual_foundation_migrator_and_demo":true})
    );
    Ok(())
}
