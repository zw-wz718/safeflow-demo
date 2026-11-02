//! Adopted D08-A/D19-A/D10-A and D20-A display boundary (68e09f29).
//! Read-only library entry point; no registration, feedback mutation or card action.
use crate::{
    query::{Limits, PagedSource, Request, Window},
    spending::{
        analyze_spending, cents, CalculationStatus, ReportMetadata, SourceKey, SpendingReport,
    },
    TrustedScope,
};
use chrono::{DateTime, Datelike, Duration, FixedOffset, Months, Timelike};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub const RULE_VERSION: &str = "billing-rules-adopted-20261027-v1";
pub const ADOPTION_COMMIT: &str = "68e09f29ec31680f3544b1f3242f38bcec3fab15";

#[derive(Debug, Serialize)]
pub struct MonthlyPlan {
    pub current: Window,
    pub previous: Window,
    pub historical: Window,
    /// Exact ISO-8601 durations. Short months may have unequal elapsed times.
    pub current_duration: String,
    pub previous_duration: String,
}

/// The host freezes as_of from its trusted clock. This planner grants no scope.
/// None means no elapsed interval at month start, not an empty complete source.
pub fn plan_monthly(as_of: &str) -> Result<Option<MonthlyPlan>, &'static str> {
    let offset = FixedOffset::east_opt(8 * 3600).ok_or("INVALID_AS_OF")?;
    let end = DateTime::parse_from_rfc3339(as_of)
        .map_err(|_| "INVALID_AS_OF")?
        .with_timezone(&offset);
    let start = end
        .with_day(1)
        .and_then(|v| v.with_hour(0))
        .and_then(|v| v.with_minute(0))
        .and_then(|v| v.with_second(0))
        .and_then(|v| v.with_nanosecond(0))
        .ok_or("INVALID_AS_OF")?;
    if start == end {
        return Ok(None);
    }
    let previous_start = start
        .checked_sub_months(Months::new(1))
        .ok_or("INVALID_AS_OF")?;
    // Do not clamp to the last day's same time: a nonexistent date means FULL prior month.
    let previous_end = previous_start
        .with_day(end.day())
        .and_then(|v| v.with_hour(end.hour()))
        .and_then(|v| v.with_minute(end.minute()))
        .and_then(|v| v.with_second(end.second()))
        .and_then(|v| v.with_nanosecond(end.nanosecond()))
        .unwrap_or(start);
    let historical_start = start
        .checked_sub_signed(Duration::days(90))
        .ok_or("INVALID_AS_OF")?;
    let make = |a: DateTime<FixedOffset>, b: DateTime<FixedOffset>| Window {
        as_of: end.to_rfc3339(),
        start_inclusive: a.to_rfc3339(),
        end_exclusive: b.to_rfc3339(),
        timezone: "Asia/Shanghai".into(),
    };
    Ok(Some(MonthlyPlan {
        current: make(start, end),
        previous: make(previous_start, previous_end),
        historical: make(historical_start, start),
        current_duration: (end - start).to_string(),
        previous_duration: (previous_end - previous_start).to_string(),
    }))
}

