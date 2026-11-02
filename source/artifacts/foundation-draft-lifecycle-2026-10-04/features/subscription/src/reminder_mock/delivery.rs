use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReminderKind {
    ChargeAdvance,
    CancellationCheck,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeliveryOutcome {
    SimulatedSuccess,
    DefiniteFailure,
    Unknown,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ReminderMessage {
    pub fictional: bool,
    pub user_id: String,
    pub subscription_id: String,
    pub kind: ReminderKind,
    pub cycle_ref: String,
    #[serde(serialize_with = "serialize_date")]
    pub expected_date: NaiveDate,
    pub channel: &'static str,
    pub content: &'static str,
}
pub trait MockInAppSink {
    /// Fixed test outcome only. No real notification transport is connected.
    fn simulate_in_app(&mut self, message: &ReminderMessage) -> DeliveryOutcome;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ItemState {
    NotDue,
    Disabled,
    ErrorBlocked,
    SkippedExpired,
    SimulatedSuccess,
    AlreadySimulated,
    RetryNextDay,
    RetryWaiting,
    RetryExhausted,
    UnknownNeedsReview,
    SettingsNeedsInput,
}
#[derive(Debug, Serialize)]
pub struct DeliveryItem {
    pub subscription_id: String,
    pub kind: ReminderKind,
    pub state: ItemState,
    #[serde(serialize_with = "serialize_date")]
    pub expected_date: NaiveDate,
    pub late: bool,
}
#[derive(Debug, Serialize)]
pub struct DeliveryReport {
    pub fictional: bool,
    pub production_registered: bool,
    pub notification_sent: bool,
    pub audit_integrated: bool,
    pub database_connected: bool,
    pub channel: &'static str,
    #[serde(serialize_with = "serialize_date")]
    pub business_date: NaiveDate,
    pub items: Vec<DeliveryItem>,
}
#[derive(Clone)]
pub(super) struct Attempt {
    pub(super) day: NaiveDate,
    pub(super) outcome: DeliveryOutcome,
    pub(super) count: u32,
}

impl MemoryReminderMock {
    pub fn current_schedule_mock(
        &self,
        ledger: &MemoryConfirmationMock,
        user: &str,
        id: &str,
    ) -> Result<&Schedule, &'static str> {
        let entry = owned_entry(ledger, user, id)?;
        if self
            .targets
            .get(&(user.into(), id.into()))
            .is_some_and(|old| !same_target(old, entry))
        {
            return Err("SERVICE_NEEDS_REVIEW");
        }
        self.profiles
            .get(&(user.into(), id.into()))
            .ok_or("DATE_CONFIRMATION_REQUIRED")
    }
    pub fn scan_mock(
        &mut self,
        ledger: &MemoryConfirmationMock,
        user: &str,
        now: &str,
        sink: &mut impl MockInAppSink,
    ) -> Result<DeliveryReport, &'static str> {
        ledger.list_mock(user).map_err(|_| "LEDGER_NOT_READY")?;
        let time = DateTime::parse_from_rfc3339(now).map_err(|_| "TIME_INVALID")?;
        if self.last_scan.get(user).is_some_and(|old| &time < old) {
            return Err("CLOCK_MOVED_BACKWARD");
        }
        let today = business_day(now)?;
        // Finish all fallible date/source checks before invoking any mock sink.
        let mut plans = Vec::new();
        let mut items = Vec::new();
        let mut rollovers = Vec::new();
        for ((owner, id), schedule) in &self.profiles {
            if owner != user {
                continue;
            }
            let entry = owned_entry(ledger, user, id)?;
            if !same_target(&self.targets[&(owner.clone(), id.clone())], entry) {
                return Err("SERVICE_NEEDS_REVIEW");
            }
            let mut schedule = schedule.clone();
            let kind = if entry.status == LedgerStatus::Cancelling {
                ReminderKind::CancellationCheck
            } else {
                ReminderKind::ChargeAdvance
            };
            let mut timing = schedule.clone();
            if kind == ReminderKind::CancellationCheck {
                timing.days_before = 0;
            }
            if self.validate_schedule(&timing).is_err() {
                items.push(DeliveryItem {
                    subscription_id: id.clone(),
                    kind,
                    state: ItemState::SettingsNeedsInput,
                    expected_date: schedule.expected_date,
                    late: today > schedule.expected_date,
                });
                continue;
            }
            if kind == ReminderKind::ChargeAdvance
                && entry.status == LedgerStatus::Active
                && schedule.enabled
                && today > schedule.expected_date
            {
                let key = (user.to_owned(), id.clone(), kind, schedule.key_cycle());
                let state = match self.attempts.get(&key).map(|a| a.outcome) {
                    Some(DeliveryOutcome::SimulatedSuccess) => ItemState::AlreadySimulated,
                    Some(DeliveryOutcome::Unknown) => ItemState::UnknownNeedsReview,
                    _ => ItemState::SkippedExpired,
                };
                items.push(DeliveryItem {
                    subscription_id: id.clone(),
                    kind,
                    state,
                    expected_date: schedule.expected_date,
                    late: true,
                });
                // Skip old periods but still consider today's daily occurrence in
                // this same scan; no backlog of historic notifications is sent.
                schedule = schedule.next_after_mock(today.pred_opt().ok_or("TIME_INVALID")?)?;
                let version = self.versions[&(user.into(), id.clone())]
                    .checked_add(1)
                    .ok_or("SETTINGS_UNAVAILABLE")?;
                rollovers.push(((user.to_owned(), id.clone()), schedule.clone(), version));
                if self.validate_schedule(&schedule).is_err() {
                    items.push(DeliveryItem {
                        subscription_id: id.clone(),
                        kind,
                        state: ItemState::SettingsNeedsInput,
                        expected_date: schedule.expected_date,
                        late: false,
                    });
                    continue;
                }
            }
            let due = if kind == ReminderKind::CancellationCheck {
                schedule.expected_date
            } else {
                schedule
                    .expected_date
                    .checked_sub_days(Days::new(schedule.days_before.into()))
                    .ok_or("DATE_INVALID")?
            };
            plans.push((id.clone(), entry.status, schedule, kind, due));
        }
        for (key, schedule, version) in rollovers {
            self.profiles.insert(key.clone(), schedule);
            self.versions.insert(key, version);
        }
        for (id, status, schedule, kind, due) in plans {
            let key = (user.to_owned(), id.clone(), kind, schedule.key_cycle());
            let previous = self.attempts.get(&key);
            let state = if status == LedgerStatus::Error {
                ItemState::ErrorBlocked
            } else if kind == ReminderKind::ChargeAdvance && !schedule.enabled {
                ItemState::Disabled
            } else if previous.is_some_and(|a| a.outcome == DeliveryOutcome::SimulatedSuccess) {
                ItemState::AlreadySimulated
            } else if kind == ReminderKind::ChargeAdvance && today > schedule.expected_date {
                ItemState::SkippedExpired
            } else if today < due {
                ItemState::NotDue
            } else if previous.is_some_and(|a| a.outcome == DeliveryOutcome::Unknown) {
                ItemState::UnknownNeedsReview
            } else if !self.legacy_replay && previous.is_some_and(|a| a.count >= 2) {
                ItemState::RetryExhausted
            } else if previous.is_some_and(|a| a.day == today) {
                ItemState::RetryWaiting
            } else {
                let message = ReminderMessage {
                    fictional: true,
                    user_id: user.into(),
                    subscription_id: id.clone(),
                    kind,
                    cycle_ref: schedule.key_cycle(),
                    expected_date: schedule.expected_date,
                    channel: "IN_APP_MOCK_ONLY",
                    content: if kind == ReminderKind::CancellationCheck {
                        "预计日期已到或已过，请核对账单及商户处理情况；SafeFlow只登记取消意图，未确认商户已取消。"
                    } else {
                        "预计将扣款，请核对订阅和商户安排；预计日期不是商户承诺，此处仅模拟应用内提醒。"
                    },
                };
                let count = previous.map_or(1, |a| a.count.saturating_add(1));
                let outcome = sink.simulate_in_app(&message);
                self.attempts.insert(
                    key,
                    Attempt {
                        day: today,
                        outcome,
                        count,
                    },
                );
                match outcome {
                    DeliveryOutcome::SimulatedSuccess => ItemState::SimulatedSuccess,
                    DeliveryOutcome::DefiniteFailure if !self.legacy_replay && count >= 2 => {
                        ItemState::RetryExhausted
                    }
                    DeliveryOutcome::DefiniteFailure => ItemState::RetryNextDay,
                    DeliveryOutcome::Unknown => ItemState::UnknownNeedsReview,
                }
            };
            items.push(DeliveryItem {
                subscription_id: id,
                kind,
                state,
                expected_date: schedule.expected_date,
                late: today > schedule.expected_date,
            });
        }
        self.last_scan.insert(user.into(), time);
        Ok(DeliveryReport {
            fictional: true,
            production_registered: false,
            notification_sent: false,
            audit_integrated: false,
            database_connected: false,
            channel: "IN_APP_MOCK_ONLY",
            business_date: today,
            items,
        })
    }
}
