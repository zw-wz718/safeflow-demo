//! Module-owned adapter for a public controlled database callback. The public
//! boundary must first bind the authenticated owner, original operation/key and
//! current trusted evidence, apply owner/task/operation protection, independent
//! confirmation and strong verification, and consume the correct permit.
//! No caller boolean or this adapter itself supplies those public guarantees.
use crate::{
    freeze::{FreezeCommand, FreezeSaveDisposition, FreezeSaveResult},
    LockedCardAccount, StoreError,
};
use safeflow_card_facts::{
    account_service::AccountCardFacts, operation_records::HistoricalLockReceipt,
};
use sqlx::PgConnection;

/// Exclusively borrows the public callback's connection. No pool, connection
/// getter, transaction control, public registration or online release entry.
pub struct CardFreezeBackend<'a> {
    scope: LockedCardAccount<'a>,
}

/// Observation inside the STILL OPEN caller transaction. Not Deserialize or
/// Serialize and no "committed/success" promotion method. The public wrapper
/// must confirm COMMIT and re-read facts before reporting a current lock state.
#[derive(Debug)]
pub struct PendingFreezeExecution {
    pub(crate) saved: FreezeSaveResult,
    pub(crate) facts: AccountCardFacts,
    pub(crate) original_lock_active: bool,
}
impl PendingFreezeExecution {
    pub fn historical_receipt(&self) -> &HistoricalLockReceipt {
        &self.saved.historical_receipt
    }
    pub fn disposition(&self) -> FreezeSaveDisposition {
        self.saved.disposition
    }
    pub fn current_facts_in_transaction(&self) -> &AccountCardFacts {
        &self.facts
    }
    pub fn original_lock_active_in_transaction(&self) -> bool {
        self.original_lock_active
    }
}

impl<'a> CardFreezeBackend<'a> {
    /// The trusted arguments come from the PUBLIC authenticated callback, not
    /// model arguments. Transaction checks are not an authentication substitute.
    pub async fn borrow(
        conn: &'a mut PgConnection,
        trusted_owner: &str,
        account_id: &str,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            scope: LockedCardAccount::acquire(conn, trusted_owner, account_id).await?,
        })
    }

    /// Use ONLY after the public layer independently confirmed/authenticated and
    /// consumed the correct permit for the retained PreparedFreeze binding.
    /// The common account protection is already held by this backend. A stale
    /// preparation never writes; an existing operation goes to original lookup.
    pub async fn execute_prepared_freeze_handoff(
        &mut self,
        prepared: &crate::freeze_preparation::PreparedFreeze,
        command: &FreezeCommand,
    ) -> Result<
        crate::freeze_completion::PendingFreezeHandoff,
        crate::freeze_preparation::PreparedFreezeExecutionIssue,
    > {
        use crate::freeze_preparation::PreparedFreezeExecutionIssue as Issue;
        self.scope.check_transaction().await.map_err(Issue::Store)?;
        prepared
            .verify_binding(&self.scope.owner, &self.scope.account, command)
            .map_err(Issue::Preparation)?;
        if let Some((bundle, event_scope)) = crate::freeze::load_freeze_on_connection(
            self.scope.conn,
            &self.scope.owner,
            &self.scope.account,
            &command.operation_id,
        )
        .await
        .map_err(Issue::Store)?
        {
            crate::freeze::check_replay_binding(
                &bundle,
                event_scope.as_deref(),
                &command.operation_id,
                &command.canonical_key,
                &command.parameters,
                command.event_scope.as_deref(),
            )
            .map_err(Issue::Store)?;
            return Err(Issue::OriginalOperationAlreadyRecorded);
        }
        let current = self.scope.read_facts().await.map_err(Issue::Store)?;
        prepared
            .verify_facts(&current)
            .map_err(Issue::Preparation)?;
        self.execute_freeze_handoff(command)
            .await
            .map_err(Issue::Store)
    }

    /// Public assembly may use this instead of execute_freeze when it will
    /// verify committed facts afterwards. Authorization and COMMIT remain its
    /// responsibility. The retained request is immutable and never a retry.
    pub async fn execute_freeze_handoff(
        &mut self,
        command: &FreezeCommand,
    ) -> Result<crate::freeze_completion::PendingFreezeHandoff, StoreError> {
        let request = crate::freeze_completion::FreezeReadbackRequest::from_command(
            &self.scope.owner,
            &self.scope.account,
            command,
        )
        .map_err(|_| StoreError::InvalidInput)?;
        let pending = self.execute_freeze(command).await?;
        Ok(crate::freeze_completion::PendingFreezeHandoff::new(
            pending, request,
        ))
    }

    /// Save and then re-read ALL current account cards/locks while retaining the
    /// same account protection. Propagate any failure to public transaction
    /// rollback; never catch it and claim business success. No automatic retry.
    pub async fn execute_freeze(
        &mut self,
        command: &FreezeCommand,
    ) -> Result<PendingFreezeExecution, StoreError> {
        let saved = self.scope.save_freeze(command).await?;
        let facts = self.scope.read_facts().await?;
        let receipt = &saved.historical_receipt;
        let card = facts
            .cards
            .iter()
            .find(|c| c.card_id == receipt.card_id)
            .ok_or(StoreError::InconsistentRecord)?;
        for c in &facts.cards {
            if c.active_locks.iter().any(|l| {
                l.id == receipt.lock_id
                    && (c.card_id != receipt.card_id || l.reason != receipt.source.public_reason())
            }) {
                return Err(StoreError::InconsistentRecord);
            }
        }
        let original_lock_active = card.active_locks.iter().any(|l| l.id == receipt.lock_id);
        if saved.disposition == FreezeSaveDisposition::PersistedInCallerTransaction
            && !original_lock_active
        {
            return Err(StoreError::InconsistentRecord);
        }
        Ok(PendingFreezeExecution {
            saved,
            facts,
            original_lock_active,
        })
    }
}
