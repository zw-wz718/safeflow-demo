use super::*;
use crate::{
    recognition::{CycleStatus, GroupStatus},
    MockContext, Snapshot,
};
use chrono::Datelike;

/// A date suggestion only. It is neither a merchant promise nor a completed charge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Prediction {
    pub cycle: Cycle,
    #[serde(serialize_with = "serialize_date")]
    pub anchor_date: NaiveDate,
    pub period_index: u32,
    #[serde(serialize_with = "serialize_date")]
    pub expected_date: NaiveDate,
    pub actual_charge_count: usize,
    pub needs_user_confirmation: bool,
}

pub fn predict_mock(
    ledger: &MemoryConfirmationMock,
    context: &MockContext,
    snapshot: &Snapshot,
    subscription_id: &str,
) -> Result<Prediction, &'static str> {
    let entry = owned_entry(ledger, &context.user_id, subscription_id)?;
    if entry.status == LedgerStatus::Error {
        return Err("REGISTRATION_NEEDS_REVIEW");
    }
    let review = ledger
        .discover_mock(context, snapshot)
        .map_err(|_| "CONSUMPTION_NOT_READY")?;
    if !review.matching_executed {
        return Err("CONSUMPTION_NOT_READY");
    }
    let groups: Vec<_> = review
        .groups
        .iter()
        .filter(|g| {
            g.matched_subscription_ids
                .iter()
                .any(|id| id == subscription_id)
        })
        .collect();
    if groups.len() != 1 {
        return Err("SERVICE_NEEDS_REVIEW");
    }
    if groups[0].matched_subscription_ids.len() != 1
        || (groups[0].disposition != crate::managed_ledger_mock::Disposition::AlreadyManaged
            && groups[0].reason != "CANCELLATION_INTENT_NEEDS_REVIEW")
    {
        return Err("SERVICE_NEEDS_REVIEW");
    }
    predict_group(&groups[0].group, snapshot)
}

/// Read-only preview before registration; no profile or notification is created.
pub fn predict_candidate_mock(
    ledger: &MemoryConfirmationMock,
    context: &MockContext,
    snapshot: &Snapshot,
    index: usize,
) -> Result<Prediction, &'static str> {
    ledger
        .list_mock(&context.user_id)
        .map_err(|_| "LEDGER_NOT_READY")?;
    let review = ledger
        .discover_mock(context, snapshot)
        .map_err(|_| "CONSUMPTION_NOT_READY")?;
    if !review.matching_executed {
        return Err("CONSUMPTION_NOT_READY");
    }
    let item = review.groups.get(index).ok_or("DATE_NEEDS_INPUT")?;
    if item.disposition != crate::managed_ledger_mock::Disposition::NotInLedger {
        return Err("SERVICE_NEEDS_REVIEW");
    }
    predict_group(&item.group, snapshot)
}

fn predict_group(
    group: &crate::recognition::SuspectedGroup,
    snapshot: &Snapshot,
) -> Result<Prediction, &'static str> {
    if group.status != GroupStatus::Suspected {
        return Err("DATE_NEEDS_INPUT");
    }
    let matches: Vec<_> = group
        .cycles
        .iter()
        .filter(|c| c.status == CycleStatus::Matches)
        .collect();
    if matches.len() != 1 {
        return Err("DATE_NEEDS_INPUT");
    }
    let assessment = matches[0];
    let dates: Result<Vec<_>, _> = group
        .actual_charges
        .iter()
        .map(|c| business_day(&c.completed_at))
        .collect();
    let dates = dates?;
    let anchor_date = *dates.iter().min().ok_or("DATE_NEEDS_INPUT")?;
    let last = *assessment
        .observed_period_indices
        .iter()
        .max()
        .ok_or("DATE_NEEDS_INPUT")?;
    let period_index = u32::try_from(last)
        .ok()
        .and_then(|i| i.checked_add(1))
        .ok_or("DATE_NEEDS_INPUT")?;
    let expected_date = occurrence_date(anchor_date, assessment.cycle, period_index)?;
    // Do not invent missing payments or silently roll an overdue prediction forward.
    if expected_date < business_day(&snapshot.window.as_of)? {
        return Err("DATE_NEEDS_INPUT");
    }
    Ok(Prediction {
        cycle: assessment.cycle,
        anchor_date,
        period_index,
        expected_date,
        actual_charge_count: dates.len(),
        needs_user_confirmation: true,
    })
}

