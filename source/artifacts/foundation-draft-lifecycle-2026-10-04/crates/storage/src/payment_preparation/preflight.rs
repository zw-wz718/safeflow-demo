//! Immutable business observations. This module never registers an executable tool/key.
use super::*;
use safeflow_contracts::{
    ContractFuture, DomainPolicy, IdempotencyKey, SecurityContext, SecurityError, SecurityFinding,
    SecurityPhase, SECURITY_CONTRACT_VERSION,
};
use safeflow_transfer_integration::{
    funds_policy::{DailyFacts, FundsBusinessPolicy, FundsPolicyFacts, FundsPolicySource},
    mock_execution::MockOutgoingState,
};

const SCOPE: &str = "PAYMENT_BUSINESS_OBSERVATION_ONLY";
const INTERNAL_TOOL: &str = "foundation_payment_business_preflight";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentBusinessObservation {
    pub observation_id: String,
    pub ordinal: i64,
    pub scope: String,
    pub preparation_binding: Value,
    pub snapshot_digest: ArgumentsHash,
    pub session_digest: String,
    pub session_version: u64,
    pub context: Value,
    pub facts: Value,
    pub source_digest: ArgumentsHash,
    pub assembly_digest: ArgumentsHash,
    pub findings: Vec<SecurityFinding>,
    pub observed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub funds_execution_enabled: bool,
}

