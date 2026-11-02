//! Local-only recognition. Mock context and mock_service_ref do not authenticate
//! a user or prove a real service identity. No production candidate IDs or writes.
use crate::{
    filter_evidence, load_fixture, Evidence, MockContext, Outcome, Report, Snapshot, Window,
};
use chrono::{DateTime, Datelike, FixedOffset, Months, NaiveDate};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub const LOCAL_RULE_VERSION: &str = "LOCAL_D22_HANNAH_2026_10_12_V1";

/// Explicit local choices, not an approved shared contract or model arguments.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct LocalRules {
    pub minimum_actual_charges: usize,
    pub maximum_missing_periods_between_charges: i64,
    pub daily_tolerance_days: i64,
    pub weekly_tolerance_days: i64,
    pub monthly_tolerance_days: i64,
    pub window_calendar_months: u32,
    pub amount_changes_gate_recognition: bool,
    pub merge_owned_accounts_for_same_mock_service: bool,
    pub monthly_anchor: &'static str,
    pub business_timezone: &'static str,
}

pub const LOCAL_RULES: LocalRules = LocalRules {
    minimum_actual_charges: 2,
    maximum_missing_periods_between_charges: 1,
    daily_tolerance_days: 0,
    weekly_tolerance_days: 1,
    monthly_tolerance_days: 3,
    window_calendar_months: 4,
    amount_changes_gate_recognition: false,
    merge_owned_accounts_for_same_mock_service: true,
    monthly_anchor: "FIRST_OBSERVED_BUSINESS_DAY_CLAMPED_IN_SHORT_MONTHS",
    business_timezone: "Asia/Shanghai",
};

