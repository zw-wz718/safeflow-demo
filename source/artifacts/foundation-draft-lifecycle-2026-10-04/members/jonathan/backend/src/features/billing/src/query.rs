//! Local BILL-02 query proposal. No production registration or authentication.
use crate::{CnyAmount, TrustedScope};
use chrono::{DateTime, FixedOffset, Months};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub as_of: String,
    pub start_inclusive: String,
    pub end_exclusive: String,
    pub timezone: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub account_ids: Vec<String>,
    pub window: Window,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionType {
    Consumption,
    Transfer,
    Refund,
    Income,
    Withdrawal,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Completed,
    Processing,
    Failed,
    Cancelled,
}

/// Draft-normalized source fields. Missing optional fields stay absent/null;
/// this is not an adapter from the transfer fixture or a frozen C-TXN type.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub transaction_id: String,
    pub account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub card_id: Option<String>,
    pub amount: CnyAmount,
    pub currency: String,
    pub direction: Direction,
    pub transaction_type: TransactionType,
    pub status: Status,
    pub completed_at: Option<String>,
    pub merchant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_tx_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub snapshot_id: String,
    pub owner_user_id: String,
    pub account_ids: Vec<String>,
    pub window: Window,
    pub items: Vec<Transaction>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    /// Source assertion plus terminal count is mandatory, not empty-page inference.
    pub range_complete: bool,
    pub total_unique_rows: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PageError {
    Unavailable,
    SnapshotExpired,
    ResourceUnavailable,
}

pub trait PagedSource {
    fn source_id(&self) -> &str;
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &Request,
        cursor: Option<&str>,
        snapshot_id: Option<&str>,
    ) -> Result<Page, PageError>;
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_pages: usize,
    pub max_rows: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pages: 100,
            max_rows: 10_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReadState {
    Complete,
    DataIncomplete,
    LookupUnavailable,
    ResourceUnavailable,
    InvalidInput,
}

#[derive(Clone, Debug, Serialize)]
pub struct Excluded {
    pub transaction_id: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub contract: &'static str,
    pub production_registered: bool,
    pub source_id: String,
    pub state: ReadState,
    pub is_complete: bool,
    pub reason: Option<String>,
    pub snapshot_id: Option<String>,
    pub next_cursor: Option<String>,
    pub pages_read: usize,
    pub duplicates_ignored: usize,
    pub observed_transactions: Vec<Transaction>,
    pub eligible_consumption: Vec<Transaction>,
    pub excluded: Vec<Excluded>,
    pub missing_merchant_ids: Vec<String>,
    pub missing_service_ids: Vec<String>,
    #[serde(skip)]
    binding: (String, Request),
    #[serde(skip)]
    // Frozen eligible rows from collect, not mutable display data or a credential.
    // Bounds are inherited from Limits; never deserialize this evidence from JSON.
    verified_consumption: Option<Vec<Transaction>>,
}

fn time(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok()
}

/// Matches Hannah's LOCAL rule only. Four calendar months, [start,end), UTC+8.
/// Does not freeze D-22, fetch a trusted clock or prove source completeness.
pub fn local_four_month_window(as_of: &str) -> Option<Window> {
    let end = time(as_of)?.with_timezone(&FixedOffset::east_opt(8 * 3600)?);
    let start = end.checked_sub_months(Months::new(4))?;
    Some(Window {
        as_of: end.to_rfc3339(),
        start_inclusive: start.to_rfc3339(),
        end_exclusive: end.to_rfc3339(),
        timezone: "Asia/Shanghai".into(),
    })
}

fn valid_window(window: &Window) -> bool {
    let (Some(start), Some(end), Some(as_of)) = (
        time(&window.start_inclusive),
        time(&window.end_exclusive),
        time(&window.as_of),
    ) else {
        return false;
    };
    window.timezone == "Asia/Shanghai" && start < end && end <= as_of
}

fn same_window(a: &Window, b: &Window) -> bool {
    a.timezone == b.timezone
        && valid_window(b)
        && time(&a.as_of) == time(&b.as_of)
        && time(&a.start_inclusive) == time(&b.start_inclusive)
        && time(&a.end_exclusive) == time(&b.end_exclusive)
}

fn account_set(ids: &[String]) -> Option<BTreeSet<&str>> {
    let set: BTreeSet<_> = ids.iter().map(String::as_str).collect();
    if set.is_empty()
        || set.len() != ids.len()
        || set.iter().any(|id| crate::validate_reference(id).is_err())
    {
        None
    } else {
        Some(set)
    }
}

fn valid_row(row: &Transaction, window: &Window) -> bool {
    if crate::validate_reference(&row.transaction_id).is_err() || row.currency != "CNY" {
        return false;
    }
    let whole = row.amount.as_str().split('.').next().unwrap_or("");
    if whole.len() > 1 && whole.starts_with('0') {
        return false;
    }
    for reference in [
        &row.operation_id,
        &row.card_id,
        &row.merchant_id,
        &row.category_code,
        &row.original_tx_id,
        &row.service_ref,
    ]
    .into_iter()
    .flatten()
    {
        if crate::validate_reference(reference).is_err() {
            return false;
        }
    }
    match row.completed_at.as_deref() {
        Some(value) => time(value)
            .zip(time(&window.as_of))
            .is_some_and(|(value, as_of)| value <= as_of),
        None => row.status != Status::Completed,
    }
}

