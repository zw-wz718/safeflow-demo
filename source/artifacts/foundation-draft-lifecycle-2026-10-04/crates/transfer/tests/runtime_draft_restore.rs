#[path = "../examples/support/mock_draft_write.rs"]
mod support;

use futures::executor::block_on;
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{MockDraftWriteRequest, RuntimeOutcome, SessionResolver};
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::{DraftRestore, DraftStatus, StoredDraft};
use serde_json::{json, Value};
use std::sync::Arc;
use support::{assemble, context, App, DemoClock};

fn authorized(
    app: &App,
    action: MockDraftAction,
    version: u32,
    arguments: Value,
) -> (MockDraftWriteRequest, MockDraftPermit) {
    let request = app
        .authority
        .request_for_action(
            action,
            SessionRef::parse("mock_session_001").unwrap(),
            context("restart_runtime").task_id,
            NodeId::parse(format!("restart_{version}")).unwrap(),
            version,
            arguments,
            60,
        )
        .unwrap();
    let confirmation = app
        .authority
        .record_explicit_confirmation(&request)
        .unwrap();
    let permit = app.authority.issue(&confirmation).unwrap();
    (request, permit)
}

fn initial(app: &App) -> StoredDraft {
    assert_eq!(
        app.sessions
            .resolve_binding(&SessionRef::parse("mock_session_001").unwrap())
            .unwrap()
            .binding_version,
        1
    );
    block_on(
        app.store
            .find_candidates(&context("restart_runtime"), "小李"),
    )
    .unwrap();
    let (request, permit) = authorized(
        app,
        MockDraftAction::Create,
        1,
        json!({
            "payer_account_id":"account_001", "payee_id":"payee_008", "amount":"200.00", "currency":"CNY"
        }),
    );
    block_on(app.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    block_on(app.store.read_created_for_task(&context("restart_runtime")))
        .unwrap()
        .unwrap()
}

#[test]
fn restored_runtime_requires_new_confirmation_and_keeps_business_ids_and_versions() {
    let old = assemble(
        Arc::new(MockBankGateway::new()),
        DemoClock(ManualClock::new(1000)),
    )
    .unwrap();
    let saved = initial(&old);
    let changes =
        json!({"draft_id":saved.draft_id, "expected_version":1, "changes":{"amount":"300.00"}});
    let (old_request, old_permit) = authorized(&old, MockDraftAction::Modify, 2, changes.clone());
    drop(old);
    let new = assemble(
        Arc::new(MockBankGateway::new()),
        DemoClock(ManualClock::new(1010)),
    )
    .unwrap();
    block_on(new.store.restore_for_backend(
        &context("restart_runtime"),
        DraftRestore {
            draft: saved.clone(),
            expected_version: 1,
            submitted: false,
            payee_query: "小李".into(),
        },
    ))
    .unwrap();
    assert_eq!(new.store.saved_receipt_count().unwrap(), 0);
    assert!(new.audit.events().unwrap().is_empty());
    assert!(block_on(
        new.assembly
            .runtime()
            .execute_mock_draft(old_request, &old_permit)
    )
    .is_err());
    assert_eq!(
        block_on(new.store.read(&context("restart_runtime"), &saved.draft_id)).unwrap(),
        saved
    );
    let (request, permit) = authorized(&new, MockDraftAction::Modify, 2, changes);
    assert!(matches!(
        block_on(new.assembly.runtime().execute_mock_draft(request, &permit)).unwrap(),
        RuntimeOutcome::Completed { .. }
    ));
    let modified = block_on(new.store.read(&context("restart_runtime"), &saved.draft_id)).unwrap();
    assert_eq!(modified.plan_version, 2);
    assert_eq!(modified.details.as_ref().unwrap().amount.as_str(), "300.00");
    let (request, permit) = authorized(
        &new,
        MockDraftAction::Cancel,
        3,
        json!({"draft_id":saved.draft_id,"expected_version":2}),
    );
    block_on(
        new.assembly
            .runtime()
            .execute_mock_draft(request.clone(), &permit),
    )
    .unwrap();
    block_on(new.assembly.runtime().execute_mock_draft(request, &permit)).unwrap();
    let cancelled = block_on(new.store.read(&context("restart_runtime"), &saved.draft_id)).unwrap();
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    assert_eq!(cancelled.operation_id, saved.operation_id);
    assert_eq!(cancelled.expires_at, saved.expires_at);
    assert_eq!(cancelled.plan_version, 2);
    assert_eq!(new.store.saved_receipt_count().unwrap(), 2);
}
