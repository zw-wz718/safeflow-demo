//! Fictional local replay. No CLI identity input, persistence or lock action.
use safeflow_billing_source_reader::{
    duplicates::*, feedback::*, query::*, spending::*, TrustedScope, DEMO_USER,
};
use serde_json::{json, Value};
struct FictionalSource(Result<Page, PageError>);
impl PagedSource for FictionalSource {
    fn source_id(&self) -> &str {
        "fictional-report-actions"
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        _: &Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        if scope.user_id() != DEMO_USER {
            return Err(PageError::ResourceUnavailable);
        }
        self.0.clone()
    }
}
fn replay() -> Value {
    // Existing input stays unchanged on disk. These deliberate fictional
    // variations are test-only, not a backfill of missing upstream merchant IDs.
    let base: Value = serde_json::from_str(include_str!(
        "../../../../../fixtures/billing/basic_spending.v1.json"
    ))
    .unwrap();
    let sample = &base["samples"][0];
    let req: Request = serde_json::from_value(sample["request"].clone()).unwrap();
    let scope = TrustedScope::from_verified_context(DEMO_USER, ["account_001"]).unwrap();
    let mut cases = vec![];
    for name in ["candidate", "missing-merchant", "source-unavailable"] {
        let mut page: Page = serde_json::from_value(sample["steps"][0]["page"].clone()).unwrap();
        page.items.truncate(2);
        page.total_unique_rows = Some(2);
        for row in &mut page.items {
            row.amount = serde_json::from_value(json!("30.00")).unwrap();
            row.merchant_id = if name == "missing-merchant" {
                None
            } else {
                Some("fictional-merchant".into())
            };
        }
        let source = FictionalSource(if name == "source-unavailable" {
            Err(PageError::Unavailable)
        } else {
            Ok(page)
        });
        let metadata = ReportMetadata {
            report_id: format!("fictional-actions-{name}"),
            calculated_at: req.window.as_of.clone(),
        };
        let report = analyze_spending(&scope, &req, &source, Limits::default(), &metadata).unwrap();
        let intent = FeedbackIntent {
            report_id: metadata.report_id,
            transaction: SourceKey {
                source_id: source.source_id().into(),
                account_id: "account_001".into(),
                transaction_id: "food-1".into(),
            },
            choice: FeedbackChoice::NotMine,
        };
        let prepared = prepare_feedback(&scope, &report, "fictional-public-operation", &intent);
        let mut changed = intent.clone();
        changed.choice = FeedbackChoice::Mine;
        let changed = prepare_feedback(&scope, &report, "fictional-public-operation", &changed);
        let retry = prepare_feedback(&scope, &report, "fictional-public-operation", &intent);
        cases.push(json!({"name":name,"duplicates":detect_duplicates(&scope,&report),"feedback":prepared,
            "same_operation_retry":prepared.as_ref().ok().zip(retry.as_ref().ok()).map(|(a,b)|a.compare_operation(b)),
            "same_operation_changed_choice":prepared.as_ref().ok().zip(changed.as_ref().ok()).map(|(a,b)|a.compare_operation(b))}));
    }
    json!({"fictional":true,"production_registered":false,"persistence_tested":false,
        "mode":"OFFLINE_CANDIDATE_AND_FEEDBACK_PREPARATION","cases":cases})
}
fn main() {
    println!("{}", serde_json::to_string_pretty(&replay()).unwrap());
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_generates_candidates_and_unsaved_feedback() {
        let output = replay();
        assert_eq!(output["fictional"], true);
        assert_eq!(
            output["cases"][0]["duplicates"]["Ok"]["pairs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            output["cases"][0]["feedback"]["Ok"]["state"],
            "PREPARED_NOT_SAVED"
        );
        assert_eq!(
            output["cases"][0]["feedback"]["Ok"]["lock_authorized"],
            false
        );
        assert_eq!(
            output["cases"][0]["same_operation_retry"],
            "IDENTICAL_RETRY"
        );
        assert_eq!(
            output["cases"][0]["same_operation_changed_choice"],
            "CONFLICT"
        );
    }
    #[test]
    fn unavailable_and_missing_evidence_do_not_become_successful_checks() {
        let output = replay();
        assert_eq!(
            output["cases"][1]["duplicates"]["Ok"]["state"],
            "MERCHANT_EVIDENCE_INCOMPLETE"
        );
        assert_eq!(
            output["cases"][2]["duplicates"]["Err"],
            "REPORT_NOT_COMPLETE"
        );
        assert_eq!(output["cases"][2]["feedback"]["Err"], "REPORT_NOT_COMPLETE");
    }
}
