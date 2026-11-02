//! Closed v2 recovery. Nonces and response projections grant no execution authority.
use super::*;
use crate::confirmation::timestamp;
use crate::confirmation_repository::{
    check_active, confirmation_details, insert_prepared_confirmation,
};
use crate::repository::{operations, task_row};
use sqlx::postgres::PgRow;
use uuid::Uuid;

const DOMAIN: &str = "billing-feedback-preparation-v2";
const SCOPE: &str = "BILLING_FEEDBACK_LOCAL_ONLY";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BillingRecoveryNonce(Uuid);
impl BillingRecoveryNonce {
    pub fn parse(value: &str) -> Result<Self, StorageError> {
        let id = Uuid::parse_str(value).map_err(|_| StorageError::Invalid("nonce"))?;
        if id.get_version_num() != 4
            || id.get_variant() != uuid::Variant::RFC4122
            || id.to_string() != value
        {
            return Err(StorageError::Invalid("canonical UUIDv4 required"));
        }
        Ok(Self(id))
    }
    pub fn as_string(&self) -> String {
        self.0.to_string()
    }
}
#[derive(Clone)]
pub struct BillingRecoveryConfig {
    realm: BillingRecoveryNonce,
    namespace: String,
    source_id: String,
    source_revision: String,
    continuity: String,
    open_limit: i64,
    retained_limit: i64,
    write_owner: i64,
    write_namespace: i64,
    read_owner: i64,
    read_namespace: i64,
    cleanup: bool,
}
impl BillingRecoveryConfig {
    /// Deployment-only explicit values, no TTL/quota/source defaults.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        realm: &str,
        namespace: &str,
        source_id: &str,
        source_revision: &str,
        continuity: &str,
        open_limit: u32,
        retained_limit: u32,
        write_owner: u32,
        write_namespace: u32,
        read_owner: u32,
        read_namespace: u32,
        allow_owner_cleanup: bool,
    ) -> Result<Self, StorageError> {
        for value in [namespace, source_id, source_revision, continuity] {
            valid_id(value)?;
        }
        if [
            open_limit,
            retained_limit,
            write_owner,
            write_namespace,
            read_owner,
            read_namespace,
        ]
        .contains(&0)
            || retained_limit < open_limit
            || !allow_owner_cleanup
        {
            return Err(StorageError::Invalid(
                "explicit recovery limits and cleanup required",
            ));
        }
        Ok(Self {
            realm: BillingRecoveryNonce::parse(realm)?,
            namespace: namespace.into(),
            source_id: source_id.into(),
            source_revision: source_revision.into(),
            continuity: continuity.into(),
            open_limit: open_limit.into(),
            retained_limit: retained_limit.into(),
            write_owner: write_owner.into(),
            write_namespace: write_namespace.into(),
            read_owner: read_owner.into(),
            read_namespace: read_namespace.into(),
            cleanup: allow_owner_cleanup,
        })
    }
    pub fn realm(&self) -> String {
        self.realm.as_string()
    }
    fn policy_digest(&self) -> Result<String, StorageError> {
        Ok(hash(&(
            DOMAIN,
            self.open_limit,
            self.retained_limit,
            self.write_owner,
            self.write_namespace,
            self.read_owner,
            self.read_namespace,
            self.cleanup,
        ))?
        .as_str()
        .into())
    }
}

