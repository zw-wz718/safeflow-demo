use safeflow_billing_source_reader::{query::*, spending::*, TrustedScope};
use serde_json::json;
use std::cell::Cell;

fn scope() -> TrustedScope {
    TrustedScope::from_verified_context("user_demo", ["account_001", "account_002"]).unwrap()
}
fn request() -> Request {
    serde_json::from_value(
        json!({"account_ids":["account_001","account_002"],"window":{
        "as_of":"2026-10-16T12:00:00+08:00","start_inclusive":"2026-10-01T00:00:00+08:00",
        "end_exclusive":"2026-10-16T12:00:00+08:00","timezone":"Asia/Shanghai"}}),
    )
    .unwrap()
}
fn metadata() -> ReportMetadata {
    ReportMetadata {
        report_id: "fictional-report-1".into(),
        calculated_at: request().window.as_of,
    }
}
fn tx(id: &str, amount: &str, category: Option<&str>) -> Transaction {
    serde_json::from_value(json!({"transaction_id":id,"account_id":"account_001",
        "amount":amount,"currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION",
        "status":"COMPLETED","completed_at":"2026-10-10T08:00:00+08:00",
        "category_code":category,"merchant_id":null,"card_id":null}))
    .unwrap()
}
fn page(rows: Vec<Transaction>) -> Page {
    Page {
        snapshot_id: "fictional-snapshot".into(),
        owner_user_id: "user_demo".into(),
        account_ids: request().account_ids,
        window: request().window,
        total_unique_rows: Some(rows.len()),
        items: rows,
        has_more: false,
        next_cursor: None,
        range_complete: true,
    }
}
struct Source {
    pages: Vec<Result<Page, PageError>>,
    calls: Cell<usize>,
}
impl Source {
    fn new(pages: Vec<Result<Page, PageError>>) -> Self {
        Self {
            pages,
            calls: Cell::new(0),
        }
    }
}
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        "fictional-spending-source"
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        _: &Request,
        cursor: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        self.calls.set(self.calls.get() + 1);
        self.pages[if cursor.is_some() { 1 } else { 0 }].clone()
    }
}
fn analyze_rows(rows: Vec<Transaction>) -> SpendingReport {
    analyze_spending(
        &scope(),
        &request(),
        &Source::new(vec![Ok(page(rows))]),
        Limits::default(),
        &metadata(),
    )
    .unwrap()
}

