//! Native-only, version-pinned local simulation sources. No identity authority.
//! Async SQL capture ends before the original synchronous collector is invoked.
use crate::{PgRepository, StorageError, SubscriptionService};
use safeflow_bank_gateway::BankGateway;
use safeflow_billing_source_reader::{query::*, TrustedScope};
use safeflow_contracts::{ResourceId, SessionRef, UserId};
use safeflow_subscription_evidence::managed_ledger_mock::LedgerFixture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction as SqlTransaction};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_ROWS: usize = 10_000;
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const DEADLINE: Duration = Duration::from_secs(5);
fn digest<T: Serialize>(v: &T) -> Result<String, StorageError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(v).map_err(|_| StorageError::CorruptRecord)?)
    ))
}
fn time(s: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, StorageError> {
    chrono::DateTime::parse_from_rfc3339(s).map_err(|_| StorageError::Invalid("source time"))
}
fn reference(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_:-.".contains(&b))
}

/// Explicit native configuration, not an HTTP grant. Every account is separately
/// checked through the host's actual BankGateway when this data is consumed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulationSourceSpec {
    pub source_id: String,
    pub owner: String,
    pub revision: String,
    pub accounts: Vec<String>,
    pub coverage: Window,
    pub complete: bool,
    pub coverage_evidence: String,
    pub page_size: usize,
}
impl SimulationSourceSpec {
    fn validate(&self) -> Result<(), StorageError> {
        if !(1..=512).contains(&self.page_size) {
            return Err(StorageError::Invalid("page size"));
        }
        if ![&self.source_id, &self.owner, &self.revision]
            .iter()
            .all(|s| reference(s))
            || !self.complete
            || self.coverage_evidence.trim().is_empty()
            || self.coverage_evidence.len() > 2048
            || self.accounts.is_empty()
            || self.accounts.len() > 16
            || self.accounts.windows(2).any(|a| a[0] >= a[1])
            || self.accounts.iter().any(|a| ResourceId::parse(a).is_err())
            || self.coverage.timezone != "Asia/Shanghai"
            || time(&self.coverage.start_inclusive)? >= time(&self.coverage.end_exclusive)?
            || time(&self.coverage.end_exclusive)? > time(&self.coverage.as_of)?
        {
            return Err(StorageError::Invalid("complete source scope required"));
        }
        Ok(())
    }
    fn request(&self) -> Request {
        Request {
            account_ids: self.accounts.clone(),
            window: self.coverage.clone(),
        }
    }
}
#[derive(Clone)]
pub struct PgSimulationSource {
    repository: PgRepository,
    spec: SimulationSourceSpec,
    database_scope: Value,
    expected_fixed_digest: Option<String>,
}
async fn database_scope(c: &mut sqlx::PgConnection) -> Result<Value, StorageError> {
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('database',current_database(),'database_oid',(SELECT oid::bigint FROM pg_database WHERE datname=current_database()),'schema',current_schema(),'schema_oid',(SELECT oid::bigint FROM pg_namespace WHERE nspname=current_schema()),'address',inet_server_addr()::text,'port',inet_server_port())").fetch_one(c).await?)
}

/// Private binding travels with same-capture selection. Never deserialized.
#[derive(Clone)]
pub(crate) struct SimulationFence {
    source: PgSimulationSource,
    generation: i64,
}
impl SimulationFence {
    pub(crate) async fn lock(&self, c: &mut sqlx::PgConnection) -> Result<(), StorageError> {
        self.source.lock(c, Some(self.generation)).await.map(|_| ())
    }
    pub(crate) fn accounts(&self) -> &[String] {
        &self.source.spec.accounts
    }
    pub(crate) fn provenance(&self) -> Value {
        serde_json::json!({"spec":self.source.spec,"database":self.source.database_scope,"generation":self.generation})
    }
}

