//! Existing sealed public fixture only; not a production issuer or lifecycle execution.
use super::*;
use crate::{OperationRecord, PaymentOriginalResult, PaymentOriginalState, PaymentSnapshot};

fn payment(r: &ExecutionRequest, b: &MockTransferRequest) -> (OperationRecord, PaymentSnapshot) {
    let snapshot = PaymentSnapshot {
        transfer: b.clone(),
        display_details: json!({}),
        draft_expires_at: u64::MAX,
    };
    let op = OperationRecord {
        operation_id: OperationId::parse("isolated-preparation-operation").unwrap(),
        task_id: b.task_id.clone(),
        node_id: node("preparation"),
        state: OperationState::Draft,
        revision: 1,
        plan_version: r.binding.snapshot.plan_version,
        facts_version: r.binding.snapshot.facts_version,
        arguments_hash: hash(&snapshot).unwrap(),
        business_operation_ref: None,
    };
    (op, snapshot)
}

async fn recover(
    db: &Db,
    op: &OperationRecord,
    snapshot: &PaymentSnapshot,
) -> PaymentOriginalResult {
    let mut tx = db.sql.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    let task = crate::repository::task_row(
        &sqlx::query("SELECT * FROM tasks WHERE task_id=$1")
            .bind(snapshot.transfer.task_id.as_str())
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
    )
    .unwrap();
    let view = crate::payment_preparation::recover_locked(&mut tx, &task, op, snapshot)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    view
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and existing sealed public fixture"]
async fn payment_execution_readonly_usage_checks_submitted_original_without_row_writes() {
    let db = Db::current44().await;
    let (p, r, b) = setup(&db).await;
    submit(&db, &p, &r, &b, MockBehavior::Immediate, false)
        .await
        .unwrap();
    let before: Json<serde_json::Value> = sqlx::query_scalar("SELECT jsonb_build_object('requests',(SELECT jsonb_agg(to_jsonb(r)) FROM public_execution_requests r),'outbox',(SELECT jsonb_agg(to_jsonb(o)) FROM public_audit_outbox o),'funds',(SELECT jsonb_agg(to_jsonb(f)) FROM transfer_fund_orders_v1 f),'ledger',(SELECT jsonb_agg(to_jsonb(l)) FROM transfer_ledger_v1 l))").fetch_one(&db.sql).await.unwrap();
    let mut tx = db.sql.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    let usage = crate::daily_usage::read_for_mode(&mut tx, &r.binding.snapshot.user_id, false)
        .await
        .unwrap();
    assert_eq!(usage.submitted.len(), 1);
    assert_eq!(usage.submitted[0].operation_id, b.operation_id);
    assert!(usage.submitted[0].submitted_at.is_some());
    tx.commit().await.unwrap();
    let after: Json<serde_json::Value> = sqlx::query_scalar("SELECT jsonb_build_object('requests',(SELECT jsonb_agg(to_jsonb(r)) FROM public_execution_requests r),'outbox',(SELECT jsonb_agg(to_jsonb(o)) FROM public_audit_outbox o),'funds',(SELECT jsonb_agg(to_jsonb(f)) FROM transfer_fund_orders_v1 f),'ledger',(SELECT jsonb_agg(to_jsonb(l)) FROM transfer_ledger_v1 l))").fetch_one(&db.sql).await.unwrap();
    assert_eq!(after, before);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and existing sealed public fixture"]
async fn payment_execution_public_binding_real_result_maps_replays_and_never_settles_from_get() {
    for (behavior, state) in [
        (MockBehavior::Immediate, PaymentOriginalState::Succeeded),
        (MockBehavior::Decline, PaymentOriginalState::Failed),
        (MockBehavior::Pending, PaymentOriginalState::Processing),
        (
            MockBehavior::LostReplyAfterCommit,
            PaymentOriginalState::Succeeded,
        ),
    ] {
        let db = Db::current44().await;
        let (p, r, b) = setup(&db).await;
        let first = submit(&db, &p, &r, &b, behavior, false).await.unwrap();
        let (op, snapshot) = payment(&r, &b);
        let outbox_before = outbox(&db).pending(100).await.unwrap();
        let v = recover(&db, &op, &snapshot).await;
        assert_eq!(v.state, state);
        assert_eq!(v.first_public_submission, Some(first.operation.clone()));
        assert_eq!(v.public_submitted_at, Some(first.recorded_at));
        assert_eq!(v.binding.preparation_operation_id, op.operation_id);
        assert_eq!(
            v.binding.execution_operation_id,
            Some(r.binding.snapshot.operation_id.clone())
        );
        assert_eq!(v.binding.business_operation_id, b.operation_id);
        assert_eq!(v.binding.idempotency_key, Some(r.idempotency_key.clone()));
        assert_eq!(
            v.binding.operation_fingerprint,
            Some(r.binding.fingerprint.clone())
        );
        assert_eq!(v.verified_business_result.as_ref().unwrap().request, b);
        assert_eq!(v, recover(&db, &op, &snapshot).await);
        assert_eq!(
            requests(&db)
                .by_key(&db.session, &r.idempotency_key)
                .await
                .unwrap()
                .current_operation
                .state,
            OperationState::Submitted,
            "reading actual bank outcome must not settle the public operation"
        );
        assert_eq!(outbox(&db).pending(100).await.unwrap(), outbox_before);
        assert_eq!(db.count("PERMIT_CONSUMED").await, 1);
        let old = requests(&db)
            .original_transfer(&db.session, &r.idempotency_key)
            .await
            .unwrap();
        assert_eq!(v.verified_business_result, Some(old.business));
        // Existing explicit reconciliation is still the only generic write command.
        let settled = requests(&db)
            .reconcile_transfer(&db.session, &r.idempotency_key)
            .await
            .unwrap();
        assert_eq!(Some(settled.business), v.verified_business_result);
        assert_eq!(v, recover(&db, &op, &snapshot).await);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and existing public metadata fixture"]
async fn payment_execution_public_reserved_and_submitted_without_business_remain_reconcile_required(
) {
    let db = Db::current44().await;
    let (_p, r, b) = setup(&db).await;
    requests(&db).reserve(&db.session, &r).await.unwrap();
    let (op, snapshot) = payment(&r, &b);
    let reserved = recover(&db, &op, &snapshot).await;
    assert_eq!(reserved.state, PaymentOriginalState::ReconcileRequired);
    assert!(reserved.first_public_submission.is_none());
    // Isolated fault injection: lose the immutable business original after a real
    // submission. The production Transfer wrapper refuses this incomplete state.
    db.finish().await;
    let db2 = Db::current44().await;
    let (permit, request, business) = setup(&db2).await;
    submit(
        &db2,
        &permit,
        &request,
        &business,
        MockBehavior::Pending,
        false,
    )
    .await
    .unwrap();
    db2.sql.execute("ALTER TABLE transfer_fund_orders_v1 DISABLE TRIGGER USER; DELETE FROM transfer_fund_orders_v1; ALTER TABLE transfer_fund_orders_v1 ENABLE TRIGGER USER; ALTER TABLE transfer_business_orders_v1 DISABLE TRIGGER USER; DELETE FROM transfer_business_orders_v1; ALTER TABLE transfer_business_orders_v1 ENABLE TRIGGER USER").await.unwrap();
    let (op, snapshot) = payment(&request, &business);
    let view = recover(&db2, &op, &snapshot).await;
    assert_eq!(view.state, PaymentOriginalState::ReconcileRequired);
    assert_eq!(
        view.first_public_submission.unwrap().state,
        OperationState::Submitted
    );
    assert!(view.verified_business_result.is_none());
    assert_eq!(view.reason, "PUBLIC_ORIGINAL_WITHOUT_BUSINESS_RESULT");
    assert_eq!(db2.count("PERMIT_CONSUMED").await, 1);
    db2.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and complete public projection comparisons"]
async fn payment_execution_public_full_projection_mismatch_and_distinct_operation_rejected() {
    let db = Db::current44().await;
    let (p, r, b) = setup(&db).await;
    submit(&db, &p, &r, &b, MockBehavior::Immediate, false)
        .await
        .unwrap();
    let (op, snapshot) = payment(&r, &b);
    for field in [
        "draft_id",
        "draft_version",
        "payer_account_id",
        "payee_id",
        "amount",
        "remark",
    ] {
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["transfer"][field] = match field {
            "draft_version" => json!(2),
            "amount" => json!("2.01"),
            _ => json!("other-original-value"),
        };
        let changed: PaymentSnapshot = serde_json::from_value(value).unwrap();
        let view = recover(&db, &op, &changed).await;
        assert_eq!(view.state, PaymentOriginalState::SourceConflict, "{field}");
        assert!(view.verified_business_result.is_none());
        assert!(view.binding.execution_operation_id.is_none());
    }
    let mut same = op.clone();
    same.operation_id = r.binding.snapshot.operation_id.clone();
    assert_eq!(
        recover(&db, &same, &snapshot).await.state,
        PaymentOriginalState::SourceConflict
    );
    assert_eq!(
        recover(&db, &op, &snapshot).await.state,
        PaymentOriginalState::Succeeded
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and separate business/runtime versions"]
async fn payment_execution_binding_preserves_business_version_two_runtime_version_one() {
    let db = Db::current44().await;
    let mut business = MockTransferRequest {
        task_id: new_task_id(),
        operation_id: OperationId::parse("business-original-id").unwrap(),
        draft_id: resource("public-contract-draft"),
        draft_version: 2,
        payer_account_id: resource("public-contract-payer"),
        payee_id: resource("public-contract-payee"),
        amount: safeflow_bank_gateway::DecimalAmount::parse("2.00").unwrap(),
        currency: safeflow_bank_gateway::CnyCurrency::Cny,
        remark: "revised".into(),
    };
    let mut v = db
        .fixture_parameters(
            1,
            business.task_id.clone(),
            serde_json::to_value(&business).unwrap(),
            business.draft_id.clone(),
        )
        .await;
    v.execution_scope = VerifiedPermitScope::Transfer;
    let mut draft = super::super::transfer_sql_tests::public_draft(&db, &v).await;
    draft.plan_version = 2;
    draft.details.as_mut().unwrap().remark = business.remark.clone();
    let mut tx = db.sql.begin().await.unwrap();
    FundsStore::save_draft(&mut tx, &draft.user_id, Some(1), &draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    business.task_id = draft.task_id;
    let permit = db.save(&v).await;
    let request = ExecutionRequest {
        idempotency_key: IdempotencyKey::parse("isolated-original-key").unwrap(),
        business_operation_ref: BusinessOperationRef::parse(business.operation_id.as_str())
            .unwrap(),
        business_version: 2,
        binding: permit.binding().clone(),
        payload: v.payload,
    };
    submit(
        &db,
        &permit,
        &request,
        &business,
        MockBehavior::Pending,
        false,
    )
    .await
    .unwrap();
    let (op, snapshot) = payment(&request, &business);
    let result = recover(&db, &op, &snapshot).await;
    assert_eq!(result.state, PaymentOriginalState::Processing);
    assert_eq!(result.binding.business_draft_version, 2);
    assert_eq!(result.binding.execution_plan_version, Some(1));
    assert_eq!(result.binding.preparation_plan_version, 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and original source fault injection"]
async fn payment_execution_conflicting_key_digest_terminal_and_missing_credit_never_become_success()
{
    for (fault, reason) in [
        ("key", "ORIGINAL_KEY_CONFLICT"),
        ("digest", "PUBLIC_ORIGINAL_RECORD_CONFLICT"),
        ("terminal", "PUBLIC_BUSINESS_TERMINAL_CONFLICT"),
        ("credit", "BUSINESS_PROJECTION_OR_LEDGER_CONFLICT"),
    ] {
        let db = Db::current44().await;
        let (p, r, b) = setup(&db).await;
        submit(&db, &p, &r, &b, MockBehavior::Immediate, false)
            .await
            .unwrap();
        // Only this freshly created isolated schema is corrupted; no member sample
        // or production transaction protection is changed.
        match fault {
            "key" => {
                db.sql.execute("ALTER TABLE transfer_business_orders_v1 DISABLE TRIGGER USER; UPDATE transfer_business_orders_v1 SET original_key='other-original-key'; ALTER TABLE transfer_business_orders_v1 ENABLE TRIGGER USER").await.unwrap();
            }
            "digest" => {
                db.sql.execute("ALTER TABLE public_execution_requests DISABLE TRIGGER USER; UPDATE public_execution_requests SET request_digest=repeat('0',64); ALTER TABLE public_execution_requests ENABLE TRIGGER USER").await.unwrap();
            }
            "terminal" => {
                sqlx::query("UPDATE operations SET state='FAILED',revision=revision+1 WHERE operation_id=$1")
                .bind(r.binding.snapshot.operation_id.as_str()).execute(&db.sql).await.unwrap();
            }
            _ => {
                db.sql.execute("ALTER TABLE transfer_ledger_v1 DISABLE TRIGGER USER; DELETE FROM transfer_ledger_v1 WHERE direction='CREDIT'; ALTER TABLE transfer_ledger_v1 ENABLE TRIGGER USER").await.unwrap();
            }
        }
        let (op, snapshot) = payment(&r, &b);
        let view = recover(&db, &op, &snapshot).await;
        assert_eq!(view.state, PaymentOriginalState::SourceConflict, "{fault}");
        assert_eq!(view.reason, reason, "{fault}");
        assert!(!view.funds_execution_enabled);
        assert!(!view.public_sync_enabled);
        assert_eq!(view, recover(&db, &op, &snapshot).await);
        db.finish().await;
    }
}
