//! Public identity/transaction boundary over the unmodified c163 domain codec.
//! Same-capture discovery stays fictional; no initial/provider or audit adoption.
use crate::{PgRepository, StorageError};
use safeflow_contracts::{ContractFuture, SessionRef};
use safeflow_runtime::{SessionBinding, SessionResolver};
use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::LedgerFixture};
use serde_json::{json, Value};
use sqlx::{PgConnection, Row};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone)]
struct ReadSelection {
    session: SessionRef,
    identity: SessionBinding,
    subscription_id: String,
    version: i64,
    expires: Instant,
}

const READ_CAPACITY: usize = 8192;
#[cfg(test)]
mod read_generation_tests {
    use super::*;
    fn identity(version: u64) -> SessionBinding {
        SessionBinding {
            user_id: safeflow_contracts::UserId::parse("owner").unwrap(),
            binding_version: version,
        }
    }
    #[test]
    fn subscription_read_failure_cancel_no_ledger_capacity_and_session_isolation() {
        let cache = Arc::new(Mutex::new(ReadCache::default()));
        let s = SessionRef::parse("session").unwrap();
        let t = SessionRef::parse("other_session").unwrap();
        let a = begin_read_cache(&cache, &s, &identity(1), || Ok(())).unwrap();
        let b = begin_read_cache(&cache, &s, &identity(1), || Ok(())).unwrap();
        let other = begin_read_cache(&cache, &t, &identity(1), || Ok(())).unwrap();
        other.publish(vec![]).unwrap();
        drop(other);
        drop(b); // failed/cancelled newer request cannot revive A
        assert_eq!(a.publish(vec![]).err(), Some(StorageError::FactsConflict));
        let rebind = begin_read_cache(&cache, &s, &identity(2), || Ok(())).unwrap();
        rebind.publish(vec![]).unwrap(); // NO_LEDGER uses this same empty publication
        assert_eq!(a.publish(vec![]).err(), Some(StorageError::FactsConflict));
        drop(rebind);
        drop(a);
        let mut leases = Vec::new();
        for _ in 0..READ_CAPACITY {
            leases.push(begin_read_cache(&cache, &s, &identity(2), || Ok(())).unwrap());
        }
        assert_eq!(
            begin_read_cache(&cache, &t, &identity(1), || Ok(()))
                .err()
                .map(|e| e),
            Some(StorageError::DependencyUnavailable)
        );
        assert_eq!(cache.lock().unwrap().requests.len(), 1);
        drop(leases);
        assert_eq!(cache.lock().unwrap().in_flight, 0);
        assert!(cache.lock().unwrap().requests.is_empty());
        let latest = begin_read_cache(&cache, &s, &identity(2), || Ok(())).unwrap();
        let selected = ReadSelection {
            session: t,
            identity: identity(1),
            subscription_id: "private".into(),
            version: 1,
            expires: Instant::now() + Duration::from_secs(120),
        };
        {
            let mut c = cache.lock().unwrap();
            for i in 0..READ_CAPACITY {
                c.choices.insert(i.to_string(), selected.clone());
            }
        }
        assert_eq!(
            latest.publish(vec![("excess".into(), selected)]).err(),
            Some(StorageError::DependencyUnavailable)
        );
        assert_eq!(cache.lock().unwrap().choices.len(), READ_CAPACITY);
    }
}
#[derive(Default)]
struct ReadCache {
    choices: HashMap<String, ReadSelection>,
    requests: HashMap<SessionRef, ReadRequest>,
    in_flight: usize,
}
struct ReadRequest {
    identity: SessionBinding,
    generation: uuid::Uuid,
    in_flight: usize,
}
// The lease keeps a slot alive until every older future has finished/dropped.
// Removing idle slots is safe: UUID generations prevent ABA after a new start.
struct ReadLease {
    cache: Arc<Mutex<ReadCache>>,
    session: SessionRef,
    identity: SessionBinding,
    generation: uuid::Uuid,
}
impl ReadLease {
    fn publish(&self, selected: Vec<(String, ReadSelection)>) -> Result<(), StorageError> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        let current = cache
            .requests
            .get(&self.session)
            .ok_or(StorageError::FactsConflict)?;
        if current.generation != self.generation || current.identity != self.identity {
            return Err(StorageError::FactsConflict);
        }
        cache.choices.retain(|_, v| v.expires > Instant::now());
        if cache.choices.len() + selected.len() > READ_CAPACITY {
            return Err(StorageError::DependencyUnavailable);
        }
        cache.choices.extend(selected);
        Ok(())
    }
}
impl Drop for ReadLease {
    fn drop(&mut self) {
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(request) = cache.requests.get_mut(&self.session) {
                request.in_flight -= 1;
                if request.in_flight == 0 {
                    cache.requests.remove(&self.session);
                }
                cache.in_flight -= 1;
            }
        }
    }
}