fn problem(code: &'static str) -> StorageError {
    StorageError::BillingRecovery(code)
}
pub(crate) async fn installation_lock(
    tx: &mut Transaction<'_, Postgres>,
    exclusive: bool,
) -> Result<(), StorageError> {
    let sql = if exclusive {
        "SELECT pg_advisory_xact_lock(hashtextextended(current_schema()||'billing-recovery-install-v2',0))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended(current_schema()||'billing-recovery-install-v2',0))"
    };
    sqlx::query(sql).execute(&mut **tx).await?;
    Ok(())
}
async fn installed(
    tx: &mut Transaction<'_, Postgres>,
    config: &BillingRecoveryConfig,
    expected: &str,
) -> Result<(), StorageError> {
    if expected != config.realm() {
        return Err(problem("RECOVERY_REALM_MISMATCH"));
    }
    let row=sqlx::query("SELECT realm::text,namespace,source_id,source_revision,continuity,policy_digest,enabled FROM billing_prepare_realms_v2 WHERE singleton FOR SHARE")
        .fetch_optional(&mut **tx).await?.ok_or(problem("DISABLED"))?;
    if row.try_get::<String, _>("realm")? != expected
        || row.try_get::<String, _>("namespace")? != config.namespace
        || row.try_get::<String, _>("source_id")? != config.source_id
        || row.try_get::<String, _>("source_revision")? != config.source_revision
        || row.try_get::<String, _>("continuity")? != config.continuity
        || row.try_get::<String, _>("policy_digest")? != config.policy_digest()?
        || !row.try_get::<bool, _>("enabled")?
    {
        return Err(problem("RECOVERY_REALM_MISMATCH"));
    }
    Ok(())
}
impl PgRepository {
    /// Trusted installation/cutover action. Caller must establish writer drain and
    /// backup continuity externally; the method never infers either from absence.
    /// Candidate tests attest only their own isolated namespace and writers.
    pub async fn install_billing_recovery_for_reviewed_backend(
        &self,
        config: &BillingRecoveryConfig,
    ) -> Result<(), StorageError> {
        if self.namespace != config.namespace {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.pool.begin().await?;
        installation_lock(&mut tx, true).await?;
        sqlx::query("INSERT INTO billing_prepare_realms_v2(singleton,realm,namespace,source_id,source_revision,continuity,policy_digest,enabled) VALUES(true,$1::uuid,$2,$3,$4,$5,$6,true) ON CONFLICT(singleton) DO NOTHING")
            .bind(config.realm()).bind(&config.namespace).bind(&config.source_id).bind(&config.source_revision).bind(&config.continuity).bind(config.policy_digest()?).execute(&mut *tx).await?;
        installed(&mut tx, config, &config.realm()).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Operator-only pause/resume after independently establishing this SAME
    /// installation's continuity. No HTTP exposure, realm rotation or history reset.
    pub async fn set_billing_recovery_enabled_for_reviewed_backend(
        &self,
        config: &BillingRecoveryConfig,
        enabled: bool,
    ) -> Result<(), StorageError> {
        if self.namespace != config.namespace {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.pool.begin().await?;
        installation_lock(&mut tx, true).await?;
        let row=sqlx::query("SELECT realm::text,namespace,source_id,source_revision,continuity,policy_digest FROM billing_prepare_realms_v2 WHERE singleton FOR UPDATE").fetch_optional(&mut *tx).await?.ok_or(problem("DISABLED"))?;
        if row.try_get::<String, _>("realm")? != config.realm()
            || row.try_get::<String, _>("namespace")? != config.namespace
            || row.try_get::<String, _>("source_id")? != config.source_id
            || row.try_get::<String, _>("source_revision")? != config.source_revision
            || row.try_get::<String, _>("continuity")? != config.continuity
            || row.try_get::<String, _>("policy_digest")? != config.policy_digest()?
        {
            return Err(problem("RECOVERY_REALM_MISMATCH"));
        }
        sqlx::query("UPDATE billing_prepare_realms_v2 SET enabled=$1 WHERE singleton")
            .bind(enabled)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

// Called BEFORE every generic task lock/write, including old confirmations and permits.
// The transaction-local marker is set only inside the closed v2 service after
// installation/source/owner checks. It is not a request value or a permit.
pub(crate) async fn reject_generic(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
) -> Result<(), StorageError> {
    let has_table: bool =
        sqlx::query_scalar("SELECT to_regclass('billing_prepare_realms_v2') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    if !has_table {
        return Ok(());
    }
    if !task
        .plan
        .nodes
        .iter()
        .any(|n| matches!(n,PlanNode::Tool{tool,..} if tool.as_str()==TOOL))
    {
        return Ok(());
    }
    installation_lock(tx, false).await?;
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_prepare_realms_v2)")
        .fetch_one(&mut **tx)
        .await?;
    let marker: Option<String> =
        sqlx::query_scalar("SELECT current_setting('safeflow.billing_recovery_v2',true)")
            .fetch_one(&mut **tx)
            .await?;
    if active && marker.as_deref() != Some("closed-service") {
        return Err(problem("LEGACY_UNFENCED"));
    }
    Ok(())
}
async fn mark(tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
    sqlx::query("SELECT set_config('safeflow.billing_recovery_v2','closed-service',true)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Clone)]
struct Root {
    realm: String,
    nonce: String,
    owner: UserId,
    state: String,
    revision: i64,
    attempts: i16,
    input_identity: Option<Value>,
    observation: Option<(String, chrono::DateTime<chrono::Utc>)>,
    task: Option<TaskId>,
    operation: Option<OperationId>,
    confirmation: Option<ConfirmationRef>,
    parameters: Option<Value>,
    accounts: Option<Vec<String>>,
    digest: Option<String>,
    session_digest: Option<String>,
    binding_version: Option<i64>,
    fence: Option<i64>,
}
fn root(row: &PgRow) -> Result<Root, StorageError> {
    Ok(Root {
        realm: row.try_get("realm_text")?,
        input_identity: row.try_get("input_identity")?,
        observation: row
            .try_get::<Option<String>, _>("confirmation_status")?
            .zip(row.try_get("confirmation_expiry")?),
        nonce: row.try_get("nonce_text")?,
        owner: UserId::parse(row.try_get::<String, _>("owner_id")?)
            .map_err(|_| StorageError::CorruptRecord)?,
        state: row.try_get("state")?,
        revision: row.try_get("revision")?,
        attempts: row.try_get("attempt_count")?,
        task: row
            .try_get::<Option<String>, _>("task_id")?
            .map(TaskId::parse)
            .transpose()
            .map_err(|_| StorageError::CorruptRecord)?,
        operation: row
            .try_get::<Option<String>, _>("operation_id")?
            .map(OperationId::parse)
            .transpose()
            .map_err(|_| StorageError::CorruptRecord)?,
        confirmation: row
            .try_get::<Option<String>, _>("confirmation_ref")?
            .map(ConfirmationRef::parse)
            .transpose()
            .map_err(|_| StorageError::CorruptRecord)?,
        parameters: row.try_get("parameters")?,
        accounts: row
            .try_get::<Option<Json<Vec<String>>>, _>("accounts")?
            .map(|v| v.0),
        digest: row.try_get("identity_digest")?,
        session_digest: row.try_get("session_digest")?,
        binding_version: row.try_get("binding_version")?,
        fence: row.try_get("fence_revision")?,
    })
}
fn envelope(config: &BillingRecoveryConfig, nonce: &BillingRecoveryNonce) -> Value {
    json!({"contract_version":2,"scope":SCOPE,"recovery_realm":config.realm(),"nonce":nonce.as_string(),"lock_authorized":false,"automatic_retry":false})
}
fn fields(mut envelope: Value, more: Value) -> Value {
    envelope
        .as_object_mut()
        .unwrap()
        .extend(more.as_object().unwrap().clone());
    envelope
}

// Admission is one-use, private, non-Clone and has no wire serialization.
pub(crate) struct Admission {
    config: BillingRecoveryConfig,
    nonce: BillingRecoveryNonce,
    claim: String,
    request_digest: String,
    accounts: Vec<String>,
    bank: Arc<dyn BankGateway>,
}
pub(crate) struct VerifiedRollback {
    admission: Admission,
    txid: i64,
}
impl VerifiedRollback {
    pub(in crate::permit) fn new(admission: Admission, txid: i64) -> Self {
        Self { admission, txid }
    }
}

impl BillingFeedbackService<'_> {
    pub async fn prepare_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        nonce: &BillingRecoveryNonce,
        intent: FeedbackIntent,
    ) -> Result<Value, StorageError> {
        self.recovery_rate(session, config, expected, true).await?;
        let (mut tx, identity) = self.recovery_begin(session, config, expected).await?;
        let previous = self
            .recovery_root(&mut tx, &identity, config, nonce, true)
            .await?;
        if previous.as_ref().is_some_and(|r| r.state == "ABANDONED") {
            return Err(problem("PREPARATION_CLOSED"));
        }
        let (archive, accounts) =
            load_original(&mut tx, &identity.user_id, &intent.report_id).await?;
        let source = crate::billing_report::original_source_identity(&archive)?;
        if source.0 != config.source_id
            || source.1 != config.source_revision
            || source.2 != crate::BILLING_MEMBER_REVISION
        {
            return Err(StorageError::DependencyUnavailable);
        }
        let scope = self.scope(session, &identity, &accounts).await?;
        let restored = restore_original(archive, &scope)?;
        let input = Input {
            input_version: 1,
            report_id: intent.report_id.clone(),
            original_digest: restored.archive_digest,
            transaction: intent.transaction.clone(),
            choice: intent.choice.clone(),
            revision: None,
        };
        let parameters = json!(input);
        let identity_value = json!({"domain":DOMAIN,"protocol_version":2,"realm":config.realm(),"namespace":config.namespace,
            "source":source,"owner_id":identity.user_id,"parameters":parameters});
        let identity_digest = hash(&identity_value)?;
        if let Some(r) = previous {
            if r.digest.as_deref() != Some(identity_digest.as_str()) {
                return Err(problem("PREPARATION_KEY_CONFLICT"));
            }
            self.check_root(&mut tx, &r).await?;
            self.scope(session, &identity, &accounts).await?;
            self.guard(session, &identity)?;
            tx.commit().await?;
            return self
                .recovery_projection(session, &identity, config, nonce, &r, "RECOVERY_ONLY")
                .await;
        }
        let operation_id = new_operation_id();
        prepare_feedback(&scope, &restored.report, operation_id.as_str(), &intent)
            .map_err(|_| StorageError::Invalid("feedback intent rejected"))?;
        let saved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_feedback_v1 WHERE owner_user_id=$1 AND source_id=$2 AND account_id=$3 AND transaction_id=$4)")
            .bind(identity.user_id.as_str()).bind(&intent.transaction.source_id).bind(&intent.transaction.account_id).bind(&intent.transaction.transaction_id).fetch_one(&mut *tx).await?;
        if saved {
            return Err(problem("ALREADY_SAVED"));
        }
        let claimed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_prepare_requests_v2 WHERE realm=$1::uuid AND owner_id=$2 AND logical_source=$3 AND source_id=$4 AND account_id=$5 AND transaction_id=$6 AND state<>'ABANDONED')")
            .bind(config.realm()).bind(identity.user_id.as_str()).bind(&config.source_id).bind(&intent.transaction.source_id).bind(&intent.transaction.account_id).bind(&intent.transaction.transaction_id).fetch_one(&mut *tx).await?;
        if claimed {
            return Err(problem("PREPARATION_ALREADY_OPEN"));
        }
        self.recovery_capacity(&mut tx, config, &identity.user_id, true)
            .await?;
        let (task, operation, confirmation) = self
            .create_recovery_locked(
                &mut tx,
                session,
                &identity,
                operation_id,
                parameters.clone(),
            )
            .await?;
        let session_identity = self.confirmations.session(session)?;
        sqlx::query("INSERT INTO billing_prepare_requests_v2(realm,nonce,owner_id,input_identity,identity_digest,parameters,accounts,task_id,operation_id,confirmation_ref,session_digest,binding_version,logical_source,source_id,account_id,transaction_id,state,revision,attempt_count) VALUES($1::uuid,$2::uuid,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,'PREPARED_OPEN',1,0)")
            .bind(config.realm()).bind(nonce.as_string()).bind(identity.user_id.as_str()).bind(Json(identity_value)).bind(identity_digest.as_str()).bind(Json(parameters.clone())).bind(Json(&accounts))
            .bind(task.task_id.as_str()).bind(operation.operation_id.as_str()).bind(confirmation.confirmation_ref.as_str()).bind(session_identity.digest.as_str()).bind(db_version(identity.binding_version)?)
            .bind(&config.source_id).bind(&intent.transaction.source_id).bind(&intent.transaction.account_id).bind(&intent.transaction.transaction_id).execute(&mut *tx).await?;
        sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
            .execute(&mut *tx)
            .await?;
        self.scope(session, &identity, &accounts).await?;
        check_active(&confirmation, self.store.clock.as_ref())?;
        self.guard(session, &identity)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        // Independent committed read; a competing close/admission suppresses the ref.
        let mut read = self.store.repository.pool.begin().await?;
        let r = self
            .recovery_root(&mut read, &identity, config, nonce, false)
            .await?
            .ok_or(StorageError::ReconcileRequired)?;
        self.check_root(&mut read, &r).await?;
        read.commit().await?;
        self.scope(session, &identity, &accounts).await?;
        self.guard(session, &identity)?;
        if r.state != "PREPARED_OPEN"
            || check_active(&confirmation, self.store.clock.as_ref()).is_err()
        {
            return self
                .recovery_projection(session, &identity, config, nonce, &r, "RECOVERY_ONLY")
                .await;
        }
        Ok(fields(
            envelope(config, nonce),
            json!({"kind":"CREATED","state":"PREPARED_OPEN","revision":1,"task_id":task.task_id,"operation_id":operation.operation_id,
            "parameters":parameters,"confirmation":{"confirmation_ref":confirmation.confirmation_ref,"status":"PREPARED","expires_at":confirmation.expires_at}}),
        ))
    }
    async fn recovery_capacity(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        config: &BillingRecoveryConfig,
        owner: &UserId,
        open: bool,
    ) -> Result<(), StorageError> {
        let(total,active):(i64,i64)=sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE state<>'ABANDONED') FROM billing_prepare_requests_v2 WHERE realm=$1::uuid AND owner_id=$2")
            .bind(config.realm()).bind(owner.as_str()).fetch_one(&mut **tx).await?;
        if total >= config.retained_limit || (open && active >= config.open_limit) {
            return Err(problem("PREPARATION_CAPACITY"));
        }
        Ok(())
    }
    async fn create_recovery_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        identity: &SessionBinding,
        operation_id: OperationId,
        parameters: Value,
    ) -> Result<(TaskRecord, OperationRecord, ConfirmationRecord), StorageError> {
        let review = NodeId::parse("review_feedback").unwrap();
        let save = NodeId::parse("save_feedback").unwrap();
        let plan = AgentPlan::new(
            new_task_id(),
            1,
            vec![
                PlanNode::UserDecision {
                    id: review.clone(),
                    depends_on: vec![],
                },
                PlanNode::Tool {
                    id: save.clone(),
                    tool: ToolName::parse(TOOL).unwrap(),
                    arguments: parameters.clone(),
                    depends_on: vec![review.clone()],
                },
            ],
        )
        .map_err(|_| StorageError::PlanConflict)?;
        let mut task = TaskRecord {
            task_id: plan.task_id.clone(),
            owner_id: identity.user_id.clone(),
            state: TaskState::Running,
            revision: 1,
            facts_version: 1,
            checkpoint: RecoveryPoint::AtNode {
                node_id: review.clone(),
            },
            plan,
            completed_nodes: vec![],
        };
        let creation_digest = hash(&(task.plan.clone(), 1u64))?;
        sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,$2,1,'RUNNING',1,1,1,$3,$4,$5,$6)")
            .bind(task.task_id.as_str()).bind(identity.user_id.as_str()).bind(Json(&task.plan)).bind(creation_digest.as_str()).bind(Json(&task.checkpoint)).bind(Json(&task.completed_nodes)).execute(&mut **tx).await?;
        audit(
            tx,
            &task,
            None,
            PersistentAuditKind::TaskCreated,
            json!({"plan_version":1,"creation_digest":creation_digest,"planning_origin":null}),
        )
        .await?;
        let mut op = OperationRecord {
            operation_id,
            task_id: task.task_id.clone(),
            node_id: save.clone(),
            state: OperationState::Draft,
            revision: 1,
            plan_version: 1,
            facts_version: 1,
            arguments_hash: hash(&parameters)?,
            business_operation_ref: None,
        };
        sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'DRAFT',1,1,1,$4)")
            .bind(op.operation_id.as_str()).bind(task.task_id.as_str()).bind(save.as_str()).bind(op.arguments_hash.as_str()).execute(&mut **tx).await?;
        task.revision = 2;
        update_task(tx, &task, 1).await?;
        audit(
            tx,
            &task,
            Some(&op),
            PersistentAuditKind::OperationCreated,
            json!({"state":"DRAFT","plan_version":1,"arguments_hash":op.arguments_hash}),
        )
        .await?;
        task.revision = 3;
        task.checkpoint = RecoveryPoint::AtNode { node_id: save };
        task.completed_nodes = vec![review];
        validate_checkpoint(&task, std::slice::from_ref(&op))?;
        update_task(tx, &task, 2).await?;
        audit(tx,&task,None,PersistentAuditKind::CheckpointSaved,json!({"event":null,"checkpoint":task.checkpoint,"completed_nodes":task.completed_nodes,"facts_version":1})).await?;
        op.state = op
            .state
            .transition(OperationEvent::GatesSatisfied)
            .map_err(|_| StorageError::IllegalTransition)?;
        op.revision = 2;
        task.revision = 4;
        sqlx::query("UPDATE operations SET state='READY',revision=2 WHERE operation_id=$1")
            .bind(op.operation_id.as_str())
            .execute(&mut **tx)
            .await?;
        update_task(tx, &task, 3).await?;
        validate_checkpoint(&task, std::slice::from_ref(&op))?;
        audit(tx,&task,Some(&op),PersistentAuditKind::OperationTransitioned,json!({"event":"GATES_SATISFIED","from":"DRAFT","to":"READY","facts_version":1,"original_reference":null})).await?;
        let binding = current_binding(&task, std::slice::from_ref(&op), &op.operation_id)?;
        let confirmation = self.confirmations.new_locked_record(binding)?;
        let identity = self.confirmations.session(session)?;
        insert_prepared_confirmation(tx, &identity, &confirmation).await?;
        audit(
            tx,
            &task,
            Some(&op),
            PersistentAuditKind::ConfirmationPrepared,
            confirmation_details(&confirmation)?,
        )
        .await?;
        Ok((task, op, confirmation))
    }
    async fn recovery_begin(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
    ) -> Result<(Transaction<'static, Postgres>, SessionBinding), StorageError> {
        let identity = self.store.identity(session)?;
        if self.store.repository.namespace != config.namespace {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.store.repository.pool.begin().await?;
        installation_lock(&mut tx, false).await?;
        installed(&mut tx, config, expected).await?;
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        self.guard(session, &identity)?;
        mark(&mut tx).await?;
        Ok((tx, identity))
    }
    async fn recovery_root(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &SessionBinding,
        config: &BillingRecoveryConfig,
        nonce: &BillingRecoveryNonce,
        lock: bool,
    ) -> Result<Option<Root>, StorageError> {
        if lock {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                .bind(format!(
                    "billing-nonce-v2:{}:{}",
                    config.realm(),
                    nonce.as_string()
                ))
                .execute(&mut **tx)
                .await?;
        }
        let sql="SELECT *,realm::text AS realm_text,nonce::text AS nonce_text,(SELECT status FROM server_confirmations c WHERE c.confirmation_ref=billing_prepare_requests_v2.confirmation_ref) AS confirmation_status,(SELECT expires_at FROM server_confirmations c WHERE c.confirmation_ref=billing_prepare_requests_v2.confirmation_ref) AS confirmation_expiry FROM billing_prepare_requests_v2 WHERE realm=$1::uuid AND nonce=$2::uuid";
        let observed = sqlx::query(sql)
            .bind(config.realm())
            .bind(nonce.as_string())
            .fetch_optional(&mut **tx)
            .await?;
        let Some(observed) = observed else {
            return Ok(None);
        };
        let observed = root(&observed)?;
        if observed.owner != identity.user_id {
            return Err(StorageError::NotFound);
        }
        if lock {
            if let Some(task) = &observed.task {
                crate::repository::lock_task(tx, &identity.user_id, task).await?;
            }
            let row = sqlx::query(&(sql.to_owned() + " FOR UPDATE"))
                .bind(config.realm())
                .bind(nonce.as_string())
                .fetch_one(&mut **tx)
                .await?;
            return Ok(Some(root(&row)?));
        }
        Ok(Some(observed))
    }
    async fn recovery_rate(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        write: bool,
    ) -> Result<(), StorageError> {
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        installation_lock(&mut tx, false).await?;
        installed(&mut tx, config, expected).await?;
        // Dedicated operational ledger: business read transactions remain READ ONLY.
        // Serialize namespace counts, so multi-process/restart cannot reset limits.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema()||'billing-recovery-rate-v2',0))").execute(&mut *tx).await?;
        let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT high_water FROM billing_prepare_rate_clock_v2 WHERE realm=$1::uuid FOR UPDATE",
        )
        .bind(config.realm())
        .fetch_optional(&mut *tx)
        .await?;
        if last.is_some_and(|last| now < last) {
            return Err(problem("UNAVAILABLE_OR_INTEGRITY"));
        }
        sqlx::query("INSERT INTO billing_prepare_rate_clock_v2(realm,high_water) VALUES($1::uuid,$2) ON CONFLICT(realm) DO UPDATE SET high_water=EXCLUDED.high_water").bind(config.realm()).bind(now).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM billing_prepare_rate_v2 WHERE realm=$1::uuid AND at<=$2-interval '60 seconds'").bind(config.realm()).bind(now).execute(&mut *tx).await?;
        let mode = if write { "WRITE" } else { "READ" };
        let (owner_limit, namespace_limit) = if write {
            (config.write_owner, config.write_namespace)
        } else {
            (config.read_owner, config.read_namespace)
        };
        let (total,own):(i64,i64)=sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE owner_id=$3) FROM billing_prepare_rate_v2 WHERE realm=$1::uuid AND mode=$2 AND at>$4-interval '60 seconds'")
            .bind(config.realm()).bind(mode).bind(identity.user_id.as_str()).bind(now).fetch_one(&mut *tx).await?;
        if total >= namespace_limit || own >= owner_limit {
            return Err(problem("PREPARATION_CAPACITY"));
        }
        sqlx::query(
            "INSERT INTO billing_prepare_rate_v2(realm,owner_id,mode,at) VALUES($1::uuid,$2,$3,$4)",
        )
        .bind(config.realm())
        .bind(identity.user_id.as_str())
        .bind(mode)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn recovery_context_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
    ) -> Result<Value, StorageError> {
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let row:Option<(String,String,String,String,String,String,bool)>=sqlx::query_as("SELECT realm::text,namespace,source_id,source_revision,continuity,policy_digest,enabled FROM billing_prepare_realms_v2 WHERE singleton").fetch_optional(&mut *tx).await?;
        let available = self.store.repository.namespace == config.namespace
            && row.as_ref().is_some_and(|r| {
                r.0 == config.realm()
                    && r.1 == config.namespace
                    && r.2 == config.source_id
                    && r.3 == config.source_revision
                    && r.4 == config.continuity
                    && r.5 == config.policy_digest().unwrap_or_default()
                    && r.6
            });
        self.guard(session, &identity)?;
        tx.commit().await?;
        Ok(
            json!({"contract_version":2,"scope":SCOPE,"recovery_realm":row.map(|r|r.0),"available":available}),
        )
    }
    async fn recovery_projection(
        &self,
        session: &SessionRef,
        identity: &SessionBinding,
        config: &BillingRecoveryConfig,
        nonce: &BillingRecoveryNonce,
        r: &Root,
        kind: &str,
    ) -> Result<Value, StorageError> {
        let visible = if let Some(accounts) = &r.accounts {
            match self.scope(session, identity, accounts).await {
                Ok(_) => true,
                Err(StorageError::NotFound | StorageError::DependencyUnavailable) => false,
                Err(e) => return Err(e),
            }
        } else {
            false
        };
        let observation = if visible {
            let row = r.observation.as_ref().ok_or(StorageError::CorruptRecord)?;
            let status = if row.0 != "REVOKED" && timestamp(self.store.clock.as_ref())? >= row.1 {
                "EXPIRED"
            } else {
                &row.0
            };
            json!({"status":status,"original_expires_at":row.1})
        } else {
            Value::Null
        };
        self.guard(session, identity)?;
        Ok(fields(
            envelope(config, nonce),
            json!({"kind":kind,"state":r.state,"revision":r.revision,
            "task_id":if visible{json!(r.task)}else{Value::Null},"operation_id":if visible{json!(r.operation)}else{Value::Null},
            "can_continue":false,"parameters":if visible{json!(r.parameters)}else{Value::Null},
            "confirmation_observation":observation,"result_available":visible&&r.state=="SAVED","fence_revision":r.fence}),
        ))
    }
    pub async fn read_preparation_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        nonce: &BillingRecoveryNonce,
    ) -> Result<Value, StorageError> {
        self.recovery_rate(session, config, expected, false).await?;
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        // READ ONLY cannot take row locks; exact immutable installation is checked in this snapshot.
        let installation:Option<(String,String,String,String,String,String,bool)>=sqlx::query_as("SELECT realm::text,namespace,source_id,source_revision,continuity,policy_digest,enabled FROM billing_prepare_realms_v2 WHERE singleton").fetch_optional(&mut *tx).await?;
        if self.store.repository.namespace != config.namespace
            || expected != config.realm()
            || !installation.is_some_and(|v| {
                v.0 == expected
                    && v.1 == config.namespace
                    && v.2 == config.source_id
                    && v.3 == config.source_revision
                    && v.4 == config.continuity
                    && v.5 == config.policy_digest().unwrap_or_default()
                    && v.6
            })
        {
            return Err(problem("RECOVERY_REALM_MISMATCH"));
        }
        let r = self
            .recovery_root(&mut tx, &identity, config, nonce, false)
            .await?
            .ok_or(StorageError::NotFound)?;
        self.check_root(&mut tx, &r).await?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        self.recovery_projection(session, &identity, config, nonce, &r, "READ")
            .await
    }
    pub async fn read_operation_preparation_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        operation: &OperationId,
    ) -> Result<Value, StorageError> {
        self.recovery_rate(session, config, expected, false).await?;
        let identity = self.store.identity(session)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let installation:Option<(String,String,String,String,String,String,bool)>=sqlx::query_as("SELECT realm::text,namespace,source_id,source_revision,continuity,policy_digest,enabled FROM billing_prepare_realms_v2 WHERE singleton").fetch_optional(&mut *tx).await?;
        if self.store.repository.namespace != config.namespace
            || expected != config.realm()
            || !installation.is_some_and(|v| {
                v.0 == expected
                    && v.1 == config.namespace
                    && v.2 == config.source_id
                    && v.3 == config.source_revision
                    && v.4 == config.continuity
                    && v.5 == config.policy_digest().unwrap_or_default()
                    && v.6
            })
        {
            return Err(problem("RECOVERY_REALM_MISMATCH"));
        }
        let nonce:Option<String>=sqlx::query_scalar("SELECT nonce::text FROM billing_prepare_requests_v2 WHERE realm=$1::uuid AND owner_id=$2 AND operation_id=$3")
            .bind(config.realm()).bind(identity.user_id.as_str()).bind(operation.as_str()).fetch_optional(&mut *tx).await?;
        let Some(nonce) = nonce else {
            let legacy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operations o JOIN tasks t USING(task_id) WHERE operation_id=$1 AND owner_id=$2)")
                .bind(operation.as_str()).bind(identity.user_id.as_str()).fetch_one(&mut *tx).await?;
            self.guard(session, &identity)?;
            return Err(if legacy {
                problem("LEGACY_UNFENCED")
            } else {
                StorageError::NotFound
            });
        };
        let nonce = BillingRecoveryNonce::parse(&nonce)?;
        let r = self
            .recovery_root(&mut tx, &identity, config, &nonce, false)
            .await?
            .ok_or(StorageError::NotFound)?;
        self.check_root(&mut tx, &r).await?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        let read = self
            .recovery_projection(session, &identity, config, &nonce, &r, "READ")
            .await?;
        if read["operation_id"] != json!(operation) {
            return Err(StorageError::NotFound);
        }
        Ok(read)
    }
    async fn check_root(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        r: &Root,
    ) -> Result<(), StorageError> {
        if r.task.is_none() {
            if r.state != "ABANDONED" || r.operation.is_some() || r.parameters.is_some() {
                return Err(StorageError::CorruptRecord);
            }
            return Ok(());
        }
        let row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2")
            .bind(r.task.as_ref().unwrap().as_str())
            .bind(r.owner.as_str())
            .fetch_one(&mut **tx)
            .await?;
        let task = task_row(&row)?;
        let ops = operations(tx, &task.task_id).await?;
        validate_plan(&task.plan)?;
        validate_checkpoint(&task, &ops)?;
        let op = ops
            .iter()
            .find(|o| Some(&o.operation_id) == r.operation.as_ref())
            .ok_or(StorageError::CorruptRecord)?;
        let tool = task
            .plan
            .nodes
            .iter()
            .find_map(|n| match n {
                PlanNode::Tool {
                    tool, arguments, ..
                } if tool.as_str() == TOOL => Some(arguments),
                _ => None,
            })
            .ok_or(StorageError::CorruptRecord)?;
        if Some(tool) != r.parameters.as_ref()
            || op.arguments_hash != hash(tool)?
            || ops.len() != 1
            || op.node_id.as_str() != "save_feedback"
        {
            return Err(StorageError::CorruptRecord);
        }
        let content = r
            .input_identity
            .as_ref()
            .ok_or(StorageError::CorruptRecord)?;
        if Some(hash(content)?.as_str()) != r.digest.as_deref()
            || content["realm"] != r.realm
            || content["owner_id"] != json!(r.owner)
            || content["parameters"] != *tool
        {
            return Err(StorageError::CorruptRecord);
        }
        let input: Input =
            serde_json::from_value(tool.clone()).map_err(|_| StorageError::CorruptRecord)?;
        let (archive, accounts) = load_original(tx, &r.owner, &input.report_id).await?;
        if json!(input) != *tool
            || input.input_version != 1
            || hash(&archive)? != input.original_digest
            || Some(&accounts) != r.accounts.as_ref()
            || content["source"]
                != json!(crate::billing_report::original_source_identity(&archive)?)
            || content["domain"] != DOMAIN
            || content["protocol_version"] != 2
            || content["namespace"] != self.store.repository.namespace
        {
            return Err(StorageError::CorruptRecord);
        }
        let [PlanNode::UserDecision {
            id: review,
            depends_on,
        }, PlanNode::Tool {
            id: save,
            tool: tool_name,
            arguments: _,
            depends_on: save_depends,
        }] = task.plan.nodes.as_slice()
        else {
            return Err(StorageError::CorruptRecord);
        };
        if review.as_str() != "review_feedback"
            || !depends_on.is_empty()
            || save.as_str() != "save_feedback"
            || tool_name.as_str() != TOOL
            || save_depends != std::slice::from_ref(review)
            || task.plan.plan_version != 1
            || task.facts_version != 1
            || op.plan_version != 1
            || op.facts_version != 1
        {
            return Err(StorageError::CorruptRecord);
        }
        let confirmation = crate::confirmation_repository::read_historical_confirmation(
            tx,
            &task.task_id,
            r.confirmation.as_ref().ok_or(StorageError::CorruptRecord)?,
        )
        .await?;
        if confirmation.record.binding.operation_id != op.operation_id
            || confirmation.record.binding.user_id != r.owner
            || Some(&confirmation.session_digest) != r.session_digest.as_ref()
            || Some(db_version(confirmation.session_binding_version)?) != r.binding_version
        {
            return Err(StorageError::CorruptRecord);
        }
        let original_audits:i64=sqlx::query_scalar("SELECT count(DISTINCT kind) FROM audit_events WHERE task_id=$1 AND kind IN ('TASK_CREATED','OPERATION_CREATED','CHECKPOINT_SAVED','OPERATION_TRANSITIONED','CONFIRMATION_PREPARED')").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
        if original_audits != 5 {
            return Err(StorageError::CorruptRecord);
        }
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM billing_prepare_attempts_v2 WHERE realm=$1::uuid AND nonce=$2::uuid").bind(&r.realm).bind(&r.nonce).fetch_one(&mut **tx).await?;
        if count != i64::from(r.attempts) {
            return Err(StorageError::CorruptRecord);
        }
        let saved: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM public_billing_feedback_results_v1 WHERE operation_id=$1)",
        )
        .bind(op.operation_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        if saved != (r.state == "SAVED") {
            return Err(StorageError::CorruptRecord);
        }
        let member:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_feedback_v1 WHERE public_operation_id=$1 AND owner_user_id=$2)").bind(op.operation_id.as_str()).bind(r.owner.as_str()).fetch_one(&mut **tx).await?;
        let submitted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=$1 AND owner_id=$2 AND first_operation IS NOT NULL)").bind(op.operation_id.as_str()).bind(r.owner.as_str()).fetch_one(&mut **tx).await?;
        let consumed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM execution_permits WHERE operation_id=$1 AND status='CONSUMED'",
        )
        .bind(op.operation_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        let broken_audit:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events e LEFT JOIN public_audit_outbox b USING(event_id) WHERE e.task_id=$1 AND (b.event_id IS NULL OR b.envelope->'public_event' IS DISTINCT FROM to_jsonb(e)))").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
        if saved != member || saved != submitted || consumed != i64::from(saved) || broken_audit {
            return Err(StorageError::CorruptRecord);
        }
        if r.attempts == 1 {
            let attempt:String=sqlx::query_scalar("SELECT state FROM billing_prepare_attempts_v2 WHERE realm=$1::uuid AND nonce=$2::uuid").bind(&r.realm).bind(&r.nonce).fetch_one(&mut **tx).await?;
            let expected = match r.state.as_str() {
                "EXECUTION_ADMITTED" => "ADMITTED",
                "SAVED" => "BUSINESS_COMMITTED",
                "CLOSED_ROLLBACK" | "ABANDONED" => "ROLLBACK_VERIFIED",
                _ => return Err(StorageError::CorruptRecord),
            };
            if attempt != expected {
                return Err(StorageError::CorruptRecord);
            }
        }
        if r.state == "SAVED"
            && (task.state != TaskState::Completed
                || op.state != OperationState::Succeeded
                || task.revision != 7
                || op.revision != 4
                || task.completed_nodes != vec![review.clone(), save.clone()])
        {
            return Err(StorageError::CorruptRecord);
        }
        if r.state != "SAVED" && task.completed_nodes != vec![review.clone()] {
            return Err(StorageError::CorruptRecord);
        }

        if r.state == "ABANDONED" {
            if task.state != TaskState::Cancelled
                || op.state != OperationState::Cancelled
                || task.checkpoint != RecoveryPoint::Finished
                || task.revision != 6
                || op.revision != 3
                || confirmation.record.status != crate::ConfirmationStatus::Revoked
                || r.fence != Some(r.revision)
            {
                return Err(StorageError::CorruptRecord);
            }
        } else if r.state != "SAVED"
            && (task.state != TaskState::Running
                || op.state != OperationState::Ready
                || task.revision != 4
                || op.revision != 2)
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(())
    }

    pub(super) async fn check_cancelled_recovery(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &SessionBinding,
        nonce: &str,
        task: &TaskId,
    ) -> Result<(), StorageError> {
        let row=sqlx::query("SELECT *,realm::text AS realm_text,nonce::text AS nonce_text,(SELECT status FROM server_confirmations c WHERE c.confirmation_ref=billing_prepare_requests_v2.confirmation_ref) AS confirmation_status,(SELECT expires_at FROM server_confirmations c WHERE c.confirmation_ref=billing_prepare_requests_v2.confirmation_ref) AS confirmation_expiry FROM billing_prepare_requests_v2 WHERE task_id=$1 AND nonce=$2::uuid AND owner_id=$3").bind(task.as_str()).bind(nonce).bind(identity.user_id.as_str()).fetch_one(&mut **tx).await?;
        let r = root(&row)?;
        self.check_root(tx, &r).await
    }
    pub async fn confirm_preparation_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        nonce: &BillingRecoveryNonce,
        target_task: &TaskId,
        target_operation: &OperationId,
        reference: &ConfirmationRef,
    ) -> Result<Value, StorageError> {
        self.recovery_rate(session, config, expected, true).await?;
        let (mut tx, identity) = self.recovery_begin(session, config, expected).await?;
        let r = self
            .recovery_root(&mut tx, &identity, config, nonce, true)
            .await?
            .ok_or(StorageError::NotFound)?;
        self.check_root(&mut tx, &r).await?;
        if r.state != "PREPARED_OPEN" {
            return Err(problem(if r.state == "ABANDONED" {
                "PREPARATION_CLOSED"
            } else {
                "RECONCILE_REQUIRED"
            }));
        }
        if r.confirmation.as_ref() != Some(reference)
            || r.task.as_ref() != Some(target_task)
            || r.operation.as_ref() != Some(target_operation)
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let task = r.task.as_ref().ok_or(StorageError::CorruptRecord)?;
        let stored =
            crate::confirmation_repository::read_stored_confirmation(&mut tx, task, reference)
                .await?;
        let cs = self.confirmations.session(session)?;
        stored.check_session(&cs)?;
        let mut confirmation = stored.record;
        check_active(&confirmation, self.store.clock.as_ref())?;
        self.scope(
            session,
            &identity,
            r.accounts.as_ref().ok_or(StorageError::CorruptRecord)?,
        )
        .await?;
        let record = crate::repository::lock_task(&mut tx, &identity.user_id, task).await?;
        let ops = operations(&mut tx, task).await?;
        if current_binding(
            &record,
            &ops,
            r.operation.as_ref().ok_or(StorageError::CorruptRecord)?,
        )? != confirmation.binding
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        if confirmation.status == crate::ConfirmationStatus::Prepared {
            confirmation.status = crate::ConfirmationStatus::Confirmed;
            confirmation.confirmed_at = Some(timestamp(self.store.clock.as_ref())?);
            sqlx::query("UPDATE server_confirmations SET status='CONFIRMED',confirmed_at=$1 WHERE confirmation_ref=$2")
                .bind(confirmation.confirmed_at).bind(reference.as_str()).execute(&mut *tx).await?;
            audit(
                &mut tx,
                &record,
                ops.first(),
                PersistentAuditKind::ExplicitConfirmationRecorded,
                confirmation_details(&confirmation)?,
            )
            .await?;
        }
        self.scope(
            session,
            &identity,
            r.accounts.as_ref().ok_or(StorageError::CorruptRecord)?,
        )
        .await?;
        check_active(&confirmation, self.store.clock.as_ref())?;
        self.guard(session, &identity)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok(fields(
            envelope(config, nonce),
            json!({"kind":"CONFIRMED","state":"PREPARED_OPEN","revision":r.revision,"task_id":r.task,"operation_id":r.operation,
            "confirmation":{"confirmation_ref":reference,"status":"CONFIRMED","expires_at":confirmation.expires_at}}),
        ))
    }

    pub async fn abandon_preparation_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        nonce: &BillingRecoveryNonce,
    ) -> Result<Value, StorageError> {
        if !config.cleanup {
            return Err(problem("DISABLED"));
        }
        self.recovery_rate(session, config, expected, true).await?;
        let (mut tx, identity) = self.recovery_begin(session, config, expected).await?;
        let previous = self
            .recovery_root(&mut tx, &identity, config, nonce, true)
            .await?;
        if let Some(r) = &previous {
            self.check_root(&mut tx, r).await?;
            match r.state.as_str() {
                "ABANDONED" => {}
                "EXECUTION_ADMITTED" => return Err(problem("RECONCILE_REQUIRED")),
                "SAVED" => return Err(problem("ALREADY_SAVED")),
                "PREPARED_OPEN" | "CLOSED_ROLLBACK" => {
                    let task = r.task.as_ref().ok_or(StorageError::CorruptRecord)?;
                    let operation = r.operation.as_ref().ok_or(StorageError::CorruptRecord)?;
                    let mut record =
                        crate::repository::lock_task(&mut tx, &identity.user_id, task).await?;
                    let ops = operations(&mut tx, task).await?;
                    let mut op = ops
                        .into_iter()
                        .find(|o| o.operation_id == *operation)
                        .ok_or(StorageError::CorruptRecord)?;
                    let consumed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=$1 AND status='CONSUMED')")
                        .bind(operation.as_str()).fetch_one(&mut *tx).await?;
                    if consumed {
                        return Err(StorageError::ReconcileRequired);
                    }
                    let stored = crate::confirmation_repository::read_stored_confirmation(
                        &mut tx,
                        task,
                        r.confirmation.as_ref().ok_or(StorageError::CorruptRecord)?,
                    )
                    .await?;
                    if stored.record.binding.user_id != identity.user_id
                        || stored.session_digest
                            != r.session_digest
                                .clone()
                                .ok_or(StorageError::CorruptRecord)?
                        || Some(db_version(stored.session_binding_version)?) != r.binding_version
                    {
                        return Err(StorageError::CorruptRecord);
                    }
                    let now = timestamp(self.store.clock.as_ref())?;
                    if now < stored.record.prepared_at
                        || stored.record.confirmed_at.is_some_and(|t| now < t)
                    {
                        return Err(StorageError::Invalid("clock moved backwards"));
                    }
                    if stored.record.status != crate::ConfirmationStatus::Revoked {
                        let mut revoked = stored.record;
                        revoked.status = crate::ConfirmationStatus::Revoked;
                        revoked.revoked_at = Some(now);
                        sqlx::query("UPDATE server_confirmations SET status='REVOKED',revoked_at=$1 WHERE confirmation_ref=$2").bind(now).bind(revoked.confirmation_ref.as_str()).execute(&mut *tx).await?;
                        audit(
                            &mut tx,
                            &record,
                            Some(&op),
                            PersistentAuditKind::ConfirmationRevoked,
                            confirmation_details(&revoked)?,
                        )
                        .await?;
                    }
                    let permits:Vec<String>=sqlx::query_scalar("SELECT permit_id FROM execution_permits WHERE owner_id=$1 AND task_id=$2 AND operation_id=$3 AND status='ACTIVE' FOR UPDATE")
                        .bind(identity.user_id.as_str()).bind(task.as_str()).bind(operation.as_str()).fetch_all(&mut *tx).await?;
                    for permit in permits {
                        sqlx::query(
                            "UPDATE execution_permits SET status='REVOKED' WHERE permit_id=$1",
                        )
                        .bind(&permit)
                        .execute(&mut *tx)
                        .await?;
                        audit(
                            &mut tx,
                            &record,
                            Some(&op),
                            PersistentAuditKind::PermitRevoked,
                            json!({"permit_id":permit}),
                        )
                        .await?;
                    }
                    let old_op = op.clone();
                    let old_task = record.clone();
                    op.state = op
                        .state
                        .transition(OperationEvent::Cancel)
                        .map_err(|_| StorageError::IllegalTransition)?;
                    op.revision += 1;
                    record.revision += 1;
                    sqlx::query("UPDATE operations SET state='CANCELLED',revision=$1 WHERE operation_id=$2 AND revision=$3").bind(db_version(op.revision)?).bind(operation.as_str()).bind(db_version(old_op.revision)?).execute(&mut *tx).await?;
                    update_task(&mut tx, &record, old_task.revision).await?;
                    audit(&mut tx,&record,Some(&op),PersistentAuditKind::OperationTransitioned,json!({"event":"CANCEL","from":old_op.state,"to":op.state,"facts_version":op.facts_version,"original_reference":null})).await?;
                    let old_revision = record.revision;
                    record.revision += 1;
                    record.state = record
                        .state
                        .transition(TaskEvent::Cancel)
                        .map_err(|_| StorageError::IllegalTransition)?;
                    record.checkpoint = RecoveryPoint::Finished;
                    validate_checkpoint(&record, std::slice::from_ref(&op))?;
                    update_task(&mut tx, &record, old_revision).await?;
                    audit(&mut tx,&record,None,PersistentAuditKind::CheckpointSaved,json!({"event":"CANCEL","from":old_task.state,"to":record.state,"checkpoint":record.checkpoint,"completed_nodes":record.completed_nodes})).await?;
                    sqlx::query("UPDATE billing_prepare_requests_v2 SET state='ABANDONED',revision=revision+1,fence_revision=revision+1,closed_at=$1 WHERE realm=$2::uuid AND nonce=$3::uuid")
                        .bind(now).bind(config.realm()).bind(nonce.as_string()).execute(&mut *tx).await?;
                }
                _ => return Err(StorageError::CorruptRecord),
            }
        } else {
            self.recovery_capacity(&mut tx, config, &identity.user_id, false)
                .await?;
            sqlx::query("INSERT INTO billing_prepare_requests_v2(realm,nonce,owner_id,state,revision,attempt_count,fence_revision,closed_at) VALUES($1::uuid,$2::uuid,$3,'ABANDONED',1,0,1,clock_timestamp())")
                .bind(config.realm()).bind(nonce.as_string()).bind(identity.user_id.as_str()).execute(&mut *tx).await?;
        }
        sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
            .execute(&mut *tx)
            .await?;
        self.guard(session, &identity)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        let read = self
            .read_preparation_v2(session, config, expected, nonce)
            .await?;
        if read["state"] != "ABANDONED" || !read["fence_revision"].is_i64() {
            return Err(StorageError::ReconcileRequired);
        }
        Ok(fields(
            envelope(config, nonce),
            json!({"kind":"ABANDONED","state":"ABANDONED","revision":read["revision"],"fence_revision":read["fence_revision"],"task_id":read["task_id"],"operation_id":read["operation_id"]}),
        ))
    }

    async fn admit_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        nonce: &BillingRecoveryNonce,
        target_task: &TaskId,
        target_operation: &OperationId,
        reference: &ConfirmationRef,
    ) -> Result<(Admission, TaskId, OperationId), StorageError> {
        self.recovery_rate(session, config, expected, true).await?;
        let (mut tx, identity) = self.recovery_begin(session, config, expected).await?;
        let r = self
            .recovery_root(&mut tx, &identity, config, nonce, true)
            .await?
            .ok_or(StorageError::NotFound)?;
        self.check_root(&mut tx, &r).await?;
        if r.state != "PREPARED_OPEN" {
            return Err(problem(match r.state.as_str() {
                "SAVED" => "ALREADY_SAVED",
                "ABANDONED" => "PREPARATION_CLOSED",
                _ => "RECONCILE_REQUIRED",
            }));
        }
        let task = r.task.ok_or(StorageError::CorruptRecord)?;
        let operation = r.operation.ok_or(StorageError::CorruptRecord)?;
        if r.confirmation.as_ref() != Some(reference)
            || &task != target_task
            || &operation != target_operation
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let stored =
            crate::confirmation_repository::read_stored_confirmation(&mut tx, &task, reference)
                .await?;
        let cs = self.confirmations.session(session)?;
        stored.check_session(&cs)?;
        crate::confirmation_repository::verify_stored_confirmation(
            &mut tx,
            reference,
            &stored.record.binding,
            cs.digest.as_str(),
            identity.binding_version,
            self.store.clock.as_ref(),
        )
        .await?;
        self.scope(
            session,
            &identity,
            r.accounts.as_ref().ok_or(StorageError::CorruptRecord)?,
        )
        .await?;
        let claim = Uuid::new_v4().to_string();
        let request_digest = hash(&(
            DOMAIN,
            config.realm(),
            nonce.as_string(),
            r.digest,
            reference,
            &cs.digest,
            identity.binding_version,
        ))?
        .as_str()
        .to_string();
        sqlx::query("INSERT INTO billing_prepare_attempts_v2(realm,nonce,attempt_no,claim,request_digest,state) VALUES($1::uuid,$2::uuid,1,$3::uuid,$4,'ADMITTED')")
            .bind(config.realm()).bind(nonce.as_string()).bind(&claim).bind(&request_digest).execute(&mut *tx).await?;
        sqlx::query("UPDATE billing_prepare_requests_v2 SET state='EXECUTION_ADMITTED',attempt_count=1,revision=revision+1 WHERE realm=$1::uuid AND nonce=$2::uuid")
            .bind(config.realm()).bind(nonce.as_string()).execute(&mut *tx).await?;
        sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
            .execute(&mut *tx)
            .await?;
        self.scope(
            session,
            &identity,
            r.accounts.as_ref().ok_or(StorageError::CorruptRecord)?,
        )
        .await?;
        check_active(&stored.record, self.store.clock.as_ref())?;
        self.guard(session, &identity)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok((
            Admission {
                config: config.clone(),
                nonce: nonce.clone(),
                claim,
                request_digest,
                accounts: r.accounts.ok_or(StorageError::CorruptRecord)?,
                bank: self.bank.clone(),
            },
            task,
            operation,
        ))
    }
    pub async fn execute_preparation_v2(
        &self,
        session: &SessionRef,
        config: &BillingRecoveryConfig,
        expected: &str,
        nonce: &BillingRecoveryNonce,
        target_task: &TaskId,
        target_operation: &OperationId,
        reference: &ConfirmationRef,
    ) -> Result<Value, StorageError> {
        let (admission, task, operation) = self
            .admit_v2(
                session,
                config,
                expected,
                nonce,
                target_task,
                target_operation,
                reference,
            )
            .await?;
        let view = self
            .execute_checked(session, &task, &operation, reference, Some(admission))
            .await?;
        let read = self
            .read_preparation_v2(session, config, expected, nonce)
            .await?;
        Ok(fields(
            envelope(config, nonce),
            json!({"kind":"SAVED","state":"SAVED","revision":read["revision"],"task_id":task,"operation_id":operation,"result":{
            "contract_version":1,"scope":SCOPE,"task_id":view.operation.task_id,"task_state":view.task_state,
            "operation_id":view.operation.operation_id,"operation_state":view.operation.state,"saved":view.saved,
            "replayed":view.replayed,"lock_authorized":false,"automatic_retry":false}}),
        ))
    }

    pub(super) async fn recovery_open_unit(
        &self,
        session: &SessionRef,
        task: &TaskId,
        admission: Admission,
    ) -> Result<PermitTransaction, StorageError> {
        let config = &admission.config;
        let (mut tx, identity) = self
            .recovery_begin(session, config, &config.realm())
            .await?;
        let r = self
            .recovery_root(&mut tx, &identity, config, &admission.nonce, true)
            .await?
            .ok_or(StorageError::NotFound)?;
        self.check_root(&mut tx, &r).await?;
        let cs = self.confirmations.session(session)?;
        if r.task.as_ref() != Some(task)
            || r.state != "EXECUTION_ADMITTED"
            || r.session_digest.as_deref() != Some(cs.digest.as_str())
            || r.binding_version != Some(db_version(identity.binding_version)?)
        {
            return Err(StorageError::ReconcileRequired);
        }
        let admitted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_prepare_attempts_v2 WHERE realm=$1::uuid AND nonce=$2::uuid AND claim=$3::uuid AND request_digest=$4 AND state='ADMITTED')")
            .bind(config.realm()).bind(admission.nonce.as_string()).bind(&admission.claim).bind(&admission.request_digest).fetch_one(&mut *tx).await?;
        if !admitted {
            return Err(StorageError::ReconcileRequired);
        }
        let transaction_id = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await?;
        let task_record = crate::repository::lock_task(&mut tx, &identity.user_id, task).await?;
        self.scope(session, &identity, &admission.accounts).await?;
        Ok(PermitTransaction {
            tx: Some(tx),
            store: self.store.clone(),
            session: session.clone(),
            identity,
            task: task_record,
            transaction_id,
            consumed: None,
            final_active: None,
            submission: None,
            transfer_result: None,
            billing_result: None,
            subscription_result: None,
            daily_usage: None,
            last_time: Cell::new(None),
            payment: None,
            recovery: Some(admission),
            commit_started: false,
            card: None,
        })
    }
}

