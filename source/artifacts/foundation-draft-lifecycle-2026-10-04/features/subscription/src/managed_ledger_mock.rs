//! Independently supplied fictional ledger; no persistence, authentication or Tool registration.
use crate::{
    recognition::{recognize_mock, RecognitionStatus, SuspectedGroup},
    MockContext, ReadKind, ReadState, Snapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerFixture {
    pub schema_version: u32,
    pub fictional: bool,
    pub production_registered: bool,
    pub owner_user_id: String,
    pub snapshot_id: Option<String>,
    pub read: ReadState,
    pub entries: Vec<LedgerEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LedgerEntry {
    pub subscription_id: String,
    pub owner_user_id: String,
    pub source_id: Option<String>,
    pub merchant_id: Option<String>,
    pub service_ref: Option<String>,
    pub status: LedgerStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LedgerStatus {
    Active,
    Cancelling,
    Error,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Disposition {
    AlreadyManaged,
    NotInLedger,
    NeedsReview,
    ServiceBlocked,
}

#[derive(Debug, Serialize)]
pub struct GroupReview {
    pub group: SuspectedGroup,
    pub disposition: Disposition,
    pub reason: &'static str,
    pub matched_subscription_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LedgerReview {
    pub mode: &'static str,
    pub production_registered: bool,
    pub team_contract_approved: bool,
    pub business_writes: bool,
    pub matching_executed: bool,
    pub reason: &'static str,
    pub groups: Vec<GroupReview>,
}

pub fn load_ledger(json: &str) -> Result<LedgerFixture, &'static str> {
    let ledger: LedgerFixture = serde_json::from_str(json).map_err(|_| "LEDGER_PARSE_FAILED")?;
    if ledger.schema_version != 1 || !ledger.fictional || ledger.production_registered {
        return Err("NOT_A_FICTIONAL_LEDGER");
    }
    Ok(ledger)
}

fn valid_optional(value: &Option<String>) -> bool {
    value.as_ref().is_none_or(|s| !s.trim().is_empty())
}
fn key(e: &LedgerEntry) -> Option<(&str, &str, &str)> {
    Some((
        e.source_id.as_deref()?,
        e.merchant_id.as_deref()?,
        e.service_ref.as_deref()?,
    ))
}

/// Only independent COMPLETE ledger input unlocks the local matching pass.
/// Caller identity and completeness remain explicit simulation assumptions.
pub fn review_mock(
    context: &MockContext,
    consumption: &Snapshot,
    ledger: &LedgerFixture,
) -> LedgerReview {
    let mut result = LedgerReview {
        mode: "OFFLINE_MOCK_LEDGER_REVIEW",
        production_registered: false,
        team_contract_approved: false,
        business_writes: false,
        matching_executed: false,
        reason: "LEDGER_INVALID",
        groups: vec![],
    };
    if ledger.schema_version != 1 || !ledger.fictional || ledger.production_registered {
        return result;
    }
    // Never disclose foreign subscription IDs, even when the source also failed.
    if context.user_id.trim().is_empty()
        || ledger.owner_user_id != context.user_id
        || ledger
            .entries
            .iter()
            .any(|e| e.owner_user_id != context.user_id)
    {
        result.reason = "RESOURCE_UNAVAILABLE";
        return result;
    }
    if !matches!(ledger.read.state, ReadKind::Complete) {
        result.reason = "LEDGER_UNAVAILABLE_OR_INCOMPLETE";
        return result;
    }
    if !ledger.read.is_complete
        || ledger.read.next_cursor.is_some()
        || ledger.read.reason.is_some()
        || ledger
            .snapshot_id
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
    {
        return result;
    }
    let mut ids = HashSet::new();
    if ledger.entries.iter().any(|e| {
        e.subscription_id.trim().is_empty()
            || !ids.insert(&e.subscription_id)
            || !valid_optional(&e.source_id)
            || !valid_optional(&e.merchant_id)
            || !valid_optional(&e.service_ref)
    }) {
        return result;
    }
    if ledger
        .entries
        .iter()
        .any(|e| e.status == LedgerStatus::Error && key(e).is_none())
    {
        result.reason = "LEDGER_ERROR_SCOPE_UNKNOWN";
        return result;
    }
    let mut snapshot = consumption.clone();
    // This replaces only the historical simulated ledger gate after validating
    // the separately supplied fixture. No production input can use this as proof.
    snapshot.managed_subscriptions_read = ledger.read.clone();
    let recognition = recognize_mock(context, &snapshot);
    if recognition.status != RecognitionStatus::Reviewed {
        result.reason = "CONSUMPTION_NOT_READY";
        return result;
    }
    for group in recognition.groups {
        let mut keys = HashSet::new();
        let mut uncertain = group.mock_service_ref.is_none();
        for charge in &group.actual_charges {
            if let (Some(origin), Some(service)) = (&charge.source_key, &group.mock_service_ref) {
                keys.insert((
                    origin.source_id.as_str(),
                    group.merchant_id.as_str(),
                    service.as_str(),
                ));
            } else {
                uncertain = true;
            }
        }
        // Unknown references that could overlap a precisely scoped ERROR are
        // blocked locally too; another provably distinct service may proceed.
        let blocked = ledger
            .entries
            .iter()
            .filter(|e| e.status == LedgerStatus::Error)
            .any(|e| {
                let (source, merchant, service) = key(e).expect("validated error scope");
                merchant == group.merchant_id
                    && group
                        .mock_service_ref
                        .as_deref()
                        .is_none_or(|s| s == service)
                    && group
                        .actual_charges
                        .iter()
                        .any(|c| c.source_key.as_ref().is_none_or(|k| k.source_id == source))
            });
        let mut matched = Vec::new();
        let mut matched_keys = HashSet::new();
        if ledger.entries.iter().any(|e| {
            e.status == LedgerStatus::Error
                && e.merchant_id.as_deref() == Some(group.merchant_id.as_str())
                && e.service_ref == group.mock_service_ref
                && !blocked
        }) {
            uncertain = true;
        }
        for entry in ledger
            .entries
            .iter()
            .filter(|e| e.status != LedgerStatus::Error)
        {
            if let Some(k) = key(entry) {
                if keys.contains(&k) {
                    matched.push(entry.subscription_id.clone());
                    matched_keys.insert(k);
                } else if entry.merchant_id.as_deref() == Some(group.merchant_id.as_str())
                    && entry.service_ref == group.mock_service_ref
                {
                    uncertain = true; // Same refs across sources are not identity proof.
                }
            } else if entry
                .merchant_id
                .as_deref()
                .is_none_or(|m| m == group.merchant_id)
            {
                uncertain = true;
            }
        }
        matched.sort();
        let (disposition, reason) = if blocked {
            (Disposition::ServiceBlocked, "RELATED_LEDGER_ERROR")
        } else if !uncertain && !keys.is_empty() && matched_keys.len() == keys.len() {
            (
                Disposition::AlreadyManaged,
                "EXACT_SOURCE_MERCHANT_SERVICE_MATCH",
            )
        } else if uncertain || !matched.is_empty() {
            (
                Disposition::NeedsReview,
                "IDENTITY_UNCERTAIN_OR_PARTIAL_MATCH",
            )
        } else {
            (Disposition::NotInLedger, "NO_EXACT_LEDGER_MATCH")
        };
        // Keep evidence for review, even when all charges were already managed.
        result.groups.push(GroupReview {
            group,
            disposition,
            reason,
            matched_subscription_ids: matched,
        });
    }
    result.reason = "MOCK_LEDGER_REVIEWED";
    result.matching_executed = true;
    result
}