fn begin_read_cache(
    shared: &Arc<Mutex<ReadCache>>,
    session: &SessionRef,
    identity: &SessionBinding,
    guard: impl FnOnce() -> Result<(), StorageError>,
) -> Result<ReadLease, StorageError> {
    let mut cache = shared
        .lock()
        .map_err(|_| StorageError::DependencyUnavailable)?;
    // A resolver call made before waiting for this mutex may already be stale.
    // Validate under the same lock BEFORE invalidating a newer binding's handles.
    guard()?;
    // Admission rejection has no effect on existing choices. Once admitted,
    // even a failed/cancelled latest request invalidates older selections.
    if cache.in_flight >= READ_CAPACITY {
        return Err(StorageError::DependencyUnavailable);
    }
    let generation = uuid::Uuid::new_v4();
    let request = cache
        .requests
        .entry(session.clone())
        .or_insert(ReadRequest {
            identity: identity.clone(),
            generation,
            in_flight: 0,
        });
    request.identity = identity.clone();
    request.generation = generation;
    request.in_flight += 1;
    cache.in_flight += 1;
    cache
        .choices
        .retain(|_, v| v.expires > Instant::now() && v.session != *session);
    Ok(ReadLease {
        cache: shared.clone(),
        session: session.clone(),
        identity: identity.clone(),
        generation,
    })
}

pub(crate) fn now() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339()
}
pub(crate) fn read_error(e: ProposedLedgerReadIssue) -> StorageError {
    match e {
        ProposedLedgerReadIssue::NotFound => StorageError::NotFound,
        ProposedLedgerReadIssue::Unavailable => StorageError::DependencyUnavailable,
        _ => StorageError::CorruptRecord,
    }
}
pub(crate) fn write_error(e: ProposedLedgerWriteIssue) -> StorageError {
    match e {
        ProposedLedgerWriteIssue::Read(e) => read_error(e),
        ProposedLedgerWriteIssue::Conflict => StorageError::FactsConflict,
        _ => StorageError::ReconcileRequired,
    }
}

/// Explicit trusted startup source, never an HTTP DTO. An absent provider is
/// different from an adopted provider's known-complete, genuinely empty ledger.
pub trait SubscriptionInitialSource: Send + Sync {
    fn load<'a>(
        &'a self,
        owner: &'a str,
    ) -> ContractFuture<'a, Result<LedgerFixture, StorageError>>;
}

/// Host-owned write generation. Private fields, no Deserialize, no client grant.
#[derive(Clone)]
pub struct SubscriptionHost {
    pub(crate) namespace: String,
    pub(crate) owner: String,
    pub(crate) instance: String,
    pub(crate) generation: i64,
}