#[derive(Serialize)]
pub struct PaymentBusinessPreflight {
    pub scope: &'static str,
    pub freshness: &'static str,
    pub observations: Vec<PaymentBusinessObservation>,
    pub formal_execution_binding_adopted: bool,
    pub missing_sources: [&'static str; 1],
    pub not_evaluated_checks: [&'static str; 2],
}

// Only this backend module constructs the source, from one borrowed transaction.
struct SnapshotSource(FundsPolicyFacts);
impl FundsPolicySource for SnapshotSource {
    fn read<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<FundsPolicyFacts, SecurityError>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

async fn source(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    saved: &SavedPayment,
    confirmation: &ConfirmationRecord,
) -> Result<(SecurityContext, FundsPolicyFacts, Value), StorageError> {
    let internal_key =
        IdempotencyKey::parse(format!("payment_preflight_{}", saved.op.operation_id))
            .map_err(|_| StorageError::CorruptRecord)?;
    let mut stored =
        FundsStore::policy_facts(tx, &task.owner_id, &internal_key, &saved.snapshot.transfer)
            .await
            .map_err(crate::transfer_sql::business_error)?;
    // An existing original is compared with its OWN received key, solely for query.
    // The private comparison key is never promoted to a proposed funds key.
    let (key, key_scope) = if let Some(key) = stored.original_key.clone() {
        stored = FundsStore::policy_facts(tx, &task.owner_id, &key, &saved.snapshot.transfer)
            .await
            .map_err(crate::transfer_sql::business_error)?;
        (key, "OBSERVED_ORIGINAL_KEY_QUERY_ONLY")
    } else {
        (internal_key, "INTERNAL_COMPARISON_ONLY")
    };
    let context = SecurityContext {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: task.task_id.clone(),
        node_id: saved.op.node_id.clone(),
        operation_id: saved.op.operation_id.clone(),
        facts_version: u32::try_from(task.facts_version)
            .map_err(|_| StorageError::FactsConflict)?,
        phase: SecurityPhase::PreAuth,
        tool_name: ToolName::parse(INTERNAL_TOOL).unwrap(),
        plan_version: task.plan.plan_version,
        authenticated_user_id: task.owner_id.clone(),
        normalized_arguments: serde_json::to_value(&saved.snapshot.transfer)
            .map_err(|_| StorageError::CorruptRecord)?,
        arguments_hash: hash(&saved.snapshot.transfer)?,
        idempotency_key: Some(key),
        confirmation_ref: Some(confirmation.confirmation_ref.clone()),
        auth_grant_ref: None,
    };
    let (daily, daily_evidence, checked_at) =
        match crate::daily_usage::read_for_mode(tx, &task.owner_id, false).await {
            Ok(usage) => {
                let evidence = json!({"scope":"OWNER_ALL_ACCOUNTS_PUBLIC_FIRST_SUBMISSION",
                "owner_id":usage.owner_id,"payer_accounts":usage.payer_accounts,
                "submitted":usage.submitted,"usage":usage.usage,"complete":true});
                (
                    Some(DailyFacts {
                        complete: true,
                        submitted: usage.submitted,
                    }),
                    evidence,
                    usage.as_of,
                )
            }
            Err(StorageError::ReconcileRequired | StorageError::FactsConflict) => (
                None,
                json!({"complete":false,"missing_reason":"PUBLIC_DAILY_BINDING_UNAVAILABLE"}),
                server_time(tx).await?.to_rfc3339(),
            ),
            Err(error) => return Err(error),
        };
    let now = DateTime::parse_from_rfc3339(&checked_at)
        .map_err(|_| StorageError::CorruptRecord)?
        .with_timezone(&Utc);
    // Deliberately omit the varying observation clock from source identity. Include
    // expiry as a predicate, and the public statistical day, so time changes matter.
    let draft = stored.draft.as_ref().map(|d| {
        json!({
            "draft_id":d.draft_id,"operation_id":d.operation_id,"task_id":d.task_id,
            "owner_id":d.user_id,"version":d.plan_version,"status":format!("{:?}", d.status),
            "expires_at":d.expires_at,"expired":now.timestamp() >= d.expires_at as i64,
            "details":d.details,
        })
    });
    let evidence = json!({"member_source":"cacc62c09ac07555844c3422b62709a577ed5375",
        "policy_source":"b45473f","isolation":"REPEATABLE_READ",
        "owner_id":stored.owner_id,"request":stored.request,"comparison_key":stored.key,
        "key_scope":key_scope,"formal_execution_binding_adopted":false,
        "draft":draft,"submitted":stored.submitted,"payer_owner_id":stored.payer_owner_id,
        "funds":stored.funds,"recipient_account_id":stored.recipient_account_id,
        "original_request":stored.original_request,"original_key":stored.original_key,
        "key_operation_id":stored.key_operation_id,"daily":daily_evidence,
        "outgoing":{"state":"UNKNOWN","missing_reason":"CARD_TRANSACTION_REPOSITORY_NOT_CONNECTED"}});
    Ok((
        context.clone(),
        FundsPolicyFacts {
            binding: context,
            checked_at,
            stored,
            outgoing: MockOutgoingState::Unknown,
            daily,
        },
        evidence,
    ))
}

pub(super) async fn observe(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    session: &ConfirmationSession,
    saved: &SavedPayment,
    confirmation: &ConfirmationRecord,
    valid: bool,
    evaluate: bool,
    refresh: bool,
    ttl_ms: i64,
) -> Result<PaymentBusinessPreflight, StorageError> {
    let rows = sqlx::query("SELECT * FROM payment_business_observations WHERE operation_id=$1 ORDER BY ordinal LIMIT 257")
        .bind(saved.op.operation_id.as_str()).fetch_all(&mut **tx).await?;
    if rows.len() > 256 {
        return Err(StorageError::CorruptRecord);
    }
    let binding = serde_json::to_value(&saved.stored.record.binding)
        .map_err(|_| StorageError::CorruptRecord)?;
    let mut observations = Vec::new();
    for row in rows {
        let observation = row
            .try_get::<Json<PaymentBusinessObservation>, _>("observation")?
            .0;
        let digest = hash(&observation)?;
        let audited: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='CHECKPOINT_SAVED' AND details->'payment_business_preflight'=$3)")
            .bind(task.task_id.as_str()).bind(saved.op.operation_id.as_str())
            .bind(Json(json!({"scope":SCOPE,"observation_id":observation.observation_id,"observation_digest":digest})))
            .fetch_one(&mut **tx).await?;
        if observation.observation_id != row.try_get::<String, _>("observation_id")?
            || observation.ordinal != observations.len() as i64 + 1
            || observation.ordinal != row.try_get::<i64, _>("ordinal")?
            || observation.scope != SCOPE
            || observation.funds_execution_enabled
            || observation.preparation_binding != binding
            || observation.snapshot_digest != saved.digest
            || observation.source_digest != hash(&(&observation.context, &observation.facts))?
            || observation.context["authenticated_user_id"] != json!(task.owner_id)
            || observation.context["normalized_arguments"] != json!(saved.snapshot.transfer)
            || observation.context["tool_name"] != INTERNAL_TOOL
            || observation.context["confirmation_ref"]
                != json!(saved.stored.record.confirmation_ref)
            || digest.as_str() != row.try_get::<String, _>("observation_digest")?
            || observation.expires_at <= observation.observed_at
            || !audited
        {
            return Err(StorageError::CorruptRecord);
        }
        observations.push(observation);
    }
    let assembly = hash(&(
        include_str!("preflight.rs").replace("\r\n", "\n"),
        include_str!("../../../transfer/src/funds_policy.rs").replace("\r\n", "\n"),
        include_str!("../../../transfer/src/funds_store.rs").replace("\r\n", "\n"),
        include_str!("../daily_usage.rs").replace("\r\n", "\n"),
        ttl_ms,
    ))?;
    let now = server_time(tx).await?;
    if evaluate && confirmation.status != ConfirmationStatus::Revoked {
        check_active_at(confirmation, now)?;
    }
    // Expiry/rejection must not append unusable observations or roll back the
    // caller's saved invalidation. Unexpired source-conflict diagnostics remain available.
    let evaluate =
        evaluate && now < confirmation.expires_at && saved.reason.as_deref() != Some("REJECTED");
    let current = if evaluate || (valid && !observations.is_empty()) {
        Some(source(tx, task, saved, confirmation).await?)
    } else {
        None
    };
    let current_digest = current
        .as_ref()
        .map(|(c, _, facts)| {
            hash(&(
                serde_json::to_value(c).map_err(|_| StorageError::CorruptRecord)?,
                facts,
            ))
        })
        .transpose()?;
    let now = server_time(tx).await?;
    if evaluate && confirmation.status != ConfirmationStatus::Revoked {
        check_active_at(confirmation, now)?;
    }
    let fresh = observations.last().is_some_and(|o| {
        valid
            && confirmation.status == ConfirmationStatus::Confirmed
            && current_digest.as_ref() == Some(&o.source_digest)
            && o.session_digest == session.digest.as_str()
            && o.session_version == session.binding.binding_version
            && o.assembly_digest == assembly
            && now < o.expires_at
    });
    // Confirmation replay never appends an observation, even after source changes.
    // Explicit evaluation is idempotent while the last observation remains current.
    let mut appended = false;
    if evaluate && now < confirmation.expires_at && (observations.is_empty() || (refresh && !fresh))
    {
        if observations.len() == 256 {
            return Err(StorageError::Invalid("payment observation history limit"));
        }
        let (context, facts, evidence) = current.ok_or(StorageError::CorruptRecord)?;
        let observed_at = DateTime::parse_from_rfc3339(&facts.checked_at)
            .map_err(|_| StorageError::CorruptRecord)?
            .with_timezone(&Utc);
        let findings = FundsBusinessPolicy::new(context.tool_name.clone(), SnapshotSource(facts))
            .evaluate(&context)
            .await
            .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
        let observation = PaymentBusinessObservation {
            observation_id: format!("payment_observation_{}", uuid::Uuid::new_v4()),
            ordinal: observations.len() as i64 + 1,
            scope: SCOPE.into(),
            preparation_binding: binding,
            snapshot_digest: saved.digest.clone(),
            session_digest: session.digest.as_str().into(),
            session_version: session.binding.binding_version,
            context: serde_json::to_value(context).map_err(|_| StorageError::CorruptRecord)?,
            facts: evidence,
            source_digest: current_digest.clone().ok_or(StorageError::CorruptRecord)?,
            assembly_digest: assembly,
            findings,
            observed_at,
            expires_at: observed_at
                .checked_add_signed(chrono::Duration::milliseconds(ttl_ms))
                .ok_or(StorageError::CorruptRecord)?
                .min(confirmation.expires_at),
            funds_execution_enabled: false,
        };
        let digest = hash(&observation)?;
        sqlx::query("INSERT INTO payment_business_observations(observation_id,operation_id,ordinal,observation,observation_digest) VALUES($1,$2,$3,$4,$5)")
            .bind(&observation.observation_id).bind(saved.op.operation_id.as_str())
            .bind(observation.ordinal).bind(Json(&observation)).bind(digest.as_str())
            .execute(&mut **tx).await.map_err(|error| {
                // Another explicit evaluation may have appended after our RR snapshot.
                if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("23505") {
                    StorageError::RevisionConflict
                } else { error.into() }
            })?;
        audit(
            tx,
            task,
            Some(&saved.op),
            PersistentAuditKind::CheckpointSaved,
            json!({"payment_business_preflight":{"scope":SCOPE,
                "observation_id":observation.observation_id,"observation_digest":digest}}),
        )
        .await?;
        if server_time(tx).await? >= observation.expires_at {
            return Err(StorageError::DecisionBindingMismatch);
        }
        observations.push(observation);
        appended = true;
    }
    let freshness = if observations.is_empty() {
        "NOT_EVALUATED"
    } else if !valid || confirmation.status != ConfirmationStatus::Confirmed {
        "PREPARATION_INVALIDATED"
    } else if fresh || appended {
        "CURRENT_OBSERVATION_ONLY"
    } else {
        "HISTORICAL_REEVALUATION_REQUIRED"
    };
    Ok(PaymentBusinessPreflight {
        scope: SCOPE,
        freshness,
        observations,
        formal_execution_binding_adopted: false,
        missing_sources: ["CARD_TRANSACTION_REPOSITORY_NOT_CONNECTED"],
        not_evaluated_checks: ["BALANCE_SUFFICIENCY", "DAILY_USAGE_BUSINESS_POLICY"],
    })
}
