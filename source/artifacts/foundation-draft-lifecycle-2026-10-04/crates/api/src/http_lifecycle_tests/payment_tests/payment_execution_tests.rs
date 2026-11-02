//! Recovery only: isolated member SQL settlement is NOT HTTP payment authority.
use super::*;
use safeflow_contracts::{IdempotencyKey, ResourceId};
use safeflow_transfer_integration::{
    funds_store::FundsStore,
    mock_execution::{MockBehavior, MockOutgoingState, MockTransferRequest},
};

fn result_path(payment: &Value) -> String {
    let path = format!("{}/original-result", path(payment));
    assert_eq!(payment["original_result_path"], path);
    path
}

// Compare all persistent funding/public facts across reads, including confirmation,
// decision and Outbox. Read-only queries must not even invalidate an old session.
pub(super) async fn persistent_state(db: &Db) -> Vec<Value> {
    let mut rows = vec![];
    for table in [
        "tasks",
        "operations",
        "payment_preparations",
        "payment_execution_links",
        "server_confirmations",
        "security_decisions",
        "public_execution_requests",
        "audit_events",
        "public_audit_outbox",
        "transfer_drafts_v1",
        "transfer_accounts_v1",
        "transfer_payee_routes_v1",
        "transfer_business_orders_v1",
        "transfer_fund_orders_v1",
        "transfer_ledger_v1",
    ] {
        rows.push(sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t"))
            .fetch_one(&db.scoped).await.unwrap());
    }
    rows
}