#[test]
fn mixed_rows_have_gross_total_and_unclassified_bucket() {
    let mut transfer = tx("transfer", "200.00", None);
    transfer.transaction_type = TransactionType::Transfer;
    let mut pending = tx("pending", "50.00", None);
    pending.status = Status::Processing;
    pending.completed_at = None;
    let mut failed = tx("failed", "40.00", None);
    failed.status = Status::Failed;
    failed.completed_at = None;
    let mut refund = tx("refund", "5.00", None);
    refund.transaction_type = TransactionType::Refund;
    refund.direction = Direction::In;
    refund.original_tx_id = Some("food1".into());
    let report = analyze_rows(vec![
        tx("food1", "30.00", Some("DEMO_FOOD")),
        tx("food2", "20.00", Some("DEMO_FOOD")),
        tx("unknown", "10.00", None),
        transfer,
        pending,
        failed,
        refund,
    ]);
    let s = report.summary.as_ref().unwrap();
    assert_eq!((s.count, s.gross_consumption.as_str()), (3, "60.00"));
    assert_eq!(
        s.categories
            .iter()
            .find(|c| c.category_code.is_none())
            .unwrap()
            .total,
        "10.00"
    );
    assert_eq!(
        s.categories
            .iter()
            .find(|c| c.category_code.as_deref() == Some("DEMO_FOOD"))
            .unwrap()
            .total,
        "50.00"
    );
    assert_eq!(
        s.top_five
            .iter()
            .map(|e| e.transaction.amount.as_str())
            .collect::<Vec<_>>(),
        vec!["30.00", "20.00", "10.00"]
    );
    assert_eq!(
        report.refunds.as_ref().unwrap()[0]
            .transaction
            .amount
            .as_str(),
        "5.00"
    );
    assert_eq!(report.evidence.as_ref().unwrap().len(), 3);
    assert_eq!(report.exclusions.as_ref().unwrap().len(), 4);
    for e in report.evidence.as_ref().unwrap() {
        assert_eq!(e.reference.source_id, "fictional-spending-source");
        assert_eq!(e.reference.account_id, e.transaction.account_id);
        assert_eq!(e.reference.transaction_id, e.transaction.transaction_id);
    }
    let output = serde_json::to_value(&report).unwrap();
    for name in ["net_consumption", "comparison", "anomalies"] {
        assert_eq!(output["capabilities"][name]["executed"], false);
    }
}
#[test]
fn decimal_sum_is_exact() {
    assert_eq!(
        analyze_rows(vec![tx("a", "0.10", None), tx("b", "0.20", None)])
            .summary
            .unwrap()
            .gross_consumption,
        "0.30"
    );
}
#[test]
fn complete_empty_is_zero() {
    let r = analyze_rows(vec![]);
    let s = r.summary.unwrap();
    assert_eq!((s.count, s.gross_consumption.as_str()), (0, "0.00"));
    assert!(s.categories.is_empty());
    assert!(s.top_five.is_empty());
    assert!(r.evidence.unwrap().is_empty());
    assert!(r.refunds.unwrap().is_empty());
}
#[test]
fn unclassified_code_does_not_collide_with_none() {
    let r = analyze_rows(vec![tx("a", "1.00", None), tx("b", "2.00", Some("未分类"))]);
    let s = r.summary.unwrap();
    assert_eq!(s.categories.len(), 2);
    assert_eq!(
        s.categories
            .iter()
            .find(|c| c.category_code.is_none())
            .unwrap()
            .total,
        "1.00"
    );
    assert_eq!(
        s.categories
            .iter()
            .find(|c| c.category_code.is_some())
            .unwrap()
            .total,
        "2.00"
    );
}
#[test]
fn sum_overflow_is_not_partial_success() {
    let r = analyze_rows(vec![
        tx("a", "184467440737095516.15", None),
        tx("b", "0.01", None),
    ]);
    assert_eq!(r.read.state, ReadState::Complete);
    assert!(r.summary.is_none());
    assert_eq!(
        r.calculation_status,
        CalculationStatus::CalculationUnavailable
    );
    assert_eq!(r.calculation_reason.as_deref(), Some("AMOUNT_OVERFLOW"));
}

