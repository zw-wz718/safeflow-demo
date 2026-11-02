//! Existing card history reached through a freshly checked original feedback.
//! This intentionally does NOT infer feedback -> freeze-operation identity.
use crate::{
    inspect_saved_feedback, FeedbackCardInspection, InspectionIssue, InspectionState,
    InspectionView,
};
use safeflow_billing_source_reader::TrustedScope;
use safeflow_card_postgres_store::{
    freeze_completion::{FreezeReadbackResolution, FreezeUserView},
    protection_history::{
        CardProtectionHistoryView, ProtectionHistoryIssue, ReadOnlyCardProtectionHistory,
        RecordedProtectionAction, SelectedProtectionRecord,
    },
};
use sqlx::{PgConnection, PgPool};
use std::sync::Arc;

const SCOPE: &str = "以下为这张卡已有的保护操作记录；本次反馈的办理结果仍须按对应原操作单独核实。查看记录不会再次冻结。";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackProtectionIssue {
    Inspection(InspectionIssue),
    NotApplicable,
    History(ProtectionHistoryIssue),
}
impl FeedbackProtectionIssue {
    pub fn message(self) -> &'static str {
        match self {
            Self::Inspection(i) => i.message(),
            Self::NotApplicable => "本次反馈未通过当前卡片关联核对，停止读取对应卡的保护记录。",
            Self::History(i) => i.message(),
        }
    }
}

/// Complete list retained in the backend. No public constructor, serialization,
/// cloning, connection getter or executable freeze/online-unlock path.
pub struct FeedbackProtectionHistory {
    association: Arc<FeedbackCardInspection>,
    history: ReadOnlyCardProtectionHistory,
}
#[derive(Debug)]
pub struct FeedbackProtectionHistoryView<'a> {
    pub scope_explanation: &'static str,
    pub association: &'a InspectionView,
    pub card_history: CardProtectionHistoryView,
}
impl FeedbackCardInspection {
    /// Reuse the existing full card-history reader; bracket it with fresh
    /// original-feedback/scope/current-card checks. Readonly observations only:
    /// changes AFTER return still require public final transaction protection.
    pub async fn read_card_protection_history(
        &self,
        billing_pool: &PgPool,
        current_scope: &TrustedScope,
        trusted_source_id: &str,
        cards: &mut PgConnection,
    ) -> Result<FeedbackProtectionHistory, FeedbackProtectionIssue> {
        use FeedbackProtectionIssue as I;
        let current = self
            .recheck(billing_pool, current_scope, trusted_source_id, cards)
            .await
            .map_err(I::Inspection)?;
        if current.view.state != InspectionState::CardAssociationVerified {
            return Err(I::NotApplicable);
        }
        let card = current.original.card.as_deref().ok_or(I::NotApplicable)?;
        let history = ReadOnlyCardProtectionHistory::read_on_connection(
            cards,
            &current.original.owner,
            &current.original.account,
            card,
        )
        .await
        .map_err(I::History)?;
        let checked = current
            .recheck(billing_pool, current_scope, trusted_source_id, cards)
            .await
            .map_err(I::Inspection)?;
        Ok(FeedbackProtectionHistory {
            association: Arc::new(checked),
            history,
        })
    }
}
impl FeedbackProtectionHistory {
    pub fn view(&self) -> FeedbackProtectionHistoryView<'_> {
        FeedbackProtectionHistoryView {
            scope_explanation: SCOPE,
            association: self.association.view(),
            card_history: self.history.view(),
        }
    }
    /// Explicit position in THIS retained list, even when only one record
    /// exists. Public positions cannot be deserialized into an executable token.
    pub fn select_record(
        &self,
        position: Option<usize>,
    ) -> Result<FeedbackProtectionRecord, FeedbackProtectionIssue> {
        let selected = self
            .history
            .select_record(position)
            .map_err(FeedbackProtectionIssue::History)?;
        let view = self.history.view();
        let display = view
            .locks
            .iter()
            .flat_map(|l| &l.protection_records)
            .find(|r| Some(r.position) == position)
            .ok_or(FeedbackProtectionIssue::History(
                ProtectionHistoryIssue::InconsistentData,
            ))?;
        Ok(FeedbackProtectionRecord {
            association: Arc::clone(&self.association),
            selected,
            original_action: display.original_action,
        })
    }
}

/// Bound to an explicitly selected existing card operation, not automatically
/// matched to the feedback operation/transaction. No public raw-ID constructor.
pub struct FeedbackProtectionRecord {
    association: Arc<FeedbackCardInspection>,
    selected: SelectedProtectionRecord,
    original_action: RecordedProtectionAction,
}
pub struct FeedbackProtectionReadback {
    association: FeedbackCardInspection,
    result: FreezeReadbackResolution,
    original_action: RecordedProtectionAction,
}
#[derive(Debug)]
pub struct FeedbackProtectionReadbackView<'a> {
    pub scope_explanation: &'static str,
    pub association: &'a InspectionView,
    pub original_action: Option<RecordedProtectionAction>,
    pub original_operation: FreezeUserView,
}
impl FeedbackProtectionRecord {
    /// Original receipt is immutable but current locks may legitimately change
    /// (e.g. a recorded offline release). Fresh card facts are expected here.
    /// Preserve original evidence binding, then bracket existing original read
    /// with fresh checks against the NEW card snapshot, not the old list revision.
    pub async fn query_current_result(
        &self,
        billing_pool: &PgPool,
        current_scope: &TrustedScope,
        trusted_source_id: &str,
        cards: &mut PgConnection,
    ) -> Result<FeedbackProtectionReadback, FeedbackProtectionIssue> {
        use FeedbackProtectionIssue as I;
        let current = inspect_saved_feedback(
            billing_pool,
            current_scope,
            &self.association.original.operation,
            trusted_source_id,
            cards,
        )
        .await
        .map_err(I::Inspection)?;
        if current.original != self.association.original
            || current.view.state != InspectionState::CardAssociationVerified
        {
            return Err(I::Inspection(InspectionIssue::FactsChanged));
        }
        let result = self
            .selected
            .query_on_connection(cards, &current.original.owner, &current.original.account)
            .await
            .map_err(I::History)?;
        let checked = current
            .recheck(billing_pool, current_scope, trusted_source_id, cards)
            .await
            .map_err(I::Inspection)?;
        Ok(FeedbackProtectionReadback {
            association: checked,
            result,
            original_action: self.original_action,
        })
    }
}
impl FeedbackProtectionReadback {
    pub fn view(&self) -> FeedbackProtectionReadbackView<'_> {
        FeedbackProtectionReadbackView {
            scope_explanation: SCOPE,
            association: self.association.view(),
            original_action: match &self.result {
                FreezeReadbackResolution::Recorded(_) => Some(self.original_action),
                FreezeReadbackResolution::NeedsReview => None,
            },
            original_operation: self.result.user_view(),
        }
    }
}