#[derive(Clone, Copy, Debug, Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Cycle {
    Day,
    Week,
    CalendarMonth,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CycleStatus {
    Matches,
    MultipleChargesInPeriod,
    DoesNotMatch,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CycleAssessment {
    pub cycle: Cycle,
    pub status: CycleStatus,
    /// One entry per actual transaction, not a list of synthetic charges.
    pub observed_period_indices: Vec<i64>,
    pub missing_period_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GroupStatus {
    Suspected,
    NeedsReview,
    NeedsServiceEvidence,
    InsufficientEvidence,
    NoSupportedCycle,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ObservedCharge {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<crate::billing_adapter::SourceKey>,
    pub transaction_id: String,
    pub account_id: String,
    pub amount_minor_units: u64,
    pub completed_at: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SuspectedGroup {
    pub merchant_id: String,
    pub mock_service_ref: Option<String>,
    pub currency: &'static str,
    pub status: GroupStatus,
    pub message: &'static str,
    pub account_ids: Vec<String>,
    pub actual_charges: Vec<ObservedCharge>,
    pub cycles: Vec<CycleAssessment>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RecognitionStatus {
    EvidenceBlocked,
    InvalidRollingWindow,
    Reviewed,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecognitionReport {
    pub mode: &'static str,
    pub rule_version: &'static str,
    pub rules: LocalRules,
    pub recognition_executed: bool,
    pub team_contract_approved: bool,
    pub annual_recognition_supported: bool,
    pub status: RecognitionStatus,
    pub message: &'static str,
    pub production_registered: bool,
    pub managed_service_filter_applied: bool,
    pub filter_report: Report,
    pub groups: Vec<SuspectedGroup>,
}

/// Business time for this local Asia/Shanghai mock. The caller supplies as_of;
/// this helper does not fetch a trusted clock or prove source completeness.
pub fn rolling_four_month_window(as_of: &str) -> Option<Window> {
    let offset = FixedOffset::east_opt(8 * 60 * 60)?;
    let end = DateTime::parse_from_rfc3339(as_of)
        .ok()?
        .with_timezone(&offset);
    let start = end.checked_sub_months(Months::new(LOCAL_RULES.window_calendar_months))?;
    Some(Window {
        as_of: end.to_rfc3339(),
        start_inclusive: start.to_rfc3339(),
        end_exclusive: end.to_rfc3339(),
        timezone: LOCAL_RULES.business_timezone.into(),
    })
}

fn same_instant(a: &str, b: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(a),
        DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Reuses validation before returning any groups. The original evidence-only
/// CLI and historical fixture expectations deliberately remain unchanged.
pub fn recognize_mock(context: &MockContext, snapshot: &Snapshot) -> RecognitionReport {
    let filtered = filter_evidence(context, snapshot);
    let mut result = RecognitionReport {
        mode: "OFFLINE_MOCK_SUSPECTED_GROUPS_ONLY",
        rule_version: LOCAL_RULE_VERSION,
        rules: LOCAL_RULES,
        recognition_executed: false,
        team_contract_approved: false,
        annual_recognition_supported: false,
        status: RecognitionStatus::EvidenceBlocked,
        message: "消费证据未满足读取要求，未生成疑似分组。",
        production_registered: false,
        managed_service_filter_applied: false,
        filter_report: filtered,
        groups: vec![],
    };
    if !matches!(
        result.filter_report.outcome,
        Outcome::EvidenceReadyRulePending | Outcome::NoEligibleEvidence
    ) {
        return result;
    }
    let Some(window) = rolling_four_month_window(&snapshot.window.as_of) else {
        result.status = RecognitionStatus::InvalidRollingWindow;
        result.message = "无法计算最近四个日历月，未生成疑似分组。";
        return result;
    };
    if !same_instant(&snapshot.window.start_inclusive, &window.start_inclusive)
        || !same_instant(&snapshot.window.end_exclusive, &window.end_exclusive)
    {
        result.status = RecognitionStatus::InvalidRollingWindow;
        result.message = "输入范围不是查询时刻之前四个日历月，不能冒充完整识别。";
        return result;
    }

    // Only validated eligible transactions reach grouping; no display names or
    // amounts establish identity. Merchant refs scope mock service refs.
    let mut merchants: BTreeMap<&str, Vec<&Evidence>> = BTreeMap::new();
    for evidence in &result.filter_report.eligible_evidence {
        merchants
            .entry(&evidence.merchant_id)
            .or_default()
            .push(evidence);
    }
    for (merchant, evidence) in merchants {
        // Unknown service evidence can overlap known services. Keep ONE merchant
        // review group rather than inventing the unknown service's identity/count.
        if evidence.iter().any(|e| e.mock_service_ref.is_none()) {
            result.groups.push(build_group(merchant, None, evidence));
            continue;
        }
        let mut services: BTreeMap<&str, Vec<&Evidence>> = BTreeMap::new();
        for e in evidence {
            services
                .entry(e.mock_service_ref.as_deref().expect("checked above"))
                .or_default()
                .push(e);
        }
        for (service, evidence) in services {
            result
                .groups
                .push(build_group(merchant, Some(service), evidence));
        }
    }
    result.status = RecognitionStatus::Reviewed;
    result.recognition_executed = true;
    result.message = "本地疑似分组已计算；未精确过滤已在管台账，需用户核对，未登记订阅。";
    result
}

fn build_group(
    merchant: &str,
    service: Option<&str>,
    mut evidence: Vec<&Evidence>,
) -> SuspectedGroup {
    evidence.sort_by(|a, b| {
        let a_time = DateTime::parse_from_rfc3339(&a.completed_at).expect("validated");
        let b_time = DateTime::parse_from_rfc3339(&b.completed_at).expect("validated");
        a_time
            .cmp(&b_time)
            .then_with(|| a.transaction_id.cmp(&b.transaction_id))
            .then_with(|| a.account_id.cmp(&b.account_id))
            .then_with(|| {
                a.source_key
                    .as_ref()
                    .map(|k| &k.source_id)
                    .cmp(&b.source_key.as_ref().map(|k| &k.source_id))
            })
    });
    let dates: Vec<_> = evidence
        .iter()
        .map(|e| {
            DateTime::parse_from_rfc3339(&e.completed_at)
                .expect("validated")
                .with_timezone(&FixedOffset::east_opt(8 * 60 * 60).expect("constant offset"))
                .date_naive()
        })
        .collect();
    let account_ids: BTreeSet<_> = evidence.iter().map(|e| e.account_id.clone()).collect();
    let mut group = SuspectedGroup {
        merchant_id: merchant.into(),
        mock_service_ref: service.map(str::to_owned),
        currency: "CNY",
        status: GroupStatus::InsufficientEvidence,
        message: "不足两次实际消费，不能据此认定没有订阅。",
        account_ids: account_ids.into_iter().collect(),
        actual_charges: evidence
            .iter()
            .map(|e| ObservedCharge {
                source_key: e.source_key.clone(),
                transaction_id: e.transaction_id.clone(),
                account_id: e.account_id.clone(),
                amount_minor_units: e.amount_minor_units,
                completed_at: e.completed_at.clone(),
            })
            .collect(),
        cycles: vec![],
    };
    if service.is_none() {
        group.status = GroupStatus::NeedsServiceEvidence;
        group.message = "服务无法可靠区分，请核对；不确定是一项还是多项订阅。";
        return group;
    }
    if dates.len() < LOCAL_RULES.minimum_actual_charges {
        return group;
    }
    group.cycles = [Cycle::Day, Cycle::Week, Cycle::CalendarMonth]
        .into_iter()
        .map(|cycle| assess_cycle(&dates, cycle))
        .collect();
    let matches = group
        .cycles
        .iter()
        .filter(|c| c.status == CycleStatus::Matches)
        .count();
    // A same-period duplicate under an otherwise viable multi-period hypothesis
    // remains visible. A daily series within one month alone isn't proof that it
    // is duplicate monthly charging.
    // Multiple real charges on one business day remain a review signal even
    // when another charge invalidates every supported cycle hypothesis.
    // Do not change the failed cycle assessments into a matching cycle.
    let same_day_charges = dates.iter().collect::<BTreeSet<_>>().len() < dates.len();
    let duplicates = same_day_charges
        || group.cycles.iter().any(|c| {
            c.status == CycleStatus::MultipleChargesInPeriod
                && (matches == 0
                    || c.observed_period_indices
                        .iter()
                        .collect::<BTreeSet<_>>()
                        .len()
                        >= 2)
        });
    if duplicates || matches > 1 {
        group.status = GroupStatus::NeedsReview;
        group.message = if duplicates {
            "有多笔扣款，请核对；全部保留，不擅自挑一笔凑规律。"
        } else {
            "存在多种周期解释，请核对；不擅自确定唯一周期。"
        };
    } else if matches == 1 {
        group.status = GroupStatus::Suspected;
        group.message = "疑似周期消费，请确认是否为订阅；未自动登记。";
    } else {
        group.status = GroupStatus::NoSupportedCycle;
        group.message = "整组证据未符合已支持周期，不表示没有订阅；未丢弃记录另凑候选。";
    }
    group
}

fn assess_cycle(dates: &[NaiveDate], cycle: Cycle) -> CycleAssessment {
    let mut result = CycleAssessment {
        cycle,
        status: CycleStatus::DoesNotMatch,
        observed_period_indices: vec![],
        missing_period_count: 0,
    };
    let anchor = dates[0];
    for date in dates {
        let Some(period) = period_index(anchor, *date, cycle) else {
            // Partial mappings aren't reported as complete evidence.
            result.observed_period_indices.clear();
            return result;
        };
        result.observed_period_indices.push(period);
    }
    let mut duplicates = false;
    for pair in result.observed_period_indices.windows(2) {
        match pair[1] - pair[0] {
            0 => duplicates = true,
            1 => {}
            gap if gap > 1 && gap <= LOCAL_RULES.maximum_missing_periods_between_charges + 1 => {
                result.missing_period_count += (gap - 1) as usize;
            }
            _ => {
                result.missing_period_count = 0;
                return result;
            }
        }
    }
    result.status = if duplicates {
        CycleStatus::MultipleChargesInPeriod
    } else {
        CycleStatus::Matches
    };
    result
}

fn period_index(anchor: NaiveDate, date: NaiveDate, cycle: Cycle) -> Option<i64> {
    let days = date.signed_duration_since(anchor).num_days();
    match cycle {
        Cycle::Day => Some(days),
        Cycle::Week => {
            let period = (days + 3) / 7;
            ((days - period * 7).abs() <= LOCAL_RULES.weekly_tolerance_days).then_some(period)
        }
        Cycle::CalendarMonth => {
            let rough = i64::from(date.year() - anchor.year()) * 12 + i64::from(date.month())
                - i64::from(anchor.month());
            // A charge within +/-3 days may fall in an adjacent calendar month.
            (rough.saturating_sub(1).max(0)..=rough + 1).find(|period| {
                u32::try_from(*period)
                    .ok()
                    .and_then(|n| anchor.checked_add_months(Months::new(n)))
                    .is_some_and(|expected| {
                        date.signed_duration_since(expected).num_days().abs()
                            <= LOCAL_RULES.monthly_tolerance_days
                    })
            })
        }
    }
}

/// Parse all fixture metadata before producing output. Historical `expected`
/// sections are intentionally ignored, just as in the evidence-only reader.
pub fn replay_mock(json: &str) -> Result<serde_json::Value, &'static str> {
    let set = load_fixture(json)?;
    let reports: Vec<_> = set
        .samples
        .iter()
        .map(|sample| {
            serde_json::json!({
                "sample_id": sample.sample_id,
                "report": recognize_mock(&sample.trusted_context_fixture, &sample.snapshot),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "fictional": true,
        "historical_expected_fields_used": false,
        "production_registration": false,
        "rule_approval_scope": "USER_CONFIRMED_OFFLINE_ONLY",
        "sample_count": reports.len(),
        "reports": reports,
    }))
}
