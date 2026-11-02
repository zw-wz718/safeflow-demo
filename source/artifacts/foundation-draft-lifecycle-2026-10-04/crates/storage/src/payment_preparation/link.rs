//! Versioned internal association, deliberately separate from a funds request.
use super::*;
use safeflow_contracts::{BusinessOperationRef, UserId};

const MODE: &str = "PAYMENT_REGISTRATION_ONLY";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaymentRegistrationBinding {
    pub owner_id: UserId,
    pub task_id: TaskId,
    pub registration_operation_id: OperationId,
    pub node_id: NodeId,
    pub preparation_operation_id: OperationId,
    pub task_revision: u64,
    pub plan_version: u32,
    pub facts_version: u64,
    pub transfer_arguments_hash: ArgumentsHash,
    pub preparation_binding_digest: ArgumentsHash,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaymentExecutionLink {
    pub scope: &'static str,
    pub preparation_operation_id: OperationId,
    pub confirmation_ref: ConfirmationRef,
    pub preparation_binding: ConfirmationBinding,
    pub registration_binding: PaymentRegistrationBinding,
    pub normalized_arguments: MockTransferRequest,
    pub registration_key: String,
    pub business_operation_id: BusinessOperationRef,
    pub registration_operation: OperationRecord,
    pub registered_at: DateTime<Utc>,
    pub state: &'static str,
    pub funds_execution_enabled: bool,
    pub confirmation_migration_authorized: bool,
}

fn binding(
    saved: &SavedPayment,
    op: &OperationRecord,
) -> Result<PaymentRegistrationBinding, StorageError> {
    let original = &saved.stored.record.binding;
    Ok(PaymentRegistrationBinding {
        owner_id: original.user_id.clone(),
        task_id: original.task_id.clone(),
        registration_operation_id: op.operation_id.clone(),
        node_id: op.node_id.clone(),
        preparation_operation_id: saved.op.operation_id.clone(),
        task_revision: original.task_revision,
        plan_version: original.plan_version,
        facts_version: original.facts_version,
        transfer_arguments_hash: hash(&saved.snapshot.transfer)?,
        preparation_binding_digest: hash(original)?,
    })
}

fn digest(
    saved: &SavedPayment,
    registration: &PaymentRegistrationBinding,
    key: &str,
) -> Result<ArgumentsHash, StorageError> {
    hash(&(
        "safeflow-payment-registration:v1",
        key,
        &saved.stored.record.confirmation_ref,
        &saved.stored.record.binding,
        &saved.snapshot,
        registration,
    ))
}

impl PaymentPreparationService<'_> {
    pub async fn register_execution(
        &self,
        reference: &SessionRef,
        task_id: &TaskId,
        operation: &OperationId,
        confirmation: &ConfirmationRef,
        display: &str,
    ) -> Result<PaymentExecutionLink, StorageError> {
        valid_id(confirmation.as_str())?;
        valid_id(display)?;
        let (session, mut tx, task) = self.begin(reference, task_id).await?;
        let saved = saved_payment(&mut tx, &task, operation, true).await?;
        if &saved.stored.record.confirmation_ref != confirmation || saved.display != display {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        // Replays return immutable history, never revive an old confirmation.
        if let Some(link) = read_link(&mut tx, &task, &saved).await? {
            self.confirmations.guard(reference, &session)?;
            tx.commit().await?;
            return Ok(link);
        }
        validate_source(&mut tx, &task, &session, &saved).await?;
        let op = OperationRecord {
            operation_id: OperationId::parse(format!(
                "payment_registration_{}",
                uuid::Uuid::new_v4()
            ))
            .unwrap(),
            task_id: task.task_id.clone(),
            node_id: NodeId::parse(format!("payment_registration_{}", uuid::Uuid::new_v4()))
                .unwrap(),
            state: OperationState::Draft,
            revision: 1,
            plan_version: saved.op.plan_version,
            facts_version: saved.op.facts_version,
            arguments_hash: hash(&saved.snapshot.transfer)?,
            business_operation_ref: None,
        };
        let registration = binding(&saved, &op)?;
        // This key identifies only the metadata registration. Funds keys are not allocated here.
        let key = format!("registration_{}", uuid::Uuid::new_v4());
        let link_digest = digest(&saved, &registration, &key)?;
        sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'DRAFT',1,$4,$5,$6)")
            .bind(op.operation_id.as_str()).bind(task.task_id.as_str()).bind(op.node_id.as_str())
            .bind(op.plan_version as i64).bind(db_version(op.facts_version)?).bind(op.arguments_hash.as_str()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO payment_execution_links(preparation_operation_id,task_id,owner_id,registration_key,registration_operation_id,business_operation_ref,business_version,confirmation_ref,preparation_binding,registration_binding,snapshot_digest,link_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
            .bind(operation.as_str()).bind(task.task_id.as_str()).bind(task.owner_id.as_str())
            .bind(&key).bind(op.operation_id.as_str()).bind(saved.snapshot.transfer.operation_id.as_str())
            .bind(i64::from(saved.snapshot.transfer.draft_version)).bind(confirmation.as_str())
            .bind(Json(&saved.stored.record.binding)).bind(Json(&registration)).bind(saved.digest.as_str()).bind(link_digest.as_str())
            .execute(&mut *tx).await?;
        audit(
            &mut tx,
            &task,
            Some(&op),
            PersistentAuditKind::OperationCreated,
            json!({"payment_registration":{"scope":MODE,"preparation_operation_id":operation,
            "confirmation_ref":confirmation,"preparation_binding":saved.stored.record.binding,
            "registration_binding":registration,"link_digest":link_digest,
            "confirmation_migration_authorized":false,"funds_execution_enabled":false}}),
        )
        .await?;
        // Recheck after trigger/Outbox waits, before the common transaction commits.
        self.confirmations.guard(reference, &session)?;
        validate_source(&mut tx, &task, &session, &saved).await?;
        let link = read_link(&mut tx, &task, &saved)
            .await?
            .ok_or(StorageError::CorruptRecord)?;
        self.confirmations.guard(reference, &session)?;
        check_active_at(&saved.stored.record, server_time(&mut tx).await?)?;
        tx.commit().await?;
        Ok(link)
    }
}

async fn validate_source(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    session: &ConfirmationSession,
    saved: &SavedPayment,
) -> Result<(), StorageError> {
    let record = &saved.stored.record;
    saved.stored.check_session(session)?;
    check_active_at(record, server_time(tx).await?)?;
    if saved.reason.is_some() || record.status != ConfirmationStatus::Confirmed {
        return Err(StorageError::ConfirmationRequired);
    }
    let b = &record.binding;
    if b.task_revision != task.revision
        || b.plan_version != task.plan.plan_version
        || b.facts_version != task.facts_version
        || hash(&task.plan)? != saved.base_digest
        || current_snapshot(
            tx,
            task,
            &saved.snapshot.transfer.draft_id,
            saved.snapshot.transfer.draft_version,
        )
        .await?
            != saved.snapshot
    {
        return Err(StorageError::ConfirmationBindingMismatch);
    }
    Ok(())
}

pub(super) async fn read_link(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    saved: &SavedPayment,
) -> Result<Option<PaymentExecutionLink>, StorageError> {
    let row =
        sqlx::query("SELECT * FROM payment_execution_links WHERE preparation_operation_id=$1")
            .bind(saved.op.operation_id.as_str())
            .fetch_optional(&mut **tx)
            .await?;
    let Some(row) = row else { return Ok(None) };
    let op_row = sqlx::query("SELECT * FROM operations WHERE task_id=$1 AND operation_id=$2")
        .bind(task.task_id.as_str())
        .bind(row.try_get::<String, _>("registration_operation_id")?)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::CorruptRecord)?;
    let op = operation_row(&op_row)?;
    let original = &saved.stored.record.binding;
    let registration = binding(saved, &op)?;
    let key: String = row.try_get("registration_key")?;
    valid_id(&key).map_err(|_| StorageError::CorruptRecord)?;
    if row.try_get::<Json<Value>, _>("preparation_binding")?.0
        != serde_json::to_value(original).map_err(|_| StorageError::CorruptRecord)?
        || row.try_get::<Json<Value>, _>("registration_binding")?.0
            != serde_json::to_value(&registration).map_err(|_| StorageError::CorruptRecord)?
        || row.try_get::<String, _>("task_id")? != task.task_id.as_str()
        || row.try_get::<String, _>("owner_id")? != task.owner_id.as_str()
        || row.try_get::<String, _>("confirmation_ref")?
            != saved.stored.record.confirmation_ref.as_str()
        || row.try_get::<String, _>("snapshot_digest")? != saved.digest.as_str()
        || row.try_get::<String, _>("link_digest")? != digest(saved, &registration, &key)?.as_str()
        || row.try_get::<String, _>("business_operation_ref")?
            != saved.snapshot.transfer.operation_id.as_str()
        || row.try_get::<i64, _>("business_version")?
            != i64::from(saved.snapshot.transfer.draft_version)
        || op.operation_id == saved.op.operation_id
        || original.user_id != task.owner_id
        || op.state != OperationState::Draft
        || op.revision != 1
        || op.business_operation_ref.is_some()
        || op.plan_version != original.plan_version
        || op.facts_version != original.facts_version
        || op.arguments_hash != registration.transfer_arguments_hash
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(Some(PaymentExecutionLink {
        scope: MODE,
        preparation_operation_id: saved.op.operation_id.clone(),
        confirmation_ref: saved.stored.record.confirmation_ref.clone(),
        preparation_binding: original.clone(),
        registration_binding: registration,
        normalized_arguments: saved.snapshot.transfer.clone(),
        registration_key: key,
        business_operation_id: BusinessOperationRef::parse(
            saved.snapshot.transfer.operation_id.as_str(),
        )
        .map_err(|_| StorageError::CorruptRecord)?,
        registration_operation: op,
        registered_at: row.try_get("registered_at")?,
        state: "REGISTERED_UNSUBMITTED",
        funds_execution_enabled: false,
        confirmation_migration_authorized: false,
    }))
}
