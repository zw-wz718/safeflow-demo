use safeflow_billing_source_reader::{
    monthly_rules::*, query::*, spending::ReportMetadata, TrustedScope,
};
use serde_json::{json, Value};
use std::cell::Cell;

fn scope() -> TrustedScope {
    TrustedScope::from_verified_context("user_001", ["account_001"]).unwrap()
}
fn read_json(name: &str) -> Value {
    let variable = if name == "team_demo_consumption.proposed.v1.json" {
        "SAFEFLOW_BILLING_TEAM_DEMO_FIXTURE"
    } else {
        "SAFEFLOW_BILLING_RULE_ACCEPTANCE_FIXTURE"
    };
    let path = std::env::var_os(variable)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../fixtures/billing")
                .join(name)
        });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
struct Source {
    rows: Vec<Transaction>,
    fail: Option<&'static str>,
    incomplete: Option<&'static str>,
    calls: Cell<usize>,
}
impl Source {
    fn new(rows: Vec<Transaction>) -> Self {
        Self {
            rows,
            fail: None,
            incomplete: None,
            calls: Cell::new(0),
        }
    }
}
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        "safeflow-team-simulation"
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        r: &Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        self.calls.set(self.calls.get() + 1);
        if self
            .fail
            .is_some_and(|s| r.window.start_inclusive.starts_with(s))
        {
            return Err(PageError::Unavailable);
        }
        // Test adapter supplies fixed history; actual reader filters by completion/window.
        Ok(Page {
            snapshot_id: "fixed-test-capture".into(),
            owner_user_id: "user_001".into(),
            account_ids: r.account_ids.clone(),
            window: r.window.clone(),
            items: self.rows.clone(),
            has_more: false,
            next_cursor: None,
            range_complete: !self
                .incomplete
                .is_some_and(|s| r.window.start_inclusive.starts_with(s)),
            total_unique_rows: Some(self.rows.len()),
        })
    }
}
fn row(id: &str, amount: i64, at: &str) -> Transaction {
    serde_json::from_value(json!({"transaction_id":id,"account_id":"account_001","amount":format!("{}.{:02}",amount/100,amount%100),"currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION","status":"COMPLETED","completed_at":at,"category_code":"FOOD"})).unwrap()
}
fn refund(id: &str, amount: i64, at: &str) -> Transaction {
    let mut r = row(id, amount, at);
    r.direction = Direction::In;
    r.transaction_type = TransactionType::Refund;
    r.original_tx_id = None;
    r
}
fn run(source: &Source, asof: &str) -> Value {
    serde_json::to_value(
        analyze_monthly(
            &scope(),
            &["account_001".into()],
            asof,
            source,
            Limits::default(),
            &ReportMetadata {
                report_id: "monthly1".into(),
                calculated_at: asof.into(),
            },
        )
        .unwrap(),
    )
    .unwrap()
}
const ASOF: &str = "2026-10-27T00:00:00+08:00";

