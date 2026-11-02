use safeflow_subscription_evidence::{
    billing_query_mock,
    confirmation_mock::MockIssue,
    ledger_source_mock::*,
    managed_ledger_mock::{Disposition, LedgerEntry, LedgerFixture, LedgerStatus},
    MockContext, ReadKind, ReadState,
};
use std::cell::RefCell;

struct Source {
    result: Result<LedgerFixture, MockLedgerSourceError>,
    requested_users: RefCell<Vec<String>>,
}
impl MockLedgerSource for Source {
    fn read_owned_mock(&self, user: &str) -> Result<LedgerFixture, MockLedgerSourceError> {
        self.requested_users.borrow_mut().push(user.into());
        self.result.clone()
    }
}
fn source(result: Result<LedgerFixture, MockLedgerSourceError>) -> Source {
    Source {
        result,
        requested_users: RefCell::new(Vec::new()),
    }
}
fn empty() -> LedgerFixture {
    LedgerFixture {
        schema_version: 1,
        fictional: true,
        production_registered: false,
        owner_user_id: "user_demo".into(),
        snapshot_id: Some("fictional-source-snapshot".into()),
        read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        entries: vec![],
    }
}
fn entry(owner: &str) -> LedgerEntry {
    LedgerEntry {
        subscription_id: "fictional-existing-subscription".into(),
        owner_user_id: owner.into(),
        source_id: Some("fictional-source".into()),
        merchant_id: Some("fictional-merchant".into()),
        service_ref: Some("fictional-service".into()),
        status: LedgerStatus::Active,
    }
}
fn issue(source: &Source) -> MockIssue {
    match load_owned_ledger_mock(source, "user_demo") {
        Err(issue) => issue,
        Ok(_) => panic!("invalid source unexpectedly accepted"),
    }
}

#[test]
fn known_empty_source_can_continue_existing_discovery_without_writing() {
    let source = source(Ok(empty()));
    let ledger = load_owned_ledger_mock(&source, "user_demo").unwrap();
    assert_eq!(*source.requested_users.borrow(), vec!["user_demo"]);
    let sample = billing_query_mock::load_case(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
        )),
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )
    .unwrap();
    let review = ledger
        .discover_mock(&sample.trusted_context_fixture, &sample.snapshot)
        .unwrap();
    assert!(review.matching_executed);
    assert_eq!(review.groups.len(), 1);
    assert_eq!(review.groups[0].disposition, Disposition::NotInLedger);
    assert!(ledger.list_mock("user_demo").unwrap().is_empty());
    assert!(ledger.events_mock("user_demo").unwrap().is_empty());
    assert!(ledger.intent_events_mock("user_demo").unwrap().is_empty());
    assert!(source.result.as_ref().unwrap().entries.is_empty());
}

#[test]
fn unavailable_source_is_not_an_empty_ledger_and_is_not_retried_implicitly() {
    let source = source(Err(MockLedgerSourceError::Unavailable));
    assert_eq!(issue(&source), MockIssue::LedgerUnavailable);
    assert_eq!(*source.requested_users.borrow(), vec!["user_demo"]);
}

#[test]
fn incomplete_or_contradictory_read_metadata_never_opens_the_ledger() {
    for variant in 0..6 {
        let mut fixture = empty();
        match variant {
            0 => fixture.read.state = ReadKind::Unavailable,
            1 => fixture.read.state = ReadKind::Incomplete,
            2 => fixture.read.is_complete = false,
            3 => fixture.read.next_cursor = Some("fictional-next-page".into()),
            4 => fixture.read.reason = Some("fictional-read-failure".into()),
            _ => fixture.snapshot_id = None,
        }
        assert_eq!(issue(&source(Ok(fixture))), MockIssue::LedgerUnavailable);
    }
}

#[test]
fn foreign_snapshot_or_late_foreign_row_never_returns_partial_owned_records() {
    let mut foreign = empty();
    foreign.owner_user_id = "other-user".into();
    assert_eq!(issue(&source(Ok(foreign))), MockIssue::ResourceUnavailable);
    let mut mixed = empty();
    mixed.entries = vec![entry("user_demo"), entry("other-user")];
    mixed.entries[1].subscription_id = "other-private-subscription".into();
    assert_eq!(issue(&source(Ok(mixed))), MockIssue::ResourceUnavailable);
}

#[test]
fn real_or_invalid_source_metadata_cannot_bypass_existing_fixture_rules() {
    for variant in 0..4 {
        let mut fixture = empty();
        match variant {
            0 => fixture.fictional = false,
            1 => fixture.production_registered = true,
            2 => fixture.schema_version = 2,
            _ => fixture.entries = vec![entry("user_demo"), entry("user_demo")],
        }
        assert_eq!(issue(&source(Ok(fixture))), MockIssue::InconsistentLedger);
    }
}

#[test]
fn nonempty_snapshot_is_preserved_and_blank_user_never_queries_the_source() {
    let mut fixture = empty();
    fixture.entries.push(entry("user_demo"));
    let source = source(Ok(fixture.clone()));
    for user in ["", "  "] {
        assert!(matches!(
            load_owned_ledger_mock(&source, user),
            Err(MockIssue::ResourceUnavailable)
        ));
    }
    assert!(source.requested_users.borrow().is_empty());
    let ledger = load_owned_ledger_mock(&source, "user_demo").unwrap();
    assert_eq!(ledger.list_mock("user_demo").unwrap(), fixture.entries);
    assert_eq!(source.result.as_ref().unwrap().entries, fixture.entries);
}
