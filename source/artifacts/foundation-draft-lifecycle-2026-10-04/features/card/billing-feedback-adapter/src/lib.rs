//! Read-only card assembly against Jonathan's real checked saved projection.
//! Immutable historical evidence is NOT AsyncSavedFeedbackDataSource: no live
//! feedback revision is invented. The pool may read another snapshot; this is
//! never final same-transaction freeze validation, permission or authentication.
use safeflow_billing_source_reader::{
    feedback_store::{load_feedback, CardDisposition, FeedbackProjection, StoreError},
    TrustedScope,
};
use safeflow_card_postgres_store::{
    card_selection::CardSelectionIssue, catalog_inspection::ReadOnlyCatalogInspection,
};
use sqlx::{PgConnection, PgPool};
mod protection;
pub use protection::{
    FeedbackProtectionHistory, FeedbackProtectionHistoryView, FeedbackProtectionIssue,
    FeedbackProtectionReadback, FeedbackProtectionReadbackView, FeedbackProtectionRecord,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectionState {
    StopMine,
    StopUnsure,
    StopNoCard,
    CardAssociationVerified,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectionIssue {
    ResourceUnavailable,
    InvalidInput,
    Unavailable,
    InconsistentEvidence,
    SourceMismatch,
    CardData(CardSelectionIssue),
    CardAssociationUnavailable,
    FactsChanged,
}
impl InspectionIssue {
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidInput => "反馈查询资料无效，请重新核对。",
            Self::ResourceUnavailable => "无法核验本人可访问的原反馈，请核对权限和资料。",
            Self::Unavailable => "暂时无法读取原反馈，不能据此继续锁卡。",
            Self::InconsistentEvidence => "原反馈资料不完整或不一致，请核对后再处理。",
            Self::SourceMismatch => "原反馈的交易来源与当前核对来源不符，停止处理。",
            Self::CardData(issue) => issue.message(),
            Self::CardAssociationUnavailable => {
                "无法在本人当前完整卡片清单中核验原反馈对应的卡，停止处理。"
            }
            Self::FactsChanged => "反馈或卡片资料已变化，请重新核对；本次没有执行冻结。",
        }
    }
}

/// Backend-only original identity: no public constructor, serde, Debug or Clone.
/// Getters neither authenticate the caller nor confer write authority.
#[derive(PartialEq, Eq)]
pub struct OriginalFeedbackReference {
    operation: String,
    owner: String,
    report: String,
    report_accounts: Vec<String>,
    source: String,
    account: String,
    transaction: String,
    card: Option<String>,
    snapshot: String,
    rule: String,
    calculated_at: String,
}
impl OriginalFeedbackReference {
    fn from_projection(p: &FeedbackProjection<'_>) -> Self {
        Self {
            operation: p.operation_id().into(),
            owner: p.owner_user_id().into(),
            report: p.report_id().into(),
            report_accounts: p.report_account_ids().to_vec(),
            source: p.reference().source_id.clone(),
            account: p.reference().account_id.clone(),
            transaction: p.reference().transaction_id.clone(),
            card: p.card_id().map(str::to_owned),
            snapshot: p.snapshot_id().into(),
            rule: p.rule_version().into(),
            calculated_at: p.calculated_at().into(),
        }
    }
    /// Original feedback operation, not purchase operation or new freeze ID.
    pub fn feedback_operation_id(&self) -> &str {
        &self.operation
    }
    pub fn owner_id(&self) -> &str {
        &self.owner
    }
    pub fn report_id(&self) -> &str {
        &self.report
    }
    pub fn report_account_ids(&self) -> &[String] {
        &self.report_accounts
    }
    pub fn source_id(&self) -> &str {
        &self.source
    }
    pub fn account_id(&self) -> &str {
        &self.account
    }
    pub fn transaction_id(&self) -> &str {
        &self.transaction
    }
    pub fn original_card_id(&self) -> Option<&str> {
        self.card.as_deref()
    }
    /// Historical snapshot ONLY, never a monotonic feedback revision.
    pub fn original_snapshot_id(&self) -> &str {
        &self.snapshot
    }
    pub fn original_rule_version(&self) -> &str {
        &self.rule
    }
    pub fn original_calculated_at(&self) -> &str {
        &self.calculated_at
    }
}

