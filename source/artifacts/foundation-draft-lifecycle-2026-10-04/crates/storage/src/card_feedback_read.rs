//! Child of card_read: both entry paths use the SAME private request container.
//! No member object is reconstructed from wire/display data. Read observations
//! confer neither freeze permission nor live feedback correction semantics.
use super::*;
use crate::{permit::CheckedBillingFeedback, repository::guarded_poll};
use safeflow_billing_source_reader::feedback_store::CardDisposition;
use safeflow_card_billing_feedback_adapter::{
    inspect_saved_feedback, FeedbackCardInspection, FeedbackProtectionHistory,
    FeedbackProtectionIssue, FeedbackProtectionRecord, InspectionIssue, InspectionState,
};
use safeflow_card_postgres_store::{
    catalog_inspection::ReadOnlyCatalogInspection, protection_history::CardProtectionHistoryView,
};
use safeflow_contracts::OperationId;

pub(super) struct FeedbackRetained {
    checked: CheckedBillingFeedback,
    account: ResourceId,
    proof: CardObservationProof,
    inspection: FeedbackCardInspection,
    history: Option<FeedbackProtectionHistory>,
    record: Option<FeedbackProtectionRecord>,
    review_only: bool,
}
fn inspection_error(e: InspectionIssue) -> StorageError {
    match e {
        InspectionIssue::ResourceUnavailable => StorageError::NotFound,
        InspectionIssue::Unavailable => StorageError::DependencyUnavailable,
        InspectionIssue::CardData(e) => selection_error(e),
        _ => StorageError::FactsConflict,
    }
}
fn feedback_error(e: FeedbackProtectionIssue) -> StorageError {
    match e {
        FeedbackProtectionIssue::Inspection(e) => inspection_error(e),
        FeedbackProtectionIssue::History(e) => history_error(e),
        FeedbackProtectionIssue::NotApplicable => StorageError::FactsConflict,
    }
}
fn expected_state(checked: &CheckedBillingFeedback) -> Result<InspectionState, StorageError> {
    Ok(match checked.projection()?.card_disposition() {
        CardDisposition::StopMine => InspectionState::StopMine,
        CardDisposition::StopUnsure => InspectionState::StopUnsure,
        CardDisposition::StopNoCard => InspectionState::StopNoCard,
        CardDisposition::AssociationUnverified => InspectionState::CardAssociationVerified,
    })
}
fn check_inspection(
    checked: &CheckedBillingFeedback,
    inspection: &FeedbackCardInspection,
) -> Result<(), StorageError> {
    let p = checked.projection()?;
    let r = inspection.original_for_backend();
    if r.feedback_operation_id() != p.operation_id()
        || r.owner_id() != p.owner_user_id()
        || r.report_id() != p.report_id()
        || r.report_account_ids() != p.report_account_ids()
        || r.source_id() != p.reference().source_id
        || r.account_id() != p.reference().account_id
        || r.transaction_id() != p.reference().transaction_id
        || r.original_card_id() != p.card_id()
        || r.original_snapshot_id() != p.snapshot_id()
        || r.original_rule_version() != p.rule_version()
        || r.original_calculated_at() != p.calculated_at()
        || inspection.view().state != expected_state(checked)?
    {
        return Err(StorageError::FactsConflict);
    }
    Ok(())
}
fn history_value(h: CardProtectionHistoryView) -> Result<Value, StorageError> {
    if h.total_saved_protection_records > MAX_RECORDS || h.locks.len() > MAX_RECORDS {
        return Err(StorageError::DependencyUnavailable);
    }
    Ok(
        json!({"last_four":h.last_four,"card_locked":h.card_has_active_locks,
        "account_outgoing_restricted":h.account_outgoing_restricted_by_card_locks,
        "message":h.message,"ordering_explanation":h.ordering_explanation,
        "locks":h.locks.iter().map(|l|json!({"position":l.position,"active":l.currently_active,
            "public_reason":l.public_reason,"records":l.protection_records.iter().map(|r|json!({
                "position":r.position,"original_action":r.original_action.explanation(),
                "public_reason":r.public_reason})).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
    )
}
impl RequestStep {
    /// Before every poll of the protected work (including a resumption from a
    /// nested member/provider await), re-read ALL original report accounts.
    /// After its terminal poll, re-read them once more before returning. The
    /// caller publishes synchronously, with no intervening await. Session and
    /// ticket checks also surround polls of the permission future itself.
    async fn feedback_wait<T, E>(
        &self,
        feedback: &BillingFeedbackService<'_>,
        accounts: &[String],
        wait: impl std::future::Future<Output = Result<T, E>>,
    ) -> Result<T, StorageError>
    where
        StorageError: From<E>,
    {
        use std::{future::Future, pin::Pin, task::Poll};
        let mut wait = std::pin::pin!(wait);
        let mut terminal = None;
        let mut permission: Option<
            Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + '_>>,
        > = None;
        std::future::poll_fn(|cx| loop {
            self.check()?;
            if permission.is_none() {
                permission = Some(Box::pin(feedback.recheck_report_read_access(
                    &self.session,
                    &self.binding,
                    accounts,
                )));
            }
            let authorization = permission
                .as_mut()
                .expect("permission poll state")
                .as_mut()
                .poll(cx);
            self.check()?;
            match authorization {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => permission = None,
            }
            if let Some(result) = terminal.take() {
                return Poll::Ready(result);
            }
            let result = wait.as_mut().poll(cx);
            self.check()?;
            match result {
                // Retain no permission success across the protected wait. A
                // subsequent wake must finish a NEW full-account check before
                // the member/provider future may resume.
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => terminal = Some(result.map_err(StorageError::from)),
            }
        })
        .await
    }
    async fn checked_feedback(
        &self,
        service: &BillingFeedbackService<'_>,
        op: &OperationId,
    ) -> Result<CheckedBillingFeedback, StorageError> {
        if !service.matches_navigation(&self.service.repository, &self.service.sessions) {
            return Err(StorageError::FactsConflict);
        }
        guarded_poll(&|| self.check(), service.checked_query(&self.session, op))
            .await?
            .ok_or(StorageError::NotFound)
    }
    fn publish_feedback(
        mut self,
        retained: FeedbackRetained,
        catalog: &ReadOnlyCatalogInspection,
        original: Option<Value>,
    ) -> Result<Value, StorageError> {
        self.check()?;
        let view = catalog.view();
        if view.cards.len() > MAX_CARDS {
            return Err(StorageError::DependencyUnavailable);
        }
        let association = retained.inspection.view();
        let mut history = retained
            .history
            .as_ref()
            .map(|h| history_value(h.view().card_history))
            .transpose()?;
        if retained.review_only {
            let h = history.as_mut().ok_or(StorageError::CorruptRecord)?;
            h["message"] = json!(
                "原结果需复核；以下保留上次完整清单，仅供核对，不能作为当前原结果。请勿重复冻结。"
            );
            h["ordering_explanation"] =
                json!("下列锁状态及动作属于清单保留时点；当前完整历史未重新取得，旧选择已停用。");
        }
        let scope = retained.history.as_ref().map(|h|h.view().scope_explanation)
            .unwrap_or("以下为这张卡已有的保护操作记录；本次反馈的办理结果仍须按对应原操作单独核实。查看记录不会再次冻结。");
        let state = match association.state {
            InspectionState::StopMine => "STOP_MINE",
            InspectionState::StopUnsure => "STOP_UNSURE",
            InspectionState::StopNoCard => "STOP_NO_CARD",
            InspectionState::CardAssociationVerified => "CARD_ASSOCIATION_VERIFIED",
        };
        let value = json!({"contract_version":1,"mode":"PROJECT_SIMULATION","scope":"CARD_READ_ONLY",
            "step_ref":self.ticket,"cards":[],"funds_authorized":false,
            "account_outgoing_restricted":view.account_outgoing_restricted_by_card_locks,
            "account_public_reasons":view.account_public_reasons,"unlock_instruction":view.unlock_instruction,
            "feedback":{"state":state,"message":association.message,"scope_explanation":scope},
            "history":history,"original":original});
        self.check()?;
        let mut slots = self
            .service
            .slots
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        let slot = slots
            .get_mut(&self.session)
            .ok_or(StorageError::RevisionConflict)?;
        if slot.ticket != self.ticket || slot.binding != self.binding {
            return Err(StorageError::RevisionConflict);
        }
        slot.retained = Some(Retained::Feedback(Box::new(retained)));
        self.armed = false;
        Ok(value)
    }
}
impl CardReadService {
    /// Explicit trusted backend assembly. The supplied feedback service must use
    /// this exact pool and SessionResolver, not merely matching namespace text.
    pub async fn query_feedback(
        &self,
        session: &SessionRef,
        feedback: &BillingFeedbackService<'_>,
        operation: &OperationId,
        request_ref: &str,
    ) -> Result<Value, StorageError> {
        let (step, _) = self.start(session, None, request_ref)?;
        let checked = step.checked_feedback(feedback, operation).await?;
        let accounts = checked.projection()?.report_account_ids().to_vec();
        step.feedback_wait(feedback, &accounts, async {
            let p = checked.projection()?;
            let account = ResourceId::parse(&p.reference().account_id)
                .map_err(|_| StorageError::CorruptRecord)?;
            let source = p.reference().source_id.clone();
            let proof = step.observe(&account).await?;
            let mut tx = self.repository.pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
                .execute(&mut *tx)
                .await?;
            let catalog = ReadOnlyCatalogInspection::read_on_connection(
                &mut tx,
                step.binding.user_id.as_str(),
                account.as_str(),
            )
            .await
            .map_err(selection_error)?;
            let inspection = inspect_saved_feedback(
                &self.repository.pool,
                &checked.scope,
                operation.as_str(),
                &source,
                &mut tx,
            )
            .await
            .map_err(inspection_error)?;
            check_inspection(&checked, &inspection)?;
            let history = if inspection.view().state == InspectionState::CardAssociationVerified {
                Some(
                    inspection
                        .read_card_protection_history(
                            &self.repository.pool,
                            &checked.scope,
                            &source,
                            &mut tx,
                        )
                        .await
                        .map_err(feedback_error)?,
                )
            } else {
                None
            };
            tx.commit().await?;
            let after = step.checked_feedback(feedback, operation).await?;
            if !checked.same_original(&after) || step.observe(&account).await? != proof {
                return Err(StorageError::FactsConflict);
            }
            Ok((
                FeedbackRetained {
                    checked: after,
                    account,
                    proof,
                    inspection,
                    history,
                    record: None,
                    review_only: false,
                },
                catalog,
            ))
        })
        .await
        .and_then(|(retained, catalog)| step.publish_feedback(retained, &catalog, None))
    }
    /// Existing SELECT_RECORD dispatches by the private retained object kind.
    pub async fn select_record_with_feedback(
        &self,
        session: &SessionRef,
        feedback: &BillingFeedbackService<'_>,
        previous: &str,
        position: usize,
        request_ref: &str,
    ) -> Result<Value, StorageError> {
        let (step, retained) = self.start(session, Some(previous), request_ref)?;
        let mut retained = match retained {
            Some(Retained::Feedback(r)) => *r,
            _ => return Err(StorageError::RevisionConflict),
        };
        if retained.review_only {
            return Err(StorageError::RevisionConflict);
        }
        let accounts = retained.checked.projection()?.report_account_ids().to_vec();
        let operation = retained.checked.operation.operation_id.clone();
        let (retained,catalog,original) = step.feedback_wait(feedback,&accounts,async {
            let checked = step.checked_feedback(feedback,&operation).await?;
            if !retained.checked.same_original(&checked) { return Err(StorageError::FactsConflict); }
            let proof = step.observe(&retained.account).await?;
            if !retained.proof.same_catalog(&proof) { return Err(StorageError::FactsConflict); }
            let record = retained.history.as_ref().ok_or(StorageError::RevisionConflict)?
                .select_record(Some(position)).map_err(feedback_error)?;
            let source = checked.projection()?.reference().source_id.clone();
            let mut tx = self.repository.pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY").execute(&mut *tx).await?;
            let catalog = ReadOnlyCatalogInspection::read_on_connection(&mut tx,step.binding.user_id.as_str(),retained.account.as_str()).await.map_err(selection_error)?;
            let inspection = inspect_saved_feedback(&self.repository.pool,&checked.scope,operation.as_str(),&source,&mut tx).await.map_err(inspection_error)?;
            check_inspection(&checked,&inspection)?;
            let result = record.query_current_result(&self.repository.pool,&checked.scope,&source,&mut tx).await.map_err(feedback_error)?;
            let v = result.view();
            if v.association.state != expected_state(&checked)? { return Err(StorageError::FactsConflict); }
            let needs_review = v.original_operation.state == FreezeUserState::NeedsReview;
            let original = json!({"state":match v.original_operation.state {
                FreezeUserState::OriginalRecordedActive=>"RECORDED_ACTIVE",FreezeUserState::OriginalRecordedInactive=>"RECORDED_INACTIVE",
                FreezeUserState::NeedsReview=>"NEEDS_REVIEW",FreezeUserState::Unavailable=>"UNAVAILABLE"},
                "last_four":v.original_operation.last_four,"account_outgoing_restricted":v.original_operation.account_outgoing_restricted,
                "public_reasons":v.original_operation.public_reasons,"message":v.original_operation.message,
                "original_action":v.original_action.map(|a|a.explanation())});
            // Refresh the visible current history, while querying the originally
            // selected opaque record (never reselect by position on a new list).
            let history = if needs_review { retained.history.take() } else {
                Some(inspection.read_card_protection_history(&self.repository.pool,&checked.scope,&source,&mut tx).await.map_err(feedback_error)?)
            };
            tx.commit().await?;
            let after = step.checked_feedback(feedback,&operation).await?;
            if !checked.same_original(&after) || step.observe(&retained.account).await? != proof { return Err(StorageError::FactsConflict); }
            retained.checked = after; retained.proof = proof; retained.inspection = inspection;
            retained.history = history; retained.record = Some(record); retained.review_only = needs_review;
            Ok((retained,catalog,original))
        }).await?;
        step.publish_feedback(retained, &catalog, Some(original))
    }
    /// Only trusted dispatch inspects the tag; it never exposes/constructs an
    /// opaque object or consumes the step. start() revalidates the ticket.
    pub fn is_feedback_step(
        &self,
        session: &SessionRef,
        previous: &str,
    ) -> Result<bool, StorageError> {
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        let slots = self
            .slots
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        let slot = slots.get(session).ok_or(StorageError::RevisionConflict)?;
        if slot.binding != binding || slot.ticket != previous {
            return Err(StorageError::RevisionConflict);
        }
        Ok(matches!(slot.retained, Some(Retained::Feedback(_))))
    }
}
