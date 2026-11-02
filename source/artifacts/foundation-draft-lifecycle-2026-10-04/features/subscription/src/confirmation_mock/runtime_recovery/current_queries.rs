//! Current business reads over an already validated, owner-bound snapshot.
use super::*;

#[derive(Clone, Debug, Serialize)]
pub struct SubscriptionListItemMock {
    pub subscription_id: String,
    pub current_state: LedgerStatus,
    pub service_data_state: &'static str,
    pub message: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct SubscriptionListViewMock {
    pub fictional: bool,
    pub items: Vec<SubscriptionListItemMock>,
}
#[derive(Clone, Debug, Serialize)]
pub struct CancellationHistoryItemMock {
    pub kind: MockIntentKind,
    pub occurred_at: String,
    pub state_after_operation: LedgerStatus,
}
#[derive(Clone, Debug, Serialize)]
pub struct CancellationStatusViewMock {
    pub fictional: bool,
    pub subscription_id: String,
    pub current_state: LedgerStatus,
    pub history: Vec<CancellationHistoryItemMock>,
    pub merchant_cancellation_confirmed: bool,
    pub message: &'static str,
}
impl SubscriptionReadSnapshotMock {
    pub fn list_subscriptions_mock(&self) -> Result<SubscriptionListViewMock> {
        let entries = self
            .state
            .ledger
            .list_mock(&self.owner)
            .map_err(RuntimeRecoveryIssue::Business)?;
        Ok(SubscriptionListViewMock {
            fictional: true,
            items: entries
                .iter()
                .map(|entry| {
                    let complete = [&entry.source_id, &entry.merchant_id, &entry.service_ref]
                        .iter()
                        .all(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()));
                    SubscriptionListItemMock {
                        subscription_id: entry.subscription_id.clone(),
                        current_state: entry.status,
                        service_data_state: if complete && entry.status != LedgerStatus::Error {
                            "AVAILABLE"
                        } else {
                            "NEEDS_REVIEW"
                        },
                        message: if entry.status == LedgerStatus::Error || !complete {
                            "订阅资料需核对；不能当成正常可操作记录。"
                        } else if entry.status == LedgerStatus::Cancelling {
                            "已记录取消意图，尚未确认商户已取消。"
                        } else {
                            "当前为本地登记状态，不代表商户合同状态。"
                        },
                    }
                })
                .collect(),
        })
    }
    pub fn cancellation_status_mock(&self, id: &str) -> Result<CancellationStatusViewMock> {
        let entries = self
            .state
            .ledger
            .list_mock(&self.owner)
            .map_err(RuntimeRecoveryIssue::Business)?;
        let entry = entries
            .iter()
            .find(|e| e.subscription_id == id)
            .ok_or(RuntimeRecoveryIssue::NotFound)?;
        let history = self
            .state
            .ledger
            .intent_events_mock(&self.owner)
            .map_err(RuntimeRecoveryIssue::Business)?
            .iter()
            .filter(|event| event.subscription_id == id)
            .map(|event| CancellationHistoryItemMock {
                kind: event.kind,
                occurred_at: event.occurred_at.clone(),
                state_after_operation: event.next_status,
            })
            .collect();
        Ok(CancellationStatusViewMock {
            fictional: true,
            subscription_id: id.into(),
            current_state: entry.status,
            history,
            merchant_cancellation_confirmed: false,
            message: match entry.status {
                LedgerStatus::Active => {
                    "当前未处于本地取消中；历史操作单独保留，不推断商户服务已恢复。"
                }
                LedgerStatus::Cancelling => "当前已记录取消意图；尚未确认商户已取消或停止扣款。",
                LedgerStatus::Error => "当前订阅资料异常，取消进度需核对；历史记录仍保留。",
            },
        })
    }
    pub fn reminder_plan_mock(
        &self,
        id: &str,
    ) -> Result<crate::reminder_mock::ReminderPlanViewMock> {
        self.state
            .reminders
            .plan_status_mock(&self.state.ledger, &self.owner, id)
            .map_err(RuntimeRecoveryIssue::Reminder)
    }
}
