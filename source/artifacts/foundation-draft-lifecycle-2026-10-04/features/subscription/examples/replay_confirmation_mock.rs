//! Six fixed fictional cases. No external authorization/confirmation arguments.
use safeflow_subscription_evidence::{confirmation_mock::*, managed_ledger_mock::*, *};
use serde_json::{json, Value};

const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
));

struct FixtureGate {
    expected: MockConfirmationBinding,
    confirmed_in_fixture: bool,
    used: bool,
}
impl MockGateVerifier for FixtureGate {
    fn verify_and_consume_mock_confirmation(
        &mut self,
        binding: &MockConfirmationBinding,
        _now: &str,
    ) -> MockGateOutcome {
        if !self.confirmed_in_fixture
            || self.used
            || binding != &self.expected
            || binding.minimum_level != "YELLOW"
        {
            return MockGateOutcome::Denied;
        }
        self.used = true;
        MockGateOutcome::Confirmed
    }
}

fn empty() -> Result<LedgerFixture, &'static str> {
    load_ledger(&json!({"schema_version":1,"fictional":true,"production_registered":false,
        "owner_user_id":"user_demo","snapshot_id":"mock-ledger-1",
        "read":{"state":"COMPLETE","is_complete":true,"next_cursor":null,"reason":null},"entries":[]}).to_string())
}

fn replay() -> Result<Value, &'static str> {
    let sample = billing_query_mock::load_case(
        INPUT,
        "complete",
        MockContext {
            user_id: "user_demo".into(),
            requested_account_ids: vec!["account_001".into()],
        },
    )?;
    let ctx = &sample.trusted_context_fixture;
    let snapshot = &sample.snapshot;
    let mut cases = vec![];
    for status in [
        LedgerStatus::Active,
        LedgerStatus::Cancelling,
        LedgerStatus::Error,
    ] {
        let mut fixture = empty()?;
        fixture.entries.push(LedgerEntry {
            subscription_id: "mock-existing".into(),
            owner_user_id: ctx.user_id.clone(),
            source_id: Some("fictional-billing-input-v1".into()),
            merchant_id: Some("merchant_demo_video".into()),
            service_ref: Some("service_demo_video_1".into()),
            status,
        });
        let ledger = MemoryConfirmationMock::from_fixture(fixture);
        let result = ledger
            .discover_mock(ctx, snapshot)
            .map_err(|_| "DISCOVERY_FAILED")?;
        let item = &result.groups[0];
        cases.push(json!({"name":format!("existing_{status:?}"),"disposition":item.disposition,"reason":item.reason,
            "review_message":if item.disposition==Disposition::NeedsReview {Some(public_review_message(item.reason))}else{None},
            "preserved_actual_charges":item.group.actual_charges.len(),"registered_count":1,"new_events":0}));
    }
    let mut ledger = MemoryConfirmationMock::from_fixture(empty()?);
    let draft = ledger
        .prepare_mock(
            ctx,
            snapshot,
            MockOperationContext {
                user_id: ctx.user_id.clone(),
                session_ref: "mock-session".into(),
                task_id: "mock-task".into(),
                node_id: "mock-confirm".into(),
                operation_id: "mock-operation".into(),
                idempotency_key: "mock-key".into(),
                plan_version: 1,
            },
            0,
            "2026-10-13T12:05:00+08:00",
        )
        .map_err(|_| "PREPARE_FAILED")?;
    let mut gate = FixtureGate {
        expected: draft.clone(),
        confirmed_in_fixture: false,
        used: false,
    };
    if ledger.commit_mock(
        &draft.draft_id,
        &draft.context,
        "2026-10-13T12:01:00+08:00",
        &mut gate,
    ) != Err(MockIssue::ConfirmationRequired)
    {
        return Err("CONFIRMATION_GATE_FAILED");
    }
    cases.push(
        json!({"name":"unconfirmed","result":"CONFIRMATION_REQUIRED",
        "registered_count":ledger.list_mock(&ctx.user_id).map_err(|_|"LIST_FAILED")?.len()}),
    );
    // Explicitly simulate a user confirming this exact draft inside the harness.
    gate.confirmed_in_fixture = true;
    let receipt = ledger
        .commit_mock(
            &draft.draft_id,
            &draft.context,
            "2026-10-13T12:01:00+08:00",
            &mut gate,
        )
        .map_err(|_| "CONFIRM_FAILED")?;
    let repeated = ledger
        .commit_mock(
            &draft.draft_id,
            &draft.context,
            "2026-10-13T12:02:00+08:00",
            &mut gate,
        )
        .map_err(|_| "REPLAY_FAILED")?;
    let next = ledger
        .discover_mock(ctx, snapshot)
        .map_err(|_| "DISCOVERY_FAILED")?;
    cases.push(json!({"name":"confirmed_and_retried","receipt":receipt,"same_original_receipt":receipt==repeated,
        "registered_count":ledger.list_mock(&ctx.user_id).map_err(|_|"LIST_FAILED")?.len(),
        "event_count":ledger.events_mock(&ctx.user_id).map_err(|_|"LIST_FAILED")?.len(),"next_discovery":next.groups[0].disposition}));
    let mut unknown = empty()?;
    unknown.read.state = ReadKind::Unavailable;
    unknown.read.is_complete = false;
    unknown.read.reason = Some("MOCK_LEDGER_UNAVAILABLE".into());
    let unknown = MemoryConfirmationMock::from_fixture(unknown);
    if !matches!(
        unknown.discover_mock(ctx, snapshot),
        Err(MockIssue::LedgerUnavailable)
    ) {
        return Err("UNKNOWN_LEDGER_GATE_FAILED");
    }
    cases.push(
        json!({"name":"unknown_ledger","result":"LEDGER_UNAVAILABLE","discovery_returned":false}),
    );
    Ok(
        json!({"mode":"OFFLINE_FICTIONAL_LEDGER_CONFIRMATION","fictional":true,"production_registered":false,
        "team_contract_approved":false,"security_kernel_connected":false,"database_connected":false,
        "audit_integrated":false,"reminders_implemented":false,"merchant_action_executed":false,
        "fixture_clock":"2026-10-13T12:01:00+08:00","independent_mock_ledger_source":true,
        "case_count":cases.len(),"cases":cases}),
    )
}

fn main() -> std::process::ExitCode {
    if std::env::args_os().len() != 1 {
        eprintln!("USAGE: replay_confirmation_mock (fixed fictional cases only)");
        return std::process::ExitCode::from(2);
    }
    match replay()
        .and_then(|result| serde_json::to_string_pretty(&result).map_err(|_| "OUTPUT_FAILED"))
    {
        Ok(output) => {
            println!("{output}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(2)
        }
    }
}
