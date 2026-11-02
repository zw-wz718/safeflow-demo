use crate::models::{db_version, directive, hash, valid_id, validate_checkpoint, validate_plan};
use crate::runtime_recovery::RuntimeWaitEvidence;
use crate::{
    CheckpointChange, OperationChange, OperationRecord, PersistentAuditEvent, PersistentAuditKind,
    RecoveryPoint, RecoverySnapshot, StorageError, TaskRecord, PERSISTENCE_CONTRACT_VERSION,
};
use safeflow_contracts::{
    AgentPlan, EventId, NodeId, OperationEvent, OperationId, OperationState, PlanNode, TaskId,
    TaskState, UserId,
};
use safeflow_runtime::{VerifiedReadOnlyWait, READ_ONLY_WAIT_CONTRACT_VERSION};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::types::Json;
use sqlx::{Connection, PgPool, Postgres, Row, Transaction};
use std::borrow::Cow;
use std::future::Future;
use std::str::FromStr;

type Guard<'a> = &'a (dyn Fn() -> Result<(), StorageError> + Send + Sync);

/// Check on every resumption of a nested member future, including errors and
/// Ready. This guards internal member awaits without changing member sources.
pub(crate) async fn guarded_poll<T, E>(
    guard: Guard<'_>,
    wait: impl Future<Output = Result<T, E>>,
) -> Result<T, StorageError>
where
    StorageError: From<E>,
{
    let mut wait = std::pin::pin!(wait);
    std::future::poll_fn(|cx| {
        guard()?;
        let result = wait.as_mut().poll(cx);
        guard()?;
        result.map(|value| value.map_err(StorageError::from))
    })
    .await
}

// Check even failed waits before exposing their result. Dropping an uncommitted
// sqlx transaction rolls it back; commit-response uncertainty is documented.
pub(crate) async fn guarded<T, E>(
    guard: Guard<'_>,
    wait: impl Future<Output = Result<T, E>>,
) -> Result<T, StorageError>
where
    StorageError: From<E>,
{
    guard()?;
    let result = wait.await;
    guard()?;
    result.map_err(StorageError::from)
}

struct RuntimeCheckpoint<'a> {
    verified: &'a VerifiedReadOnlyWait,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskCreation {
    AllowExisting,
    RequireNewWithRejectionAudit,
    PlanningIntakeOnly,
    PlanningConfirmationOnly,
}

/// SQL and mutation access stay inside this implementation. Construct the Service
/// only in trusted backend assembly; do not expose it as a client/LLM Tool.
#[derive(Clone)]
pub struct PgRepository {
    pub(crate) pool: PgPool,
    pub(crate) lifecycle_time_pool: PgPool,
    pub(crate) subscription_read_pool: PgPool,
    pub(crate) simulation_source_pool: PgPool,
    pub(crate) revision_gate_pool: PgPool,
    pub(crate) namespace: String,
}

