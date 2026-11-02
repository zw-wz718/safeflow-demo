//! One fictional subscription startup/recovery boundary. No production sender,
//! SecurityKernel, shared migration, or recovery of other banking modules.
use super::*;
use crate::reminder_mock::{
    DeliveryOutcome, DeliveryReport, MemoryReminderMock, MockInAppSink, MockScheduleGate,
    ReminderMessage, Schedule, ScheduleBinding, ScheduleReceipt,
};
use chrono::NaiveDate;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use std::collections::BTreeSet;
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeRecoveryIssue {
    Unavailable,
    NotFound,
    Conflict,
    InvalidJournal,
    DedicatedDatabaseRequired,
    RestartRequiresNewOperation,
    OperationAbandoned,
    StagedOperationUnsupported,
    CommitNotObserved,
    OriginalRequestConflict,
    InvalidCursor,
    InvalidPageSize,
    Business(MockIssue),
    Reminder(&'static str),
}
impl std::fmt::Display for RuntimeRecoveryIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RuntimeRecoveryIssue {}
type Result<T> = std::result::Result<T, RuntimeRecoveryIssue>;
fn value(v: &impl Serialize) -> Result<Value> {
    serde_json::to_value(v).map_err(|_| RuntimeRecoveryIssue::InvalidJournal)
}
fn equal(actual: Value, expected: &Value) -> Result<()> {
    if &actual == expected {
        Ok(())
    } else {
        Err(RuntimeRecoveryIssue::InvalidJournal)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSchedule {
    cycle: crate::recognition::Cycle,
    anchor: String,
    index: u32,
    expected: String,
    enabled: bool,
    days_before: u8,
}
impl StoredSchedule {
    fn from(s: &Schedule) -> Self {
        Self {
            cycle: s.cycle,
            anchor: s.anchor_date.to_string(),
            index: s.period_index,
            expected: s.expected_date.to_string(),
            enabled: s.enabled,
            days_before: s.days_before,
        }
    }
    fn schedule(&self) -> Result<Schedule> {
        let parse = |s: &str| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| RuntimeRecoveryIssue::InvalidJournal)
        };
        Ok(Schedule {
            cycle: self.cycle,
            anchor_date: parse(&self.anchor)?,
            period_index: self.index,
            expected_date: parse(&self.expected)?,
            enabled: self.enabled,
            days_before: self.days_before,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Command {
    AbandonPending {
        position: usize,
        scope: String,
        context: MockOperationContext,
        binding: Value,
        now: String,
    },
    PrepareRegistration {
        context: MockContext,
        snapshot: Box<Snapshot>,
        operation: MockOperationContext,
        index: usize,
        expires: String,
        binding: Value,
    },
    Register {
        draft: String,
        context: MockOperationContext,
        now: String,
        receipt: Value,
    },
    PrepareIntent {
        context: MockOperationContext,
        id: String,
        intent: MockIntentKind,
        now: String,
        expires: String,
        binding: Value,
    },
    Intent {
        draft: String,
        context: MockOperationContext,
        now: String,
        receipt: Value,
    },
    PrepareSettings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reminder_rules: Option<u8>,
        context: MockOperationContext,
        id: String,
        schedule: StoredSchedule,
        now: String,
        expires: String,
        binding: Value,
    },
    Settings {
        operation: String,
        binding: Value,
        now: String,
        receipt: Value,
    },
    ReserveScan {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reminder_rules: Option<u8>,
        now: String,
        messages: Vec<Value>,
        report: Value,
    },
    ResolveScan {
        now: String,
        outcomes: Vec<DeliveryOutcome>,
        report: Value,
    },
    Restart,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    rule_version: String,
    fictional: bool,
    production_registered: bool,
    initial: LedgerFixture,
    commands: Vec<Command>,
}
// These gates validate replay of already committed fictional facts only. They
// are never exposed as approval providers or called against external services.
struct HistoricalGate;
impl MockGateVerifier for HistoricalGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        _: &MockConfirmationBinding,
        _: &str,
    ) -> MockGateOutcome {
        MockGateOutcome::Confirmed
    }
}
impl MockIntentGateVerifier for HistoricalGate {
    fn verify_and_consume_mock_intent(
        &mut self,
        _: &MockIntentBinding,
        _: &str,
    ) -> MockGateOutcome {
        MockGateOutcome::Confirmed
    }
}
impl MockScheduleGate for HistoricalGate {
    fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
        MockGateOutcome::Confirmed
    }
}
#[derive(Default)]
struct ReserveSink {
    messages: Vec<ReminderMessage>,
}
impl MockInAppSink for ReserveSink {
    fn simulate_in_app(&mut self, m: &ReminderMessage) -> DeliveryOutcome {
        self.messages.push(m.clone());
        DeliveryOutcome::Unknown
    }
}
struct RecordedSink {
    outcomes: Vec<DeliveryOutcome>,
    index: usize,
}
impl MockInAppSink for RecordedSink {
    fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
        let outcome = self
            .outcomes
            .get(self.index)
            .copied()
            .unwrap_or(DeliveryOutcome::Unknown);
        self.index += 1;
        outcome
    }
}
#[derive(Clone)]
struct State {
    ledger: MemoryConfirmationMock,
    reminders: MemoryReminderMock,
    settings: BTreeMap<String, ScheduleBinding>,
    pending: BTreeSet<(String, String, String)>,
    blocked: BTreeSet<(String, String)>,
    abandoned: BTreeSet<(String, String)>,
    scan_base: Option<(String, MemoryReminderMock, usize)>,
    current_reminder_rules: bool,
}
impl State {
    fn initial(fixture: LedgerFixture, user: &str) -> Result<Self> {
        let ledger = MemoryConfirmationMock::from_fixture(fixture);
        ledger.ready(user).map_err(RuntimeRecoveryIssue::Business)?;
        Ok(Self {
            ledger,
            reminders: MemoryReminderMock::default(),
            settings: BTreeMap::new(),
            pending: BTreeSet::new(),
            blocked: BTreeSet::new(),
            abandoned: BTreeSet::new(),
            scan_base: None,
            current_reminder_rules: false,
        })
    }
    fn guard(&self, scope: &str, c: &MockOperationContext, user: &str) -> Result<()> {
        if c.user_id != user {
            return Err(RuntimeRecoveryIssue::Business(
                MockIssue::ResourceUnavailable,
            ));
        }
        if self
            .abandoned
            .contains(&(scope.into(), c.operation_id.clone()))
            || self
                .abandoned
                .contains(&(scope.into(), c.idempotency_key.clone()))
        {
            return Err(RuntimeRecoveryIssue::OperationAbandoned);
        }
        if self
            .blocked
            .contains(&(scope.into(), c.operation_id.clone()))
            || self
                .blocked
                .contains(&(scope.into(), c.idempotency_key.clone()))
        {
            return Err(RuntimeRecoveryIssue::RestartRequiresNewOperation);
        }
        Ok(())
    }
    fn pending(&mut self, scope: &str, c: &MockOperationContext) {
        self.pending.insert((
            scope.into(),
            c.operation_id.clone(),
            c.idempotency_key.clone(),
        ));
    }
    fn finished(&mut self, scope: &str, c: &MockOperationContext) {
        self.pending.remove(&(
            scope.into(),
            c.operation_id.clone(),
            c.idempotency_key.clone(),
        ));
    }
    fn apply(&mut self, command: &Command, user: &str) -> Result<()> {
        if let Command::PrepareSettings { reminder_rules, .. }
        | Command::ReserveScan { reminder_rules, .. } = command
        {
            match reminder_rules {
                Some(1) => self.current_reminder_rules = true,
                None if !self.current_reminder_rules => {}
                _ => return Err(RuntimeRecoveryIssue::InvalidJournal),
            }
        }
        // Historical behavior is scoped to this decoder call only. Public
        // reminder getters/clones must always enforce the current rules.
        self.reminders
            .set_legacy_replay_mock(!self.current_reminder_rules);
        let result = self.apply_historical_command(command, user);
        self.reminders.set_legacy_replay_mock(false);
        result
    }
    fn apply_historical_command(&mut self, command: &Command, user: &str) -> Result<()> {
        match command {
            Command::AbandonPending {
                position: _,
                scope,
                context,
                binding,
                now,
            } => {
                let scope = scope.as_str();
                pending_control::validate_abandon(self, user, scope, context, binding, now)?;
                self.guard(scope, context, user)?;
                self.finished(scope, context);
                self.abandoned
                    .insert((scope.into(), context.operation_id.clone()));
                self.abandoned
                    .insert((scope.into(), context.idempotency_key.clone()));
                if scope == "reminder" {
                    self.reminders.invalidate_operation_mock(
                        user,
                        &context.operation_id,
                        &context.idempotency_key,
                    );
                } else {
                    self.ledger
                        .drafts
                        .retain(|_, binding| binding.context.operation_id != context.operation_id);
                    self.ledger.latest_drafts.remove(&context.operation_id);
                    self.ledger
                        .intent_drafts
                        .retain(|_, binding| binding.context.operation_id != context.operation_id);
                    self.ledger
                        .latest_intent_drafts
                        .remove(&context.operation_id);
                }
            }
            Command::PrepareRegistration {
                context,
                snapshot,
                operation,
                index,
                expires,
                binding,
            } => {
                self.guard("ledger", operation, user)?;
                let actual = self
                    .ledger
                    .prepare_mock(context, snapshot, operation.clone(), *index, expires)
                    .map_err(RuntimeRecoveryIssue::Business)?;
                equal(value(&actual)?, binding)?;
                self.pending("ledger", operation);
            }
            Command::Register {
                draft,
                context,
                now,
                receipt,
            } => {
                self.guard("ledger", context, user)?;
                let actual = self
                    .ledger
                    .commit_mock(draft, context, now, &mut HistoricalGate)
                    .map_err(RuntimeRecoveryIssue::Business)?;
                equal(value(&actual)?, receipt)?;
                self.finished("ledger", context);
            }
            Command::PrepareIntent {
                context,
                id,
                intent,
                now,
                expires,
                binding,
            } => {
                self.guard("ledger", context, user)?;
                let actual = self
                    .ledger
                    .prepare_intent_mock(context.clone(), id, *intent, now, expires)
                    .map_err(RuntimeRecoveryIssue::Business)?;
                equal(value(&actual)?, binding)?;
                self.pending("ledger", context);
            }
            Command::Intent {
                draft,
                context,
                now,
                receipt,
            } => {
                self.guard("ledger", context, user)?;
                let actual = self
                    .ledger
                    .commit_intent_mock(draft, context, now, &mut HistoricalGate)
                    .map_err(RuntimeRecoveryIssue::Business)?;
                equal(value(&actual)?, receipt)?;
                self.finished("ledger", context);
            }
            Command::PrepareSettings {
                reminder_rules: _,
                context,
                id,
                schedule,
                now,
                expires,
                binding,
            } => {
                self.guard("reminder", context, user)?;
                let actual = self
                    .reminders
                    .prepare_mock(
                        &self.ledger,
                        context.clone(),
                        id,
                        schedule.schedule()?,
                        now,
                        expires,
                    )
                    .map_err(RuntimeRecoveryIssue::Reminder)?;
                equal(value(&actual)?, binding)?;
                self.settings.insert(context.operation_id.clone(), actual);
                self.pending("reminder", context);
            }
            Command::Settings {
                operation,
                binding,
                now,
                receipt,
            } => {
                let actual = self
                    .settings
                    .get(operation)
                    .cloned()
                    .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
                self.guard("reminder", &actual.context, user)?;
                equal(value(&actual)?, binding)?;
                let r = self
                    .reminders
                    .confirm_mock(&self.ledger, &actual, now, &mut HistoricalGate)
                    .map_err(RuntimeRecoveryIssue::Reminder)?;
                equal(value(&r)?, receipt)?;
                self.finished("reminder", &actual.context);
            }
            Command::ReserveScan {
                reminder_rules: _,
                now,
                messages,
                report,
            } => {
                if self.scan_base.is_some() {
                    return Err(RuntimeRecoveryIssue::Conflict);
                }
                let base = self.reminders.clone();
                let mut sink = ReserveSink::default();
                let actual = self
                    .reminders
                    .scan_mock(&self.ledger, user, now, &mut sink)
                    .map_err(RuntimeRecoveryIssue::Reminder)?;
                let actual_messages = sink
                    .messages
                    .iter()
                    .map(value)
                    .collect::<Result<Vec<_>>>()?;
                if &actual_messages != messages {
                    return Err(RuntimeRecoveryIssue::InvalidJournal);
                }
                equal(value(&actual)?, report)?;
                self.scan_base = Some((now.clone(), base, messages.len()));
            }
            Command::ResolveScan {
                now,
                outcomes,
                report,
            } => {
                let (reserved, mut base, count) = self
                    .scan_base
                    .take()
                    .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
                if &reserved != now || count != outcomes.len() {
                    return Err(RuntimeRecoveryIssue::InvalidJournal);
                }
                let mut sink = RecordedSink {
                    outcomes: outcomes.clone(),
                    index: 0,
                };
                let actual = base
                    .scan_mock(&self.ledger, user, now, &mut sink)
                    .map_err(RuntimeRecoveryIssue::Reminder)?;
                if sink.index != count {
                    return Err(RuntimeRecoveryIssue::InvalidJournal);
                }
                equal(value(&actual)?, report)?;
                self.reminders = base;
            }
            Command::Restart => {
                for (scope, op, key) in std::mem::take(&mut self.pending) {
                    self.blocked.insert((scope.clone(), op));
                    self.blocked.insert((scope, key));
                }
                self.ledger.drafts.clear();
                self.ledger.latest_drafts.clear();
                self.ledger.intent_drafts.clear();
                self.ledger.latest_intent_drafts.clear();
                self.reminders.invalidate_pending_for_restart_mock();
                // A reserved but unacknowledged attempt remains Unknown. Startup
                // never calls the external sink and never guesses its outcome.
                self.scan_base = None;
            }
        }
        Ok(())
    }
}

