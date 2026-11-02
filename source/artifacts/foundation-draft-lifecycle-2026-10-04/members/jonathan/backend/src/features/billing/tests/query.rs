use safeflow_billing_source_reader::{query::*, TrustedScope};
use serde_json::{json, Value};

fn scope() -> TrustedScope {
    TrustedScope::from_verified_context("user_demo", ["account_001", "account_002"]).unwrap()
}
fn request() -> Request {
    serde_json::from_value(json!({"account_ids":["account_001"], "window":{
        "as_of":"2026-10-13T12:00:00+08:00", "start_inclusive":"2026-06-13T12:00:00+08:00",
        "end_exclusive":"2026-10-13T12:00:00+08:00", "timezone":"Asia/Shanghai"}}))
    .unwrap()
}
fn tx(id: &str) -> Transaction {
    serde_json::from_value(json!({"transaction_id":id,"account_id":"account_001",
        "amount":"20.00","currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION",
        "status":"COMPLETED","completed_at":"2026-07-15T08:00:00+08:00",
        "merchant_id":"merchant_demo","category_code":"DEMO_MEDIA","service_ref":"service_demo"}))
    .unwrap()
}
fn page(items: Vec<Transaction>, next: Option<&str>, total: Option<usize>) -> Page {
    Page {
        snapshot_id: "mock-snapshot-1".into(),
        owner_user_id: "user_demo".into(),
        account_ids: request().account_ids,
        window: request().window,
        items,
        has_more: next.is_some(),
        next_cursor: next.map(str::to_owned),
        range_complete: total.is_some(),
        total_unique_rows: total,
    }
}
struct Source(Vec<Result<Page, PageError>>);
impl PagedSource for Source {
    fn source_id(&self) -> &str {
        "fictional-test-source"
    }
    fn read_page(
        &self,
        _scope: &TrustedScope,
        _request: &Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        let index = match cursor {
            None => 0,
            Some("page2") => 1,
            Some("page3") => 2,
            _ => panic!("unexpected cursor"),
        };
        if index > 0 {
            assert_eq!(snapshot, Some("mock-snapshot-1"));
        }
        self.0[index].clone()
    }
}
fn read(source: Source) -> Report {
    collect(&scope(), &request(), &source, Limits::default())
}