pub(crate) async fn stage_saved(unit: &mut PermitTransaction) -> Result<(), StorageError> {
    if unit.billing_result.is_none() {
        return Err(StorageError::ReconcileRequired);
    }
    let a = unit
        .recovery
        .as_ref()
        .ok_or(StorageError::ReconcileRequired)?;
    let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
    let changed=sqlx::query("UPDATE billing_prepare_attempts_v2 SET state='BUSINESS_COMMITTED',business_txid=$1 WHERE realm=$2::uuid AND nonce=$3::uuid AND claim=$4::uuid AND request_digest=$5 AND state='ADMITTED'")
        .bind(unit.transaction_id).bind(a.config.realm()).bind(a.nonce.as_string()).bind(&a.claim).bind(&a.request_digest).execute(&mut **tx).await?.rows_affected();
    if changed != 1 {
        return Err(StorageError::ReconcileRequired);
    }
    let changed=sqlx::query("UPDATE billing_prepare_requests_v2 SET state='SAVED',revision=revision+1 WHERE realm=$1::uuid AND nonce=$2::uuid AND owner_id=$3 AND state='EXECUTION_ADMITTED' AND attempt_count=1")
        .bind(a.config.realm()).bind(a.nonce.as_string()).bind(unit.identity.user_id.as_str()).execute(&mut **tx).await?.rows_affected();
    if changed != 1 {
        return Err(StorageError::ReconcileRequired);
    }
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut **tx)
        .await?;
    unit.session_guard()?;
    Ok(())
}
pub(crate) async fn final_scope(unit: &mut PermitTransaction) -> Result<(), StorageError> {
    let a = unit
        .recovery
        .as_ref()
        .ok_or(StorageError::ReconcileRequired)?;
    installed(
        unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
        &a.config,
        &a.config.realm(),
    )
    .await?;
    for account in &a.accounts {
        let account = ResourceId::parse(account).map_err(|_| StorageError::CorruptRecord)?;
        let balance = a
            .bank
            .get_account_balance(&unit.identity.user_id, &account)
            .await;
        unit.session_guard()?;
        balance
            .map_err(|_| StorageError::DependencyUnavailable)?
            .validate_for(&account)
            .map_err(|_| StorageError::CorruptRecord)?;
    }
    if let Some(consumed) = unit.consumed.clone() {
        let digest = unit.session_digest()?;
        crate::confirmation_repository::verify_stored_confirmation(
            unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,
            &consumed.confirmation_ref,
            &consumed.binding.snapshot,
            &digest,
            unit.identity.binding_version,
            unit.store.clock.as_ref(),
        )
        .await?;
        unit.session_guard()?;
        consumed.active_time(unit.now()?)?;
    } else {
        return Err(StorageError::ReconcileRequired);
    }
    unit.session_guard()?;
    Ok(())
}
pub(crate) async fn seal_rollback(
    store: &PermitStore,
    session: &SessionRef,
    identity: &SessionBinding,
    task: &TaskId,
    proof: VerifiedRollback,
) -> Result<(), StorageError> {
    let VerifiedRollback { admission: a, txid } = proof;
    if txid <= 0 || store.identity(session)? != *identity {
        return Err(StorageError::ReconcileRequired);
    }
    let mut tx = store.repository.pool.begin().await?;
    installation_lock(&mut tx, false).await?;
    installed(&mut tx, &a.config, &a.config.realm()).await?;
    crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
    mark(&mut tx).await?;
    let record = crate::repository::lock_task(&mut tx, &identity.user_id, task).await?;
    let row=sqlx::query("SELECT *,realm::text AS realm_text,nonce::text AS nonce_text,(SELECT status FROM server_confirmations c WHERE c.confirmation_ref=billing_prepare_requests_v2.confirmation_ref) AS confirmation_status,(SELECT expires_at FROM server_confirmations c WHERE c.confirmation_ref=billing_prepare_requests_v2.confirmation_ref) AS confirmation_expiry FROM billing_prepare_requests_v2 WHERE realm=$1::uuid AND nonce=$2::uuid FOR UPDATE").bind(a.config.realm()).bind(a.nonce.as_string()).fetch_one(&mut *tx).await?;
    let r = root(&row)?;
    if r.state != "EXECUTION_ADMITTED"
        || r.task.as_ref() != Some(task)
        || r.owner != identity.user_id
        || record.state != TaskState::Running
    {
        return Err(StorageError::ReconcileRequired);
    }
    let op = r.operation.ok_or(StorageError::CorruptRecord)?;
    let evidence:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_billing_feedback_results_v1 WHERE operation_id=$1) OR EXISTS(SELECT 1 FROM billing_feedback_v1 WHERE public_operation_id=$1) OR EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=$1 AND status='CONSUMED') OR EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=$1 AND first_operation IS NOT NULL)")
        .bind(op.as_str()).fetch_one(&mut *tx).await?;
    if evidence {
        return Err(StorageError::ReconcileRequired);
    }
    let proof_digest = hash(&(
        DOMAIN,
        &a.claim,
        &a.request_digest,
        txid,
        "EXPLICIT_ROLLBACK_BEFORE_COMMIT",
    ))?;
    let n=sqlx::query("UPDATE billing_prepare_attempts_v2 SET state='ROLLBACK_VERIFIED',business_txid=$1,rollback_proof=$2 WHERE realm=$3::uuid AND nonce=$4::uuid AND claim=$5::uuid AND request_digest=$6 AND state='ADMITTED'")
        .bind(txid).bind(proof_digest.as_str()).bind(a.config.realm()).bind(a.nonce.as_string()).bind(&a.claim).bind(&a.request_digest).execute(&mut *tx).await?.rows_affected();
    if n != 1 {
        return Err(StorageError::ReconcileRequired);
    }
    sqlx::query("UPDATE billing_prepare_requests_v2 SET state='CLOSED_ROLLBACK',revision=revision+1 WHERE realm=$1::uuid AND nonce=$2::uuid AND state='EXECUTION_ADMITTED'")
        .bind(a.config.realm()).bind(a.nonce.as_string()).execute(&mut *tx).await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *tx)
        .await?;
    if store.identity(session)? != *identity {
        return Err(StorageError::Session);
    }
    tx.commit()
        .await
        .map_err(|_| StorageError::ReconcileRequired)?;
    Ok(())
}