#[derive(Debug, Serialize)]
pub struct NetPeriod {
    pub gross: String,
    pub refunds: String,
    pub net: String,
    /// Signed integer cents encoded as text to avoid JSON precision loss.
    pub net_cents: String,
    pub refund_excess: Option<String>,
    /// Cash-period aggregation does not validate/guess original purchase linkage.
    pub refund_origin_status: &'static str,
    #[serde(skip)]
    value: i128,
}
#[derive(Debug, Serialize)]
pub struct Comparison {
    pub status: &'static str,
    pub current: Option<NetPeriod>,
    pub previous: Option<NetPeriod>,
    pub current_reason: Option<&'static str>,
    pub previous_reason: Option<&'static str>,
    pub difference: Option<String>,
    pub percent: Option<String>,
    pub percent_reason: Option<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct Anomaly {
    pub reference: SourceKey,
    pub category_code: Option<String>,
    pub amount: String,
    pub status: &'static str,
    pub reason: &'static str,
    pub sample_count: Option<usize>,
    pub median_cents_twice: Option<String>,
    pub message: &'static str,
}
#[derive(Debug, Serialize)]
pub struct FeedbackPolicy {
    pub correction_supported: bool,
    pub revocation_supported: bool,
    pub current_feedback_head_supported: bool,
    pub historical_feedback_is_action_authorization: bool,
    pub message: &'static str,
}
#[derive(Debug, Serialize)]
pub struct MonthlyReport {
    pub contract: &'static str,
    pub rule_version: &'static str,
    pub adoption_commit: &'static str,
    pub production_registered: bool,
    pub status: &'static str,
    pub metadata: ReportMetadata,
    pub plan: Option<MonthlyPlan>,
    /// Original gross reports remain available for the existing feedback preparation API.
    /// Their original rule versions and immutable evidence are NOT relabelled.
    pub current: Option<SpendingReport>,
    pub previous: Option<SpendingReport>,
    pub historical: Option<SpendingReport>,
    pub comparison: Option<Comparison>,
    pub anomalies: Option<Vec<Anomaly>>,
    pub feedback_policy: FeedbackPolicy,
}

fn money(value: i128) -> String {
    let absolute = value.unsigned_abs();
    format!(
        "{}{}.{:02}",
        if value < 0 { "-" } else { "" },
        absolute / 100,
        absolute % 100
    )
}
fn net_period(report: &SpendingReport, scope: &TrustedScope) -> Result<NetPeriod, &'static str> {
    let verified = report.verified_for(scope).map_err(|_| "DATA_UNAVAILABLE")?;
    if report.calculation_status != CalculationStatus::Ready {
        return Err("CALCULATION_UNAVAILABLE");
    }
    let sum = |rows: &[crate::spending::TransactionEvidence]| {
        rows.iter()
            .try_fold(0u64, |total, row| {
                total.checked_add(cents(&row.transaction))
            })
            .ok_or("AMOUNT_OVERFLOW")
    };
    let gross = sum(&verified.rows)?;
    let refunds = report.refunds.as_deref().ok_or("DATA_UNAVAILABLE")?;
    let refund_total = sum(refunds)?;
    let value = i128::from(gross) - i128::from(refund_total);
    Ok(NetPeriod {
        gross: money(i128::from(gross)),
        refunds: money(i128::from(refund_total)),
        net: money(value),
        net_cents: value.to_string(),
        refund_excess: (value < 0).then(|| money(-value)),
        refund_origin_status: if refunds.is_empty() {
            "NO_REFUNDS"
        } else {
            "ORIGINAL_NOT_VERIFIED"
        },
        value,
    })
}
fn comparison(
    current: &SpendingReport,
    previous: &SpendingReport,
    scope: &TrustedScope,
) -> Comparison {
    let current = net_period(current, scope);
    let previous = net_period(previous, scope);
    let current_reason = current.as_ref().err().copied();
    let previous_reason = previous.as_ref().err().copied();
    let (current, previous) = (current.ok(), previous.ok());
    let (difference, percent, percent_reason) = match (&current, &previous) {
        (Some(c), Some(p)) => {
            let diff = c.value - p.value;
            if p.value > 0 {
                // 0.1 percentage point = ratio * 1000. Exact, half away from zero.
                // Each period fits +/-u64 cents, so *1000 and *2 fit i128/u128.
                let numerator = diff.unsigned_abs() * 1000;
                let denominator = p.value as u128;
                let tenths = numerator / denominator
                    + u128::from((numerator % denominator) * 2 >= denominator);
                let text = format!(
                    "{}{}.{:01}",
                    if diff < 0 && tenths != 0 { "-" } else { "" },
                    tenths / 10,
                    tenths % 10
                );
                (Some(money(diff)), Some(text), None)
            } else {
                (
                    Some(money(diff)),
                    None,
                    Some(if p.value == 0 {
                        "NO_POSITIVE_BASE"
                    } else {
                        "NEGATIVE_PREVIOUS_NET"
                    }),
                )
            }
        }
        _ => (None, None, Some("DATA_OR_CALCULATION_UNAVAILABLE")),
    };
    Comparison {
        status: if current.is_some() && previous.is_some() {
            "READY"
        } else {
            "UNAVAILABLE"
        },
        current,
        previous,
        current_reason,
        previous_reason,
        difference,
        percent,
        percent_reason,
    }
}
fn anomalies(
    current: &SpendingReport,
    historical: &SpendingReport,
    scope: &TrustedScope,
) -> Option<Vec<Anomaly>> {
    let current = current.verified_for(scope).ok()?;
    let history = historical.verified_for(scope).ok();
    let mut groups: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    if let Some(history) = history {
        for row in &history.rows {
            if let Some(category) = row.transaction.category_code.as_deref() {
                groups
                    .entry(category)
                    .or_default()
                    .push(cents(&row.transaction));
            }
        }
    }
    let medians: BTreeMap<_, _> = groups
        .into_iter()
        .map(|(category, mut values)| {
            values.sort_unstable();
            let count = values.len();
            let twice = u128::from(values[(count - 1) / 2]) + u128::from(values[count / 2]);
            (category, (count, twice))
        })
        .collect();
    Some(
        current
            .rows
            .iter()
            .map(|row| {
                let mut result = Anomaly {
                    reference: row.reference.clone(),
                    category_code: row.transaction.category_code.clone(),
                    amount: row.transaction.amount.as_str().into(),
                    status: "INSUFFICIENT_EVIDENCE",
                    reason: "CATEGORY_UNKNOWN",
                    sample_count: None,
                    median_cents_twice: None,
                    message: "历史证据不足，无法判断金额是否偏高",
                };
                if history.is_none() {
                    result.status = "DATA_UNAVAILABLE";
                    result.reason = "HISTORY_READ_NOT_COMPLETE";
                    result.message = "历史数据不可用，无法判断金额是否偏高";
                } else if let Some(category) = row.transaction.category_code.as_deref() {
                    let (count, twice) = medians.get(category).copied().unwrap_or((0, 0));
                    result.sample_count = Some(count);
                    result.reason = "FEWER_THAN_FIVE_SAMPLES";
                    if count >= 5 {
                        result.reason = "NON_POSITIVE_MEDIAN";
                        if twice > 0 {
                            let amount_twice = u128::from(cents(&row.transaction)) * 2;
                            let triggered =
                                amount_twice >= twice * 3 && amount_twice >= twice + 40000;
                            result.status = if triggered {
                                "NEEDS_REVIEW"
                            } else {
                                "NOT_TRIGGERED"
                            };
                            result.reason = if triggered {
                                "BOTH_AMOUNT_THRESHOLDS_MET"
                            } else {
                                "AMOUNT_THRESHOLDS_NOT_BOTH_MET"
                            };
                            result.message = if triggered {
                                "金额较历史偏高，请核对"
                            } else {
                                "未触发该金额规则，不代表交易安全"
                            };
                            result.median_cents_twice = Some(twice.to_string());
                        }
                    }
                }
                result
            })
            .collect(),
    )
}

