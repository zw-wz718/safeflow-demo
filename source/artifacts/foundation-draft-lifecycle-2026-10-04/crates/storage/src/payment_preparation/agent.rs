use super::*;
use safeflow_contracts::{PaymentDraftLocator, PAYMENT_INTENT_TOOL, PAYMENT_LOCATOR_PROJECTION};
use safeflow_runtime::{PlanningGatewaySource, PlanningPersistenceReview};

/// Correspondence, not an executable fingerprint or an authorization grant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentAgentBinding {
    pub scope: String,
    pub projection_revision: String,
    pub reviewed_plan: AgentPlan,
    pub reviewed_plan_digest: ArgumentsHash,
    pub registration_digest: ArgumentsHash,
    pub planning_source: PlanningGatewaySource,
    pub locator: PaymentDraftLocator,
    pub preparation_operation_id: OperationId,
    pub confirmation_ref: ConfirmationRef,
    pub snapshot_digest: ArgumentsHash,
    pub lifecycle_plan_digest: ArgumentsHash,
}

fn locator(plan: &AgentPlan) -> Result<PaymentDraftLocator, StorageError> {
    validate_plan(plan)?;
    if plan.nodes.len() != 1 {
        return Err(StorageError::PlanConflict);
    }
    match &plan.nodes[0] {
        PlanNode::Tool {
            tool,
            arguments,
            depends_on,
            ..
        } if tool.as_str() == PAYMENT_INTENT_TOOL && depends_on.is_empty() => {
            let value: PaymentDraftLocator = serde_json::from_value(arguments.clone())
                .map_err(|_| StorageError::PlanConflict)?;
            valid_id(value.draft_id.as_str())?;
            if value.draft_version == 0 {
                return Err(StorageError::PlanConflict);
            }
            Ok(value)
        }
        _ => Err(StorageError::PlanConflict),
    }
}

pub(super) async fn bind(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    operation: &OperationId,
    review: &PlanningPersistenceReview<'_>,
) -> Result<(), StorageError> {
    let saved = saved_payment(tx, task, operation, false).await?;
    let plan = review
        .planning_result()
        .plan
        .as_ref()
        .ok_or(StorageError::PlanConflict)?;
    let mut binding = PaymentAgentBinding {
        scope: "PAYMENT_AGENT_INTENT_ONLY".into(),
        projection_revision: PAYMENT_LOCATOR_PROJECTION.into(),
        reviewed_plan: plan.clone(),
        reviewed_plan_digest: hash(plan)?,
        registration_digest: review.registration_hash(),
        planning_source: review.gateway_source(),
        locator: locator(plan)?,
        preparation_operation_id: operation.clone(),
        confirmation_ref: saved.stored.record.confirmation_ref.clone(),
        snapshot_digest: saved.digest.clone(),
        lifecycle_plan_digest: saved.base_digest.clone(),
    };
    validate(&binding, &saved, task)?;
    if let Some(old) = read_binding(tx, task, &saved).await? {
        // A model node label is not payment identity. Both plans were validated
        // as one dependency-free payment node; preserve the first provenance.
        if let PlanNode::Tool { id, .. } = &mut binding.reviewed_plan.nodes[0] {
            *id = old.reviewed_plan.nodes[0].id().clone();
        }
        binding.reviewed_plan_digest = hash(&binding.reviewed_plan)?;
        if old != binding {
            return Err(StorageError::ExecutionKeyConflict);
        }
        return Ok(());
    }
    let digest = hash(&binding)?;
    sqlx::query(
        "INSERT INTO payment_agent_bindings(operation_id,binding,binding_digest) VALUES($1,$2,$3)",
    )
    .bind(operation.as_str())
    .bind(Json(&binding))
    .bind(digest.as_str())
    .execute(&mut **tx)
    .await?;
    audit(
        tx,
        task,
        Some(&saved.op),
        PersistentAuditKind::CheckpointSaved,
        json!({"payment_agent_binding":{"binding_digest":digest,"scope":binding.scope}}),
    )
    .await?;
    Ok(())
}

fn validate(
    binding: &PaymentAgentBinding,
    saved: &SavedPayment,
    task: &TaskRecord,
) -> Result<(), StorageError> {
    if binding.scope != "PAYMENT_AGENT_INTENT_ONLY"
        || binding.projection_revision != PAYMENT_LOCATOR_PROJECTION
        || binding.reviewed_plan.task_id != task.task_id
        || Some(binding.reviewed_plan.plan_version) != saved.op.plan_version.checked_add(1)
        || binding.reviewed_plan_digest != hash(&binding.reviewed_plan)?
        || binding.locator != locator(&binding.reviewed_plan)?
        || binding.locator.draft_id != saved.snapshot.transfer.draft_id
        || binding.locator.draft_version != saved.snapshot.transfer.draft_version
        || binding.preparation_operation_id != saved.op.operation_id
        || binding.snapshot_digest != saved.digest
        || binding.lifecycle_plan_digest != saved.base_digest
        || binding.confirmation_ref != saved.stored.record.confirmation_ref
        || !matches!(
            binding.planning_source,
            PlanningGatewaySource::MockLlm | PlanningGatewaySource::OpenaiCompatible
        )
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}

pub(super) async fn read_binding(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    saved: &SavedPayment,
) -> Result<Option<PaymentAgentBinding>, StorageError> {
    let row = sqlx::query(
        "SELECT binding,binding_digest FROM payment_agent_bindings WHERE operation_id=$1",
    )
    .bind(saved.op.operation_id.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let binding = row.try_get::<Json<PaymentAgentBinding>, _>("binding")?.0;
    validate(&binding, saved, task)?;
    let digest = hash(&binding)?;
    let audited: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1 AND operation_id=$2 AND kind='CHECKPOINT_SAVED' AND details->'payment_agent_binding'=$3)")
        .bind(task.task_id.as_str()).bind(saved.op.operation_id.as_str())
        .bind(Json(json!({"binding_digest":digest,"scope":binding.scope}))).fetch_one(&mut **tx).await?;
    if row.try_get::<String, _>("binding_digest")? != digest.as_str() || !audited {
        return Err(StorageError::CorruptRecord);
    }
    Ok(Some(binding))
}
