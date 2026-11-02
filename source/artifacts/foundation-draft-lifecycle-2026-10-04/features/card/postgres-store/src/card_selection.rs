//! Account-scoped readonly card discovery and explicit selection. Not public
//! authentication, a confirmation token, payment permission or a freeze call.
//! The trusted assembler retains these opaque objects in the proper session;
//! a displayed position is meaningful ONLY inside its retained original list.
use crate::{
    freeze::FreezeCommand,
    freeze_completion::{require_owner, require_read_boundary, FreezeReadbackIssue},
    freeze_preparation::{FreezePreparationIssue, PreparedFreeze},
    read_complete_facts, read_revision, valid_id, StoreError,
};
use safeflow_card_facts::account_service::AccountCardFacts;
use sqlx::PgConnection;
use std::collections::HashMap;

/// Display allow-list. Positions are scoped to this list, not stable IDs or
/// authority. Neither raw card/account/lock IDs nor versions are exposed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardListEntry {
    pub position: usize,
    pub last_four: String,
    pub last_four_is_ambiguous: bool,
    pub card_has_active_locks: bool,
    pub public_reasons: Vec<&'static str>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardListView {
    pub cards: Vec<CardListEntry>,
    pub account_outgoing_restricted_by_card_locks: bool,
    pub account_public_reasons: Vec<&'static str>,
    pub message: &'static str,
    pub selection_instruction: &'static str,
    pub restriction_explanation: &'static str,
    pub unlock_instruction: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardSelectionIssue {
    ResourceUnavailable,
    ReadOnlyBoundaryRequired,
    IncompleteData,
    InconsistentData,
    Unavailable,
    FactsChanged,
    SelectionRequired,
    InvalidSelection,
    BindingChanged,
    Preparation(FreezePreparationIssue),
}
impl CardSelectionIssue {
    pub fn message(self) -> &'static str {
        match self {
            Self::ResourceUnavailable => "无法核验本人对应账户或卡片，请核对资料。",
            Self::ReadOnlyBoundaryRequired | Self::Unavailable => {
                "暂时无法查询卡片，请稍后核对；不能据此判断无锁。"
            }
            Self::IncompleteData => "尚未取得完整卡片资料，不能列为无卡或无锁。",
            Self::InconsistentData => "卡片资料存在不一致，需要核对后再处理。",
            Self::FactsChanged => "卡片清单或锁定资料已变化，请重新查卡、选卡并核对。",
            Self::SelectionRequired => "请明确选择本次清单中的一张卡；仅提供尾号不能完成选卡。",
            Self::InvalidSelection => "本次选择不在保留的卡片清单中，请重新核对。",
            Self::BindingChanged => "选定卡片与本次冻结内容不一致，请重新核对。",
            Self::Preparation(FreezePreparationIssue::OriginalOperationAlreadyRecorded) => {
                "已有本次操作记录，请查询原操作，不要重复冻结。"
            }
            Self::Preparation(_) => {
                "暂时无法生成本次冻结核对单，请核对资料及原操作；不要重复冻结。"
            }
        }
    }
}
/// Opaque backend-held complete list; not Deserialize/Serialize/Clone/Debug.
/// Does not enumerate all accounts or establish the authority of its arguments.
pub struct ReadOnlyCardList {
    pub(super) owner: String,
    pub(super) facts: AccountCardFacts,
}
impl ReadOnlyCardList {
    /// Trusted-source alternative for module simulations/internal adapters. The
    /// existing query service verifies ownership, full pagination and revision.
    /// This is not a client snapshot constructor or public permission.
    pub fn read_from_source<S: safeflow_card_facts::account_service::AccountDataSource>(
        service: &safeflow_card_facts::account_service::AccountQueryService<S>,
        trusted_owner: &str,
        account: &str,
    ) -> Result<Self, CardSelectionIssue> {
        use safeflow_card_facts::account_service::AccountQueryIssue as I;
        let facts = service
            .query(trusted_owner, Some(account))
            .map_err(|e| match e {
                I::ResourceUnavailable => CardSelectionIssue::ResourceUnavailable,
                I::LookupUnavailable => CardSelectionIssue::Unavailable,
                I::IncompleteData => CardSelectionIssue::IncompleteData,
                I::InconsistentData => CardSelectionIssue::InconsistentData,
                I::SnapshotChanged => CardSelectionIssue::FactsChanged,
            })?;
        Ok(Self {
            owner: trusted_owner.into(),
            facts,
        })
    }
    /// The caller supplies independently authorized READ ONLY / READ COMMITTED
    /// access to ONE already-selected account. No FOR UPDATE, txid allocation,
    /// pool, transaction control, truncation or fallback to an empty list.
    pub async fn read_on_connection(
        connection: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
    ) -> Result<Self, CardSelectionIssue> {
        if !valid_id(trusted_owner) || !valid_id(account) {
            return Err(CardSelectionIssue::ResourceUnavailable);
        }
        require_read_boundary(connection)
            .await
            .map_err(read_issue)?;
        require_owner(connection, trusted_owner, account)
            .await
            .map_err(read_issue)?;
        let before = read_revision(connection, account)
            .await
            .map_err(store_issue)?;
        let facts = read_complete_facts(connection, trusted_owner, account)
            .await
            .map_err(store_issue)?;
        if read_revision(connection, account)
            .await
            .map_err(store_issue)?
            != before
        {
            return Err(CardSelectionIssue::FactsChanged);
        }
        require_owner(connection, trusted_owner, account)
            .await
            .map_err(read_issue)?;
        require_read_boundary(connection)
            .await
            .map_err(read_issue)?;
        Ok(Self {
            owner: trusted_owner.into(),
            facts,
        })
    }
    pub fn view(&self) -> CardListView {
        let mut tails = HashMap::new();
        for card in &self.facts.cards {
            *tails.entry(card.last_four.as_str()).or_insert(0usize) += 1;
        }
        CardListView {
            cards: self
                .facts
                .cards
                .iter()
                .enumerate()
                .map(|(index, card)| CardListEntry {
                    position: index + 1,
                    last_four: card.last_four.clone(),
                    last_four_is_ambiguous: tails[card.last_four.as_str()]>1,
                    card_has_active_locks: !card.active_locks.is_empty(),
                    public_reasons: card.active_locks.iter().map(|l| l.reason).collect(),
                })
                .collect(),
            account_outgoing_restricted_by_card_locks: self.facts.outgoing_restricted(),
            account_public_reasons: self
                .facts
                .cards
                .iter()
                .flat_map(|c| c.active_locks.iter().map(|l| l.reason))
                .collect(),
            message: if self.facts.cards.is_empty() {
                "本次完整查询未发现该账户关联卡片；这不代表账户已获准出账。"
            } else {
                "请核对本次卡片清单，需要冻结时请明确选一张卡；查询和选卡尚未执行冻结。"
            },
            selection_instruction:"选项序号只对应本次保留的清单；尾号相同时请先通过可信资料核对具体选项，不能仅凭尾号猜卡。",
            restriction_explanation: if self.facts.outgoing_restricted() {
                "该账户存在有效卡锁，全部出账受限，包括转账、刷卡和自动扣款；某张卡自身无锁也不能绕过同账户其他卡的限制。查询、收款和其他账户不受卡锁影响。"
            } else {
                "本次完整查询未发现该账户的有效卡锁；这不代表其他出账条件已满足。"
            },
            unlock_instruction:
                "解除请到线下银行办理；多条有效锁必须全部解除，才能解除卡锁造成的出账限制。",
        }
    }
    /// Require an explicit position even for a one-card list. No tail lookup,
    /// inference, first-card default or public executable credential is issued.
    pub fn select_position(
        &self,
        position: Option<usize>,
    ) -> Result<SelectedCard, CardSelectionIssue> {
        let position = position.ok_or(CardSelectionIssue::SelectionRequired)?;
        let index = position
            .checked_sub(1)
            .ok_or(CardSelectionIssue::InvalidSelection)?;
        let card = self
            .facts
            .cards
            .get(index)
            .ok_or(CardSelectionIssue::InvalidSelection)?;
        Ok(SelectedCard {
            owner: self.owner.clone(),
            facts: self.facts.clone(),
            card_id: card.card_id.clone(),
        })
    }
}
/// Module-local selection bound to the FULL original list/version. No public
/// deserialization. Any ID accessor is for backend assembly, not a credential.
pub struct SelectedCard {
    owner: String,
    facts: AccountCardFacts,
    card_id: String,
}
impl SelectedCard {
    /// Read CURRENT saved records for the retained card on an independently
    /// authorized readonly connection. This lookup is not a freeze preview.
    pub async fn read_protection_history_on_connection(
        &self,
        connection: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
    ) -> Result<
        crate::protection_history::ReadOnlyCardProtectionHistory,
        crate::protection_history::ProtectionHistoryIssue,
    > {
        if trusted_owner != self.owner || account != self.facts.account_id {
            return Err(crate::protection_history::ProtectionHistoryIssue::ResourceUnavailable);
        }
        crate::protection_history::ReadOnlyCardProtectionHistory::read_on_connection(
            connection,
            trusted_owner,
            account,
            &self.card_id,
        )
        .await
    }

