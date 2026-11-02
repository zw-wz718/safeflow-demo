//! Fictional in-memory reminder configuration and delivery simulation only.
//! No production identity, authorization, timer, notification service or database.
use crate::{
    confirmation_mock::{MemoryConfirmationMock, MockGateOutcome, MockOperationContext},
    managed_ledger_mock::{LedgerEntry, LedgerStatus},
    recognition::Cycle,
};
use chrono::{DateTime, Days, FixedOffset, NaiveDate};
use serde::Serialize;
use std::collections::BTreeMap;

fn serialize_date<S: serde::Serializer>(
    value: &NaiveDate,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

mod delivery;
mod forecast;
mod preview;
pub use preview::SettingsPreviewMock;
mod status;
pub use delivery::{
    DeliveryOutcome, DeliveryReport, ItemState, MockInAppSink, ReminderKind, ReminderMessage,
};
pub use forecast::{occurrence_date, predict_candidate_mock, predict_mock, Prediction, Schedule};
pub use status::{ReminderAttemptStateMock, ReminderAttemptViewMock, ReminderPlanViewMock};

fn business_day(now: &str) -> Result<NaiveDate, &'static str> {
    Ok(DateTime::parse_from_rfc3339(now)
        .map_err(|_| "TIME_INVALID")?
        .with_timezone(&FixedOffset::east_opt(8 * 3600).expect("fixed business timezone"))
        .date_naive())
}
fn owned_entry<'a>(
    ledger: &'a MemoryConfirmationMock,
    user: &str,
    id: &str,
) -> Result<&'a LedgerEntry, &'static str> {
    ledger
        .list_mock(user)
        .map_err(|e| {
            if e == crate::confirmation_mock::MockIssue::ResourceUnavailable {
                "RESOURCE_UNAVAILABLE"
            } else {
                "LEDGER_NOT_READY"
            }
        })?
        .iter()
        .find(|e| e.subscription_id == id)
        .ok_or("RESOURCE_UNAVAILABLE")
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScheduleBinding {
    pub minimum_level: &'static str,
    pub context: MockOperationContext,
    pub target: LedgerEntry,
    pub ledger_revision: u64,
    pub settings_revision: u64,
    pub schedule: Schedule,
    pub prepared_at: String,
    pub expires_at: String,
}
pub trait MockScheduleGate {
    /// A fixed fixture confirms the full binding; never a production grant.
    fn verify_and_consume_mock(&mut self, binding: &ScheduleBinding) -> MockGateOutcome;
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ScheduleReceipt {
    pub fictional: bool,
    pub production_registered: bool,
    pub notification_sent: bool,
    pub operation_id: String,
    pub subscription_id: String,
    #[serde(serialize_with = "serialize_date")]
    pub expected_date: NaiveDate,
    pub days_before: u8,
    pub message: &'static str,
}
#[derive(Clone, Default)]
pub struct MemoryReminderMock {
    legacy_replay: bool,
    profiles: BTreeMap<(String, String), Schedule>,
    targets: BTreeMap<(String, String), LedgerEntry>,
    versions: BTreeMap<(String, String), u64>,
    drafts: BTreeMap<(String, String), ScheduleBinding>,
    completed: BTreeMap<(String, String), (ScheduleBinding, ScheduleReceipt)>,
    attempts: BTreeMap<(String, String, ReminderKind, String), delivery::Attempt>,
    last_scan: BTreeMap<String, DateTime<FixedOffset>>,
}
impl MemoryReminderMock {
    // Only the versioned journal decoder may enable historical behavior.
    #[cfg(feature = "postgres-mock-store")]
    pub(crate) fn set_legacy_replay_mock(&mut self, legacy: bool) {
        self.legacy_replay = legacy;
    }
    #[cfg(feature = "postgres-mock-store")]
    pub(crate) fn with_current_rules_mock(&self) -> Self {
        let mut next = self.clone();
        next.legacy_replay = false;
        next
    }
    fn validate_schedule(&self, schedule: &Schedule) -> Result<(), &'static str> {
        if self.legacy_replay {
            schedule.validate_legacy()
        } else {
            schedule.validate()
        }
    }

    #[cfg(feature = "postgres-mock-store")]
    pub(crate) fn invalidate_operation_mock(&mut self, user: &str, operation: &str, key: &str) {
        self.drafts.retain(|_, binding| {
            binding.context.user_id != user
                || (binding.context.operation_id != operation
                    && binding.context.idempotency_key != key)
        });
    }
    #[cfg(feature = "postgres-mock-store")]
    pub(crate) fn invalidate_pending_for_restart_mock(&mut self) {
        self.drafts.clear();
    }
    /// Read the original mock settings receipt without confirming or changing a profile.
    pub fn settings_result_mock(
        &self,
        ledger: &MemoryConfirmationMock,
        user: &str,
        operation_id: &str,
    ) -> Result<&ScheduleReceipt, &'static str> {
        ledger.list_mock(user).map_err(|issue| {
            if issue == crate::confirmation_mock::MockIssue::ResourceUnavailable {
                "RESOURCE_UNAVAILABLE"
            } else {
                "LEDGER_NOT_READY"
            }
        })?;
        let (_, receipt) = self
            .completed
            .values()
            .find(|(binding, _)| {
                binding.context.user_id == user && binding.context.operation_id == operation_id
            })
            .ok_or("RESOURCE_UNAVAILABLE")?;
        owned_entry(ledger, user, &receipt.subscription_id)?;
        Ok(receipt)
    }
    #[allow(clippy::too_many_arguments)] // Explicit mock confirmation binds each immutable input.
    pub fn prepare_mock(
        &mut self,
        ledger: &MemoryConfirmationMock,
        context: MockOperationContext,
        id: &str,
        schedule: Schedule,
        now: &str,
        expires_at: &str,
    ) -> Result<ScheduleBinding, &'static str> {
        let target = owned_entry(ledger, &context.user_id, id)?;
        if target.status == LedgerStatus::Error {
            return Err("REGISTRATION_NEEDS_REVIEW");
        }
        // A manually confirmed date cannot supply missing source/service proof.
        // Check only this target; unrelated incomplete rows remain reviewable.
        if [&target.source_id, &target.merchant_id, &target.service_ref]
            .iter()
            .any(|value| value.as_deref().is_none_or(|text| text.trim().is_empty()))
        {
            return Err("SERVICE_NEEDS_REVIEW");
        }
        if context.plan_version == 0
            || [
                &context.session_ref,
                &context.task_id,
                &context.node_id,
                &context.operation_id,
                &context.idempotency_key,
            ]
            .iter()
            .any(|v| v.trim().is_empty())
        {
            return Err("CONTEXT_INVALID");
        }
        self.validate_schedule(&schedule)?;
        let profile_key = (context.user_id.clone(), id.to_owned());
        if let Some(old) = self.profiles.get(&profile_key) {
            if old.cycle != schedule.cycle
                || old.anchor_date != schedule.anchor_date
                || old.period_index != schedule.period_index
            {
                return Err("CYCLE_RECHECK_REQUIRED");
            }
            if !same_target(&self.targets[&profile_key], target) {
                return Err("SERVICE_NEEDS_REVIEW");
            }
        }
        let now_time = DateTime::parse_from_rfc3339(now).map_err(|_| "TIME_INVALID")?;
        if DateTime::parse_from_rfc3339(expires_at).map_err(|_| "TIME_INVALID")? <= now_time {
            return Err("DRAFT_EXPIRED");
        }
        let key = (context.user_id.clone(), context.idempotency_key.clone());
        if self.completed.contains_key(&key)
            || self.completed.values().any(|(b, _)| {
                b.context.user_id == context.user_id
                    && b.context.operation_id == context.operation_id
            })
        {
            return Err("OPERATION_CONFLICT");
        }
        if self.drafts.iter().any(|(k, b)| {
            k != &key
                && b.context.user_id == context.user_id
                && b.context.operation_id == context.operation_id
        }) {
            return Err("OPERATION_CONFLICT");
        }
        let profile = (context.user_id.clone(), id.to_owned());
        let binding = ScheduleBinding {
            minimum_level: "YELLOW",
            context,
            target: target.clone(),
            ledger_revision: ledger
                .reminder_revision_mock(&profile.0)
                .map_err(|_| "LEDGER_NOT_READY")?,
            settings_revision: *self.versions.get(&profile).unwrap_or(&0),
            schedule,
            prepared_at: now.into(),
            expires_at: expires_at.into(),
        };
        if let Some(old) = self.drafts.get(&key) {
            if old == &binding {
                return Ok(old.clone());
            }
            if binding.context.plan_version <= old.context.plan_version
                || binding.context.operation_id != old.context.operation_id
                || binding.context.session_ref != old.context.session_ref
                || binding.context.task_id != old.context.task_id
                || binding.context.node_id != old.context.node_id
            {
                return Err("PLAN_CHANGED");
            }
        }
        self.drafts.insert(key, binding.clone());
        Ok(binding)
    }
    pub fn confirm_mock(
        &mut self,
        ledger: &MemoryConfirmationMock,
        binding: &ScheduleBinding,
        now: &str,
        gate: &mut impl MockScheduleGate,
    ) -> Result<ScheduleReceipt, &'static str> {
        let user = &binding.context.user_id;
        let target = owned_entry(ledger, user, &binding.target.subscription_id)?;
        let key = (user.clone(), binding.context.idempotency_key.clone());
        if let Some((first, receipt)) = self.completed.get(&key) {
            return if first == binding {
                Ok(receipt.clone())
            } else {
                Err("OPERATION_CONFLICT")
            };
        }
        self.validate_schedule(&binding.schedule)?;
        if self.drafts.get(&key) != Some(binding) {
            return Err("PLAN_CHANGED");
        }
        let profile = (user.clone(), target.subscription_id.clone());
        if target != &binding.target
            || ledger
                .reminder_revision_mock(user)
                .map_err(|_| "LEDGER_NOT_READY")?
                != binding.ledger_revision
            || *self.versions.get(&profile).unwrap_or(&0) != binding.settings_revision
        {
            return Err("STALE_FACTS");
        }
        let time = DateTime::parse_from_rfc3339(now).map_err(|_| "TIME_INVALID")?;
        if self.last_scan.get(user).is_some_and(|old| &time < old) {
            return Err("TIME_INVALID");
        }
        let start =
            DateTime::parse_from_rfc3339(&binding.prepared_at).map_err(|_| "TIME_INVALID")?;
        let end = DateTime::parse_from_rfc3339(&binding.expires_at).map_err(|_| "TIME_INVALID")?;
        if time < start {
            return Err("TIME_INVALID");
        }
        if time >= end {
            return Err("DRAFT_EXPIRED");
        }
        let revision = binding
            .settings_revision
            .checked_add(1)
            .ok_or("SETTINGS_UNAVAILABLE")?;
        match gate.verify_and_consume_mock(binding) {
            MockGateOutcome::Confirmed => {}
            MockGateOutcome::Denied => return Err("CONFIRMATION_REQUIRED"),
            MockGateOutcome::StrongAuthRequired => return Err("WAITING_AUTH"),
            MockGateOutcome::Unavailable => return Err("GATE_UNAVAILABLE"),
        }
        let receipt = ScheduleReceipt {
            fictional: true,
            production_registered: false,
            notification_sent: false,
            operation_id: binding.context.operation_id.clone(),
            subscription_id: target.subscription_id.clone(),
            expected_date: binding.schedule.expected_date,
            days_before: binding.schedule.days_before,
            message: "已确认本次模拟提醒日期与设置；仅本地模拟，未向用户发送通知。",
        };
        self.profiles
            .insert(profile.clone(), binding.schedule.clone());
        self.targets.insert(profile.clone(), target.clone());
        self.versions.insert(profile, revision);
        self.completed
            .insert(key, (binding.clone(), receipt.clone()));
        self.last_scan.insert(user.clone(), time);
        Ok(receipt)
    }
}

fn same_target(old: &LedgerEntry, now: &LedgerEntry) -> bool {
    old.subscription_id == now.subscription_id
        && old.owner_user_id == now.owner_user_id
        && old.source_id == now.source_id
        && old.merchant_id == now.merchant_id
        && old.service_ref == now.service_ref
}
