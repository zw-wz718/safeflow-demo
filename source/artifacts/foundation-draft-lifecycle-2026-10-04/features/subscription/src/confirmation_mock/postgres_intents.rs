//! Committed cancellation/revocation recovery in a dedicated fictional DB.
//! Not registration/reminder persistence, real authorization, or Agent assembly.
use super::*;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentRecoveryIssue {
    Unavailable,
    NotFound,
    Conflict,
    InvalidCheckpoint,
    Business(MockIssue),
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletedIntent {
    context: MockOperationContext,
    draft_id: String,
    ledger_revision: u64,
    target: LedgerEntry,
    kind: MockIntentKind,
    prepared_at: String,
    expires_at: String,
    receipt: Value,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema_version: u32,
    fictional: bool,
    production_registered: bool,
    ledger: LedgerFixture,
    revision: u64,
    next_draft: u64,
    completed: Vec<CompletedIntent>,
    events: Vec<MockIntentEvent>,
}
fn invalid<T>() -> Result<T, IntentRecoveryIssue> {
    Err(IntentRecoveryIssue::InvalidCheckpoint)
}
fn checkpoint(memory: &MemoryConfirmationMock, user: &str) -> Result<Value, IntentRecoveryIssue> {
    memory.ready(user).map_err(IntentRecoveryIssue::Business)?;
    // This narrow repository must never silently discard registration state.
    if !memory.drafts.is_empty() || !memory.completed.is_empty() || !memory.events.is_empty() {
        return invalid();
    }
    let mut completed = Vec::new();
    for event in &memory.intent_events {
        let (b, receipt) = memory
            .intent_completed
            .values()
            .find(|(b, _)| b.context.operation_id == event.operation_id)
            .ok_or(IntentRecoveryIssue::InvalidCheckpoint)?;
        completed.push(CompletedIntent {
            context: b.context.clone(),
            draft_id: b.draft_id.clone(),
            ledger_revision: b.ledger_revision,
            target: b.target.clone(),
            kind: b.kind,
            prepared_at: b.prepared_at.clone(),
            expires_at: b.expires_at.clone(),
            receipt: serde_json::to_value(receipt)
                .map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)?,
        });
    }
    serde_json::to_value(Checkpoint {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        ledger: memory.ledger.clone(),
        revision: memory.revision,
        next_draft: memory.next_draft,
        completed,
        events: memory.intent_events.clone(),
    })
    .map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)
}
fn restore(value: Value, user: &str) -> Result<MemoryConfirmationMock, IntentRecoveryIssue> {
    let state: Checkpoint =
        serde_json::from_value(value).map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)?;
    if state.schema_version != 1
        || !state.fictional
        || state.production_registered
        || state.events.len() != state.completed.len()
        || state.revision != state.events.len() as u64
    {
        return invalid();
    }
    MemoryConfirmationMock::from_fixture(state.ledger.clone())
        .ready(user)
        .map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)?;
    // Reconstruct initial rows from the complete event chain, then validate each
    // historical transition with the existing business rules, without any sender.
    let mut initial = state.ledger.clone();
    for (event, completed) in state.events.iter().zip(&state.completed).rev() {
        let row = initial
            .entries
            .iter_mut()
            .find(|e| e.subscription_id == event.subscription_id)
            .ok_or(IntentRecoveryIssue::InvalidCheckpoint)?;
        if event.operation_id != completed.context.operation_id
            || event.subscription_id != completed.target.subscription_id
            || event.kind != completed.kind
            || row.status != event.next_status
        {
            return invalid();
        }
        row.status = event.previous_status;
        if *row != completed.target {
            return invalid();
        }
    }
    let mut memory = MemoryConfirmationMock::from_fixture(initial);
    struct HistoricalMockGate(MockIntentBinding);
    impl MockIntentGateVerifier for HistoricalMockGate {
        fn verify_and_consume_mock_intent(
            &mut self,
            binding: &MockIntentBinding,
            _: &str,
        ) -> MockGateOutcome {
            if binding == &self.0 {
                MockGateOutcome::Confirmed
            } else {
                MockGateOutcome::Denied
            }
        }
    }
    for (event, completed) in state.events.iter().zip(&state.completed) {
        let counter = completed
            .draft_id
            .strip_prefix("mock-intent-draft-")
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > memory.next_draft && *n <= state.next_draft)
            .ok_or(IntentRecoveryIssue::InvalidCheckpoint)?;
        memory.next_draft = counter - 1;
        let binding = memory
            .prepare_intent_mock(
                completed.context.clone(),
                &completed.target.subscription_id,
                completed.kind,
                &completed.prepared_at,
                &completed.expires_at,
            )
            .map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)?;
        if binding.draft_id != completed.draft_id
            || binding.ledger_revision != completed.ledger_revision
        {
            return invalid();
        }
        let receipt = memory
            .commit_intent_mock(
                &binding.draft_id,
                &binding.context,
                &event.occurred_at,
                &mut HistoricalMockGate(binding.clone()),
            )
            .map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)?;
        if serde_json::to_value(receipt).map_err(|_| IntentRecoveryIssue::InvalidCheckpoint)?
            != completed.receipt
        {
            return invalid();
        }
    }
    if memory.intent_events != state.events
        || memory.ledger.entries != state.ledger.entries
        || memory.revision != state.revision
    {
        return invalid();
    }
    memory.next_draft = state.next_draft;
    // Uncommitted drafts/confirmation grants are intentionally not restored.
    memory.intent_drafts.clear();
    memory.latest_intent_drafts.clear();
    Ok(memory)
}

