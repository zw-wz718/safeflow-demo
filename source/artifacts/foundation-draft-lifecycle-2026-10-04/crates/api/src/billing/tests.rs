use super::*;
use safeflow_bank_gateway::{BankError, FindPayeesOutput, GetAccountBalanceOutput};
use safeflow_transfer_integration::mock_execution::MockTransferBank;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

const FIXTURE: &str =
    include_str!("../../../../members/jonathan/fixtures/billing/basic_spending.v1.json");
const EXPECTED: &str =
    include_str!("../../../../members/jonathan/fixtures/billing/basic_spending.expected.v1.json");

pub(crate) struct FixtureClock;
impl MockDraftClock for FixtureClock {
    fn now_seconds(&self) -> u64 {
        chrono::DateTime::parse_from_rfc3339("2026-10-16T12:00:00+08:00")
            .unwrap()
            .timestamp() as u64
    }
}
impl safeflow_transfer_integration::Clock for FixtureClock {
    fn now(&self) -> u64 {
        self.now_seconds()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: u32,
    fictional: bool,
    production_registered: bool,
    source_id: String,
    samples: Vec<Sample>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    name: String,
    metadata: ReportMetadata,
    request: Request,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    cursor: Option<String>,
    page: Option<Page>,
    error: Option<PageError>,
}

pub(crate) struct Replay {
    id: String,
    sample: Sample,
    pub(crate) calls: AtomicUsize,
    pub(crate) owners: Mutex<Vec<String>>,
}
impl PagedSource for Replay {
    fn source_id(&self) -> &str {
        &self.id
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        _: &Request,
        cursor: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.owners.lock().unwrap().push(scope.user_id().into());
        // The member's original replay checks DEMO_USER and selects by cursor.
        if scope.user_id() != safeflow_billing_source_reader::DEMO_USER {
            return Err(PageError::ResourceUnavailable);
        }
        match self
            .sample
            .steps
            .iter()
            .find(|step| step.cursor.as_deref() == cursor)
        {
            Some(Step {
                page: Some(page),
                error: None,
                ..
            }) => Ok(page.clone()),
            Some(Step {
                page: None,
                error: Some(error),
                ..
            }) => Err(*error),
            _ => Err(PageError::Unavailable),
        }
    }
}
pub(crate) fn replay(case: &str) -> Arc<Replay> {
    let fixture: Fixture = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(fixture.schema_version, 1);
    assert!(fixture.fictional && !fixture.production_registered);
    let sample = fixture
        .samples
        .into_iter()
        .find(|sample| sample.name == case)
        .unwrap();
    assert_eq!(sample.metadata.calculated_at, "2026-10-16T12:00:00+08:00");
    Arc::new(Replay {
        id: fixture.source_id,
        sample,
        calls: AtomicUsize::new(0),
        owners: Mutex::new(vec![]),
    })
}
pub(crate) fn arguments(case: &str) -> Value {
    let source = replay(case);
    let request = &source.sample.request;
    json!({"input_version":1,"account_ids_json":serde_json::to_string(&request.account_ids).unwrap(),
        "start_inclusive":request.window.start_inclusive,"end_exclusive":request.window.end_exclusive,"timezone":request.window.timezone})
}
pub(crate) fn bank() -> Arc<dyn BankGateway> {
    Arc::new(MockTransferBank::demo(Arc::new(FixtureClock)))
}
pub(crate) fn approved(
    bank: Arc<dyn BankGateway>,
    source: Arc<dyn PagedSource + Send + Sync>,
    revision: &str,
) -> ReadOnlyToolRegistration {
    registration(
        bank,
        Some(LocalBillingSource::for_reviewed_local_bounded_reader(source, revision).unwrap()),
        Arc::new(FixtureClock),
    )
    .unwrap()
}
pub(crate) fn assert_report(case: &str, report: &Value) {
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let expected = &expected["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == case)
        .unwrap()["report"];
    // Report ID is newly server-owned, so compare it separately. No business
    // expectation or original JSON is changed to accommodate the adapter.
    for (field, value) in expected.as_object().unwrap() {
        if field != "metadata" {
            assert_eq!(&report[field], value, "case {case}, field {field}");
        }
    }
    assert_eq!(
        report["metadata"]["calculated_at"],
        expected["metadata"]["calculated_at"]
    );
    assert!(report["metadata"]["report_id"]
        .as_str()
        .unwrap()
        .starts_with("billing_report_"));
    assert_ne!(
        report["metadata"]["report_id"],
        expected["metadata"]["report_id"]
    );
}
fn context(owner: &str) -> ExecutionContext {
    ExecutionContext {
        task_id: TaskId::parse("billing_test_task").unwrap(),
        node_id: NodeId::parse("report").unwrap(),
        operation_id: OperationId::parse("billing_test_operation").unwrap(),
        authenticated_user_id: UserId::parse(owner).unwrap(),
        plan_version: 1,
        idempotency_key: None,
    }
}

#[tokio::test]
async fn original_four_fixtures_run_member_analyze_spending_without_rewriting_business_expectations(
) {
    for case in ["mixed", "empty", "page-failed", "unknown-refund"] {
        let source = replay(case);
        let registration = approved(bank(), source.clone(), &format!("7007ce2:{case}"));
        let result = registration
            .tool
            .execute(
                &context("user_demo"),
                ToolExecutionAuthorization::ReadOnly,
                arguments(case),
            )
            .await
            .unwrap();
        assert_report(case, &result.data["report"]);
        assert_eq!(result.data["production_source_connected"], false);
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            if case == "page-failed" { 2 } else { 1 }
        );
        assert!(source
            .owners
            .lock()
            .unwrap()
            .iter()
            .all(|owner| owner == "user_demo"));
    }
}

#[tokio::test]
async fn each_selected_account_is_checked_and_foreign_owner_never_reaches_source() {
    let source = replay("mixed");
    let registration = approved(bank(), source.clone(), "7007ce2:mixed");
    for (owner, ids) in [
        ("user_demo", r#"["account_001","account_002"]"#),
        ("user_001", r#"["account_001"]"#),
    ] {
        let mut input = arguments("mixed");
        input["account_ids_json"] = json!(ids);
        let error = registration
            .tool
            .execute(&context(owner), ToolExecutionAuthorization::ReadOnly, input)
            .await
            .unwrap_err();
        assert_eq!(error.reason_code.as_str(), "RESOURCE_NOT_OWNED");
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_fields_array_shape_duplicate_accounts_and_invalid_window_are_rejected() {
    let source = replay("mixed");
    let registration = approved(bank(), source.clone(), "7007ce2:mixed");
    for field in [
        "user_id",
        "owner_id",
        "trusted_scope",
        "as_of",
        "report_id",
        "calculated_at",
        "is_complete",
        "page",
        "source_revision",
    ] {
        let mut input = arguments("mixed");
        input[field] = json!("forged");
        assert!(registration
            .tool
            .execute(
                &context("user_demo"),
                ToolExecutionAuthorization::ReadOnly,
                input
            )
            .await
            .is_err());
    }
    for (field, value) in [
        ("account_ids_json", json!([])),
        ("account_ids_json", json!("[]")),
        (
            "account_ids_json",
            json!(r#"["account_001","account_001"]"#),
        ),
        ("account_ids_json", json!("[\"account_001\",\"bad id\"]")),
        ("input_version", json!(2)),
        ("timezone", json!("UTC")),
        ("start_inclusive", json!("2026-11-01T00:00:00+08:00")),
        ("end_exclusive", json!("2026-11-01T00:00:00+08:00")),
    ] {
        let mut input = arguments("mixed");
        input[field] = value;
        assert!(
            registration
                .tool
                .execute(
                    &context("user_demo"),
                    ToolExecutionAuthorization::ReadOnly,
                    input
                )
                .await
                .is_err(),
            "field {field}"
        );
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

struct MismatchedBank(Arc<dyn BankGateway>);
impl BankGateway for MismatchedBank {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            let mut output = self.0.get_account_balance(user, account).await?;
            output.account.account_id = ResourceId::parse("account_002").unwrap();
            Ok(output)
        })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.0.find_payees(user, query)
    }
}
#[tokio::test]
async fn bank_response_account_mismatch_stops_before_member_read() {
    let source = replay("mixed");
    let registration = approved(
        Arc::new(MismatchedBank(bank())),
        source.clone(),
        "7007ce2:mixed",
    );
    let error = registration
        .tool
        .execute(
            &context("user_demo"),
            ToolExecutionAuthorization::ReadOnly,
            arguments("mixed"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.reason_code.as_str(), "INVALID_BANK_RESPONSE");
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn missing_source_and_empty_revision_fail_and_explicit_source_revision_binds_approval() {
    assert!(billing_report_registration_for_backend(bank(), None).is_err());
    assert!(LocalBillingSource::for_reviewed_local_bounded_reader(replay("mixed"), "").is_err());
    let first = approved(bank(), replay("mixed"), "revision-1");
    let same = approved(bank(), replay("mixed"), "revision-1");
    let changed = approved(bank(), replay("mixed"), "revision-2");
    assert_eq!(first.implementation_revision, same.implementation_revision);
    assert_ne!(
        first.implementation_revision,
        changed.implementation_revision
    );
    assert_eq!(first.profile.minimum_level, SecurityLevel::Green);
    assert_eq!(first.approved_action, "READ_LOCAL_BASIC_SPENDING");
}

pub(crate) struct GatedReplay {
    pub(crate) inner: Arc<Replay>,
    pub(crate) entered: tokio::sync::Notify,
    gate: (Mutex<bool>, std::sync::Condvar),
}
impl GatedReplay {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: replay("mixed"),
            entered: tokio::sync::Notify::new(),
            gate: (Mutex::new(false), std::sync::Condvar::new()),
        })
    }
    pub(crate) fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}
impl PagedSource for GatedReplay {
    fn source_id(&self) -> &str {
        self.inner.source_id()
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        self.entered.notify_one();
        let guard = self.gate.0.lock().unwrap();
        // Test-only deterministic barrier with a finite safety bound.
        let (_guard, wait) = self
            .gate
            .1
            .wait_timeout_while(guard, std::time::Duration::from_secs(10), |open| !*open)
            .unwrap();
        if wait.timed_out() {
            return Err(PageError::Unavailable);
        }
        self.inner.read_page(scope, request, cursor, snapshot)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_read_future_does_not_block_executor_or_release_busy_slot_early() {
    let source = GatedReplay::new();
    let reader = LocalBillingSource::for_reviewed_local_bounded_reader(
        source.clone(),
        "gated-original-fixture",
    )
    .unwrap();
    let slots = reader.slots.clone();
    let registration = registration(bank(), Some(reader), Arc::new(FixtureClock)).unwrap();
    let tool = registration.tool.clone();
    let first = tokio::spawn(async move {
        tool.execute(
            &context("user_demo"),
            ToolExecutionAuthorization::ReadOnly,
            arguments("mixed"),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), source.entered.notified())
        .await
        .unwrap();
    assert_eq!(slots.available_permits(), 1);
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(slots.available_permits(), 1);
    // Executor remains live while the local worker is held; cancellation does
    // not claim to abort its synchronous work or publish a result.
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        tokio::task::yield_now(),
    )
    .await
    .unwrap();
    source.release();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while slots.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