impl Report {
    fn fail(mut self, state: ReadState, reason: &str) -> Self {
        self.state = state;
        self.is_complete = false;
        self.reason = Some(reason.into());
        self.eligible_consumption.clear();
        self.excluded.clear();
        self.missing_merchant_ids.clear();
        self.missing_service_ids.clear();
        self.verified_consumption = None;
        if matches!(
            state,
            ReadState::InvalidInput | ReadState::ResourceUnavailable
        ) {
            self.observed_transactions.clear();
            self.snapshot_id = None;
            self.next_cursor = None;
            self.duplicates_ignored = 0;
            if state == ReadState::ResourceUnavailable {
                self.pages_read = 0;
            }
        }
        self
    }
}

/// Source adapters independently enforce ownership. Their terminal assertion,
/// count and stable query binding are needed; an empty page alone is not proof.
pub fn collect(
    scope: &TrustedScope,
    request: &Request,
    source: &impl PagedSource,
    limits: Limits,
) -> Report {
    let mut report = Report {
        contract: "LOCAL_BILLING_QUERY_V1",
        production_registered: false,
        source_id: source.source_id().into(),
        state: ReadState::LookupUnavailable,
        is_complete: false,
        reason: None,
        snapshot_id: None,
        next_cursor: None,
        pages_read: 0,
        duplicates_ignored: 0,
        observed_transactions: vec![],
        eligible_consumption: vec![],
        excluded: vec![],
        missing_merchant_ids: vec![],
        missing_service_ids: vec![],
        binding: (scope.user_id().into(), request.clone()),
        verified_consumption: None,
    };
    let Some(accounts) = account_set(&request.account_ids) else {
        return report.fail(ReadState::InvalidInput, "INVALID_ACCOUNT_SCOPE");
    };
    if accounts.iter().any(|id| !scope.contains_account(id)) {
        return report.fail(ReadState::ResourceUnavailable, "ACCOUNT_NOT_ACCESSIBLE");
    }
    if !valid_window(&request.window)
        || limits.max_pages == 0
        || limits.max_rows == 0
        || crate::validate_reference(source.source_id()).is_err()
    {
        return report.fail(ReadState::InvalidInput, "INVALID_REQUEST");
    }
    let mut keys: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut used_cursors = BTreeSet::new();
    loop {
        if report.pages_read >= limits.max_pages {
            return report.fail(ReadState::DataIncomplete, "PAGE_LIMIT");
        }
        let page = match source.read_page(
            scope,
            request,
            report.next_cursor.as_deref(),
            report.snapshot_id.as_deref(),
        ) {
            Ok(page) => page,
            Err(PageError::ResourceUnavailable) => {
                return report.fail(ReadState::ResourceUnavailable, "SOURCE_ACCESS_DENIED")
            }
            Err(error) => {
                let state = if report.pages_read == 0 {
                    ReadState::LookupUnavailable
                } else {
                    ReadState::DataIncomplete
                };
                let reason = if error == PageError::SnapshotExpired {
                    "SNAPSHOT_EXPIRED"
                } else {
                    "SOURCE_UNAVAILABLE"
                };
                return report.fail(state, reason);
            }
        };
        if page.owner_user_id != scope.user_id()
            || account_set(&page.account_ids) != Some(accounts.clone())
            || page.items.iter().any(|row| {
                !accounts.contains(row.account_id.as_str())
                    || !scope.contains_account(&row.account_id)
            })
        {
            return report.fail(ReadState::ResourceUnavailable, "SOURCE_SCOPE_MISMATCH");
        }
        if crate::validate_reference(&page.snapshot_id).is_err()
            || !same_window(&request.window, &page.window)
            || page.has_more != page.next_cursor.is_some()
            || page
                .next_cursor
                .as_ref()
                .is_some_and(|id| crate::validate_reference(id).is_err())
            || (page.has_more && page.range_complete)
        {
            return report.fail(ReadState::InvalidInput, "INVALID_PAGE_METADATA");
        }
        if report
            .snapshot_id
            .as_ref()
            .is_some_and(|id| id != &page.snapshot_id)
        {
            return report.fail(ReadState::DataIncomplete, "SNAPSHOT_CHANGED");
        }
        if page
            .items
            .iter()
            .any(|row| !valid_row(row, &request.window))
        {
            return report.fail(ReadState::InvalidInput, "INVALID_TRANSACTION");
        }
        report.snapshot_id = Some(page.snapshot_id);
        report.pages_read += 1;
        for row in page.items {
            let key = (row.account_id.clone(), row.transaction_id.clone());
            if let Some(index) = keys.get(&key) {
                if report.observed_transactions[*index] != row {
                    return report.fail(ReadState::InvalidInput, "CONFLICTING_TRANSACTION_ID");
                }
                report.duplicates_ignored += 1;
            } else {
                if report.observed_transactions.len() >= limits.max_rows {
                    return report.fail(ReadState::DataIncomplete, "ROW_LIMIT");
                }
                keys.insert(key, report.observed_transactions.len());
                report.observed_transactions.push(row);
            }
        }
        report.next_cursor = page.next_cursor;
        if let Some(cursor) = &report.next_cursor {
            if !used_cursors.insert(cursor.clone()) {
                return report.fail(ReadState::DataIncomplete, "CURSOR_LOOP");
            }
            continue;
        }
        if !page.range_complete || page.total_unique_rows.is_none() {
            return report.fail(ReadState::DataIncomplete, "MISSING_COMPLETION_PROOF");
        }
        if page.total_unique_rows != Some(report.observed_transactions.len()) {
            return report.fail(ReadState::InvalidInput, "COMPLETION_COUNT_MISMATCH");
        }
        report.state = ReadState::Complete;
        report.is_complete = true;
        break;
    }
    let start = time(&request.window.start_inclusive).expect("validated request");
    let end = time(&request.window.end_exclusive).expect("validated request");
    for row in &report.observed_transactions {
        let reason = if row.status != Status::Completed {
            Some("NOT_COMPLETED")
        } else if row.transaction_type != TransactionType::Consumption {
            Some("NOT_CONSUMPTION")
        } else if row.direction != Direction::Out {
            Some("NOT_OUTGOING")
        } else if !time(
            row.completed_at
                .as_deref()
                .expect("validated completed row"),
        )
        .is_some_and(|at| at >= start && at < end)
        {
            Some("OUTSIDE_WINDOW")
        } else {
            None
        };
        if let Some(reason) = reason {
            report.excluded.push(Excluded {
                transaction_id: row.transaction_id.clone(),
                reason,
            });
        } else {
            if row.merchant_id.is_none() {
                report.missing_merchant_ids.push(row.transaction_id.clone());
            }
            if row.service_ref.is_none() {
                report.missing_service_ids.push(row.transaction_id.clone());
            }
            report.eligible_consumption.push(row.clone());
        }
    }
    report.verified_consumption = Some(report.eligible_consumption.clone());
    report
}

