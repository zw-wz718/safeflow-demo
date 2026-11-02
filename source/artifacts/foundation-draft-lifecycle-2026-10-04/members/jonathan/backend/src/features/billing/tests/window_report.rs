use safeflow_billing_source_reader::{
    query::*, spending::ReportMetadata, window_report::*, TrustedScope,
};
use serde_json::{json, Value};
struct Source {
    fail_history: bool,
    incomplete_prior: bool,
}
fn requests() -> WindowRequests {
    fn req(start: &str, end: &str) -> Request {
        serde_json::from_value(json!({"account_ids":["account1"],"window":{
        "as_of":"2026-10-19T12:00:00+08:00","start_inclusive":start,"end_exclusive":end,"timezone":"Asia/Shanghai"}})).unwrap()
    }
    WindowRequests {
        current: req("2026-10-01T00:00:00+08:00", "2026-10-19T12:00:00+08:00"),
        comparable: req("2026-09-01T00:00:00+08:00", "2026-09-19T12:00:00+08:00"),
        historical: req("2026-07-01T00:00:00+08:00", "2026-10-01T00:00:00+08:00"),
        refund_boundary: req("2026-06-01T00:00:00+08:00", "2026-10-19T12:00:00+08:00"),
    }
}
fn scope() -> TrustedScope {
    TrustedScope::from_verified_context("user1", ["account1"]).unwrap()
}
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        "fictional-window-source"
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        r: &Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        let historical = r.window.start_inclusive.starts_with("2026-07");
        if historical && self.fail_history {
            return Err(PageError::Unavailable);
        }
        let rows = if historical {
            vec![
                ("a", "1.00", Some("food")),
                ("b", "1.01", Some("food")),
                ("c", "99.00", None),
            ]
        } else {
            vec![("current", "2.00", Some("food"))]
        };
        let items=rows.into_iter().map(|(id,amount,cat)|serde_json::from_value(json!({"transaction_id":id,"account_id":"account1",
            "amount":amount,"currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION","status":"COMPLETED",
            "completed_at":r.window.start_inclusive,"category_code":cat})).unwrap()).collect::<Vec<_>>();
        Ok(Page {
            snapshot_id: format!("snapshot-{}", r.window.start_inclusive),
            owner_user_id: "user1".into(),
            account_ids: r.account_ids.clone(),
            window: r.window.clone(),
            total_unique_rows: Some(items.len()),
            items,
            has_more: false,
            next_cursor: None,
            range_complete: !(self.incomplete_prior
                && r.window.start_inclusive.starts_with("2026-09")),
        })
    }
}
fn run(src: Source) -> Value {
    serde_json::to_value(
        analyze_windows(
            &scope(),
            &src,
            &requests(),
            &ReportMetadata {
                report_id: "bundle1".into(),
                calculated_at: "2026-10-19T12:00:00+08:00".into(),
            },
            Limits::default(),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn failed_history_does_not_erase_current_or_invent_a_zero_median() {
    let v = run(Source {
        fail_history: true,
        incomplete_prior: false,
    });
    assert_eq!(v["current"]["summary"]["gross_consumption"], "2.00");
    assert_eq!(v["historical"]["read"]["is_complete"], false);
    assert!(v["historical_categories"].is_null());
    assert_eq!(v["all_windows_complete"], false);
    assert_eq!(v["anomaly_rule_applied"], false);
}
#[test]
fn median_is_exact_half_cent_and_keeps_source_samples() {
    let v = run(Source {
        fail_history: false,
        incomplete_prior: false,
    });
    let cat = &v["historical_categories"][0];
    assert_eq!(cat["category_code"], "food");
    assert_eq!(cat["sample_count"], 2);
    assert_eq!(cat["median_cents_twice"], "201");
    assert_eq!(
        cat["samples"][0]["reference"]["source_id"],
        "fictional-window-source"
    );
    assert_eq!(v["historical_categories"].as_array().unwrap().len(), 1);
    assert_eq!(v["unclassified_history_count"], 1);
    assert_eq!(v["historical"]["evidence"].as_array().unwrap().len(), 3);
    assert_eq!(v["all_windows_complete"], true);
    assert_eq!(v["comparison_rule_applied"], false);
    assert_eq!(v["current"]["metadata"]["report_id"], "bundle1:current");
    assert_eq!(
        v["comparable"]["request"]["window"]["start_inclusive"],
        "2026-09-01T00:00:00+08:00"
    );
    assert_eq!(v["refund_boundary"]["read"]["is_complete"], true);
}
#[test]
fn prior_incompleteness_is_separate_from_current_completeness() {
    let v = run(Source {
        fail_history: false,
        incomplete_prior: true,
    });
    assert_eq!(v["current"]["read"]["is_complete"], true);
    assert_eq!(v["comparable"]["read"]["is_complete"], false);
    assert!(v["comparable"]["summary"].is_null());
    assert_eq!(v["all_windows_complete"], false);
    assert_eq!(v["comparison_rule_applied"], false);
}
#[test]
fn mismatched_accounts_cutoffs_and_overlapping_history_are_rejected() {
    let src = Source {
        fail_history: false,
        incomplete_prior: false,
    };
    for field in ["account", "as_of", "overlap"] {
        let mut r = requests();
        match field {
            "account" => r.comparable.account_ids = vec!["other".into()],
            "as_of" => r.comparable.window.as_of = "2026-10-20T00:00:00+08:00".into(),
            _ => r.historical.window.end_exclusive = "2026-10-02T00:00:00+08:00".into(),
        }
        assert!(analyze_windows(
            &scope(),
            &src,
            &r,
            &ReportMetadata {
                report_id: "bundle".into(),
                calculated_at: "2026-10-19T12:00:00+08:00".into()
            },
            Limits::default()
        )
        .is_err());
    }
}

#[test]
fn empty_parent_report_id_cannot_become_a_valid_child_id() {
    assert!(analyze_windows(
        &scope(),
        &Source {
            fail_history: false,
            incomplete_prior: false
        },
        &requests(),
        &ReportMetadata {
            report_id: String::new(),
            calculated_at: "2026-10-19T12:00:00+08:00".into()
        },
        Limits::default()
    )
    .is_err());
}
