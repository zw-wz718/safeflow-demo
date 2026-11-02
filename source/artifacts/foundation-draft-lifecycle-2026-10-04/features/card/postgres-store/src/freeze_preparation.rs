//! Module-local business preview and stale-facts guard. Not confirmation,
//! authentication, a public fingerprint, permit, migration or a draft database.
//! Trusted assembly retains this opaque value and binds it through its existing
//! public confirmation/authentication. A client echoing the visible view is not
//! enough to construct it. No pool, transaction control, writes or expiry policy.
use crate::{
    freeze::{validate_command, FreezeCommand},
    freeze_completion::{
        require_owner, require_read_boundary, FreezeReadbackIssue, FreezeReadbackRequest,
    },
    read_complete_facts, read_revision, StoreError,
};
use safeflow_card_facts::account_service::AccountCardFacts;
use sqlx::PgConnection;

/// Public-content allow-list only. Planned effect is NOT an executed freeze.
/// No account/card IDs, keys, event/feedback/transaction IDs or internal scores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreezePreparationView {
    pub last_four: String,
    pub requested_public_reason: &'static str,
    pub current_account_restricted_by_card_locks: bool,
    pub existing_public_reasons: Vec<&'static str>,
    pub planned_effect: &'static str,
    pub unlock_instruction: &'static str,
    pub message: &'static str,
}

/// Immutable trusted-backend facts, never Deserialize, Serialize or a caller
/// confirmed=true. The original binding also includes operation/key/provider.
pub struct PreparedFreeze {
    original: FreezeReadbackRequest,
    facts: AccountCardFacts,
    view: FreezePreparationView,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezePreparationIssue {
    InvalidCommand,
    ReadOnlyBoundaryRequired,
    ResourceUnavailable,
    Unavailable,
    IncompleteData,
    InconsistentData,
    FactsChanged,
    BindingChanged,
    OriginalOperationAlreadyRecorded,
}
#[derive(Debug)]
pub enum PreparedFreezeExecutionIssue {
    Preparation(FreezePreparationIssue),
    Store(StoreError),
    /// Stop; separately query the committed original. This is not a failure of
    /// the original, and does not replay a write or mint a new public operation.
    OriginalOperationAlreadyRecorded,
}
impl PreparedFreeze {
    /// Command identifiers are supplied by the trusted existing operation
    /// boundary; preparation neither allocates nor persists business identifiers.
    pub async fn read_on_connection(
        connection: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
        command: &FreezeCommand,
    ) -> Result<Self, FreezePreparationIssue> {
        validate_command(trusted_owner, command)
            .map_err(|_| FreezePreparationIssue::InvalidCommand)?;
        let original = FreezeReadbackRequest::from_command(trusted_owner, account, command)
            .map_err(|_| FreezePreparationIssue::InvalidCommand)?;
        require_read_boundary(connection)
            .await
            .map_err(read_issue)?;
        require_owner(connection, trusted_owner, account)
            .await
            .map_err(read_issue)?;
        let before = read_revision(connection, account)
            .await
            .map_err(store_issue)?;
        if let Some((bundle, event_scope)) = crate::freeze::load_freeze_on_connection(
            connection,
            trusted_owner,
            account,
            &command.operation_id,
        )
        .await
        .map_err(store_issue)?
        {
            crate::freeze::check_replay_binding(
                &bundle,
                event_scope.as_deref(),
                &command.operation_id,
                &command.canonical_key,
                &command.parameters,
                command.event_scope.as_deref(),
            )
            .map_err(|_| FreezePreparationIssue::BindingChanged)?;
            return Err(FreezePreparationIssue::OriginalOperationAlreadyRecorded);
        }
        let facts = read_complete_facts(connection, trusted_owner, account)
            .await
            .map_err(store_issue)?;
        let selected = facts
            .cards
            .iter()
            .find(|c| c.card_id == command.parameters.card_id)
            .ok_or(FreezePreparationIssue::ResourceUnavailable)?;
        let view=FreezePreparationView {
            last_four:selected.last_four.clone(),
            requested_public_reason:command.parameters.source.public_reason(),
            current_account_restricted_by_card_locks:facts.outgoing_restricted(),
            existing_public_reasons:facts.cards.iter().flat_map(|c|c.active_locks.iter().map(|l|l.reason)).collect(),
            planned_effect:"确认并完成冻结后，该卡关联账户的全部出账受限，包括转账、刷卡和自动扣款；查询、收款及其他账户不受卡锁影响。",
            unlock_instruction:"解除请到线下银行办理；多条有效锁必须全部解除，才能解除卡锁造成的出账限制。",
            message:"请核对本次选定卡片及冻结影响；这里只展示待确认内容，尚未执行本次锁卡。",
        };
        if read_revision(connection, account)
            .await
            .map_err(store_issue)?
            != before
        {
            return Err(FreezePreparationIssue::FactsChanged);
        }
        require_owner(connection, trusted_owner, account)
            .await
            .map_err(read_issue)?;
        require_read_boundary(connection)
            .await
            .map_err(read_issue)?;
        Ok(Self {
            original,
            facts,
            view,
        })
    }
    pub fn view(&self) -> &FreezePreparationView {
        &self.view
    }
    /// Internal fact version for public binding; this alone is not the public
    /// facts_version/confirmation digest and must not be accepted from a client.
    pub fn business_revision(&self) -> &str {
        &self.facts.revision
    }
    pub fn original_request(&self) -> &FreezeReadbackRequest {
        &self.original
    }
    pub(crate) fn verify_binding(
        &self,
        owner: &str,
        account: &str,
        command: &FreezeCommand,
    ) -> Result<(), FreezePreparationIssue> {
        let current = FreezeReadbackRequest::from_command(owner, account, command)
            .map_err(|_| FreezePreparationIssue::InvalidCommand)?;
        if current != self.original {
            return Err(FreezePreparationIssue::BindingChanged);
        }
        Ok(())
    }
    pub(crate) fn verify_facts(
        &self,
        current: &AccountCardFacts,
    ) -> Result<(), FreezePreparationIssue> {
        // Compare the entire validated account projection AND its monotonic
        // version; changing and restoring a lock must not revive an old preview.
        if current != &self.facts {
            return Err(FreezePreparationIssue::FactsChanged);
        }
        Ok(())
    }
}
fn read_issue(issue: FreezeReadbackIssue) -> FreezePreparationIssue {
    match issue {
        FreezeReadbackIssue::ReadOnlyBoundaryRequired => {
            FreezePreparationIssue::ReadOnlyBoundaryRequired
        }
        FreezeReadbackIssue::ResourceUnavailable => FreezePreparationIssue::ResourceUnavailable,
        FreezeReadbackIssue::IncompleteCatalog => FreezePreparationIssue::IncompleteData,
        FreezeReadbackIssue::SnapshotChanged => FreezePreparationIssue::FactsChanged,
        FreezeReadbackIssue::InconsistentRecord => FreezePreparationIssue::InconsistentData,
        _ => FreezePreparationIssue::Unavailable,
    }
}
fn store_issue(issue: StoreError) -> FreezePreparationIssue {
    match issue {
        StoreError::CatalogUninitialized => FreezePreparationIssue::IncompleteData,
        StoreError::ResourceUnavailable => FreezePreparationIssue::ResourceUnavailable,
        StoreError::SnapshotChanged => FreezePreparationIssue::FactsChanged,
        StoreError::InconsistentRecord => FreezePreparationIssue::InconsistentData,
        _ => FreezePreparationIssue::Unavailable,
    }
}
