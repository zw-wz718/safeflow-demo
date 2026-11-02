//! Card-scoped saved protection history over an authorized readonly connection.
//! Opaque backend-held lists/choices, not public tokens, authentication, a freeze
//! retry, an audit log, a chronological timeline or an online unlock endpoint.
use crate::{
    freeze::{checked_receipt, load_freeze_on_connection},
    freeze_completion::{
        require_owner, require_read_boundary, FreezeReadbackIssue, FreezeReadbackRequest,
        FreezeReadbackResolution,
    },
    read_complete_facts, read_revision, valid_id, StoreError,
};
use safeflow_card_facts::{
    account_service::AccountCardFacts,
    operation_records::{HistoricalLockReceipt, OriginalLockEffect},
    operation_replay::LockReplayParameters,
    LockSource,
};
use sqlx::PgConnection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectionHistoryIssue {
    ResourceUnavailable,
    ReadOnlyBoundaryRequired,
    IncompleteData,
    InconsistentData,
    Unavailable,
    FactsChanged,
    SelectionRequired,
    InvalidSelection,
    Readback(FreezeReadbackIssue),
}
impl ProtectionHistoryIssue {
    pub fn message(self) -> &'static str {
        match self {
            Self::ResourceUnavailable => "无法核验本人对应卡片或账户，请核对资料。",
            Self::IncompleteData => "尚未取得完整保护记录，不能当成没有保护记录。",
            Self::InconsistentData => "保护记录关联存在不一致，需要核对后再处理。",
            Self::FactsChanged => "保护记录或卡片资料已变化，请重新查询。",
            Self::SelectionRequired => "请明确选择本次保护记录清单中的一条记录。",
            Self::InvalidSelection => "本次选择不在保留的保护记录清单中，请重新核对。",
            Self::ReadOnlyBoundaryRequired | Self::Unavailable | Self::Readback(_) => {
                "暂时无法核实保护记录，请保留原操作继续核对，不要重复冻结。"
            }
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordedProtectionAction {
    CreatedLock,
    ReusedExistingLock,
}
impl RecordedProtectionAction {
    pub fn explanation(self) -> &'static str {
        match self {
            Self::CreatedLock => "本条记录记载当时新建了一把锁；是否仍有效需看当前状态。",
            Self::ReusedExistingLock => {
                "本条记录记载复用已有有效锁，并留下本次保护原因记录；不代表新增一把锁。"
            }
        }
    }
}
/// Positions are for this backend-retained list ONLY. No raw IDs, reason codes,
/// keys, event/transaction/feedback references, times or risk internals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectionRecordView {
    pub position: usize,
    pub original_action: RecordedProtectionAction,
    pub public_reason: &'static str,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectionLockView {
    pub position: usize,
    pub currently_active: bool,
    pub public_reason: &'static str,
    pub protection_records: Vec<ProtectionRecordView>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardProtectionHistoryView {
    pub last_four: String,
    pub locks: Vec<ProtectionLockView>,
    pub total_saved_protection_records: usize,
    pub card_has_active_locks: bool,
    pub account_outgoing_restricted_by_card_locks: bool,
    pub message: &'static str,
    pub ordering_explanation: &'static str,
    pub unlock_instruction: &'static str,
}
struct SavedProtectionRecord {
    request: FreezeReadbackRequest,
    receipt: HistoricalLockReceipt,
}
struct SavedProtectionLock {
    id: String,
    source: LockSource,
    active: bool,
}
/// Private fields, no Deserialize/Serialize/Debug/Clone. Source identity and
/// completeness are not established by a caller echoing a public view.
pub struct ReadOnlyCardProtectionHistory {
    owner: String,
    account: String,
    card: String,
    last_four: String,
    facts: AccountCardFacts,
    locks: Vec<SavedProtectionLock>,
    records: Vec<SavedProtectionRecord>,
}
impl ReadOnlyCardProtectionHistory {
    pub async fn read_on_connection(
        c: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
        card: &str,
    ) -> Result<Self, ProtectionHistoryIssue> {
        use ProtectionHistoryIssue as I;
        if [trusted_owner, account, card].iter().any(|s| !valid_id(s)) {
            return Err(I::ResourceUnavailable);
        }
        require_read_boundary(c).await.map_err(read_issue)?;
        require_owner(c, trusted_owner, account)
            .await
            .map_err(read_issue)?;
        let before = read_revision(c, account).await.map_err(store_issue)?;
        let facts = read_complete_facts(c, trusted_owner, account)
            .await
            .map_err(store_issue)?;
        let selected = facts
            .cards
            .iter()
            .find(|v| v.card_id == card)
            .ok_or(I::ResourceUnavailable)?;
        let last_four = selected.last_four.clone();
        let rows:Vec<(String,String,String)>=sqlx::query_as("SELECT lock_id,source,state FROM card_locks_v1 WHERE card_id=$1 ORDER BY lock_id COLLATE \"C\"")
            .bind(card).fetch_all(&mut *c).await.map_err(|_|I::Unavailable)?;
        let mut locks = Vec::new();
        for (id, source, state) in rows {
            let source = match source.as_str() {
                "USER_SELF" => LockSource::UserSelf,
                "ANOMALY_DISPUTE" => LockSource::AnomalyDispute,
                "RISK_CONTROL" => LockSource::RiskControl,
                _ => return Err(I::InconsistentData),
            };
            let active = match state.as_str() {
                "ACTIVE" => true,
                "RELEASED" => false,
                _ => return Err(I::InconsistentData),
            };
            if !valid_id(&id) {
                return Err(I::InconsistentData);
            }
            locks.push(SavedProtectionLock { id, source, active });
        }
        // Read by the already-owned stable card, then verify EVERY row's owner
        // and account: malformed rows must not disappear through an owner filter.
        let operations:Vec<(String,String,String)>=sqlx::query_as("SELECT owner_id,account_id,operation_id FROM card_freeze_operations_v1 WHERE card_id=$1 ORDER BY operation_id COLLATE \"C\"")
            .bind(card).fetch_all(&mut *c).await.map_err(|_|I::Unavailable)?;
        let mut records = Vec::new();
        for (owner, record_account, operation_id) in operations {
            if owner != trusted_owner || record_account != account {
                return Err(I::InconsistentData);
            }
            let (bundle, scope) =
                load_freeze_on_connection(c, trusted_owner, account, &operation_id)
                    .await
                    .map_err(store_issue)?
                    .ok_or(I::InconsistentData)?;
            if bundle.operation.card_id != card {
                return Err(I::InconsistentData);
            }
            let receipt = checked_receipt(trusted_owner, bundle.clone()).map_err(store_issue)?;
            if !locks
                .iter()
                .any(|l| l.id == receipt.lock_id && l.source == receipt.source)
            {
                return Err(I::InconsistentData);
            }
            let op = &bundle.operation;
            let request = FreezeReadbackRequest::new(
                trusted_owner,
                account,
                &op.operation_id,
                &op.original_key,
                LockReplayParameters {
                    card_id: op.card_id.clone(),
                    source: op.source,
                    reason: op.reason,
                    event_id: op.event_id.clone(),
                    transaction_id: op.transaction_id.clone(),
                    feedback_id: op.feedback_id.clone(),
                },
                scope,
            )
            .map_err(|_| I::InconsistentData)?;
            records.push(SavedProtectionRecord { request, receipt });
        }
        // Verify complete associations, including extra/orphan reason/receipt
        // rows. A lock without verifiable saved operations is NOT empty history.
        for lock in &locks {
            let count = records
                .iter()
                .filter(|r| r.receipt.lock_id == lock.id)
                .count() as i64;
            let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM card_freeze_operations_v1 WHERE lock_id=$1),(SELECT count(*) FROM card_freeze_reasons_v1 WHERE lock_id=$1),(SELECT count(*) FROM card_freeze_receipts_v1 WHERE lock_id=$1)")
                .bind(&lock.id).fetch_one(&mut *c).await.map_err(|_|I::Unavailable)?;
            if count == 0 {
                return Err(I::IncompleteData);
            }
            let created = created_record_count(&records, &lock.id);
            if created == 0 {
                return Err(I::IncompleteData);
            }
            if created != 1 {
                return Err(I::InconsistentData);
            }
            if counts != (count, count, count) {
                return Err(I::InconsistentData);
            }
        }
        if read_revision(c, account).await.map_err(store_issue)? != before {
            return Err(I::FactsChanged);
        }
        require_owner(c, trusted_owner, account)
            .await
            .map_err(read_issue)?;
        require_read_boundary(c).await.map_err(read_issue)?;
        Ok(Self {
            owner: trusted_owner.into(),
            account: account.into(),
            card: card.into(),
            last_four,
            facts,
            locks,
            records,
        })
    }
    pub fn view(&self) -> CardProtectionHistoryView {
        CardProtectionHistoryView{
            last_four:self.last_four.clone(),
            locks:self.locks.iter().enumerate().map(|(i,l)|ProtectionLockView{
                position:i+1,currently_active:l.active,public_reason:l.source.public_reason(),
                protection_records:self.records.iter().enumerate().filter(|(_,r)|r.receipt.lock_id==l.id).map(|(j,r)|ProtectionRecordView{
                    position:j+1,original_action:match r.receipt.original_effect{OriginalLockEffect::Created=>RecordedProtectionAction::CreatedLock,OriginalLockEffect::ReusedActive=>RecordedProtectionAction::ReusedExistingLock},public_reason:r.receipt.public_reason,
                }).collect(),
            }).collect(),
            total_saved_protection_records:self.records.len(),
            card_has_active_locks:self.locks.iter().any(|l|l.active),
            account_outgoing_restricted_by_card_locks:self.facts.outgoing_restricted(),
            message:if self.records.is_empty(){"本次完整查询未发现这张卡已保存的保护操作记录；这不表示账户已获准出账。"}else{"以下是已保存的保护记录；当时新建或复用锁，与当前这把锁是否有效分别展示。查看记录不会再次冻结。"},
            ordering_explanation:"选项序号只对应本次保留的清单，不表示发生先后；现有记录没有可信时间，本次不展示时间线。",
            unlock_instruction:"解除请到线下银行办理；多条有效锁必须全部解除，才能解除卡锁造成的出账限制。",
        }
    }
    pub fn select_record(
        &self,
        position: Option<usize>,
    ) -> Result<SelectedProtectionRecord, ProtectionHistoryIssue> {
        let i = position
            .ok_or(ProtectionHistoryIssue::SelectionRequired)?
            .checked_sub(1)
            .ok_or(ProtectionHistoryIssue::InvalidSelection)?;
        let r = self
            .records
            .get(i)
            .ok_or(ProtectionHistoryIssue::InvalidSelection)?;
        Ok(SelectedProtectionRecord {
            owner: self.owner.clone(),
            account: self.account.clone(),
            card: self.card.clone(),
            request: r.request.clone(),
            receipt: r.receipt.clone(),
        })
    }
}
/// Backend-held original binding, not a public token or executable operation.
pub struct SelectedProtectionRecord {
    owner: String,
    account: String,
    card: String,
    request: FreezeReadbackRequest,
    receipt: HistoricalLockReceipt,
}
impl SelectedProtectionRecord {
    /// Re-authorize the readonly boundary on each call. New current lock facts
    /// are expected: an old historical Created result cannot assert still-active.
    pub async fn query_on_connection(
        &self,
        c: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
    ) -> Result<FreezeReadbackResolution, ProtectionHistoryIssue> {
        if trusted_owner != self.owner || account != self.account {
            return Err(ProtectionHistoryIssue::ResourceUnavailable);
        }
        let result = self
            .request
            .query(c)
            .await
            .map_err(ProtectionHistoryIssue::Readback)?;
        if let FreezeReadbackResolution::Recorded(v) = &result {
            if v.historical_receipt() != &self.receipt
                || v.historical_receipt().card_id != self.card
            {
                return Err(ProtectionHistoryIssue::InconsistentData);
            }
        }
        Ok(result)
    }
}
fn read_issue(e: FreezeReadbackIssue) -> ProtectionHistoryIssue {
    match e {
        FreezeReadbackIssue::ResourceUnavailable => ProtectionHistoryIssue::ResourceUnavailable,
        FreezeReadbackIssue::ReadOnlyBoundaryRequired => {
            ProtectionHistoryIssue::ReadOnlyBoundaryRequired
        }
        _ => ProtectionHistoryIssue::Unavailable,
    }
}
fn store_issue(e: StoreError) -> ProtectionHistoryIssue {
    match e {
        StoreError::CatalogUninitialized => ProtectionHistoryIssue::IncompleteData,
        StoreError::InconsistentRecord => ProtectionHistoryIssue::InconsistentData,
        StoreError::SnapshotChanged => ProtectionHistoryIssue::FactsChanged,
        StoreError::ResourceUnavailable => ProtectionHistoryIssue::ResourceUnavailable,
        _ => ProtectionHistoryIssue::Unavailable,
    }
}

fn created_record_count(records: &[SavedProtectionRecord], lock_id: &str) -> usize {
    records
        .iter()
        .filter(|r| {
            r.receipt.lock_id == lock_id && r.receipt.original_effect == OriginalLockEffect::Created
        })
        .count()
}