/// The transaction holds the current source row SHARE lock until publication or
/// drop. Cancellation rolls back/releases it; no detached SQL producer exists.
pub struct SimulationCapture {
    reader: SimulationCaptureReader,
    pub(crate) fence: SimulationFence,
    _lease: SqlTransaction<'static, Postgres>,
}
impl SimulationCapture {
    pub(crate) fn evidence_for_public(&self)->Value {
        serde_json::json!({"source":self.fence.provenance(),"snapshot":self.reader.snapshot,
            "request":self.reader.request,"transactions":self.reader.rows.as_ref()})
    }
    pub(crate) async fn check_same_database(
        &self,
        c: &mut sqlx::PgConnection,
    ) -> Result<(), StorageError> {
        if database_scope(c).await? != self.fence.source.database_scope {
            return Err(StorageError::TransactionScope);
        }
        self.check_live()
    }
    pub fn reader(&self) -> SimulationCaptureReader {
        self.reader.clone()
    }
    pub fn check_live(&self) -> Result<(), StorageError> {
        if Instant::now() >= self.reader.expires {
            Err(StorageError::DependencyUnavailable)
        } else {
            Ok(())
        }
    }
}
#[derive(Clone)]
pub struct SimulationCaptureReader {
    source: String,
    owner: String,
    request: Request,
    snapshot: String,
    rows: Arc<Vec<Transaction>>,
    page_size: usize,
    expires: Instant,
}
impl PagedSource for SimulationCaptureReader {
    fn source_id(&self) -> &str {
        &self.source
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        r: &Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        if Instant::now() >= self.expires {
            return Err(PageError::SnapshotExpired);
        }
        if scope.user_id() != self.owner
            || r != &self.request
            || r.account_ids.iter().any(|a| !scope.contains_account(a))
        {
            return Err(PageError::ResourceUnavailable);
        }
        let offset = match (cursor, snapshot) {
            (None, None) => 0,
            (Some(c), Some(s)) if s == self.snapshot => {
                let prefix = format!("{}:", self.snapshot);
                let n = c
                    .strip_prefix(&prefix)
                    .and_then(|n| n.parse::<usize>().ok())
                    .ok_or(PageError::SnapshotExpired)?;
                if n == 0
                    || n % self.page_size != 0
                    || n >= self.rows.len()
                    || c != format!("{prefix}{n}")
                {
                    return Err(PageError::SnapshotExpired);
                }
                n
            }
            _ => return Err(PageError::SnapshotExpired),
        };
        let end = (offset + self.page_size).min(self.rows.len());
        let more = end < self.rows.len();
        Ok(Page {
            snapshot_id: self.snapshot.clone(),
            owner_user_id: self.owner.clone(),
            account_ids: r.account_ids.clone(),
            window: r.window.clone(),
            items: self.rows[offset..end].to_vec(),
            has_more: more,
            next_cursor: more.then(|| format!("{}:{end}", self.snapshot)),
            range_complete: !more,
            total_unique_rows: (!more).then_some(self.rows.len()),
        })
    }
}
impl PgSimulationSource {
    /// Import a complete original typed set atomically. Rows are not fixture Pages.
    /// Installation does not activate it, initialize a ledger or authorize access.
    pub async fn import(
        repository: PgRepository,
        spec: SimulationSourceSpec,
        rows: Vec<Transaction>,
        initial: Option<LedgerFixture>,
    ) -> Result<Self, StorageError> {
        spec.validate()?;
        if rows.len() > MAX_ROWS
            || serde_json::to_vec(&(&spec, &rows, &initial))
                .map_err(|_| StorageError::CorruptRecord)?
                .len()
                > MAX_BYTES
        {
            return Err(StorageError::DependencyUnavailable);
        }
        if let Some(v) = &initial {
            crate::SubscriptionService::for_source_preview(&repository, &spec.owner, v.clone())?;
        }
        let mut unique = BTreeMap::new();
        for row in rows {
            if !spec.accounts.contains(&row.account_id) {
                return Err(StorageError::FactsConflict);
            }
            if let Some(t) = &row.completed_at {
                let t = time(t)?;
                if t < time(&spec.coverage.start_inclusive)?
                    || t >= time(&spec.coverage.end_exclusive)?
                {
                    return Err(StorageError::FactsConflict);
                }
            }
            let key = (row.account_id.clone(), row.transaction_id.clone());
            if let Some(old) = unique.insert(key, row.clone()) {
                if old != row {
                    return Err(StorageError::FactsConflict);
                }
            }
        }
        let rows: Vec<_> = unique.into_values().collect();
        let reader = SimulationCaptureReader {
            source: spec.source_id.clone(),
            owner: spec.owner.clone(),
            request: spec.request(),
            snapshot: "import-validation".into(),
            rows: Arc::new(rows.clone()),
            page_size: spec.page_size,
            expires: Instant::now() + DEADLINE,
        };
        let scope = TrustedScope::from_verified_context(
            &spec.owner,
            spec.accounts.iter().map(String::as_str),
        )
        .map_err(|_| StorageError::Invalid("source scope"))?;
        if !collect(&scope, &spec.request(), &reader, Limits::default()).is_complete {
            return Err(StorageError::CorruptRecord);
        }
        let hash = digest(&(&spec, &rows, &initial))?;
        let scope = tokio::time::timeout(DEADLINE, async {
            database_scope(&mut *repository.pool.acquire().await?).await
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)??;
        let source = Self {
            repository,
            spec,
            database_scope: scope,
            expected_fixed_digest: None,
        };
        tokio::time::timeout(DEADLINE,async {
   let mut tx=source.repository.pool.begin().await?;
   sqlx::query("SET LOCAL statement_timeout='4000ms'").execute(&mut *tx).await?;
   let inserted=sqlx::query("INSERT INTO simulation_source_versions_v1(source_id,owner_id,revision,spec,digest,initial) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
    .bind(&source.spec.source_id).bind(&source.spec.owner).bind(&source.spec.revision).bind(serde_json::to_value(&source.spec).unwrap()).bind(&hash).bind(initial.as_ref().map(|v|serde_json::to_value(v).unwrap())).execute(&mut *tx).await?.rows_affected();
   let actual:String=sqlx::query_scalar("SELECT digest FROM simulation_source_versions_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&source.spec.source_id).bind(&source.spec.owner).bind(&source.spec.revision).fetch_one(&mut *tx).await?;
   if actual!=hash{return Err(StorageError::FactsConflict)}
   if inserted==1 {for row in &rows {
    sqlx::query("INSERT INTO simulation_source_rows_v1(source_id,owner_id,revision,account_id,transaction_id,completed_at,body,digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
     .bind(&source.spec.source_id).bind(&source.spec.owner).bind(&source.spec.revision).bind(&row.account_id).bind(&row.transaction_id).bind(row.completed_at.as_deref().map(time).transpose()?).bind(serde_json::to_value(row).unwrap()).bind(digest(row)?).execute(&mut *tx).await?;
   }}
   source.verify_version(&mut tx).await?;
   tx.commit().await.map_err(|_|StorageError::ReconcileRequired)
  }).await.map_err(|_|StorageError::DependencyUnavailable)??;
        Ok(source)
    }
    pub async fn open(
        repository: PgRepository,
        spec: SimulationSourceSpec,
    ) -> Result<Self, StorageError> {
        let source = Self::open_imported(repository, spec).await?;
        tokio::time::timeout(DEADLINE, async {
            let mut tx = source.repository.pool.begin().await?;
            source.lock(&mut tx, None).await?;
            tx.rollback().await?;
            Ok::<_, StorageError>(())
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)??;
        Ok(source)
    }
    pub async fn open_imported(
        repository: PgRepository,
        spec: SimulationSourceSpec,
    ) -> Result<Self, StorageError> {
        spec.validate()?;
        tokio::time::timeout(DEADLINE,async {
   let mut tx=repository.pool.begin().await?;
   let scope=database_scope(&mut tx).await?;
   let saved:Value=sqlx::query_scalar("SELECT spec FROM simulation_source_versions_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&spec.source_id).bind(&spec.owner).bind(&spec.revision).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
   if saved!=serde_json::to_value(&spec).unwrap(){return Err(StorageError::FactsConflict)}
   let source=Self{repository,spec,database_scope:scope,expected_fixed_digest:None};source.verify_version(&mut tx).await?;tx.rollback().await?;Ok(source)
  }).await.map_err(|_|StorageError::DependencyUnavailable)?
    }
    pub fn spec(&self) -> &SimulationSourceSpec {
        &self.spec
    }
    pub(crate) fn repository_for_backend(&self) -> PgRepository {
        self.repository.clone()
    }
    pub(crate) async fn bind_fixed_digest(&mut self, digest: String) -> Result<(), StorageError> {
        self.expected_fixed_digest = Some(digest);
        tokio::time::timeout(DEADLINE, async {
            let mut tx = self.repository.simulation_source_pool.begin().await?;
            sqlx::query("SET LOCAL statement_timeout='4000ms'")
                .execute(&mut *tx)
                .await?;
            self.verify_version(&mut tx).await?;
            tx.rollback().await?;
            Ok(())
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)?
    }
    /// Provenance only, never an identity, account grant or executable token.
    pub fn provenance_for_backend(&self) -> Value {
        serde_json::json!({"spec":self.spec,"database":self.database_scope})
    }
    /// Native-only local technical generation; never an account revocation API.
    pub async fn set_active(&self, active: bool) -> Result<(), StorageError> {
        tokio::time::timeout(DEADLINE,async {
   let mut tx=self.repository.pool.begin().await?;
   sqlx::query("SET LOCAL lock_timeout='3000ms'").execute(&mut *tx).await?;
   let saved:Value=sqlx::query_scalar("SELECT spec FROM simulation_source_versions_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
   if saved!=serde_json::to_value(&self.spec).unwrap(){return Err(StorageError::FactsConflict)}
   sqlx::query("INSERT INTO simulation_source_heads_v1(source_id,owner_id,revision,generation,active) VALUES($1,$2,$3,1,$4) ON CONFLICT(source_id,owner_id) DO UPDATE SET revision=excluded.revision,generation=simulation_source_heads_v1.generation+1,active=excluded.active")
    .bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).bind(active).execute(&mut *tx).await?;
   tx.commit().await.map_err(|_|StorageError::ReconcileRequired)
  }).await.map_err(|_|StorageError::DependencyUnavailable)?
    }
    async fn lock(
        &self,
        c: &mut sqlx::PgConnection,
        generation: Option<i64>,
    ) -> Result<i64, StorageError> {
        let scope = database_scope(c).await?;
        // A token cannot accidentally validate against another repository/schema.
        if scope != self.database_scope {
            return Err(StorageError::TransactionScope);
        }
        let h:Option<(String,i64,bool,Value)>=sqlx::query_as("SELECT h.revision,h.generation,h.active,v.spec FROM simulation_source_heads_v1 h JOIN simulation_source_versions_v1 v USING(source_id,owner_id,revision) WHERE h.source_id=$1 AND h.owner_id=$2 FOR SHARE OF h")
   .bind(&self.spec.source_id).bind(&self.spec.owner).fetch_optional(c).await?;
        let (revision, g, active, spec) = h.ok_or(StorageError::DependencyUnavailable)?;
        if !active
            || revision != self.spec.revision
            || generation.is_some_and(|n| n != g)
            || spec != serde_json::to_value(&self.spec).unwrap()
        {
            return Err(StorageError::FactsConflict);
        }
        Ok(g)
    }
    pub async fn capture(
        &self,
        owner: &UserId,
        request: &Request,
    ) -> Result<SimulationCapture, StorageError> {
        if owner.as_str() != self.spec.owner
            || request.account_ids.is_empty()
            || request.account_ids.iter().collect::<BTreeSet<_>>().len()
                != request.account_ids.len()
            || request
                .account_ids
                .iter()
                .any(|a| !self.spec.accounts.contains(a))
            || request.window.timezone != "Asia/Shanghai"
            || time(&request.window.start_inclusive)? < time(&self.spec.coverage.start_inclusive)?
            || time(&request.window.end_exclusive)? > time(&self.spec.coverage.end_exclusive)?
            || time(&request.window.start_inclusive)? >= time(&request.window.end_exclusive)?
            || time(&request.window.end_exclusive)? > time(&request.window.as_of)?
        {
            return Err(StorageError::FactsConflict);
        }
        tokio::time::timeout(DEADLINE,async {
   let mut tx=self.repository.simulation_source_pool.begin().await?;
   sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").execute(&mut *tx).await?;
   sqlx::query("SET LOCAL statement_timeout='4000ms'").execute(&mut *tx).await?;
   let generation=self.lock(&mut tx,None).await?;
   self.verify_version(&mut tx).await?;
   let count:i64=sqlx::query_scalar("SELECT count(*) FROM simulation_source_rows_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3 AND account_id=ANY($4) AND (completed_at IS NULL OR (completed_at >= $5 AND completed_at < $6))")
    .bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).bind(&request.account_ids).bind(time(&request.window.start_inclusive)?).bind(time(&request.window.end_exclusive)?).fetch_one(&mut *tx).await?;
   if count<0||count as usize>MAX_ROWS||count as usize>self.spec.page_size*100{return Err(StorageError::DependencyUnavailable)}
   let saved=sqlx::query("SELECT body,digest,account_id,transaction_id FROM simulation_source_rows_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3 AND account_id=ANY($4) AND (completed_at IS NULL OR (completed_at >= $5 AND completed_at < $6)) ORDER BY completed_at NULLS FIRST,account_id,transaction_id LIMIT 10001")
    .bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).bind(&request.account_ids).bind(time(&request.window.start_inclusive)?).bind(time(&request.window.end_exclusive)?).fetch_all(&mut *tx).await?;
   if saved.len()!=count as usize{return Err(StorageError::CorruptRecord)}
   let mut rows=Vec::new();let mut bytes=0usize;
   for row in saved {let body:Value=row.try_get("body")?;bytes+=serde_json::to_vec(&body).map_err(|_|StorageError::CorruptRecord)?.len();if bytes>MAX_BYTES{return Err(StorageError::DependencyUnavailable)}
    let item:Transaction=serde_json::from_value(body).map_err(|_|StorageError::CorruptRecord)?;
    if digest(&item)?!=row.try_get::<String,_>("digest")? || item.account_id!=row.try_get::<String,_>("account_id")? || item.transaction_id!=row.try_get::<String,_>("transaction_id")?{return Err(StorageError::CorruptRecord)}rows.push(item);
   }
   let snapshot=digest(&(&self.database_scope,&self.spec,request,generation,digest(&rows)?))?;
   sqlx::query("SET LOCAL idle_in_transaction_session_timeout='6000ms'").execute(&mut *tx).await?;
   Ok(SimulationCapture{reader:SimulationCaptureReader{source:self.spec.source_id.clone(),owner:self.spec.owner.clone(),request:request.clone(),snapshot,rows:Arc::new(rows),page_size:self.spec.page_size,expires:Instant::now()+DEADLINE},fence:SimulationFence{source:self.clone(),generation},_lease:tx})
  }).await.map_err(|_|StorageError::DependencyUnavailable)?
    }
    pub(crate) async fn verify_version(
        &self,
        c: &mut sqlx::PgConnection,
    ) -> Result<(), StorageError> {
        let (count,bytes):(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(octet_length(body::text)),0)::bigint FROM simulation_source_rows_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).fetch_one(&mut *c).await?;
        if count < 0 || count as usize > MAX_ROWS || bytes < 0 || bytes as usize > MAX_BYTES {
            return Err(StorageError::DependencyUnavailable);
        }
        let saved=sqlx::query("SELECT body,digest,account_id,transaction_id,completed_at FROM simulation_source_rows_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3 ORDER BY account_id,transaction_id LIMIT 10001").bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).fetch_all(&mut *c).await?;
        if saved.len() != count as usize {
            return Err(StorageError::CorruptRecord);
        }
        let mut rows = Vec::<Transaction>::new();
        for row in saved {
            let item: Transaction = serde_json::from_value(row.try_get("body")?)
                .map_err(|_| StorageError::CorruptRecord)?;
            let indexed: Option<chrono::DateTime<chrono::Utc>> = row.try_get("completed_at")?;
            if digest(&item)? != row.try_get::<String, _>("digest")?
                || item.account_id != row.try_get::<String, _>("account_id")?
                || item.transaction_id != row.try_get::<String, _>("transaction_id")?
                || item
                    .completed_at
                    .as_deref()
                    .map(time)
                    .transpose()?
                    .map(|t| t.with_timezone(&chrono::Utc))
                    != indexed
            {
                return Err(StorageError::CorruptRecord);
            }
            rows.push(item);
        }
        let (expected,initial):(String,Option<Value>)=sqlx::query_as("SELECT digest,initial FROM simulation_source_versions_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&self.spec.source_id).bind(&self.spec.owner).bind(&self.spec.revision).fetch_one(c).await?;
        let initial: Option<LedgerFixture> = initial
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| StorageError::CorruptRecord)?;
        if expected != digest(&(&self.spec, &rows, &initial))?
            || self
                .expected_fixed_digest
                .as_ref()
                .is_some_and(|fixed| fixed != &expected)
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(())
    }
}
impl SubscriptionService {
    fn for_source_preview(
        repository: &PgRepository,
        owner: &str,
        initial: LedgerFixture,
    ) -> Result<(), StorageError> {
        use safeflow_subscription_evidence::confirmation_mock::ProposedLedgerStoreMock;
        ProposedLedgerStoreMock::new(&repository.namespace)
            .map_err(crate::subscription::write_error)?
            .prepare_initialization_mock(owner, initial)
            .map_err(crate::subscription::write_error)?
            .preview_mock()
            .map_err(crate::subscription::write_error)?;
        Ok(())
    }
    pub async fn capture_simulation_for_backend(
        &self,
        session: &SessionRef,
        bank: &dyn BankGateway,
        source: &PgSimulationSource,
        request: &Request,
    ) -> Result<SimulationCapture, StorageError> {
        tokio::time::timeout(DEADLINE, async {
            let identity = self.identity(session)?;
            self.check_simulation_accounts(session, bank, &identity, &request.account_ids)
                .await?;
            let capture = source.capture(&identity.user_id, request).await?;
            self.check(session, &identity)?;
            self.check_simulation_accounts(session, bank, &identity, &request.account_ids)
                .await?;
            capture.check_live()?;
            Ok(capture)
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)?
    }
    pub(crate) async fn check_simulation_accounts(
        &self,
        session: &SessionRef,
        bank: &dyn BankGateway,
        identity: &safeflow_runtime::SessionBinding,
        accounts: &[String],
    ) -> Result<(), StorageError> {
        self.check(session, identity)?;
        for account in accounts {
            let id = ResourceId::parse(account).map_err(|_| StorageError::Invalid("account"))?;
            let result = bank.get_account_balance(&identity.user_id, &id).await;
            self.check(session, identity)?;
            result
                .map_err(|e| match e {
                    safeflow_bank_gateway::BankError::ResourceNotOwned => StorageError::NotFound,
                    safeflow_bank_gateway::BankError::ValidationError => {
                        StorageError::Invalid("account")
                    }
                    safeflow_bank_gateway::BankError::Unavailable => {
                        StorageError::DependencyUnavailable
                    }
                    safeflow_bank_gateway::BankError::InvalidResponse => {
                        StorageError::CorruptRecord
                    }
                })?
                .validate_for(&id)
                .map_err(|_| StorageError::CorruptRecord)?;
        }
        self.check(session, identity)
    }
    /// Distinct trusted entry: unlike old providers, this path requires SQL source
    /// generation and every configured account to remain valid through COMMIT.
    pub async fn initialize_simulation_for_host(
        &self,
        session: &SessionRef,
        bank: &dyn BankGateway,
        source: &PgSimulationSource,
    ) -> Result<(), StorageError> {
        tokio::time::timeout(DEADLINE,async {
   let identity=self.identity(session)?;
   if source.repository.namespace!=self.repository.namespace || identity.user_id.as_str()!=source.spec.owner{return Err(StorageError::FactsConflict)}
   let mut writer=self.repository.pool.acquire().await?;use sqlx::Connection;
   let mut tx=writer.begin().await?;
   crate::daily_usage::protect_owner(&mut tx,&identity.user_id).await?;
   let host:Option<(i64,String)>=sqlx::query_as("SELECT generation,instance_id FROM subscription_host_generations_v1 WHERE owner_id=$1 FOR UPDATE").bind(identity.user_id.as_str()).fetch_optional(&mut *tx).await?;
   source.lock(&mut tx,None).await?;
   source.verify_version(&mut tx).await?;
   let raw:Option<Value>=sqlx::query_scalar("SELECT initial FROM simulation_source_versions_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&source.spec.source_id).bind(&source.spec.owner).bind(&source.spec.revision).fetch_one(&mut *tx).await?;
   let initial:LedgerFixture=serde_json::from_value(raw.ok_or(StorageError::DependencyUnavailable)?).map_err(|_|StorageError::CorruptRecord)?;
   let request=self.store()?.prepare_initialization_mock(identity.user_id.as_str(),initial).map_err(crate::subscription::write_error)?;
   let pending=self.store()?.initialize_on_connection_mock(&mut tx,identity.user_id.as_str(),&request).await.map_err(crate::subscription::write_error)?;
   let current:Option<(i64,String)>=sqlx::query_as("SELECT generation,instance_id FROM subscription_host_generations_v1 WHERE owner_id=$1").bind(identity.user_id.as_str()).fetch_optional(&mut *tx).await?;
   if current!=host{return Err(StorageError::FactsConflict)}
   self.check_simulation_accounts(session,bank,&identity,&source.spec.accounts).await?;
   tx.commit().await.map_err(|_|StorageError::ReconcileRequired)?;
   let mut reader=self.repository.subscription_read_pool.acquire().await?;
   pending.read_committed_on_connection_mock(&mut reader).await.map_err(crate::subscription::write_error)?;
   self.check_simulation_accounts(session,bank,&identity,&source.spec.accounts).await?;self.check(session,&identity)
  }).await.map_err(|_|StorageError::ReconcileRequired)?
    }
    /// Original initialization lookup only. Never retries a write after unknown.
    pub async fn lookup_simulation_initial_for_backend(
        &self,
        session: &SessionRef,
        source: &PgSimulationSource,
    ) -> Result<bool, StorageError> {
        tokio::time::timeout(DEADLINE,async {
  let identity=self.identity(session)?;
  if identity.user_id.as_str()!=source.spec.owner || self.repository.namespace!=source.repository.namespace{return Err(StorageError::FactsConflict)}
  let mut tx=self.repository.subscription_read_pool.begin().await?;
  if database_scope(&mut tx).await?!=source.database_scope{return Err(StorageError::TransactionScope)}
  source.verify_version(&mut tx).await?;
  let raw:Option<Value>=sqlx::query_scalar("SELECT initial FROM simulation_source_versions_v1 WHERE source_id=$1 AND owner_id=$2 AND revision=$3").bind(&source.spec.source_id).bind(&source.spec.owner).bind(&source.spec.revision).fetch_one(&mut *tx).await?;
  let request=self.store()?.prepare_initialization_mock(identity.user_id.as_str(),serde_json::from_value(raw.ok_or(StorageError::DependencyUnavailable)?).map_err(|_|StorageError::CorruptRecord)?).map_err(crate::subscription::write_error)?;
  let exists=self.store()?.lookup_initialization_on_connection_mock(&mut tx,identity.user_id.as_str(),&request).await.map_err(crate::subscription::write_error)?.is_some();self.check(session,&identity)?;tx.rollback().await?;self.check(session,&identity)?;Ok(exists)
  }).await.map_err(|_|StorageError::DependencyUnavailable)?
    }
}
