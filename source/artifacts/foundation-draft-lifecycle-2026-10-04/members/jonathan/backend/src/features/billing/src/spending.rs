//! Local, read-only gross consumption report. No authentication or write actions.
use crate::{query::*, TrustedScope};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportMetadata {
    pub report_id: String,
    pub calculated_at: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpendingError {
    InvalidMetadata,
}
impl std::fmt::Display for SpendingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("INVALID_REPORT_METADATA")
    }
}
impl std::error::Error for SpendingError {}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CalculationStatus {
    Ready,
    ReadBlocked,
    CalculationUnavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceKey {
    pub source_id: String,
    pub account_id: String,
    pub transaction_id: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct TransactionEvidence {
    pub reference: SourceKey,
    pub transaction: Transaction,
}
#[derive(Debug, Serialize)]
pub struct ExclusionEvidence {
    pub reference: SourceKey,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct CategorySummary {
    pub category_code: Option<String>,
    pub display_name: String,
    pub count: usize,
    pub total: String,
    pub references: Vec<SourceKey>,
}
#[derive(Debug, Serialize)]
pub struct SpendingSummary {
    pub count: usize,
    pub gross_consumption: String,
    pub currency: String,
    pub categories: Vec<CategorySummary>,
    pub top_five: Vec<TransactionEvidence>,
}
#[derive(Debug, Serialize)]
pub struct ReadSummary {
    pub source_id: String,
    pub state: ReadState,
    pub is_complete: bool,
    pub reason: Option<String>,
    pub snapshot_id: Option<String>,
    pub pages_read: usize,
    pub duplicates_ignored: usize,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Capability {
    pub executed: bool,
    pub reason: &'static str,
}
#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub net_consumption: Capability,
    pub comparison: Capability,
    pub anomalies: Capability,
}
#[derive(Debug, Serialize)]
pub struct SpendingReport {
    pub contract: &'static str,
    pub rule_version: &'static str,
    pub production_registered: bool,
    pub metadata: ReportMetadata,
    pub request: Request,
    pub read: ReadSummary,
    pub calculation_status: CalculationStatus,
    pub calculation_reason: Option<String>,
    pub summary: Option<SpendingSummary>,
    pub evidence: Option<Vec<TransactionEvidence>>,
    pub refunds: Option<Vec<TransactionEvidence>>,
    pub exclusions: Option<Vec<ExclusionEvidence>>,
    pub capabilities: Capabilities,
    #[serde(skip)]
    verified: Option<VerifiedReport>,
}
/// Server-side immutable evidence, not a credential or persistent report store.
/// Keeping this separate prevents mutable display fields from re-binding actions.
#[derive(Debug)]
pub(crate) struct VerifiedReport {
    pub owner_user_id: String,
    pub metadata: ReportMetadata,
    pub request: Request,
    pub source_id: String,
    pub snapshot_id: String,
    pub rule_version: String,
    pub rows: Vec<TransactionEvidence>,
}
impl SpendingReport {
    pub(crate) fn verified_for(
        &self,
        scope: &TrustedScope,
    ) -> Result<&VerifiedReport, &'static str> {
        let verified = self.verified.as_ref().ok_or("REPORT_NOT_COMPLETE")?;
        if verified.owner_user_id != scope.user_id()
            || verified
                .request
                .account_ids
                .iter()
                .any(|id| !scope.contains_account(id))
        {
            return Err("RESOURCE_UNAVAILABLE");
        }
        Ok(verified)
    }
}
fn reference(source: &str, row: &Transaction) -> SourceKey {
    SourceKey {
        source_id: source.into(),
        account_id: row.account_id.clone(),
        transaction_id: row.transaction_id.clone(),
    }
}
fn evidence(source: &str, row: &Transaction) -> TransactionEvidence {
    TransactionEvidence {
        reference: reference(source, row),
        transaction: row.clone(),
    }
}
pub(crate) fn cents(row: &Transaction) -> u64 {
    let (whole, fraction) = row
        .amount
        .as_str()
        .split_once('.')
        .expect("validated CnyAmount");
    whole.parse::<u64>().expect("validated CnyAmount") * 100
        + fraction.parse::<u64>().expect("validated CnyAmount")
}
fn money(value: u64) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}
fn completed_at(row: &Transaction) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(
        row.completed_at
            .as_deref()
            .expect("validated completed row"),
    )
    .expect("collect validated completed time")
}
fn summarize(rows: &[TransactionEvidence]) -> Option<SpendingSummary> {
    let mut total = 0u64;
    let mut groups: BTreeMap<Option<String>, (u64, Vec<SourceKey>)> = BTreeMap::new();
    for row in rows {
        let value = cents(&row.transaction);
        total = total.checked_add(value)?;
        let group = groups
            .entry(row.transaction.category_code.clone())
            .or_default();
        group.0 = group.0.checked_add(value)?;
        group.1.push(row.reference.clone());
    }
    let categories = groups
        .into_iter()
        .map(|(code, (value, mut refs))| {
            refs.sort();
            CategorySummary {
                display_name: code.clone().unwrap_or_else(|| "未分类".into()),
                category_code: code,
                count: refs.len(),
                total: money(value),
                references: refs,
            }
        })
        .collect();
    let mut top = rows.to_vec();
    top.sort_by(|a, b| {
        cents(&b.transaction)
            .cmp(&cents(&a.transaction))
            .then_with(|| completed_at(&b.transaction).cmp(&completed_at(&a.transaction)))
            .then_with(|| a.reference.cmp(&b.reference))
    });
    top.truncate(5);
    Some(SpendingSummary {
        count: rows.len(),
        gross_consumption: money(total),
        currency: "CNY".into(),
        categories,
        top_five: top,
    })
}

