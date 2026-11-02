//! Same-transaction payment minimum evaluation in the existing decision store.
use super::*;
use crate::{OperationRecord, PaymentSnapshot};
use safeflow_runtime::PaymentMinimumInput;

pub(crate) const MODE: &str = "TRUSTED_PAYMENT_MINIMUM_V1";

pub(crate) fn validate_historical(
    b: &ConfirmationBinding,
    metadata: &Value,
    decision: &SecurityDecision,
    phase: SecurityPhase,
) -> Result<(), StorageError> {
    let used = metadata["facts"]["used_cents"]
        .as_u64()
        .ok_or(StorageError::CorruptRecord)?;
    let current = metadata["facts"]["current_cents"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(StorageError::CorruptRecord)?;
    let combined = used
        .checked_add(current)
        .ok_or(StorageError::CorruptRecord)?;
    let red = combined > 100_000;
    if b.tool_name.as_str() != "foundation_payment_intent"
        || phase != SecurityPhase::PreAuth
        || metadata["kernel"]["version"] != decision.policy_version.as_str()
        || metadata["kernel"]["scope"] != "PUBLIC_PAYMENT_MINIMUM_ONLY"
        || metadata["kernel"]["risk_mode"] != "NOT_EVALUATED"
        || metadata["kernel"]["domain_policy_evaluated"] != false
        || metadata["kernel"]["funds_execution_enabled"] != false
        || metadata["facts"]["combined_cents"].as_u64() != Some(combined)
        || decision.minimum_level
            != if red {
                safeflow_contracts::SecurityLevel::Red
            } else {
                safeflow_contracts::SecurityLevel::Yellow
            }
        || decision.outcome
            != if red {
                SecurityOutcome::WaitingAuth
            } else {
                SecurityOutcome::ReadyToConfirm
            }
        || decision.required_gates
            != if red {
                vec![
                    safeflow_contracts::SecurityGate::ExplicitConfirmation,
                    safeflow_contracts::SecurityGate::StrongAuthentication,
                ]
            } else {
                vec![safeflow_contracts::SecurityGate::ExplicitConfirmation]
            }
        || decision.risk_score != 0
        || decision.findings.len() != 1
        || decision.findings[0].finding.reason_code.as_str()
            != "PAYMENT_MINIMUM_ONLY_DOMAIN_POLICY_NOT_EVALUATED"
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}

/// Omit observation clock from identity, retain statistical day and every source fact's digest.
pub(crate) async fn facts(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    payment: &PaymentSnapshot,
) -> Result<Value, StorageError> {
    let usage = crate::daily_usage::read_locked(tx, &task.owner_id).await?;
    facts_value(usage, payment)
}

async fn facts_for_mode(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    payment: &PaymentSnapshot,
    persist: bool,
) -> Result<Value, StorageError> {
    if persist {
        facts(tx, task, payment).await
    } else {
        facts_value(
            crate::daily_usage::read_for_mode(tx, &task.owner_id, false).await?,
            payment,
        )
    }
}

fn facts_value(
    usage: crate::DailyUsageSnapshot,
    payment: &PaymentSnapshot,
) -> Result<Value, StorageError> {
    if !usage
        .payer_accounts
        .contains(&payment.transfer.payer_account_id)
        || usage
            .submitted
            .iter()
            .any(|f| f.operation_id == payment.transfer.operation_id)
    {
        return Err(StorageError::ReconcileRequired);
    }
    // Same checked fixed-point conversion as the received transfer adapter. No float or rounding.
    let (whole, fraction) = payment
        .transfer
        .amount
        .as_str()
        .split_once('.')
        .ok_or(StorageError::CorruptRecord)?;
    let current = whole
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(100))
        .and_then(|v| v.checked_add(fraction.parse().ok()?))
        .filter(|v| *v > 0)
        .ok_or(StorageError::FactsConflict)?;
    let combined = usage
        .usage
        .used_cents
        .checked_add(current)
        .ok_or(StorageError::FactsConflict)?;
    Ok(
        json!({"usage_rule_version":usage.usage.rule_version,"statistical_day":usage.usage.statistical_day,
        "used_cents":usage.usage.used_cents,"current_cents":current,"combined_cents":combined,
        "usage_digest":hash(&(&usage.owner_id,&usage.payer_accounts,&usage.submitted,&usage.usage))?,
        "payment_digest":hash(payment)?,"business_operation_id":payment.transfer.operation_id,
        "tool_parameters":{"draft_id":payment.transfer.draft_id,"draft_version":payment.transfer.draft_version}}),
    )
}