    /// Trusted backend assembly of the existing FreezeCommand only. Never put
    /// this internal ID in the visible list or treat knowing it as authorization.
    pub fn card_id_for_backend(&self) -> &str {
        &self.card_id
    }

    /// Return the existing opaque freeze preview, after fresh readonly lookup,
    /// owner/card binding and full monotonic-facts comparison. The public caller
    /// must THEN perform its independent confirmation/authentication/permit flow.
    pub async fn prepare_freeze_on_connection(
        &self,
        connection: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
        command: &FreezeCommand,
    ) -> Result<PreparedFreeze, CardSelectionIssue> {
        if trusted_owner != self.owner || account != self.facts.account_id {
            return Err(CardSelectionIssue::ResourceUnavailable);
        }
        if command.parameters.card_id != self.card_id {
            return Err(CardSelectionIssue::BindingChanged);
        }
        let prepared =
            PreparedFreeze::read_on_connection(connection, trusted_owner, account, command)
                .await
                .map_err(CardSelectionIssue::Preparation)?;
        prepared
            .verify_facts(&self.facts)
            .map_err(|_| CardSelectionIssue::FactsChanged)?;
        Ok(prepared)
    }
}
fn read_issue(issue: FreezeReadbackIssue) -> CardSelectionIssue {
    match issue {
        FreezeReadbackIssue::ReadOnlyBoundaryRequired => {
            CardSelectionIssue::ReadOnlyBoundaryRequired
        }
        FreezeReadbackIssue::ResourceUnavailable => CardSelectionIssue::ResourceUnavailable,
        _ => CardSelectionIssue::Unavailable,
    }
}
fn store_issue(issue: StoreError) -> CardSelectionIssue {
    match issue {
        StoreError::CatalogUninitialized => CardSelectionIssue::IncompleteData,
        StoreError::SnapshotChanged => CardSelectionIssue::FactsChanged,
        StoreError::InconsistentRecord => CardSelectionIssue::InconsistentData,
        StoreError::ResourceUnavailable => CardSelectionIssue::ResourceUnavailable,
        _ => CardSelectionIssue::Unavailable,
    }
}