pub(super) async fn submit_isolated(db: &Db, payment: &Value, behavior: MockBehavior) {
    let request: MockTransferRequest =
        serde_json::from_value(payment["snapshot"]["transfer"].clone()).unwrap();
    let owner = UserId::parse("user_001").unwrap();
    let mut tx = db.scoped.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("isolated-recipient").unwrap(),
        &ResourceId::parse("isolated-recipient-account").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::provision_payee(
        &mut tx,
        &owner,
        &request.payee_id,
        &ResourceId::parse("isolated-recipient-account").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::submit(
        &mut tx,
        &owner,
        &IdempotencyKey::parse("isolated-original-key").unwrap(),
        &request,
        behavior,
        MockOutgoingState::Allowed,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_execution_http_owner_selectors_replay_and_no_query_writes() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    decided(&server, &cookie, &payment, "CONFIRM").await;
    let endpoint = result_path(&payment);
    let before = persistent_state(&db).await;
    let result = ok(query(&server, &cookie, &endpoint).await);
    assert_eq!(result["state"], "UNSUBMITTED");
    assert_eq!(
        result["binding"]["preparation_operation_id"],
        payment["operation"]["operation_id"]
    );
    assert_eq!(
        result["binding"]["normalized_arguments"],
        payment["snapshot"]["transfer"]
    );
    assert_eq!(
        result["binding"]["preparation_arguments_hash"],
        payment["snapshot_digest"]
    );
    assert_ne!(
        result["binding"]["transfer_arguments_hash"],
        payment["snapshot_digest"]
    );
    assert!(result["binding"]["execution_operation_id"].is_null());
    assert!(result["binding"]["idempotency_key"].is_null());
    assert!(result["verified_business_result"].is_null());
    assert_eq!(result["funds_execution_enabled"], false);
    assert_eq!(result["public_sync_enabled"], false);
    assert_eq!(ok(query(&server, &cookie, &endpoint).await), result);
    assert_eq!(
        query(
            &server,
            &cookie,
            &format!("{endpoint}?business_operation_id=forged")
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        command(&server, &cookie, &endpoint, json!({})).await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        query(
            &server,
            &cookie,
            &format!("{}/missing/original-result", root(&payment))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        query(
            &server,
            &cookie,
            &format!("{}/original-result", operation_path(&created))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let other = Server::with_draft(
        &db,
        "user_002",
        Duration::from_secs(30),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "original-result-other",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let stranger = other.login().await;
    assert_eq!(
        query(&other, &stranger, &endpoint).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .call(Method::GET, &endpoint, None, None, None, None, false)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(persistent_state(&db).await, before);
    assert_eq!(server.calls(), 1);
    eprintln!("payment original-result HTTP (unsubmitted): {result}");
    other.stop();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and received member SQL recovery"]
async fn payment_execution_http_reads_actual_success_failure_processing_and_lost_reply_without_public_claim(
) {
    // Same received member behaviors as postgres_business_actual_v1.json; no new protocol.
    for (behavior, first, latest) in [
        (MockBehavior::Immediate, "SUCCEEDED", "SUCCEEDED"),
        (MockBehavior::Decline, "FAILED", "FAILED"),
        (MockBehavior::Pending, "PROCESSING", "PROCESSING"),
        (
            MockBehavior::LostReplyAfterCommit,
            "PROCESSING",
            "SUCCEEDED",
        ),
    ] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let payment = prepared(&server, &cookie, &created).await;
        submit_isolated(&db, &payment, behavior).await;
        let before = persistent_state(&db).await;
        let result = ok(query(&server, &cookie, &result_path(&payment)).await);
        assert_eq!(result["state"], "RECONCILE_REQUIRED");
        assert_eq!(result["reason"], "BUSINESS_RESULT_WITHOUT_PUBLIC_ORIGINAL");
        assert_eq!(
            result["verified_business_result"]["request"],
            payment["snapshot"]["transfer"]
        );
        assert_eq!(
            result["verified_business_result"]["first_receipt"]["status"],
            first
        );
        assert_eq!(
            result["verified_business_result"]["latest_receipt"]["status"],
            latest
        );
        assert_eq!(
            result["binding"]["idempotency_key"],
            "isolated-original-key"
        );
        assert!(result["first_public_submission"].is_null());
        assert_eq!(
            ok(query(&server, &cookie, &result_path(&payment)).await),
            result
        );
        assert_eq!(persistent_state(&db).await, before);
        assert_eq!(count(&db, "execution_permits").await, 0);
        let public_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM public_execution_requests")
                .fetch_one(&db.scoped)
                .await
                .unwrap();
        assert_eq!(public_count, 0);
        eprintln!("payment original-result HTTP ({first}/{latest}): {result}");
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_execution_http_submitted_marker_without_original_never_means_no_debit() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    sqlx::query("UPDATE transfer_drafts_v1 SET submitted=true WHERE draft_id=$1")
        .bind(
            payment["snapshot"]["transfer"]["draft_id"]
                .as_str()
                .unwrap(),
        )
        .execute(&db.scoped)
        .await
        .unwrap();
    let before = persistent_state(&db).await;
    let result = ok(query(&server, &cookie, &result_path(&payment)).await);
    assert_eq!(result["state"], "RECONCILE_REQUIRED");
    assert!(result["verified_business_result"].is_null());
    assert_eq!(persistent_state(&db).await, before);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual confirmation expiry"]
async fn payment_execution_http_expired_original_confirmation_still_reads_without_mutation() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(30),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(2),
                Duration::from_secs(300),
                600,
                "original-result-expiry",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    decided(&server, &cookie, &payment, "CONFIRM").await;
    submit_isolated(&db, &payment, MockBehavior::Immediate).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let before = persistent_state(&db).await;
    let result = ok(query(&server, &cookie, &result_path(&payment)).await);
    assert_eq!(
        result["verified_business_result"]["latest_receipt"]["status"],
        "SUCCEEDED"
    );
    assert_eq!(result["funds_execution_enabled"], false);
    assert_eq!(persistent_state(&db).await, before);
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle modification"]
async fn payment_execution_http_complete_original_projection_conflict_and_business_runtime_versions(
) {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let old = prepared(&server, &cookie, &created).await;
    let selected = select(&server, &cookie, &created, "MODIFY_MOCK_DRAFT",
        json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"remark":"original-source-changed"}})).await;
    let modified = run(&server, &cookie, &selected).await.1;
    let current = prepared(&server, &cookie, &modified).await;
    submit_isolated(&db, &current, MockBehavior::Immediate).await;
    let before = persistent_state(&db).await;
    let result = ok(query(&server, &cookie, &result_path(&old)).await);
    assert_eq!(result["state"], "SOURCE_CONFLICT");
    assert!(result["verified_business_result"].is_null());
    let result = ok(query(&server, &cookie, &result_path(&current)).await);
    assert_eq!(
        result["binding"]["business_draft_version"],
        modified["receipt"]["draft_version"]
    );
    assert_eq!(
        result["binding"]["preparation_plan_version"],
        modified["plan_version"]
    );
    assert_eq!(
        result["verified_business_result"]["request"],
        current["snapshot"]["transfer"]
    );
    assert_eq!(persistent_state(&db).await, before);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real restarted HTTP processes"]
async fn payment_execution_http_restart_same_owner_reads_original_result_without_confirmation_authority(
) {
    let db = Db::new().await;
    provision_payer(&db).await;
    let first = ProcessServer::start(&db).await;
    let cookie = first.login().await;
    let confirmed = first.confirmed(&cookie).await;
    let created = ok(first
        .call(
            Method::POST,
            &format!("{}/execute", operation_path(&confirmed)),
            Some(execution(&confirmed)),
            Some(&cookie),
        )
        .await);
    let payment = ok(first
        .call(
            Method::POST,
            &root(&created),
            Some(prepare_input(&created)),
            Some(&cookie),
        )
        .await);
    let payment = ok(first
        .call(
            Method::POST,
            &format!("{}/confirm", path(&payment)),
            Some(decision(&payment, "CONFIRM")),
            Some(&cookie),
        )
        .await);
    submit_isolated(&db, &payment, MockBehavior::LostReplyAfterCommit).await;
    let result = ok(first
        .call(Method::GET, &result_path(&payment), None, Some(&cookie))
        .await);
    first.kill();
    let before = persistent_state(&db).await;
    let second = ProcessServer::start(&db).await;
    assert_eq!(
        second
            .call(Method::GET, &result_path(&payment), None, Some(&cookie))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = second.login().await;
    let recovered = ok(second
        .call(Method::GET, &result_path(&payment), None, Some(&fresh))
        .await);
    assert_eq!(recovered, result);
    assert_eq!(
        recovered["verified_business_result"]["latest_receipt"]["status"],
        "SUCCEEDED"
    );
    assert_eq!(persistent_state(&db).await, before);
    assert_eq!(
        second
            .call(
                Method::POST,
                &format!("{}/confirm", path(&payment)),
                Some(decision(&payment, "CONFIRM")),
                Some(&fresh)
            )
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        funds_state(&db).await["ledger"].as_array().unwrap().len(),
        2
    );
    second.kill();
    db.finish().await;
}