pub struct SubscriptionRuntimeMock {
    pool: Option<PgPool>,
    user: String,
    version: i64,
    journal: Journal,
    state: State,
    staged: bool,
}
impl SubscriptionRuntimeMock {
    async fn dedicated(pool: &PgPool) -> Result<()> {
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(pool)
            .await
            .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        if database != "safeflow_subscription_mock_test" {
            return Err(RuntimeRecoveryIssue::DedicatedDatabaseRequired);
        }
        Ok(())
    }
    pub async fn initialize_local_schema(pool: &PgPool) -> Result<()> {
        Self::dedicated(pool).await?;
        sqlx::raw_sql(include_str!(
            "../../sql/runtime_recovery_mock_v1.proposed.sql"
        ))
        .execute(pool)
        .await
        .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        Ok(())
    }
    /// Explicit first-time fixture setup. Never overwrites an existing journal.
    pub async fn create_fixture_mock(
        pool: PgPool,
        user: &str,
        initial: LedgerFixture,
    ) -> Result<Self> {
        Self::dedicated(&pool).await?;
        let state = State::initial(initial.clone(), user)?;
        let journal = Journal {
            schema_version: 1,
            rule_version: crate::recognition::LOCAL_RULE_VERSION.into(),
            fictional: true,
            production_registered: false,
            initial,
            commands: vec![],
        };
        let count=sqlx::query("INSERT INTO safeflow_subscription_mock_v1.runtime_recovery (owner_id,version,journal) VALUES ($1,1,$2) ON CONFLICT (owner_id) DO NOTHING")
            .bind(user).bind(value(&journal)?).execute(&pool).await.map_err(|_|RuntimeRecoveryIssue::Unavailable)?.rows_affected();
        if count != 1 {
            return Err(RuntimeRecoveryIssue::Conflict);
        }
        Ok(Self {
            pool: Some(pool),
            user: user.into(),
            version: 1,
            journal,
            state,
            staged: false,
        })
    }
    /// Standalone subscription startup: validate/load all saved facts, invalidate
    /// pending confirmations, preserve uncertain reminders, then expose the runtime.
    pub async fn start_mock(pool: PgPool, user: &str) -> Result<Self> {
        Self::dedicated(&pool).await?;
        if user.trim().is_empty() {
            return Err(RuntimeRecoveryIssue::InvalidJournal);
        }
        let row:Option<(i64,Value)>=sqlx::query_as("SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
            .bind(user).fetch_optional(&pool).await.map_err(|_|RuntimeRecoveryIssue::Unavailable)?;
        let (version, v) = row.ok_or(RuntimeRecoveryIssue::NotFound)?;
        let (journal, state) = decode_journal(user, version, v)?;
        let mut runtime = Self {
            pool: Some(pool),
            user: user.into(),
            version,
            journal,
            state,
            staged: false,
        };
        runtime.append(Command::Restart).await?;
        Ok(runtime)
    }
    async fn append(&mut self, command: Command) -> Result<()> {
        pending_control::validate_reference(&self.journal.commands, &command)?;
        let mut state = self.state.clone();
        state.apply(&command, &self.user)?;
        let mut journal = self.journal.clone();
        journal.commands.push(command);
        let next = self
            .version
            .checked_add(1)
            .ok_or(RuntimeRecoveryIssue::Conflict)?;
        if !self.staged {
            let mut connection = self
                .pool
                .as_ref()
                .ok_or(RuntimeRecoveryIssue::StagedOperationUnsupported)?
                .acquire()
                .await
                .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
            write_journal_on_connection(&mut connection, &self.user, self.version, next, &journal)
                .await?;
        }
        self.version = next;
        self.journal = journal;
        self.state = state;
        Ok(())
    }
    /// Isolated adapter increment: the caller owns the transaction. Only the
    /// supplied connection writes; the live runtime and business output stay hidden
    /// until commit is independently observed. No public permit is issued here.
    pub async fn stage_on_connection_mock<T, F>(
        &self,
        connection: &mut PgConnection,
        work: F,
    ) -> Result<PendingRuntimeWriteMock<T>>
    where
        F: for<'a> FnOnce(&'a mut Self) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>,
    {
        if self.staged {
            return Err(RuntimeRecoveryIssue::StagedOperationUnsupported);
        }
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        if database != "safeflow_subscription_mock_test" {
            return Err(RuntimeRecoveryIssue::DedicatedDatabaseRequired);
        }
        let mut candidate = Self {
            pool: self.pool.clone(),
            user: self.user.clone(),
            version: self.version,
            journal: self.journal.clone(),
            state: self.state.clone(),
            staged: true,
        };
        let output = work(&mut candidate).await?;
        write_journal_on_connection(
            connection,
            &self.user,
            self.version,
            candidate.version,
            &candidate.journal,
        )
        .await?;
        Ok(PendingRuntimeWriteMock {
            base_version: self.version,
            base_journal: value(&self.journal)?,
            candidate,
            output,
        })
    }
    pub fn ledger_mock(&self) -> &MemoryConfirmationMock {
        &self.state.ledger
    }
    pub fn reminders_mock(&self) -> &MemoryReminderMock {
        &self.state.reminders
    }
    pub async fn prepare_registration_mock(
        &mut self,
        context: &MockContext,
        snapshot: &Snapshot,
        operation: MockOperationContext,
        index: usize,
        expires: &str,
    ) -> Result<MockConfirmationBinding> {
        self.state.guard("ledger", &operation, &self.user)?;
        let b = self
            .state
            .ledger
            .clone()
            .prepare_mock(context, snapshot, operation.clone(), index, expires)
            .map_err(RuntimeRecoveryIssue::Business)?;
        self.append(Command::PrepareRegistration {
            context: context.clone(),
            snapshot: Box::new(snapshot.clone()),
            operation,
            index,
            expires: expires.into(),
            binding: value(&b)?,
        })
        .await?;
        Ok(b)
    }
    pub async fn register_mock(
        &mut self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockGateVerifier,
    ) -> Result<MockConfirmationReceipt> {
        self.state.guard("ledger", context, &self.user)?;
        let r = self
            .state
            .ledger
            .clone()
            .commit_mock(draft, context, now, gate)
            .map_err(RuntimeRecoveryIssue::Business)?;
        self.append(Command::Register {
            draft: draft.into(),
            context: context.clone(),
            now: now.into(),
            receipt: value(&r)?,
        })
        .await?;
        Ok(r)
    }
    pub async fn prepare_intent_mock(
        &mut self,
        context: MockOperationContext,
        id: &str,
        intent: MockIntentKind,
        now: &str,
        expires: &str,
    ) -> Result<MockIntentBinding> {
        self.state.guard("ledger", &context, &self.user)?;
        let b = self
            .state
            .ledger
            .clone()
            .prepare_intent_mock(context.clone(), id, intent, now, expires)
            .map_err(RuntimeRecoveryIssue::Business)?;
        self.append(Command::PrepareIntent {
            context,
            id: id.into(),
            intent,
            now: now.into(),
            expires: expires.into(),
            binding: value(&b)?,
        })
        .await?;
        Ok(b)
    }
    pub async fn commit_intent_mock(
        &mut self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockIntentGateVerifier,
    ) -> Result<MockIntentReceipt> {
        self.state.guard("ledger", context, &self.user)?;
        let r = self
            .state
            .ledger
            .clone()
            .commit_intent_mock(draft, context, now, gate)
            .map_err(RuntimeRecoveryIssue::Business)?;
        self.append(Command::Intent {
            draft: draft.into(),
            context: context.clone(),
            now: now.into(),
            receipt: value(&r)?,
        })
        .await?;
        Ok(r)
    }
    pub async fn prepare_settings_mock(
        &mut self,
        context: MockOperationContext,
        id: &str,
        schedule: Schedule,
        now: &str,
        expires: &str,
    ) -> Result<ScheduleBinding> {
        self.state.guard("reminder", &context, &self.user)?;
        let b = self
            .state
            .reminders
            .with_current_rules_mock()
            .prepare_mock(
                &self.state.ledger,
                context.clone(),
                id,
                schedule.clone(),
                now,
                expires,
            )
            .map_err(RuntimeRecoveryIssue::Reminder)?;
        self.append(Command::PrepareSettings {
            reminder_rules: Some(1),
            context,
            id: id.into(),
            schedule: StoredSchedule::from(&schedule),
            now: now.into(),
            expires: expires.into(),
            binding: value(&b)?,
        })
        .await?;
        Ok(b)
    }
    pub async fn confirm_settings_mock(
        &mut self,
        binding: &ScheduleBinding,
        now: &str,
        gate: &mut impl MockScheduleGate,
    ) -> Result<ScheduleReceipt> {
        self.state.guard("reminder", &binding.context, &self.user)?;
        let r = self
            .state
            .reminders
            .with_current_rules_mock()
            .confirm_mock(&self.state.ledger, binding, now, gate)
            .map_err(RuntimeRecoveryIssue::Reminder)?;
        self.append(Command::Settings {
            operation: binding.context.operation_id.clone(),
            binding: value(binding)?,
            now: now.into(),
            receipt: value(&r)?,
        })
        .await?;
        Ok(r)
    }
    pub async fn scan_mock(
        &mut self,
        now: &str,
        sink: &mut impl MockInAppSink,
    ) -> Result<DeliveryReport> {
        if self.staged {
            return Err(RuntimeRecoveryIssue::StagedOperationUnsupported);
        }
        let mut reserve = ReserveSink::default();
        let report = self
            .state
            .reminders
            .with_current_rules_mock()
            .scan_mock(&self.state.ledger, &self.user, now, &mut reserve)
            .map_err(RuntimeRecoveryIssue::Reminder)?;
        self.append(Command::ReserveScan {
            reminder_rules: Some(1),
            now: now.into(),
            messages: reserve
                .messages
                .iter()
                .map(value)
                .collect::<Result<Vec<_>>>()?,
            report: value(&report)?,
        })
        .await?;
        let outcomes: Vec<_> = reserve
            .messages
            .iter()
            .map(|m| sink.simulate_in_app(m))
            .collect();
        let mut fixed = RecordedSink {
            outcomes: outcomes.clone(),
            index: 0,
        };
        let mut base = self
            .state
            .scan_base
            .as_ref()
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?
            .1
            .clone();
        let mut report = base
            .scan_mock(&self.state.ledger, &self.user, now, &mut fixed)
            .map_err(RuntimeRecoveryIssue::Reminder)?;
        self.append(Command::ResolveScan {
            now: now.into(),
            outcomes,
            report: value(&report)?,
        })
        .await?;
        report.database_connected = true;
        Ok(report)
    }
}

