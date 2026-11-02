//! In-memory confirmation over the existing managed_ledger_mock model.
//! Every identity, source and gate here is fictional. No SecurityKernel, database,
//! Tool registration, notification or merchant API is connected.
use crate::managed_ledger_mock::{
    review_mock, Disposition, LedgerEntry, LedgerFixture, LedgerReview, LedgerStatus,
};
use crate::recognition::{GroupStatus, SuspectedGroup};
use crate::{MockContext, ReadKind, Snapshot};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

mod intents;
mod previews;
pub use previews::{ChargePreviewMock, IntentPreviewMock, RegistrationPreviewMock};
#[cfg(feature = "postgres-mock-store")]
mod runtime_recovery;
#[cfg(feature = "postgres-mock-store")]
pub use runtime_recovery::{
    shared_query_error_view_mock, AbandonPendingReceiptMock, BusinessRecordCursorMock,
    BusinessRecordPageMock, CancellationHistoryItemMock, CancellationStatusViewMock,
    CandidateViewMock, CommittedReminderBatchMock, DateSuggestionViewMock, DiscoveryViewMock,
    DueReminderPreviewMock, LedgerInitializationPreviewMock, LedgerInitializationRequestMock,
    OriginalLookupAttemptMock, OriginalOperationRequestMock, OriginalOperationViewMock,
    OriginalReminderRequestMock, OriginalReminderResultMock, OriginalResultMock,
    PendingConfirmationItemMock, PendingConfirmationsMock, PendingProposedLedgerWriteMock,
    PendingRuntimeWriteMock, PreparedDiscoveryRegistrationMock, PreparedSubscriptionChangeMock,
    ProposedLedgerDiscoveryMock, ProposedLedgerReadIssue, ProposedLedgerReaderMock,
    ProposedLedgerStoreMock, ProposedLedgerWorkMock, ProposedLedgerWriteIssue,
    RawBusinessRecordMock, ReadOnlyWriteRecoveryMock, ReminderBatchStateMock,
    ReminderHistoryItemMock, ReminderPreparationMock, ReminderQueryItemViewMock,
    ReminderQueryViewMock, ReminderReadQueryMock, ReminderReviewItemMock, ReselectionResultMock,
    RuntimeRecoveryIssue, SavedDiscoverySelectionMock, SharedBusinessReadQueryMock,
    SharedBusinessRecordCursorMock, SharedBusinessRecordPageMock, SharedSubscriptionReadQueryMock,
    SharedSubscriptionWriteHandoffMock, SharedWriteCompletionViewMock, SimulatedReminderBatchMock,
    SubscriptionChangeBindingMock, SubscriptionChangeRequestMock, SubscriptionListItemMock,
    SubscriptionListViewMock, SubscriptionReadSnapshotMock, SubscriptionRuntimeMock,
    SubscriptionWriteCompletionMock, SubscriptionWriteHandoffMock, FICTIONAL_JOURNAL_ENCODING_V1,
};
#[cfg(feature = "postgres-mock-store")]
mod postgres_intents;
pub use intents::{
    MockIntentBinding, MockIntentEvent, MockIntentGateVerifier, MockIntentKind, MockIntentReceipt,
};
#[cfg(feature = "postgres-mock-store")]
pub use postgres_intents::{IntentRecoveryIssue, PostgresIntentSessionMock};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MockOperationContext {
    pub user_id: String,
    pub session_ref: String,
    pub task_id: String,
    pub node_id: String,
    pub operation_id: String,
    pub idempotency_key: String,
    pub plan_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MockConfirmationBinding {
    pub minimum_level: &'static str,
    pub context: MockOperationContext,
    pub draft_id: String,
    pub ledger_revision: u64,
    pub requested_account_ids: Vec<String>,
    pub as_of: String,
    pub expires_at: String,
    // Compare the complete immutable parameters; no weak/unstable hash.
    pub candidate: SuspectedGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockGateOutcome {
    Confirmed,
    Denied,
    StrongAuthRequired,
    Unavailable,
}

pub trait MockGateVerifier {
    /// The fixed harness checks binding, expiry and one-time consumption.
    /// NOT a production authorization API or a replacement SecurityKernel.
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
        now: &str,
    ) -> MockGateOutcome;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockIssue {
    ResourceUnavailable,
    LedgerUnavailable,
    InconsistentLedger,
    NeedsInput,
    InvalidContext,
    DraftUnavailable,
    DraftExpired,
    PlanConflict,
    StaleFacts,
    ConfirmationRequired,
    WaitingAuth,
    GateUnavailable,
    IdempotencyConflict,
    StateConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MockConfirmationReceipt {
    pub fictional: bool,
    pub production_registered: bool,
    pub operation_id: String,
    pub subscription_id: String,
    pub state: &'static str,
    pub message: &'static str,
    pub notification_sent: bool,
    pub audit_integrated: bool,
    pub merchant_action_executed: bool,
}

#[derive(Clone, Debug)]
pub struct MockConfirmedEvent {
    pub operation_id: String,
    pub subscription_id: String,
    pub occurred_at: String,
    pub evidence: SuspectedGroup,
}

#[derive(Clone)]
pub struct MemoryConfirmationMock {
    ledger: LedgerFixture,
    revision: u64,
    drafts: BTreeMap<String, MockConfirmationBinding>,
    latest_drafts: BTreeMap<String, String>,
    completed: BTreeMap<String, (MockConfirmationBinding, MockConfirmationReceipt)>,
    events: Vec<MockConfirmedEvent>,
    next_draft: u64,
    intent_drafts: BTreeMap<String, MockIntentBinding>,
    latest_intent_drafts: BTreeMap<String, String>,
    intent_completed: BTreeMap<String, (MockIntentBinding, MockIntentReceipt)>,
    intent_events: Vec<MockIntentEvent>,
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}
fn valid_operation(context: &MockOperationContext) -> bool {
    context.plan_version > 0
        && [
            &context.user_id,
            &context.session_ref,
            &context.task_id,
            &context.node_id,
            &context.operation_id,
            &context.idempotency_key,
        ]
        .iter()
        .all(|value| nonempty(value))
}
fn same_service(entry: &LedgerEntry, group: &SuspectedGroup) -> bool {
    entry.merchant_id.as_deref() == Some(group.merchant_id.as_str())
        && entry.service_ref == group.mock_service_ref
        && group.actual_charges.iter().any(|charge| {
            charge
                .source_key
                .as_ref()
                .is_some_and(|key| Some(&key.source_id) == entry.source_id.as_ref())
        })
}

pub fn public_review_message(reason: &str) -> &'static str {
    match reason {
        "CANCELLATION_INTENT_NEEDS_REVIEW" => {
            "已登记取消意图，发现相关扣款记录，请核对商户处理情况；未确认商户已取消。"
        }
        "REGISTERED_ERROR_NEEDS_REVIEW" => {
            "已有登记记录异常，需要核对；消费证据保留，未自动再登记。"
        }
        "MULTIPLE_CHARGES_NEED_REVIEW" => "有多笔扣款，请核对；全部保留，不擅自挑一笔凑规律。",
        _ => "已有登记或服务对应关系需要核对，不自动登记或操作商户。",
    }
}

impl MemoryConfirmationMock {
    pub fn from_fixture(ledger: LedgerFixture) -> Self {
        Self {
            ledger,
            revision: 0,
            drafts: BTreeMap::new(),
            latest_drafts: BTreeMap::new(),
            completed: BTreeMap::new(),
            events: vec![],
            next_draft: 0,
            intent_drafts: BTreeMap::new(),
            latest_intent_drafts: BTreeMap::new(),
            intent_completed: BTreeMap::new(),
            intent_events: vec![],
        }
    }

    fn ready(&self, user: &str) -> Result<(), MockIssue> {
        if !nonempty(user)
            || self.ledger.owner_user_id != user
            || self.ledger.entries.iter().any(|e| e.owner_user_id != user)
        {
            return Err(MockIssue::ResourceUnavailable);
        }
        if self.ledger.schema_version != 1
            || !self.ledger.fictional
            || self.ledger.production_registered
        {
            return Err(MockIssue::InconsistentLedger);
        }
        let read = &self.ledger.read;
        if read.state != ReadKind::Complete
            || !read.is_complete
            || read.next_cursor.is_some()
            || read.reason.is_some()
            || self
                .ledger
                .snapshot_id
                .as_deref()
                .is_none_or(|s| !nonempty(s))
        {
            return Err(MockIssue::LedgerUnavailable);
        }
        let mut ids = HashSet::new();
        if self.ledger.entries.iter().any(|entry| {
            !nonempty(&entry.subscription_id)
                || !ids.insert(&entry.subscription_id)
                || [&entry.source_id, &entry.merchant_id, &entry.service_ref]
                    .iter()
                    .any(|value| value.as_deref().is_some_and(|s| !nonempty(s)))
        }) {
            return Err(MockIssue::InconsistentLedger);
        }
        Ok(())
    }

    pub fn list_mock(&self, user: &str) -> Result<&[LedgerEntry], MockIssue> {
        self.ready(user)?;
        Ok(&self.ledger.entries)
    }
    pub(crate) fn reminder_revision_mock(&self, user: &str) -> Result<u64, MockIssue> {
        self.ready(user)?;
        Ok(self.revision)
    }
    pub fn events_mock(&self, user: &str) -> Result<&[MockConfirmedEvent], MockIssue> {
        self.ready(user)?;
        Ok(&self.events)
    }

    /// Read the immutable first registration result without repeating a write
    /// or consuming confirmation. The receipt's ACTIVE state is historical;
    /// current status must be obtained from list_mock separately.
    pub fn confirmation_result_mock(
        &self,
        user: &str,
        operation_id: &str,
    ) -> Result<&MockConfirmationReceipt, MockIssue> {
        self.ready(user)?;
        self.completed
            .values()
            .find(|(binding, _)| binding.context.operation_id == operation_id)
            .map(|(_, receipt)| receipt)
            .ok_or(MockIssue::ResourceUnavailable)
    }

    /// Current user choices layered over the historical read-only replay.
    /// The old review_mock API/9-case golden replay remains unchanged.
    pub fn discover_mock(
        &self,
        context: &MockContext,
        snapshot: &Snapshot,
    ) -> Result<LedgerReview, MockIssue> {
        self.ready(&context.user_id)?;
        let mut review = review_mock(context, snapshot, &self.ledger);
        for item in &mut review.groups {
            if self.ledger.entries.iter().any(|entry| {
                entry.status == LedgerStatus::Cancelling && same_service(entry, &item.group)
            }) {
                item.disposition = Disposition::NeedsReview;
                item.reason = "CANCELLATION_INTENT_NEEDS_REVIEW";
            } else if item.disposition == Disposition::ServiceBlocked {
                item.disposition = Disposition::NeedsReview;
                item.reason = "REGISTERED_ERROR_NEEDS_REVIEW";
            } else if item.disposition == Disposition::AlreadyManaged
                && self
                    .ledger
                    .entries
                    .iter()
                    .filter(|entry| same_service(entry, &item.group))
                    .count()
                    > 1
            {
                item.disposition = Disposition::NeedsReview;
                item.reason = "MULTIPLE_LEDGER_RECORDS_NEED_REVIEW";
            } else if item.disposition == Disposition::AlreadyManaged
                && item.group.status == GroupStatus::NeedsReview
            {
                item.disposition = Disposition::NeedsReview;
                item.reason = "MULTIPLE_CHARGES_NEED_REVIEW";
            }
            let sources: HashSet<_> = item
                .group
                .actual_charges
                .iter()
                .filter_map(|c| c.source_key.as_ref().map(|k| k.source_id.as_str()))
                .collect();
            if item.disposition == Disposition::NotInLedger && sources.len() != 1 {
                item.disposition = Disposition::NeedsReview;
                item.reason = "CROSS_SOURCE_IDENTITY_NEEDS_REVIEW";
            }
        }
        Ok(review)
    }

    pub fn prepare_mock(
        &mut self,
        context: &MockContext,
        snapshot: &Snapshot,
        operation: MockOperationContext,
        group_index: usize,
        expires_at: &str,
    ) -> Result<MockConfirmationBinding, MockIssue> {
        self.ready(&operation.user_id)?;
        if operation.user_id != context.user_id || !valid_operation(&operation) {
            return Err(MockIssue::InvalidContext);
        }
        let now = DateTime::parse_from_rfc3339(&snapshot.window.as_of)
            .map_err(|_| MockIssue::InvalidContext)?;
        let expiry =
            DateTime::parse_from_rfc3339(expires_at).map_err(|_| MockIssue::InvalidContext)?;
        if expiry <= now {
            return Err(MockIssue::DraftExpired);
        }
        if self.completed.contains_key(&operation.idempotency_key) {
            return Err(MockIssue::IdempotencyConflict);
        }
        if self.intent_drafts.values().any(|draft| {
            draft.context.operation_id == operation.operation_id
                || draft.context.idempotency_key == operation.idempotency_key
        }) || self
            .intent_completed
            .values()
            .any(|(draft, _)| draft.context.operation_id == operation.operation_id)
            || self
                .intent_completed
                .contains_key(&operation.idempotency_key)
        {
            return Err(MockIssue::IdempotencyConflict);
        }
        let review = self.discover_mock(context, snapshot)?;
        let item = review
            .groups
            .get(group_index)
            .ok_or(MockIssue::NeedsInput)?;
        if !review.matching_executed
            || item.disposition != Disposition::NotInLedger
            || item.group.status != GroupStatus::Suspected
        {
            return Err(MockIssue::NeedsInput);
        }
        let mut accounts = context.requested_account_ids.clone();
        accounts.sort();
        if let Some(id) = self.latest_drafts.get(&operation.operation_id) {
            let old = &self.drafts[id];
            if old.context == operation
                && old.candidate == item.group
                && old.as_of == snapshot.window.as_of
                && old.expires_at == expires_at
                && old.requested_account_ids == accounts
                && old.ledger_revision == self.revision
            {
                return Ok(old.clone());
            }
            if operation.plan_version <= old.context.plan_version
                || operation.session_ref != old.context.session_ref
                || operation.task_id != old.context.task_id
                || operation.node_id != old.context.node_id
                || operation.idempotency_key != old.context.idempotency_key
            {
                return Err(MockIssue::PlanConflict);
            }
        }
        let next = self
            .next_draft
            .checked_add(1)
            .ok_or(MockIssue::InvalidContext)?;
        let draft_id = format!("mock-sub-draft-{next}");
        let binding = MockConfirmationBinding {
            minimum_level: "YELLOW",
            context: operation,
            draft_id: draft_id.clone(),
            ledger_revision: self.revision,
            requested_account_ids: accounts,
            as_of: snapshot.window.as_of.clone(),
            expires_at: expires_at.into(),
            candidate: item.group.clone(),
        };
        self.next_draft = next;
        self.latest_drafts
            .insert(binding.context.operation_id.clone(), draft_id.clone());
        self.drafts.insert(draft_id, binding.clone());
        Ok(binding)
    }

    pub fn commit_mock(
        &mut self,
        draft_id: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockGateVerifier,
    ) -> Result<MockConfirmationReceipt, MockIssue> {
        self.ready(&context.user_id)?;
        if self.intent_completed.contains_key(&context.idempotency_key)
            || self.intent_drafts.values().any(|draft| {
                draft.context.operation_id == context.operation_id
                    || draft.context.idempotency_key == context.idempotency_key
            })
        {
            return Err(MockIssue::IdempotencyConflict);
        }
        if let Some((binding, receipt)) = self.completed.get(&context.idempotency_key) {
            return if binding.draft_id == draft_id && &binding.context == context {
                Ok(receipt.clone())
            } else {
                Err(MockIssue::IdempotencyConflict)
            };
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
        let time = DateTime::parse_from_rfc3339(now).map_err(|_| MockIssue::InvalidContext)?;
        let expiry = DateTime::parse_from_rfc3339(&binding.expires_at)
            .map_err(|_| MockIssue::InvalidContext)?;
        let prepared_at =
            DateTime::parse_from_rfc3339(&binding.as_of).map_err(|_| MockIssue::InvalidContext)?;
        if time < prepared_at {
            return Err(MockIssue::InvalidContext);
        }
        if time >= expiry {
            return Err(MockIssue::DraftExpired);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(MockIssue::InconsistentLedger)?;
        let subscription_id = format!("mock-sub-{}-{revision}", binding.draft_id);
        if self.ledger.entries.iter().any(|entry| {
            entry.subscription_id == subscription_id || same_service(entry, &binding.candidate)
        }) {
            return Err(MockIssue::StaleFacts);
        }
        let source = binding
            .candidate
            .actual_charges
            .first()
            .and_then(|c| c.source_key.as_ref())
            .ok_or(MockIssue::NeedsInput)?
            .source_id
            .clone();
        // All fallible business checks precede mock one-time confirmation consumption.
        match gate.verify_and_consume_mock_confirmation(binding, now) {
            MockGateOutcome::Confirmed => {}
            MockGateOutcome::Denied => return Err(MockIssue::ConfirmationRequired),
            MockGateOutcome::StrongAuthRequired => return Err(MockIssue::WaitingAuth),
            MockGateOutcome::Unavailable => return Err(MockIssue::GateUnavailable),
        }
        let receipt = MockConfirmationReceipt {
            fictional: true,
            production_registered: false,
            operation_id: context.operation_id.clone(),
            subscription_id: subscription_id.clone(),
            state: "ACTIVE",
            message: "已写入本次模拟台账；未接入正式系统，未发提醒或操作商户。",
            notification_sent: false,
            audit_integrated: false,
            merchant_action_executed: false,
        };
        self.ledger.entries.push(LedgerEntry {
            subscription_id: subscription_id.clone(),
            owner_user_id: context.user_id.clone(),
            source_id: Some(source),
            merchant_id: Some(binding.candidate.merchant_id.clone()),
            service_ref: binding.candidate.mock_service_ref.clone(),
            status: LedgerStatus::Active,
        });
        self.events.push(MockConfirmedEvent {
            operation_id: context.operation_id.clone(),
            subscription_id,
            occurred_at: now.into(),
            evidence: binding.candidate.clone(),
        });
        self.revision = revision;
        self.completed.insert(
            context.idempotency_key.clone(),
            (binding.clone(), receipt.clone()),
        );
        Ok(receipt)
    }
}
