//! Isolated adopted-v1 self-freeze safety and closed transaction assembly.
use super::*;
use crate::card_catalog::read_adopted_card_facts;
use crate::confirmation::ConfirmationSession;
use crate::confirmation_repository::{insert_prepared_confirmation, read_stored_confirmation};
use crate::{new_operation_id, new_task_id};
use crate::{
    CardCatalogSourceBinding, ConfirmationRecord, ConfirmationStatus, PgUnifiedFictionalSource,
    UNIFIED_FICTIONAL_IMPLEMENTATION, UNIFIED_FICTIONAL_SOURCE,
};
use safeflow_card_facts::{
    operation_records::StoredReasonCode, operation_replay::LockReplayParameters, LockSource,
};
use safeflow_card_postgres_store::{
    card_selection::ReadOnlyCardList,
    freeze::{FreezeCommand, FreezeRecordIds},
};
use safeflow_contracts::*;
use safeflow_runtime::{
    BackendFinalPolicy, BackendFinalPolicyRegistration, Runtime, ToolSecurityProfile,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicI64, Ordering};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub(crate) mod final_transaction;

const TOOL: &str = "card_freeze_isolated";
const POLICY: &str = "sf-isolated-demo-policy-20261027-v1";
const GATES: [SecurityGate; 2] = [
    SecurityGate::ExplicitConfirmation,
    SecurityGate::StrongAuthentication,
];

