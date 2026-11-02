//! Evidence assembly, not adoption of comparison, refund-net or anomaly rules.
use crate::{
    query::{Limits, PagedSource, Request},
    spending::{analyze_spending, cents, ReportMetadata, SpendingReport, TransactionEvidence},
    TrustedScope,
};
use chrono::DateTime;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub struct WindowRequests {
    pub current: Request,
    pub comparable: Request,
    pub historical: Request,
    pub refund_boundary: Request,
}
#[derive(Debug, Serialize)]
pub struct HistoricalCategory {
    pub category_code: String,
    pub sample_count: usize,
    /// Twice the median, in cents. String avoids JSON integer precision loss.
    /// 201 means 100.5 cents, not 201 cents. No rounding or anomaly threshold.
    pub median_cents_twice: String,
    pub samples: Vec<TransactionEvidence>,
}
#[derive(Debug, Serialize)]
pub struct WindowReport {
    pub contract: &'static str,
    pub rule_status: &'static str,
    pub all_windows_complete: bool,
    pub comparison_rule_applied: bool,
    pub anomaly_rule_applied: bool,
    pub net_refund_rule_applied: bool,
    pub current: SpendingReport,
    pub comparable: SpendingReport,
    pub historical: SpendingReport,
    pub refund_boundary: SpendingReport,
    pub historical_categories: Option<Vec<HistoricalCategory>>,
    pub unclassified_history_count: Option<usize>,
}

pub fn analyze_windows(
    scope: &TrustedScope,
    source: &impl PagedSource,
    requests: &WindowRequests,
    metadata: &ReportMetadata,
    limits: Limits,
) -> Result<WindowReport, &'static str> {
    crate::validate_reference(&metadata.report_id).map_err(|_| "INVALID_REPORT_METADATA")?;
    let current = &requests.current;
    let parse = |s: &str| DateTime::parse_from_rfc3339(s).map_err(|_| "INVALID_WINDOW_SET");
    let current_start = parse(&current.window.start_inclusive)?;
    let as_of = parse(&current.window.as_of)?;
    let accounts: BTreeSet<_> = current.account_ids.iter().collect();
    for r in [
        &requests.current,
        &requests.comparable,
        &requests.historical,
        &requests.refund_boundary,
    ] {
        let start = parse(&r.window.start_inclusive)?;
        let end = parse(&r.window.end_exclusive)?;
        if r.account_ids.iter().collect::<BTreeSet<_>>() != accounts
            || r.window.timezone != current.window.timezone
            || parse(&r.window.as_of)? != as_of
            || start >= end
            || end > as_of
        {
            return Err("INVALID_WINDOW_SET");
        }
    }
    if parse(&requests.comparable.window.end_exclusive)? > current_start
        || parse(&requests.historical.window.end_exclusive)? > current_start
    {
        return Err("INVALID_WINDOW_SET");
    }
    let make = |r: &Request, label: &str| {
        analyze_spending(
            scope,
            r,
            source,
            limits,
            &ReportMetadata {
                report_id: format!("{}:{label}", metadata.report_id),
                calculated_at: metadata.calculated_at.clone(),
            },
        )
        .map_err(|_| "INVALID_REPORT_METADATA")
    };
    let current = make(current, "current")?;
    let comparable = make(&requests.comparable, "comparable")?;
    let historical = make(&requests.historical, "historical")?;
    let refund_boundary = make(&requests.refund_boundary, "refund_boundary")?;
    let (historical_categories, unclassified_history_count) = match historical.verified_for(scope) {
        Err(_) => (None, None),
        Ok(evidence) => {
            let mut groups: BTreeMap<String, Vec<TransactionEvidence>> = BTreeMap::new();
            let mut missing = 0;
            for row in &evidence.rows {
                if let Some(category) = &row.transaction.category_code {
                    groups
                        .entry(category.clone())
                        .or_default()
                        .push(row.clone());
                } else {
                    missing += 1;
                }
            }
            let categories = groups
                .into_iter()
                .map(|(category_code, mut samples)| {
                    samples.sort_by_key(|row| (cents(&row.transaction), row.reference.clone()));
                    let count = samples.len();
                    let middle = count / 2;
                    let a = cents(&samples[middle].transaction) as u128;
                    let b = cents(&samples[(count - 1) / 2].transaction) as u128;
                    HistoricalCategory {
                        category_code,
                        sample_count: count,
                        median_cents_twice: (a + b).to_string(),
                        samples,
                    }
                })
                .collect();
            (Some(categories), Some(missing))
        }
    };
    Ok(WindowReport {
        contract: "billing-window-evidence-v1-local",
        rule_status: "D08_D19_D10_PENDING_NOT_APPLIED",
        all_windows_complete: [&current, &comparable, &historical, &refund_boundary]
            .iter()
            .all(|r| r.read.is_complete),
        comparison_rule_applied: false,
        anomaly_rule_applied: false,
        net_refund_rule_applied: false,
        current,
        comparable,
        historical,
        refund_boundary,
        historical_categories,
        unclassified_history_count,
    })
}
