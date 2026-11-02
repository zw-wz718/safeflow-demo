//! Closed self-freeze assembly. No callbacks, caller results or free command DTOs.
use super::*;
use safeflow_card_facts::operation_records::OriginalLockEffect;
use safeflow_card_postgres_store::{
    backend::CardFreezeBackend,
    freeze::FreezeSaveDisposition,
    freeze_completion::{FreezeReadbackRequest, FreezeReadbackResolution},
    freeze_preparation::PreparedFreeze,
};
mod deployment;
mod read_authority;
pub use deployment::CardFinalDeployment;

pub(crate) struct CardCompletion {
    pub(super) facts: serde_json::Value,
    attempt: i64,
    token: uuid::Uuid,
}
#[derive(Clone)]
pub(super) struct Admission {
    number: i64,
    token: uuid::Uuid,
}
#[derive(Serialize)]
pub struct CardOriginalOutcome {
    pub operation_id: OperationId,
    pub state: String,
    pub event_id: Option<String>,
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    pub original_effect: Option<String>,
    pub original_lock_active: Option<bool>,
    pub account_outgoing_restricted: Option<bool>,
    pub business_executed: bool,
}
pub(crate) fn checked_details(v: &serde_json::Value) -> Result<(), StorageError> {
    if v == &details("CREATED") || v == &details("REUSED_ACTIVE") {
        Ok(())
    } else {
        Err(StorageError::CorruptRecord)
    }
}
fn details(effect: &str) -> serde_json::Value {
    json!({"schema_version":1,"mapping":"card-freeze-operation-v1","action":"FREEZE_CARD","result":"OPERATION_RECORDED","original_effect":effect})
}
pub(super) async fn deployment_guard(
    tx: &mut Transaction<'_, Postgres>,
    required: bool,
) -> Result<(), StorageError> {
    let r=sqlx::query("SELECT d.*, (SELECT oid FROM pg_database WHERE datname=current_database()) AS db_oid, (SELECT oid FROM pg_namespace WHERE nspname=current_schema()) AS ns_oid FROM public_card_final_deployment_v1 d WHERE singleton=1 FOR SHARE").fetch_optional(&mut **tx).await?;
    let Some(r) = r else {
        return if required {
            Err(StorageError::DependencyUnavailable)
        } else {
            Ok(())
        };
    };
    if !r.try_get::<bool, _>("active")?
        || r.try_get::<String, _>("reader_version")? != "card-final-v1"
        || r.try_get::<String, _>("source_id")? != UNIFIED_FICTIONAL_SOURCE
        || r.try_get::<String, _>("implementation")? != UNIFIED_FICTIONAL_IMPLEMENTATION
        || r.try_get::<sqlx::postgres::types::Oid, _>("schema_oid")?
            != r.try_get::<sqlx::postgres::types::Oid, _>("ns_oid")?
        || r.try_get::<sqlx::postgres::types::Oid, _>("database_oid")?
            != r.try_get::<sqlx::postgres::types::Oid, _>("db_oid")?
    {
        return Err(StorageError::DecisionAssemblyChanged);
    }
    let object_digest: String = sqlx::query_scalar("SELECT public_card_final_objects_v1()")
        .fetch_one(&mut **tx)
        .await?;
    let expected: String = sqlx::query_scalar("SELECT public_card_final_expected_digest_v1(deployment_id,object_digest) FROM public_card_final_deployment_v1 WHERE singleton=1")
        .fetch_one(&mut **tx).await?;
    if object_digest != expected {
        return Err(StorageError::DecisionAssemblyChanged);
    }
    if required {
        let yes:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_final_logins_v1 v JOIN pg_roles r ON r.rolname=v.role_name WHERE v.role_name=session_user::text AND v.reader_version='card-final-v1' AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolbypassrls) AND NOT has_schema_privilege(session_user,current_schema(),'CREATE') AND NOT has_database_privilege(session_user,current_database(),'TEMP') AND NOT EXISTS(SELECT 1 FROM pg_class WHERE relnamespace=pg_my_temp_schema())").fetch_one(&mut **tx).await?;
        if !yes {
            return Err(StorageError::FormalAuthUnavailable);
        }
    }
    Ok(())
}
pub(super) async fn journal_lock(
    tx: &mut Transaction<'_, Postgres>,
    op: &OperationId,
) -> Result<(), StorageError> {
    sqlx::query("SELECT operation_id FROM public_card_attempts_v1 WHERE operation_id=$1 ORDER BY attempt_no FOR UPDATE").bind(op.as_str()).fetch_all(&mut **tx).await?;
    Ok(())
}
async fn readonly_deployment_guard(tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
    // No row locks and no active-writer requirement: withdrawal preserves the
    // current reader, but an absent/replaced database is never an empty result.
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE singleton=1 AND reader_version='card-final-v1' AND source_id=$1 AND implementation=$2 AND schema_oid=(SELECT oid FROM pg_namespace WHERE nspname=current_schema()) AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND public_card_final_expected_digest_v1(deployment_id,object_digest)=public_card_final_objects_v1())").bind(UNIFIED_FICTIONAL_SOURCE).bind(UNIFIED_FICTIONAL_IMPLEMENTATION).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(StorageError::DecisionAssemblyChanged);
    }
    Ok(())
}
impl CardSafetyService<'_> {
    /// Execute this original once. Retry is explicit and only after a confirmed
    /// whole-transaction rollback. Unknown admissions are never taken over.
    pub async fn execute_freeze(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CardOriginalOutcome, StorageError> {
        self.execute_freeze_command(s, op, false).await
    }
    pub async fn retry_rolled_back_freeze(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CardOriginalOutcome, StorageError> {
        self.execute_freeze_command(s, op, true).await
    }
    async fn execute_freeze_command(
        &self,
        s: &SessionRef,
        op: &OperationId,
        retry: bool,
    ) -> Result<CardOriginalOutcome, StorageError> {
        let history = self.query_freeze_original(s, op).await?;
        if history.state != "PREPARED_ONLY" && !(retry && history.state == "ROLLBACK_VERIFIED") {
            return Ok(history);
        }
        let admission = match self.session_checked(s, self.admit(s, op, retry)).await {
            Ok(a) => a,
            Err(e) => {
                let now = self.query_freeze_original(s, op).await?;
                if now.state == "RECORDED" || now.state == "UNKNOWN" || now.state == "NEEDS_REVIEW"
                {
                    return Ok(now);
                }
                return Err(e);
            }
        };
        let Some(admission) = admission else {
            return self.query_freeze_original(s, op).await;
        };
        self.execute_admitted(s, op, &admission).await
    }
    async fn admit(
        &self,
        s: &SessionRef,
        op: &OperationId,
        retry: bool,
    ) -> Result<Option<Admission>, StorageError> {
        let (mut tx, b, o) = self.locked(s, op).await?;
        deployment_guard(&mut tx, true).await?;
        let legacy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM public_card_legacy_operations_v1 WHERE operation_id=$1)",
        )
        .bind(op.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if legacy {
            return Err(StorageError::ReconcileRequired);
        }
        let previous:Option<(i64,String)>=sqlx::query_as("SELECT attempt_no,state FROM public_card_attempts_v1 WHERE operation_id=$1 ORDER BY attempt_no DESC LIMIT 1").bind(op.as_str()).fetch_optional(&mut *tx).await?;
        let n = match previous {
            None => 1,
            Some((n, state)) if retry && state == "ROLLBACK_VERIFIED" => {
                n.checked_add(1).ok_or(StorageError::RevisionConflict)?
            }
            _ => {
                tx.rollback().await?;
                return Ok(None);
            }
        };
        // HTTP actions construct fresh services. Sample the trusted primary
        // after the waits before reading any absolute confirmation deadline.
        time(&mut tx, self.test_clock.as_ref(), &self.clock).await?;
        self.confirmed(&o)?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=$1 AND status='ACTIVE' AND execution_scope='ISOLATED_CARD' AND card_attempt_no=$2)").bind(op.as_str()).bind(n-1).fetch_one(&mut *tx).await?;
        if !active {
            return Err(StorageError::PermitNotActive);
        }
        let a = Admission {
            number: n,
            token: uuid::Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO public_card_attempts_v1(operation_id,attempt_no,token,state,permit_id) SELECT $1,$2,$3::uuid,'ADMITTED',permit_id FROM execution_permits WHERE operation_id=$1 AND card_attempt_no=$2-1 AND status='ACTIVE'").bind(op.as_str()).bind(n).bind(a.token.to_string()).execute(&mut *tx).await?;
        self.guard(s, &b)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        self.guard(s, &b)?;
        Ok(Some(a))
    }
    async fn execute_admitted(
        &self,
        s: &SessionRef,
        op: &OperationId,
        a: &Admission,
    ) -> Result<CardOriginalOutcome, StorageError> {
        let prepared = async {
            let (mut u, o) = self.unit(s, op).await?;
            u.card
                .as_mut()
                .ok_or(StorageError::FormalAuthUnavailable)?
                .admission = Some(a.clone());
            let run = self.compose(&mut u, &o, a).await;
            if let Err(e) = run {
                u.rollback().await;
                return Err(e);
            }
            Ok(u)
        };
        // guard only the pre-commit future; never cancel a submitted COMMIT.
        let u = self.session_checked(s, prepared).await?;
        u.finish(true).await?;
        self.query_freeze_original(s, op).await
    }
    async fn compose(
        &self,
        u: &mut PermitTransaction,
        o: &Original,
        a: &Admission,
    ) -> Result<(), StorageError> {
        let op = o.confirmation.binding.operation_id.clone();
        let tx = u.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        deployment_guard(tx, true).await?;
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_attempts_v1 WHERE operation_id=$1 AND attempt_no=$2 AND token=$3::uuid AND state='ADMITTED') AND NOT EXISTS(SELECT 1 FROM public_card_results_v1 WHERE operation_id=$1)").bind(op.as_str()).bind(a.number).bind(a.token.to_string()).fetch_one(&mut **tx).await?;
        if !valid {
            return Err(StorageError::ReconcileRequired);
        }
        let mut read = self.store.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
            .execute(&mut *read)
            .await?;
        let preview = PreparedFreeze::read_on_connection(
            &mut read,
            u.identity.user_id.as_str(),
            o.input.account_id.as_str(),
            &o.input.command(),
        )
        .await
        .map_err(|_| StorageError::FactsConflict)?;
        read.commit().await?;
        let binding = PermitBinding {
            snapshot: o.confirmation.binding.clone(),
            fingerprint: compute_operation_fingerprint(&payload(o)?)
                .map_err(|_| StorageError::PermitBindingMismatch)?,
        };
        let pid:String=sqlx::query_scalar("SELECT permit_id FROM public_card_attempts_v1 WHERE operation_id=$1 ORDER BY attempt_no DESC LIMIT 1").bind(op.as_str()).fetch_one(&mut **tx).await?;
        guard_unit(u).await?;
        // Private consume: failure rollback is owned by this closed composer.
        u.consume_inner(
            &PermitId::parse(pid).map_err(|_| StorageError::CorruptRecord)?,
            &binding,
        )
        .await?;
        let request = ExecutionRequest {
            idempotency_key: IdempotencyKey::parse(&o.input.canonical_key)
                .map_err(|_| StorageError::CorruptRecord)?,
            business_operation_ref: BusinessOperationRef::parse(&o.input.member_operation_id)
                .map_err(|_| StorageError::CorruptRecord)?,
            business_version: 1,
            binding,
            payload: payload(o)?,
        };
        let submission = u.record_submission_inner(&request).await?;
        let tx = u.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
        let pending = {
            let mut backend = CardFreezeBackend::borrow(
                tx,
                u.identity.user_id.as_str(),
                o.input.account_id.as_str(),
            )
            .await
            .map_err(|_| StorageError::FactsConflict)?;
            backend
                .execute_prepared_freeze_handoff(&preview, &o.input.command())
                .await
                .map_err(|_| StorageError::FactsConflict)?
        };
        let pending = pending.pending_execution();
        if pending.disposition() != FreezeSaveDisposition::PersistedInCallerTransaction
            || !pending.original_lock_active_in_transaction()
        {
            return Err(StorageError::ReconcileRequired);
        }
        let receipt = pending.historical_receipt();
        let effect = match receipt.original_effect {
            OriginalLockEffect::Created => "CREATED",
            OriginalLockEffect::ReusedActive => "REUSED_ACTIVE",
        };
        let facts = facts_projection(pending.current_facts_in_transaction());
        let record = o.input.operation_record_id.clone();
        let lock = receipt.lock_id.clone();
        let reason = o.input.reason_history_id.clone();
        if receipt.operation_id != o.input.member_operation_id
            || receipt.operation_record_id != record
            || receipt.reason_history_id != reason
            || receipt.card_id != o.input.card_id
            || receipt.source != LockSource::UserSelf
            || effect == "CREATED" && lock != o.input.lock_id
        {
            return Err(StorageError::CorruptRecord);
        }
        let mut task = u.task.clone();
        let mut operation = submission.operation.clone();
        operation.state = OperationState::Succeeded;
        operation.revision = 3;
        task.state = TaskState::Completed;
        task.revision = 3;
        task.completed_nodes = vec![operation.node_id.clone()];
        task.checkpoint = RecoveryPoint::Finished;
        validate_checkpoint(&task, &[operation.clone()])?;
        sqlx::query("UPDATE operations SET state='SUCCEEDED',revision=3 WHERE operation_id=$1 AND state='SUBMITTED' AND revision=2").bind(op.as_str()).execute(&mut **tx).await?;
        crate::repository::update_task(tx, &task, 2).await?;
        sqlx::query("WITH v AS (SELECT $1::text AS operation_id,$2::text AS member_operation_id,$3::text AS canonical_key,$4::text AS record_id,$5::text AS lock_id,$6::text AS reason_history_id,$7::text AS original_effect,$8::bigint AS attempt_no,$9::bigint AS business_txid,$10::jsonb AS post_facts,$11::jsonb AS first_submission) INSERT INTO public_card_results_v1 SELECT v.*,encode(sha256(convert_to(to_jsonb(v)::text,'UTF8')),'hex') FROM v").bind(op.as_str()).bind(&o.input.member_operation_id).bind(&o.input.canonical_key).bind(&record).bind(&lock).bind(&reason).bind(effect).bind(a.number).bind(u.transaction_id).bind(Json(&facts)).bind(Json(&submission.operation)).execute(&mut **tx).await?;
        let event = id("card_audit_event");
        let _: (String,chrono::DateTime<chrono::Utc>)=sqlx::query_as("INSERT INTO audit_events(event_id,task_id,operation_id,kind,task_revision,operation_revision,details) VALUES($1,$2,$3,'CARD_FREEZE_OPERATION_RECORDED',3,3,$4) RETURNING event_id,occurred_at").bind(&event).bind(task.task_id.as_str()).bind(op.as_str()).bind(Json(details(effect))).fetch_one(&mut **tx).await?;
        sqlx::query("INSERT INTO public_card_audit_links_v1(operation_id,event_id,record_id,request_digest,result_digest) SELECT $1,$2,$3,$4,result_digest FROM public_card_results_v1 WHERE operation_id=$1").bind(op.as_str()).bind(&event).bind(&record).bind(hash(&json!(o.input))?.as_str()).execute(&mut **tx).await?;
        sqlx::query("UPDATE public_card_attempts_v1 SET state='BUSINESS_COMMITTED',business_txid=$4 WHERE operation_id=$1 AND attempt_no=$2 AND token=$3::uuid AND state='ADMITTED'").bind(op.as_str()).bind(a.number).bind(a.token.to_string()).bind(u.transaction_id).execute(&mut **tx).await?;
        u.card
            .as_mut()
            .ok_or(StorageError::FormalAuthUnavailable)?
            .completion = Some(CardCompletion {
            facts,
            attempt: a.number,
            token: a.token,
        });
        Ok(())
    }
    pub async fn query_freeze_original(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CardOriginalOutcome, StorageError> {
        self.session_checked(s, self.query_final_checked(s, op))
            .await
    }
    async fn query_final_checked(
        &self,
        s: &SessionRef,
        op: &OperationId,
    ) -> Result<CardOriginalOutcome, StorageError> {
        let b = self.store.identity(s)?;
        let mut tx = self.store.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
            .execute(&mut *tx)
            .await?;
        readonly_deployment_guard(&mut tx).await?;
        let r = sqlx::query(
            "SELECT * FROM public_card_safety_requests_v1 WHERE operation_id=$1 AND owner_id=$2",
        )
        .bind(op.as_str())
        .bind(b.user_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StorageError::NotFound)?;
        let input: Input = r.try_get::<Json<Input>, _>("input")?.0;
        if hash(&json!(input))?.as_str() != r.try_get::<String, _>("input_digest")? {
            return Err(StorageError::CorruptRecord);
        }
        self.history_authorized(&mut tx, s, &b, &input).await?;
        let task_id: TaskId = decode(&r, "task_id")?;
        let confirmation = crate::confirmation_repository::read_historical_confirmation(
            &mut tx,
            &task_id,
            &decode(&r, "confirmation_ref")?,
        )
        .await?;
        if input.input_version != 1
            || input.action != "FREEZE_CARD"
            || input.mode != "ISOLATED_FICTIONAL_SELF_CARD_V1"
            || input.source_revision != UNIFIED_FICTIONAL_IMPLEMENTATION
            || input.account_id.as_str() != r.try_get::<String, _>("account_id")?
            || input.card_id != r.try_get::<String, _>("card_id")?
            || confirmation.record.binding.user_id != b.user_id
            || confirmation.record.binding.operation_id != *op
            || confirmation.record.binding.task_id != task_id
            || confirmation.record.binding.arguments_hash != hash(&json!(input))?
        {
            return Err(StorageError::CorruptRecord);
        }
        let mut out = CardOriginalOutcome {
            operation_id: op.clone(),
            state: "PREPARED_ONLY".into(),
            event_id: None,
            occurred_at: None,
            original_effect: None,
            original_lock_active: None,
            account_outgoing_restricted: None,
            business_executed: false,
        };
        let legacy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM public_card_legacy_operations_v1 WHERE operation_id=$1)",
        )
        .bind(op.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let attempt:Option<String>=sqlx::query_scalar("SELECT state FROM public_card_attempts_v1 WHERE operation_id=$1 ORDER BY attempt_no DESC LIMIT 1").bind(op.as_str()).fetch_optional(&mut *tx).await?;
        if legacy {
            out.state = "LEGACY_UNLINKED".into();
        } else if let Some(state) = attempt {
            if state == "BUSINESS_COMMITTED" {
                let checked = sqlx::query("SELECT public_card_final_assert_v1($1)")
                    .bind(op.as_str())
                    .execute(&mut *tx)
                    .await;
                if checked.is_err() {
                    return Err(StorageError::ReconcileRequired);
                }
                let result = FreezeReadbackRequest::from_command(
                    b.user_id.as_str(),
                    input.account_id.as_str(),
                    &input.command(),
                )
                .map_err(|_| StorageError::CorruptRecord)?
                .query(&mut tx)
                .await
                .map_err(|_| StorageError::ReconcileRequired)?;
                match result {
                    FreezeReadbackResolution::Recorded(v) => {
                        let ev=sqlx::query("SELECT a.event_id,a.occurred_at,a.details->>'original_effect' AS effect FROM public_card_audit_links_v1 l JOIN audit_events a USING(event_id) WHERE l.operation_id=$1").bind(op.as_str()).fetch_one(&mut *tx).await?;
                        out.state = "RECORDED".into();
                        out.event_id = Some(ev.try_get("event_id")?);
                        out.occurred_at = Some(ev.try_get("occurred_at")?);
                        out.original_effect = Some(ev.try_get("effect")?);
                        out.original_lock_active = Some(v.original_lock_active());
                        out.account_outgoing_restricted =
                            Some(v.current_facts().outgoing_restricted());
                        out.business_executed = true;
                    }
                    FreezeReadbackResolution::NeedsReview => {
                        out.state = "NEEDS_REVIEW".into();
                    }
                }
            } else {
                out.state = if state == "ROLLBACK_VERIFIED" {
                    "ROLLBACK_VERIFIED"
                } else {
                    "UNKNOWN"
                }
                .into();
            }
        }
        // The final database authority decision uses ONE statement snapshot,
        // after all history/member awaits. Do not split it into sequential checks.
        self.final_read_authorized(&mut tx, s, &b, &input).await?;
        self.guard(s, &b)?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        self.guard(s, &b)?;
        Ok(out)
    }
    async fn history_authorized(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        b: &SessionBinding,
        input: &Input,
    ) -> Result<(), StorageError> {
        self.source.check_readonly(tx).await?;
        let authorized:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfer_accounts_v1 WHERE owner_id=$1 AND account_id=$2) AND NOT EXISTS(SELECT 1 FROM card_safety_account_fences_v1 WHERE owner_id=$1 AND account_id=$2 AND revoked) AND NOT EXISTS(SELECT 1 FROM card_safety_session_fences_v1 WHERE session_digest=$3 AND (owner_id<>$1 OR binding_version<>$4 OR revoked))").bind(b.user_id.as_str()).bind(input.account_id.as_str()).bind(digest(s)?).bind(db_version(b.binding_version)?).fetch_one(&mut **tx).await?;
        let adoption:Option<serde_json::Value>=sqlx::query_scalar("SELECT to_jsonb(a) FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1) a").bind(input.account_id.as_str()).fetch_optional(&mut **tx).await?;
        if !authorized || adoption.as_ref() != Some(&input.adoption) {
            return Err(StorageError::NotFound);
        }
        self.guard(s, b)
    }
}
pub(in crate::permit) async fn verify_completion(
    u: &mut PermitTransaction,
    p: &PermitRow,
) -> Result<(), StorageError> {
    let c = u
        .card
        .as_ref()
        .and_then(|p| p.completion.as_ref())
        .ok_or(StorageError::FormalAuthUnavailable)?;
    let op = &p.binding.snapshot.operation_id;
    let tx = u.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
    sqlx::query("SELECT public_card_final_assert_v1($1)")
        .bind(op.as_str())
        .execute(&mut **tx)
        .await?;
    let stored: Json<serde_json::Value> =
        sqlx::query_scalar("SELECT post_facts FROM public_card_results_v1 WHERE operation_id=$1")
            .bind(op.as_str())
            .fetch_one(&mut **tx)
            .await?;
    if stored.0 != c.facts {
        return Err(StorageError::FactsConflict);
    }
    let task = lock_task(tx, &u.identity.user_id, &u.task.task_id).await?;
    let ops = operations(tx, &u.task.task_id).await?;
    if task.plan != u.task.plan
        || task.completed_nodes != vec![p.binding.snapshot.node_id.clone()]
        || task.checkpoint != RecoveryPoint::Finished
        || task.revision != 3
        || task.state != TaskState::Completed
        || ops.len() != 1
        || ops[0].arguments_hash != p.binding.snapshot.arguments_hash
        || ops[0].revision != 3
        || ops[0].state != OperationState::Succeeded
    {
        return Err(StorageError::PermitBindingMismatch);
    }
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_attempts_v1 WHERE operation_id=$1 AND attempt_no=$2 AND token=$3::uuid AND state='BUSINESS_COMMITTED' AND business_txid=$4)").bind(op.as_str()).bind(c.attempt).bind(c.token.to_string()).bind(u.transaction_id).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(StorageError::ReconcileRequired);
    }
    Ok(())
}
pub(crate) async fn finish_checks(u: &mut PermitTransaction) -> Result<(), StorageError> {
    if u.card
        .as_ref()
        .and_then(|p| p.completion.as_ref())
        .is_none()
    {
        return Err(StorageError::FormalAuthUnavailable);
    }
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut **u.tx.as_mut().ok_or(StorageError::TransactionAborted)?)
        .await?;
    deployment_guard(u.tx.as_mut().ok_or(StorageError::TransactionAborted)?, true).await?;
    guard_final_unit(u, true).await
}
pub(crate) async fn seal_unit_rollback(u: &mut PermitTransaction) {
    // Called ONLY by PermitTransaction after server-confirmed whole rollback
    // and before COMMIT was sent. Drop/disconnect/cancellation never calls it.
    let Some(p) = u.card.as_ref() else {
        return;
    };
    let Some(a) = p.admission.as_ref() else {
        return;
    };
    let persist=async {
        let mut tx=u.store.repository.pool.begin().await?;
        // Same persistent root order as execution, even after authorization
        // withdrawal. This stores a server proof, never a new business right.
        sqlx::query("SELECT singleton FROM public_card_final_deployment_v1 WHERE singleton=1 FOR SHARE").fetch_one(&mut *tx).await?;
        let q=sqlx::query("SELECT * FROM public_card_safety_requests_v1 WHERE operation_id=$1").bind(p.operation.as_str()).fetch_one(&mut *tx).await?;
        sqlx::query("SELECT session_digest FROM card_safety_session_fences_v1 WHERE session_digest=$1 FOR UPDATE").bind(q.try_get::<String,_>("session_digest")?).fetch_one(&mut *tx).await?;
        sqlx::query("SELECT epoch FROM card_safety_account_fences_v1 WHERE owner_id=$1 AND account_id=$2 FOR UPDATE").bind(u.identity.user_id.as_str()).bind(p.input.account_id.as_str()).fetch_one(&mut *tx).await?;
        crate::daily_usage::protect_owner(&mut tx,&u.identity.user_id).await?;
        lock_task(&mut tx,&u.identity.user_id,&u.task.task_id).await?;
        journal_lock(&mut tx,&p.operation).await?;
        let absent:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM public_card_results_v1 WHERE operation_id=$1) AND NOT EXISTS(SELECT 1 FROM public_card_audit_links_v1 WHERE operation_id=$1) AND NOT EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=$1) AND NOT EXISTS(SELECT 1 FROM audit_events WHERE operation_id=$1 AND kind='CARD_FREEZE_OPERATION_RECORDED') AND NOT EXISTS(SELECT 1 FROM card_freeze_operations_v1 WHERE operation_id=$2 OR original_key=$3 OR record_id=$4)").bind(p.operation.as_str()).bind(&p.input.member_operation_id).bind(&p.input.canonical_key).bind(&p.input.operation_record_id).fetch_one(&mut *tx).await?;
        if !absent{return Err(StorageError::ReconcileRequired);}
        let original_permit:Option<String>=sqlx::query_scalar("SELECT p.permit_id FROM execution_permits p JOIN public_card_attempts_v1 a ON a.permit_id=p.permit_id WHERE a.operation_id=$1 AND a.attempt_no=$2 AND a.token=$3::uuid AND a.state='ADMITTED' AND p.operation_id=a.operation_id AND p.owner_id=$4 AND p.execution_scope='ISOLATED_CARD' AND p.card_attempt_no=a.attempt_no-1 AND p.status='ACTIVE' FOR UPDATE OF p").bind(p.operation.as_str()).bind(a.number).bind(a.token.to_string()).bind(u.identity.user_id.as_str()).fetch_optional(&mut *tx).await?;
        let permit_id=original_permit.ok_or(StorageError::ReconcileRequired)?;
        let updated=sqlx::query("UPDATE public_card_attempts_v1 SET state='ROLLBACK_VERIFIED',rollback_txid=$4,rollback_confirmed_at=clock_timestamp() WHERE operation_id=$1 AND attempt_no=$2 AND token=$3::uuid AND state='ADMITTED'").bind(p.operation.as_str()).bind(a.number).bind(a.token.to_string()).bind(u.transaction_id).execute(&mut *tx).await?;
        if updated.rows_affected()!=1{return Err(StorageError::ReconcileRequired);}
        let revoked=sqlx::query("UPDATE execution_permits SET status='REVOKED' WHERE permit_id=$1 AND status='ACTIVE'").bind(permit_id).execute(&mut *tx).await?;
        if revoked.rows_affected()!=1{return Err(StorageError::ReconcileRequired);}
        tx.commit().await?;Ok::<_,StorageError>(())
    }.await;
    // Failure to persist proof remains UNKNOWN, never inferred rolled back.
    let _ = persist;
}

#[cfg(test)]
mod tests;