impl PgRepository {
    /// Selectors only, never confirmation or write authority. The owner is
    /// supplied by the authenticated backend. Never silently truncate history.
    pub async fn bank_recovery_for_backend(
        &self,
        owner: &UserId,
    ) -> Result<serde_json::Value, StorageError> {
        let mut tx = self.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx).await?;
        let tasks: Vec<String> = sqlx::query_scalar(
            "SELECT t.task_id FROM tasks t JOIN mock_draft_lifecycle_tasks l USING(task_id) WHERE t.owner_id=$1 ORDER BY t.created_at,t.task_id LIMIT 257")
            .bind(owner.as_str()).fetch_all(&mut *tx).await?;
        let subscriptions: Vec<String> = sqlx::query_scalar(
            "SELECT p.nonce FROM subscription_preparations_v1 p JOIN subscription_public_bindings_v1 b USING(business_ref) WHERE b.owner_id=$1 ORDER BY p.nonce LIMIT 257")
            .bind(owner.as_str()).fetch_all(&mut *tx).await?;
        if tasks.len() > 256 || subscriptions.len() > 256 {
            return Err(StorageError::DependencyUnavailable);
        }
        let mut closed_nonfunding_tasks = Vec::new();
        for task in &tasks {
            let id=TaskId::parse(task.clone()).map_err(|_|StorageError::CorruptRecord)?;
            if crate::lifecycle::closed_nonfunding_draft(&mut tx,owner,&id).await? {
                closed_nonfunding_tasks.push(task.clone());
            }
        }
        let mut ended_draft_intents=Vec::new();
        for task in &tasks {
            let id=TaskId::parse(task.clone()).map_err(|_|StorageError::CorruptRecord)?;
            if let Some(note)=crate::lifecycle::intent::note(&mut tx,owner,&id).await? {ended_draft_intents.push(note);}
        }
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *tx).await?;
        use sha2::{Digest, Sha256};
        let context_ref = format!("{:x}", Sha256::digest(format!("bank-recovery-v1:{database}:{}:{}", self.namespace, owner.as_str()).as_bytes()));
        tx.commit().await?;
        Ok(json!({"context_ref": context_ref, "payment_tasks": tasks, "subscription_nonces": subscriptions, "closed_nonfunding_tasks":closed_nonfunding_tasks,"ended_draft_intents":ended_draft_intents}))
    }
    pub async fn is_lifecycle_task(
        &self,
        owner: &UserId,
        id: &TaskId,
    ) -> Result<bool, StorageError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 AND owner_id=$2")
            .bind(id.as_str())
            .bind(owner.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StorageError::NotFound)?;
        let lifecycle = crate::lifecycle::is_lifecycle_task(&mut tx, id).await?;
        tx.commit().await?;
        Ok(lifecycle)
    }
    pub async fn connect(database_url: &str, namespace: &str) -> Result<Self, StorageError> {
        Self::connect_inner(database_url, namespace, true).await
    }

    /// Opens an already provisioned namespace without creating or migrating it.
    pub async fn connect_existing(
        database_url: &str,
        namespace: &str,
    ) -> Result<Self, StorageError> {
        Self::connect_inner(database_url, namespace, false).await
    }

    async fn connect_inner(
        database_url: &str,
        namespace: &str,
        create: bool,
    ) -> Result<Self, StorageError> {
        if namespace.is_empty()
            || namespace.len() > 63
            || !namespace
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase())
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            || namespace == "public"
            || namespace == "information_schema"
            || namespace.starts_with("pg_")
        {
            return Err(StorageError::Invalid("unsafe storage namespace"));
        }
        let options =
            PgConnectOptions::from_str(database_url).map_err(|_| StorageError::Database)?;
        // One dedicated connection for transaction-scoped revision nonce gates.
        // A prepare holds the gate across existing writer transactions, so it
        // must never consume one of the two writer connections while waiting
        // to acquire another. No session locks, table writes or new grants.
        let revision_gate_pool=PgPoolOptions::new().max_connections(1)
            .connect_lazy_with(options.clone());
        let verification_path = namespace.to_owned();
        // Independent, SELECT-only backend. Writers never queue a read behind
        // another owner-locked writer in the two-connection mutation pool.
        let subscription_read_pool = PgPoolOptions::new().max_connections(1)
            .after_connect(move |connection,_| {
                let path=verification_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false),set_config('default_transaction_read_only','on',false)")
                        .bind(path).execute(connection).await?;
                    Ok(())
                })
            }).connect_lazy_with(options.clone());
        let search_path = namespace.to_owned();
        // Captures retain a short SHARE lease through publication. They must
        // never consume the two writer connections needed by their consumers.
        let source_path=namespace.to_owned();
        // Two admitted monthly computations each retain three source SHARE leases.
        let simulation_source_pool=PgPoolOptions::new().max_connections(6)
            .after_connect(move |connection,_| {let path=source_path.clone();Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)").bind(path).execute(connection).await?;Ok(())
            })}).connect_lazy_with(options.clone());
        // All lifecycle request services share this task-lock-free time connection.
        let lifecycle_time_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(options.clone());
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;
        let version: i32 =
            sqlx::query_scalar("SELECT current_setting('server_version_num')::integer")
                .fetch_one(&pool)
                .await?;
        if !(160000..170000).contains(&version) {
            return Err(StorageError::Invalid("this slice requires PostgreSQL 16"));
        }
        if create {
            // Identifier interpolation is limited to the validated namespace above.
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{namespace}\""))
                .execute(&pool)
                .await?;
        } else {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
                    .bind(namespace)
                    .fetch_one(&pool)
                    .await?;
            if !exists {
                return Err(StorageError::NotFound);
            }
        }
        Ok(Self {
            pool,
            lifecycle_time_pool,
            subscription_read_pool,
            simulation_source_pool,
            revision_gate_pool,
            namespace: namespace.to_owned(),
        })
    }

    pub async fn migrate(&self) -> Result<(), StorageError> {
        // PostgreSQL DDL and the verified forward binding must become durable
        // together. SQLx uses nested savepoints on this connection; a crash
        // cannot leave migration 36 committed without the old card binding.
        let mut maintenance=self.pool.begin().await?;
        // Maintenance window only. Capture a VALID old card deployment before
        // additive DDL changes its object identity. Never rebind a stale or
        // corrupt predecessor, overwrite its digest or weaken an old binary.
        let prior: Option<(String,String,String)> = {
            let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(format!("{}.public_card_final_deployment_v1",self.namespace))
                .fetch_one(&mut *maintenance).await?;
            if exists {
                let deployed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_final_deployment_v1) AND NOT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=39 AND success)").fetch_one(&mut *maintenance).await?;
                if deployed{return Err(StorageError::Invalid("persistent reminders candidate requires a new isolated database; installed deployment upgrade is not enabled"));}
                let pending: bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=38 AND success)").fetch_one(&mut *maintenance).await?;
                if pending {
                    // An inactive writer still has authorized history readers.
                    // Forward its verified binding too, without reactivating it.
                    let has36:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=36 AND success)").fetch_one(&mut *maintenance).await?;
                    let query=if has36 {"SELECT deployment_id::text,object_digest,public_card_final_expected_digest_v1(deployment_id,object_digest),public_card_final_objects_v1() FROM public_card_final_deployment_v1 WHERE singleton=1 FOR UPDATE"}else{"SELECT deployment_id::text,object_digest,object_digest,public_card_final_objects_v1() FROM public_card_final_deployment_v1 WHERE singleton=1 FOR UPDATE"};
                    let row:Option<(String,String,String,String)>=sqlx::query_as(query).fetch_optional(&mut *maintenance).await?;
                    if let Some((id,original,expected,actual))=row {
                        if expected!=actual {return Err(StorageError::DecisionAssemblyChanged);}
                        Some((id,original,expected))
                    }else{None}
                }else{None}
            }else{None}
        };
        let prior42: Option<(String,String,String)> = {
            let exists:bool=sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(format!("{}.public_card_final_deployment_v1",self.namespace)).fetch_one(&mut *maintenance).await?;
            if exists {
                sqlx::query("LOCK TABLE public_card_final_deployment_v1 IN ACCESS EXCLUSIVE MODE").execute(&mut *maintenance).await?;
                let row:Option<(String,String,String,String,bool)>=sqlx::query_as("SELECT deployment_id::text,object_digest,public_card_final_expected_digest_v1(deployment_id,object_digest),public_card_final_objects_v1(),reader_version='card-final-v1' AND source_id=$1 AND implementation=$2 AND schema_oid=(SELECT oid FROM pg_namespace WHERE nspname=current_schema()) AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()) FROM public_card_final_deployment_v1 WHERE singleton=1 FOR UPDATE")
                    .bind(crate::UNIFIED_FICTIONAL_SOURCE).bind(crate::UNIFIED_FICTIONAL_IMPLEMENTATION).fetch_optional(&mut *maintenance).await?;
                if let Some((id,original,expected,actual,identity))=row {
                    if !identity || expected!=actual{return Err(StorageError::DecisionAssemblyChanged);}
                    let version:i64=sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success").fetch_one(&mut *maintenance).await?;
                    match version {
                        40=>Some((id,original,expected)),
                        42=>None,
                        _=>return Err(StorageError::Invalid("recipient upgrade requires verified v40 or already bound v42; remain quiesced")),
                    }
                }else{None}
            }else{None}
        };
        let initial = sqlx::migrate::Migration::new(
            1,
            "recovery points".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0001_recovery_points.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let plan_constraint = sqlx::migrate::Migration::new(
            2,
            "plan identity version".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0002_plan_identity_version.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let confirmations = sqlx::migrate::Migration::new(
            3,
            "server confirmations".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0003_server_confirmations.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let permits = sqlx::migrate::Migration::new(
            4,
            "execution permit store".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0004_execution_permits.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let auth = sqlx::migrate::Migration::new(
            5,
            "isolated simulation authentication".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0005_simulation_auth.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let decisions = sqlx::migrate::Migration::new(
            6,
            "preflight security decisions".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0006_security_decisions.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let permit_gates = sqlx::migrate::Migration::new(
            7,
            "permit authentication gates".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0007_permit_authentication_gates.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let precise_decision_fence = sqlx::migrate::Migration::new(
            8,
            "decision microsecond time fence".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0008_decision_microsecond_fence.sql")
                .replace("\r\n", "\n")
                .into(),
            false,
        );
        let migrator = sqlx::migrate::Migrator {
            migrations: Cow::Owned(vec![
                initial,
                plan_constraint,
                confirmations,
                permits,
                auth,
                decisions,
                permit_gates,
                precise_decision_fence,
                sqlx::migrate::Migration::new(
                    9,
                    "read only runtime claims".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0009_read_only_runtime_claims.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    10,
                    "read only runtime continuations".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0010_read_only_runtime_continuations.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    11,
                    "planning confirmation preparations".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0011_planning_confirmation_preparations.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    12,
                    "persistent mock draft execution".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0012_persistent_mock_draft_execution.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    13,
                    "same task mock draft lifecycle v2".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0013_mock_draft_lifecycle_v2.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    14,
                    "public execution requests".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0014_public_execution_requests.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    15,
                    "transfer business orders v1".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0015_transfer_business_orders_v1.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    16,
                    "transfer funds v1".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0016_transfer_funds_v1.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    17,
                    "public audit outbox".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0017_public_audit_outbox.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    18,
                    "permit execution scope".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0018_permit_execution_scope.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    19,
                    "payment preparation".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0019_payment_preparation.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    20,
                    "payment minimum decisions".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0020_payment_minimum_decisions.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    21,
                    "payment execution links".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0021_payment_execution_links.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    22,
                    "payment business observations".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0022_payment_business_preflight.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    23,
                    "payment agent bindings".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0023_payment_agent_bindings.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    24,
                    "card catalog, freeze store and adopted sources".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0024_card_catalog_and_freeze.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    25,
                    "billing feedback and trusted original reports".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0025_billing_feedback.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    26,
                    "subscription full journal and public bindings".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0026_subscription_journal.sql")
                        .replace("__SCHEMA__", &self.namespace)
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    27,
                    "billing preparation recovery and admission".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0027_billing_prepare_recovery.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    28,
                    "candidate billing feedback domain audit".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0028_billing_domain_audit.sql")
                        .replace("\r\n", "\n")
                        .into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    29,
                    "isolated subscription A2 domain candidate".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0029_subscription_a2_audit.sql").replace("\r\n", "\n").into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    30,
                    "local persistent simulation sources".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0030_simulation_sources.sql").replace("\r\n", "\n").into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    31,
                    "unadopted payment completion persistence candidate".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0031_payment_completion_candidate.sql").replace("\r\n", "\n").into(),
                    false,
                ),
                sqlx::migrate::Migration::new(
                    32,
                    "isolated unified fictional source provenance".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0032_unified_fictional_source.sql").replace("\r\n", "\n").into(),
                    false,
                ),
                sqlx::migrate::Migration::new(33, "isolated card public safety".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0033_card_public_safety.sql").replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(34, "explicit background acceptance".into(),
                    sqlx::migrate::MigrationType::Simple,
                    include_str!("../migrations/0034_payment_background_acceptance.sql").replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(35, "card final transaction".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0035_card_final_transaction.sql").replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(36, "payment final transaction".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0036_payment_final_transaction.sql").replace("__SCHEMA__", &self.namespace).replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(37, "subscription public evidence".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0037_subscription_public_evidence.sql").replace("__SCHEMA__", &self.namespace).replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(38, "simulation source management authority".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0038_simulation_source_authority.sql").replace("__SCHEMA__", &self.namespace).replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(39, "persistent simulated reminders".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0039_persistent_simulated_reminders.sql").replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(40, "automatic simulated settlement".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0040_automatic_simulated_settlement.sql").replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(41, "recipient profile".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0041_recipient_profile.sql").replace("\r\n", "\n").into(), false),
                sqlx::migrate::Migration::new(42, "recipient card forward binding".into(), sqlx::migrate::MigrationType::Simple, include_str!("../migrations/0042_recipient_card_forward.sql").replace("__SCHEMA__", &self.namespace).replace("\r\n", "\n").into(), false),
            ]),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        if let Some((deployment,original_digest,old_digest))=&prior {
            let needs37:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=37 AND success)").fetch_one(&mut *maintenance).await?;
            if needs37 {
                // Freeze the real intermediate 37 object identity before 38
                // changes it. Both phases and both bindings share this tx.
                let through37=sqlx::migrate::Migrator {
                    migrations: std::borrow::Cow::Owned(migrator.migrations.iter().filter(|m|m.version<=37).cloned().collect()),
                    ..sqlx::migrate::Migrator::DEFAULT
                };
                through37.run(&mut *maintenance).await.map_err(|_|StorageError::Migration)?;
                sqlx::query("INSERT INTO public_card_final_forward37_v1(deployment_id,original_digest,from_digest,to_digest,migration_version,migration_checksum) SELECT $1::uuid,$2,$3,public_card_final_objects_v1(),37,checksum FROM _sqlx_migrations WHERE version=37 AND success")
                    .bind(deployment).bind(original_digest).bind(old_digest).execute(&mut *maintenance).await?;
            }
        }
        let prior38=if let Some((deployment,original_digest,_))=&prior {
            let row:(String,String)=sqlx::query_as("SELECT public_card_final_expected_digest_v1($1::uuid,$2),public_card_final_objects_v1()")
                .bind(deployment).bind(original_digest).fetch_one(&mut *maintenance).await?;
            if row.0!=row.1 {return Err(StorageError::DecisionAssemblyChanged);}
            Some(row.0)
        }else{None};
        migrator
            .run(&mut *maintenance)
            .await
            .map_err(|_| StorageError::Migration)?;
        if let Some((deployment,original_digest,_))=prior {
            let old_digest=prior38.ok_or(StorageError::DecisionAssemblyChanged)?;
            // No old SQL/checksum/deployment/forward binding is rewritten.
            sqlx::query("INSERT INTO public_card_final_forward38_v1(deployment_id,original_digest,from_digest,to_digest,migration_version,migration_checksum) SELECT $1::uuid,$2,$3,public_card_final_objects_v1(),38,checksum FROM _sqlx_migrations WHERE version=38 AND success")
                .bind(deployment).bind(original_digest).bind(old_digest).execute(&mut *maintenance).await?;
        }
        if let Some((deployment,original_digest,old_digest))=prior42 {
            let result=sqlx::query("INSERT INTO public_card_final_forward42_v1(deployment_id,original_digest,from_digest,to_digest,predecessor_version,predecessor_checksum,migration_version,migration_checksum) SELECT $1::uuid,$2,$3,public_card_final_objects_v1(),40,p.checksum,42,m.checksum FROM _sqlx_migrations p CROSS JOIN _sqlx_migrations m WHERE p.version=40 AND p.success AND m.version=42 AND m.success")
                .bind(deployment).bind(original_digest).bind(old_digest).execute(&mut *maintenance).await?;
            if result.rows_affected()!=1{return Err(StorageError::DecisionAssemblyChanged);}
            let valid:bool=sqlx::query_scalar("SELECT public_card_final_expected_digest_v1(deployment_id,object_digest)=public_card_final_objects_v1() FROM public_card_final_deployment_v1 WHERE singleton=1").fetch_one(&mut *maintenance).await?;
            if !valid{return Err(StorageError::DecisionAssemblyChanged);}
        }
        maintenance.commit().await.map_err(|_|StorageError::ReconcileRequired)?;
        // Migrations run only in a writer-off maintenance window. Additive
        // columns change SELECT * result types; discard every resident prepared
        // statement after the schema change, including verification connections.
        for pool in [&self.pool,&self.lifecycle_time_pool,&self.subscription_read_pool,&self.simulation_source_pool,&self.revision_gate_pool] {
            let mut connections=Vec::new();
            for _ in 0..pool.size(){connections.push(pool.acquire().await?);}
            for mut connection in connections {connection.clear_cached_statements().await?;}
        }
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
        self.lifecycle_time_pool.close().await;
        self.subscription_read_pool.close().await;
        self.simulation_source_pool.close().await;
        self.revision_gate_pool.close().await;
    }

    /// Backend readiness only; no task, identity, business result or permission is read.
    pub async fn check_available(&self) -> Result<(), StorageError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn create_task(
        &self,
        owner: &UserId,
        plan: AgentPlan,
        facts_version: u64,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        self.create_task_inner(
            owner,
            plan,
            facts_version,
            TaskCreation::AllowExisting,
            None,
            guard,
        )
        .await
    }

    pub(crate) async fn create_runtime_task(
        &self,
        owner: &UserId,
        plan: AgentPlan,
        facts_version: u64,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        self.create_task_inner(
            owner,
            plan,
            facts_version,
            TaskCreation::RequireNewWithRejectionAudit,
            None,
            guard,
        )
        .await
    }

    pub(crate) async fn create_planning_task(
        &self,
        review: &safeflow_runtime::PlanningPersistenceReview<'_>,
        plan: AgentPlan,
        facts_version: u64,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        self.create_task_inner(
            review.owner(),
            plan,
            facts_version,
            TaskCreation::PlanningIntakeOnly,
            Some(review),
            guard,
        )
        .await
    }

    pub(crate) async fn create_planning_confirmation_task(
        &self,
        review: &safeflow_runtime::PlanningPersistenceReview<'_>,
        plan: AgentPlan,
        facts_version: u64,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        self.create_task_inner(
            review.owner(),
            plan,
            facts_version,
            TaskCreation::PlanningConfirmationOnly,
            Some(review),
            guard,
        )
        .await
    }

    async fn create_task_inner(
        &self,
        owner: &UserId,
        plan: AgentPlan,
        facts_version: u64,
        mode: TaskCreation,
        planning_review: Option<&safeflow_runtime::PlanningPersistenceReview<'_>>,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        validate_plan(&plan)?;
        valid_id(owner.as_str())?;
        db_version(facts_version)?;
        let digest = hash(&(plan.clone(), facts_version))?;
        let first = plan
            .nodes
            .iter()
            .find(|node| node.depends_on().is_empty())
            .ok_or(StorageError::Invalid("no entry node"))?;
        let task = TaskRecord {
            task_id: plan.task_id.clone(),
            owner_id: owner.clone(),
            state: TaskState::Running,
            revision: 1,
            facts_version,
            checkpoint: RecoveryPoint::AtNode {
                node_id: first.id().clone(),
            },
            plan,
            completed_nodes: vec![],
        };
        let mut tx = guarded(guard, self.pool.begin()).await?;
        crate::permit::reject_billing_recovery_generic(&mut tx, &task).await?;
        let result = guarded(guard, sqlx::query("INSERT INTO tasks (task_id, owner_id, contract_version, state, revision, plan_version, facts_version, plan, creation_digest, checkpoint, completed_nodes) VALUES ($1,$2,$3,$4,1,$5,$6,$7,$8,$9,$10) ON CONFLICT (task_id) DO NOTHING")
            .bind(task.task_id.as_str()).bind(owner.as_str()).bind(PERSISTENCE_CONTRACT_VERSION as i16)
            .bind(encode(task.state)?).bind(task.plan.plan_version as i64).bind(db_version(facts_version)?)
            .bind(Json(&task.plan)).bind(digest.as_str()).bind(Json(&task.checkpoint)).bind(Json(&task.completed_nodes))
            .execute(&mut *tx)).await?;
        if result.rows_affected() == 0 {
            let row = guarded(
                guard,
                sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE")
                    .bind(task.task_id.as_str())
                    .bind(owner.as_str())
                    .fetch_optional(&mut *tx),
            )
            .await?
            .ok_or(StorageError::NotFound)?;
            let stored = task_row(&row)?;
            if matches!(
                mode,
                TaskCreation::PlanningIntakeOnly | TaskCreation::PlanningConfirmationOnly
            ) {
                return Err(StorageError::IdConflict);
            }
            if mode == TaskCreation::RequireNewWithRejectionAudit
                || row.try_get::<String, _>("creation_digest")? != digest.as_str()
            {
                return reject(tx, &stored, None, StorageError::IdConflict, guard).await;
            }
            guarded(guard, tx.commit()).await?;
            return Ok(stored);
        }
        let planning_origin = planning_review
            .map(|review| -> Result<serde_json::Value, StorageError> {
                Ok(json!({"session_hash": hash(&review.session_ref())?,
                "binding_version": review.session_binding().binding_version,
                "registration_hash": review.registration_hash()}))
            })
            .transpose()?;
        let mut details = json!({"plan_version": task.plan.plan_version, "creation_digest": digest,
            "planning_origin": planning_origin});
        if let Some(review) = planning_review {
            details["planning_gateway_source"] = serde_json::to_value(review.gateway_source())
                .map_err(|_| StorageError::CorruptRecord)?;
        }
        if mode == TaskCreation::PlanningConfirmationOnly {
            let review = planning_review.ok_or(StorageError::RuntimeIntakeRequired)?;
            let origin = crate::planning_confirmation::record_created_request(
                &mut tx,
                &task,
                &review.registration_hash(),
            )
            .await?;
            details["planning_confirmation_origin"] = origin;
        }
        guarded(
            guard,
            audit(
                &mut tx,
                &task,
                None,
                PersistentAuditKind::TaskCreated,
                details,
            ),
        )
        .await?;
        if matches!(
            mode,
            TaskCreation::PlanningIntakeOnly | TaskCreation::PlanningConfirmationOnly
        ) {
            let row = guarded(
                guard,
                sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE")
                    .bind(task.task_id.as_str())
                    .bind(owner.as_str())
                    .fetch_one(&mut *tx),
            )
            .await?;
            let ops = guarded(guard, operations(&mut tx, &task.task_id)).await?;
            if task_row(&row)? != task
                || row.try_get::<String, _>("creation_digest")? != digest.as_str()
                || !ops.is_empty()
            {
                return Err(StorageError::CorruptRecord);
            }
        }
        guarded(guard, tx.commit()).await?;
        Ok(task)
    }

    pub(crate) async fn recover(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        guard: Guard<'_>,
    ) -> Result<RecoverySnapshot, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        // All writes lock the task first. FOR SHARE gives one coherent aggregate.
        let row = guarded(
            guard,
            sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR SHARE")
                .bind(task_id.as_str())
                .bind(owner.as_str())
                .fetch_optional(&mut *tx),
        )
        .await?
        .ok_or(StorageError::NotFound)?;
        let task = task_row(&row)?;
        guarded(guard, crate::lifecycle::check_mode(&mut tx, task_id, false)).await?;
        let operations = guarded(guard, operations(&mut tx, task_id)).await?;
        let rows = guarded(
            guard,
            sqlx::query("SELECT * FROM audit_events WHERE task_id=$1 ORDER BY sequence")
                .bind(task_id.as_str())
                .fetch_all(&mut *tx),
        )
        .await?;
        let audit = rows.iter().map(audit_row).collect::<Result<Vec<_>, _>>()?;
        let directive = directive(&task, &operations)?;
        guarded(guard, tx.commit()).await?;
        Ok(RecoverySnapshot {
            task,
            operations,
            audit,
            directive,
        })
    }

    pub(crate) async fn checkpoint(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        change: CheckpointChange,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        self.checkpoint_inner(owner, task_id, change, None, guard)
            .await
    }

    pub(crate) async fn checkpoint_runtime_wait(
        &self,
        initial: &TaskRecord,
        verified: &VerifiedReadOnlyWait,
        guard: &(dyn Fn() -> Result<(), StorageError> + Send + Sync),
    ) -> Result<TaskRecord, StorageError> {
        self.checkpoint_inner(
            verified.owner(),
            &initial.task_id,
            CheckpointChange {
                expected_revision: initial.revision,
                event: Some(safeflow_contracts::TaskEvent::AwaitUser),
                checkpoint: RecoveryPoint::AtNode {
                    node_id: verified.node_id().clone(),
                },
                completed_nodes: verified.completed_nodes().to_vec(),
                facts_version: verified.facts_version(),
            },
            Some(RuntimeCheckpoint { verified }),
            guard,
        )
        .await
    }

    async fn checkpoint_inner(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        change: CheckpointChange,
        runtime: Option<RuntimeCheckpoint<'_>>,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let old = guarded(guard, lock_task(&mut tx, owner, task_id)).await?;
        reject_mock_execution_claim(&mut tx, task_id).await?;
        let ops = guarded(guard, operations(&mut tx, task_id)).await?;
        let prepare = || {
            check_revision(&old, change.expected_revision)?;
            check_facts(old.facts_version, change.facts_version)?;
            if old.state.is_terminal() {
                return Err(StorageError::Terminal);
            }
            if let Some(runtime) = &runtime {
                if old.plan != *runtime.verified.plan()
                    || old.facts_version != runtime.verified.facts_version()
                    || old.owner_id != *runtime.verified.owner()
                {
                    return Err(StorageError::PlanConflict);
                }
                if old.state != TaskState::Running
                    || !old.completed_nodes.is_empty()
                    || !ops.is_empty()
                {
                    return Err(StorageError::Invalid(
                        "Runtime wait requires fresh read-only task",
                    ));
                }
            }
            if old
                .completed_nodes
                .iter()
                .any(|id| !change.completed_nodes.contains(id))
            {
                return Err(StorageError::Invalid(
                    "completed progress cannot be removed",
                ));
            }
            let mut next = old.clone();
            if let Some(event) = change.event {
                next.state = old
                    .state
                    .transition(event)
                    .map_err(|_| StorageError::IllegalTransition)?;
            }
            next.revision = next_revision(old.revision)?;
            next.facts_version = change.facts_version;
            next.checkpoint = change.checkpoint.clone();
            next.completed_nodes = change.completed_nodes.clone();
            validate_checkpoint(&next, &ops)?;
            Ok(next)
        };
        let next = match prepare() {
            Ok(next) => next,
            Err(error) => return reject(tx, &old, None, error, guard).await,
        };
        guarded(guard, update_task(&mut tx, &next, old.revision)).await?;
        let mut details = json!({"from": old.state, "to": next.state, "event": change.event, "checkpoint": next.checkpoint, "completed_nodes": next.completed_nodes, "facts_version": next.facts_version});
        if let Some(runtime) = &runtime {
            details["runtime_wait"] = json!(RuntimeWaitEvidence {
                contract_version: READ_ONLY_WAIT_CONTRACT_VERSION,
                bindings_hash: runtime.verified.bindings_hash().clone(),
                results_hash: runtime.verified.results_hash().clone(),
                task_hash: hash(&next)?,
            });
        }
        guarded(
            guard,
            audit(
                &mut tx,
                &next,
                None,
                PersistentAuditKind::CheckpointSaved,
                details,
            ),
        )
        .await?;
        guarded(guard, tx.commit()).await?;
        Ok(next)
    }

    pub(crate) async fn replace_plan(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        expected_revision: u64,
        plan: AgentPlan,
        facts_version: u64,
        guard: Guard<'_>,
    ) -> Result<TaskRecord, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let old = guarded(guard, lock_task(&mut tx, owner, task_id)).await?;
        reject_mock_execution_claim(&mut tx, task_id).await?;
        let ops = guarded(guard, operations(&mut tx, task_id)).await?;
        let prepare = || {
            check_revision(&old, expected_revision)?;
            validate_plan(&plan)?;
            check_facts(old.facts_version, facts_version)?;
            if old.state.is_terminal() {
                return Err(StorageError::Terminal);
            }
            if plan.task_id != old.task_id {
                return Err(StorageError::Invalid("plan task mismatch"));
            }
            if plan.plan_version < old.plan.plan_version {
                return Err(StorageError::StalePlan);
            }
            if plan.plan_version == old.plan.plan_version {
                if plan != old.plan {
                    return Err(StorageError::PlanConflict);
                }
                if facts_version != old.facts_version {
                    return Err(StorageError::FactsConflict);
                }
                return Ok(old.clone());
            }
            if !ops.is_empty() {
                return Err(StorageError::Invalid(
                    "replanning after operation creation is outside this slice",
                ));
            }
            for id in &old.completed_nodes {
                if old.plan.nodes.iter().find(|n| n.id() == id)
                    != plan.nodes.iter().find(|n| n.id() == id)
                {
                    return Err(StorageError::Invalid("completed node changed"));
                }
            }
            let mut next = old.clone();
            next.plan = plan.clone();
            next.facts_version = facts_version;
            next.revision = next_revision(old.revision)?;
            validate_checkpoint(&next, &ops)?;
            Ok(next)
        };
        let next = match prepare() {
            Ok(next) => next,
            Err(error) => return reject(tx, &old, None, error, guard).await,
        };
        if next != old {
            guarded(guard, update_task(&mut tx, &next, old.revision)).await?;
            guarded(guard, audit(&mut tx, &next, None, PersistentAuditKind::PlanReplaced, json!({"from_plan_version": old.plan.plan_version, "to_plan_version": next.plan.plan_version, "plan_digest": hash(&next.plan)?}))).await?;
        }
        guarded(guard, tx.commit()).await?;
        Ok(next)
    }

    pub(crate) async fn create_operation(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        expected_task_revision: u64,
        operation_id: OperationId,
        node_id: NodeId,
        plan_version: u32,
        guard: Guard<'_>,
    ) -> Result<OperationRecord, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let old = guarded(guard, lock_task(&mut tx, owner, task_id)).await?;
        reject_mock_execution_claim(&mut tx, task_id).await?;
        let mut ops = guarded(guard, operations(&mut tx, task_id)).await?;
        let prepare = || {
            check_revision(&old, expected_task_revision)?;
            valid_id(operation_id.as_str())?;
            valid_id(node_id.as_str())?;
            if old.state.is_terminal() {
                return Err(StorageError::Terminal);
            }
            if plan_version != old.plan.plan_version {
                return Err(StorageError::StalePlan);
            }
            if ops.iter().any(|op| {
                matches!(
                    op.state,
                    OperationState::Submitted | OperationState::Processing
                )
            }) {
                return Err(StorageError::ReconcileRequired);
            }
            let node = old
                .plan
                .nodes
                .iter()
                .find(|n| n.id() == &node_id)
                .ok_or(StorageError::Invalid("operation node missing"))?;
            let PlanNode::Tool { arguments, .. } = node else {
                return Err(StorageError::Invalid("operation requires a tool node"));
            };
            Ok(OperationRecord {
                operation_id: operation_id.clone(),
                task_id: task_id.clone(),
                node_id: node_id.clone(),
                state: OperationState::Draft,
                revision: 1,
                plan_version,
                facts_version: old.facts_version,
                arguments_hash: hash(arguments)?,
                business_operation_ref: None,
            })
        };
        let operation = match prepare() {
            Ok(op) => op,
            Err(error) => return reject(tx, &old, None, error, guard).await,
        };
        if let Some(existing) = ops.iter().find(|op| op.node_id == node_id) {
            if existing.operation_id != operation_id
                || existing.arguments_hash != operation.arguments_hash
            {
                return reject(tx, &old, Some(existing), StorageError::IdConflict, guard).await;
            }
            guarded(guard, tx.commit()).await?;
            return Ok(existing.clone());
        }
        let mut next = old.clone();
        next.revision = next_revision(old.revision)?;
        ops.push(operation.clone());
        if let Err(error) = validate_checkpoint(&next, &ops) {
            return reject(tx, &old, None, error, guard).await;
        }
        let result = guarded(guard, sqlx::query("INSERT INTO operations (operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES ($1,$2,$3,'DRAFT',1,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(operation_id.as_str()).bind(task_id.as_str()).bind(node_id.as_str()).bind(plan_version as i64)
            .bind(db_version(operation.facts_version)?).bind(operation.arguments_hash.as_str()).execute(&mut *tx)).await?;
        if result.rows_affected() != 1 {
            return reject(tx, &old, None, StorageError::IdConflict, guard).await;
        }
        guarded(guard, update_task(&mut tx, &next, old.revision)).await?;
        guarded(guard, audit(&mut tx, &next, Some(&operation), PersistentAuditKind::OperationCreated,
            json!({"state": operation.state, "plan_version": plan_version, "arguments_hash": operation.arguments_hash}))).await?;
        guarded(guard, tx.commit()).await?;
        Ok(operation)
    }

    pub(crate) async fn operation_event(
        &self,
        owner: &UserId,
        task_id: &TaskId,
        operation_id: &OperationId,
        change: OperationChange,
        guard: Guard<'_>,
    ) -> Result<OperationRecord, StorageError> {
        let mut tx = guarded(guard, self.pool.begin()).await?;
        let old_task = guarded(guard, lock_task(&mut tx, owner, task_id)).await?;
        reject_mock_execution_claim(&mut tx, task_id).await?;
        let mut ops = guarded(guard, operations(&mut tx, task_id)).await?;
        let index = ops
            .iter()
            .position(|op| &op.operation_id == operation_id)
            .ok_or(StorageError::NotFound)?;
        let old = ops[index].clone();
        let prepare = || {
            if change.expected_revision != old.revision {
                return Err(StorageError::RevisionConflict);
            }
            check_facts(
                old_task.facts_version.max(old.facts_version),
                change.facts_version,
            )?;
            if old_task.state.is_terminal() || old.state.is_terminal() {
                return Err(StorageError::Terminal);
            }
            let mut next = old.clone();
            next.state = old
                .state
                .transition(change.event)
                .map_err(|_| StorageError::IllegalTransition)?;
            next.revision = next_revision(old.revision)?;
            next.facts_version = change.facts_version;
            if change.event == OperationEvent::SubmissionRecorded {
                let reference = change
                    .business_operation_ref
                    .clone()
                    .ok_or(StorageError::Invalid("submission needs original reference"))?;
                valid_id(reference.as_str())?;
                if old.business_operation_ref.is_some() {
                    return Err(StorageError::IdConflict);
                }
                next.business_operation_ref = Some(reference);
            } else if change.business_operation_ref != old.business_operation_ref {
                return Err(StorageError::IdConflict);
            }
            let mut task = old_task.clone();
            task.revision = next_revision(task.revision)?;
            task.facts_version = change.facts_version;
            if matches!(
                change.event,
                OperationEvent::GatesSatisfied | OperationEvent::SubmissionRecorded
            ) {
                if task.state != TaskState::Running
                    || task.checkpoint
                        != (RecoveryPoint::AtNode {
                            node_id: old.node_id.clone(),
                        })
                {
                    return Err(StorageError::IllegalTransition);
                }
            }
            if next.state == OperationState::Submitted {
                task.state = task
                    .state
                    .transition(safeflow_contracts::TaskEvent::AwaitExternal)
                    .map_err(|_| StorageError::IllegalTransition)?;
                task.checkpoint = RecoveryPoint::ReconcileOperation {
                    operation_id: operation_id.clone(),
                };
            }
            Ok((task, next))
        };
        let (task, next) = match prepare() {
            Ok(next) => next,
            Err(error) => return reject(tx, &old_task, Some(&old), error, guard).await,
        };
        ops[index] = next.clone();
        if let Err(error) = validate_checkpoint(&task, &ops) {
            return reject(tx, &old_task, Some(&old), error, guard).await;
        }
        let result = guarded(guard, sqlx::query("UPDATE operations SET state=$1, revision=$2, facts_version=$3, business_operation_ref=$4, updated_at=clock_timestamp() WHERE operation_id=$5 AND task_id=$6 AND revision=$7")
            .bind(encode(next.state)?).bind(db_version(next.revision)?).bind(db_version(next.facts_version)?)
            .bind(next.business_operation_ref.as_ref().map(|r| r.as_str())).bind(operation_id.as_str()).bind(task_id.as_str()).bind(db_version(old.revision)?)
            .execute(&mut *tx)).await?;
        if result.rows_affected() != 1 {
            return Err(StorageError::RevisionConflict);
        }
        guarded(guard, update_task(&mut tx, &task, old_task.revision)).await?;
        guarded(guard, audit(&mut tx, &task, Some(&next), PersistentAuditKind::OperationTransitioned,
            json!({"event": change.event, "from": old.state, "to": next.state, "facts_version": next.facts_version, "original_reference": next.business_operation_ref}))).await?;
        guarded(guard, tx.commit()).await?;
        Ok(next)
    }
}

pub(crate) fn encode(value: impl Serialize) -> Result<String, StorageError> {
    serde_json::to_value(value)
        .map_err(|_| StorageError::CorruptRecord)?
        .as_str()
        .map(str::to_owned)
        .ok_or(StorageError::CorruptRecord)
}

async fn reject_mock_execution_claim(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskId,
) -> Result<(), StorageError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mock_draft_execution_claims WHERE task_id=$1)",
    )
    .bind(task.as_str())
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Err(StorageError::ReconcileRequired);
    }
    Ok(())
}

pub(crate) fn decode<T: DeserializeOwned>(row: &PgRow, column: &str) -> Result<T, StorageError> {
    serde_json::from_value(json!(row.try_get::<String, _>(column)?))
        .map_err(|_| StorageError::CorruptRecord)
}

pub(crate) fn version(row: &PgRow, column: &str) -> Result<u64, StorageError> {
    let value: i64 = row.try_get(column)?;
    if value <= 0 {
        return Err(StorageError::CorruptRecord);
    }
    Ok(value as u64)
}

pub(crate) fn task_row(row: &PgRow) -> Result<TaskRecord, StorageError> {
    if row.try_get::<i16, _>("contract_version")? != PERSISTENCE_CONTRACT_VERSION as i16 {
        return Err(StorageError::CorruptRecord);
    }
    Ok(TaskRecord {
        task_id: decode(row, "task_id")?,
        owner_id: decode(row, "owner_id")?,
        state: decode(row, "state")?,
        revision: version(row, "revision")?,
        facts_version: version(row, "facts_version")?,
        plan: row.try_get::<Json<AgentPlan>, _>("plan")?.0,
        checkpoint: row.try_get::<Json<RecoveryPoint>, _>("checkpoint")?.0,
        completed_nodes: row.try_get::<Json<Vec<NodeId>>, _>("completed_nodes")?.0,
    })
}

pub(crate) fn operation_row(row: &PgRow) -> Result<OperationRecord, StorageError> {
    Ok(OperationRecord {
        operation_id: decode(row, "operation_id")?,
        task_id: decode(row, "task_id")?,
        node_id: decode(row, "node_id")?,
        state: decode(row, "state")?,
        revision: version(row, "revision")?,
        facts_version: version(row, "facts_version")?,
        plan_version: u32::try_from(version(row, "plan_version")?)
            .map_err(|_| StorageError::CorruptRecord)?,
        arguments_hash: decode(row, "arguments_hash")?,
        business_operation_ref: row
            .try_get::<Option<String>, _>("business_operation_ref")?
            .map(safeflow_contracts::BusinessOperationRef::parse)
            .transpose()
            .map_err(|_| StorageError::CorruptRecord)?,
    })
}

fn audit_row(row: &PgRow) -> Result<PersistentAuditEvent, StorageError> {
    let kind: PersistentAuditKind = decode(row, "kind")?;
    if kind == PersistentAuditKind::SubscriptionLocalChangeRecorded {
        crate::subscription_local_audit::checked_details(&row.try_get::<Json<serde_json::Value>, _>("details")?.0)?;
    }
    if kind == PersistentAuditKind::CardFreezeOperationRecorded {
        crate::permit::card_safety::final_transaction::checked_details(&row.try_get::<Json<serde_json::Value>, _>("details")?.0)?;
    }
    Ok(PersistentAuditEvent {
        sequence: row.try_get("sequence")?,
        event_id: decode(row, "event_id")?,
        task_id: decode(row, "task_id")?,
        operation_id: row
            .try_get::<Option<String>, _>("operation_id")?
            .map(OperationId::parse)
            .transpose()
            .map_err(|_| StorageError::CorruptRecord)?,
        kind,
        task_revision: version(row, "task_revision")?,
        operation_revision: row
            .try_get::<Option<i64>, _>("operation_revision")?
            .map(|v| u64::try_from(v).map_err(|_| StorageError::CorruptRecord))
            .transpose()?,
        details: row.try_get::<Json<serde_json::Value>, _>("details")?.0,
        occurred_at: row.try_get("occurred_at")?,
    })
}

pub(crate) async fn lock_task(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    task_id: &TaskId,
) -> Result<TaskRecord, StorageError> {
    lock_task_for_mode(tx, owner, task_id, false).await
}

pub(crate) async fn read_task(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    task_id: &TaskId,
) -> Result<TaskRecord, StorageError> {
    let row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2")
        .bind(task_id.as_str())
        .bind(owner.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::NotFound)?;
    let task = task_row(&row)?;
    crate::lifecycle::check_mode(tx, task_id, false).await?;
    Ok(task)
}

pub(crate) async fn lock_task_for_mode(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    task_id: &TaskId,
    lifecycle: bool,
) -> Result<TaskRecord, StorageError> {
    crate::daily_usage::protect_owner(tx, owner).await?;
    let row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE")
        .bind(task_id.as_str())
        .bind(owner.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::NotFound)?;
    let task = task_row(&row)?;
    crate::lifecycle::check_mode(tx, task_id, lifecycle).await?;
    crate::permit::reject_billing_recovery_generic(tx, &task).await?;
    Ok(task)
}

pub(crate) async fn operations(
    tx: &mut Transaction<'_, Postgres>,
    task_id: &TaskId,
) -> Result<Vec<OperationRecord>, StorageError> {
    // Preparation plans belong to the task but are never Runtime execution nodes.
    // Their immutable operations are queried through PaymentPreparationService.
    // Historical recovery can read a schema before its next migration is applied.
    let installed: bool =
        sqlx::query_scalar("SELECT to_regclass('payment_preparations') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    let links_installed: bool =
        sqlx::query_scalar("SELECT to_regclass('payment_execution_links') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    let payment_installed: bool =
        sqlx::query_scalar("SELECT to_regclass('payment_funds_execution_v1') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    let query = if payment_installed {
        "SELECT o.* FROM operations o WHERE task_id=$1 AND NOT EXISTS (SELECT 1 FROM payment_preparations p WHERE p.operation_id=o.operation_id) AND NOT EXISTS (SELECT 1 FROM payment_execution_links l WHERE l.registration_operation_id=o.operation_id) AND NOT EXISTS (SELECT 1 FROM payment_funds_execution_v1 e WHERE e.execution_operation_id=o.operation_id) ORDER BY operation_id"
    } else if links_installed {
        "SELECT o.* FROM operations o WHERE task_id=$1 AND NOT EXISTS (SELECT 1 FROM payment_preparations p WHERE p.operation_id=o.operation_id) AND NOT EXISTS (SELECT 1 FROM payment_execution_links l WHERE l.registration_operation_id=o.operation_id) ORDER BY operation_id"
    } else if installed {
        "SELECT o.* FROM operations o WHERE task_id=$1 AND NOT EXISTS (SELECT 1 FROM payment_preparations p WHERE p.operation_id=o.operation_id) ORDER BY operation_id"
    } else {
        "SELECT * FROM operations WHERE task_id=$1 ORDER BY operation_id"
    };
    let rows = sqlx::query(query)
        .bind(task_id.as_str())
        .fetch_all(&mut **tx)
        .await?;
    rows.iter().map(operation_row).collect()
}

fn check_revision(task: &TaskRecord, expected: u64) -> Result<(), StorageError> {
    if task.revision != expected {
        return Err(StorageError::RevisionConflict);
    }
    Ok(())
}

fn check_facts(old: u64, next: u64) -> Result<(), StorageError> {
    db_version(next)?;
    if next < old {
        return Err(StorageError::FactsConflict);
    }
    Ok(())
}

fn next_revision(old: u64) -> Result<u64, StorageError> {
    let next = old
        .checked_add(1)
        .ok_or(StorageError::Invalid("revision exhausted"))?;
    db_version(next)?;
    Ok(next)
}

pub(crate) async fn update_task(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    expected: u64,
) -> Result<(), StorageError> {
    let result = sqlx::query("UPDATE tasks SET state=$1,revision=$2,plan_version=$3,facts_version=$4,plan=$5,checkpoint=$6,completed_nodes=$7,updated_at=clock_timestamp() WHERE task_id=$8 AND owner_id=$9 AND revision=$10")
        .bind(encode(task.state)?).bind(db_version(task.revision)?).bind(task.plan.plan_version as i64).bind(db_version(task.facts_version)?)
        .bind(Json(&task.plan)).bind(Json(&task.checkpoint)).bind(Json(&task.completed_nodes))
        .bind(task.task_id.as_str()).bind(task.owner_id.as_str()).bind(db_version(expected)?)
        .execute(&mut **tx).await?;
    if result.rows_affected() != 1 {
        return Err(StorageError::RevisionConflict);
    }
    Ok(())
}

pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    operation: Option<&OperationRecord>,
    kind: PersistentAuditKind,
    details: serde_json::Value,
) -> Result<(), StorageError> {
    let event_id =
        EventId::parse(format!("event_{}", uuid::Uuid::new_v4())).expect("UUID has content");
    sqlx::query("INSERT INTO audit_events (event_id,task_id,operation_id,kind,task_revision,operation_revision,details) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(event_id.as_str()).bind(task.task_id.as_str()).bind(operation.map(|op| op.operation_id.as_str()))
        .bind(encode(kind)?).bind(db_version(task.revision)?).bind(operation.map(|op| db_version(op.revision)).transpose()?)
        .bind(Json(details)).execute(&mut **tx).await?;
    Ok(())
}

async fn reject<T>(
    mut tx: Transaction<'_, Postgres>,
    task: &TaskRecord,
    operation: Option<&OperationRecord>,
    error: StorageError,
    guard: Guard<'_>,
) -> Result<T, StorageError> {
    guarded(
        guard,
        audit(
            &mut tx,
            task,
            operation,
            PersistentAuditKind::ChangeRejected,
            json!({"reason": error.to_string()}),
        ),
    )
    .await?;
    guarded(guard, tx.commit()).await?;
    Err(error)
}