pub fn occurrence_date(
    anchor: NaiveDate,
    cycle: Cycle,
    index: u32,
) -> Result<NaiveDate, &'static str> {
    match cycle {
        Cycle::Day => anchor.checked_add_days(Days::new(index.into())),
        Cycle::Week => anchor.checked_add_days(Days::new(u64::from(index) * 7)),
        Cycle::CalendarMonth => anchor.checked_add_months(chrono::Months::new(index)),
    }
    .ok_or("DATE_INVALID")
}

/// Explicit mock form values. Ambiguous recognition requires the user to fill
/// the cycle/date; no guessed date can enter the scheduler without confirmation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Schedule {
    pub cycle: Cycle,
    #[serde(serialize_with = "serialize_date")]
    pub anchor_date: NaiveDate,
    pub period_index: u32,
    #[serde(serialize_with = "serialize_date")]
    pub expected_date: NaiveDate,
    pub enabled: bool,
    pub days_before: u8,
}
impl Schedule {
    pub fn from_prediction(p: &Prediction) -> Self {
        Self {
            cycle: p.cycle,
            anchor_date: p.anchor_date,
            period_index: p.period_index,
            expected_date: p.expected_date,
            enabled: true,
            days_before: if p.cycle == Cycle::Day { 0 } else { 3 },
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_legacy()?;
        if self.cycle == Cycle::Day && self.days_before != 0 {
            return Err("DAILY_SAME_DAY_REQUIRED");
        }
        Ok(())
    }
    pub(super) fn validate_legacy(&self) -> Result<(), &'static str> {
        if self.period_index == 0 || self.days_before > 30 {
            return Err("SETTINGS_NEEDS_INPUT");
        }
        let previous = occurrence_date(self.anchor_date, self.cycle, self.period_index - 1)?;
        let natural = occurrence_date(self.anchor_date, self.cycle, self.period_index)?;
        let span = natural.signed_duration_since(previous).num_days();
        if i64::from(self.days_before) > span {
            return Err("LEAD_EXCEEDS_CYCLE");
        }
        if self.expected_date <= previous {
            return Err("DATE_NEEDS_INPUT");
        }
        let next = occurrence_date(
            self.anchor_date,
            self.cycle,
            self.period_index.checked_add(1).ok_or("DATE_INVALID")?,
        )?;
        if self.expected_date >= next {
            return Err("DATE_NEEDS_INPUT");
        }
        let reminder = self
            .expected_date
            .checked_sub_days(Days::new(self.days_before.into()))
            .ok_or("DATE_INVALID")?;
        if reminder < previous {
            return Err("LEAD_EXCEEDS_CYCLE");
        }
        Ok(())
    }
    pub(super) fn key_cycle(&self) -> String {
        let name = match self.cycle {
            Cycle::Day => "DAY",
            Cycle::Week => "WEEK",
            Cycle::CalendarMonth => "MONTH",
        };
        format!("{name}:{}:{}", self.anchor_date, self.period_index)
    }
    /// An edit affects this occurrence only. All later dates use the original anchor.
    pub fn next_after_mock(&self, day: NaiveDate) -> Result<Self, &'static str> {
        let floor = day.max(self.expected_date);
        let days = floor.signed_duration_since(self.anchor_date).num_days();
        let rough = match self.cycle {
            Cycle::Day => days,
            Cycle::Week => days.div_euclid(7),
            Cycle::CalendarMonth => {
                i64::from(floor.year() - self.anchor_date.year()) * 12 + i64::from(floor.month())
                    - i64::from(self.anchor_date.month())
            }
        };
        let mut index = self
            .period_index
            .checked_add(1)
            .ok_or("DATE_INVALID")?
            .max(u32::try_from(rough).unwrap_or(0));
        let mut date = occurrence_date(self.anchor_date, self.cycle, index)?;
        while date <= floor {
            index = index.checked_add(1).ok_or("DATE_INVALID")?;
            date = occurrence_date(self.anchor_date, self.cycle, index)?;
        }
        let next = Self {
            period_index: index,
            expected_date: date,
            ..self.clone()
        };
        Ok(next)
    }
}
