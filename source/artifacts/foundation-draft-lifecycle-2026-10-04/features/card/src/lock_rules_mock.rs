//! CARD-02 isolated rules over explicitly fictional data.
//!
//! These types describe simulated sources; they do not authenticate a caller,
//! issue a permit, implement a Tool/Policy, or freeze a bank card. The pure
//! function returns a new in-memory value. No network, file or database is used.
//! New operations against released history require coordination; release and
//! reopening are not modeled. Retries replay the original operation fact and
//! make no claim about the current effectiveness of its lock.

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockUser {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockAccount {
    pub id: String,
    pub user_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockCard {
    pub id: String,
    pub account_id: String,
    pub last_four: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockTransaction {
    pub id: String,
    pub user_id: String,
    pub card_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockFeedback {
    pub id: String,
    pub user_id: String,
    pub transaction_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockEventKind {
    Anomaly {
        transaction_id: String,
        feedback_id: String,
    },
    RiskControl,
}

/// A record in the fictional fixture, never caller-supplied proof of authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockTrustedEvent {
    pub id: String,
    pub user_id: String,
    pub card_id: Option<String>,
    pub kind: MockEventKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MockLockSource {
    UserSelf,
    AnomalyDispute,
    RiskControl,
}

impl MockLockSource {
    pub fn mock_public_reason(self) -> &'static str {
        match self {
            Self::UserSelf => "卡片丢失／安全顾虑",
            Self::AnomalyDispute => "针对您反馈的交易采取保护",
            Self::RiskControl => "另有安全限制，请联系人工客服",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockReasonCode {
    UserReportLost,
    UserSecurityConcern,
    NonOwnerTransaction,
    RiskControlFlag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockLockStatus {
    Active,
    Released,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockLock {
    pub id: String,
    pub card_id: String,
    pub source: MockLockSource,
    pub event_id: Option<String>,
    pub status: MockLockStatus,
}

/// Source labels for test scenarios only, not authentication or authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockPrincipal {
    User { user_id: String },
    AnomalyChain { user_id: String },
    RiskEngine,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockRequest {
    pub user_id: String,
    /// A stable card ID is required. Last-four digits are never a selector.
    pub card_id: Option<String>,
    pub operation_id: String,
    pub idempotency_key: String,
    pub principal: MockPrincipal,
    pub source: MockLockSource,
    pub reason: MockReasonCode,
    pub event_id: Option<String>,
    pub transaction_id: Option<String>,
    pub feedback_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockLockEffect {
    Created,
    ReusedActive,
}

/// A simulated receipt contains the original operation fact and public reason,
/// not internal event details or current lock effectiveness. It makes no claim
/// about a transfer, renewal, or real card status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockReceipt {
    pub lock_id: String,
    pub operation_record_id: String,
    pub reason_history_id: String,
    pub source: MockLockSource,
    pub public_reason: &'static str,
    pub effect: MockLockEffect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockOperation {
    pub id: String,
    pub request: MockRequest,
    /// Additional retry keys belong to this same operation, never a new lock.
    pub idempotency_keys: Vec<String>,
    pub receipt: MockReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockReasonHistory {
    pub id: String,
    pub lock_id: String,
    pub operation_record_id: String,
    pub reason: MockReasonCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockCompleteness {
    Complete,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockState {
    pub completeness: MockCompleteness,
    pub users: Vec<MockUser>,
    pub accounts: Vec<MockAccount>,
    pub cards: Vec<MockCard>,
    pub transactions: Vec<MockTransaction>,
    pub feedback: Vec<MockFeedback>,
    pub events: Vec<MockTrustedEvent>,
    pub locks: Vec<MockLock>,
    pub reason_history: Vec<MockReasonHistory>,
    pub operations: Vec<MockOperation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockOutcome {
    Simulated,
    Replayed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockSimulation {
    pub next_state: MockState,
    pub receipt: MockReceipt,
    pub outcome: MockOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockIssue {
    IncompleteData,
    InconsistentData,
    MissingCardReference,
    /// An absent card and another person's card have the same outward error.
    ResourceUnavailable,
    InvalidRequest,
    SourceNotAllowed,
    EvidenceUnavailable,
    Conflict,
    NeedsCoordination,
}

/// Simulate one request. The input state and request are never changed.
/// Current ownership and source evidence are checked before an earlier receipt
/// can be replayed. Error results carry no previous lock ID or receipt.
pub fn simulate_lock_request(
    state: &MockState,
    request: &MockRequest,
) -> Result<MockSimulation, MockIssue> {
    mock_validate_state(state)?;
    if !mock_nonempty(&request.user_id)
        || !mock_nonempty(&request.operation_id)
        || !mock_nonempty(&request.idempotency_key)
    {
        return Err(MockIssue::InvalidRequest);
    }
    let card_id = request
        .card_id
        .as_deref()
        .filter(|id| mock_nonempty(id))
        .ok_or(MockIssue::MissingCardReference)?;
    if !mock_owned_card(state, &request.user_id, card_id) {
        return Err(MockIssue::ResourceUnavailable);
    }
    mock_validate_source(state, request, card_id)?;

    let operation = state.operations.iter().find(|record| {
        record.request.user_id == request.user_id
            && record.request.operation_id == request.operation_id
    });
    let keyed = state.operations.iter().find(|record| {
        record.request.user_id == request.user_id
            && record.idempotency_keys.contains(&request.idempotency_key)
    });
    for previous in [operation, keyed].into_iter().flatten() {
        if !mock_same_parameters(&previous.request, request) {
            return Err(MockIssue::Conflict);
        }
    }
    if let Some(previous) = operation.or(keyed) {
        let mut next_state = state.clone();
        if !previous.idempotency_keys.contains(&request.idempotency_key) {
            let next_operation = next_state
                .operations
                .iter_mut()
                .find(|record| record.id == previous.id)
                .ok_or(MockIssue::InconsistentData)?;
            next_operation
                .idempotency_keys
                .push(request.idempotency_key.clone());
        }
        return Ok(MockSimulation {
            next_state,
            receipt: previous.receipt.clone(),
            outcome: MockOutcome::Replayed,
        });
    }

    let matching: Vec<_> = state
        .locks
        .iter()
        .filter(|lock| {
            lock.card_id == card_id
                && lock.source == request.source
                && (request.source == MockLockSource::UserSelf
                    || lock.event_id == request.event_id)
        })
        .collect();
    let active = matching
        .iter()
        .find(|lock| lock.status == MockLockStatus::Active);
    if active.is_none()
        && matching
            .iter()
            .any(|lock| lock.status == MockLockStatus::Released)
    {
        return Err(MockIssue::NeedsCoordination);
    }
    let effect = if active.is_some() {
        MockLockEffect::ReusedActive
    } else {
        MockLockEffect::Created
    };
    let lock_id = active
        .map(|lock| lock.id.clone())
        .unwrap_or_else(|| mock_generated_id("lock", request));
    let operation_record_id = mock_generated_id("operation", request);
    let reason_history_id = mock_generated_id("reason", request);
    if (active.is_none() && state.locks.iter().any(|lock| lock.id == lock_id))
        || state
            .operations
            .iter()
            .any(|record| record.id == operation_record_id)
        || state
            .reason_history
            .iter()
            .any(|reason| reason.id == reason_history_id)
    {
        return Err(MockIssue::InconsistentData);
    }

    let receipt = MockReceipt {
        lock_id: lock_id.clone(),
        operation_record_id: operation_record_id.clone(),
        reason_history_id: reason_history_id.clone(),
        source: request.source,
        public_reason: request.source.mock_public_reason(),
        effect,
    };
    let mut next_state = state.clone();
    if active.is_none() {
        next_state.locks.push(MockLock {
            id: lock_id.clone(),
            card_id: card_id.to_owned(),
            source: request.source,
            event_id: request.event_id.clone(),
            status: MockLockStatus::Active,
        });
    }
    next_state.reason_history.push(MockReasonHistory {
        id: reason_history_id,
        lock_id,
        operation_record_id: operation_record_id.clone(),
        reason: request.reason,
    });
    next_state.operations.push(MockOperation {
        id: operation_record_id,
        request: request.clone(),
        idempotency_keys: vec![request.idempotency_key.clone()],
        receipt: receipt.clone(),
    });
    Ok(MockSimulation {
        next_state,
        receipt,
        outcome: MockOutcome::Simulated,
    })
}

fn mock_nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn mock_unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> bool {
    let mut unique = HashSet::new();
    ids.into_iter()
        .all(|id| mock_nonempty(id) && unique.insert(id))
}

fn mock_owned_card(state: &MockState, user_id: &str, card_id: &str) -> bool {
    state.users.iter().any(|user| user.id == user_id)
        && state.cards.iter().any(|card| {
            card.id == card_id
                && state.accounts.iter().any(|account| {
                    account.id == card.account_id && account.user_id == user_id
                })
        })
}

fn mock_reason_matches(source: MockLockSource, reason: MockReasonCode) -> bool {
    matches!(
        (source, reason),
        (MockLockSource::UserSelf, MockReasonCode::UserReportLost)
            | (MockLockSource::UserSelf, MockReasonCode::UserSecurityConcern)
            | (
                MockLockSource::AnomalyDispute,
                MockReasonCode::NonOwnerTransaction
            )
            | (MockLockSource::RiskControl, MockReasonCode::RiskControlFlag)
    )
}

fn mock_validate_source(
    state: &MockState,
    request: &MockRequest,
    card_id: &str,
) -> Result<(), MockIssue> {
    if !mock_reason_matches(request.source, request.reason) {
        return Err(MockIssue::InvalidRequest);
    }
    match (&request.principal, request.source) {
        (MockPrincipal::User { user_id }, MockLockSource::UserSelf)
            if user_id == &request.user_id =>
        {
            if request.event_id.is_some()
                || request.transaction_id.is_some()
                || request.feedback_id.is_some()
            {
                return Err(MockIssue::InvalidRequest);
            }
            Ok(())
        }
        (MockPrincipal::AnomalyChain { user_id }, MockLockSource::AnomalyDispute)
            if user_id == &request.user_id =>
        {
            let event = mock_request_event(state, request, card_id)?;
            let (transaction_id, feedback_id) = match &event.kind {
                MockEventKind::Anomaly {
                    transaction_id,
                    feedback_id,
                } => (transaction_id, feedback_id),
                MockEventKind::RiskControl => return Err(MockIssue::EvidenceUnavailable),
            };
            if request.transaction_id.as_ref() != Some(transaction_id)
                || request.feedback_id.as_ref() != Some(feedback_id)
            {
                return Err(MockIssue::EvidenceUnavailable);
            }
            if mock_event_evidence_matches(state, event, card_id) {
                Ok(())
            } else {
                Err(MockIssue::EvidenceUnavailable)
            }
        }
        (MockPrincipal::RiskEngine, MockLockSource::RiskControl) => {
            let event = mock_request_event(state, request, card_id)?;
            if event.kind != MockEventKind::RiskControl
                || request.transaction_id.is_some()
                || request.feedback_id.is_some()
            {
                return Err(MockIssue::EvidenceUnavailable);
            }
            Ok(())
        }
        _ => Err(MockIssue::SourceNotAllowed),
    }
}

fn mock_request_event<'a>(
    state: &'a MockState,
    request: &MockRequest,
    card_id: &str,
) -> Result<&'a MockTrustedEvent, MockIssue> {
    state
        .events
        .iter()
        .find(|event| {
            Some(&event.id) == request.event_id.as_ref()
                && event.user_id == request.user_id
                && event.card_id.as_deref() == Some(card_id)
        })
        .ok_or(MockIssue::EvidenceUnavailable)
}

fn mock_event_evidence_matches(
    state: &MockState,
    event: &MockTrustedEvent,
    card_id: &str,
) -> bool {
    match &event.kind {
        MockEventKind::RiskControl => true,
        MockEventKind::Anomaly { transaction_id, feedback_id } => {
            state.transactions.iter().any(|transaction| {
                transaction.id == *transaction_id
                    && transaction.user_id == event.user_id
                    && transaction.card_id.as_deref() == Some(card_id)
            }) && state.feedback.iter().any(|feedback| {
                feedback.id == *feedback_id
                    && feedback.user_id == event.user_id
                    && feedback.transaction_id == *transaction_id
            })
        }
    }
}

fn mock_same_parameters(left: &MockRequest, right: &MockRequest) -> bool {
    left.user_id == right.user_id
        && left.card_id == right.card_id
        && left.operation_id == right.operation_id
        && left.principal == right.principal
        && left.source == right.source
        && left.reason == right.reason
        && left.event_id == right.event_id
        && left.transaction_id == right.transaction_id
        && left.feedback_id == right.feedback_id
}

fn mock_generated_id(kind: &str, request: &MockRequest) -> String {
    // Length-prefixed parts avoid ambiguity if fixture IDs contain separators.
    format!(
        "Mock-{}-{}:{}-{}:{}",
        kind,
        request.user_id.len(),
        request.user_id,
        request.operation_id.len(),
        request.operation_id
    )
}

pub(crate) fn mock_validate_state(state: &MockState) -> Result<(), MockIssue> {
    if state.completeness != MockCompleteness::Complete {
        return Err(MockIssue::IncompleteData);
    }
    let unique = mock_unique_ids(state.users.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.accounts.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.cards.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.transactions.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.feedback.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.events.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.locks.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.reason_history.iter().map(|item| item.id.as_str()))
        && mock_unique_ids(state.operations.iter().map(|item| item.id.as_str()));
    if !unique {
        return Err(MockIssue::InconsistentData);
    }
    if state.accounts.iter().any(|account| {
        !state.users.iter().any(|user| user.id == account.user_id)
    }) || state.cards.iter().any(|card| {
        !state
            .accounts
            .iter()
            .any(|account| account.id == card.account_id)
            || card.last_four.len() != 4
            || !card.last_four.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return Err(MockIssue::InconsistentData);
    }
    let mut active_scopes = HashSet::new();
    for lock in &state.locks {
        let card = state
            .cards
            .iter()
            .find(|card| card.id == lock.card_id)
            .ok_or(MockIssue::InconsistentData)?;
        if lock.source == MockLockSource::UserSelf {
            if lock.event_id.is_some() {
                return Err(MockIssue::InconsistentData);
            }
        } else {
            let event = state
                .events
                .iter()
                .find(|event| {
                    Some(&event.id) == lock.event_id.as_ref()
                        && event.card_id.as_ref() == Some(&lock.card_id)
                        && mock_owned_card(state, &event.user_id, &card.id)
                })
                .ok_or(MockIssue::InconsistentData)?;
            if !matches!(
                (&event.kind, lock.source),
                (MockEventKind::Anomaly { .. }, MockLockSource::AnomalyDispute)
                    | (MockEventKind::RiskControl, MockLockSource::RiskControl)
            ) || !mock_event_evidence_matches(state, event, &lock.card_id) {
                return Err(MockIssue::InconsistentData);
            }
        }
        if lock.status == MockLockStatus::Active
            && !active_scopes.insert((&lock.card_id, lock.source, &lock.event_id))
        {
            return Err(MockIssue::InconsistentData);
        }
    }
    let mut operation_scopes = HashSet::new();
    let mut key_scopes = HashSet::new();
    for operation in &state.operations {
        let request = &operation.request;
        if !mock_nonempty(&request.user_id)
            || !mock_nonempty(&request.operation_id)
            || !mock_reason_matches(request.source, request.reason)
            || !operation_scopes.insert((&request.user_id, &request.operation_id))
            || !operation.idempotency_keys.contains(&request.idempotency_key)
            || operation.receipt.operation_record_id != operation.id
            || operation.receipt.source != request.source
            || operation.receipt.public_reason != request.source.mock_public_reason()
        {
            return Err(MockIssue::InconsistentData);
        }
        for key in &operation.idempotency_keys {
            if !mock_nonempty(key) || !key_scopes.insert((&request.user_id, key)) {
                return Err(MockIssue::InconsistentData);
            }
        }
        let matching_lock = state.locks.iter().find(|lock| {
            lock.id == operation.receipt.lock_id
                && request.card_id.as_ref() == Some(&lock.card_id)
                && lock.source == request.source
                && lock.event_id == request.event_id
        });
        let reason_matches = state.reason_history.iter().any(|reason| {
            reason.id == operation.receipt.reason_history_id
                && reason.lock_id == operation.receipt.lock_id
                && reason.operation_record_id == operation.id
                && reason.reason == request.reason
        });
        let Some(lock) = matching_lock else {
            return Err(MockIssue::InconsistentData);
        };
        if !reason_matches || mock_validate_source(state, request, &lock.card_id).is_err() {
            return Err(MockIssue::InconsistentData);
        }
    }
    for reason in &state.reason_history {
        if !state.operations.iter().any(|operation| {
            operation.id == reason.operation_record_id
                && operation.receipt.reason_history_id == reason.id
                && operation.receipt.lock_id == reason.lock_id
                && operation.request.reason == reason.reason
        }) {
            return Err(MockIssue::InconsistentData);
        }
    }
    // A snapshot marked Complete must retain the operation/reason history for
    // every lock; a bare seed lock must not silently lose its original reason.
    if state.locks.iter().any(|lock| {
        !state.operations.iter().any(|operation| operation.receipt.lock_id == lock.id)
    }) {
        return Err(MockIssue::InconsistentData);
    }
    Ok(())
}

/// Fixed fictional users, accounts, cards and evidence for tests and demos.
/// This fixture has no relation to a signed-in user or a real bank database.
pub fn simulated_sample_state() -> MockState {
    MockState {
        completeness: MockCompleteness::Complete,
        users: vec![MockUser { id: "U1".into() }, MockUser { id: "U2".into() }],
        accounts: vec![
            MockAccount { id: "A".into(), user_id: "U1".into() },
            MockAccount { id: "B".into(), user_id: "U2".into() },
        ],
        cards: vec![
            MockCard { id: "K".into(), account_id: "A".into(), last_four: "1234".into() },
            MockCard { id: "L".into(), account_id: "B".into(), last_four: "1234".into() },
        ],
        transactions: vec![
            MockTransaction { id: "T1".into(), user_id: "U1".into(), card_id: Some("K".into()) },
            MockTransaction { id: "T2".into(), user_id: "U1".into(), card_id: Some("K".into()) },
        ],
        feedback: vec![
            MockFeedback { id: "F1".into(), user_id: "U1".into(), transaction_id: "T1".into() },
            MockFeedback { id: "F2".into(), user_id: "U1".into(), transaction_id: "T2".into() },
        ],
        events: vec![
            MockTrustedEvent { id: "E1".into(), user_id: "U1".into(), card_id: Some("K".into()), kind: MockEventKind::Anomaly { transaction_id: "T1".into(), feedback_id: "F1".into() } },
            MockTrustedEvent { id: "E2".into(), user_id: "U1".into(), card_id: Some("K".into()), kind: MockEventKind::Anomaly { transaction_id: "T2".into(), feedback_id: "F2".into() } },
            MockTrustedEvent { id: "R1".into(), user_id: "U1".into(), card_id: Some("K".into()), kind: MockEventKind::RiskControl },
            MockTrustedEvent { id: "R2".into(), user_id: "U1".into(), card_id: Some("K".into()), kind: MockEventKind::RiskControl },
        ],
        locks: Vec::new(),
        reason_history: Vec::new(),
        operations: Vec::new(),
    }
}