/// Calls the source itself; a caller-supplied serialized read report is never accepted.
pub fn analyze_spending(
    scope: &TrustedScope,
    request: &Request,
    source: &impl PagedSource,
    limits: Limits,
    metadata: &ReportMetadata,
) -> Result<SpendingReport, SpendingError> {
    let calculated_at = DateTime::parse_from_rfc3339(&metadata.calculated_at)
        .map_err(|_| SpendingError::InvalidMetadata)?;
    if crate::validate_reference(&metadata.report_id).is_err()
        || DateTime::parse_from_rfc3339(&request.window.as_of)
            .ok()
            .is_some_and(|as_of| calculated_at < as_of)
    {
        return Err(SpendingError::InvalidMetadata);
    }
    let read = collect(scope, request, source, limits);
    let mut result = SpendingReport {
        contract: "LOCAL_BILLING_SPENDING_V1",
        rule_version: "LOCAL_BASIC_SPENDING_2026_10_16_V1",
        production_registered: false,
        metadata: metadata.clone(),
        request: request.clone(),
        read: ReadSummary {
            source_id: read.source_id.clone(),
            state: read.state,
            is_complete: read.is_complete,
            reason: read.reason.clone(),
            snapshot_id: read.snapshot_id.clone(),
            pages_read: read.pages_read,
            duplicates_ignored: read.duplicates_ignored,
            next_cursor: read.next_cursor.clone(),
        },
        calculation_status: CalculationStatus::ReadBlocked,
        calculation_reason: Some("READ_NOT_COMPLETE".into()),
        summary: None,
        evidence: None,
        refunds: None,
        exclusions: None,
        capabilities: Capabilities {
            net_consumption: Capability {
                executed: false,
                reason: "REFUND_FORMULA_NOT_CONFIRMED",
            },
            comparison: Capability {
                executed: false,
                reason: "COMPARISON_NOT_IMPLEMENTED_RULES_UNCONFIRMED",
            },
            anomalies: Capability {
                executed: false,
                reason: "ANOMALY_DETECTION_NOT_IMPLEMENTED",
            },
        },
        verified: None,
    };
    if read.state != ReadState::Complete || !read.is_complete {
        return Ok(result);
    }
    let rows: Vec<_> = read
        .eligible_consumption
        .iter()
        .map(|r| evidence(&read.source_id, r))
        .collect();
    result.verified = Some(VerifiedReport {
        owner_user_id: scope.user_id().into(),
        metadata: metadata.clone(),
        request: request.clone(),
        source_id: read.source_id.clone(),
        snapshot_id: read.snapshot_id.clone().expect("collect verified snapshot"),
        rule_version: result.rule_version.into(),
        rows: rows.clone(),
    });
    let eligible: BTreeSet<_> = rows.iter().map(|r| r.reference.clone()).collect();
    // Positional pairing is not used: duplicate transaction IDs may belong to different accounts.
    let exclusions = read
        .observed_transactions
        .iter()
        .filter(|r| !eligible.contains(&reference(&read.source_id, r)))
        .map(|r| {
            let reason = if r.status != Status::Completed {
                "NOT_COMPLETED"
            } else if r.transaction_type != TransactionType::Consumption {
                "NOT_CONSUMPTION"
            } else if r.direction != Direction::Out {
                "NOT_OUTGOING"
            } else {
                "OUTSIDE_WINDOW"
            };
            ExclusionEvidence {
                reference: reference(&read.source_id, r),
                reason: reason.into(),
            }
        })
        .collect();
    let start = DateTime::parse_from_rfc3339(&request.window.start_inclusive)
        .expect("collect validated window");
    let end = DateTime::parse_from_rfc3339(&request.window.end_exclusive)
        .expect("collect validated window");
    result.refunds = Some(
        read.observed_transactions
            .iter()
            .filter(|r| {
                r.transaction_type == TransactionType::Refund
                    && r.status == Status::Completed
                    && r.direction == Direction::In
                    && completed_at(r) >= start
                    && completed_at(r) < end
            })
            .map(|r| evidence(&read.source_id, r))
            .collect(),
    );
    result.summary = summarize(&rows);
    result.calculation_status = if result.summary.is_some() {
        CalculationStatus::Ready
    } else {
        CalculationStatus::CalculationUnavailable
    };
    result.calculation_reason = if result.summary.is_some() {
        None
    } else {
        Some("AMOUNT_OVERFLOW".into())
    };
    result.evidence = Some(rows);
    result.exclusions = Some(exclusions);
    Ok(result)
}
