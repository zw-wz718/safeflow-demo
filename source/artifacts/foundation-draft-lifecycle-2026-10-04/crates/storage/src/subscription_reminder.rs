//! Explicit current-session local mock scheduler. Never a transport or daemon.
use crate::{
    subscription::write_error, PgRepository, StorageError, SubscriptionHost, SubscriptionService,
};
use safeflow_bank_gateway::BankGateway;
use safeflow_contracts::{ResourceId, SessionRef, UserId};
use safeflow_runtime::SessionBinding;
use safeflow_subscription_evidence::reminder_mock::*;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
mod persistent;

/// Trusted operator configuration only; no request/Tool DTO, no defaults.
#[derive(Clone, Debug)]
pub struct SubscriptionReminderConfig {
    pub owner: UserId,
    pub accounts: Vec<ResourceId>,
    pub max_messages: usize,
    pub max_ledger_entries: usize,
    pub scan_timeout: Duration,
    pub cycles: usize,
    pub interval: Duration,
}
impl SubscriptionReminderConfig {
    pub fn validate(&self) -> Result<(), StorageError> {
        let mut accounts = self.accounts.clone();
        accounts.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        accounts.dedup();
        if accounts.is_empty()
            || accounts.len() > 64
            || accounts.len() != self.accounts.len()
            || !(1..=128).contains(&self.max_messages)
            || !(1..=4096).contains(&self.max_ledger_entries)
            || self.scan_timeout < Duration::from_millis(100)
            || self.scan_timeout > Duration::from_secs(30)
            || !(1..=64).contains(&self.cycles)
            || self.interval < Duration::from_secs(1)
            || self.interval > Duration::from_secs(60)
        {
            return Err(StorageError::Invalid(
                "explicit bounded reminder configuration",
            ));
        }
        Ok(())
    }
}
#[derive(Default)]
struct CancellationState {
    stopped: AtomicBool,
    signal: tokio::sync::Notify,
}
#[derive(Clone, Default)]
pub struct SubscriptionReminderStop(Arc<CancellationState>);
impl SubscriptionReminderStop {
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
        self.0.signal.notify_waiters();
    }
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::SeqCst)
    }
    pub async fn cancelled(&self) {
        loop {
            let signal = self.0.signal.notified();
            tokio::pin!(signal);
            signal.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            signal.await;
        }
    }
}
/// Stage flags refer only to this scan. Original member views retain all states.
#[derive(Debug, Serialize)]
pub struct SubscriptionReminderScan {
    pub fictional: bool,
    pub notification_sent: bool,
    pub production_registered: bool,
    pub scanned: bool,
    pub reserved: bool,
    pub simulated: bool,
    pub result_saved: bool,
    pub result_commit_acknowledged: bool,
    pub due_count: usize,
    pub simulated_count: usize,
    pub trusted_now: String,
    pub original_due_report: Value,
    pub original_saved_plans: Vec<Value>,
    pub original_attempt_refs: Vec<String>,
}
pub struct SubscriptionReminderHost {
    service: SubscriptionService,
    session: SessionRef,
    identity: SessionBinding,
    host: SubscriptionHost,
    bank: Arc<dyn BankGateway>,
    config: SubscriptionReminderConfig,
    database_scope: Value,
    public_scope: Option<crate::SubscriptionPublicScope>,
    #[cfg(test)]
    probe: tests::Probe,
}
async fn database_scope(c: &mut PgConnection) -> Result<Value, StorageError> {
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('database',current_database(),'database_oid',(SELECT oid::bigint FROM pg_database WHERE datname=current_database()),'schema',current_schema(),'schema_oid',(SELECT oid::bigint FROM pg_namespace WHERE nspname=current_schema()),'address',inet_server_addr()::text,'port',inet_server_port())").fetch_one(c).await?)
}
impl SubscriptionService {
    /// Uses an EXISTING host generation and current identity. Does not startup,
    /// initialize, change settings, mint a background identity or register a tool.
    pub async fn reminder_host_for_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        bank: Arc<dyn BankGateway>,
        config: SubscriptionReminderConfig,
    ) -> Result<SubscriptionReminderHost, StorageError> {
        config.validate()?;
        let identity = self.identity(session)?;
        if identity.user_id != config.owner
            || host.owner != identity.user_id.as_str()
            || host.namespace != self.repository.namespace
        {
            return Err(StorageError::Session);
        }
        tokio::time::timeout(config.scan_timeout, async {
            // Existing check_host locks its row; the verification pool is
            // deliberately read-only and must stay so. Binding uses writer.
            let mut c = self.repository.pool.acquire().await?;
            self.check(session, &identity)?;
            let scope = database_scope(&mut c).await?;
            self.check(session, &identity)?;
            self.check_host(&mut c, host, identity.user_id.as_str())
                .await?;
            self.check(session, &identity)?;
            Ok(SubscriptionReminderHost {
                service: self.clone(),
                session: session.clone(),
                identity,
                host: host.clone(),
                bank,
                config,
                database_scope: scope,
                public_scope: None,
                #[cfg(test)]
                probe: tests::Probe::default(),
            })
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)?
    }
}
impl SubscriptionReminderHost {
    fn check(
        &self,
        stop: &SubscriptionReminderStop,
        deadline: Instant,
    ) -> Result<(), StorageError> {
        if stop.is_stopped() || Instant::now() >= deadline {
            return Err(StorageError::ReconcileRequired);
        }
        self.service.check(&self.session, &self.identity)
    }
    async fn fence(
        &self,
        c: &mut PgConnection,
        stop: &SubscriptionReminderStop,
        deadline: Instant,
    ) -> Result<(), StorageError> {
        self.check(stop, deadline)?;
        let actual = database_scope(c).await?;
        self.check(stop, deadline)?;
        if actual != self.database_scope {
            return Err(StorageError::TransactionScope);
        }
        self.service
            .check_host(c, &self.host, self.identity.user_id.as_str())
            .await?;
        self.check(stop, deadline)?;
        if let Some(scope)=&self.public_scope {
            scope.check_reminder_database(&self.session,c).await?;
            self.check(stop,deadline)?;
        }
        for account in &self.config.accounts {
            let result = self
                .bank
                .get_account_balance(&self.identity.user_id, account)
                .await;
            self.check(stop, deadline)?;
            result
                .map_err(|_| StorageError::NotFound)?
                .validate_for(account)
                .map_err(|_| StorageError::CorruptRecord)?;
        }
        Ok(())
    }
    pub async fn scan_once(
        &self,
        stop: &SubscriptionReminderStop,
    ) -> Result<SubscriptionReminderScan, StorageError> {
        if self.public_scope.is_some() {
            let mut sink=persistent::PersistentSink::new(self,stop);
            self.scan_with_sink(stop,&mut sink).await
        } else { self.scan_with_sink(stop, &mut FixedSimulation).await }
    }
    /// Finite, explicit invocation. Cancellation also interrupts the interval.
    /// A failure ends the run; it never automatically retries a reserved attempt.
    pub async fn run(
        &self,
        stop: &SubscriptionReminderStop,
    ) -> Result<Vec<SubscriptionReminderScan>, StorageError> {
        let mut reports = Vec::with_capacity(self.config.cycles);
        for i in 0..self.config.cycles {
            if i != 0 {
                tokio::select! {biased; _=stop.cancelled()=>return Err(StorageError::ReconcileRequired),
                _=tokio::time::sleep(self.config.interval)=>{} }
            }
            reports.push(self.scan_once(stop).await?);
        }
        self.check(stop, Instant::now() + self.config.scan_timeout)?;
        Ok(reports)
    }
    async fn scan_with_sink(
        &self,
        stop: &SubscriptionReminderStop,
        sink: &mut impl MockInAppSink,
    ) -> Result<SubscriptionReminderScan, StorageError> {
        let deadline = Instant::now() + self.config.scan_timeout;
        tokio::select! {biased;
            _=stop.cancelled()=>Err(StorageError::ReconcileRequired),
            r=tokio::time::timeout(self.config.scan_timeout,self.scan_inner(stop,deadline,sink))=>r.map_err(|_|StorageError::ReconcileRequired)?,
        }
    }
    async fn scan_inner(
        &self,
        stop: &SubscriptionReminderStop,
        deadline: Instant,
        sink: &mut impl MockInAppSink,
    ) -> Result<SubscriptionReminderScan, StorageError> {
        self.check(stop, deadline)?;
        let repository: &PgRepository = &self.service.repository;
        let mut c = repository.pool.acquire().await?;
        // Session locks MUST never go back into the pool, including cancellation
        // during lock acquisition. Pool's permit remains bounded until close.
        c.close_on_drop();
        self.check(stop, deadline)?;
        sqlx::query("SET statement_timeout='4000ms'")
            .execute(&mut *c)
            .await?;
        self.check(stop, deadline)?;
        // Same installation -> owner order/key as existing public writers.
        sqlx::query("SELECT pg_advisory_lock_shared(hashtextextended(current_schema()||'billing-recovery-install-v2',0))").execute(&mut *c).await?;
        self.check(stop, deadline)?;
        sqlx::query("SELECT pg_advisory_lock(hashtextextended(current_schema() || $1,0))")
            .bind(json!(["public-owner-execution-v1", self.identity.user_id]).to_string())
            .execute(&mut *c)
            .await?;
        self.check(stop, deadline)?;
        self.fence(&mut c, stop, deadline).await?;
        let now:String=sqlx::query_scalar("SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')").fetch_one(&mut *c).await?;
        self.check(stop, deadline)?;
        #[cfg(test)]
        let now = self.probe.clock.lock().unwrap().clone().unwrap_or(now);
        // Bound replay before materializing the member's complete immutable
        // Journal. Never truncate it or fabricate an empty/partial snapshot.
        let limits:Option<(i64,i64)>=sqlx::query_as("SELECT octet_length(journal::text)::bigint,jsonb_array_length(journal->'commands')::bigint FROM subscription_runtime_records WHERE owner_id=$1")
            .bind(self.identity.user_id.as_str()).fetch_optional(&mut *c).await?;
        self.check(stop, deadline)?;
        let (bytes, commands) = limits.ok_or(StorageError::NotFound)?;
        if bytes > 8 * 1024 * 1024 || commands > 10000 {
            return Err(StorageError::DependencyUnavailable);
        }
        let snapshot = self
            .service
            .load(&mut c, self.identity.user_id.as_str())
            .await?;
        self.check(stop, deadline)?;
        let list = snapshot
            .list_subscriptions_mock()
            .map_err(|_| StorageError::CorruptRecord)?;
        if list.items.len() > self.config.max_ledger_entries {
            return Err(StorageError::DependencyUnavailable);
        }
        let due = snapshot
            .due_reminders_mock(&now)
            .map_err(|_| StorageError::CorruptRecord)?;
        if due.messages.len() > self.config.max_messages
            || due.report.items.len() > self.config.max_ledger_entries * 2
        {
            return Err(StorageError::DependencyUnavailable);
        }
        let mut report = SubscriptionReminderScan {
            fictional: true,
            notification_sent: false,
            production_registered: false,
            scanned: true,
            reserved: false,
            simulated: false,
            result_saved: false,
            result_commit_acknowledged: false,
            due_count: due.messages.len(),
            simulated_count: 0,
            trusted_now: now.clone(),
            original_due_report: serde_json::to_value(&due.report)
                .map_err(|_| StorageError::CorruptRecord)?,
            original_saved_plans: Vec::new(),
            original_attempt_refs: Vec::new(),
        };
        if due.messages.is_empty() {
            self.fence(&mut c, stop, deadline).await?;
            c.close().await?;
            self.check(stop, deadline)?;
            return Ok(report);
        }
        let store = self.service.store()?;
        let mut tx = c.begin().await?;
        self.check(stop, deadline)?;
        crate::daily_usage::protect_owner(&mut tx, &self.identity.user_id).await?;
        self.check(stop, deadline)?;
        self.fence(&mut tx, stop, deadline).await?;
        let mut work = store
            .load_work_on_connection_mock(&mut tx, self.identity.user_id.as_str())
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        work.reserve_reminders_mock(&now)
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        let pending = store
            .save_on_connection_mock(&mut tx, work)
            .await
            .map_err(write_error)?;
        if let Some(scope)=&self.public_scope {
            report.original_attempt_refs=persistent::reserve_attempts(self,scope,&mut tx,&due.messages).await?;
        }
        self.check(stop, deadline)?;
        self.fence(&mut tx, stop, deadline).await?;
        #[cfg(test)]
        self.probe.at("reserve-staged").await;
        self.check(stop, deadline)?;
        let committed = tx.commit().await.is_ok();
        self.check(stop, deadline)?;
        #[cfg(test)]
        self.probe.at("reserve-committed").await;
        self.check(stop, deadline)?;
        let mut reader = repository.subscription_read_pool.acquire().await?;
        self.check(stop, deadline)?;
        if !committed {
            // Unknown COMMIT: observe only. Never turn observation into a new
            // simulation token or retry this reservation automatically.
            let _ = pending.read_committed_on_connection_mock(&mut reader).await;
            self.check(stop, deadline)?;
            return Err(StorageError::ReconcileRequired);
        }
        let batch = pending
            .into_committed_reminder_batch_mock(&mut reader)
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        drop(reader);
        report.reserved = true;
        #[cfg(test)]
        self.probe.at("batch-verified").await;
        self.fence(&mut c, stop, deadline).await?;
        // No business transaction is open. Full original Journal is rechecked
        // inside member code. This sink has no transport or caller result DTO.
        let mut guarded = GuardedSimulation {
            host: self,
            stop,
            deadline,
            sink,
            calls: 0,
            denied: false,
        };
        let simulated = batch
            .simulate_on_connection_mock(&mut c, &mut guarded)
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        if guarded.denied {
            return Err(StorageError::Session);
        }
        report.simulated_count = guarded.calls;
        report.simulated = true;
        #[cfg(test)]
        self.probe.at("simulated").await;
        self.fence(&mut c, stop, deadline).await?;
        let mut tx = c.begin().await?;
        self.check(stop, deadline)?;
        crate::daily_usage::protect_owner(&mut tx, &self.identity.user_id).await?;
        self.check(stop, deadline)?;
        self.fence(&mut tx, stop, deadline).await?;
        let mut work = store
            .load_work_on_connection_mock(&mut tx, self.identity.user_id.as_str())
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        work.record_reminder_results_mock(simulated)
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        let pending = store
            .save_on_connection_mock(&mut tx, work)
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        self.fence(&mut tx, stop, deadline).await?;
        #[cfg(test)]
        self.probe.at("results-staged").await;
        self.check(stop, deadline)?;
        let ack = tx.commit().await.is_ok();
        self.check(stop, deadline)?;
        #[cfg(test)]
        self.probe.at("results-committed").await;
        self.check(stop, deadline)?;
        let mut reader = repository.subscription_read_pool.acquire().await?;
        self.check(stop, deadline)?;
        let saved = pending
            .read_committed_on_connection_mock(&mut reader)
            .await
            .map_err(write_error)?;
        self.check(stop, deadline)?;
        drop(reader);
        // The original facts have now been observed, even after an ambiguous
        // result COMMIT. Do not publish a normal run receipt or retry the sink.
        if !ack {
            return Err(StorageError::ReconcileRequired);
        }
        for item in &list.items {
            report.original_saved_plans.push(
                serde_json::to_value(
                    saved
                        .reminder_plan_mock(&item.subscription_id)
                        .map_err(|_| StorageError::CorruptRecord)?,
                )
                .map_err(|_| StorageError::CorruptRecord)?,
            );
        }
        self.fence(&mut c, stop, deadline).await?;
        report.result_saved = true;
        report.result_commit_acknowledged = ack;
        c.close().await?;
        self.check(stop, deadline)?;
        Ok(report)
    }
}
struct FixedSimulation;
impl MockInAppSink for FixedSimulation {
    fn simulate_in_app(&mut self, _: &ReminderMessage) -> DeliveryOutcome {
        DeliveryOutcome::SimulatedSuccess
    }
}
struct GuardedSimulation<'a, T> {
    host: &'a SubscriptionReminderHost,
    stop: &'a SubscriptionReminderStop,
    deadline: Instant,
    sink: &'a mut T,
    calls: usize,
    denied: bool,
}
impl<T: MockInAppSink> MockInAppSink for GuardedSimulation<'_, T> {
    fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome {
        if self.host.check(self.stop, self.deadline).is_err()
            || message.user_id != self.host.identity.user_id.as_str()
            || !message.fictional
            || message.channel != "IN_APP_MOCK_ONLY"
        {
            self.denied = true;
            return DeliveryOutcome::Unknown;
        }
        self.calls += 1;
        self.sink.simulate_in_app(message)
    }
}
#[cfg(test)]
#[path = "subscription_reminder_tests.rs"]
mod tests;
