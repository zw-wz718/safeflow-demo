// Fictional inputs for real PostgreSQL integration tests.
use safeflow_billing_source_reader::{feedback::*, query::*, spending::*, TrustedScope};
use serde_json::json;

pub fn scope(user: &str) -> TrustedScope {
    TrustedScope::from_verified_context(user, ["account_001", "account_002"]).unwrap()
}
struct Source(Page, String);
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        &self.1
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        _: &Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        Ok(self.0.clone())
    }
}
pub fn prepared(
    user: &str,
    op: &str,
    report_id: &str,
    tx: &str,
    choice: FeedbackChoice,
    snapshot: &str,
) -> PreparedFeedback {
    prepared_source(
        user,
        op,
        report_id,
        choice,
        snapshot,
        SourceKey {
            source_id: "fictional-pg".into(),
            account_id: "account_001".into(),
            transaction_id: tx.into(),
        },
        None,
    )
}
// Shared by integration tests; examples only use the original fixture helper.
#[allow(dead_code)]
pub fn prepared_with_reference(
    user: &str,
    op: &str,
    report_id: &str,
    choice: FeedbackChoice,
    snapshot: &str,
    reference: SourceKey,
) -> PreparedFeedback {
    prepared_source(
        user,
        op,
        report_id,
        choice,
        snapshot,
        reference,
        Some("original_purchase_op"),
    )
}
fn prepared_source(
    user: &str,
    op: &str,
    report_id: &str,
    choice: FeedbackChoice,
    snapshot: &str,
    reference: SourceKey,
    original_operation: Option<&str>,
) -> PreparedFeedback {
    let tx = reference.transaction_id.as_str();
    let request: Request = serde_json::from_value(
        json!({"account_ids":["account_001","account_002"],"window":{
        "as_of":"2026-10-19T12:00:00+08:00","start_inclusive":"2026-10-01T00:00:00+08:00",
        "end_exclusive":"2026-10-19T12:00:00+08:00","timezone":"Asia/Shanghai"}}),
    )
    .unwrap();
    let row = serde_json::from_value(
        json!({"transaction_id":tx,"account_id":reference.account_id,"operation_id":original_operation,"amount":"30.00",
        "currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION","status":"COMPLETED",
        "completed_at":"2026-10-10T00:00:00Z","merchant_id":"fictional_merchant",
        "card_id":if tx=="card_tx"{Some("fictional_card")}else{None}}),
    )
    .unwrap();
    let page = Page {
        snapshot_id: snapshot.into(),
        owner_user_id: user.into(),
        account_ids: request.account_ids.clone(),
        window: request.window.clone(),
        total_unique_rows: Some(1),
        items: vec![row],
        has_more: false,
        next_cursor: None,
        range_complete: true,
    };
    let report = analyze_spending(
        &scope(user),
        &request,
        &Source(page, reference.source_id.clone()),
        Limits::default(),
        &ReportMetadata {
            report_id: report_id.into(),
            calculated_at: request.window.as_of.clone(),
        },
    )
    .unwrap();
    prepare_feedback(
        &scope(user),
        &report,
        op,
        &FeedbackIntent {
            report_id: report_id.into(),
            transaction: reference,
            choice,
        },
    )
    .unwrap()
}
