//! Offline mock boundary only. Caller-supplied fixture identity is NOT authentication.
//! No production Tools, database, network or business side effects. Confirmation
//! writes only to an explicitly fictional in-memory harness. The recognition
//! module uses Hannah-approved rules with simulated service references only.
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub mod billing_adapter;
pub mod billing_query_mock;
pub mod confirmation_mock;
pub mod ledger_source_mock;
pub mod managed_ledger_mock;
pub mod peer_review;
#[cfg(feature = "postgres-mock-store")]
pub mod postgres_ledger_mock;
pub mod recognition;
pub mod reminder_mock;

#[derive(Clone, Debug, Deserialize)]
pub struct FixtureSet {
    pub schema_version: u32,
    pub fictional: bool,
    pub production_registration: bool,
    pub rule_version: Option<String>,
    pub sample_count: usize,
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Sample {
    pub sample_id: String,
    pub trusted_context_fixture: MockContext,
    pub snapshot: Snapshot,
    // Deliberately omit `expected`: it must never drive the filter.
}

/// Only a simulated service context; production must inject authenticated facts.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MockContext {
    pub user_id: String,
    pub requested_account_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub window: Window,
    pub accounts: Vec<Account>,
    pub transactions_read: ReadState,
    pub managed_subscriptions_read: ReadState,
    pub transactions: Vec<Transaction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Window {
    pub as_of: String,
    pub start_inclusive: String,
    pub end_exclusive: String,
    pub timezone: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Account {
    pub account_id: String,
    pub user_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadState {
    pub state: ReadKind,
    pub is_complete: bool,
    pub next_cursor: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReadKind {
    Complete,
    Incomplete,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Transaction {
    pub source_id: Option<String>,
    pub transaction_id: String,
    pub account_id: String,
    pub card_id: Option<String>,
    pub amount: String,
    pub currency: String,
    pub direction: Direction,
    pub transaction_type: TransactionType,
    pub status: TransactionStatus,
    pub completed_at: Option<String>,
    pub merchant_id: Option<String>,
    pub mock_service_ref: Option<String>,
    pub category_code: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionType {
    Consumption,
    Transfer,
    Refund,
    Income,
    Withdrawal,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionStatus {
    Completed,
    Processing,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Outcome {
    ResourceUnavailable,
    InvalidInput,
    LookupUnavailable,
    DataIncomplete,
    LedgerUnavailable,
    NoEligibleEvidence,
    NeedsMerchantEvidence,
    EvidenceReadyRulePending,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Evidence {
    #[serde(skip)]
    pub account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<billing_adapter::SourceKey>,
    pub transaction_id: String,
    pub merchant_id: String,
    pub mock_service_ref: Option<String>,
    pub amount_minor_units: u64,
    pub completed_at: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Exclusion {
    NotCompleted,
    NotConsumption,
    NotOutgoing,
    OutsideWindow,
    MissingMerchant,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ExcludedTransaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<billing_adapter::SourceKey>,
    pub transaction_id: String,
    pub reason: Exclusion,
}

/// Internal mock result, not C-SUB or a bank receipt. No candidate/subscription IDs.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub message: &'static str,
    pub eligible_evidence: Vec<Evidence>,
    pub excluded: Vec<ExcludedTransaction>,
    pub next_cursor: Option<String>,
}

fn report(outcome: Outcome) -> Report {
    let message = match outcome {
        Outcome::ResourceUnavailable => "无法访问该范围的资料。",
        Outcome::InvalidInput => "模拟输入无效，未完成证据筛选。",
        Outcome::LookupUnavailable => "流水查询失败，未完成证据筛选。",
        Outcome::DataIncomplete => "流水未取全，不能给出完整识别结论。",
        Outcome::LedgerUnavailable => "已在管台账未完整读取，不能完成后续精确过滤。",
        Outcome::NoEligibleEvidence => "本次模拟范围没有合格消费证据，不表示没有订阅。",
        Outcome::NeedsMerchantEvidence => {
            "部分消费缺可信商户标识，不能仅凭名称、金额和日期认定订阅。"
        }
        Outcome::EvidenceReadyRulePending => {
            "消费证据已筛选；周期、次数及容差待确认，尚未识别订阅。"
        }
    };
    Report {
        outcome,
        message,
        eligible_evidence: vec![],
        excluded: vec![],
        next_cursor: None,
    }
}

/// Require positive canonical yuan with exactly two decimal digits; never use floats.
pub fn amount_minor_units(amount: &str) -> Option<u64> {
    let (whole, fraction) = amount.split_once('.')?;
    if whole.is_empty()
        || fraction.len() != 2
        || (whole.len() > 1 && whole.starts_with('0'))
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let cents = whole
        .parse::<u64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(fraction.parse::<u64>().ok()?)?;
    (cents > 0).then_some(cents)
}

fn date(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok()
}

fn optional_id_valid(value: &Option<String>) -> bool {
    value.as_ref().is_none_or(|s| !s.trim().is_empty())
}

fn read_consistent(read: &ReadState) -> bool {
    optional_id_valid(&read.next_cursor)
        && optional_id_valid(&read.reason)
        && match read.state {
            ReadKind::Complete => {
                read.is_complete && read.next_cursor.is_none() && read.reason.is_none()
            }
            ReadKind::Incomplete | ReadKind::Unavailable => !read.is_complete,
        }
}

/// Parse only a fictional, unregistered v1 fixture. Unknown enum values fail closed.
pub fn load_fixture(json: &str) -> Result<FixtureSet, &'static str> {
    let set: FixtureSet = serde_json::from_str(json).map_err(|_| "FIXTURE_PARSE_FAILED")?;
    let mut ids = HashSet::new();
    if set.schema_version != 1
        || !set.fictional
        || set.production_registration
        || set.rule_version.is_some()
        || set.samples.is_empty()
        || set.sample_count != set.samples.len()
        || set
            .samples
            .iter()
            .any(|s| s.sample_id.trim().is_empty() || !ids.insert(&s.sample_id))
    {
        return Err("FIXTURE_METADATA_INVALID");
    }
    Ok(set)
}

/// Scope is checked before returning any transaction IDs, counts, or failure cursors.
pub fn filter_evidence(context: &MockContext, snapshot: &Snapshot) -> Report {
    let invalid = || report(Outcome::InvalidInput);
    let unavailable = || report(Outcome::ResourceUnavailable);
    if context.user_id.trim().is_empty() || context.requested_account_ids.is_empty() {
        return invalid();
    }
    let mut accounts = HashMap::new();
    for account in &snapshot.accounts {
        if account.account_id.trim().is_empty()
            || account.user_id.trim().is_empty()
            || accounts
                .insert(&account.account_id, &account.user_id)
                .is_some()
        {
            return invalid();
        }
    }
    let mut scope = HashSet::new();
    for id in &context.requested_account_ids {
        if !scope.insert(id) {
            return invalid();
        }
        if accounts.get(id).copied() != Some(&context.user_id) {
            return unavailable();
        }
    }
    // Do not silently discard unauthorized rows and then call the remaining set complete.
    if snapshot.transactions.iter().any(|tx| {
        !scope.contains(&tx.account_id)
            || accounts.get(&tx.account_id).copied() != Some(&context.user_id)
    }) {
        return unavailable();
    }
    let (Some(start), Some(end), Some(as_of)) = (
        date(&snapshot.window.start_inclusive),
        date(&snapshot.window.end_exclusive),
        date(&snapshot.window.as_of),
    ) else {
        return invalid();
    };
    // This mock replays the supplied interval, never computes a production four-month window.
    if start >= end
        || end > as_of
        || snapshot.window.timezone != "Asia/Shanghai"
        || !read_consistent(&snapshot.transactions_read)
        || !read_consistent(&snapshot.managed_subscriptions_read)
    {
        return invalid();
    }
    match snapshot.transactions_read.state {
        ReadKind::Unavailable => return report(Outcome::LookupUnavailable),
        ReadKind::Incomplete => {
            let mut result = report(Outcome::DataIncomplete);
            result.next_cursor = snapshot.transactions_read.next_cursor.clone();
            return result;
        }
        ReadKind::Complete => {}
    }
    if snapshot.managed_subscriptions_read.state != ReadKind::Complete {
        return report(Outcome::LedgerUnavailable);
    }
    let mut ids = HashSet::new();
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();
    for tx in &snapshot.transactions {
        let Some(cents) = amount_minor_units(&tx.amount) else {
            return invalid();
        };
        if tx.transaction_id.trim().is_empty()
            || !ids.insert((&tx.source_id, &tx.account_id, &tx.transaction_id))
            || tx.currency != "CNY"
            || !optional_id_valid(&tx.category_code)
            || !optional_id_valid(&tx.source_id)
            || !optional_id_valid(&tx.card_id)
            || !optional_id_valid(&tx.merchant_id)
            || !optional_id_valid(&tx.mock_service_ref)
        {
            return invalid();
        }
        let completed = match &tx.completed_at {
            Some(value) => match date(value) {
                Some(parsed) => Some(parsed),
                None => return invalid(),
            },
            None => None,
        };
        if tx.status == TransactionStatus::Completed && completed.is_none() {
            return invalid();
        }
        if completed.is_some_and(|time| time > as_of) {
            return invalid();
        }
        let reason = if tx.status != TransactionStatus::Completed {
            Some(Exclusion::NotCompleted)
        } else if tx.transaction_type != TransactionType::Consumption {
            // Includes fund-investment mock (OTHER); do not invent a shared category table.
            Some(Exclusion::NotConsumption)
        } else if tx.direction != Direction::Out {
            Some(Exclusion::NotOutgoing)
        } else if completed.is_some_and(|time| time < start || time >= end) {
            Some(Exclusion::OutsideWindow)
        } else if tx.merchant_id.is_none() {
            Some(Exclusion::MissingMerchant)
        } else {
            None
        };
        let source_key = tx
            .source_id
            .as_ref()
            .map(|source| billing_adapter::SourceKey {
                source_id: source.clone(),
                account_id: tx.account_id.clone(),
                transaction_id: tx.transaction_id.clone(),
            });
        if let Some(reason) = reason {
            excluded.push(ExcludedTransaction {
                source_key,
                transaction_id: tx.transaction_id.clone(),
                reason,
            });
        } else {
            eligible.push(Evidence {
                source_key,
                account_id: tx.account_id.clone(),
                transaction_id: tx.transaction_id.clone(),
                merchant_id: tx.merchant_id.clone().expect("checked above"),
                mock_service_ref: tx.mock_service_ref.clone(),
                amount_minor_units: cents,
                completed_at: tx.completed_at.clone().expect("checked above"),
            });
        }
    }
    let outcome = if excluded
        .iter()
        .any(|tx| tx.reason == Exclusion::MissingMerchant)
    {
        Outcome::NeedsMerchantEvidence
    } else if eligible.is_empty() {
        Outcome::NoEligibleEvidence
    } else {
        Outcome::EvidenceReadyRulePending
    };
    Report {
        eligible_evidence: eligible,
        excluded,
        ..report(outcome)
    }
}