/// A staged SQL write is not a successful business receipt. Dropping this value
/// never commits the caller's transaction or publishes runtime state.
#[must_use]
pub struct PendingRuntimeWriteMock<T> {
    base_version: i64,
    base_journal: Value,
    candidate: SubscriptionRuntimeMock,
    output: T,
}
impl<T> PendingRuntimeWriteMock<T> {
    /// Call after the caller has successfully committed. A separate pool read
    /// verifies committed facts; an open/rolled-back transaction cannot publish.
    pub async fn publish_after_commit_mock(
        self,
        runtime: &mut SubscriptionRuntimeMock,
    ) -> Result<T> {
        if runtime.staged
            || runtime.user != self.candidate.user
            || runtime.version != self.base_version
            || value(&runtime.journal)? != self.base_journal
        {
            return Err(RuntimeRecoveryIssue::Conflict);
        }
        let row: Option<(i64, Value)> = sqlx::query_as(
            "SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
            .bind(&runtime.user).fetch_optional(runtime.pool.as_ref().ok_or(RuntimeRecoveryIssue::StagedOperationUnsupported)?).await
            .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        let expected = value(&self.candidate.journal)?;
        if row != Some((self.candidate.version, expected)) {
            return Err(RuntimeRecoveryIssue::CommitNotObserved);
        }
        runtime.version = self.candidate.version;
        runtime.journal = self.candidate.journal;
        runtime.state = self.candidate.state;
        Ok(self.output)
    }
}
async fn write_journal_on_connection(
    connection: &mut PgConnection,
    user: &str,
    previous: i64,
    next: i64,
    journal: &Journal,
) -> Result<()> {
    let count = sqlx::query("UPDATE safeflow_subscription_mock_v1.runtime_recovery SET version=$1,journal=$2 WHERE owner_id=$3 AND version=$4")
        .bind(next).bind(value(journal)?).bind(user).bind(previous)
        .execute(connection).await.map_err(|_| RuntimeRecoveryIssue::Unavailable)?.rows_affected();
    if count != 1 {
        return Err(RuntimeRecoveryIssue::Conflict);
    }
    Ok(())
}

fn decode_journal(user: &str, version: i64, v: Value) -> Result<(Journal, State)> {
    let journal: Journal =
        serde_json::from_value(v).map_err(|_| RuntimeRecoveryIssue::InvalidJournal)?;
    if journal.schema_version != 1
        || journal.rule_version != crate::recognition::LOCAL_RULE_VERSION
        || !journal.fictional
        || journal.production_registered
        || version <= 0
        || u64::try_from(version).ok() != (journal.commands.len() as u64).checked_add(1)
    {
        return Err(RuntimeRecoveryIssue::InvalidJournal);
    }
    let mut state = State::initial(journal.initial.clone(), user)
        .map_err(|_| RuntimeRecoveryIssue::InvalidJournal)?;
    for (position, c) in journal.commands.iter().enumerate() {
        pending_control::validate_reference(&journal.commands[..position], c)
            .map_err(|_| RuntimeRecoveryIssue::InvalidJournal)?;
        state
            .apply(c, user)
            .map_err(|_| RuntimeRecoveryIssue::InvalidJournal)?;
    }
    Ok((journal, state))
}

/// Immutable snapshot read on a caller-supplied connection. Reading does not
/// append Restart, invalidate confirmations, send reminders, or acquire a pool.
pub struct SubscriptionReadSnapshotMock {
    owner: String,
    version: i64,
    state: State,
    business_records: Vec<RawBusinessRecordMock>,
    journal: Journal,
}
impl SubscriptionReadSnapshotMock {
    /// The caller supplies a trusted owner scope and controls transaction/read
    /// isolation. This mock adapter does not authenticate an arbitrary user ID.
    pub async fn load_on_connection_mock(
        connection: &mut PgConnection,
        user: &str,
    ) -> Result<Self> {
        if user.trim().is_empty() {
            return Err(RuntimeRecoveryIssue::InvalidJournal);
        }
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        if database != "safeflow_subscription_mock_test" {
            return Err(RuntimeRecoveryIssue::DedicatedDatabaseRequired);
        }
        let row: Option<(i64, Value)> = sqlx::query_as(
            "SELECT version,journal FROM safeflow_subscription_mock_v1.runtime_recovery WHERE owner_id=$1")
            .bind(user).fetch_optional(&mut *connection).await
            .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        let (version, journal) = row.ok_or(RuntimeRecoveryIssue::NotFound)?;
        Self::from_stored_journal_mock(user, version, journal)
    }
    fn from_stored_journal_mock(user: &str, version: i64, journal: Value) -> Result<Self> {
        let (journal, state) = decode_journal(user, version, journal)?;
        let business_records = collect_business_records(&journal)?;
        Ok(Self {
            owner: user.into(),
            version,
            state,
            business_records,
            journal,
        })
    }
    /// Trusted-backend evidence only. No C-AUD mapping, event ID, ACK or send.
    pub fn business_records_mock(&self) -> &[RawBusinessRecordMock] {
        &self.business_records
    }
    pub fn version_mock(&self) -> i64 {
        self.version
    }
    pub fn ledger_mock(&self) -> &MemoryConfirmationMock {
        &self.state.ledger
    }
    pub fn reminders_mock(&self) -> &MemoryReminderMock {
        &self.state.reminders
    }
}

/// Original, validated fictional journal facts. Internal bindings may contain
/// sensitive references: do not expose this backend export to a user/LLM tool.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawBusinessRecordMock {
    source_command_position: usize,
    original_binding: Value,
    original_command: Value,
}
fn collect_business_records(journal: &Journal) -> Result<Vec<RawBusinessRecordMock>> {
    use std::collections::BTreeMap;
    let mut registrations = BTreeMap::new();
    let mut intents = BTreeMap::new();
    let mut settings = BTreeMap::new();
    let mut seen: BTreeMap<(String, String), (Value, Value)> = BTreeMap::new();
    let mut records = Vec::new();
    for (position, command) in journal.commands.iter().enumerate() {
        let (scope, binding, receipt) = match command {
            Command::PrepareRegistration { binding, .. } => {
                let draft = binding["draft_id"]
                    .as_str()
                    .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
                registrations.insert(draft.to_owned(), binding.clone());
                continue;
            }
            Command::PrepareIntent { binding, .. } => {
                let draft = binding["draft_id"]
                    .as_str()
                    .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
                intents.insert(draft.to_owned(), binding.clone());
                continue;
            }
            Command::PrepareSettings {
                reminder_rules: _,
                context,
                binding,
                ..
            } => {
                settings.insert(context.operation_id.clone(), binding.clone());
                continue;
            }
            Command::Register { draft, receipt, .. } => {
                ("ledger", registrations.get(draft), receipt)
            }
            Command::Intent { draft, receipt, .. } => ("ledger", intents.get(draft), receipt),
            Command::Settings {
                operation, receipt, ..
            } => ("reminder", settings.get(operation), receipt),
            // Preparations, Restart, reservation/scan-with-no-change and delivery
            // simulations are not invented as new successful business events.
            _ => continue,
        };
        let binding = binding.ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
        let context = &binding["context"];
        let key = context["idempotency_key"]
            .as_str()
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
        let identity = (scope.to_owned(), key.to_owned());
        if let Some((old_binding, old_receipt)) = seen.get(&identity) {
            if old_binding != binding || old_receipt != receipt {
                return Err(RuntimeRecoveryIssue::InvalidJournal);
            }
            continue;
        }
        seen.insert(identity, (binding.clone(), receipt.clone()));
        records.push(RawBusinessRecordMock {
            source_command_position: position,
            original_binding: binding.clone(),
            original_command: value(command)?,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod raw_records_tests {
    use super::*;
    use crate::reminder_mock::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    const EXPIRY: &str = "2026-10-14T12:05:00+08:00";
    struct FixtureGate;
    impl MockGateVerifier for FixtureGate {
        fn verify_and_consume_mock_confirmation(
            &mut self,
            _: &MockConfirmationBinding,
            _: &str,
        ) -> MockGateOutcome {
            MockGateOutcome::Confirmed
        }
    }
    impl MockIntentGateVerifier for FixtureGate {
        fn verify_and_consume_mock_intent(
            &mut self,
            _: &MockIntentBinding,
            _: &str,
        ) -> MockGateOutcome {
            MockGateOutcome::Confirmed
        }
    }
    impl MockScheduleGate for FixtureGate {
        fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
            MockGateOutcome::Confirmed
        }
    }
    fn context(id: &str) -> MockOperationContext {
        MockOperationContext {
            user_id: "user_demo".into(),
            session_ref: "fictional-session".into(),
            task_id: "fictional-task".into(),
            node_id: id.into(),
            operation_id: id.into(),
            idempotency_key: format!("fictional-key-{id}"),
            plan_version: 1,
        }
    }
    pub(super) fn actual_fixture_journal() -> Journal {
        let fixture = LedgerFixture {
            schema_version: 1,
            fictional: true,
            production_registered: false,
            owner_user_id: "user_demo".into(),
            snapshot_id: Some("fictional-raw-records".into()),
            read: crate::ReadState {
                state: crate::ReadKind::Complete,
                is_complete: true,
                next_cursor: None,
                reason: None,
            },
            entries: vec![],
        };
        let sample = crate::billing_query_mock::load_case(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
            )),
            "complete",
            MockContext {
                user_id: "user_demo".into(),
                requested_account_ids: vec!["account_001".into()],
            },
        )
        .unwrap();
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture.clone());
        let binding = ledger
            .prepare_mock(
                &sample.trusted_context_fixture,
                &sample.snapshot,
                context("register"),
                0,
                EXPIRY,
            )
            .unwrap();
        let mut commands = vec![Command::PrepareRegistration {
            context: sample.trusted_context_fixture,
            snapshot: Box::new(sample.snapshot),
            operation: binding.context.clone(),
            index: 0,
            expires: EXPIRY.into(),
            binding: value(&binding).unwrap(),
        }];
        let receipt = ledger
            .commit_mock(&binding.draft_id, &binding.context, NOW, &mut FixtureGate)
            .unwrap();
        commands.push(Command::Register {
            draft: binding.draft_id,
            context: binding.context,
            now: NOW.into(),
            receipt: value(&receipt).unwrap(),
        });
        let mut reminders = MemoryReminderMock::default();
        let schedule = Schedule {
            cycle: crate::recognition::Cycle::CalendarMonth,
            anchor_date: NaiveDate::from_ymd_opt(2026, 9, 17).unwrap(),
            period_index: 1,
            expected_date: NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
            enabled: true,
            days_before: 3,
        };
        let settings = reminders
            .prepare_mock(
                &ledger,
                context("settings"),
                &receipt.subscription_id,
                schedule.clone(),
                NOW,
                EXPIRY,
            )
            .unwrap();
        commands.push(Command::PrepareSettings {
            reminder_rules: None,
            context: settings.context.clone(),
            id: receipt.subscription_id.clone(),
            schedule: StoredSchedule::from(&schedule),
            now: NOW.into(),
            expires: EXPIRY.into(),
            binding: value(&settings).unwrap(),
        });
        let result = reminders
            .confirm_mock(&ledger, &settings, NOW, &mut FixtureGate)
            .unwrap();
        commands.push(Command::Settings {
            operation: settings.context.operation_id.clone(),
            binding: value(&settings).unwrap(),
            now: NOW.into(),
            receipt: value(&result).unwrap(),
        });
        for (id, kind) in [
            ("cancel", MockIntentKind::Record),
            ("revoke", MockIntentKind::Revoke),
        ] {
            let binding = ledger
                .prepare_intent_mock(context(id), &receipt.subscription_id, kind, NOW, EXPIRY)
                .unwrap();
            commands.push(Command::PrepareIntent {
                context: binding.context.clone(),
                id: receipt.subscription_id.clone(),
                intent: kind,
                now: NOW.into(),
                expires: EXPIRY.into(),
                binding: value(&binding).unwrap(),
            });
            let result = ledger
                .commit_intent_mock(&binding.draft_id, &binding.context, NOW, &mut FixtureGate)
                .unwrap();
            commands.push(Command::Intent {
                draft: binding.draft_id,
                context: binding.context,
                now: NOW.into(),
                receipt: value(&result).unwrap(),
            });
        }
        Journal {
            schema_version: 1,
            rule_version: crate::recognition::LOCAL_RULE_VERSION.into(),
            fictional: true,
            production_registered: false,
            initial: fixture,
            commands,
        }
    }
    fn checked(journal: &Journal) -> Result<Vec<RawBusinessRecordMock>> {
        let (validated, _) = decode_journal(
            "user_demo",
            journal.commands.len() as i64 + 1,
            value(journal)?,
        )?;
        collect_business_records(&validated)
    }
    #[test]
    fn exports_four_actual_business_receipts_with_original_bindings() {
        let journal = actual_fixture_journal();
        let records = checked(&journal).unwrap();
        assert_eq!(records.len(), 4);
        for (record, index) in records.iter().zip([1, 3, 5, 7]) {
            assert_eq!(record.source_command_position, index);
            assert_eq!(
                record.original_command,
                value(&journal.commands[index]).unwrap()
            );
            assert!(!record.original_command["receipt"]["notification_sent"]
                .as_bool()
                .unwrap());
        }
        assert_eq!(records[2].original_binding["kind"], "RECORD");
        assert_eq!(records[3].original_binding["kind"], "REVOKE");
        println!(
            "BUSINESS_RECORDS_FIXTURE_JSON:{}",
            serde_json::json!({"fictional":true,"source":"actual_in_memory_fixture_replay",
            "database_executed":false,"audit_integrated":false,"notification_sent":false,"records":records})
        );
    }
    #[test]
    fn replay_and_restart_do_not_export_duplicate_business_events() {
        let mut journal = actual_fixture_journal();
        let original = checked(&journal).unwrap();
        journal.commands.push(Command::Restart);
        journal.commands.push(journal.commands[1].clone());
        assert_eq!(checked(&journal).unwrap(), original);
    }
    #[test]
    fn preparations_alone_do_not_export_success() {
        let mut journal = actual_fixture_journal();
        journal.commands.truncate(1);
        assert!(checked(&journal).unwrap().is_empty());
    }
    #[test]
    fn forged_receipt_or_foreign_owner_is_rejected_before_export() {
        let mut journal = actual_fixture_journal();
        if let Command::Register { receipt, .. } = &mut journal.commands[1] {
            receipt["notification_sent"] = Value::Bool(true);
        }
        assert!(matches!(
            checked(&journal),
            Err(RuntimeRecoveryIssue::InvalidJournal)
        ));
        let mut journal = actual_fixture_journal();
        journal.initial.owner_user_id = "foreign".into();
        assert!(matches!(
            checked(&journal),
            Err(RuntimeRecoveryIssue::InvalidJournal)
        ));
    }
}