#[test]
fn adopted_window_vectors_use_the_product_planner() {
    let cases = read_json("rule_acceptance.proposed.v1.json");
    let mut checked = 0;
    for case in cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["rule"] == "D08" && c["option"] == "A")
    {
        let plan = plan_monthly(case["input"]["as_of"].as_str().unwrap()).unwrap();
        if case["expected"]["status"] == "NO_ELAPSED_WINDOW" {
            assert!(plan.is_none());
        } else {
            let p = plan.unwrap();
            assert_eq!(
                json!([p.current.start_inclusive, p.current.end_exclusive]),
                case["expected"]["current"],
                "{}",
                case["id"]
            );
            assert_eq!(
                json!([p.previous.start_inclusive, p.previous.end_exclusive]),
                case["expected"]["previous"],
                "{}",
                case["id"]
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 6);
}
#[test]
fn utc_is_normalized_and_invalid_dates_rejected() {
    assert_eq!(
        plan_monthly("2026-10-26T16:00:00Z")
            .unwrap()
            .unwrap()
            .current,
        plan_monthly(ASOF).unwrap().unwrap().current
    );
    assert!(plan_monthly("2026-02-30T00:00:00+08:00").is_err());
    let p = plan_monthly("2026-03-31T12:00:00+08:00").unwrap().unwrap();
    assert_ne!(p.current_duration, p.previous_duration);
}
#[test]
fn month_start_does_not_read_or_claim_zero_and_still_checks_scope() {
    let source = Source::new(vec![]);
    let v = run(&source, "2026-10-01T00:00:00+08:00");
    assert_eq!(v["status"], "NO_ELAPSED_WINDOW");
    assert!(v["current"].is_null());
    assert_eq!(source.calls.get(), 0);
    for accounts in [
        vec![],
        vec!["other".into()],
        vec!["account_001".into(), "account_001".into()],
    ] {
        assert!(analyze_monthly(
            &scope(),
            &accounts,
            "2026-10-01T00:00:00+08:00",
            &source,
            Limits::default(),
            &ReportMetadata {
                report_id: "r".into(),
                calculated_at: ASOF.into()
            }
        )
        .is_err());
    }
    assert_eq!(source.calls.get(), 0);
}
#[test]
fn adopted_fixture_net_and_category_anomalies_are_real_product_output() {
    let data = read_json("team_demo_consumption.proposed.v1.json");
    let rows = data["users"][0]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["page"]["items"].as_array().unwrap().iter().cloned())
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
    let v = run(&Source::new(rows), ASOF);
    assert_eq!(v["status"], "READY");
    assert_eq!(v["comparison"]["previous"]["net"], "379.80");
    assert_eq!(v["comparison"]["current"]["gross"], "162.80");
    assert_eq!(v["comparison"]["current"]["refunds"], "320.00");
    assert_eq!(v["comparison"]["current"]["net"], "-157.20");
    assert_eq!(v["comparison"]["difference"], "-537.00");
    assert_eq!(v["comparison"]["percent"], "-141.4");
    assert_eq!(v["comparison"]["current"]["refund_excess"], "157.20");
    assert_eq!(v["anomalies"].as_array().unwrap().len(), 5);
    for a in v["anomalies"].as_array().unwrap() {
        if a["category_code"] == "DEMO_MEDIA" {
            assert_eq!(a["status"], "NOT_TRIGGERED");
            assert_eq!(a["sample_count"], 6);
            assert_eq!(a["median_cents_twice"], "2980");
        } else {
            assert_eq!(a["status"], "INSUFFICIENT_EVIDENCE");
        }
    }
    assert_eq!(v["current"]["summary"]["gross_consumption"], "162.80");
    assert_eq!(v["feedback_policy"]["correction_supported"], false);
    assert_eq!(v["production_registered"], false);
}
#[test]
fn comparison_vectors_run_through_source_and_product() {
    let cases = read_json("rule_acceptance.proposed.v1.json");
    let mut checked = 0;
    for c in cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "comparison")
    {
        let mut rows = vec![];
        for (key, at) in [
            ("previous", "2026-09-10T00:00:00+08:00"),
            ("current", "2026-10-10T00:00:00+08:00"),
        ] {
            if let Some(n) = c["input"][format!("{key}_net_cents")].as_i64() {
                if n > 0 {
                    rows.push(row(key, n, at));
                } else if n < 0 {
                    rows.push(refund(key, -n, at));
                }
            }
        }
        let mut s = Source::new(rows);
        if c["input"]["source_complete"] == false {
            s.fail = Some("2026-09");
        }
        let v = run(&s, ASOF);
        let output = &v["comparison"];
        for key in ["previous", "current"] {
            let expected = &c["expected"][format!("{key}_net_cents")];
            if expected.is_null() {
                assert!(output[key].is_null());
            } else {
                assert_eq!(
                    output[key]["net_cents"],
                    expected.as_i64().unwrap().to_string(),
                    "{}",
                    c["id"]
                );
            }
        }
        assert_eq!(output["percent"], c["expected"]["percent"], "{}", c["id"]);
        checked += 1;
    }
    assert_eq!(checked, 8);
}
#[test]
fn threshold_vectors_run_with_typed_history_and_current_source() {
    let cases = read_json("rule_acceptance.proposed.v1.json");
    let mut checked = 0;
    for c in cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "anomaly" && c["id"] != "D10-zero")
    {
        let mut rows: Vec<_> = c["input"]["history_cents"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, n)| {
                row(
                    &format!("h{i}"),
                    n.as_i64().unwrap(),
                    "2026-08-10T00:00:00+08:00",
                )
            })
            .collect();
        let mut target = row(
            "target",
            c["input"]["target_cents"].as_i64().unwrap(),
            "2026-10-10T00:00:00+08:00",
        );
        if c["input"]["category_known"] == false {
            target.category_code = None;
        }
        rows.push(target);
        let mut s = Source::new(rows);
        if c["input"]["history_complete"] == false {
            s.incomplete = Some("2026-07");
        }
        let v = run(&s, ASOF);
        let a = &v["anomalies"][0];
        assert_eq!(a["status"], c["expected"]["status"], "{}", c["id"]);
        if !c["expected"]["median_cents_twice"].is_null() {
            assert_eq!(
                a["median_cents_twice"],
                c["expected"]["median_cents_twice"]
                    .as_i64()
                    .unwrap()
                    .to_string()
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 11);
}
#[test]
fn history_failure_does_not_erase_current_or_report_safe() {
    let mut s = Source::new(vec![row("c", 30000, "2026-10-10T00:00:00+08:00")]);
    s.fail = Some("2026-07");
    let v = run(&s, ASOF);
    assert_eq!(v["comparison"]["current"]["net"], "300.00");
    assert_eq!(v["anomalies"][0]["status"], "DATA_UNAVAILABLE");
    assert_eq!(v["status"], "PARTIAL");
}
#[test]
fn current_missing_and_true_empty_are_distinct() {
    let mut s = Source::new(vec![]);
    s.incomplete = Some("2026-10");
    let v = run(&s, ASOF);
    assert!(v["comparison"]["current"].is_null());
    assert!(v["anomalies"].is_null());
    assert!(v["comparison"]["difference"].is_null());
    let v = run(&Source::new(vec![]), ASOF);
    assert_eq!(v["comparison"]["current"]["net"], "0.00");
    assert_eq!(v["anomalies"], json!([]));
    assert_eq!(v["comparison"]["percent_reason"], "NO_POSITIVE_BASE");
}
#[test]
fn unknown_refund_origin_counts_only_in_cash_period() {
    let v = run(
        &Source::new(vec![refund("r", 500, "2026-10-10T00:00:00+08:00")]),
        ASOF,
    );
    assert_eq!(v["comparison"]["current"]["net"], "-5.00");
    assert_eq!(
        v["comparison"]["current"]["refund_origin_status"],
        "ORIGINAL_NOT_VERIFIED"
    );
}
#[test]
fn historical_window_filters_current_and_outside_samples() {
    let mut rows: Vec<_> = (0..4)
        .map(|i| row(&format!("h{i}"), 10000, "2026-08-10T00:00:00+08:00"))
        .collect();
    rows.push(row("old", 10000, "2026-07-02T23:59:59+08:00"));
    rows.push(row("target", 30000, "2026-10-01T00:00:00+08:00"));
    let v = run(&Source::new(rows), ASOF);
    assert_eq!(v["anomalies"][0]["sample_count"], 4);
    assert_eq!(v["anomalies"][0]["status"], "INSUFFICIENT_EVIDENCE");
}

