//! Local duplicate-charge candidates, never an authorization to act.
use crate::{
    spending::{SourceKey, SpendingReport},
    TrustedScope,
};
use chrono::{DateTime, Duration, FixedOffset};
use serde::Serialize;
use std::collections::BTreeMap;
#[derive(Debug, Serialize)]
pub struct DuplicateReport {
    pub contract: &'static str,
    pub rule_version: &'static str,
    pub report_id: String,
    pub snapshot_id: String,
    pub source_id: String,
    pub production_registered: bool,
    pub state: &'static str,
    pub pairs: Option<Vec<DuplicatePair>>,
    pub missing_merchant: Vec<SourceKey>,
    pub risk_action_authorized: bool,
}
#[derive(Debug, Serialize)]
pub struct DuplicatePair {
    pub first: SourceKey,
    pub second: SourceKey,
}
/// Adapter must supply authoritative merchant IDs, not names or inferred labels.
/// This LOCAL rule produces candidate pairs, not confirmed duplicate charges.
pub fn detect_duplicates(
    scope: &TrustedScope,
    report: &SpendingReport,
) -> Result<DuplicateReport, &'static str> {
    let original = report.verified_for(scope)?;
    let mut result = DuplicateReport {
        contract: "LOCAL_DUPLICATE_CANDIDATES_V1",
        rule_version: "LOCAL_SAME_MERCHANT_CNY_AMOUNT_0_600S_V1",
        report_id: original.metadata.report_id.clone(),
        snapshot_id: original.snapshot_id.clone(),
        source_id: original.source_id.clone(),
        production_registered: false,
        state: "COMPLETE",
        pairs: Some(vec![]),
        missing_merchant: vec![],
        risk_action_authorized: false,
    };
    type TimedReference<'a> = (DateTime<FixedOffset>, &'a SourceKey);
    let mut groups: BTreeMap<(&str, &str, u64), Vec<TimedReference<'_>>> = BTreeMap::new();
    for evidence in &original.rows {
        let row = &evidence.transaction;
        let Some(merchant) = row.merchant_id.as_deref() else {
            result.missing_merchant.push(evidence.reference.clone());
            continue;
        };
        let time = DateTime::parse_from_rfc3339(
            row.completed_at
                .as_deref()
                .expect("verified completed time"),
        )
        .expect("collect validated time");
        groups
            .entry((merchant, &row.currency, crate::spending::cents(row)))
            .or_default()
            .push((time, &evidence.reference));
    }
    result.missing_merchant.sort();
    if !result.missing_merchant.is_empty() {
        result.state = "MERCHANT_EVIDENCE_INCOMPLETE";
    }
    let pairs = result.pairs.as_mut().expect("initialized pairs");
    for rows in groups.values_mut() {
        rows.sort();
        for (i, (first_time, first)) in rows.iter().enumerate() {
            for (second_time, second) in rows.iter().skip(i + 1) {
                if *second_time - *first_time > Duration::seconds(600) {
                    break;
                }
                if pairs.len() == 1000 {
                    result.state = "PAIR_LIMIT_EXCEEDED";
                    result.pairs = None;
                    return Ok(result);
                }
                pairs.push(DuplicatePair {
                    first: (*first).clone(),
                    second: (*second).clone(),
                });
            }
        }
    }
    Ok(result)
}