mod current_queries;
mod reminder_queries;
pub use reminder_queries::{
    OriginalReminderRequestMock, OriginalReminderResultMock, ReminderBatchStateMock,
    ReminderHistoryItemMock, ReminderQueryItemViewMock, ReminderQueryViewMock,
    ReminderReadQueryMock, ReminderReviewItemMock,
};
mod discovery_queries;
mod pending_control;
mod preview_queries;
mod queries;
pub use current_queries::{
    CancellationHistoryItemMock, CancellationStatusViewMock, SubscriptionListItemMock,
    SubscriptionListViewMock,
};
pub use discovery_queries::{
    CandidateViewMock, DateSuggestionViewMock, DiscoveryViewMock, ReminderPreparationMock,
};
pub use pending_control::{
    AbandonPendingReceiptMock, PendingConfirmationItemMock, PendingConfirmationsMock,
    ReselectionResultMock,
};
pub use queries::{
    BusinessRecordCursorMock, BusinessRecordPageMock, OriginalLookupAttemptMock,
    OriginalOperationRequestMock, OriginalOperationViewMock, OriginalResultMock,
    ReadOnlyWriteRecoveryMock, SubscriptionWriteCompletionMock, SubscriptionWriteHandoffMock,
};

mod proposed_ledger_read;
pub use proposed_ledger_read::{
    ProposedLedgerDiscoveryMock, ProposedLedgerReadIssue, ProposedLedgerReaderMock,
    FICTIONAL_JOURNAL_ENCODING_V1,
};

mod proposed_ledger_write;
pub use proposed_ledger_write::{
    shared_query_error_view_mock, CommittedReminderBatchMock, DueReminderPreviewMock,
    LedgerInitializationPreviewMock, LedgerInitializationRequestMock,
    PendingProposedLedgerWriteMock, PreparedDiscoveryRegistrationMock,
    PreparedSubscriptionChangeMock, ProposedLedgerStoreMock, ProposedLedgerWorkMock,
    ProposedLedgerWriteIssue, SavedDiscoverySelectionMock, SharedBusinessReadQueryMock,
    SharedBusinessRecordCursorMock, SharedBusinessRecordPageMock, SharedSubscriptionReadQueryMock,
    SharedSubscriptionWriteHandoffMock, SharedWriteCompletionViewMock, SimulatedReminderBatchMock,
    SubscriptionChangeBindingMock, SubscriptionChangeRequestMock,
};

#[cfg(test)]
#[path = "runtime_recovery/reminder_policy_tests.rs"]
mod reminder_policy_tests;