/// Experimental shape compatible with Hannah's a76aa85 local Snapshot, not C-SUB.
/// Only the proven query's eligible subset is handed off; the billing report
/// keeps refund/transfer evidence separately. Never pretend an unknown ledger is empty.
pub fn subscription_snapshot(
    report: &Report,
    scope: &TrustedScope,
    request: &Request,
) -> Result<serde_json::Value, &'static str> {
    if report.state != ReadState::Complete
        || !report.is_complete
        || report.verified_consumption.is_none()
    {
        return Err("READ_NOT_COMPLETE");
    }
    if report.binding.0 != scope.user_id()
        || report.binding.1 != *request
        || request
            .account_ids
            .iter()
            .any(|id| !scope.contains_account(id))
    {
        return Err("SCOPE_MISMATCH");
    }
    if report.verified_consumption.as_ref() != Some(&report.eligible_consumption) {
        return Err("QUERY_EVIDENCE_CHANGED");
    }
    let mut transactions = vec![];
    let mut transaction_ids = BTreeSet::new();
    for row in &report.eligible_consumption {
        if !scope.contains_account(&row.account_id)
            || !request.account_ids.contains(&row.account_id)
            || !valid_row(row, &request.window)
            || row.status != Status::Completed
            || row.transaction_type != TransactionType::Consumption
            || row.direction != Direction::Out
        {
            return Err("INVALID_CONSUMPTION_SCOPE");
        }
        if !transaction_ids.insert(&row.transaction_id) {
            return Err("CONSUMER_TRANSACTION_ID_NOT_UNIQUE");
        }
        let Some(category) = &row.category_code else {
            return Err("SOURCE_CATEGORY_NOT_PROVIDED");
        };
        transactions.push(serde_json::json!({"transaction_id":row.transaction_id,"account_id":row.account_id,
            "card_id":row.card_id,"amount":row.amount,"currency":row.currency,"direction":row.direction,
            "transaction_type":row.transaction_type,"status":row.status,"completed_at":row.completed_at,
            "merchant_id":row.merchant_id,"category_code":category,"mock_service_ref":row.service_ref}));
    }
    Ok(serde_json::json!({"window":request.window,
        "accounts":request.account_ids.iter().map(|id|serde_json::json!({"account_id":id,"user_id":scope.user_id()})).collect::<Vec<_>>(),
        "transactions_read":{"state":"COMPLETE","is_complete":true,"next_cursor":null,"reason":null},
        "managed_subscriptions_read":{"state":"UNAVAILABLE","is_complete":false,"next_cursor":null,"reason":"MANAGED_LEDGER_NOT_PROVIDED"},
        "transactions":transactions}))
}
