//! Committed-fact lookup, NOT public operation success or authorization.
//! The caller supplies a trusted original binding and a READ ONLY / READ COMMITTED
//! connection, separately from its write callback. No pool/transaction control,
//! lock insertion, permission issuance, automatic retry or online release.
use crate::{
    backend::PendingFreezeExecution,
    freeze::{check_replay_binding, load_freeze_on_connection, FreezeCommand},
    read_complete_facts, read_revision, valid_id, StoreError,
};
use safeflow_card_facts::{
    account_service::AccountCardFacts, operation_records::HistoricalLockReceipt,
    operation_replay::LockReplayParameters, LockSource,
};
use sqlx::PgConnection;

/// Backend-only original business identity, not a client/LLM credential or a
/// confirmation fingerprint. No Deserialize/Debug to expose internal references.
#[derive(Clone, PartialEq, Eq)]
pub struct FreezeReadbackRequest {
    owner: String,
    account: String,
    operation: String,
    key: String,
    parameters: LockReplayParameters,
    event_scope: Option<String>,
}
impl FreezeReadbackRequest {
    pub fn new(
        owner: &str,
        account: &str,
        operation: &str,
        original_key: &str,
        parameters: LockReplayParameters,
        event_scope: Option<String>,
    ) -> Result<Self, FreezeReadbackIssue> {
        if [owner, account, operation, original_key, &parameters.card_id]
            .iter()
            .any(|s| !valid_id(s))
            || (parameters.source == LockSource::UserSelf) != event_scope.is_none()
            || [
                event_scope.as_deref(),
                parameters.event_id.as_deref(),
                parameters.transaction_id.as_deref(),
                parameters.feedback_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|s| !valid_id(s))
        {
            return Err(FreezeReadbackIssue::InvalidBinding);
        }
        Ok(Self {
            owner: owner.into(),
            account: account.into(),
            operation: operation.into(),
            key: original_key.into(),
            parameters,
            event_scope,
        })
    }
    pub fn from_command(
        owner: &str,
        account: &str,
        command: &FreezeCommand,
    ) -> Result<Self, FreezeReadbackIssue> {
        Self::new(
            owner,
            account,
            &command.operation_id,
            &command.canonical_key,
            command.parameters.clone(),
            command.event_scope.clone(),
        )
    }
    /// Recover the ORIGINAL only. Missing is indeterminate, never a reason to
    /// call save_freeze again. A successful response is committed database facts,
    /// not proof that public confirmation/audit/payment conditions are satisfied.
    pub async fn query(
        &self,
        connection: &mut PgConnection,
    ) -> Result<FreezeReadbackResolution, FreezeReadbackIssue> {
        require_read_boundary(connection).await?;
        require_owner(connection, &self.owner, &self.account).await?;
        let before = read_revision(connection, &self.account)
            .await
            .map_err(classify)?;
        let stored =
            load_freeze_on_connection(connection, &self.owner, &self.account, &self.operation)
                .await
                .map_err(classify)?;
        let result = if let Some((bundle, scope)) = stored {
            check_replay_binding(
                &bundle,
                scope.as_deref(),
                &self.operation,
                &self.key,
                &self.parameters,
                self.event_scope.as_deref(),
            )
            .map_err(classify)?;
            // This is the same history validator/projection used by the store.
            let receipt = crate::freeze::checked_receipt(&self.owner, bundle).map_err(classify)?;
            let facts = read_complete_facts(connection, &self.owner, &self.account)
                .await
                .map_err(classify)?;
            let card = facts
                .cards
                .iter()
                .find(|c| c.card_id == receipt.card_id)
                .ok_or(FreezeReadbackIssue::InconsistentRecord)?;
            if facts.cards.iter().any(|c| {
                c.active_locks.iter().any(|l| {
                    l.id == receipt.lock_id
                        && (c.card_id != receipt.card_id || l.reason != receipt.public_reason)
                })
            }) {
                return Err(FreezeReadbackIssue::InconsistentRecord);
            }
            let original_lock_active = card.active_locks.iter().any(|l| l.id == receipt.lock_id);
            FreezeReadbackResolution::Recorded(Box::new(VerifiedFreezeReadback {
                receipt,
                facts,
                original_lock_active,
            }))
        } else {
            FreezeReadbackResolution::NeedsReview
        };
        if read_revision(connection, &self.account)
            .await
            .map_err(classify)?
            != before
        {
            return Err(FreezeReadbackIssue::SnapshotChanged);
        }
        require_owner(connection, &self.owner, &self.account).await?;
        require_read_boundary(connection).await?;
        Ok(result)
    }
}
pub(crate) async fn require_read_boundary(
    connection: &mut PgConnection,
) -> Result<(), FreezeReadbackIssue> {
    let (read_only, isolation): (String, String) = sqlx::query_as(
        "SELECT current_setting('transaction_read_only'),current_setting('transaction_isolation')",
    )
    .fetch_one(connection)
    .await
    .map_err(|_| FreezeReadbackIssue::Unavailable)?;
    // PostgreSQL disallows changing a transaction to read-only after it wrote.
    // This rejects a writer reading its own uncommitted rows; READ COMMITTED
    // also avoids an old repeatable-read snapshot being called current.
    if read_only != "on" || isolation != "read committed" {
        return Err(FreezeReadbackIssue::ReadOnlyBoundaryRequired);
    }
    Ok(())
}
pub(crate) async fn require_owner(
    connection: &mut PgConnection,
    owner: &str,
    account: &str,
) -> Result<(), FreezeReadbackIssue> {
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM transfer_accounts_v1 WHERE account_id=$1 AND owner_id=$2)",
    )
    .bind(account)
    .bind(owner)
    .fetch_one(connection)
    .await
    .map_err(|_| FreezeReadbackIssue::Unavailable)?;
    if owns {
        Ok(())
    } else {
        Err(FreezeReadbackIssue::ResourceUnavailable)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezeReadbackIssue {
    InvalidBinding,
    ReadOnlyBoundaryRequired,
    ResourceUnavailable,
    Unavailable,
    IncompleteCatalog,
    InconsistentRecord,
    SnapshotChanged,
    BindingConflict,
    RetryKeyNeedsCoordination,
}
fn classify(e: StoreError) -> FreezeReadbackIssue {
    match e {
        StoreError::InvalidInput => FreezeReadbackIssue::InvalidBinding,
        StoreError::ResourceUnavailable => FreezeReadbackIssue::ResourceUnavailable,
        StoreError::CatalogUninitialized => FreezeReadbackIssue::IncompleteCatalog,
        StoreError::InconsistentRecord => FreezeReadbackIssue::InconsistentRecord,
        StoreError::SnapshotChanged => FreezeReadbackIssue::SnapshotChanged,
        StoreError::Conflict => FreezeReadbackIssue::BindingConflict,
        StoreError::RetryKeyNeedsCoordination | StoreError::NeedsCoordination => {
            FreezeReadbackIssue::RetryKeyNeedsCoordination
        }
        _ => FreezeReadbackIssue::Unavailable,
    }
}
/// Cannot be constructed/serialized from an in-transaction Pending result.
#[derive(Debug)]
pub struct VerifiedFreezeReadback {
    receipt: HistoricalLockReceipt,
    facts: AccountCardFacts,
    original_lock_active: bool,
}
impl VerifiedFreezeReadback {
    pub fn historical_receipt(&self) -> &HistoricalLockReceipt {
        &self.receipt
    }
    pub fn current_facts(&self) -> &AccountCardFacts {
        &self.facts
    }
    pub fn original_lock_active(&self) -> bool {
        self.original_lock_active
    }
    pub fn user_view(&self) -> FreezeUserView {
        let card = self
            .facts
            .cards
            .iter()
            .find(|c| c.card_id == self.receipt.card_id)
            .expect("validated card in private readback");
        let reasons = self
            .facts
            .cards
            .iter()
            .flat_map(|c| c.active_locks.iter().map(|l| l.reason))
            .collect();
        FreezeUserView {
            state: if self.original_lock_active {
                FreezeUserState::OriginalRecordedActive
            } else {
                FreezeUserState::OriginalRecordedInactive
            },
            last_four: Some(card.last_four.clone()),
            account_outgoing_restricted: Some(self.facts.outgoing_restricted()),
            public_reasons: reasons,
            message: if self.original_lock_active {
                "已核实原锁卡记录，当前这把锁仍有效。该账户全部出账受限；锁卡限制不影响查询、收款和其他账户。解除请到线下银行办理。"
            } else if self.facts.outgoing_restricted() {
                "已核实原锁卡记录，当前这把锁已不再有效，但该账户仍有其他有效锁，全部出账继续受限。解除请到线下银行办理。"
            } else {
                "已核实原锁卡记录，当前该账户未查到有效卡锁；这不代表其他出账条件已满足。线下处理情况以银行实际记录为准。"
            },
        }
    }
}
#[derive(Debug)]
pub enum FreezeReadbackResolution {
    Recorded(Box<VerifiedFreezeReadback>),
    NeedsReview,
}
impl FreezeReadbackResolution {
    pub fn user_view(&self) -> FreezeUserView {
        match self {
            Self::Recorded(v) => v.user_view(),
            Self::NeedsReview => FreezeUserView::unconfirmed(
                FreezeUserState::NeedsReview,
                "暂未查到可核实的原锁卡记录，结果仍待核对，请勿重复提交锁卡。",
            ),
        }
    }
}
/// Allow-listed local display only, not an adopted public HTTP/result contract.
/// No keys, private event/feedback/transaction IDs, account IDs or risk internals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreezeUserView {
    pub state: FreezeUserState,
    pub last_four: Option<String>,
    pub account_outgoing_restricted: Option<bool>,
    pub public_reasons: Vec<&'static str>,
    pub message: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezeUserState {
    OriginalRecordedActive,
    OriginalRecordedInactive,
    NeedsReview,
    Unavailable,
}
impl FreezeUserView {
    fn unconfirmed(state: FreezeUserState, message: &'static str) -> Self {
        Self {
            state,
            last_four: None,
            account_outgoing_restricted: None,
            public_reasons: vec![],
            message,
        }
    }
    pub fn unavailable(_issue: FreezeReadbackIssue) -> Self {
        Self::unconfirmed(
            FreezeUserState::Unavailable,
            "暂时无法核实锁卡结果，请稍后核对原操作，不要重复提交锁卡。",
        )
    }
}
/// Wraps the old pending result for audit preparation plus an immutable original
/// lookup. No COMMIT method or success boolean. Caller still owns the public
/// transaction; it then supplies a separately authorized read-only connection.
pub struct PendingFreezeHandoff {
    pending: PendingFreezeExecution,
    request: FreezeReadbackRequest,
}
impl PendingFreezeHandoff {
    pub(crate) fn new(pending: PendingFreezeExecution, request: FreezeReadbackRequest) -> Self {
        Self { pending, request }
    }
    pub fn pending_execution(&self) -> &PendingFreezeExecution {
        &self.pending
    }
    pub fn original_request(&self) -> &FreezeReadbackRequest {
        &self.request
    }
    pub async fn query_after_caller_completion(
        self,
        connection: &mut PgConnection,
    ) -> Result<FreezeReadbackResolution, FreezeReadbackIssue> {
        let result = self.request.query(connection).await?;
        if let FreezeReadbackResolution::Recorded(ref verified) = result {
            if verified.historical_receipt() != self.pending.historical_receipt() {
                return Err(FreezeReadbackIssue::InconsistentRecord);
            }
        }
        Ok(result)
    }
}