#[test]
fn zero_source_amount_is_rejected_before_median_evaluation() {
    let mut value = serde_json::to_value(row("zero", 1, "2026-08-01T00:00:00+08:00")).unwrap();
    value["amount"] = json!("0.00");
    assert!(serde_json::from_value::<Transaction>(value).is_err());
}
fn max_row(id: &str, at: &str) -> Transaction {
    let mut value = serde_json::to_value(row(id, 1, at)).unwrap();
    value["amount"] = json!("184467440737095516.15");
    serde_json::from_value(value).unwrap()
}
#[test]
fn maximum_amount_percentage_uses_wide_integer_math() {
    let v = run(
        &Source::new(vec![
            row("previous", 1, "2026-09-10T00:00:00+08:00"),
            max_row("max", "2026-10-10T00:00:00+08:00"),
        ]),
        ASOF,
    );
    assert_eq!(
        v["comparison"]["current"]["net_cents"],
        "18446744073709551615"
    );
    assert_eq!(v["comparison"]["percent"], "1844674407370955161400.0");
}
#[test]
fn aggregate_overflow_is_unavailable_not_zero_or_wrapped() {
    let at = "2026-10-10T00:00:00+08:00";
    let v = run(&Source::new(vec![max_row("a", at), row("b", 1, at)]), ASOF);
    assert!(v["comparison"]["current"].is_null());
    assert_eq!(v["comparison"]["current_reason"], "CALCULATION_UNAVAILABLE");
    assert!(v["comparison"]["difference"].is_null());
    let mut a = max_row("ra", at);
    a.direction = Direction::In;
    a.transaction_type = TransactionType::Refund;
    let v = run(&Source::new(vec![a, refund("rb", 1, at)]), ASOF);
    assert!(v["comparison"]["current"].is_null());
    assert_eq!(v["comparison"]["current_reason"], "AMOUNT_OVERFLOW");
}
#[test]
fn foreign_account_or_currency_cannot_produce_net_or_anomalies() {
    for foreign_account in [false, true] {
        let mut r = row("bad", 30000, "2026-10-10T00:00:00+08:00");
        if foreign_account {
            r.account_id = "account_002".into();
        } else {
            r.currency = "USD".into();
        }
        let v = run(&Source::new(vec![r]), ASOF);
        assert!(v["comparison"]["current"].is_null());
        assert!(v["anomalies"].is_null());
    }
}
#[test]
fn uncompleted_transfer_and_refund_are_not_anomaly_samples() {
    let mut rows: Vec<_> = (0..4)
        .map(|i| row(&format!("h{i}"), 10000, "2026-08-10T00:00:00+08:00"))
        .collect();
    let mut transfer = row("transfer", 10000, "2026-08-10T00:00:00+08:00");
    transfer.transaction_type = TransactionType::Transfer;
    rows.push(transfer);
    rows.push(refund("refund", 10000, "2026-08-10T00:00:00+08:00"));
    for (id, status) in [("pending", Status::Processing), ("failed", Status::Failed)] {
        let mut r = row(id, 10000, "2026-08-10T00:00:00+08:00");
        r.status = status;
        r.completed_at = None;
        rows.push(r);
    }
    rows.push(row("current", 30000, "2026-10-10T00:00:00+08:00"));
    let v = run(&Source::new(rows), ASOF);
    assert_eq!(v["anomalies"][0]["sample_count"], 4);
    assert_eq!(v["anomalies"][0]["status"], "INSUFFICIENT_EVIDENCE");
}
#[test]
fn invalid_metadata_is_rejected_even_at_month_start() {
    let source = Source::new(vec![]);
    for (id, at) in [
        ("", "2026-10-01T00:00:00+08:00"),
        ("r", "2026-09-30T00:00:00+08:00"),
        ("r", "invalid"),
    ] {
        assert!(analyze_monthly(
            &scope(),
            &["account_001".into()],
            "2026-10-01T00:00:00+08:00",
            &source,
            Limits::default(),
            &ReportMetadata {
                report_id: id.into(),
                calculated_at: at.into()
            }
        )
        .is_err());
    }
    assert_eq!(source.calls.get(), 0);
}
