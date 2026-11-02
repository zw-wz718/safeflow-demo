//! Read-only presentation of current stored drafts; never a confirmation grant.
use super::*;

fn check_time(start: &str, end: &str, now: &str) -> Result<(), MockIssue> {
    let parse = |s| DateTime::parse_from_rfc3339(s).map_err(|_| MockIssue::InvalidContext);
    let time = parse(now)?;
    if time < parse(start)? {
        return Err(MockIssue::InvalidContext);
    }
    if time >= parse(end)? {
        return Err(MockIssue::DraftExpired);
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize)]
pub struct ChargePreviewMock {
    pub amount_minor_units: u64,
    pub currency: &'static str,
    pub completed_at: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RegistrationPreviewMock {
    pub fictional: bool,
    pub action: &'static str,
    pub charges: Vec<ChargePreviewMock>,
    pub owned_account_count: usize,
    pub matching_cycles: Vec<crate::recognition::Cycle>,
    pub expires_at: String,
    pub message: &'static str,
    pub service_display_name: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct IntentPreviewMock {
    pub fictional: bool,
    pub subscription_id: String,
    pub kind: MockIntentKind,
    pub before: LedgerStatus,
    pub after: LedgerStatus,
    pub expires_at: String,
    pub message: &'static str,
}
impl MemoryConfirmationMock {
    pub fn registration_preview_mock(
        &self,
        draft_id: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<RegistrationPreviewMock, MockIssue> {
        self.ready(&context.user_id)?;
        if self.completed.contains_key(&context.idempotency_key) {
            return Err(MockIssue::IdempotencyConflict);
        }
        let binding = self
            .drafts
            .get(draft_id)
            .ok_or(MockIssue::DraftUnavailable)?;
        if &binding.context != context
            || self.latest_drafts.get(&context.operation_id) != Some(&binding.draft_id)
        {
            return Err(MockIssue::PlanConflict);
        }
        if binding.ledger_revision != self.revision {
            return Err(MockIssue::StaleFacts);
        }
        check_time(&binding.as_of, &binding.expires_at, now)?;
        if self
            .ledger
            .entries
            .iter()
            .any(|entry| same_service(entry, &binding.candidate))
        {
            return Err(MockIssue::StaleFacts);
        }
        Ok(RegistrationPreviewMock {
            fictional:true,action:"REGISTER_SUBSCRIPTION",
            charges:binding.candidate.actual_charges.iter().map(|charge|ChargePreviewMock {
                amount_minor_units:charge.amount_minor_units,currency:binding.candidate.currency,completed_at:charge.completed_at.clone(),
            }).collect(),
            owned_account_count:binding.candidate.account_ids.len(),
            matching_cycles:binding.candidate.cycles.iter().filter(|c|c.status==crate::recognition::CycleStatus::Matches).map(|c|c.cycle).collect(),
            expires_at:binding.expires_at.clone(),service_display_name:None,
            message:"请核对本次所选项目及真实扣款依据；服务展示名称暂无可靠来源。确认后仅登记本地订阅，不扣款、不操作商户、不发送提醒；本预览尚未确认或登记。",
        })
    }
    pub fn intent_preview_mock(
        &self,
        draft_id: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<IntentPreviewMock, MockIssue> {
        self.ready(&context.user_id)?;
        if self.intent_completed.contains_key(&context.idempotency_key) {
            return Err(MockIssue::IdempotencyConflict);
        }
        let binding = self
            .intent_drafts
            .get(draft_id)
            .ok_or(MockIssue::DraftUnavailable)?;
        if &binding.context != context
            || self.latest_intent_drafts.get(&context.operation_id) != Some(&binding.draft_id)
        {
            return Err(MockIssue::PlanConflict);
        }
        if binding.ledger_revision != self.revision {
            return Err(MockIssue::StaleFacts);
        }
        check_time(&binding.prepared_at, &binding.expires_at, now)?;
        let target = self
            .ledger
            .entries
            .iter()
            .find(|entry| entry.subscription_id == binding.target.subscription_id)
            .ok_or(MockIssue::ResourceUnavailable)?;
        if target != &binding.target {
            return Err(MockIssue::StaleFacts);
        }
        let (before,after,message)=match binding.kind {
            MockIntentKind::Record=>(LedgerStatus::Active,LedgerStatus::Cancelling,"本次仅记录取消意图，不替你取消商户订阅、不停止扣款或退款；尚未执行，请核对后另行确认。"),
            MockIntentKind::Revoke=>(LedgerStatus::Cancelling,LedgerStatus::Active,"本次仅撤回本地取消意图，原历史保留；不表示恢复商户服务或重新签约。尚未执行，请核对后另行确认。"),
        };
        if target.status != before {
            return Err(MockIssue::StateConflict);
        }
        Ok(IntentPreviewMock {
            fictional: true,
            subscription_id: target.subscription_id.clone(),
            kind: binding.kind,
            before,
            after,
            expires_at: binding.expires_at.clone(),
            message,
        })
    }
}