#[derive(Clone)]
pub struct SubscriptionService {
    pub(crate) repository: PgRepository,
    pub(crate) sessions: Arc<dyn SessionResolver>,
    source: Option<Arc<dyn SubscriptionInitialSource>>,
    selections: Arc<Mutex<ReadCache>>,
}
impl SubscriptionService {
    /// Read the existing host identity without starting/restarting the Journal.
    pub async fn current_host_for_backend(&self,session:&SessionRef)->Result<SubscriptionHost,StorageError>{
        let identity=self.identity(session)?;
        let row:Option<(i64,String)>=sqlx::query_as("SELECT generation,instance_id FROM subscription_host_generations_v1 WHERE owner_id=$1")
            .bind(identity.user_id.as_str()).fetch_optional(&self.repository.subscription_read_pool).await?;
        self.check(session,&identity)?;
        let (generation,instance)=row.ok_or(StorageError::NotFound)?;
        Ok(SubscriptionHost{namespace:self.repository.namespace.clone(),owner:identity.user_id.to_string(),generation,instance})
    }
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
    ) -> Self {
        Self {
            repository,
            sessions,
            source: None,
            selections: Arc::new(Mutex::new(ReadCache::default())),
        }
    }
    /// Does not initialize, restart or enable any write action.
    pub fn with_initial_source(mut self, source: Arc<dyn SubscriptionInitialSource>) -> Self {
        self.source = Some(source);
        self
    }
    pub(crate) fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        let b = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if b.binding_version == 0 {
            return Err(StorageError::Session);
        }
        Ok(b)
    }
    pub(crate) fn check(&self, s: &SessionRef, b: &SessionBinding) -> Result<(), StorageError> {
        if self.identity(s)? != *b {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    pub(crate) fn store(&self) -> Result<ProposedLedgerStoreMock, StorageError> {
        ProposedLedgerStoreMock::new(&self.repository.namespace).map_err(write_error)
    }
    pub(crate) async fn load(
        &self,
        c: &mut PgConnection,
        owner: &str,
    ) -> Result<SubscriptionReadSnapshotMock, StorageError> {
        ProposedLedgerReaderMock::new(&self.repository.namespace)
            .map_err(read_error)?
            .load_on_connection_mock(c, owner)
            .await
            .map_err(read_error)
    }
    /// Trusted, explicitly supplied fictional source only. Scope is rebuilt from
    /// the current identity and verified accounts before the actual 7bc reader.
    /// No default shared consumption/date/service provider is installed.
    pub async fn identify_for_backend(
        &self,
        session: &SessionRef,
        bank: &dyn safeflow_bank_gateway::BankGateway,
        request: &safeflow_billing_source_reader::query::Request,
        source: &impl safeflow_billing_source_reader::query::PagedSource,
        limits: safeflow_billing_source_reader::query::Limits,
    ) -> Result<Value, StorageError> {
        let (identity, capture) = self
            .collect_billing_for_backend(session, bank, request, source, limits)
            .await?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let value = capture
            .identify_on_connection_mock(&self.store()?, &mut tx)
            .await
            .map_err(|_| StorageError::DependencyUnavailable)?;
        self.check(session, &identity)?;
        tx.commit().await?;
        self.check(session, &identity)?;
        Ok(value)
    }
    // Shared actual collector: callers retain the identity checked across all
    // account awaits and the complete, unmodified, single collected input.
    pub(crate) async fn collect_billing_for_backend(
        &self,
        session: &SessionRef,
        bank: &dyn safeflow_bank_gateway::BankGateway,
        request: &safeflow_billing_source_reader::query::Request,
        source: &impl safeflow_billing_source_reader::query::PagedSource,
        limits: safeflow_billing_source_reader::query::Limits,
    ) -> Result<
        (
            SessionBinding,
            crate::subscription_billing_reader::CollectedBillingEvidenceMock,
        ),
        StorageError,
    > {
        let identity = self.identity(session)?;
        for account in &request.account_ids {
            let id = safeflow_contracts::ResourceId::parse(account)
                .map_err(|_| StorageError::Invalid("account"))?;
            let result = bank.get_account_balance(&identity.user_id, &id).await;
            self.check(session, &identity)?;
            result
                .map_err(|_| StorageError::NotFound)?
                .validate_for(&id)
                .map_err(|_| StorageError::CorruptRecord)?;
        }
        let scope = safeflow_billing_source_reader::TrustedScope::from_verified_context(
            identity.user_id.as_str(),
            request.account_ids.iter().map(String::as_str),
        )
        .map_err(|_| StorageError::NotFound)?;
        let capture = crate::subscription_billing_reader::collect_from_billing_mock(
            &scope, request, source, limits,
        )
        .map_err(|_| StorageError::DependencyUnavailable)?;
        self.check(session, &identity)?;
        Ok((identity, capture))
    }
    /// One consistent, read-only snapshot; no initialization, Restart or draft.
    pub async fn read(&self, session: &SessionRef) -> Result<Value, StorageError> {
        let identity = self.identity(session)?;
        let lease = self.begin_read(session, &identity)?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        self.check(session, &identity)?;
        let reader =
            ProposedLedgerReaderMock::new(&self.repository.namespace).map_err(read_error)?;
        let snapshot = reader
            .load_on_connection_mock(&mut tx, identity.user_id.as_str())
            .await;
        self.check(session, &identity)?;
        let snapshot = match snapshot {
            Ok(v) => v,
            Err(ProposedLedgerReadIssue::NotFound) => {
                tx.commit().await?;
                self.check(session, &identity)?;
                lease.publish(vec![])?;
                return Ok(
                    json!({"contract_version":1,"mode":"PROJECT_SIMULATION","scope":"SUBSCRIPTION_READ_ONLY",
                    "status":"NO_LEDGER","items":[],"originals":[],"pending":[],"writes_enabled":false,
                    "notification_sent":false,"merchant_action_executed":false,
                    "message":"尚无可核对的本人订阅资料，不能据此判断没有订阅。"}),
                );
            }
            Err(e) => return Err(read_error(e)),
        };
        let list = snapshot
            .list_subscriptions_mock()
            .map_err(|_| StorageError::CorruptRecord)?;
        if list.items.len() > 4096 {
            return Err(StorageError::DependencyUnavailable);
        }
        let mut items = Vec::new();
        let mut selected = Vec::new();
        for (i, item) in list.items.iter().enumerate() {
            let selection_ref = uuid::Uuid::new_v4().to_string();
            selected.push((
                selection_ref.clone(),
                ReadSelection {
                    session: session.clone(),
                    identity: identity.clone(),
                    subscription_id: item.subscription_id.clone(),
                    version: snapshot.version_mock(),
                    expires: Instant::now() + Duration::from_secs(120),
                },
            ));
            let plan = snapshot
                .reminder_plan_mock(&item.subscription_id)
                .map_err(|_| StorageError::CorruptRecord)?;
            items.push(json!({"position":i+1,"selection_ref":selection_ref,"current_state":item.current_state,
                "service_data_state":item.service_data_state,"message":item.message,
                "reminder":plan.schedule.as_ref().map(|s|json!({"expected_date":s.expected_date.to_string(),
                    "enabled":s.enabled,"days_before":s.days_before})),
                "reminder_state":plan.data_state}));
        }
        // Persisted exact originals are selected ONLY by the current owner.
        // A new legitimate session can read old facts, without inheriting writes.
        let rows = sqlx::query("SELECT business_ref,original_request FROM subscription_public_bindings_v1 WHERE owner_id=$1 ORDER BY business_ref LIMIT 8193")
            .bind(identity.user_id.as_str()).fetch_all(&mut *tx).await?;
        let mut originals = Vec::new();
        if rows.len() > 8192 {
            return Err(StorageError::DependencyUnavailable);
        }
        for row in rows {
            let request: OriginalOperationRequestMock =
                serde_json::from_value(row.try_get("original_request")?)
                    .map_err(|_| StorageError::CorruptRecord)?;
            let v = serde_json::to_value(
                snapshot
                    .original_view_mock(&request)
                    .map_err(|_| StorageError::CorruptRecord)?,
            )
            .map_err(|_| StorageError::CorruptRecord)?;
            originals.push(json!({"original_ref":row.try_get::<String,_>("business_ref")?,"status":v["status"],"message":v["message"]}));
        }
        let pending = snapshot
            .pending_confirmations_mock(&now())
            .map_err(|_| StorageError::CorruptRecord)?;
        if pending.items.len() > 8192 {
            return Err(StorageError::DependencyUnavailable);
        }
        let pending: Vec<Value> = pending
            .items
            .iter()
            .map(|p| json!({"action":p.action,"state":p.state,"expires_at":p.expires_at}))
            .collect();
        self.check(session, &identity)?;
        tx.commit().await?;
        self.check(session, &identity)?;
        // Short-lived read handles only. No database writes, grant, domain ID or
        // durable selection survives logout/rebind, relisting or backend restart.
        lease.publish(selected)?;
        Ok(
            json!({"contract_version":1,"mode":"PROJECT_SIMULATION","scope":"SUBSCRIPTION_READ_ONLY",
            "status":"AVAILABLE","items":items,"originals":originals,"pending":pending,"writes_enabled":false,
            "notification_sent":false,"merchant_action_executed":false,
            "message":"本人订阅资料只读查询；取消和撤回仅表示本地意图，提醒设置不代表已经发送。"}),
        )
    }
    fn begin_read(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
    ) -> Result<ReadLease, StorageError> {
        begin_read_cache(&self.selections, session, identity, || {
            self.check(session, identity)
        })
    }
    /// Resolve a server-issued choice against the current owner/session/version.
    /// Clients supply neither domain IDs, time, snapshots nor original bindings.
    pub(crate) fn target_for_public_write(&self, session: &SessionRef, selection_ref: &str) -> Result<(String,i64),StorageError> {
        let identity=self.identity(session)?;
        let cache=self.selections.lock().map_err(|_|StorageError::DependencyUnavailable)?;
        let choice=cache.choices.get(selection_ref).ok_or(StorageError::FactsConflict)?;
        if choice.session!=*session || choice.identity!=identity || choice.expires<=Instant::now() {
            return Err(StorageError::FactsConflict);
        }
        Ok((choice.subscription_id.clone(),choice.version))
    }
    /// Clients supply neither domain IDs, time, snapshots nor original bindings.
    pub async fn details(
        &self,
        session: &SessionRef,
        selection_ref: &str,
    ) -> Result<Value, StorageError> {
        uuid::Uuid::parse_str(selection_ref)
            .map_err(|_| StorageError::Invalid("selection reference"))?;
        let identity = self.identity(session)?;
        let selected = self
            .selections
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?
            .choices
            .get(selection_ref)
            .cloned()
            .ok_or(StorageError::FactsConflict)?;
        if selected.session != *session
            || selected.identity != identity
            || selected.expires <= Instant::now()
        {
            return Err(StorageError::FactsConflict);
        }
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let snapshot = self.load(&mut tx, identity.user_id.as_str()).await?;
        self.check(session, &identity)?;
        if snapshot.version_mock() != selected.version {
            return Err(StorageError::FactsConflict);
        }
        let query = SharedSubscriptionReadQueryMock::Business(
            SharedBusinessReadQueryMock::SubscriptionDetails {
                subscription_id: selected.subscription_id,
                now: now(),
            },
        );
        let view = self
            .store()?
            .query_on_connection_mock(&mut tx, identity.user_id.as_str(), &query)
            .await
            .map_err(write_error)?;
        self.check(session, &identity)?;
        tx.commit().await?;
        self.check(session, &identity)?;
        let cache = self
            .selections
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        if selected.expires <= Instant::now() || !cache.choices.contains_key(selection_ref) {
            return Err(StorageError::FactsConflict);
        }
        Ok(view)
    }
    #[cfg(test)]
    pub(crate) fn expire_read_selections_for_test(&self) {
        for choice in self.selections.lock().unwrap().choices.values_mut() {
            choice.expires = Instant::now();
        }
    }
    /// Read B through its backend mapping. Never accept a client-built original.
    pub async fn original(
        &self,
        session: &SessionRef,
        business_ref: &str,
    ) -> Result<Value, StorageError> {
        uuid::Uuid::parse_str(business_ref)
            .map_err(|_| StorageError::Invalid("original reference"))?;
        let identity = self.identity(session)?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let operation:String=sqlx::query_scalar("SELECT operation_id FROM subscription_public_bindings_v1 WHERE business_ref=$1 AND owner_id=$2").bind(business_ref).bind(identity.user_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
        crate::permit::domain_original_check(&mut tx,&identity.user_id,&safeflow_contracts::OperationId::parse(operation).map_err(|_|StorageError::CorruptRecord)?,self).await?;
        let raw: Value = sqlx::query_scalar("SELECT original_request FROM subscription_public_bindings_v1 WHERE business_ref=$1 AND owner_id=$2")
            .bind(business_ref).bind(identity.user_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
        let request = serde_json::from_value(raw).map_err(|_| StorageError::CorruptRecord)?;
        let saved = self.load(&mut tx, identity.user_id.as_str()).await?;
        let view = saved
            .original_view_mock(&request)
            .map_err(|_| StorageError::CorruptRecord)?;
        self.check(session, &identity)?;
        tx.commit().await?;
        self.check(session, &identity)?;
        serde_json::to_value(view).map_err(|_| StorageError::CorruptRecord)
    }
    /// Only trusted host assembly calls this. Ordinary reads never call it.
    /// Hold the writer until the independent read so its backend cannot be reused.
    pub async fn initialize_for_host(&self, session: &SessionRef) -> Result<(), StorageError> {
        let identity = self.identity(session)?;
        let source = self
            .source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let initial = source.load(identity.user_id.as_str()).await?;
        self.check(session, &identity)?;
        let store = self.store()?;
        let request = store
            .prepare_initialization_mock(identity.user_id.as_str(), initial)
            .map_err(write_error)?;
        let mut writer = self.repository.pool.acquire().await?;
        use sqlx::Connection;
        let mut tx = writer.begin().await?;
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        let pending = store
            .initialize_on_connection_mock(&mut tx, identity.user_id.as_str(), &request)
            .await
            .map_err(write_error)?;
        self.check(session, &identity)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        let mut reader = self.repository.subscription_read_pool.acquire().await?;
        pending
            .read_committed_on_connection_mock(&mut reader)
            .await
            .map_err(write_error)?;
        self.check(session, &identity)
    }
    /// Explicit single-owner startup. This is a write and is never an API route.
    /// Owner lock serializes it with the final writer-generation check.
    pub async fn startup_for_host(
        &self,
        session: &SessionRef,
    ) -> Result<SubscriptionHost, StorageError> {
        let identity = self.identity(session)?;
        let store = self.store()?;
        let mut writer = self.repository.pool.acquire().await?;
        use sqlx::Connection;
        let mut tx = writer.begin().await?;
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        let pending = store
            .stage_startup_recovery_on_connection_mock(&mut tx, identity.user_id.as_str())
            .await
            .map_err(write_error)?;
        let instance = uuid::Uuid::new_v4().to_string();
        let generation:i64 = sqlx::query_scalar("INSERT INTO subscription_host_generations_v1(owner_id,generation,instance_id) VALUES($1,1,$2) ON CONFLICT(owner_id) DO UPDATE SET generation=subscription_host_generations_v1.generation+1,instance_id=excluded.instance_id RETURNING generation")
            .bind(identity.user_id.as_str()).bind(&instance).fetch_one(&mut *tx).await?;
        self.check(session, &identity)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        let mut reader = self.repository.subscription_read_pool.acquire().await?;
        pending
            .read_committed_startup_summary_on_connection_mock(&mut reader, &now())
            .await
            .map_err(write_error)?;
        self.check(session, &identity)?;
        Ok(SubscriptionHost {
            namespace: self.repository.namespace.clone(),
            owner: identity.user_id.to_string(),
            instance,
            generation,
        })
    }
    pub(crate) async fn check_host(
        &self,
        c: &mut PgConnection,
        host: &SubscriptionHost,
        owner: &str,
    ) -> Result<(), StorageError> {
        if host.namespace != self.repository.namespace || host.owner != owner {
            return Err(StorageError::Session);
        }
        let actual:Option<(i64,String)> = sqlx::query_as("SELECT generation,instance_id FROM subscription_host_generations_v1 WHERE owner_id=$1 FOR UPDATE")
            .bind(owner).fetch_optional(c).await?;
        if actual != Some((host.generation, host.instance.clone())) {
            return Err(StorageError::FactsConflict);
        }
        Ok(())
    }
    /// A2 applies before task creation, draft save, permit issuance or consumption.
    pub fn require_write_adoption(&self, session: &SessionRef) -> Result<(), StorageError> {
        self.identity(session)?;
        Err(StorageError::DependencyUnavailable)
    }
}