pub struct PostgresIntentSessionMock {
    pool: PgPool,
    user: String,
    version: i64,
    memory: MemoryConfirmationMock,
}
impl PostgresIntentSessionMock {
    pub async fn initialize_local_schema(pool: &PgPool) -> Result<(), IntentRecoveryIssue> {
        sqlx::raw_sql(include_str!(
            "../../sql/intent_recovery_mock_v1.proposed.sql"
        ))
        .execute(pool)
        .await
        .map_err(|_| IntentRecoveryIssue::Unavailable)?;
        Ok(())
    }
    /// Explicit initial fictional fixture creation; existing owner is not replaced.
    pub async fn create_fixture_mock(
        pool: PgPool,
        user: &str,
        fixture: LedgerFixture,
    ) -> Result<Self, IntentRecoveryIssue> {
        let memory = MemoryConfirmationMock::from_fixture(fixture);
        let value = checkpoint(&memory, user)?;
        let count = sqlx::query("INSERT INTO safeflow_subscription_mock_v1.intent_recovery (owner_id,version,checkpoint) VALUES ($1,1,$2) ON CONFLICT (owner_id) DO NOTHING")
            .bind(user).bind(value).execute(&pool).await.map_err(|_| IntentRecoveryIssue::Unavailable)?.rows_affected();
        if count != 1 {
            return Err(IntentRecoveryIssue::Conflict);
        }
        Ok(Self {
            pool,
            user: user.into(),
            version: 1,
            memory,
        })
    }
    pub async fn load_mock(pool: PgPool, user: &str) -> Result<Self, IntentRecoveryIssue> {
        if user.trim().is_empty() {
            return invalid();
        }
        let row: Option<(i64, Value)> = sqlx::query_as("SELECT version,checkpoint FROM safeflow_subscription_mock_v1.intent_recovery WHERE owner_id=$1")
            .bind(user).fetch_optional(&pool).await.map_err(|_| IntentRecoveryIssue::Unavailable)?;
        let (version, value) = row.ok_or(IntentRecoveryIssue::NotFound)?;
        let memory = restore(value, user)?;
        Ok(Self {
            pool,
            user: user.into(),
            version,
            memory,
        })
    }
    pub fn list_mock(&self) -> Result<&[LedgerEntry], MockIssue> {
        self.memory.list_mock(&self.user)
    }
    pub fn intent_result_mock(&self, operation: &str) -> Result<&MockIntentReceipt, MockIssue> {
        self.memory.intent_result_mock(&self.user, operation)
    }
    pub fn intent_events_mock(&self) -> Result<&[MockIntentEvent], MockIssue> {
        self.memory.intent_events_mock(&self.user)
    }
    pub async fn prepare_intent_mock(
        &mut self,
        context: MockOperationContext,
        id: &str,
        kind: MockIntentKind,
        now: &str,
        expires: &str,
    ) -> Result<MockIntentBinding, IntentRecoveryIssue> {
        let mut candidate = self.memory.clone();
        let binding = candidate
            .prepare_intent_mock(context, id, kind, now, expires)
            .map_err(IntentRecoveryIssue::Business)?;
        // Persist the reserved counter so a discarded pre-restart draft ID cannot
        // be assigned to a fresh confirmation after restart.
        self.publish(candidate).await?;
        Ok(binding)
    }
    /// Publish success only after current rows, original receipt, history and
    /// deduplication facts are committed together. Failed writes leave memory unchanged.
    pub async fn commit_intent_mock(
        &mut self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockIntentGateVerifier,
    ) -> Result<MockIntentReceipt, IntentRecoveryIssue> {
        let mut candidate = self.memory.clone();
        let receipt = candidate
            .commit_intent_mock(draft, context, now, gate)
            .map_err(IntentRecoveryIssue::Business)?;
        self.publish(candidate).await?;
        Ok(receipt)
    }
    async fn publish(
        &mut self,
        candidate: MemoryConfirmationMock,
    ) -> Result<(), IntentRecoveryIssue> {
        let value = checkpoint(&candidate, &self.user)?;
        let next = self
            .version
            .checked_add(1)
            .ok_or(IntentRecoveryIssue::Conflict)?;
        let count = sqlx::query("UPDATE safeflow_subscription_mock_v1.intent_recovery SET version=$1,checkpoint=$2 WHERE owner_id=$3 AND version=$4")
            .bind(next).bind(value).bind(&self.user).bind(self.version).execute(&self.pool).await
            .map_err(|_| IntentRecoveryIssue::Unavailable)?.rows_affected();
        if count != 1 {
            return Err(IntentRecoveryIssue::Conflict);
        }
        self.version = next;
        self.memory = candidate;
        Ok(())
    }
}