/// Display allow-list: no original IDs, provider details or versions.
#[derive(Debug, PartialEq, Eq)]
pub struct InspectionView {
    pub state: InspectionState,
    pub message: &'static str,
    pub last_four: Option<String>,
    pub card_has_active_locks: Option<bool>,
    pub account_outgoing_restricted_by_card_locks: Option<bool>,
    pub card_public_reasons: Vec<&'static str>,
    pub restriction_explanation: Option<&'static str>,
    pub unlock_instruction: &'static str,
}
/// Backend-held observation: no executable preview, PreparedFreeze conversion,
/// tool registration, write callback or public credential.
pub struct FeedbackCardInspection {
    original: OriginalFeedbackReference,
    catalog_revision: Option<String>,
    view: InspectionView,
}
impl FeedbackCardInspection {
    pub fn view(&self) -> &InspectionView {
        &self.view
    }
    pub fn original_for_backend(&self) -> &OriginalFeedbackReference {
        &self.original
    }
    pub fn card_revision_at_read_for_backend(&self) -> Option<&str> {
        self.catalog_revision.as_deref()
    }
    /// Fresh pool load + checked projection + current full card read. Caller
    /// must inject CURRENT trusted scope; this cannot discover session revocation.
    /// Equality is a readonly observation, not atomic protection of a write.
    pub async fn recheck(
        &self,
        pool: &PgPool,
        current_scope: &TrustedScope,
        trusted_source_id: &str,
        cards: &mut PgConnection,
    ) -> Result<Self, InspectionIssue> {
        let current = inspect_saved_feedback(
            pool,
            current_scope,
            &self.original.operation,
            trusted_source_id,
            cards,
        )
        .await?;
        if current.original != self.original
            || current.catalog_revision != self.catalog_revision
            || current.view != self.view
        {
            return Err(InspectionIssue::FactsChanged);
        }
        Ok(current)
    }
}
fn billing_issue(issue: StoreError) -> InspectionIssue {
    match issue {
        StoreError::InvalidInput => InspectionIssue::InvalidInput,
        StoreError::ResourceUnavailable => InspectionIssue::ResourceUnavailable,
        StoreError::CorruptRecord => InspectionIssue::InconsistentEvidence,
        _ => InspectionIssue::Unavailable,
    }
}

/// Read committed original evidence. Source routing, current scope and the
/// independently authorized READ ONLY / READ COMMITTED card connection/schema
/// come from trusted assembly, not client claims. Equal account IDs across
/// arbitrary databases do not prove a shared adopted namespace.
pub async fn inspect_saved_feedback(
    pool: &PgPool,
    current_scope: &TrustedScope,
    feedback_operation_id: &str,
    trusted_source_id: &str,
    cards: &mut PgConnection,
) -> Result<FeedbackCardInspection, InspectionIssue> {
    if trusted_source_id.trim().is_empty() {
        return Err(InspectionIssue::InvalidInput);
    }
    let saved = load_feedback(pool, current_scope, feedback_operation_id)
        .await
        .map_err(billing_issue)?
        .ok_or(InspectionIssue::ResourceUnavailable)?;
    let p = saved
        .checked_projection(current_scope)
        .map_err(billing_issue)?;
    if p.operation_id() != feedback_operation_id {
        return Err(InspectionIssue::InconsistentEvidence);
    }
    if p.reference().source_id != trusted_source_id {
        return Err(InspectionIssue::SourceMismatch);
    }
    let state = match p.card_disposition() {
        CardDisposition::StopMine => InspectionState::StopMine,
        CardDisposition::StopUnsure => InspectionState::StopUnsure,
        CardDisposition::StopNoCard => InspectionState::StopNoCard,
        CardDisposition::AssociationUnverified => InspectionState::CardAssociationVerified,
    };
    let mut view = InspectionView {
        state,
        message: match state {
            InspectionState::StopMine => "原反馈为本人交易，停止本次异常反馈锁卡流程。",
            InspectionState::StopUnsure => "原反馈为不确定，停止本次异常反馈锁卡流程。",
            InspectionState::StopNoCard => "原反馈没有对应卡片，停止处理，不按尾号或其他资料猜卡。",
            InspectionState::CardAssociationVerified => {
                "原反馈与本人当前卡片目录核对一致；尚未执行冻结，仍须完成确认、认证和最终复核。"
            }
        },
        last_four: None,
        card_has_active_locks: None,
        account_outgoing_restricted_by_card_locks: None,
        card_public_reasons: Vec::new(),
        restriction_explanation: None,
        unlock_instruction:
            "解锁只能到线下银行办理；多条有效锁必须全部解除，才能解除卡锁造成的出账限制。",
    };
    let mut revision = None;
    if state == InspectionState::CardAssociationVerified {
        let catalog = ReadOnlyCatalogInspection::read_on_connection(
            cards,
            p.owner_user_id(),
            &p.reference().account_id,
        )
        .await
        .map_err(InspectionIssue::CardData)?;
        let position = catalog
            .card_ids_for_backend()
            .position(|id| Some(id) == p.card_id())
            .ok_or(InspectionIssue::CardAssociationUnavailable)?;
        let list = catalog.view();
        let card = &list.cards[position];
        view.last_four = Some(card.last_four.clone());
        view.card_has_active_locks = Some(card.card_has_active_locks);
        view.account_outgoing_restricted_by_card_locks =
            Some(list.account_outgoing_restricted_by_card_locks);
        view.card_public_reasons = card.public_reasons.clone();
        view.restriction_explanation = Some(list.restriction_explanation);
        revision = Some(catalog.local_revision_for_backend().into());
    }
    Ok(FeedbackCardInspection {
        original: OriginalFeedbackReference::from_projection(&p),
        catalog_revision: revision,
        view,
    })
}