impl DecisionService<'_> {
    /// Private trusted path: caller holds owner/task/draft locks and validated confirmation.
    pub(crate) async fn payment_locked(
        tx: &mut Transaction<'_, Postgres>,
        runtime: &Runtime,
        config: &DecisionConfig,
        task: &TaskRecord,
        op: &OperationRecord,
        session_ref: &SessionRef,
        session: &ConfirmationSession,
        binding: &ConfirmationBinding,
        payment: &PaymentSnapshot,
        confirmation: &ConfirmationRecord,
        valid: bool,
        evaluate: bool,
        refresh: bool,
        persist: bool,
    ) -> Result<(Vec<StoredSecurityDecision>, &'static str), StorageError> {
        let query = if persist {
            "SELECT * FROM security_decisions WHERE task_id=$1 AND operation_id=$2 AND source_mode=$3 ORDER BY evaluated_at_ms,stored_decision_id LIMIT 257 FOR UPDATE"
        } else {
            "SELECT * FROM security_decisions WHERE task_id=$1 AND operation_id=$2 AND source_mode=$3 ORDER BY evaluated_at_ms,stored_decision_id LIMIT 257"
        };
        let rows = sqlx::query(query)
            .bind(task.task_id.as_str())
            .bind(op.operation_id.as_str())
            .bind(MODE)
            .fetch_all(&mut **tx)
            .await?;
        let mut rows = rows.iter().map(decode_row).collect::<Result<Vec<_>, _>>()?;
        if rows.len() > 256 {
            return Err(StorageError::CorruptRecord);
        }
        let evaluate = persist && evaluate && (rows.is_empty() || refresh);
        let kernel = runtime.payment_minimum_metadata_for_backend();
        let settings = json!({"ttl_ms":config.ttl_ms,"deployment_revision_digest":hash(&config.deployment_revision)?});
        let mut unavailable = false;
        let current = if valid && confirmation.status == ConfirmationStatus::Confirmed {
            match facts_for_mode(tx, task, payment, persist).await {
                Ok(facts) => Some(facts),
                Err(StorageError::FactsConflict | StorageError::ReconcileRequired) if !evaluate => {
                    unavailable = true;
                    None
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let current_digest = current
            .as_ref()
            .map(|f| hash(&(binding, f, &confirmation.confirmation_ref)))
            .transpose()?;
        let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut **tx)
            .await?;
        let mut fresh = false;
        for row in &mut rows {
            if now.timestamp_millis() < row.record.evaluated_at_ms {
                return Err(StorageError::DecisionBindingMismatch);
            }
            if row.record.binding.user_id != task.owner_id
                || row.record.binding.operation_id != op.operation_id
            {
                return Err(StorageError::CorruptRecord);
            }
            let matches = current_digest.as_ref() == Some(&row.record.snapshot_digest)
                && row.record.binding == *binding
                && row.session_digest == session.digest.as_str()
                && row.session_version == session.binding.binding_version;
            let assembly_matches = row.record.assembly_metadata["kernel"] == kernel
                && row.record.assembly_metadata["settings"] == settings
                && row.record.assembly_metadata["storage_source_digest"]
                    == hash(&include_str!("payment.rs").replace("\r\n", "\n"))?.as_str();
            let expired = now.timestamp_millis() >= row.record.expires_at_ms;
            if row.record.status == StoredDecisionStatus::Active
                && (!matches || !assembly_matches || expired)
            {
                let reason = if !assembly_matches {
                    DecisionInvalidationReason::AssemblyChanged
                } else if expired {
                    DecisionInvalidationReason::Expired
                } else {
                    DecisionInvalidationReason::SnapshotChanged
                };
                row.record.status = StoredDecisionStatus::Invalidated;
                row.record.ended_at_ms =
                    Some(now.timestamp_millis().max(row.record.evaluated_at_ms));
                row.record.invalidation_reason = Some(reason);
                if persist {
                    let changed=sqlx::query("UPDATE security_decisions SET status='INVALIDATED',ended_at_ms=$1,invalidation_reason=$2 WHERE stored_decision_id=$3 AND status='ACTIVE'")
                    .bind(row.record.ended_at_ms).bind(reason_text(reason)).bind(row.record.stored_decision_id.as_str()).execute(&mut **tx).await?;
                    if changed.rows_affected() != 1 {
                        return Err(StorageError::DecisionConflict);
                    }
                    payment_audit(
                        tx,
                        task,
                        op,
                        &row.record,
                        PersistentAuditKind::SecurityDecisionInvalidated,
                    )
                    .await?;
                }
            }
            fresh |= row.record.status == StoredDecisionStatus::Active
                && matches
                && assembly_matches
                && !expired;
        }
        if evaluate && current.is_some() && !fresh {
            if rows.len() == 256 {
                return Err(StorageError::Invalid("payment history limit"));
            }
            let current = current.as_ref().ok_or(StorageError::ConfirmationRequired)?;
            let result = runtime
                .review_payment_minimum_for_backend(
                    session_ref,
                    PaymentMinimumInput {
                        operation_id: op.operation_id.clone(),
                        facts_version: u32::try_from(binding.facts_version)
                            .map_err(|_| StorageError::FactsConflict)?,
                        used_cents: current["used_cents"]
                            .as_u64()
                            .ok_or(StorageError::CorruptRecord)?,
                        current_cents: current["current_cents"]
                            .as_u64()
                            .ok_or(StorageError::CorruptRecord)?,
                    },
                )
                .map_err(|_| StorageError::DecisionSourcesUnavailable)?;
            if result.session_binding() != &session.binding {
                return Err(StorageError::Session);
            }
            let metadata = json!({"kernel":kernel,"facts":current,"settings":settings,
                "confirmation_ref":confirmation.confirmation_ref,"confirmation_binding_digest":hash(binding)?,
                "storage_source_digest":hash(&include_str!("payment.rs").replace("\r\n","\n"))?});
            let decision = result.decision().clone();
            validate_historical(binding, &metadata, &decision, SecurityPhase::PreAuth)?;
            let row = DecisionRow {
                record: StoredSecurityDecision {
                    contract_version: 1,
                    stored_decision_id: StoredDecisionId::new_server_id(),
                    request_id: DecisionRequestId::new_server_id(),
                    evidence_kind: "PREFLIGHT_ONLY",
                    source_mode: MODE,
                    source_instance_id: uuid::Uuid::new_v4().to_string(),
                    binding: binding.clone(),
                    phase: SecurityPhase::PreAuth,
                    snapshot_digest: hash(&(binding, current, &confirmation.confirmation_ref))?,
                    assembly_digest: hash(&metadata)?,
                    assembly_metadata: metadata,
                    decision_digest: hash(&decision)?,
                    decision,
                    // A minimum review does not claim an executable funds plan was accepted.
                    plan_review_digest: hash(&("PAYMENT_MINIMUM_ONLY", binding))?,
                    evaluated_at_ms: now.timestamp_millis(),
                    expires_at_ms: now
                        .timestamp_millis()
                        .checked_add(config.ttl_ms)
                        .ok_or(StorageError::Invalid("payment decision expiry overflow"))?
                        .min(confirmation.expires_at.timestamp_millis()),
                    status: StoredDecisionStatus::Active,
                    ended_at_ms: None,
                    invalidation_reason: None,
                },
                session_digest: session.digest.as_str().to_owned(),
                session_version: session.binding.binding_version,
            };
            check_time(&row.record, now.timestamp_millis())?;
            insert_decision(tx, &row).await?;
            payment_audit(
                tx,
                task,
                op,
                &row.record,
                PersistentAuditKind::SecurityDecisionRecorded,
            )
            .await?;
            rows.push(row);
            fresh = true;
        }
        // Re-read after storage/audit waits: day, ownership, usage and expiry cannot drift at commit.
        if fresh {
            let later = facts_for_mode(tx, task, payment, persist).await?;
            let time: chrono::DateTime<chrono::Utc> =
                sqlx::query_scalar("SELECT clock_timestamp()")
                    .fetch_one(&mut **tx)
                    .await?;
            if Some(&later) != current.as_ref() || time >= confirmation.expires_at {
                return Err(StorageError::DecisionBindingMismatch);
            }
            for row in &rows {
                if row.record.status == StoredDecisionStatus::Active {
                    check_time(&row.record, time.timestamp_millis())?;
                }
            }
        }
        let state = if unavailable {
            "FACTS_UNAVAILABLE"
        } else if fresh {
            "CURRENT_MINIMUM_ONLY"
        } else if rows.is_empty() {
            "NOT_EVALUATED"
        } else {
            "HISTORICAL_REEVALUATION_REQUIRED"
        };
        Ok((rows.into_iter().map(|r| r.record).collect(), state))
    }
}

async fn payment_audit(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    op: &OperationRecord,
    r: &StoredSecurityDecision,
    kind: PersistentAuditKind,
) -> Result<(), StorageError> {
    // Frozen persistent audit shape, shared with the existing decision history reader.
    audit(tx,task,Some(op),kind,json!({"stored_decision_id":r.stored_decision_id,"request_id":r.request_id,
        "evidence_kind":"PREFLIGHT_ONLY","source_mode":r.source_mode,"phase":r.phase,
        "outcome":r.decision.outcome,"assembly_digest":r.assembly_digest,"decision_digest":r.decision_digest})).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use safeflow_bank_gateway::{CnyCurrency, DecimalAmount};
    use safeflow_contracts::{ResourceId, ToolResultStatus, UserId};
    use safeflow_transfer_integration::{
        daily_usage::{calculate_daily_usage, DailyUsageInput, SubmittedTransferFact},
        mock_execution::MockTransferRequest,
    };

    #[test]
    fn daily_payment_minimum_cross_day_changes_identity_even_with_identical_combined_amount() {
        let owner = UserId::parse("fixture_owner").unwrap();
        let payer = ResourceId::parse("fixture_payer").unwrap();
        let submitted = vec![SubmittedTransferFact {
            owner_id: owner.clone(),
            operation_id: OperationId::parse("prior_original").unwrap(),
            payer_account_id: payer.clone(),
            amount: DecimalAmount::parse("900.00").unwrap(),
            status: ToolResultStatus::Processing,
            submitted_at: Some("2026-10-17T15:00:00Z".into()),
        }];
        let payment = PaymentSnapshot {
            transfer: MockTransferRequest {
                task_id: TaskId::parse("payment_task").unwrap(),
                operation_id: OperationId::parse("unsubmitted_original").unwrap(),
                draft_id: ResourceId::parse("payment_draft").unwrap(),
                draft_version: 2,
                payer_account_id: payer.clone(),
                payee_id: ResourceId::parse("fixture_payee").unwrap(),
                amount: DecimalAmount::parse("200.00").unwrap(),
                currency: CnyCurrency::Cny,
                remark: "exact binding".into(),
            },
            display_details: json!({"fixture":"public fact identity only"}),
            draft_expires_at: 2_000_000_000,
        };
        let source = |as_of: &str| {
            let usage = calculate_daily_usage(DailyUsageInput {
                owner_id: &owner,
                as_of,
                complete: true,
                submitted: &submitted,
            })
            .unwrap();
            facts_value(
                crate::DailyUsageSnapshot {
                    owner_id: owner.clone(),
                    payer_accounts: vec![payer.clone()],
                    as_of: as_of.into(),
                    submitted: submitted.clone(),
                    usage,
                },
                &payment,
            )
            .unwrap()
        };
        let before = source("2026-10-17T15:59:59.999Z");
        let after = source("2026-10-17T16:00:00Z");
        assert_eq!(before["combined_cents"], 110_000);
        assert_eq!(before["combined_cents"], after["combined_cents"]);
        assert_eq!(before["statistical_day"], "2026-10-17");
        assert_eq!(after["statistical_day"], "2026-10-18");
        assert_ne!(hash(&before).unwrap(), hash(&after).unwrap());
        assert_eq!(after, source("2026-10-17T16:00:01Z"));
    }
}