#[test]
fn complete_pages_publish_only_completed_outgoing_consumption() {
    let mut transfer = tx("transfer");
    transfer.transaction_type = TransactionType::Transfer;
    let mut refund = tx("refund");
    refund.transaction_type = TransactionType::Refund;
    refund.direction = Direction::In;
    refund.original_tx_id = Some("one".into());
    let mut pending = tx("pending");
    pending.status = Status::Processing;
    pending.completed_at = None;
    let mut failed = tx("failed");
    failed.status = Status::Failed;
    failed.completed_at = None;
    let result = read(Source(vec![
        Ok(page(vec![tx("one"), transfer], Some("page2"), None)),
        Ok(page(
            vec![refund, pending, failed, tx("two")],
            None,
            Some(6),
        )),
    ]));
    assert_eq!(result.state, ReadState::Complete);
    assert!(result.is_complete);
    assert_eq!(
        result
            .eligible_consumption
            .iter()
            .map(|t| t.transaction_id.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    assert_eq!(result.observed_transactions.len(), 6);
    assert_eq!(result.excluded.len(), 4);
}
#[test]
fn second_page_failure_keeps_partial_rows_but_blocks_evidence() {
    let result = read(Source(vec![
        Ok(page(vec![tx("one")], Some("page2"), None)),
        Err(PageError::Unavailable),
    ]));
    assert_eq!(result.state, ReadState::DataIncomplete);
    assert!(!result.is_complete);
    assert_eq!(result.observed_transactions.len(), 1);
    assert!(result.eligible_consumption.is_empty());
    assert_eq!(result.next_cursor.as_deref(), Some("page2"));
    assert!(subscription_snapshot(&result, &scope(), &request()).is_err());
}
#[test]
fn cursor_cycle_and_snapshot_change_never_claim_complete() {
    for mut second in [
        page(vec![tx("two")], Some("page2"), None),
        page(vec![tx("two")], None, Some(2)),
    ] {
        if second.next_cursor.is_none() {
            second.snapshot_id = "another-snapshot".into();
        }
        let result = read(Source(vec![
            Ok(page(vec![tx("one")], Some("page2"), None)),
            Ok(second),
        ]));
        assert_eq!(result.state, ReadState::DataIncomplete);
        assert!(result.eligible_consumption.is_empty());
    }
}
#[test]
fn empty_terminal_page_requires_source_completion_proof() {
    let result = read(Source(vec![Ok(page(vec![], None, None))]));
    assert_eq!(result.state, ReadState::DataIncomplete);
    assert!(!result.is_complete);
    let valid = read(Source(vec![Ok(page(vec![], None, Some(0)))]));
    assert_eq!(valid.state, ReadState::Complete);
    assert!(valid.is_complete);
}
#[test]
fn repeating_identical_rows_deduplicates_but_equal_timestamps_do_not() {
    let result = read(Source(vec![
        Ok(page(vec![tx("one")], Some("page2"), None)),
        Ok(page(vec![tx("one"), tx("two")], None, Some(2))),
    ]));
    assert_eq!(result.state, ReadState::Complete);
    assert_eq!(result.observed_transactions.len(), 2);
    assert_eq!(result.duplicates_ignored, 1);
}
#[test]
fn conflicting_transaction_id_rejects_the_read() {
    let mut changed = tx("one");
    changed.merchant_id = Some("different".into());
    let result = read(Source(vec![Ok(page(
        vec![tx("one"), changed],
        None,
        Some(1),
    ))]));
    assert_eq!(result.state, ReadState::InvalidInput);
    assert!(result.observed_transactions.is_empty());
}
#[test]
fn foreign_accounts_and_source_owner_fail_without_leaking_rows() {
    let mut foreign = tx("private-foreign-id");
    foreign.account_id = "account_other".into();
    let mut wrong_owner = page(vec![tx("private-id")], None, Some(1));
    wrong_owner.owner_user_id = "other-user".into();
    for input in [page(vec![foreign], None, Some(1)), wrong_owner] {
        let result = read(Source(vec![Ok(input)]));
        assert_eq!(result.state, ReadState::ResourceUnavailable);
        assert!(result.observed_transactions.is_empty());
        assert!(result.snapshot_id.is_none());
        assert!(result.next_cursor.is_none());
    }
    let mut req = request();
    req.account_ids = vec!["account_other".into()];
    let result = collect(&scope(), &req, &Source(vec![]), Limits::default());
    assert_eq!(result.state, ReadState::ResourceUnavailable);
}
#[test]
fn window_is_start_inclusive_end_exclusive_and_compares_instants() {
    let mut start = tx("start");
    start.completed_at = Some("2026-06-13T04:00:00Z".into());
    let mut end = tx("end");
    end.completed_at = Some("2026-10-13T04:00:00Z".into());
    let mut before = tx("before");
    before.completed_at = Some("2026-06-13T03:59:59Z".into());
    let result = read(Source(vec![Ok(page(
        vec![start, end, before],
        None,
        Some(3),
    ))]));
    assert_eq!(
        result
            .eligible_consumption
            .iter()
            .map(|t| t.transaction_id.as_str())
            .collect::<Vec<_>>(),
        vec!["start"]
    );
}
#[test]
fn local_calendar_window_handles_month_end_and_invalid_time() {
    let window = local_four_month_window("2026-06-30T04:00:00Z").unwrap();
    assert_eq!(window.start_inclusive, "2026-02-28T12:00:00+08:00");
    assert_eq!(window.end_exclusive, "2026-06-30T12:00:00+08:00");
    assert!(local_four_month_window("not-time").is_none());
}
#[test]
fn malformed_row_or_false_counts_fail_instead_of_complete() {
    for mut row in [tx("bad-date"), tx("missing-time"), tx("wrong-currency")] {
        match row.transaction_id.as_str() {
            "bad-date" => row.completed_at = Some("not-time".into()),
            "missing-time" => row.completed_at = None,
            _ => row.currency = "USD".into(),
        }
        let result = read(Source(vec![Ok(page(vec![row], None, Some(1)))]));
        assert_eq!(result.state, ReadState::InvalidInput);
        assert!(result.eligible_consumption.is_empty());
    }
    assert_eq!(
        read(Source(vec![Ok(page(vec![tx("one")], None, Some(2)))])).state,
        ReadState::InvalidInput
    );
}
#[test]
fn page_and_row_limits_never_produce_false_completeness() {
    let source = Source(vec![
        Ok(page(vec![tx("one")], Some("page2"), None)),
        Ok(page(vec![tx("two")], None, Some(2))),
    ]);
    for limits in [
        Limits {
            max_pages: 1,
            max_rows: 10,
        },
        Limits {
            max_pages: 10,
            max_rows: 1,
        },
    ] {
        let result = collect(&scope(), &request(), &source, limits);
        assert_eq!(result.state, ReadState::DataIncomplete);
        assert!(result.eligible_consumption.is_empty());
    }
}
#[test]
fn missing_references_are_preserved_and_not_inferred() {
    let mut row = tx("one");
    row.card_id = None;
    row.merchant_id = None;
    row.service_ref = None;
    row.category_code = None;
    let result = read(Source(vec![Ok(page(vec![row], None, Some(1)))]));
    assert_eq!(result.missing_merchant_ids, vec!["one"]);
    assert_eq!(result.missing_service_ids, vec!["one"]);
    assert_eq!(result.eligible_consumption[0].card_id, None);
    assert!(subscription_snapshot(&result, &scope(), &request()).is_err());
    let serialized = serde_json::to_value(&result.eligible_consumption[0]).unwrap();
    assert!(serialized.get("category_code").is_none());
    assert!(serialized.get("original_tx_id").is_none());
}
#[test]
fn subscription_export_preserves_services_and_marks_ledger_unavailable() {
    let mut second = tx("two");
    second.service_ref = Some("other-service".into());
    let result = read(Source(vec![Ok(page(
        vec![tx("one"), second],
        None,
        Some(2),
    ))]));
    let exported = subscription_snapshot(&result, &scope(), &request()).unwrap();
    assert_eq!(
        exported["transactions"][0]["mock_service_ref"],
        "service_demo"
    );
    assert_eq!(
        exported["transactions"][1]["mock_service_ref"],
        "other-service"
    );
    assert_eq!(
        exported["managed_subscriptions_read"]["state"],
        "UNAVAILABLE"
    );
    let value: Value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["production_registered"], json!(false));
}

#[test]
fn consumer_export_revalidates_scope_and_global_transaction_identity() {
    let mut result = read(Source(vec![Ok(page(vec![tx("one")], None, Some(1)))]));
    result.eligible_consumption[0].account_id = "account_other".into();
    assert!(subscription_snapshot(&result, &scope(), &request()).is_err());
    let mut req = request();
    req.account_ids.push("account_002".into());
    let mut second = tx("same-id");
    second.account_id = "account_002".into();
    let mut input = page(vec![tx("same-id"), second], None, Some(2));
    input.account_ids = req.account_ids.clone();
    let result = collect(&scope(), &req, &Source(vec![Ok(input)]), Limits::default());
    assert_eq!(result.state, ReadState::Complete);
    assert_eq!(result.observed_transactions.len(), 2);
    assert!(subscription_snapshot(&result, &scope(), &req).is_err());
}

#[test]
fn malformed_metadata_and_expired_snapshot_fail_closed() {
    let mut input = page(vec![tx("one")], None, Some(1));
    input.has_more = true;
    assert_eq!(read(Source(vec![Ok(input)])).state, ReadState::InvalidInput);
    let result = read(Source(vec![
        Ok(page(vec![tx("one")], Some("page2"), None)),
        Err(PageError::SnapshotExpired),
    ]));
    assert_eq!(result.state, ReadState::DataIncomplete);
    assert_eq!(result.reason.as_deref(), Some("SNAPSHOT_EXPIRED"));
    let result = read(Source(vec![Err(PageError::Unavailable)]));
    assert_eq!(result.state, ReadState::LookupUnavailable);
}

#[test]
fn subscription_export_rejects_rewritten_service_and_merchant_evidence() {
    let original = read(Source(vec![Ok(page(vec![tx("one")], None, Some(1)))]));
    for field in ["merchant", "service", "amount", "identity"] {
        let mut changed = original.clone();
        let row = &mut changed.eligible_consumption[0];
        match field {
            "merchant" => row.merchant_id = Some("invented_merchant".into()),
            "service" => row.service_ref = Some("invented_service".into()),
            "amount" => row.amount = serde_json::from_value(json!("99.99")).unwrap(),
            _ => row.transaction_id = "invented_transaction".into(),
        }
        assert_eq!(
            subscription_snapshot(&changed, &scope(), &request()),
            Err("QUERY_EVIDENCE_CHANGED"),
            "{field}"
        );
    }
}

#[test]
fn subscription_export_rejects_deleted_or_injected_consumption() {
    let original = read(Source(vec![Ok(page(vec![tx("one")], None, Some(1)))]));
    let mut deleted = original.clone();
    deleted.eligible_consumption.clear();
    assert_eq!(
        subscription_snapshot(&deleted, &scope(), &request()),
        Err("QUERY_EVIDENCE_CHANGED")
    );
    let mut injected = original;
    injected.eligible_consumption.push(tx("invented"));
    assert_eq!(
        subscription_snapshot(&injected, &scope(), &request()),
        Err("QUERY_EVIDENCE_CHANGED")
    );
}
