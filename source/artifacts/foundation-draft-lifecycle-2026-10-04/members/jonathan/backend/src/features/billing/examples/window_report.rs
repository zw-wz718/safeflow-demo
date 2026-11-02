//! Fixed fictional source; no user, account, credentials or source overrides.
use safeflow_billing_source_reader::{
    query::*, spending::ReportMetadata, window_report::*, TrustedScope,
};
use serde_json::{json, Value};
struct Source {
    fail_history: bool,
}
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        "fictional-window-demo"
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        r: &Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        let history = r.window.start_inclusive.starts_with("2026-07");
        if history && self.fail_history {
            return Err(PageError::Unavailable);
        }
        let amounts = if history {
            vec!["1.00", "1.01"]
        } else {
            vec!["2.00"]
        };
        let items=amounts.iter().enumerate().map(|(i,amount)|serde_json::from_value(json!({
            "transaction_id":format!("fictional_{i}_{}",r.window.start_inclusive),"account_id":"account_demo",
            "amount":amount,"currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION","status":"COMPLETED",
            "completed_at":r.window.start_inclusive,"category_code":"food"})).unwrap()).collect::<Vec<_>>();
        Ok(Page {
            snapshot_id: format!("fictional_{}", r.window.start_inclusive),
            owner_user_id: "user_demo".into(),
            account_ids: r.account_ids.clone(),
            window: r.window.clone(),
            total_unique_rows: Some(items.len()),
            items,
            has_more: false,
            next_cursor: None,
            range_complete: true,
        })
    }
}
fn request(start: &str, end: &str) -> Request {
    serde_json::from_value(json!({"account_ids":["account_demo"],"window":{
        "as_of":"2026-10-19T12:00:00+08:00","start_inclusive":start,"end_exclusive":end,"timezone":"Asia/Shanghai"}})).unwrap()
}
fn demo(fail_history: bool) -> Value {
    let requests = WindowRequests {
        current: request("2026-10-01T00:00:00+08:00", "2026-10-19T12:00:00+08:00"),
        comparable: request("2026-09-01T00:00:00+08:00", "2026-09-19T12:00:00+08:00"),
        historical: request("2026-07-01T00:00:00+08:00", "2026-10-01T00:00:00+08:00"),
        refund_boundary: request("2026-06-01T00:00:00+08:00", "2026-10-19T12:00:00+08:00"),
    };
    let scope = TrustedScope::from_verified_context("user_demo", ["account_demo"]).unwrap();
    serde_json::to_value(
        analyze_windows(
            &scope,
            &Source { fail_history },
            &requests,
            &ReportMetadata {
                report_id: if fail_history {
                    "fictional_partial"
                } else {
                    "fictional_complete"
                }
                .into(),
                calculated_at: "2026-10-19T12:00:00+08:00".into(),
            },
            Limits::default(),
        )
        .unwrap(),
    )
    .unwrap()
}
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"complete":demo(false),"history_unavailable":demo(true)})
        )
        .unwrap()
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runnable_sample_retains_partial_knowledge() {
        let complete = demo(false);
        assert_eq!(complete["all_windows_complete"], true);
        assert_eq!(
            complete["historical_categories"][0]["median_cents_twice"],
            "201"
        );
        let partial = demo(true);
        assert_eq!(partial["current"]["summary"]["gross_consumption"], "2.00");
        assert_eq!(partial["all_windows_complete"], false);
        assert!(partial["historical_categories"].is_null());
    }
}
