//! Local intent state transitions only; shared memory ledger, never merchant cancellation.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MockIntentKind {
    Record,
    Revoke,
}

impl MockIntentKind {
    fn before(self) -> LedgerStatus {
        match self {
            Self::Record => LedgerStatus::Active,
            Self::Revoke => LedgerStatus::Cancelling,
        }
    }
    fn after(self) -> LedgerStatus {
        match self {
            Self::Record => LedgerStatus::Cancelling,
            Self::Revoke => LedgerStatus::Active,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MockIntentBinding {
    pub minimum_level: &'static str,
    pub kind: MockIntentKind,
    pub context: MockOperationContext,
    pub draft_id: String,
    pub ledger_revision: u64,
    pub target: LedgerEntry,
    pub prepared_at: String,
    pub expires_at: String,
}

pub trait MockIntentGateVerifier {
    /// A fixed fictional harness validates this exact binding and consumes its mock confirmation once.
    /// Production must call the team's SecurityKernel; this is not authorization.
    fn verify_and_consume_mock_intent(
        &mut self,
        binding: &MockIntentBinding,
        now: &str,
    ) -> MockGateOutcome;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MockIntentReceipt {
    pub fictional: bool,
    pub production_registered: bool,
    pub operation_id: String,
    pub subscription_id: String,
    pub kind: MockIntentKind,
    pub local_intent_state: &'static str,
    pub ledger_state_at_operation: &'static str,
    pub message: &'static str,
    pub merchant_action_executed: bool,
    pub merchant_cancellation_confirmed: bool,
    pub merchant_service_restored: bool,
    pub notification_sent: bool,
    pub audit_integrated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MockIntentEvent {
    pub operation_id: String,
    pub subscription_id: String,
    pub kind: MockIntentKind,
    pub previous_status: LedgerStatus,
    pub next_status: LedgerStatus,
    pub occurred_at: String,
}

impl MemoryConfirmationMock {
    pub fn prepare_intent_mock(
        &mut self,
        context: MockOperationContext,
        subscription_id: &str,
        kind: MockIntentKind,
        now: &str,
        expires_at: &str,
    ) -> Result<MockIntentBinding, MockIssue> {
        self.ready(&context.user_id)?;
        if !valid_operation(&context) {
            return Err(MockIssue::InvalidContext);
        }
        let time = DateTime::parse_from_rfc3339(now).map_err(|_| MockIssue::InvalidContext)?;
        let expiry =
            DateTime::parse_from_rfc3339(expires_at).map_err(|_| MockIssue::InvalidContext)?;
        if expiry <= time {
            return Err(MockIssue::DraftExpired);
        }
        if self.drafts.values().any(|draft| {
            draft.context.operation_id == context.operation_id
                || draft.context.idempotency_key == context.idempotency_key
        }) || self
            .completed
            .values()
            .any(|(draft, _)| draft.context.operation_id == context.operation_id)
            || self.completed.contains_key(&context.idempotency_key)
            || self
                .intent_completed
                .values()
                .any(|(draft, _)| draft.context.operation_id == context.operation_id)
            || self.intent_completed.contains_key(&context.idempotency_key)
        {
            return Err(MockIssue::IdempotencyConflict);
        }
        if self.intent_drafts.values().any(|draft| {
            draft.context.idempotency_key == context.idempotency_key
                && draft.context.operation_id != context.operation_id
        }) {
            return Err(MockIssue::IdempotencyConflict);
        }
        let target = self
            .ledger
            .entries
            .iter()
            .find(|entry| entry.subscription_id == subscription_id)
            .ok_or(MockIssue::ResourceUnavailable)?;
        if target.status != kind.before() {
            return Err(MockIssue::StateConflict);
        }
        if let Some(id) = self.latest_intent_drafts.get(&context.operation_id) {
            let old = &self.intent_drafts[id];
            if old.context == context
                && old.kind == kind
                && &old.target == target
                && old.prepared_at == now
                && old.expires_at == expires_at
                && old.ledger_revision == self.revision
            {
                return Ok(old.clone());
            }
            if context.plan_version <= old.context.plan_version
                || context.session_ref != old.context.session_ref
                || context.task_id != old.context.task_id
                || context.node_id != old.context.node_id
                || context.idempotency_key != old.context.idempotency_key
            {
                return Err(MockIssue::PlanConflict);
            }
        }
        let next = self
            .next_draft
            .checked_add(1)
            .ok_or(MockIssue::InvalidContext)?;
        let draft_id = format!("mock-intent-draft-{next}");
        let binding = MockIntentBinding {
            minimum_level: "YELLOW",
            kind,
            context,
            draft_id: draft_id.clone(),
            ledger_revision: self.revision,
            target: target.clone(),
            prepared_at: now.into(),
            expires_at: expires_at.into(),
        };
        self.next_draft = next;
        self.latest_intent_drafts
            .insert(binding.context.operation_id.clone(), draft_id.clone());
        self.intent_drafts.insert(draft_id, binding.clone());
        Ok(binding)
    }

    pub fn commit_intent_mock(
        &mut self,
        draft_id: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockIntentGateVerifier,
    ) -> Result<MockIntentReceipt, MockIssue> {
        self.ready(&context.user_id)?;
        if self.completed.contains_key(&context.idempotency_key)
            || self.drafts.values().any(|draft| {
                draft.context.operation_id == context.operation_id
                    || draft.context.idempotency_key == context.idempotency_key
            })
        {
            return Err(MockIssue::IdempotencyConflict);
        }
        if let Some((binding, receipt)) = self.intent_completed.get(&context.idempotency_key) {
            return if binding.draft_id == draft_id && &binding.context == context {
                Ok(receipt.clone())
            } else {
                Err(MockIssue::IdempotencyConflict)
            };
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
        let time = DateTime::parse_from_rfc3339(now).map_err(|_| MockIssue::InvalidContext)?;
        let expiry = DateTime::parse_from_rfc3339(&binding.expires_at)
            .map_err(|_| MockIssue::InvalidContext)?;
        let prepared_at = DateTime::parse_from_rfc3339(&binding.prepared_at)
            .map_err(|_| MockIssue::InvalidContext)?;
        if time < prepared_at {
            return Err(MockIssue::InvalidContext);
        }
        if time >= expiry {
            return Err(MockIssue::DraftExpired);
        }
        let index = self
            .ledger
            .entries
            .iter()
            .position(|entry| entry.subscription_id == binding.target.subscription_id)
            .ok_or(MockIssue::ResourceUnavailable)?;
        if self.ledger.entries[index] != binding.target {
            return Err(MockIssue::StaleFacts);
        }
        if self.ledger.entries[index].status != binding.kind.before() {
            return Err(MockIssue::StateConflict);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(MockIssue::InconsistentLedger)?;
        match gate.verify_and_consume_mock_intent(binding, now) {
            MockGateOutcome::Confirmed => {}
            MockGateOutcome::Denied => return Err(MockIssue::ConfirmationRequired),
            MockGateOutcome::StrongAuthRequired => return Err(MockIssue::WaitingAuth),
            MockGateOutcome::Unavailable => return Err(MockIssue::GateUnavailable),
        }
        let (state, ledger_state, message) = match binding.kind {
            MockIntentKind::Record => (
                "RECORDED",
                "CANCELLING",
                "已记录本次模拟取消意图，请到商户官方渠道完成；未替您取消商户订阅。",
            ),
            MockIntentKind::Revoke => (
                "REVOKED",
                "ACTIVE",
                "已撤回本次模拟台账的取消意图；仅影响SafeFlow登记，无法替您恢复商户服务。",
            ),
        };
        let receipt = MockIntentReceipt {
            fictional: true,
            production_registered: false,
            operation_id: context.operation_id.clone(),
            subscription_id: binding.target.subscription_id.clone(),
            kind: binding.kind,
            local_intent_state: state,
            ledger_state_at_operation: ledger_state,
            message,
            merchant_action_executed: false,
            merchant_cancellation_confirmed: false,
            merchant_service_restored: false,
            notification_sent: false,
            audit_integrated: false,
        };
        self.ledger.entries[index].status = binding.kind.after();
        self.intent_events.push(MockIntentEvent {
            operation_id: context.operation_id.clone(),
            subscription_id: binding.target.subscription_id.clone(),
            kind: binding.kind,
            previous_status: binding.kind.before(),
            next_status: binding.kind.after(),
            occurred_at: now.into(),
        });
        self.revision = revision;
        self.intent_completed.insert(
            context.idempotency_key.clone(),
            (binding.clone(), receipt.clone()),
        );
        Ok(receipt)
    }

    pub fn intent_events_mock(&self, user: &str) -> Result<&[MockIntentEvent], MockIssue> {
        self.ready(user)?;
        Ok(&self.intent_events)
    }

    /// Query the immutable result of a past local operation, not the current merchant status.
    pub fn intent_result_mock(
        &self,
        user: &str,
        operation_id: &str,
    ) -> Result<&MockIntentReceipt, MockIssue> {
        self.ready(user)?;
        self.intent_completed
            .values()
            .find(|(binding, _)| binding.context.operation_id == operation_id)
            .map(|(_, receipt)| receipt)
            .ok_or(MockIssue::ResourceUnavailable)
    }
}