fn run_source(source: &Source, req: &Request) -> SpendingReport {
    analyze_spending(&scope(), req, source, Limits::default(), &metadata()).unwrap()
}
fn assert_blocked(r: &SpendingReport) {
    assert_eq!(r.calculation_status, CalculationStatus::ReadBlocked);
    assert!(r.summary.is_none());
    assert!(r.evidence.is_none());
    assert!(r.refunds.is_none());
    assert!(r.exclusions.is_none());
}
#[test]
fn incomplete_and_unavailable_never_return_zero() {
    let mut first = page(vec![tx("a", "1.00", None)]);
    first.has_more = true;
    first.next_cursor = Some("page2".into());
    first.range_complete = false;
    first.total_unique_rows = None;
    let mut loop_page = first.clone();
    loop_page.items.clear();
    let mut changed = page(vec![tx("b", "2.00", None)]);
    changed.snapshot_id = "changed".into();
    let mut no_proof = page(vec![]);
    no_proof.range_complete = false;
    no_proof.total_unique_rows = None;
    for (pages, state, reason) in [
        (
            vec![Ok(first.clone()), Err(PageError::Unavailable)],
            ReadState::DataIncomplete,
            "SOURCE_UNAVAILABLE",
        ),
        (
            vec![Ok(first.clone()), Ok(loop_page)],
            ReadState::DataIncomplete,
            "CURSOR_LOOP",
        ),
        (
            vec![Ok(first), Ok(changed)],
            ReadState::DataIncomplete,
            "SNAPSHOT_CHANGED",
        ),
        (
            vec![Ok(no_proof)],
            ReadState::DataIncomplete,
            "MISSING_COMPLETION_PROOF",
        ),
        (
            vec![Err(PageError::Unavailable)],
            ReadState::LookupUnavailable,
            "SOURCE_UNAVAILABLE",
        ),
    ] {
        let r = run_source(&Source::new(pages), &request());
        assert_blocked(&r);
        assert_eq!(r.read.state, state);
        assert_eq!(r.read.reason.as_deref(), Some(reason));
    }
}
#[test]
fn scope_denial_never_leaks_source_rows() {
    let mut row = tx("private-foreign-id", "12.00", None);
    row.account_id = "foreign".into();
    let source = Source::new(vec![Ok(page(vec![row]))]);
    for mut req in [request(), request()] {
        if source.calls.get() > 0 {
            req.account_ids = vec!["foreign".into()];
        }
        let r = run_source(&source, &req);
        assert_blocked(&r);
        assert_eq!(r.read.state, ReadState::ResourceUnavailable);
        assert!(!serde_json::to_string(&r)
            .unwrap()
            .contains("private-foreign-id"));
    }
}
fn top_ids(report: SpendingReport) -> Vec<String> {
    report
        .summary
        .unwrap()
        .top_five
        .into_iter()
        .map(|e| e.reference.transaction_id)
        .collect()
}
#[test]
fn top_five_order_is_stable() {
    let mut rows: Vec<_> = [
        ("a", "30.00"),
        ("b", "30.00"),
        ("c", "40.00"),
        ("d", "30.00"),
        ("e", "20.00"),
        ("f", "10.00"),
        ("g", "5.00"),
    ]
    .into_iter()
    .map(|(id, amount)| tx(id, amount, None))
    .collect();
    rows[1].completed_at = Some("2026-10-11T00:00:00Z".into());
    assert_eq!(
        top_ids(analyze_rows(rows.clone())),
        vec!["c", "b", "a", "d", "e"]
    );
    rows.reverse();
    assert_eq!(top_ids(analyze_rows(rows)), vec!["c", "b", "a", "d", "e"]);
}
#[test]
fn timestamps_compare_instants_and_respect_half_open_window() {
    let mut start = tx("start", "1.00", None);
    start.completed_at = Some("2026-09-30T16:00:00Z".into());
    let mut end = tx("end", "2.00", None);
    end.completed_at = Some("2026-10-16T04:00:00Z".into());
    let mut before = tx("before", "4.00", None);
    before.completed_at = Some("2026-09-30T15:59:59Z".into());
    let mut a = tx("a", "3.00", None);
    a.completed_at = Some("2026-10-10T00:00:00Z".into());
    let mut b = tx("b", "3.00", None);
    b.completed_at = Some("2026-10-10T08:00:00+08:00".into());
    let r = analyze_rows(vec![b, a, end, before, start]);
    assert_eq!(r.summary.as_ref().unwrap().gross_consumption, "7.00");
    assert_eq!(top_ids(r), vec!["a", "b", "start"]);
}
#[test]
fn same_second_and_cross_account_ids_survive() {
    let a = tx("same", "1.00", None);
    let mut b = a.clone();
    b.account_id = "account_002".into();
    let c = tx("other", "2.00", None);
    let r = analyze_rows(vec![a.clone(), b, c, a.clone()]);
    // Exact duplicates need terminal proof for unique rows, not raw rows.
    assert_eq!(r.read.reason.as_deref(), Some("COMPLETION_COUNT_MISMATCH"));
    let mut p = page(vec![
        a.clone(),
        {
            let mut x = a.clone();
            x.account_id = "account_002".into();
            x
        },
        tx("other", "2.00", None),
        a,
    ]);
    p.total_unique_rows = Some(3);
    let r = run_source(&Source::new(vec![Ok(p)]), &request());
    assert_eq!(r.summary.as_ref().unwrap().gross_consumption, "4.00");
    assert_eq!(r.summary.as_ref().unwrap().count, 3);
    assert_eq!(r.read.duplicates_ignored, 1);
    let mut wrong = tx("same", "9.00", None);
    wrong.account_id = "account_001".into();
    let r = analyze_rows(vec![tx("same", "1.00", None), wrong]);
    assert_blocked(&r);
    assert_eq!(r.read.reason.as_deref(), Some("CONFLICTING_TRANSACTION_ID"));
}
#[test]
fn duplicate_pages_do_not_double_count() {
    let row = tx("same", "1.00", None);
    let mut first = page(vec![row.clone()]);
    first.has_more = true;
    first.next_cursor = Some("page2".into());
    first.range_complete = false;
    first.total_unique_rows = None;
    let r = run_source(
        &Source::new(vec![Ok(first), Ok(page(vec![row]))]),
        &request(),
    );
    assert_eq!(r.summary.unwrap().gross_consumption, "1.00");
    assert_eq!(r.read.duplicates_ignored, 1);
}
#[test]
fn refund_references_are_not_guessed() {
    let refund = |id: &str| {
        let mut r = tx(id, "5.00", None);
        r.transaction_type = TransactionType::Refund;
        r.direction = Direction::In;
        r
    };
    let missing = refund("missing");
    let mut outside_original = refund("old-original");
    outside_original.original_tx_id = Some("old-transaction".into());
    let mut foreign_original = refund("cross-account");
    foreign_original.original_tx_id = Some("same".into());
    let mut consumption = tx("same", "30.00", None);
    consumption.account_id = "account_002".into();
    let mut pending = refund("pending");
    pending.status = Status::Processing;
    pending.completed_at = None;
    let mut outgoing = refund("outgoing");
    outgoing.direction = Direction::Out;
    let mut outside = refund("outside");
    outside.completed_at = Some(request().window.end_exclusive);
    let r = analyze_rows(vec![
        missing,
        outside_original,
        foreign_original,
        consumption,
        pending,
        outgoing,
        outside,
    ]);
    assert_eq!(
        r.refunds
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| e.reference.transaction_id.as_str())
            .collect::<Vec<_>>(),
        vec!["missing", "old-original", "cross-account"]
    );
    assert!(r.refunds.as_ref().unwrap()[0]
        .transaction
        .original_tx_id
        .is_none());
    assert_eq!(
        r.refunds.as_ref().unwrap()[1]
            .transaction
            .original_tx_id
            .as_deref(),
        Some("old-transaction")
    );
    assert_eq!(r.summary.unwrap().gross_consumption, "30.00");
    assert!(!r.capabilities.net_consumption.executed);
}
#[test]
fn metadata_rejected_without_source_access() {
    let source = Source::new(vec![Ok(page(vec![]))]);
    for (id, at) in [
        ("", metadata().calculated_at),
        ("bad\nreference", metadata().calculated_at),
        ("report", "garbage".into()),
        ("report", "2026-10-16T03:59:59Z".into()),
    ] {
        let m = ReportMetadata {
            report_id: id.into(),
            calculated_at: at,
        };
        assert_eq!(
            analyze_spending(&scope(), &request(), &source, Limits::default(), &m).unwrap_err(),
            SpendingError::InvalidMetadata
        );
    }
    assert_eq!(source.calls.get(), 0);
}
#[test]
fn invalid_request_and_currency_remain_source_errors() {
    let source = Source::new(vec![Ok(page(vec![]))]);
    let mut req = request();
    req.window.as_of = "bad".into();
    let r = run_source(&source, &req);
    assert_blocked(&r);
    assert_eq!(r.read.state, ReadState::InvalidInput);
    for bad in ["currency", "time"] {
        let mut row = tx("a", "1.00", None);
        if bad == "currency" {
            row.currency = "USD".into();
        } else {
            row.completed_at = Some("bad".into());
        }
        let r = analyze_rows(vec![row]);
        assert_blocked(&r);
        assert_eq!(r.read.reason.as_deref(), Some("INVALID_TRANSACTION"));
    }
}
