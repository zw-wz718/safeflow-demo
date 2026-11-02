//! Read saved configuration and latest saved outcome per cycle. Never scan/send.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReminderAttemptStateMock {
    NotAttempted,
    SimulatedSuccess,
    Failed,
    RetryExhausted,
    NeedsReview,
}
impl From<DeliveryOutcome> for ReminderAttemptStateMock {
    fn from(value: DeliveryOutcome) -> Self {
        match value {
            DeliveryOutcome::SimulatedSuccess => Self::SimulatedSuccess,
            DeliveryOutcome::DefiniteFailure => Self::Failed,
            DeliveryOutcome::Unknown => Self::NeedsReview,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct ReminderAttemptViewMock {
    pub kind: ReminderKind,
    pub cycle_ref: String,
    pub last_attempt_date: String,
    pub state: ReminderAttemptStateMock,
    pub attempt_count: u32,
    pub retries_remaining: u8,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReminderPlanViewMock {
    pub fictional: bool,
    pub data_state: &'static str,
    pub schedule: Option<Schedule>,
    pub active_kind: Option<ReminderKind>,
    pub current_attempt: Option<ReminderAttemptStateMock>,
    pub current_attempt_count: Option<u32>,
    pub retries_remaining: Option<u8>,
    pub saved_cycle_outcomes: Vec<ReminderAttemptViewMock>,
    pub notification_sent: bool,
    pub message: &'static str,
}
impl MemoryReminderMock {
    pub fn plan_status_mock(
        &self,
        ledger: &MemoryConfirmationMock,
        user: &str,
        id: &str,
    ) -> Result<ReminderPlanViewMock, &'static str> {
        let entry = owned_entry(ledger, user, id)?;
        let key = (user.to_owned(), id.to_owned());
        let schedule = self.profiles.get(&key).cloned();
        let needs_review = entry.status == LedgerStatus::Error
            || schedule.as_ref().is_some_and(|s| s.validate().is_err())
            || [&entry.source_id, &entry.merchant_id, &entry.service_ref]
                .iter()
                .any(|value| value.as_deref().is_none_or(|s| s.trim().is_empty()))
            || self
                .targets
                .get(&key)
                .is_some_and(|target| !same_target(target, entry));
        let kind = if entry.status == LedgerStatus::Cancelling {
            ReminderKind::CancellationCheck
        } else {
            ReminderKind::ChargeAdvance
        };
        let current_attempt = schedule.as_ref().map(|s| {
            self.attempts
                .get(&(user.to_owned(), id.to_owned(), kind, s.key_cycle()))
                .map(attempt_state)
                .unwrap_or(ReminderAttemptStateMock::NotAttempted)
        });
        let current = schedule.as_ref().and_then(|s| {
            self.attempts
                .get(&(user.to_owned(), id.to_owned(), kind, s.key_cycle()))
        });
        let current_attempt_count = if needs_review {
            None
        } else {
            schedule.as_ref().map(|_| current.map_or(0, |a| a.count))
        };
        let retries_remaining = if needs_review {
            None
        } else {
            schedule.as_ref().map(|_| current.map_or(1, remaining))
        };
        let saved_cycle_outcomes = self
            .attempts
            .iter()
            .filter(|((owner, subscription, _, _), _)| owner == user && subscription == id)
            .map(|((_, _, kind, cycle), attempt)| ReminderAttemptViewMock {
                kind: *kind,
                cycle_ref: cycle.clone(),
                last_attempt_date: attempt.day.to_string(),
                state: attempt_state(attempt),
                attempt_count: attempt.count,
                retries_remaining: remaining(attempt),
            })
            .collect();
        Ok(ReminderPlanViewMock {
            fictional: true,
            data_state: if needs_review {
                "NEEDS_REVIEW"
            } else if schedule.is_some() {
                "CONFIGURED"
            } else {
                "NOT_CONFIGURED"
            },
            active_kind: if needs_review || schedule.is_none() {
                None
            } else {
                Some(kind)
            },
            current_attempt: if needs_review { None } else { current_attempt },
            schedule,
            current_attempt_count,
            retries_remaining,
            saved_cycle_outcomes,
            notification_sent: false,
            message: if needs_review {
                "订阅资料需核对；保留已有设置和尝试记录，不推断提醒可执行。"
            } else if current.is_some_and(|a| a.outcome == DeliveryOutcome::Unknown) {
                "这次提醒结果不明，需先核对原尝试；不会自动重发，未确认真实消息送达。"
            } else if current
                .is_some_and(|a| a.outcome == DeliveryOutcome::DefiniteFailure && a.count >= 2)
            {
                "本周期的补发次数已用完；不会再自动尝试，未确认真实消息送达。"
            } else if current.is_some_and(|a| a.outcome == DeliveryOutcome::DefiniteFailure) {
                "本次模拟处理明确失败；同日不重试，只可在允许时间补发一次，未发送真实通知。"
            } else if entry.status == LedgerStatus::Cancelling {
                "仅查询已保存计划；取消核对提醒按预计日期处理，普通续费开关不代表取消意图已撤回。未发送真实通知。"
            } else {
                "仅查询已保存计划和各周期最近一次模拟结果；未设置日期时不推测，不触发发送。"
            },
        })
    }
}

fn attempt_state(attempt: &delivery::Attempt) -> ReminderAttemptStateMock {
    if attempt.outcome == DeliveryOutcome::DefiniteFailure && attempt.count >= 2 {
        ReminderAttemptStateMock::RetryExhausted
    } else {
        attempt.outcome.into()
    }
}

fn remaining(attempt: &delivery::Attempt) -> u8 {
    u8::from(attempt.outcome == DeliveryOutcome::DefiniteFailure && attempt.count < 2)
}
