use crate::confirmation::{current_binding, timestamp};
use crate::confirmation_repository::{legacy_binding_row, verify_stored_confirmation};
use crate::models::{db_version, hash, valid_id};
use crate::repository::{audit, decode, encode, lock_task, operations, version};
use crate::{
    ConfirmationBinding, ConfirmationClock, PersistentAuditKind, PgRepository, StorageError,
    TaskRecord,
};
use safeflow_contracts::{ConfirmationRef, OperationId, SessionRef, TaskId};
use safeflow_runtime::{SessionBinding, SessionResolver};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{
    postgres::{PgPoolOptions, PgRow},
    PgPool, Postgres, Row, Transaction,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub const SIMULATION_AUTH_CONTRACT_VERSION: u16 = 1;
const MODE: &str = "FOUNDATION_ISOLATED_V1";

macro_rules! reference {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, StorageError> {
                let value = value.into();
                let suffix = value
                    .strip_prefix($prefix)
                    .ok_or(StorageError::Invalid("simulation reference prefix"))?;
                let uuid = uuid::Uuid::parse_str(suffix)
                    .map_err(|_| StorageError::Invalid("simulation reference UUID"))?;
                if uuid.to_string() != suffix {
                    return Err(StorageError::Invalid(
                        "simulation reference is not canonical",
                    ));
                }
                Ok(Self(value))
            }
            pub fn new_server_id() -> Self {
                Self(format!("{}{}", $prefix, uuid::Uuid::new_v4()))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
reference!(SimulationChallengeId, "sim_challenge_");
reference!(SimulationSubmissionId, "sim_submission_");
reference!(SimulationGrantId, "sim_grant_");

/// Trusted assembly choice, not a client/LLM input or a production authentication mode.
#[derive(Clone, Copy)]
pub enum SimulationAuthMode {
    FoundationIsolatedV1,
}
#[derive(Clone)]
pub struct SimulationAuthConfig {
    challenge_ttl_ms: i64,
    grant_ttl_ms: i64,
    max_failures: u32,
    secret_bytes: usize,
}
impl SimulationAuthConfig {
    pub fn new(
        challenge_ttl: Duration,
        grant_ttl: Duration,
        max_failures: u32,
        secret_bytes: usize,
    ) -> Result<Self, StorageError> {
        if !(1..=100).contains(&max_failures) || !(16..=64).contains(&secret_bytes) {
            return Err(StorageError::Invalid(
                "simulation configuration safety bounds",
            ));
        }
        Ok(Self {
            challenge_ttl_ms: ttl(challenge_ttl)?,
            grant_ttl_ms: ttl(grant_ttl)?,
            max_failures,
            secret_bytes,
        })
    }
}
fn ttl(value: Duration) -> Result<i64, StorageError> {
    let millis =
        i64::try_from(value.as_millis()).map_err(|_| StorageError::Invalid("auth TTL range"))?;
    if millis <= 0 || value.subsec_nanos() % 1_000_000 != 0 {
        return Err(StorageError::Invalid(
            "auth TTL requires positive whole milliseconds",
        ));
    }
    Ok(millis)
}

/// One-time controlled backend export, never Serialize/Debug or an ordinary data response.
/// This random-secret exercise is NOT SMS, a face check or proof of a real person's identity.
/// ```compile_fail
/// fn leak(secret: &safeflow_storage::SimulationSecret) {
///     println!("{secret:?}");
/// }
/// ```
pub struct SimulationSecret(Zeroizing<Vec<u8>>);
impl SimulationSecret {
    pub fn expose_for_isolated_backend(&self) -> &[u8] {
        &self.0
    }
}
/// Finite submitted material. Cannot log/serialize it through this API.
pub struct SimulationProof(Zeroizing<Vec<u8>>);
impl SimulationProof {
    pub fn from_material(bytes: &[u8]) -> Result<Self, StorageError> {
        if bytes.is_empty() || bytes.len() > 64 {
            return Err(StorageError::Invalid("simulation material length"));
        }
        Ok(Self(Zeroizing::new(bytes.to_vec())))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SimulationChallengeStatus {
    Pending,
    Succeeded,
    Failed,
    Revoked,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SimulationGrantStatus {
    Active,
    Revoked,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SimulationChallengeRecord {
    pub contract_version: u16,
    pub mode: &'static str,
    pub challenge_id: SimulationChallengeId,
    pub binding: ConfirmationBinding,
    pub confirmation_ref: ConfirmationRef,
    pub status: SimulationChallengeStatus,
    pub failures: u32,
    pub max_failures: u32,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
}
/// Data/reference only, no conversion to AuthGrantRef or ExecutionPermit.
/// ```compile_fail
/// fn funds(grant: &safeflow_storage::SimulationAuthGrantRecord) {
///     let _: &safeflow_contracts::ExecutionPermit = grant;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SimulationAuthGrantRecord {
    pub contract_version: u16,
    pub mode: &'static str,
    pub grant_id: SimulationGrantId,
    pub challenge_id: SimulationChallengeId,
    pub binding: ConfirmationBinding,
    pub confirmation_ref: ConfirmationRef,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub status: SimulationGrantStatus,
    pub revoked_at_ms: Option<i64>,
}
pub struct SimulationChallengeDelivery {
    record: SimulationChallengeRecord,
    secret: Option<SimulationSecret>,
}
impl SimulationChallengeDelivery {
    pub fn record(&self) -> &SimulationChallengeRecord {
        &self.record
    }
    /// Retries return None: never re-send a secret or reset attempts/expiry.
    pub fn take_secret(&mut self) -> Option<SimulationSecret> {
        self.secret.take()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SimulationVerificationResult {
    Verified,
    NotVerified,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SimulationSubmissionRecord {
    pub submission_id: SimulationSubmissionId,
    pub challenge_id: SimulationChallengeId,
    pub result: SimulationVerificationResult,
    pub failures: u32,
    pub submitted_at_ms: i64,
    pub grant: Option<SimulationAuthGrantRecord>,
}
struct ServerClock;
impl ConfirmationClock for ServerClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        SystemTime::now().into()
    }
}
struct ObservedClock(chrono::DateTime<chrono::Utc>);
impl ConfirmationClock for ObservedClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.0
    }
}

#[derive(Clone)]
pub struct SimulationAuthService {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
    clock: Arc<dyn ConfirmationClock>,
    config: SimulationAuthConfig,
    time_pool: PgPool,
}
impl SimulationAuthService {
    pub fn for_isolated_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: SimulationAuthConfig,
        mode: SimulationAuthMode,
    ) -> Result<Self, StorageError> {
        Self::for_isolated_backend_with_clock(
            repository,
            sessions,
            config,
            mode,
            Arc::new(ServerClock),
        )
    }
    pub fn for_isolated_backend_with_clock(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        config: SimulationAuthConfig,
        _mode: SimulationAuthMode,
        clock: Arc<dyn ConfirmationClock>,
    ) -> Result<Self, StorageError> {
        let suffix = repository
            .namespace
            .strip_prefix("sf_auth_sim_")
            .ok_or(StorageError::SimulationModeRequired)?;
        if suffix.len() != 32
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StorageError::SimulationModeRequired);
        }
        // A separate pool avoids waiting for a connection held by an authorization transaction.
        let time_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(repository.pool.connect_options().as_ref().clone());
        Ok(Self {
            repository,
            sessions,
            clock,
            config,
            time_pool,
        })
    }
    /// Formal policy/method and trusted identity evidence remain unconnected.
    pub async fn request_formal_auth(
        &self,
        session: &SessionRef,
        _task: &TaskId,
    ) -> Result<(), StorageError> {
        self.identity(session)?;
        Err(StorageError::FormalAuthUnavailable)
    }
    pub async fn create_challenge(
        &self,
        session: &SessionRef,
        task: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
    ) -> Result<SimulationChallengeDelivery, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = unit.create(operation, confirmation).await;
        unit.complete(result, false).await
    }
    pub async fn read_challenge(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationChallengeId,
    ) -> Result<SimulationChallengeRecord, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = async {
            let row = unit.checked(id).await?;
            unit.expected_grant = unit.grant_for(&row).await?;
            unit.expected = Some(row.clone());
            Ok(row.record)
        }
        .await;
        unit.complete(result, false).await
    }
    pub async fn submit(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationChallengeId,
        submission: &SimulationSubmissionId,
        proof: &SimulationProof,
    ) -> Result<SimulationSubmissionRecord, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = unit.submit(id, submission, proof).await;
        unit.complete(result, false).await
    }
    pub async fn read_grant(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationGrantId,
    ) -> Result<SimulationAuthGrantRecord, StorageError> {
        self.access_grant(session, task, id, None).await
    }
    pub async fn verify_grant(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationGrantId,
        expected: &ConfirmationBinding,
    ) -> Result<SimulationAuthGrantRecord, StorageError> {
        self.access_grant(session, task, id, Some(expected)).await
    }
    async fn access_grant(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationGrantId,
        expected: Option<&ConfirmationBinding>,
    ) -> Result<SimulationAuthGrantRecord, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = async {
            let (row, grant) = unit.grant(id).await?;
            unit.validate(&row).await?;
            check_grant(&row, &grant, unit.now().await?)?;
            if expected.is_some_and(|b| b != &row.record.binding) {
                return Err(StorageError::AuthBindingMismatch);
            }
            unit.expected = Some(row);
            unit.expected_grant = Some(grant.clone());
            unit.grant_required = true;
            Ok(grant)
        }
        .await;
        unit.complete(result, false).await
    }
    pub async fn revoke_challenge(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationChallengeId,
    ) -> Result<SimulationChallengeRecord, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = unit.revoke(id).await;
        unit.complete(result, true).await
    }
    pub async fn revoke_grant(
        &self,
        session: &SessionRef,
        task: &TaskId,
        id: &SimulationGrantId,
    ) -> Result<SimulationAuthGrantRecord, StorageError> {
        let mut unit = self.open(session, task).await?;
        let result = async {
            let (row, _) = unit.grant(id).await?;
            unit.revoke(&row.record.challenge_id).await?;
            Ok(unit
                .expected_grant
                .clone()
                .ok_or(StorageError::CorruptRecord)?)
        }
        .await;
        unit.complete(result, true).await
    }
    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        valid_id(session.as_str())?;
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        valid_id(binding.user_id.as_str())?;
        db_version(binding.binding_version)?;
        Ok(binding)
    }
    async fn open(&self, session: &SessionRef, task: &TaskId) -> Result<AuthUnit, StorageError> {
        valid_id(task.as_str())?;
        let identity = self.identity(session)?;
        let observed = self.observe_time().await?;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SELECT set_config('search_path',$1,true)")
            .bind(&self.repository.namespace)
            .execute(&mut *tx)
            .await?;
        let task = lock_task(&mut tx, &identity.user_id, task).await?;
        let unit = AuthUnit {
            tx: Some(tx),
            service: self.clone(),
            session: session.clone(),
            identity,
            task,
            expected: None,
            expected_grant: None,
            expected_submission: None,
            grant_required: false,
            last_time: Some(observed),
        };
        unit.session_guard()?;
        Ok(unit)
    }
    async fn observe_time(&self) -> Result<i64, StorageError> {
        // This transaction commits only a time high-water mark, never authentication state.
        // It never locks a task/challenge/grant, so task-first authorization cannot deadlock it.
        let table = format!(
            "\"{}\".simulation_auth_time_fence",
            self.repository.namespace
        );
        let mut tx = self.time_pool.begin().await?;
        sqlx::query(&format!("INSERT INTO {table} (scope,last_observed_ms) VALUES (1,0) ON CONFLICT (scope) DO NOTHING")).execute(&mut *tx).await?;
        let previous: i64 = sqlx::query_scalar(&format!(
            "SELECT last_observed_ms FROM {table} WHERE scope=1 FOR UPDATE"
        ))
        .fetch_one(&mut *tx)
        .await?;
        // Sample after any fence lock wait; a cached pre-wait timestamp is not authoritative.
        let now = timestamp(self.clock.as_ref())?.timestamp_millis();
        if now < 0 || now < previous {
            return Err(StorageError::Invalid("auth server clock moved backwards"));
        }
        sqlx::query(&format!(
            "UPDATE {table} SET last_observed_ms=$1 WHERE scope=1"
        ))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(now)
    }
}
#[derive(Clone, PartialEq, Eq)]
struct ChallengeRow {
    record: SimulationChallengeRecord,
    secret_digest: String,
    secret_bytes: usize,
    session_digest: String,
    session_version: u64,
    challenge_ttl_ms: i64,
    grant_ttl_ms: i64,
}
struct AuthUnit {
    tx: Option<Transaction<'static, Postgres>>,
    service: SimulationAuthService,
    session: SessionRef,
    identity: SessionBinding,
    task: TaskRecord,
    expected: Option<ChallengeRow>,
    expected_grant: Option<SimulationAuthGrantRecord>,
    expected_submission: Option<SubmissionRow>,
    grant_required: bool,
    last_time: Option<i64>,
}
#[derive(Clone, PartialEq, Eq)]
struct SubmissionRow {
    id: SimulationSubmissionId,
    challenge: SimulationChallengeId,
    input_digest: String,
    result: SimulationVerificationResult,
    failures: u32,
    grant: Option<SimulationGrantId>,
    submitted_at_ms: i64,
}
impl AuthUnit {
    fn tx(&mut self) -> Result<&mut Transaction<'static, Postgres>, StorageError> {
        self.tx.as_mut().ok_or(StorageError::TransactionAborted)
    }
    fn session_guard(&self) -> Result<(), StorageError> {
        if self.service.identity(&self.session)? != self.identity {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    fn session_digest(&self) -> Result<String, StorageError> {
        Ok(
            hash(&("safeflow-confirmation-session:v1", self.session.as_str()))?
                .as_str()
                .to_owned(),
        )
    }
    async fn now(&mut self) -> Result<i64, StorageError> {
        let now = self.service.observe_time().await?;
        if now < 0 || self.last_time.is_some_and(|last| now < last) {
            return Err(StorageError::Invalid("auth server clock moved backwards"));
        }
        self.last_time = Some(now);
        Ok(now)
    }
    async fn binding(
        &mut self,
        operation: &OperationId,
    ) -> Result<ConfirmationBinding, StorageError> {
        valid_id(operation.as_str())?;
        let owner = self.identity.user_id.clone();
        let id = self.task.task_id.clone();
        let task = lock_task(self.tx()?, &owner, &id).await?;
        let ops = operations(self.tx()?, &id).await?;
        current_binding(&task, &ops, operation)
    }
    async fn validate(&mut self, row: &ChallengeRow) -> Result<i64, StorageError> {
        check_time(
            row.record.created_at_ms,
            row.record.expires_at_ms,
            self.now().await?,
        )?;
        if row
            .record
            .completed_at_ms
            .is_some_and(|time| self.last_time.unwrap() < time)
        {
            return Err(StorageError::Invalid("auth clock precedes completion"));
        }
        let current = self.binding(&row.record.binding.operation_id).await?;
        if current != row.record.binding {
            return Err(StorageError::AuthBindingMismatch);
        }
        let digest = self.session_digest()?;
        let ver = self.identity.binding_version;
        // The legacy confirmation verifier sees an already-persisted time observation.
        let clock = ObservedClock(
            chrono::DateTime::from_timestamp_millis(self.now().await?)
                .ok_or(StorageError::CorruptRecord)?,
        );
        let expires = verify_stored_confirmation(
            self.tx()?,
            &row.record.confirmation_ref,
            &current,
            &digest,
            ver,
            &clock,
        )
        .await?;
        let now = self.now().await?;
        self.session_guard()?;
        if now >= expires.timestamp_millis() {
            return Err(StorageError::ConfirmationExpired);
        }
        Ok(expires.timestamp_millis())
    }
    async fn row(&mut self, id: &SimulationChallengeId) -> Result<ChallengeRow, StorageError> {
        let task = self.task.task_id.clone();
        let row=sqlx::query("SELECT * FROM simulation_auth_challenges WHERE task_id=$1 AND challenge_id=$2 FOR UPDATE")
            .bind(task.as_str()).bind(id.as_str()).fetch_optional(&mut **self.tx()?).await?.ok_or(StorageError::NotFound)?;
        let row = challenge_row(&row)?;
        if row.record.binding.user_id != self.identity.user_id
            || row.session_digest != self.session_digest()?
            || row.session_version != self.identity.binding_version
        {
            return Err(StorageError::NotFound);
        }
        Ok(row)
    }
    async fn checked(&mut self, id: &SimulationChallengeId) -> Result<ChallengeRow, StorageError> {
        let row = self.row(id).await?;
        self.validate(&row).await?;
        Ok(row)
    }
    async fn emit(
        &mut self,
        row: &ChallengeRow,
        kind: PersistentAuditKind,
        details: serde_json::Value,
    ) -> Result<(), StorageError> {
        let task = self.task.clone();
        let ops = operations(self.tx()?, &task.task_id).await?;
        audit(
            self.tx()?,
            &task,
            ops.iter()
                .find(|op| op.operation_id == row.record.binding.operation_id),
            kind,
            details,
        )
        .await?;
        Ok(())
    }
    async fn create(
        &mut self,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
    ) -> Result<SimulationChallengeDelivery, StorageError> {
        valid_id(confirmation.as_str())?;
        let binding = self.binding(operation).await?;
        let digest = self.session_digest()?;
        let ver = self.identity.binding_version;
        let clock = ObservedClock(
            chrono::DateTime::from_timestamp_millis(self.now().await?)
                .ok_or(StorageError::CorruptRecord)?,
        );
        let confirmation_expiry =
            verify_stored_confirmation(self.tx()?, confirmation, &binding, &digest, ver, &clock)
                .await?
                .timestamp_millis();
        let existing = sqlx::query(
            "SELECT * FROM simulation_auth_challenges WHERE operation_id=$1 FOR UPDATE",
        )
        .bind(operation.as_str())
        .fetch_optional(&mut **self.tx()?)
        .await?;
        if let Some(existing) = existing {
            let row = challenge_row(&existing)?;
            if row.record.binding != binding
                || row.record.confirmation_ref != *confirmation
                || row.session_digest != digest
                || row.session_version != ver
                || row.secret_bytes != self.service.config.secret_bytes
                || row.record.max_failures != self.service.config.max_failures
                || row.challenge_ttl_ms != self.service.config.challenge_ttl_ms
                || row.grant_ttl_ms != self.service.config.grant_ttl_ms
            {
                return Err(StorageError::AuthChallengeConflict);
            }
            self.validate(&row).await?;
            if !matches!(
                row.record.status,
                SimulationChallengeStatus::Pending | SimulationChallengeStatus::Succeeded
            ) {
                return Err(StorageError::AuthChallengeInactive);
            }
            self.expected_grant = self.grant_for(&row).await?;
            self.grant_required = row.record.status == SimulationChallengeStatus::Succeeded;
            self.expected = Some(row.clone());
            return Ok(SimulationChallengeDelivery {
                record: row.record,
                secret: None,
            });
        }
        let id = SimulationChallengeId::new_server_id();
        let created = self.now().await?;
        let expires =
            deadline(created, self.service.config.challenge_ttl_ms)?.min(confirmation_expiry);
        if expires <= created {
            return Err(StorageError::AuthExpired);
        }
        let mut secret =
            SimulationSecret(Zeroizing::new(vec![0; self.service.config.secret_bytes]));
        getrandom::getrandom(&mut secret.0)
            .map_err(|_| StorageError::Invalid("OS randomness unavailable"))?;
        let row = ChallengeRow {
            record: SimulationChallengeRecord {
                contract_version: 1,
                mode: MODE,
                challenge_id: id.clone(),
                binding,
                confirmation_ref: confirmation.clone(),
                status: SimulationChallengeStatus::Pending,
                failures: 0,
                max_failures: self.service.config.max_failures,
                created_at_ms: created,
                expires_at_ms: expires,
                completed_at_ms: None,
                revoked_at_ms: None,
            },
            secret_digest: material_digest(&id, &secret.0),
            secret_bytes: self.service.config.secret_bytes,
            session_digest: digest,
            session_version: ver,
            challenge_ttl_ms: self.service.config.challenge_ttl_ms,
            grant_ttl_ms: self.service.config.grant_ttl_ms,
        };
        let b = &row.record.binding;
        self.session_guard()?;
        sqlx::query("INSERT INTO simulation_auth_challenges (challenge_id,contract_version,mode,owner_id,task_id,operation_id,node_id,tool_name,task_revision,plan_version,operation_revision,facts_version,arguments_hash,binding_digest,session_digest,session_binding_version,confirmation_ref,secret_digest,secret_bytes,max_failures,failures,challenge_ttl_ms,grant_ttl_ms,created_at_ms,expires_at_ms,status) VALUES ($1,1,'FOUNDATION_ISOLATED_V1',$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,0,$19,$20,$21,$22,'PENDING')")
            .bind(id.as_str()).bind(b.user_id.as_str()).bind(b.task_id.as_str()).bind(b.operation_id.as_str()).bind(b.node_id.as_str()).bind(b.tool_name.as_str())
            .bind(db_version(b.task_revision)?).bind(b.plan_version as i64).bind(db_version(b.operation_revision)?).bind(db_version(b.facts_version)?)
            .bind(b.arguments_hash.as_str()).bind(hash(b)?.as_str()).bind(&row.session_digest).bind(db_version(ver)?).bind(confirmation.as_str())
            .bind(&row.secret_digest).bind(row.secret_bytes as i32).bind(row.record.max_failures as i32).bind(row.challenge_ttl_ms).bind(row.grant_ttl_ms).bind(created).bind(expires)
            .execute(&mut **self.tx()?).await?;
        self.emit(
            &row,
            PersistentAuditKind::SimAuthChallengeCreated,
            json!({"challenge_id":id,"mode":MODE}),
        )
        .await?;
        self.expected = Some(row.clone());
        Ok(SimulationChallengeDelivery {
            record: row.record,
            secret: Some(secret),
        })
    }
    async fn grant(
        &mut self,
        id: &SimulationGrantId,
    ) -> Result<(ChallengeRow, SimulationAuthGrantRecord), StorageError> {
        let task = self.task.task_id.clone();
        let cid:Option<String>=sqlx::query_scalar("SELECT g.challenge_id FROM simulation_auth_grants g JOIN simulation_auth_challenges c USING(challenge_id) WHERE g.grant_id=$1 AND c.task_id=$2")
            .bind(id.as_str()).bind(task.as_str()).fetch_optional(&mut **self.tx()?).await?;
        let row = self
            .row(&SimulationChallengeId::parse(
                cid.ok_or(StorageError::NotFound)?,
            )?)
            .await?;
        let grant = self.grant_for(&row).await?.ok_or(StorageError::NotFound)?;
        Ok((row, grant))
    }
    async fn grant_for(
        &mut self,
        row: &ChallengeRow,
    ) -> Result<Option<SimulationAuthGrantRecord>, StorageError> {
        let g =
            sqlx::query("SELECT * FROM simulation_auth_grants WHERE challenge_id=$1 FOR UPDATE")
                .bind(row.record.challenge_id.as_str())
                .fetch_optional(&mut **self.tx()?)
                .await?;
        g.map(|g| grant_row(&g, row)).transpose()
    }
    async fn submit(
        &mut self,
        id: &SimulationChallengeId,
        submission: &SimulationSubmissionId,
        proof: &SimulationProof,
    ) -> Result<SimulationSubmissionRecord, StorageError> {
        let mut row = self.checked(id).await?;
        if row.record.status == SimulationChallengeStatus::Revoked {
            return Err(StorageError::AuthChallengeInactive);
        }
        let input = material_digest(id, &proof.0);
        if let Some(attempt) = self.submission(submission).await? {
            if attempt.challenge != *id || !digest_eq(&attempt.input_digest, &input) {
                return Err(StorageError::AuthSubmissionConflict);
            }
            let grant = self.grant_for(&row).await?;
            let result = attempt.result;
            if result == SimulationVerificationResult::Verified {
                check_grant(
                    &row,
                    grant.as_ref().ok_or(StorageError::CorruptRecord)?,
                    self.now().await?,
                )?;
            }
            if attempt.grant
                != grant
                    .as_ref()
                    .filter(|_| result == SimulationVerificationResult::Verified)
                    .map(|g| g.grant_id.clone())
            {
                return Err(StorageError::CorruptRecord);
            }
            let record = SimulationSubmissionRecord {
                submission_id: submission.clone(),
                challenge_id: id.clone(),
                result,
                failures: attempt.failures,
                submitted_at_ms: attempt.submitted_at_ms,
                grant: if result == SimulationVerificationResult::Verified {
                    grant.clone()
                } else {
                    None
                },
            };
            self.expected = Some(row);
            self.expected_grant = grant;
            self.expected_submission = Some(attempt);
            self.grant_required = result == SimulationVerificationResult::Verified;
            return Ok(record);
        }
        if row.record.status != SimulationChallengeStatus::Pending {
            return Err(StorageError::AuthChallengeInactive);
        }
        let now = self.now().await?;
        let passed = proof.0.len() == row.secret_bytes && digest_eq(&row.secret_digest, &input);
        let mut grant = None;
        if passed {
            let confirmation_expiry = self.validate(&row).await?;
            let expires = deadline(now, row.grant_ttl_ms)?
                .min(row.record.expires_at_ms)
                .min(confirmation_expiry);
            if expires <= now {
                return Err(StorageError::AuthExpired);
            }
            let g = SimulationAuthGrantRecord {
                contract_version: 1,
                mode: MODE,
                grant_id: SimulationGrantId::new_server_id(),
                challenge_id: id.clone(),
                binding: row.record.binding.clone(),
                confirmation_ref: row.record.confirmation_ref.clone(),
                issued_at_ms: now,
                expires_at_ms: expires,
                status: SimulationGrantStatus::Active,
                revoked_at_ms: None,
            };
            sqlx::query("INSERT INTO simulation_auth_grants (grant_id,contract_version,mode,challenge_id,issued_at_ms,expires_at_ms,status) VALUES ($1,1,'FOUNDATION_ISOLATED_V1',$2,$3,$4,'ACTIVE')")
                .bind(g.grant_id.as_str()).bind(id.as_str()).bind(now).bind(expires).execute(&mut **self.tx()?).await?;
            row.record.status = SimulationChallengeStatus::Succeeded;
            row.record.completed_at_ms = Some(now);
            grant = Some(g);
        } else {
            row.record.failures = row
                .record
                .failures
                .checked_add(1)
                .ok_or(StorageError::CorruptRecord)?;
            if row.record.failures >= row.record.max_failures {
                row.record.status = SimulationChallengeStatus::Failed;
                row.record.completed_at_ms = Some(now);
            }
        }
        let result = if passed {
            SimulationVerificationResult::Verified
        } else {
            SimulationVerificationResult::NotVerified
        };
        let inserted=sqlx::query("INSERT INTO simulation_auth_submissions (submission_id,challenge_id,input_digest,result,failures,grant_id,submitted_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (submission_id) DO NOTHING")
            .bind(submission.as_str()).bind(id.as_str()).bind(&input).bind(encode(result)?).bind(row.record.failures as i32)
            .bind(grant.as_ref().map(|g|g.grant_id.as_str())).bind(now).execute(&mut **self.tx()?).await?;
        if inserted.rows_affected() != 1 {
            return Err(StorageError::AuthSubmissionConflict);
        }
        let updated=sqlx::query("UPDATE simulation_auth_challenges SET status=$1,failures=$2,completed_at_ms=$3 WHERE challenge_id=$4 AND status='PENDING'")
            .bind(encode(row.record.status)?).bind(row.record.failures as i32).bind(row.record.completed_at_ms).bind(id.as_str()).execute(&mut **self.tx()?).await?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::AuthChallengeInactive);
        }
        self.emit(&row,if passed {PersistentAuditKind::SimAuthVerified}else{PersistentAuditKind::SimAuthFailed},json!({"challenge_id":id,"submission_id":submission,"failures":row.record.failures,"mode":MODE})).await?;
        if let Some(g) = &grant {
            self.emit(
                &row,
                PersistentAuditKind::SimAuthGrantIssued,
                json!({"challenge_id":id,"grant_id":g.grant_id,"mode":MODE}),
            )
            .await?;
        }
        let record = SimulationSubmissionRecord {
            submission_id: submission.clone(),
            challenge_id: id.clone(),
            result,
            failures: row.record.failures,
            submitted_at_ms: now,
            grant: grant.clone(),
        };
        self.expected_submission = Some(SubmissionRow {
            id: submission.clone(),
            challenge: id.clone(),
            input_digest: input,
            result,
            failures: row.record.failures,
            grant: grant.as_ref().map(|g| g.grant_id.clone()),
            submitted_at_ms: now,
        });
        self.expected = Some(row);
        self.expected_grant = grant;
        self.grant_required = passed;
        Ok(record)
    }
    async fn submission(
        &mut self,
        id: &SimulationSubmissionId,
    ) -> Result<Option<SubmissionRow>, StorageError> {
        let row = sqlx::query(
            "SELECT * FROM simulation_auth_submissions WHERE submission_id=$1 FOR UPDATE",
        )
        .bind(id.as_str())
        .fetch_optional(&mut **self.tx()?)
        .await?;
        row.map(|r| {
            Ok(SubmissionRow {
                id: SimulationSubmissionId::parse(r.try_get::<String, _>("submission_id")?)?,
                challenge: SimulationChallengeId::parse(r.try_get::<String, _>("challenge_id")?)?,
                input_digest: r.try_get("input_digest")?,
                result: decode(&r, "result")?,
                failures: u32::try_from(r.try_get::<i32, _>("failures")?)
                    .map_err(|_| StorageError::CorruptRecord)?,
                grant: r
                    .try_get::<Option<String>, _>("grant_id")?
                    .map(SimulationGrantId::parse)
                    .transpose()?,
                submitted_at_ms: r.try_get("submitted_at_ms")?,
            })
        })
        .transpose()
    }
    async fn revoke(
        &mut self,
        id: &SimulationChallengeId,
    ) -> Result<SimulationChallengeRecord, StorageError> {
        let mut row = self.row(id).await?;
        let mut grant = self.grant_for(&row).await?;
        let now = self.now().await?;
        if now < row.record.created_at_ms
            || row.record.completed_at_ms.is_some_and(|time| now < time)
            || row.record.revoked_at_ms.is_some_and(|time| now < time)
        {
            return Err(StorageError::Invalid("auth clock precedes record"));
        }
        if row.record.status != SimulationChallengeStatus::Revoked {
            sqlx::query("UPDATE simulation_auth_challenges SET status='REVOKED',revoked_at_ms=$1 WHERE challenge_id=$2").bind(now).bind(id.as_str()).execute(&mut **self.tx()?).await?;
            row.record.status = SimulationChallengeStatus::Revoked;
            row.record.revoked_at_ms = Some(now);
            self.emit(
                &row,
                PersistentAuditKind::SimAuthChallengeRevoked,
                json!({"challenge_id":id,"mode":MODE}),
            )
            .await?;
        }
        if let Some(g) = grant.as_mut() {
            if now < g.issued_at_ms || g.revoked_at_ms.is_some_and(|time| now < time) {
                return Err(StorageError::Invalid("auth clock precedes grant"));
            }
            if g.status != SimulationGrantStatus::Revoked {
                sqlx::query("UPDATE simulation_auth_grants SET status='REVOKED',revoked_at_ms=$1 WHERE grant_id=$2").bind(now).bind(g.grant_id.as_str()).execute(&mut **self.tx()?).await?;
                g.status = SimulationGrantStatus::Revoked;
                g.revoked_at_ms = Some(now);
                self.emit(
                    &row,
                    PersistentAuditKind::SimAuthGrantRevoked,
                    json!({"challenge_id":id,"grant_id":g.grant_id,"mode":MODE}),
                )
                .await?;
            }
        }
        let record = row.record.clone();
        self.expected = Some(row);
        self.expected_grant = grant;
        Ok(record)
    }
    async fn complete<T>(
        mut self,
        result: Result<T, StorageError>,
        cleanup: bool,
    ) -> Result<T, StorageError> {
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                self.rollback().await;
                return Err(error);
            }
        };
        if let Err(error) = self.final_guard(cleanup).await {
            self.rollback().await;
            return Err(error);
        }
        self.tx
            .take()
            .ok_or(StorageError::TransactionAborted)?
            .commit()
            .await?;
        Ok(value)
    }
    async fn rollback(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.rollback().await;
        }
    }
    async fn final_guard(&mut self, cleanup: bool) -> Result<(), StorageError> {
        self.session_guard()?;
        let expected = self.expected.clone().ok_or(StorageError::CorruptRecord)?;
        let owner = self.identity.user_id.clone();
        let task = self.task.task_id.clone();
        if lock_task(self.tx()?, &owner, &task).await? != self.task {
            return Err(StorageError::AuthBindingMismatch);
        }
        let row = self.row(&expected.record.challenge_id).await?;
        if row != expected {
            return Err(StorageError::AuthBindingMismatch);
        }
        let confirmation_expiry = if cleanup {
            None
        } else {
            Some(self.validate(&row).await?)
        };
        if self.grant_for(&row).await? != self.expected_grant {
            return Err(StorageError::AuthBindingMismatch);
        }
        if let Some(submission) = self.expected_submission.clone() {
            if self.submission(&submission.id).await?.as_ref() != Some(&submission) {
                return Err(StorageError::AuthSubmissionConflict);
            }
        }
        self.session_guard()?;
        let now = self.now().await?;
        self.session_guard()?;
        if cleanup {
            if now < row.record.revoked_at_ms.unwrap_or(row.record.created_at_ms) {
                return Err(StorageError::Invalid("auth clock moved backwards"));
            }
        } else {
            check_time(row.record.created_at_ms, row.record.expires_at_ms, now)?;
            if now >= confirmation_expiry.ok_or(StorageError::CorruptRecord)? {
                return Err(StorageError::ConfirmationExpired);
            }
            if self.grant_required {
                check_grant(
                    &row,
                    self.expected_grant
                        .as_ref()
                        .ok_or(StorageError::CorruptRecord)?,
                    now,
                )?;
            }
        }
        Ok(())
    }
}
fn deadline(now: i64, ttl: i64) -> Result<i64, StorageError> {
    now.checked_add(ttl)
        .ok_or(StorageError::Invalid("auth expiry overflow"))
}
fn check_time(created: i64, expires: i64, now: i64) -> Result<(), StorageError> {
    if now < created {
        return Err(StorageError::Invalid("auth clock precedes record"));
    }
    if now >= expires {
        return Err(StorageError::AuthExpired);
    }
    Ok(())
}
fn check_grant(
    row: &ChallengeRow,
    grant: &SimulationAuthGrantRecord,
    now: i64,
) -> Result<(), StorageError> {
    if row.record.status != SimulationChallengeStatus::Succeeded
        || grant.status != SimulationGrantStatus::Active
    {
        return Err(StorageError::AuthGrantRevoked);
    }
    check_time(grant.issued_at_ms, grant.expires_at_ms, now)
}
fn material_digest(id: &SimulationChallengeId, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"safeflow-isolated-auth-material:v1\0");
    hash.update(id.as_str().as_bytes());
    hash.update([0]);
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}
fn digest_eq(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
fn check_mode(row: &PgRow) -> Result<(), StorageError> {
    if row.try_get::<i16, _>("contract_version")? != SIMULATION_AUTH_CONTRACT_VERSION as i16
        || row.try_get::<String, _>("mode")? != MODE
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}
fn challenge_row(row: &PgRow) -> Result<ChallengeRow, StorageError> {
    check_mode(row)?;
    let binding = legacy_binding_row(row)?;
    if hash(&binding)?.as_str() != row.try_get::<String, _>("binding_digest")? {
        return Err(StorageError::CorruptRecord);
    }
    Ok(ChallengeRow {
        record: SimulationChallengeRecord {
            contract_version: 1,
            mode: MODE,
            challenge_id: SimulationChallengeId::parse(row.try_get::<String, _>("challenge_id")?)?,
            binding,
            confirmation_ref: decode(row, "confirmation_ref")?,
            status: decode(row, "status")?,
            failures: u32::try_from(row.try_get::<i32, _>("failures")?)
                .map_err(|_| StorageError::CorruptRecord)?,
            max_failures: u32::try_from(row.try_get::<i32, _>("max_failures")?)
                .map_err(|_| StorageError::CorruptRecord)?,
            created_at_ms: row.try_get("created_at_ms")?,
            expires_at_ms: row.try_get("expires_at_ms")?,
            completed_at_ms: row.try_get("completed_at_ms")?,
            revoked_at_ms: row.try_get("revoked_at_ms")?,
        },
        secret_digest: row.try_get("secret_digest")?,
        secret_bytes: usize::try_from(row.try_get::<i32, _>("secret_bytes")?)
            .map_err(|_| StorageError::CorruptRecord)?,
        session_digest: row.try_get("session_digest")?,
        session_version: version(row, "session_binding_version")?,
        challenge_ttl_ms: row.try_get("challenge_ttl_ms")?,
        grant_ttl_ms: row.try_get("grant_ttl_ms")?,
    })
}
fn grant_row(
    row: &PgRow,
    challenge: &ChallengeRow,
) -> Result<SimulationAuthGrantRecord, StorageError> {
    check_mode(row)?;
    Ok(SimulationAuthGrantRecord {
        contract_version: 1,
        mode: MODE,
        grant_id: SimulationGrantId::parse(row.try_get::<String, _>("grant_id")?)?,
        challenge_id: challenge.record.challenge_id.clone(),
        binding: challenge.record.binding.clone(),
        confirmation_ref: challenge.record.confirmation_ref.clone(),
        issued_at_ms: row.try_get("issued_at_ms")?,
        expires_at_ms: row.try_get("expires_at_ms")?,
        status: decode(row, "status")?,
        revoked_at_ms: row.try_get("revoked_at_ms")?,
    })
}