/// Native assembly capability, not Deserialize/Debug or an HTTP registration.
/// A NEW host always has a new generation, invalidating all old auth evidence.
#[derive(Clone)]
pub struct CardSafetyHost {
    generation: uuid::Uuid,
    controller_hash: [u8; 32],
}
impl CardSafetyHost {
    pub fn for_isolated_native_host(secret: Zeroizing<String>) -> Result<Self, StorageError> {
        if !(32..=256).contains(&secret.len()) {
            return Err(StorageError::Invalid(
                "independent controller secret length",
            ));
        }
        Ok(Self {
            generation: uuid::Uuid::new_v4(),
            controller_hash: Sha256::digest(secret.as_bytes()).into(),
        })
    }
    fn authenticate(&self, secret: &str) -> Result<(), StorageError> {
        let h: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        if bool::from(h.ct_eq(&self.controller_hash)) {
            Ok(())
        } else {
            Err(StorageError::FormalAuthUnavailable)
        }
    }
}
/// One-time native export. Does not implement Serialize/Debug/Clone.
pub struct CardChallengeSecret(Zeroizing<String>);
impl CardChallengeSecret {
    pub fn expose_once_for_native_display(&self) -> &str {
        &self.0
    }
}
pub struct CardChallengeDelivery {
    pub challenge_id: String,
    secret: Option<CardChallengeSecret>,
}
impl CardChallengeDelivery {
    pub fn take_secret(&mut self) -> Option<CardChallengeSecret> {
        self.secret.take()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Input {
    input_version: u16,
    mode: String,
    action: String,
    account_id: ResourceId,
    card_id: String,
    member_operation_id: String,
    canonical_key: String,
    lock_id: String,
    operation_record_id: String,
    reason_history_id: String,
    source_revision: String,
    adoption: serde_json::Value,
    facts: serde_json::Value,
    display: serde_json::Value,
}
impl Input {
    fn command(&self) -> FreezeCommand {
        FreezeCommand {
            operation_id: self.member_operation_id.clone(),
            canonical_key: self.canonical_key.clone(),
            parameters: LockReplayParameters {
                card_id: self.card_id.clone(),
                source: LockSource::UserSelf,
                reason: StoredReasonCode::UserSecurityConcern,
                event_id: None,
                transaction_id: None,
                feedback_id: None,
            },
            event_scope: None,
            ids: FreezeRecordIds {
                lock_id: self.lock_id.clone(),
                operation_record_id: self.operation_record_id.clone(),
                reason_history_id: self.reason_history_id.clone(),
            },
        }
    }
}
/// Only original preparation/safety status. Never claims a business outcome.
#[derive(Serialize)]
pub struct CardSafetyView {
    pub operation_id: OperationId,
    pub task_id: TaskId,
    pub confirmation_ref: ConfirmationRef,
    pub state: String,
    pub confirmation_status: ConfirmationStatus,
    pub confirmed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub confirmation_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub parameters: serde_json::Value,
    pub business_executed: bool,
}
#[derive(Clone)]
struct ObservedClock(Arc<AtomicI64>);
impl ConfirmationClock for ObservedClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.0.load(Ordering::SeqCst))
            .expect("trusted DB milliseconds")
    }
}
struct Facts(SecurityContext);
struct Policy;
impl BackendFinalPolicy for Policy {
    fn evaluate<'a>(
        &'a self,
        c: &'a SecurityContext,
        f: &'a (dyn std::any::Any + Send + Sync),
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            if f.downcast_ref::<Facts>().is_some_and(|f| f.0 == *c)
                && c.tool_name.as_str() == TOOL
                && serde_json::from_value::<Input>(c.normalized_arguments.clone()).is_ok()
            {
                Ok(vec![])
            } else {
                Err(SecurityError {
                    reason_code: ReasonCode::parse("CARD_SCOPE_INVALID").unwrap(),
                    message: "checked card context required".into(),
                })
            }
        })
    }
}
pub struct CardSafetyService<'a> {
    store: PermitStore,
    runtime: &'a Runtime,
    source: PgUnifiedFictionalSource,
    catalog: CardCatalogSourceBinding,
    host: CardSafetyHost,
    clock: ObservedClock,
    test_clock: Option<Arc<dyn ConfirmationClock>>,
    policy: BackendFinalPolicyRegistration,
}
pub(super) struct CardProtection {
    input: Input,
    source: PgUnifiedFictionalSource,
    catalog: CardCatalogSourceBinding,
    host: uuid::Uuid,
    clock: ObservedClock,
    test_clock: Option<Arc<dyn ConfirmationClock>>,
    session_epoch: i64,
    account_epoch: i64,
    operation: OperationId,
    pub(super) completion: Option<final_transaction::CardCompletion>,
    admission: Option<final_transaction::Admission>,
}
// Returned only after the guarded pre-commit work has completed. COMMIT itself
// is awaited to its outcome, rather than cancelled by a later session expiry.
struct CheckedCardTransaction<T> {
    tx: Transaction<'static, Postgres>,
    binding: SessionBinding,
    value: T,
}
struct Original {
    input: Input,
    confirmation: ConfirmationRecord,
    session_epoch: i64,
    account_epoch: i64,
    state: String,
}
fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4())
}
fn digest(s: &SessionRef) -> Result<String, StorageError> {
    Ok(hash(&("safeflow-confirmation-session:v1", s.as_str()))?
        .as_str()
        .into())
}
fn nonce(s: &str) -> Result<uuid::Uuid, StorageError> {
    let n =
        uuid::Uuid::parse_str(s).map_err(|_| StorageError::Invalid("canonical nonce required"))?;
    if n.is_nil() || n.to_string() != s {
        return Err(StorageError::Invalid("canonical nonce required"));
    }
    Ok(n)
}
fn code_hash(challenge: &str, code: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("isolated-card-code-v1:{challenge}:{code}").as_bytes())
    )
}
fn facts_projection(
    f: &safeflow_card_facts::account_service::AccountCardFacts,
) -> serde_json::Value {
    let route = match f.unlock_route {
        safeflow_card_facts::account_restrictions::UnlockRoute::OfflineBank => "OFFLINE_BANK",
    };
    json!({"account_id":f.account_id,"revision":f.revision,"active_card_lock_ids":f.active_card_lock_ids,"unlock_route":route,
        "cards":f.cards.iter().map(|c|json!({"card_id":c.card_id,"account_id":c.account_id,"last_four":c.last_four,
            "active_locks":c.active_locks.iter().map(|l|json!({"id":l.id,"reason":l.reason})).collect::<Vec<_>>() })).collect::<Vec<_>>()})
}
async fn time(
    tx: &mut Transaction<'_, Postgres>,
    test: Option<&Arc<dyn ConfirmationClock>>,
    clock: &ObservedClock,
) -> Result<i64, StorageError> {
    let ms = if let Some(c) = test {
        c.now().timestamp_millis()
    } else {
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint")
            .fetch_one(&mut **tx)
            .await?
    };
    if ms < 0 || ms < clock.0.load(Ordering::SeqCst) {
        return Err(StorageError::Invalid("card clock moved backwards"));
    }
    clock.0.store(ms, Ordering::SeqCst);
    Ok(ms)
}
impl<'a> CardSafetyService<'a> {
    pub fn for_isolated_backend(
        source: PgUnifiedFictionalSource,
        sessions: Arc<dyn SessionResolver>,
        runtime: &'a Runtime,
        host: CardSafetyHost,
    ) -> Result<Self, StorageError> {
        if !runtime.final_payment_uses_session_resolver_for_backend(&sessions) {
            return Err(StorageError::Session);
        }
        let clock = ObservedClock(Arc::new(AtomicI64::new(0)));
        let store = PermitStore::for_trusted_backend_with_clock(
            source.repository_for_isolated_backend(),
            sessions,
            PermitConfig::new(Duration::from_secs(60))?,
            Arc::new(clock.clone()),
        );
        let catalog = CardCatalogSourceBinding::for_reviewed_backend_provider(
            UNIFIED_FICTIONAL_SOURCE,
            UNIFIED_FICTIONAL_IMPLEMENTATION,
            Arc::new(source.clone()),
        )?;
        let policy = BackendFinalPolicyRegistration::for_reviewed_backend(
            ToolSecurityProfile {
                tool_name: ToolName::parse(TOOL).unwrap(),
                tool_schema_version: 1,
                policy_version: PolicyVersion::parse(POLICY).unwrap(),
                minimum_level: SecurityLevel::Red,
            },
            "isolated-self-card-safety-v1",
            Arc::new(Policy),
        )
        .map_err(|_| StorageError::DecisionAssemblyChanged)?;
        Ok(Self {
            store,
            runtime,
            source,
            catalog,
            host,
            clock,
            test_clock: None,
            policy,
        })
    }
    #[cfg(test)]
    fn with_clock(mut self, c: Arc<dyn ConfirmationClock>) -> Self {
        self.test_clock = Some(c);
        self
    }
    fn guard(&self, s: &SessionRef, b: &SessionBinding) -> Result<(), StorageError> {
        if self.store.identity(s)? != *b {
            return Err(StorageError::Session);
        }
        Ok(())
    }
    async fn commit_checked<T>(
        &self,
        s: &SessionRef,
        pending: CheckedCardTransaction<T>,
    ) -> Result<T, StorageError> {
        self.guard(s, &pending.binding)?;
        pending
            .tx
            .commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        self.guard(s, &pending.binding)?;
        Ok(pending.value)
    }
    /// Check the original current binding before/after every pre-commit poll,
    /// including nested SQLx/member waits. History adds no execution fence.
    fn session_checked<'c, T: 'c>(
        &'c self,
        s: &'c SessionRef,
        wait: impl std::future::Future<Output = Result<T, StorageError>> + 'c,
    ) -> impl std::future::Future<Output = Result<T, StorageError>> + 'c {
        // Allocate before constructing the outer async state: SQLx/member
        // futures are large in debug builds, especially under nested guards.
        let wait = Box::pin(wait);
        async move {
            let binding = self.store.identity(s)?;
            crate::repository::guarded_poll(&|| self.guard(s, &binding), wait).await
        }
    }
    async fn begin(
        &self,
        s: &SessionRef,
        account: &ResourceId,
        initialize: bool,
    ) -> Result<(Transaction<'static, Postgres>, SessionBinding, i64, i64), StorageError> {
        self.session_checked(s, self.begin_checked(s, account, initialize))
            .await
    }
    async fn begin_checked(
        &self,
        s: &SessionRef,
        account: &ResourceId,
        initialize: bool,
    ) -> Result<(Transaction<'static, Postgres>, SessionBinding, i64, i64), StorageError> {
        let identity = self.store.identity(s)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        final_transaction::deployment_guard(&mut tx, false).await?;
        let d = digest(s)?;
        if initialize {
            // Authorization still must be freshly checked before any COMMIT.
            sqlx::query("INSERT INTO card_safety_session_fences_v1(session_digest,owner_id,binding_version) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(&d).bind(identity.user_id.as_str()).bind(db_version(identity.binding_version)?).execute(&mut *tx).await?;
        }
        let se = session_fence(&mut tx, &identity, &d).await?;
        if initialize {
            sqlx::query("INSERT INTO card_safety_account_fences_v1(owner_id,account_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(identity.user_id.as_str()).bind(account.as_str()).execute(&mut *tx).await?;
        }
        let ae = account_fence(&mut tx, &identity.user_id, account).await?;
        crate::daily_usage::protect_owner(&mut tx, &identity.user_id).await?;
        self.guard(s, &identity)?;
        Ok((tx, identity, se, ae))
    }
    async fn protected_input(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &SessionBinding,
        account: &ResourceId,
    ) -> Result<(serde_json::Value, serde_json::Value), StorageError> {
        self.source
            .protect_card_scope(tx, &identity.user_id, account)
            .await?;
        let facts =
            read_adopted_card_facts(tx, &identity.user_id, account, Some(&self.catalog)).await?;
        let adoption:serde_json::Value=sqlx::query_scalar("SELECT to_jsonb(a) FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1) a").bind(account.as_str()).fetch_one(&mut **tx).await?;
        Ok((facts_projection(&facts), adoption))
    }
    pub async fn prepare(
        &self,
        s: &SessionRef,
        account: &ResourceId,
        card: &str,
        request_nonce: &str,
    ) -> Result<CardSafetyView, StorageError> {
        let pending = self
            .session_checked(s, self.prepare_checked(s, account, card, request_nonce))
            .await?;
        self.commit_checked(s, pending).await
    }
    async fn prepare_checked(
        &self,
        s: &SessionRef,
        account: &ResourceId,
        card: &str,
        request_nonce: &str,
    ) -> Result<CheckedCardTransaction<CardSafetyView>, StorageError> {
        let n = nonce(request_nonce)?;
        valid_id(card)?;
        let identity = self.store.identity(s)?;
        if !self
            .source
            .complete_accounts(&identity.user_id)
            .await?
            .iter()
            .any(|a| a.account_id == *account)
        {
            return Err(StorageError::NotFound);
        }
        self.guard(s, &identity)?;
        let (mut tx, identity, se, ae) = self.begin(s, account, true).await?;
        let existing:Option<String>=sqlx::query_scalar("SELECT operation_id FROM public_card_safety_requests_v1 WHERE owner_id=$1 AND nonce=$2::uuid").bind(identity.user_id.as_str()).bind(n.to_string()).fetch_optional(&mut *tx).await?;
        if let Some(op) = existing {
            let original = self
                .original(
                    &mut tx,
                    s,
                    &identity,
                    &OperationId::parse(op).map_err(|_| StorageError::CorruptRecord)?,
                )
                .await?;
            if original.input.account_id != *account || original.input.card_id != card {
                return Err(StorageError::ExecutionKeyConflict);
            }
            self.check_original(&mut tx, s, &identity, &original, se, ae)
                .await?;
            let view = self.view(&original);
            self.guard(s, &identity)?;
            return Ok(CheckedCardTransaction {
                tx,
                binding: identity,
                value: view,
            });
        }
        let (facts, adoption) = self.protected_input(&mut tx, &identity, account).await?;
        let cards = facts["cards"]
            .as_array()
            .ok_or(StorageError::CorruptRecord)?;
        let position = cards
            .iter()
            .position(|c| c["card_id"] == card)
            .ok_or(StorageError::NotFound)?
            + 1;
        let mut input = Input {
            input_version: 1,
            mode: "ISOLATED_FICTIONAL_SELF_CARD_V1".into(),
            action: "FREEZE_CARD".into(),
            account_id: account.clone(),
            card_id: card.into(),
            member_operation_id: id("member_card_operation"),
            canonical_key: id("card_key"),
            lock_id: id("card_lock"),
            operation_record_id: id("card_record"),
            reason_history_id: id("card_reason"),
            source_revision: UNIFIED_FICTIONAL_IMPLEMENTATION.into(),
            adoption,
            facts,
            display: json!({}),
        };
        // Original member selection and opaque preview, on an independent read-only
        // connection. The first connection holds scope/account/source protection.
        let mut read = self.store.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
            .execute(&mut *read)
            .await?;
        let list = ReadOnlyCardList::read_on_connection(
            &mut read,
            identity.user_id.as_str(),
            account.as_str(),
        )
        .await
        .map_err(|_| StorageError::FactsConflict)?;
        let selected = list
            .select_position(Some(position))
            .map_err(|_| StorageError::FactsConflict)?;
        let preview = selected
            .prepare_freeze_on_connection(
                &mut read,
                identity.user_id.as_str(),
                account.as_str(),
                &input.command(),
            )
            .await
            .map_err(|_| StorageError::FactsConflict)?;
        let v = preview.view();
        input.display = json!({"last_four":v.last_four,"reason":v.requested_public_reason,"impact":v.planned_effect,"unlock_instruction":v.unlock_instruction,"message":v.message});
        read.commit().await?;
        let op = new_operation_id();
        let node = NodeId::parse("card_safety_action").unwrap();
        let parameters = json!(input);
        let plan = AgentPlan::new(
            new_task_id(),
            1,
            vec![PlanNode::Tool {
                id: node.clone(),
                tool: ToolName::parse(TOOL).unwrap(),
                arguments: parameters.clone(),
                depends_on: vec![],
            }],
        )
        .map_err(|_| StorageError::PlanConflict)?;
        // READY is storage readiness only. Independent confirmation/auth/permit
        // below remain mandatory; no Tool can execute this backend-only name.
        let task = TaskRecord {
            task_id: plan.task_id.clone(),
            owner_id: identity.user_id.clone(),
            state: TaskState::Running,
            revision: 1,
            facts_version: 1,
            checkpoint: RecoveryPoint::AtNode {
                node_id: node.clone(),
            },
            plan,
            completed_nodes: vec![],
        };
        let operation = crate::OperationRecord {
            operation_id: op.clone(),
            task_id: task.task_id.clone(),
            node_id: node,
            state: OperationState::Ready,
            revision: 1,
            plan_version: 1,
            facts_version: 1,
            arguments_hash: hash(&parameters)?,
            business_operation_ref: None,
        };
        sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,$2,1,'RUNNING',1,1,1,$3,$4,$5,'[]')")
            .bind(task.task_id.as_str()).bind(identity.user_id.as_str()).bind(Json(&task.plan)).bind(hash(&(task.plan.clone(),1u64))?.as_str()).bind(Json(&task.checkpoint)).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'READY',1,1,1,$4)").bind(op.as_str()).bind(task.task_id.as_str()).bind(operation.node_id.as_str()).bind(operation.arguments_hash.as_str()).execute(&mut *tx).await?;
        let binding = current_binding(&task, &[operation.clone()], &op)?;
        let ms = time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        let confirmation = ConfirmationRecord {
            contract_version: 1,
            confirmation_ref: ConfirmationRef::parse(id("confirmation")).unwrap(),
            binding,
            status: ConfirmationStatus::Prepared,
            prepared_at: chrono::DateTime::from_timestamp_millis(ms).unwrap(),
            expires_at: chrono::DateTime::parse_from_rfc3339("9999-12-31T00:00:00Z")
                .unwrap()
                .to_utc(),
            confirmed_at: None,
            revoked_at: None,
        };
        // Sentinel is only an unconfirmed preparation, never an auth duration.
        insert_prepared_confirmation(
            &mut tx,
            &ConfirmationSession {
                binding: identity.clone(),
                digest: hash(&("safeflow-confirmation-session:v1", s.as_str()))?,
            },
            &confirmation,
        )
        .await?;
        sqlx::query("INSERT INTO public_card_safety_requests_v1(operation_id,task_id,owner_id,nonce,session_digest,binding_version,session_epoch,account_epoch,account_id,card_id,input,input_digest,confirmation_ref) VALUES($1,$2,$3,$4::uuid,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
            .bind(op.as_str()).bind(task.task_id.as_str()).bind(identity.user_id.as_str()).bind(n.to_string()).bind(digest(s)?).bind(db_version(identity.binding_version)?).bind(se).bind(ae).bind(account.as_str()).bind(card).bind(Json(&input)).bind(hash(&json!(input))?.as_str()).bind(confirmation.confirmation_ref.as_str()).execute(&mut *tx).await?;
        audit(&mut tx,&task,None,PersistentAuditKind::TaskCreated,json!({"scope":"ISOLATED_CARD_SAFETY_V1","plan_version":1,"creation_digest":hash(&(task.plan.clone(),1u64))?})).await?;
        audit(&mut tx,&task,Some(&operation),PersistentAuditKind::OperationCreated,json!({"scope":"ISOLATED_CARD_SAFETY_V1","state":"READY","arguments_hash":operation.arguments_hash})).await?;
        audit(
            &mut tx,
            &task,
            Some(&operation),
            PersistentAuditKind::ConfirmationPrepared,
            crate::confirmation_repository::confirmation_details(&confirmation)?,
        )
        .await?;
        self.guard(s, &identity)?;
        self.guard(s, &identity)?;
        let view = self.view(&Original {
            input,
            confirmation,
            session_epoch: se,
            account_epoch: ae,
            state: "PREPARED".into(),
        });
        Ok(CheckedCardTransaction {
            tx,
            binding: identity,
            value: view,
        })
    }
    async fn locate(&self, s: &SessionRef, op: &OperationId) -> Result<ResourceId, StorageError> {
        let identity = self.store.identity(s)?;
        let a:String=sqlx::query_scalar("SELECT account_id FROM public_card_safety_requests_v1 WHERE operation_id=$1 AND owner_id=$2").bind(op.as_str()).bind(identity.user_id.as_str()).fetch_optional(&self.store.repository.subscription_read_pool).await?.ok_or(StorageError::NotFound)?;
        self.guard(s, &identity)?;
        ResourceId::parse(a).map_err(|_| StorageError::CorruptRecord)
    }
    async fn original(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        identity: &SessionBinding,
        op: &OperationId,
    ) -> Result<Original, StorageError> {
        let r = sqlx::query(
            "SELECT * FROM public_card_safety_requests_v1 WHERE operation_id=$1 AND owner_id=$2",
        )
        .bind(op.as_str())
        .bind(identity.user_id.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::NotFound)?;
        if r.try_get::<String, _>("session_digest")? != digest(s)?
            || r.try_get::<i64, _>("binding_version")? != db_version(identity.binding_version)?
        {
            return Err(StorageError::Session);
        }
        let task_id: TaskId = decode(&r, "task_id")?;
        let task = lock_task(tx, &identity.user_id, &task_id).await?;
        let ops = operations(tx, &task_id).await?;
        let binding = current_binding(&task, &ops, op)?;
        let c = read_stored_confirmation(tx, &task_id, &decode(&r, "confirmation_ref")?).await?;
        let input: Input = r.try_get::<Json<Input>, _>("input")?.0;
        if input.input_version != 1
            || input.action != "FREEZE_CARD"
            || input.mode != "ISOLATED_FICTIONAL_SELF_CARD_V1"
            || input.source_revision != UNIFIED_FICTIONAL_IMPLEMENTATION
            || hash(&json!(input))?.as_str() != r.try_get::<String, _>("input_digest")?
            || hash(&json!(input))? != binding.arguments_hash
            || binding != c.record.binding
            || c.session_digest != digest(s)?
            || c.session_binding_version != identity.binding_version
            || input.account_id.as_str() != r.try_get::<String, _>("account_id")?
            || input.card_id != r.try_get::<String, _>("card_id")?
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(Original {
            input,
            confirmation: c.record,
            session_epoch: r.try_get("session_epoch")?,
            account_epoch: r.try_get("account_epoch")?,
            state: r.try_get("state")?,
        })
    }
    async fn check_original(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        identity: &SessionBinding,
        o: &Original,
        se: i64,
        ae: i64,
    ) -> Result<(), StorageError> {
        if o.state != "PREPARED" {
            return Err(if o.state == "REVOKED" {
                StorageError::ConfirmationRevoked
            } else {
                StorageError::ReconcileRequired
            });
        }
        if se != o.session_epoch || ae != o.account_epoch {
            return Err(StorageError::AuthGrantRevoked);
        }
        let (f, a) = self
            .protected_input(tx, identity, &o.input.account_id)
            .await?;
        if f != o.input.facts || a != o.input.adoption {
            return Err(StorageError::FactsConflict);
        }
        self.guard(s, identity)
    }
    fn view(&self, o: &Original) -> CardSafetyView {
        CardSafetyView {
            operation_id: o.confirmation.binding.operation_id.clone(),
            task_id: o.confirmation.binding.task_id.clone(),
            confirmation_ref: o.confirmation.confirmation_ref.clone(),
            state: o.state.clone(),
            confirmation_status: o.confirmation.status,
            confirmed_at: o.confirmation.confirmed_at,
            confirmation_expires_at: o
                .confirmation
                .confirmed_at
                .map(|_| o.confirmation.expires_at),
            parameters: json!({"mode":o.input.mode,"action":o.input.action,"account_id":o.input.account_id,"card_id":o.input.card_id,"display":o.input.display}),
            business_executed: false,
        }
    }
    async fn locked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<(Transaction<'static, Postgres>, SessionBinding, Original), StorageError> {
        let a = self.locate(s, op).await?;
        let (mut tx, b, se, ae) = self.begin(s, &a, false).await?;
        let o = self.original(&mut tx, s, &b, op).await?;
        final_transaction::journal_lock(&mut tx, op).await?;
        if o.input.account_id != a {
            return Err(StorageError::FactsConflict);
        }
        self.check_original(&mut tx, s, &b, &o, se, ae).await?;
        Ok((tx, b, o))
    }
    /// Private owner lookup after an interrupted preparation. No new operation.
    pub async fn query_nonce(&self,s:&SessionRef,n:&str)->Result<CardSafetyView,StorageError>{
        let n=nonce(n)?;let identity=self.store.identity(s)?;
        let op:String=sqlx::query_scalar("SELECT operation_id FROM public_card_safety_requests_v1 WHERE owner_id=$1 AND nonce=$2::uuid")
            .bind(identity.user_id.as_str()).bind(n.to_string()).fetch_optional(&self.store.repository.pool).await?.ok_or(StorageError::NotFound)?;
        self.guard(s,&identity)?;self.query(s,&OperationId::parse(op).map_err(|_|StorageError::CorruptRecord)?).await
    }
    pub async fn query(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CardSafetyView, StorageError> {
        let pending = self.session_checked(s, self.query_checked(s, op)).await?;
        self.commit_checked(s, pending).await
    }
    async fn query_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CheckedCardTransaction<CardSafetyView>, StorageError> {
        // History uses current read identity, not the old execution session,
        // challenge generation, active fence or old catalog revision.
        let b = self.store.identity(s)?;
        let mut tx = self.store.repository.pool.begin().await?;
        let r = sqlx::query(
            "SELECT * FROM public_card_safety_requests_v1 WHERE operation_id=$1 AND owner_id=$2",
        )
        .bind(op.as_str())
        .bind(b.user_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StorageError::NotFound)?;
        let account: String = r.try_get("account_id")?;
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT owner_id FROM transfer_accounts_v1 WHERE account_id=$1 FOR SHARE",
        )
        .bind(&account)
        .fetch_optional(&mut *tx)
        .await?;
        if owner.as_deref() != Some(b.user_id.as_str()) {
            return Err(StorageError::NotFound);
        }
        // The owner SHARE lease may have waited behind a cancellation. Read
        // original state AFTER that lease, then read confirmation without an
        // UPDATE lock (avoids inverting the execution lock order).
        let r = sqlx::query(
            "SELECT * FROM public_card_safety_requests_v1 WHERE operation_id=$1 AND owner_id=$2",
        )
        .bind(op.as_str())
        .bind(b.user_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let task: TaskId = decode(&r, "task_id")?;
        let c = crate::confirmation_repository::read_historical_confirmation(
            &mut tx,
            &task,
            &decode(&r, "confirmation_ref")?,
        )
        .await?;
        let input: Input = r.try_get::<Json<Input>, _>("input")?.0;
        if hash(&json!(input))?.as_str() != r.try_get::<String, _>("input_digest")?
            || hash(&json!(input))? != c.record.binding.arguments_hash
            || c.record.binding.user_id != b.user_id
            || c.record.binding.operation_id != *op
            || input.account_id.as_str() != account
            || input.card_id != r.try_get::<String, _>("card_id")?
        {
            return Err(StorageError::CorruptRecord);
        }
        let o = Original {
            input,
            confirmation: c.record,
            session_epoch: r.try_get("session_epoch")?,
            account_epoch: r.try_get("account_epoch")?,
            state: r.try_get("state")?,
        };
        let v = self.view(&o);
        self.guard(s, &b)?;
        Ok(CheckedCardTransaction {
            tx,
            binding: b,
            value: v,
        })
    }
    pub async fn confirm(
        &self,
        s: &SessionRef,
        op: &OperationId,
        displayed: &ConfirmationRef,
    ) -> Result<CardSafetyView, StorageError> {
        let pending = self
            .session_checked(s, self.confirm_checked(s, op, displayed))
            .await?;
        self.commit_checked(s, pending).await
    }
    async fn confirm_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
        displayed: &ConfirmationRef,
    ) -> Result<CheckedCardTransaction<CardSafetyView>, StorageError> {
        let (mut tx, b, mut o) = self.locked(s, op).await?;
        if o.confirmation.confirmation_ref != *displayed {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let now = time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        if o.confirmation.status == ConfirmationStatus::Prepared {
            o.confirmation.status = ConfirmationStatus::Confirmed;
            o.confirmation.confirmed_at = chrono::DateTime::from_timestamp_millis(now);
            o.confirmation.expires_at = chrono::DateTime::from_timestamp_millis(
                now.checked_add(300000)
                    .ok_or(StorageError::ConfirmationExpired)?,
            )
            .unwrap();
            sqlx::query("UPDATE server_confirmations SET status='CONFIRMED',confirmed_at=$1,expires_at=$2 WHERE confirmation_ref=$3 AND status='PREPARED'").bind(o.confirmation.confirmed_at).bind(o.confirmation.expires_at).bind(displayed.as_str()).execute(&mut *tx).await?;
            let t = lock_task(&mut tx, &b.user_id, &o.confirmation.binding.task_id).await?;
            let ops = operations(&mut tx, &t.task_id).await?;
            audit(
                &mut tx,
                &t,
                ops.first(),
                PersistentAuditKind::ExplicitConfirmationRecorded,
                crate::confirmation_repository::confirmation_details(&o.confirmation)?,
            )
            .await?;
        }
        self.final_confirmation(&mut tx, s, &b, &o).await?;
        Ok(CheckedCardTransaction {
            tx,
            binding: b,
            value: self.view(&o),
        })
    }
    /// No corresponding HTTP route. Controller secret is distinct from login.
    pub async fn challenge_for_native_controller(
        &self,
        controller_secret: &str,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CardChallengeDelivery, StorageError> {
        let pending = self
            .session_checked(
                s,
                self.challenge_for_native_controller_checked(controller_secret, s, op),
            )
            .await?;
        self.commit_checked(s, pending).await
    }
    async fn challenge_for_native_controller_checked(
        &self,
        controller_secret: &str,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CheckedCardTransaction<CardChallengeDelivery>, StorageError> {
        self.host.authenticate(controller_secret)?;
        let (mut tx, b, o) = self.locked(s, op).await?;
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT challenge_id::text FROM card_safety_challenges_v1 WHERE operation_id=$1",
        )
        .bind(op.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let now = time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        self.confirmed(&o)?;
        if let Some(cid) = existing {
            self.final_confirmation(&mut tx, s, &b, &o).await?;
            return Ok(CheckedCardTransaction {
                tx,
                binding: b,
                value: CardChallengeDelivery {
                    challenge_id: cid,
                    secret: None,
                },
            });
        }
        // Rejection sampling removes modulo bias. Six decimal digits, including zeroes.
        let number = loop {
            let mut v = [0u8; 4];
            getrandom::getrandom(&mut v).map_err(|_| StorageError::FormalAuthUnavailable)?;
            let n = u32::from_le_bytes(v);
            if n < 4_294_000_000 {
                break n % 1_000_000;
            }
        };
        let code = Zeroizing::new(format!("{number:06}"));
        let cid = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO card_safety_challenges_v1(challenge_id,operation_id,host_generation,code_hash,created_ms,expires_ms) VALUES($1::uuid,$2,$3::uuid,$4,$5,$6)").bind(&cid).bind(op.as_str()).bind(self.host.generation.to_string()).bind(code_hash(&cid,&code)).bind(now).bind(now+120000).execute(&mut *tx).await?;
        self.safe_audit(
            &mut tx,
            &b,
            &o,
            PersistentAuditKind::SimAuthChallengeCreated,
            json!({"mode":"ISOLATED_CARD_6_DIGIT","challenge_id":cid,"expires_at_ms":now+120000}),
        )
        .await?;
        let final_now = self.final_confirmation(&mut tx, s, &b, &o).await?;
        if final_now >= now + 120000 {
            return Err(StorageError::AuthExpired);
        }
        Ok(CheckedCardTransaction {
            tx,
            binding: b,
            value: CardChallengeDelivery {
                challenge_id: cid,
                secret: Some(CardChallengeSecret(code)),
            },
        })
    }
    async fn final_confirmation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        b: &SessionBinding,
        o: &Original,
    ) -> Result<i64, StorageError> {
        let current = self
            .original(tx, s, b, &o.confirmation.binding.operation_id)
            .await?;
        if current.input != o.input
            || current.confirmation.binding != o.confirmation.binding
            || current.confirmation.confirmation_ref != o.confirmation.confirmation_ref
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        self.check_original(tx, s, b, &current, o.session_epoch, o.account_epoch)
            .await?;
        let now = time(tx, self.test_clock.as_ref(), &self.clock).await?;
        self.confirmed(&current)?;
        self.guard(s, b)?;
        Ok(now)
    }
    fn confirmed(&self, o: &Original) -> Result<(), StorageError> {
        crate::confirmation_repository::check_active(&o.confirmation, &self.clock)?;
        if o.confirmation.status != ConfirmationStatus::Confirmed {
            return Err(StorageError::ConfirmationRequired);
        }
        Ok(())
    }
    pub async fn verify_code(
        &self,
        s: &SessionRef,
        op: &OperationId,
        challenge: &str,
        code: Zeroizing<String>,
    ) -> Result<bool, StorageError> {
        let pending = self
            .session_checked(s, self.verify_code_checked(s, op, challenge, code))
            .await?;
        self.commit_checked(s, pending).await
    }
    async fn verify_code_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
        challenge: &str,
        code: Zeroizing<String>,
    ) -> Result<CheckedCardTransaction<bool>, StorageError> {
        nonce(challenge)?;
        let (mut tx, b, o) = self.locked(s, op).await?;
        let r=sqlx::query("SELECT *,host_generation::text AS host_text FROM card_safety_challenges_v1 WHERE challenge_id=$1::uuid AND operation_id=$2 FOR UPDATE").bind(challenge).bind(op.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::AuthChallengeInactive)?;
        // Sample only after obtaining the protected challenge row. The hash/code
        // comparison below has no await, so verified_ms is this verification time.
        let now = time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        self.confirmed(&o)?;
        challenge_active(&r, self.host.generation, now)?;
        let expected: String = r.try_get("code_hash")?;
        let actual = code_hash(challenge, &code);
        let success = code.len() == 6
            && code.bytes().all(|b| b.is_ascii_digit())
            && bool::from(expected.as_bytes().ct_eq(actual.as_bytes()));
        let kind = if success {
            sqlx::query("UPDATE card_safety_challenges_v1 SET state='VERIFIED',verified_ms=$1,grant_expires_ms=$2,grant_ref=$3 WHERE challenge_id=$4::uuid").bind(now).bind(now+120000).bind(id("isolated_card_grant")).bind(challenge).execute(&mut *tx).await?;
            PersistentAuditKind::SimAuthVerified
        } else {
            sqlx::query("UPDATE card_safety_challenges_v1 SET failures=failures+1,state=CASE WHEN failures+1=5 THEN 'FAILED' ELSE 'PENDING' END WHERE challenge_id=$1::uuid").bind(challenge).execute(&mut *tx).await?;
            PersistentAuditKind::SimAuthFailed
        };
        self.safe_audit(
            &mut tx,
            &b,
            &o,
            kind,
            json!({"mode":"ISOLATED_CARD_6_DIGIT","challenge_id":challenge}),
        )
        .await?;
        let final_now = self.final_confirmation(&mut tx, s, &b, &o).await?;
        // The locked pre-update row supplies the original challenge deadline;
        // do not interpret our VERIFIED/FAILED update as a new challenge.
        challenge_active(&r, self.host.generation, final_now)?;
        if success && final_now >= now + 120000 {
            return Err(StorageError::AuthExpired);
        }
        self.guard(s, &b)?;
        Ok(CheckedCardTransaction {
            tx,
            binding: b,
            value: success,
        })
    }
    async fn safe_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        b: &SessionBinding,
        o: &Original,
        kind: PersistentAuditKind,
        value: serde_json::Value,
    ) -> Result<(), StorageError> {
        let t = lock_task(tx, &b.user_id, &o.confirmation.binding.task_id).await?;
        let ops = operations(tx, &t.task_id).await?;
        audit(tx, &t, ops.first(), kind, value).await
    }
    pub async fn issue_permit(&self, s: &SessionRef, op: &OperationId) -> Result<(), StorageError> {
        let unit = self
            .session_checked(s, self.issue_permit_checked(s, op))
            .await?;
        let binding = unit.identity.clone();
        unit.finish(false).await?;
        self.guard(s, &binding)
    }
    async fn issue_permit_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<PermitTransaction, StorageError> {
        let (mut unit, o) = self.unit(s, op).await?;
        guard_unit(&mut unit).await?;
        let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let c =
            sqlx::query("SELECT * FROM card_safety_challenges_v1 WHERE operation_id=$1 FOR UPDATE")
                .bind(op.as_str())
                .fetch_one(&mut **tx)
                .await?;
        let grant: String = c.try_get("grant_ref")?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT permit_id FROM execution_permits WHERE operation_id=$1 AND card_attempt_no=(SELECT COALESCE(MAX(attempt_no),0) FROM public_card_attempts_v1 WHERE operation_id=$1 AND state='ROLLBACK_VERIFIED')")
                .bind(op.as_str())
                .fetch_optional(&mut **tx)
                .await?;
        if let Some(p) = existing {
            let binding = PermitBinding {
                snapshot: o.confirmation.binding.clone(),
                fingerprint: compute_operation_fingerprint(&payload(&o)?)
                    .map_err(|_| StorageError::PermitBindingMismatch)?,
            };
            unit.checked_row(
                &PermitId::parse(p).map_err(|_| StorageError::CorruptRecord)?,
                &binding,
            )
            .await?;
            return Ok(unit);
        }
        let b = &o.confirmation.binding;
        let context = SecurityContext {
            contract_version: SECURITY_CONTRACT_VERSION,
            phase: SecurityPhase::PreCommit,
            task_id: b.task_id.clone(),
            node_id: b.node_id.clone(),
            operation_id: op.clone(),
            facts_version: 1,
            tool_name: b.tool_name.clone(),
            plan_version: b.plan_version,
            authenticated_user_id: b.user_id.clone(),
            normalized_arguments: json!(o.input),
            arguments_hash: b.arguments_hash.clone(),
            idempotency_key: Some(IdempotencyKey::parse(o.input.canonical_key.clone()).unwrap()),
            confirmation_ref: Some(o.confirmation.confirmation_ref.clone()),
            auth_grant_ref: None,
        };
        let decision = self
            .runtime
            .final_nonfunding_policy_for_backend(s, &context, &self.policy, &Facts(context.clone()))
            .await
            .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        // The existing Kernel's Red verdict is WAITING_AUTH, a mandatory gate
        // requirement, not a completed-auth assertion. Only this sealed issuer
        // can satisfy it with the transaction-locked confirmation + one-use
        // challenge verified above and again by finish(). Never relabel the
        // Kernel verdict or accept client-auth facts; higher/blocked/unknown
        // policy results remain closed.
        if decision.outcome != SecurityOutcome::WaitingAuth
            || decision.minimum_level != SecurityLevel::Red
            || decision.required_gates != GATES
            || decision
                .findings
                .iter()
                .any(|f| f.finding.kind != FindingKind::Informational)
        {
            return Err(StorageError::FormalAuthUnavailable);
        }
        let verified = VerifiedPermitSnapshot {
            binding: b.clone(),
            payload: payload(&o)?,
            decision_id: decision.decision_id.clone(),
            confirmation_ref: o.confirmation.confirmation_ref.clone(),
            required_gates: decision.required_gates.clone(),
            authentication: VerifiedPermitAuthentication::IsolatedCard {
                grant_ref: AuthGrantRef::parse(grant).map_err(|_| StorageError::CorruptRecord)?,
            },
            execution_scope: VerifiedPermitScope::IsolatedCard,
            session_digest: unit.session_digest()?,
            session_version: unit.identity.binding_version,
        };
        self.safe_audit(unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?,&unit.identity,&o,PersistentAuditKind::SecurityDecisionRecorded,json!({"scope":"ISOLATED_CARD_SAFETY_V1","decision":decision,"required_gates_satisfied_from_persisted_original":true})).await?;
        unit.save(&verified).await?;
        Ok(unit)
    }
    // Single owner of the transaction; future Step 07 can consume only this
    // crate-private unit. No public connection, callback or completion claim.
    async fn unit(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<(PermitTransaction, Original), StorageError> {
        self.session_checked(s, self.unit_checked(s, op)).await
    }
    async fn unit_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<(PermitTransaction, Original), StorageError> {
        let (mut tx, b, o) = self.locked(s, op).await?;
        let transaction_id = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await?;
        let task = lock_task(&mut tx, &b.user_id, &o.confirmation.binding.task_id).await?;
        time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        self.confirmed(&o)?;
        let protection = CardProtection {
            input: o.input.clone(),
            source: self.source.clone(),
            catalog: self.catalog.clone(),
            host: self.host.generation,
            clock: self.clock.clone(),
            test_clock: self.test_clock.clone(),
            session_epoch: o.session_epoch,
            account_epoch: o.account_epoch,
            operation: op.clone(),
            completion: None,
            admission: None,
        };
        let u = PermitTransaction {
            tx: Some(tx),
            store: self.store.clone(),
            session: s.clone(),
            identity: b,
            task,
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
            recovery: None,
            commit_started: false,
            card: Some(protection),
        };
        Ok((u, o))
    }
    pub async fn check_permit(&self, s: &SessionRef, op: &OperationId) -> Result<(), StorageError> {
        self.session_checked(s, self.check_permit_checked(s, op))
            .await
    }
    async fn check_permit_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<(), StorageError> {
        let (mut u, o) = self.unit(s, op).await?;
        guard_unit(&mut u).await?;
        let p: Option<String> =
            sqlx::query_scalar("SELECT permit_id FROM execution_permits WHERE operation_id=$1 AND card_attempt_no=(SELECT COALESCE(MAX(attempt_no),0) FROM public_card_attempts_v1 WHERE operation_id=$1 AND state='ROLLBACK_VERIFIED')")
                .bind(op.as_str())
                .fetch_optional(&mut **u.tx.as_mut().ok_or(StorageError::TransactionAborted)?)
                .await?;
        let p = PermitId::parse(p.ok_or(StorageError::PermitNotActive)?)
            .map_err(|_| StorageError::CorruptRecord)?;
        let binding = PermitBinding {
            snapshot: o.confirmation.binding.clone(),
            fingerprint: compute_operation_fingerprint(&payload(&o)?)
                .map_err(|_| StorageError::PermitBindingMismatch)?,
        };
        u.checked_row(&p, &binding).await?;
        u.final_guard(false).await?;
        u.rollback().await;
        Ok(())
    }
    pub async fn cancel(&self, s: &SessionRef, op: &OperationId) -> Result<(), StorageError> {
        let pending = self.session_checked(s, self.cancel_checked(s, op)).await?;
        self.commit_checked(s, pending).await
    }
    async fn cancel_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CheckedCardTransaction<()>, StorageError> {
        let (mut tx, b, o) = self.locked(s, op).await?;
        let now = time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        sqlx::query("UPDATE public_card_safety_requests_v1 SET state='REVOKED' WHERE operation_id=$1 AND state='PREPARED'").bind(op.as_str()).execute(&mut *tx).await?;
        sqlx::query("UPDATE server_confirmations SET status='REVOKED',revoked_at=$1 WHERE confirmation_ref=$2 AND status<>'REVOKED'").bind(chrono::DateTime::from_timestamp_millis(now).unwrap()).bind(o.confirmation.confirmation_ref.as_str()).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_permits SET status='REVOKED' WHERE operation_id=$1 AND status='ACTIVE'").bind(op.as_str()).execute(&mut *tx).await?;
        self.safe_audit(&mut tx,&b,&o,PersistentAuditKind::ConfirmationRevoked,json!({"scope":"ISOLATED_CARD_SAFETY_V1","confirmation_ref":o.confirmation.confirmation_ref})).await?;
        self.guard(s, &b)?;
        Ok(CheckedCardTransaction {
            tx,
            binding: b,
            value: (),
        })
    }
    /// Call BEFORE resolver logout/rebind. Same durable rank-0 row as completion.
    pub async fn revoke_session(&self, s: &SessionRef) -> Result<(), StorageError> {
        let b = self.store.identity(s)?;
        let d = digest(s)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("INSERT INTO card_safety_session_fences_v1(session_digest,owner_id,binding_version,revoked) VALUES($1,$2,$3,true) ON CONFLICT DO NOTHING").bind(&d).bind(b.user_id.as_str()).bind(db_version(b.binding_version)?).execute(&mut *tx).await?;
        sqlx::query("SELECT session_digest FROM card_safety_session_fences_v1 WHERE session_digest=$1 FOR UPDATE").bind(&d).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE card_safety_session_fences_v1 SET revoked=true,epoch=epoch+1 WHERE session_digest=$1 AND NOT revoked").bind(&d).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok(())
    }
    /// Native maintenance only, authenticated independently of client session.
    pub async fn revoke_account_for_native_controller(
        &self,
        secret: &str,
        owner: &UserId,
        account: &ResourceId,
    ) -> Result<(), StorageError> {
        self.host.authenticate(secret)?;
        let mut tx = self.store.repository.pool.begin().await?;
        // Insert a tombstone even if no preparation exists yet.
        sqlx::query("INSERT INTO card_safety_account_fences_v1(owner_id,account_id,revoked) VALUES($1,$2,true) ON CONFLICT DO NOTHING").bind(owner.as_str()).bind(account.as_str()).execute(&mut *tx).await?;
        sqlx::query("SELECT epoch FROM card_safety_account_fences_v1 WHERE owner_id=$1 AND account_id=$2 FOR UPDATE").bind(owner.as_str()).bind(account.as_str()).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE card_safety_account_fences_v1 SET revoked=true,epoch=epoch+1 WHERE owner_id=$1 AND account_id=$2 AND NOT revoked").bind(owner.as_str()).bind(account.as_str()).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok(())
    }
}
fn payload(o: &Original) -> Result<FingerprintPayloadV1, StorageError> {
    let b = &o.confirmation.binding;
    Ok(FingerprintPayloadV1 {
        user_id: b.user_id.clone(),
        operation_id: b.operation_id.clone(),
        tool_name: b.tool_name.clone(),
        resource_id: o.input.account_id.clone(),
        normalized_arguments: json!(o.input),
        plan_version: b.plan_version,
        schema_version: 1,
    })
}
async fn session_fence(
    tx: &mut Transaction<'_, Postgres>,
    b: &SessionBinding,
    d: &str,
) -> Result<i64, StorageError> {
    let r = sqlx::query(
        "SELECT * FROM card_safety_session_fences_v1 WHERE session_digest=$1 FOR UPDATE",
    )
    .bind(d)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::Session)?;
    if r.try_get::<bool, _>("revoked")?
        || r.try_get::<String, _>("owner_id")? != b.user_id.as_str()
        || r.try_get::<i64, _>("binding_version")? != db_version(b.binding_version)?
    {
        return Err(StorageError::Session);
    }
    Ok(r.try_get("epoch")?)
}
async fn account_fence(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    a: &ResourceId,
) -> Result<i64, StorageError> {
    let r=sqlx::query("SELECT epoch,revoked FROM card_safety_account_fences_v1 WHERE owner_id=$1 AND account_id=$2 FOR UPDATE").bind(owner.as_str()).bind(a.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::NotFound)?;
    if r.try_get::<bool, _>("revoked")? {
        return Err(StorageError::AuthGrantRevoked);
    }
    Ok(r.try_get("epoch")?)
}
fn challenge_active(r: &PgRow, host: uuid::Uuid, now: i64) -> Result<(), StorageError> {
    let actual = r
        .try_get::<String, _>("host_text")
        .map_err(|_| StorageError::CorruptRecord)?;
    if actual != host.to_string()
        || r.try_get::<String, _>("state")? != "PENDING"
        || r.try_get::<i16, _>("failures")? >= 5
    {
        return Err(StorageError::AuthChallengeInactive);
    }
    if now < r.try_get::<i64, _>("created_ms")? || now >= r.try_get::<i64, _>("expires_ms")? {
        return Err(StorageError::AuthExpired);
    }
    Ok(())
}
// Protect the original PermitTransaction final guard, then let its unchanged
// finish() own COMMIT. The other (non-card) scopes keep their original behavior.
pub(super) async fn guard_final_unit(
    u: &mut PermitTransaction,
    require_consumed: bool,
) -> Result<(), StorageError> {
    let store = u.store.clone();
    let session = u.session.clone();
    let binding = u.identity.clone();
    let guard = || {
        if store.identity(&session)? != binding {
            return Err(StorageError::Session);
        }
        Ok(())
    };
    crate::repository::guarded_poll(&guard, Box::pin(u.final_guard_checked(require_consumed))).await
}
pub(super) async fn guard_unit(u: &mut PermitTransaction) -> Result<(), StorageError> {
    let store = u.store.clone();
    let session = u.session.clone();
    let binding = u.identity.clone();
    let guard = || {
        if store.identity(&session)? != binding {
            return Err(StorageError::Session);
        }
        Ok(())
    };
    crate::repository::guarded_poll(&guard, Box::pin(guard_unit_checked(u))).await
}
async fn guard_unit_checked(u: &mut PermitTransaction) -> Result<(), StorageError> {
    let p = u.card.as_ref().ok_or(StorageError::FormalAuthUnavailable)?;
    let tx = u.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
    if session_fence(tx, &u.identity, &digest(&u.session)?).await? != p.session_epoch
        || account_fence(tx, &u.identity.user_id, &p.input.account_id).await? != p.account_epoch
    {
        return Err(StorageError::AuthGrantRevoked);
    }
    p.source
        .protect_card_scope(tx, &u.identity.user_id, &p.input.account_id)
        .await?;
    let facts = read_adopted_card_facts(
        tx,
        &u.identity.user_id,
        &p.input.account_id,
        Some(&p.catalog),
    )
    .await?;
    let adoption:serde_json::Value=sqlx::query_scalar("SELECT to_jsonb(a) FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1) a").bind(p.input.account_id.as_str()).fetch_one(&mut **tx).await?;
    let expected = p.completion.as_ref().map(|c| &c.facts).unwrap_or(&p.input.facts);
    if facts_projection(&facts) != *expected || adoption != p.input.adoption {
        return Err(StorageError::FactsConflict);
    }
    let state: String = sqlx::query_scalar(
        "SELECT state FROM public_card_safety_requests_v1 WHERE operation_id=$1",
    )
    .bind(p.operation.as_str())
    .fetch_one(&mut **tx)
    .await?;
    if state != "PREPARED" {
        return Err(StorageError::ReconcileRequired);
    }
    let r=sqlx::query("SELECT *,host_generation::text AS host_text FROM card_safety_challenges_v1 WHERE operation_id=$1 FOR UPDATE").bind(p.operation.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::FormalAuthUnavailable)?;
    let now = time(tx, p.test_clock.as_ref(), &p.clock).await?;
    if r.try_get::<String, _>("state")? != "VERIFIED"
        || r.try_get::<String, _>("host_text")? != p.host.to_string()
    {
        return Err(StorageError::FormalAuthUnavailable);
    }
    if now < r.try_get::<i64, _>("verified_ms")?
        || now >= r.try_get::<i64, _>("grant_expires_ms")?
    {
        return Err(StorageError::AuthExpired);
    }
    let confirmation: String = sqlx::query_scalar(
        "SELECT confirmation_ref FROM public_card_safety_requests_v1 WHERE operation_id=$1",
    )
    .bind(p.operation.as_str())
    .fetch_one(&mut **tx)
    .await?;
    let c = read_stored_confirmation(
        tx,
        &u.task.task_id,
        &ConfirmationRef::parse(confirmation).map_err(|_| StorageError::CorruptRecord)?,
    )
    .await?;
    crate::confirmation_repository::check_active(&c.record, &p.clock)?;
    if c.record.status != ConfirmationStatus::Confirmed {
        return Err(StorageError::ConfirmationRequired);
    }
    // Last trusted DB sample after every async source/permission check. Synchronous
    // resolver and all absolute deadlines follow with no further awaits.
    let final_now = time(tx, p.test_clock.as_ref(), &p.clock).await?;
    if final_now >= c.record.expires_at.timestamp_millis()
        || final_now >= r.try_get::<i64, _>("grant_expires_ms")?
    {
        return Err(StorageError::AuthExpired);
    }
    if let Some(row) = u.consumed.as_ref().or(u.final_active.as_ref()) {
        row.active_time(final_now)?;
        let grant: String = r.try_get("grant_ref")?;
        if row.authentication.grant_ref().map(AuthGrantRef::as_str) != Some(grant.as_str()) {
            return Err(StorageError::AuthBindingMismatch);
        }
    }
    u.session_guard()?;
    Ok(())
}

#[cfg(test)]
mod tests;
