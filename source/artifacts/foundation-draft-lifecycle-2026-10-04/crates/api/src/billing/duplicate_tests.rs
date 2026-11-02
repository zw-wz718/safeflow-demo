use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const ACTIONS: &str =
    include_str!("../../../../members/jonathan/fixtures/billing/report_actions.actual.v1.json");

pub(crate) struct ActionsSource {
    page: Result<Page, PageError>,
    pub calls: AtomicUsize,
}
impl ActionsSource {
    pub(crate) fn new(case: &str) -> Arc<Self> {
        // Exact fictional variations specified by the member's report_actions
        // example; pair-limit uses its existing 46-row boundary test.
        let base: Value = serde_json::from_str(include_str!(
            "../../../../members/jonathan/fixtures/billing/basic_spending.v1.json"
        ))
        .unwrap();
        let mut page: Page =
            serde_json::from_value(base["samples"][0]["steps"][0]["page"].clone()).unwrap();
        page.items.truncate(2);
        page.total_unique_rows = Some(2);
        for row in &mut page.items {
            row.amount = serde_json::from_value(json!("30.00")).unwrap();
            row.merchant_id = (case != "missing-merchant").then(|| "fictional-merchant".into());
        }
        if case == "pair-limit" {
            page.items = (0..46)
                .map(|i| {
                    let mut row = page.items[0].clone();
                    row.transaction_id = format!("tx{i}");
                    row
                })
                .collect();
            page.total_unique_rows = Some(46);
        }
        Arc::new(Self {
            page: if case == "source-unavailable" {
                Err(PageError::Unavailable)
            } else {
                Ok(page)
            },
            calls: AtomicUsize::new(0),
        })
    }
}
impl PagedSource for ActionsSource {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        if scope.user_id() != safeflow_billing_source_reader::DEMO_USER {
            return Err(PageError::ResourceUnavailable);
        }
        self.page.clone()
    }
}
pub(crate) fn assert_candidates(case: &str, data: &Value) {
    assert_eq!(data["adapter_contract"], "LOCAL_BILLING_REPORT_ADAPTER_V2");
    assert_eq!(data["member_revision"], MEMBER);
    assert_eq!(
        data["report"]["capabilities"]["anomalies"]["executed"],
        false
    );
    let expected: Value = serde_json::from_str(ACTIONS).unwrap();
    let expected = &expected["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == case)
        .unwrap()["duplicates"];
    let actual = &data["duplicate_candidates"];
    if let Some(ok) = expected.get("Ok") {
        for (field, value) in ok.as_object().unwrap() {
            if field != "report_id" {
                assert_eq!(&actual["Ok"][field], value, "{case}: {field}");
            }
        }
        // Public operation owns report_id, unlike the standalone member example.
        assert_eq!(
            actual["Ok"]["report_id"],
            data["report"]["metadata"]["report_id"]
        );
        assert_eq!(
            actual["Ok"]["source_id"],
            data["report"]["read"]["source_id"]
        );
        assert_eq!(
            actual["Ok"]["snapshot_id"],
            data["report"]["read"]["snapshot_id"]
        );
    } else {
        assert_eq!(actual, expected);
    }
}
fn context(owner: &str) -> ExecutionContext {
    ExecutionContext {
        task_id: TaskId::parse("duplicate_test_task").unwrap(),
        node_id: NodeId::parse("report").unwrap(),
        operation_id: OperationId::parse("duplicate_test_operation").unwrap(),
        authenticated_user_id: UserId::parse(owner).unwrap(),
        plan_version: 1,
        idempotency_key: None,
    }
}
#[tokio::test]
async fn original_three_actions_cases_share_one_read_and_preserve_member_results() {
    for case in ["candidate", "missing-merchant", "source-unavailable"] {
        let source = ActionsSource::new(case);
        let registration = tests::approved(tests::bank(), source.clone(), "d822709:actions");
        assert!(registration
            .implementation_revision
            .starts_with("local-billing-v2:"));
        let result = registration
            .tool
            .execute(
                &context("user_demo"),
                ToolExecutionAuthorization::ReadOnly,
                tests::arguments("mixed"),
            )
            .await
            .unwrap();
        assert_candidates(case, &result.data);
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn pair_limit_keeps_null_instead_of_a_false_empty_or_truncated_list() {
    let source = ActionsSource::new("pair-limit");
    let registration = tests::approved(tests::bank(), source.clone(), "d822709:pair-limit");
    let result = registration
        .tool
        .execute(
            &context("user_demo"),
            ToolExecutionAuthorization::ReadOnly,
            tests::arguments("mixed"),
        )
        .await
        .unwrap();
    assert_eq!(
        result.data["duplicate_candidates"]["Ok"]["state"],
        "PAIR_LIMIT_EXCEEDED"
    );
    assert!(result.data["duplicate_candidates"]["Ok"]["pairs"].is_null());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn original_candidate_source_never_bypasses_public_account_ownership() {
    let source = ActionsSource::new("candidate");
    let registration = tests::approved(tests::bank(), source.clone(), "d822709:actions");
    let error = registration
        .tool
        .execute(
            &context("user_001"),
            ToolExecutionAuthorization::ReadOnly,
            tests::arguments("mixed"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.reason_code.as_str(), "RESOURCE_NOT_OWNED");
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

// The old serializer is modeled only for durable protocol compatibility tests.
// It is never registered by product startup and adds no business implementation.
struct LegacyV1(Arc<dyn AgentTool>);
impl AgentTool for LegacyV1 {
    fn definition(&self) -> ToolDefinition {
        self.0.definition()
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        auth: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let mut result = self.0.execute(context, auth, arguments).await?;
            let data = result.data.as_object_mut().unwrap();
            data.insert(
                "adapter_contract".into(),
                json!("LOCAL_BILLING_REPORT_ADAPTER_V1"),
            );
            data.remove("duplicate_candidates");
            data.remove("member_revision");
            // Historical V1 had no in-process source-page attachment. Preserve
            // the old wire-only behavior when wrapping today's adapter.
            result.backend_evidence = None;
            Ok(result)
        })
    }
}
pub(crate) fn legacy_v1(mut registration: ReadOnlyToolRegistration) -> ReadOnlyToolRegistration {
    registration.tool = Arc::new(LegacyV1(registration.tool));
    registration.implementation_revision =
        "local-billing-v1:legacy-serializer-protocol-test".into();
    registration
}