/// Calls the actual typed reader for three windows. Scope/as_of/metadata must come
/// from the trusted host; this is not an authentication or HTTP endpoint.
pub fn analyze_monthly(
    scope: &TrustedScope,
    account_ids: &[String],
    as_of: &str,
    source: &impl PagedSource,
    limits: Limits,
    metadata: &ReportMetadata,
) -> Result<MonthlyReport, &'static str> {
    if account_ids.is_empty()
        || account_ids.iter().collect::<BTreeSet<_>>().len() != account_ids.len()
        || account_ids.iter().any(|a| !scope.contains_account(a))
    {
        return Err("RESOURCE_UNAVAILABLE");
    }
    crate::validate_reference(&metadata.report_id).map_err(|_| "INVALID_REPORT_METADATA")?;
    let asof = DateTime::parse_from_rfc3339(as_of).map_err(|_| "INVALID_AS_OF")?;
    let calculated = DateTime::parse_from_rfc3339(&metadata.calculated_at)
        .map_err(|_| "INVALID_REPORT_METADATA")?;
    if calculated < asof {
        return Err("INVALID_REPORT_METADATA");
    }
    let plan = plan_monthly(as_of)?;
    let mut report = MonthlyReport {
        contract: "billing-monthly-rules-v1",
        rule_version: RULE_VERSION,
        adoption_commit: ADOPTION_COMMIT,
        production_registered: false,
        status: "NO_ELAPSED_WINDOW",
        metadata: metadata.clone(),
        plan,
        current: None,
        previous: None,
        historical: None,
        comparison: None,
        anomalies: None,
        feedback_policy: FeedbackPolicy {
            correction_supported: false,
            revocation_supported: false,
            current_feedback_head_supported: false,
            historical_feedback_is_action_authorization: false,
            message: "当前版本暂不支持反馈更正或撤销；历史反馈不能充当当前有效状态或锁卡许可",
        },
    };
    if let Some(plan) = &report.plan {
        let make = |window: &Window, suffix: &str| {
            analyze_spending(
                scope,
                &Request {
                    account_ids: account_ids.to_vec(),
                    window: window.clone(),
                },
                source,
                limits,
                &ReportMetadata {
                    report_id: format!("{}:{suffix}", metadata.report_id),
                    calculated_at: metadata.calculated_at.clone(),
                },
            )
            .map_err(|_| "INVALID_REPORT_METADATA")
        };
        let current = make(&plan.current, "current")?;
        let previous = make(&plan.previous, "previous")?;
        let historical = make(&plan.historical, "historical")?;
        let comparison = comparison(&current, &previous, scope);
        let anomalies = anomalies(&current, &historical, scope);
        report.status = if comparison.status == "READY" && historical.read.is_complete {
            "READY"
        } else {
            "PARTIAL"
        };
        report.comparison = Some(comparison);
        report.anomalies = anomalies;
        report.current = Some(current);
        report.previous = Some(previous);
        report.historical = Some(historical);
    }
    Ok(report)
}
