//! Backend-only daily transfer facts, NOT a limit decision or execution permit.
//!
//! The caller must supply a consistent, complete owner-wide snapshot containing
//! today's formally submitted outgoing transfers AND every older unresolved one.
//! `complete` is a trusted adapter assertion, not proof of authorization. Never
//! expose these inputs to a model/client or derive submitted_at from completed_at.

use crate::mock_execution::amount_cents;
use chrono::{DateTime, Duration, NaiveDate};
use safeflow_bank_gateway::DecimalAmount;
use safeflow_contracts::{OperationId, ResourceId, ToolResultStatus, UserId};
use serde::Serialize;
use std::collections::HashMap;

pub const DAILY_USAGE_RULE_VERSION: &str = "transfer-daily-usage-2026-10-17-v1";

/// Latest authoritative state of a FORMALLY SUBMITTED outgoing business order.
/// Not a draft, receipt history, incoming ledger row or wire DTO. Intentionally
/// has no Deserialize. Map the foundation's existing immutable first submission
/// time (public_execution_requests.submitted_at / first_submission.recorded_at)
/// from the matching FUNDS business operation, not a draft action or reservation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SubmittedTransferFact {
    pub owner_id: UserId,
    pub operation_id: OperationId,
    pub payer_account_id: ResourceId,
    pub amount: DecimalAmount,
    pub submitted_at: Option<String>,
    pub status: ToolResultStatus,
}

pub struct DailyUsageInput<'a> {
    pub owner_id: &'a UserId,
    /// Trusted snapshot time, not a client-controlled local calendar day.
    pub as_of: &'a str,
    pub complete: bool,
    pub submitted: &'a [SubmittedTransferFact],
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct DailyUsage {
    pub rule_version: &'static str,
    pub statistical_day: String,
    pub successful_cents: u64,
    pub pending_today_cents: u64,
    pub carried_pending_cents: u64,
    pub used_cents: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyUsageError {
    IncompleteSnapshot,
    InvalidTime,
    InvalidFact,
    ConflictingOperation,
    AmountOverflow,
}
impl std::fmt::Display for DailyUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daily usage unavailable: {self:?}")
    }
}
impl std::error::Error for DailyUsageError {}

fn day(value: &str) -> Result<NaiveDate, DailyUsageError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| DailyUsageError::InvalidTime)?
        .naive_utc()
        .checked_add_signed(Duration::hours(8))
        .map(|t| t.date())
        .ok_or(DailyUsageError::InvalidTime)
}

pub fn calculate_daily_usage(input: DailyUsageInput<'_>) -> Result<DailyUsage, DailyUsageError> {
    if !input.complete {
        return Err(DailyUsageError::IncompleteSnapshot);
    }
    let today = day(input.as_of)?;
    let as_of =
        DateTime::parse_from_rfc3339(input.as_of).map_err(|_| DailyUsageError::InvalidTime)?;
    if !valid_id(input.owner_id.as_str()) {
        return Err(DailyUsageError::InvalidFact);
    }
    let mut seen = HashMap::new();
    let mut successful_cents: u64 = 0;
    let mut pending_today_cents: u64 = 0;
    let mut carried_pending_cents: u64 = 0;
    for fact in input.submitted {
        let amount = amount_cents(&fact.amount).map_err(|_| DailyUsageError::AmountOverflow)?;
        if fact.owner_id != *input.owner_id
            || amount == 0
            || !valid_id(fact.operation_id.as_str())
            || !valid_id(fact.payer_account_id.as_str())
        {
            return Err(DailyUsageError::InvalidFact);
        }
        let submitted_at = fact
            .submitted_at
            .as_deref()
            .ok_or(DailyUsageError::InvalidTime)?;
        let submitted_time =
            DateTime::parse_from_rfc3339(submitted_at).map_err(|_| DailyUsageError::InvalidTime)?;
        if submitted_time > as_of {
            return Err(DailyUsageError::InvalidTime);
        }
        if let Some(previous) = seen.insert(&fact.operation_id, fact) {
            if previous != fact {
                return Err(DailyUsageError::ConflictingOperation);
            }
            continue;
        }
        let submitted_day = day(submitted_at)?;
        let bucket = match fact.status {
            ToolResultStatus::Succeeded if submitted_day == today => &mut successful_cents,
            ToolResultStatus::Processing if submitted_day == today => &mut pending_today_cents,
            ToolResultStatus::Processing => &mut carried_pending_cents,
            _ => continue,
        };
        *bucket = bucket
            .checked_add(amount)
            .ok_or(DailyUsageError::AmountOverflow)?;
    }
    let used_cents = successful_cents
        .checked_add(pending_today_cents)
        .and_then(|sum| sum.checked_add(carried_pending_cents))
        .ok_or(DailyUsageError::AmountOverflow)?;
    Ok(DailyUsage {
        rule_version: DAILY_USAGE_RULE_VERSION,
        statistical_day: today.to_string(),
        successful_cents,
        pending_today_cents,
        carried_pending_cents,
        used_cents,
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}
