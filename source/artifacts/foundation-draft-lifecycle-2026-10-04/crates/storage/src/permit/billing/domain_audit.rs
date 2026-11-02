//! Candidate, private same-connection proof. Never constructs member private types.
use super::*;
use crate::billing_report::{original_source_identity, BILLING_MEMBER_REVISION};
use sqlx::postgres::PgRow;

const MAPPING: &str = "billing-feedback-saved-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BillingDomainAuditStatus {
    NotActivated,
    PublicHistoryOnlyV1,
    LegacyWithoutDomainEvent,
    VerifiedV1,
}

/// Operator-only inputs. No Deserialize/HTTP configuration or auto registration.
pub struct BillingDomainAuditInstallation {
    namespace: String,
    primary_identity: String,
    source_id: String,
    source_revision: String,
}
impl BillingDomainAuditInstallation {
    pub fn for_reviewed_backend(
        namespace: &str,
        primary_identity: &str,
        source_id: &str,
        source_revision: &str,
    ) -> Result<Self, StorageError> {
        for id in [namespace, primary_identity, source_id, source_revision] {
            valid_id(id)?;
        }
        Ok(Self {
            namespace: namespace.into(),
            primary_identity: primary_identity.into(),
            source_id: source_id.into(),
            source_revision: source_revision.into(),
        })
    }
}
pub(super) async fn writer_lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(1680000028,hashtext(current_schema()))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
impl PgRepository {
    /// Explicit cutover only, never called by startup or HTTP. Operator first stops
    /// related writers. Exclusive advisory and table locks then drain SQL writers
    /// and enumerate exact original O's in this transaction (no historical backfill).
    pub async fn install_billing_domain_audit_for_reviewed_backend(
        &self,
        config: &BillingDomainAuditInstallation,
    ) -> Result<String, StorageError> {
        if self.namespace != config.namespace {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(1680000028,hashtext(current_schema()))")
            .execute(&mut *tx)
            .await?;
        sqlx::query("LOCK TABLE public_billing_feedback_results_v1,billing_feedback_v1,billing_report_bindings_v1,billing_feedback_events_v1 IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx).await?;
        let exists: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_billing_audit_sources_v1 WHERE source_id=$1 AND source_revision=$2)")
            .bind(&config.source_id).bind(&config.source_revision).fetch_one(&mut *tx).await?;
        if exists {
            return Err(StorageError::IdConflict);
        }
        sqlx::query(
            "SELECT set_config('safeflow.billing_audit_install',txid_current()::text,true)",
        )
        .execute(&mut *tx)
        .await?;
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO public_billing_audit_sources_v1(source_binding_id,primary_identity,database_name,database_oid,server_port,namespace,member_namespace,member_revision,source_id,source_revision,mapping_version) SELECT $1::uuid,$2,current_database(),oid,current_setting('port')::integer,current_schema(),'billing_feedback_v1',$3,$4,$5,$6 FROM pg_database WHERE datname=current_database()")
            .bind(&id).bind(&config.primary_identity).bind(BILLING_MEMBER_REVISION)
            .bind(&config.source_id).bind(&config.source_revision).bind(MAPPING).execute(&mut *tx).await?;
        let rows=sqlx::query("SELECT r.operation_id,r.owner_id FROM public_billing_feedback_results_v1 r JOIN public_billing_reports_v1 b ON b.owner_id=r.owner_id AND b.report_id=r.report_id WHERE b.archive->>'source_id'=$1 AND b.archive->>'source_revision'=$2 AND b.archive->>'member_revision'=$3 ORDER BY r.operation_id")
            .bind(&config.source_id).bind(&config.source_revision).bind(BILLING_MEMBER_REVISION).fetch_all(&mut *tx).await?;
        for row in &rows {
            let operation: String = row.try_get("operation_id")?;
            let stored =
                sqlx::query("SELECT * FROM public_execution_requests WHERE operation_id=$1")
                    .bind(&operation)
                    .fetch_one(&mut *tx)
                    .await?;
            let request = crate::execution_request::stored_row(&stored)?.request;
            let owner = UserId::parse(row.try_get::<String, _>("owner_id")?)
                .map_err(|_| StorageError::CorruptRecord)?;
            verify_result(&mut tx, &owner, &request).await?;
            sqlx::query("INSERT INTO public_billing_audit_legacy_v1(operation_id,source_binding_id,owner_id,request_digest) VALUES($1,$2::uuid,$3,$4)")
                .bind(&operation).bind(&id).bind(owner.as_str()).bind(request_digest(&request)?.as_str()).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO public_billing_audit_activations_v1(source_binding_id,legacy_count) VALUES($1::uuid,$2)")
            .bind(&id).bind(rows.len() as i64).execute(&mut *tx).await?;
        for row in rows {
            sqlx::query("SELECT verify_billing_audit_v1($1)")
                .bind(row.try_get::<String, _>("operation_id")?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(id)
    }
}

struct VerifiedBillingFacts {
    source: Option<String>,
    member_event: i64,
    digest: ArgumentsHash,
}

/// Complete fact comparison, including order-sensitive original Request, header,
/// all payload fields and the genuine archive rebuild. This uses ONLY original tx.
async fn checked_facts(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    request: &ExecutionRequest,
) -> Result<VerifiedBillingFacts, StorageError> {
    let input = input(request)?;
    if input.input_version != 1 || request.payload.normalized_arguments != json!(input) {
        return Err(StorageError::CorruptRecord);
    }
    let operation = &request.binding.snapshot.operation_id;
    let (archive, accounts) = load_original(tx, owner, &input.report_id).await?;
    let source_identity = original_source_identity(&archive)?;
    let scope =
        TrustedScope::from_verified_context(owner.as_str(), accounts.iter().map(String::as_str))
            .map_err(|_| StorageError::CorruptRecord)?;
    let restored = restore_original(archive.clone(), &scope)?;
    if input.original_digest != restored.archive_digest
        || request.binding.snapshot.user_id != *owner
    {
        return Err(StorageError::CorruptRecord);
    }
    // Public member constructor validates complete report evidence, not display JSON.
    prepare_feedback(
        &scope,
        &restored.report,
        operation.as_str(),
        &input.intent(),
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    let original = restored
        .report
        .evidence
        .as_ref()
        .and_then(|rows| rows.iter().find(|r| r.reference == input.transaction))
        .ok_or(StorageError::CorruptRecord)?;
    let row=sqlx::query(r#"SELECT r.*,f.owner_user_id,f.report_id AS member_report,f.source_id,f.account_id,f.transaction_id,f.payload,h.header,e.event_id,e.event_kind,
      to_char(e.recorded_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS member_time
      FROM public_billing_feedback_results_v1 r
      JOIN billing_feedback_v1 f ON f.public_operation_id=r.operation_id
      JOIN billing_report_bindings_v1 h ON h.owner_user_id=f.owner_user_id AND h.report_id=f.report_id
      JOIN billing_feedback_events_v1 e ON e.public_operation_id=f.public_operation_id
      WHERE r.operation_id=$1 AND r.owner_id=$2"#)
        .bind(operation.as_str()).bind(owner.as_str()).fetch_optional(&mut **tx).await?
        .ok_or(StorageError::ReconcileRequired)?;
    let payload = json!({"public_operation_id":operation,"owner_user_id":owner,"report_id":input.report_id,
      "rule_version":restored.report.rule_version,"snapshot_id":restored.report.read.snapshot_id,
      "calculated_at":restored.report.metadata.calculated_at,"request":restored.request,
      "reference":original.reference,"transaction":original.transaction,"choice":input.choice});
    let header = json!({"owner_user_id":owner,"report_id":input.report_id,"request":restored.request,
      "source_id":original.reference.source_id,"snapshot_id":restored.report.read.snapshot_id,
      "rule_version":restored.report.rule_version,"calculated_at":restored.report.metadata.calculated_at});
    let event = row.try_get::<i64, _>("event_id")?;
    let time = row.try_get::<String, _>("member_time")?;
    let staged = json!({"state":"STAGED_AWAITING_PUBLIC_COMMIT","public_operation_id":operation,"replayed":false,
      "event":{"event_id":event,"event_kind":"BILLING_FEEDBACK_RECORDED_LOCAL_V1","recorded_at":time}});
    for (column, expected) in [
        ("report_id", input.report_id.as_str()),
        ("member_report", input.report_id.as_str()),
        ("owner_user_id", owner.as_str()),
        ("source_id", input.transaction.source_id.as_str()),
        ("account_id", input.transaction.account_id.as_str()),
        ("transaction_id", input.transaction.transaction_id.as_str()),
    ] {
        if row.try_get::<String, _>(column)? != expected {
            return Err(StorageError::CorruptRecord);
        }
    }
    if row.try_get::<Value, _>("payload")? != payload
        || row.try_get::<Value, _>("header")? != header
        || row.try_get::<Value, _>("staged")? != staged
        || row.try_get::<String, _>("request_digest")? != request_digest(request)?.as_str()
        || event <= 0
        || chrono::DateTime::parse_from_rfc3339(&time).is_err()
        || row.try_get::<String, _>("event_kind")? != "BILLING_FEEDBACK_RECORDED_LOCAL_V1"
    {
        return Err(StorageError::CorruptRecord);
    }
    let stored = sqlx::query("SELECT * FROM public_execution_requests WHERE operation_id=$1")
        .bind(operation.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let stored = crate::execution_request::stored_row(&stored)?;
    if stored.request != *request || stored.first_submission.is_none() {
        return Err(StorageError::CorruptRecord);
    }
    let confirmation_ref = ConfirmationRef::parse(row.try_get::<String, _>("confirmation_ref")?)
        .map_err(|_| StorageError::CorruptRecord)?;
    let confirmation = crate::confirmation_repository::read_historical_confirmation(
        tx,
        &request.binding.snapshot.task_id,
        &confirmation_ref,
    )
    .await?;
    let permits =
        sqlx::query("SELECT * FROM execution_permits WHERE operation_id=$1 AND status='CONSUMED'")
            .bind(operation.as_str())
            .fetch_all(&mut **tx)
            .await?;
    if permits.len() != 1 {
        return Err(StorageError::CorruptRecord);
    }
    let permit = permit_row(&permits[0])?;
    if confirmation.record.binding != request.binding.snapshot
        || confirmation.record.confirmed_at.is_none()
        || confirmation.session_digest != row.try_get::<String, _>("session_digest")?
        || db_version(confirmation.session_binding_version)?
            != row.try_get::<i64, _>("binding_version")?
        || permit.binding != request.binding
        || permit.payload != request.payload
        || permit.confirmation_ref != confirmation_ref
        || permit.session_digest != confirmation.session_digest
        || permit.session_version != confirmation.session_binding_version
        || permit.execution_scope != Some(VerifiedPermitScope::Common)
    {
        return Err(StorageError::CorruptRecord);
    }
    let source=sqlx::query("SELECT s.*,s.source_binding_id::text AS binding_text FROM public_billing_audit_sources_v1 s JOIN public_billing_audit_activations_v1 a USING(source_binding_id) WHERE source_id=$1 AND source_revision=$2 AND member_revision=$3")
        .bind(&source_identity.0).bind(&source_identity.1).bind(&source_identity.2).fetch_optional(&mut **tx).await?;
    let source = if let Some(s) = source {
        let valid: bool=sqlx::query_scalar("SELECT $1=current_schema() AND $2=current_database() AND $3::bigint=(SELECT oid::bigint FROM pg_database WHERE datname=current_database()) AND $4=current_setting('port')::integer")
            .bind(s.try_get::<String,_>("namespace")?).bind(s.try_get::<String,_>("database_name")?)
            .bind(s.try_get::<sqlx::postgres::types::Oid,_>("database_oid")?.0 as i64).bind(s.try_get::<i32,_>("server_port")?)
            .fetch_one(&mut **tx).await?;
        if !valid
            || s.try_get::<String, _>("member_namespace")? != "billing_feedback_v1"
            || s.try_get::<String, _>("mapping_version")? != MAPPING
        {
            return Err(StorageError::TransactionScope);
        }
        Some(s.try_get::<String, _>("binding_text")?)
    } else {
        None
    };
    Ok(VerifiedBillingFacts {
        source,
        member_event: event,
        digest: hash(&json!({"payload":payload,"header":header,
      "staged":staged,"archive":archive,"request":{"binding":request.binding.snapshot,"fingerprint":request.binding.fingerprint,
      "payload":request.payload,"key":request.idempotency_key,"business":request.business_operation_ref,
      "version":request.business_version}}))?,
    })
}

pub(super) async fn verify_facts(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    request: &ExecutionRequest,
) -> Result<(), StorageError> {
    checked_facts(tx, owner, request).await.map(|_| ())
}

struct VerifiedBillingAuditPending {
    facts: VerifiedBillingFacts,
    completion: BillingCompletion,
    request_digest: ArgumentsHash,
}
fn details() -> Value {
    json!({"mapping_version":MAPPING,"schema_version":1,"action":"SAVE_BILLING_FEEDBACK","result":"SAVED","lock_authorized":false})
}
fn complete_digest(
    facts: &VerifiedBillingFacts,
    completion: &BillingCompletion,
) -> Result<ArgumentsHash, StorageError> {
    hash(&(&facts.digest, &completion.task, &completion.operation))
}
async fn append_billing_feedback_saved(
    tx: &mut Transaction<'_, Postgres>,
    pending: VerifiedBillingAuditPending,
) -> Result<EventId, StorageError> {
    let source = pending
        .facts
        .source
        .as_ref()
        .ok_or(StorageError::DependencyUnavailable)?;
    let task = &pending.completion.task;
    let op = &pending.completion.operation;
    let event = EventId::parse(format!("event_{}", uuid::Uuid::new_v4()))
        .map_err(|_| StorageError::CorruptRecord)?;
    sqlx::query("INSERT INTO audit_events(event_id,task_id,operation_id,kind,task_revision,operation_revision,details) VALUES($1,$2,$3,'BILLING_FEEDBACK_SAVED',$4,$5,$6)")
        .bind(event.as_str()).bind(task.task_id.as_str()).bind(op.operation_id.as_str()).bind(db_version(task.revision)?)
        .bind(db_version(op.revision)?).bind(Json(details())).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO public_billing_audit_links_v1(event_id,operation_id,member_event_id,source_binding_id,owner_id,task_id,kind,request_digest,facts_digest) VALUES($1,$2,$3,$4::uuid,$5,$6,'BILLING_FEEDBACK_SAVED',$7,$8)")
        .bind(event.as_str()).bind(op.operation_id.as_str()).bind(pending.facts.member_event).bind(source)
        .bind(task.owner_id.as_str()).bind(task.task_id.as_str()).bind(pending.request_digest.as_str())
        .bind(complete_digest(&pending.facts,&pending.completion)?.as_str()).execute(&mut **tx).await?;
    Ok(event)
}
pub(super) async fn append(
    tx: &mut Transaction<'_, Postgres>,
    request: &ExecutionRequest,
    completion: &BillingCompletion,
) -> Result<(), StorageError> {
    let facts = checked_facts(tx, &completion.task.owner_id, request).await?;
    if facts.source.is_some() {
        append_billing_feedback_saved(
            tx,
            VerifiedBillingAuditPending {
                facts,
                completion: completion.clone(),
                request_digest: request_digest(request)?,
            },
        )
        .await?;
    }
    verify_link(tx, request, completion).await?;
    Ok(())
}
pub(super) async fn verify_link(
    tx: &mut Transaction<'_, Postgres>,
    request: &ExecutionRequest,
    completion: &BillingCompletion,
) -> Result<BillingDomainAuditStatus, StorageError> {
    let task = &completion.task;
    let op = &completion.operation;
    let b = &request.binding.snapshot;
    let review = NodeId::parse("review_feedback").map_err(|_| StorageError::CorruptRecord)?;
    let save = NodeId::parse("save_feedback").map_err(|_| StorageError::CorruptRecord)?;
    let expected_plan = AgentPlan::new(
        b.task_id.clone(),
        b.plan_version,
        vec![
            PlanNode::UserDecision {
                id: review.clone(),
                depends_on: vec![],
            },
            PlanNode::Tool {
                id: save.clone(),
                tool: ToolName::parse(TOOL).map_err(|_| StorageError::CorruptRecord)?,
                arguments: request.payload.normalized_arguments.clone(),
                depends_on: vec![review.clone()],
            },
        ],
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    if task.plan != expected_plan
        || task.task_id != b.task_id
        || task.owner_id != b.user_id
        || task.state != TaskState::Completed
        || task.checkpoint != RecoveryPoint::Finished
        || task.completed_nodes != vec![review, save.clone()]
        || task.facts_version != b.facts_version
        || Some(task.revision) != b.task_revision.checked_add(3)
        || op.operation_id != b.operation_id
        || op.task_id != b.task_id
        || op.node_id != save
        || op.state != OperationState::Succeeded
        || Some(op.revision) != b.operation_revision.checked_add(2)
        || op.plan_version != b.plan_version
        || op.facts_version != b.facts_version
        || op.arguments_hash != b.arguments_hash
        || op.business_operation_ref.as_ref() != Some(&request.business_operation_ref)
    {
        return Err(StorageError::CorruptRecord);
    }
    let facts = checked_facts(tx, &completion.task.owner_id, request).await?;
    sqlx::query("SELECT verify_billing_audit_v1($1)")
        .bind(completion.operation.operation_id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(|_| StorageError::CorruptRecord)?;
    let legacy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM public_billing_audit_legacy_v1 WHERE operation_id=$1)",
    )
    .bind(completion.operation.operation_id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    let status = if facts.source.is_none() {
        BillingDomainAuditStatus::NotActivated
    } else if legacy {
        BillingDomainAuditStatus::LegacyWithoutDomainEvent
    } else {
        let row: PgRow = sqlx::query(
            "SELECT facts_digest FROM public_billing_audit_links_v1 WHERE operation_id=$1",
        )
        .bind(completion.operation.operation_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        if row.try_get::<String, _>("facts_digest")?
            != complete_digest(&facts, completion)?.as_str()
        {
            return Err(StorageError::CorruptRecord);
        }
        BillingDomainAuditStatus::VerifiedV1
    };
    Ok(status)
}
