// Real PostgreSQL integration tests; no silent skip when the feature is enabled.
#![cfg(feature = "postgres")]
#[path = "support/feedback.rs"]
mod support;
use safeflow_billing_source_reader::{
    feedback::FeedbackChoice::*, feedback_store::*, TrustedScope,
};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::atomic::{AtomicUsize, Ordering};
use support::*;

static NEXT: AtomicUsize = AtomicUsize::new(0);
async fn database() -> (PgPool, String) {
    let url = std::env::var("BILLING_TEST_DATABASE_URL")
        .expect("use an isolated billing_test_* database; do not skip PG tests");
    let admin = PgPoolOptions::new().connect(&url).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert!(
        name.starts_with("billing_test_"),
        "refuse non-test database"
    );
    let schema = format!(
        "billing_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    );
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .after_connect(move |conn, _| {
            let sql = format!("SET search_path TO {path}");
            Box::pin(async move {
                sqlx::query(&sql).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../sql/feedback-v1.sql"))
        .execute(&pool)
        .await
        .unwrap();
    admin.close().await;
    (pool, schema)
}
async fn cleanup(pool: PgPool, schema: String) {
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}
async fn counts(pool: &PgPool) -> (i64, i64) {
    (
        sqlx::query_scalar("SELECT count(*) FROM billing_feedback_v1")
            .fetch_one(pool)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM billing_feedback_events_v1")
            .fetch_one(pool)
            .await
            .unwrap(),
    )
}
fn value<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap()
}

#[tokio::test]
async fn three_choices_commit_read_and_replay_original_event() {
    let (p, s) = database().await;
    for (i, choice) in [Mine, NotMine, Unsure].into_iter().enumerate() {
        let op = format!("op{i}");
        let item = prepared(
            "user_demo",
            &op,
            "report1",
            &format!("tx{i}"),
            choice,
            "snapshot1",
        );
        let mut tx = p.begin().await.unwrap();
        let staged = stage_feedback(&mut tx, &scope("user_demo"), &item)
            .await
            .unwrap();
        assert_eq!(value(staged)["state"], "STAGED_AWAITING_PUBLIC_COMMIT");
        assert!(load_feedback(&p, &scope("user_demo"), &op)
            .await
            .unwrap()
            .is_none());
        tx.commit().await.unwrap();
        let saved = value(
            load_feedback(&p, &scope("user_demo"), &op)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(saved["persisted"], true);
        assert_eq!(saved["lock_authorized"], false);
        assert_eq!(saved["card_handoff"], "BLOCKED_NO_CARD");
        assert_eq!(saved["evidence"]["transaction"]["amount"], "30.00");
        assert!(saved["event"]["event_id"].as_i64().unwrap() > 0);
        assert!(saved["event"]["recorded_at"]
            .as_str()
            .unwrap()
            .contains('T'));
        let mut tx = p.begin().await.unwrap();
        assert_eq!(
            value(
                stage_feedback(&mut tx, &scope("user_demo"), &item)
                    .await
                    .unwrap()
            )["replayed"],
            true
        );
        tx.commit().await.unwrap();
        assert_eq!(
            saved,
            value(
                load_feedback(&p, &scope("user_demo"), &op)
                    .await
                    .unwrap()
                    .unwrap()
            )
        );
    }
    assert_eq!(counts(&p).await, (3, 3));
    cleanup(p, s).await;
}

#[tokio::test]
async fn conflicts_scope_and_report_identity_never_overwrite() {
    let (p, s) = database().await;
    let item = prepared("user_demo", "op1", "report1", "tx1", Mine, "snapshot1");
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &scope("user_demo"), &item)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for other in [
        prepared("user_demo", "op1", "report1", "tx1", NotMine, "snapshot1"),
        prepared("user_demo", "op1", "report2", "tx1", Mine, "snapshot1"),
        prepared("other", "op1", "report1", "tx1", Mine, "snapshot1"),
    ] {
        let user = if value(&other)["owner_user_id"] == "other" {
            "other"
        } else {
            "user_demo"
        };
        let mut tx = p.begin().await.unwrap();
        assert_eq!(
            stage_feedback(&mut tx, &scope(user), &other)
                .await
                .unwrap_err(),
            StoreError::OperationConflict
        );
        tx.commit().await.unwrap();
    }
    for (other, want) in [
        (
            prepared("user_demo", "op2", "report1", "tx1", Mine, "snapshot1"),
            StoreError::RepeatedFeedbackRuleUnconfirmed,
        ),
        (
            prepared("user_demo", "op2", "report1", "tx2", Mine, "snapshot2"),
            StoreError::ReportConflict,
        ),
    ] {
        let mut tx = p.begin().await.unwrap();
        assert_eq!(
            stage_feedback(&mut tx, &scope("user_demo"), &other)
                .await
                .unwrap_err(),
            want
        );
        tx.commit().await.unwrap();
    }
    let revoked = TrustedScope::from_verified_context("user_demo", ["account_001"]).unwrap();
    let mut tx = p.begin().await.unwrap();
    assert_eq!(
        stage_feedback(&mut tx, &revoked, &item).await.unwrap_err(),
        StoreError::ResourceUnavailable
    );
    tx.rollback().await.unwrap();
    assert!(load_feedback(&p, &revoked, "op1").await.unwrap().is_none());
    assert!(load_feedback(&p, &scope("other"), "op1")
        .await
        .unwrap()
        .is_none());
    assert_eq!(counts(&p).await, (1, 1));
    cleanup(p, s).await;
}

#[tokio::test]
async fn outer_rollback_and_mid_write_failure_leave_no_feedback() {
    let (p, s) = database().await;
    let item = prepared("user_demo", "op1", "report1", "tx1", Mine, "snapshot1");
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &scope("user_demo"), &item)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(counts(&p).await, (0, 0));
    sqlx::query(
        "ALTER TABLE billing_feedback_events_v1 ADD CONSTRAINT forced_failure CHECK (event_id < 0)",
    )
    .execute(&p)
    .await
    .unwrap();
    let mut tx = p.begin().await.unwrap();
    assert_eq!(
        stage_feedback(&mut tx, &scope("user_demo"), &item)
            .await
            .unwrap_err(),
        StoreError::Database
    );
    tx.commit().await.unwrap();
    assert_eq!(counts(&p).await, (0, 0));
    let reports: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_report_bindings_v1")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(reports, 0);
    cleanup(p, s).await;
}

#[tokio::test]
async fn autocommit_is_rejected_and_query_failure_is_not_not_found() {
    let (p, s) = database().await;
    let item = prepared("user_demo", "op1", "report1", "tx1", Mine, "snapshot1");
    let mut conn = p.acquire().await.unwrap();
    assert_eq!(
        stage_feedback(&mut conn, &scope("user_demo"), &item)
            .await
            .unwrap_err(),
        StoreError::TransactionRequired
    );
    drop(conn);
    assert_eq!(counts(&p).await, (0, 0));
    cleanup(p.clone(), s).await;
    assert_eq!(
        load_feedback(&p, &scope("user_demo"), "op1")
            .await
            .unwrap_err(),
        StoreError::Database
    );
}

#[tokio::test]
async fn simultaneous_retries_write_exactly_once() {
    let (p, s) = database().await;
    let mut jobs = Vec::new();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(5));
    for _ in 0..5 {
        let pool = p.clone();
        let barrier = barrier.clone();
        jobs.push(tokio::spawn(async move {
            let item = prepared("user_demo", "op1", "report1", "tx1", Mine, "snapshot1");
            let mut tx = pool.begin().await.unwrap();
            barrier.wait().await;
            let result = stage_feedback(&mut tx, &scope("user_demo"), &item)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            value(result)
        }));
    }
    let mut fresh = 0;
    for job in jobs {
        if job.await.unwrap()["replayed"] == json!(false) {
            fresh += 1;
        }
    }
    assert_eq!(fresh, 1);
    assert_eq!(counts(&p).await, (1, 1));
    cleanup(p, s).await;
}

#[tokio::test]
async fn card_reference_does_not_grant_lock_and_saved_rows_are_immutable() {
    let (p, s) = database().await;
    let item = prepared(
        "user_demo",
        "op1",
        "report1",
        "card_tx",
        NotMine,
        "snapshot1",
    );
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &scope("user_demo"), &item)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let saved = value(
        load_feedback(&p, &scope("user_demo"), "op1")
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(saved["card_handoff"], "BLOCKED_CARD_ASSOCIATION_UNVERIFIED");
    assert_eq!(saved["lock_authorized"], false);
    assert_eq!(
        saved["evidence"]["transaction"]["card_id"],
        "fictional_card"
    );
    assert!(sqlx::query("UPDATE billing_feedback_v1 SET payload='{}'")
        .execute(&p)
        .await
        .is_err());
    assert!(sqlx::query("DELETE FROM billing_feedback_events_v1")
        .execute(&p)
        .await
        .is_err());
    assert_eq!(
        saved,
        value(
            load_feedback(&p, &scope("user_demo"), "op1")
                .await
                .unwrap()
                .unwrap()
        )
    );
    cleanup(p, s).await;
}

#[tokio::test]
async fn oversized_operation_is_rejected_before_any_write() {
    let (p, s) = database().await;
    let item = prepared(
        "user_demo",
        &"x".repeat(201),
        "report1",
        "tx1",
        Mine,
        "snapshot1",
    );
    let mut tx = p.begin().await.unwrap();
    assert_eq!(
        stage_feedback(&mut tx, &scope("user_demo"), &item)
            .await
            .unwrap_err(),
        StoreError::InvalidInput
    );
    tx.commit().await.unwrap();
    assert_eq!(counts(&p).await, (0, 0));
    cleanup(p, s).await;
}

#[tokio::test]
async fn oversized_read_owner_is_rejected_before_encoding_sql_arguments() {
    let (p, s) = database().await;
    let invalid = TrustedScope::from_verified_context(&"x".repeat(201), ["account_001"]).unwrap();
    assert_eq!(
        load_feedback(&p, &invalid, "op1").await.unwrap_err(),
        StoreError::InvalidInput
    );
    cleanup(p, s).await;
}

#[tokio::test]
async fn discarded_reply_can_be_recovered_by_original_id_after_pool_recreation() {
    let (p, s) = database().await;
    let item = prepared(
        "user_demo",
        "lost_reply_op",
        "report1",
        "tx1",
        Unsure,
        "snapshot1",
    );
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &scope("user_demo"), &item)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Deliberately retain no returned feedback result and create no replacement operation.
    p.close().await;
    let path = s.clone();
    let fresh = PgPoolOptions::new()
        .after_connect(move |conn, _| {
            let sql = format!("SET search_path TO {path}");
            Box::pin(async move {
                sqlx::query(&sql).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&std::env::var("BILLING_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    let recovered = value(
        load_feedback(&fresh, &scope("user_demo"), "lost_reply_op")
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(recovered["persisted"], true);
    assert_eq!(
        recovered["evidence"]["public_operation_id"],
        "lost_reply_op"
    );
    assert_eq!(recovered["evidence"]["choice"], "UNSURE");
    assert_eq!(counts(&fresh).await, (1, 1));
    cleanup(fresh, s).await;
}

// Exercise this module's real transaction boundary, not a mock cancellation flag.
// A premature internal COMMIT would make the uncommitted case fail; returning a
// replacement event on retry would make the committed case fail.
async fn cancellation_at_commit_boundary(committed: bool) {
    use std::time::Duration;
    let (p, s) = database().await;
    let worker_pool = p.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let item = prepared(
            "user_demo",
            "cancel_op",
            "report1",
            "tx1",
            NotMine,
            "snapshot1",
        );
        let mut tx = worker_pool.begin().await.unwrap();
        let staged = value(
            stage_feedback(&mut tx, &scope("user_demo"), &item)
                .await
                .unwrap(),
        );
        if committed {
            tx.commit().await.unwrap();
            ready_tx.send(staged).unwrap();
            std::future::pending::<()>().await;
        } else {
            ready_tx.send(staged).unwrap();
            std::future::pending::<()>().await;
            // Kept alive across pending; abort drops the caller-owned transaction.
            tx.rollback().await.unwrap();
        }
    });
    let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx).await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    let staged = ready
        .expect("worker must reach the chosen boundary")
        .unwrap();
    assert_eq!(staged["state"], "STAGED_AWAITING_PUBLIC_COMMIT");
    let original = load_feedback(&p, &scope("user_demo"), "cancel_op")
        .await
        .unwrap();
    assert_eq!(original.is_some(), committed);
    assert_eq!(counts(&p).await, if committed { (1, 1) } else { (0, 0) });
    let reports: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_report_bindings_v1")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(reports, if committed { 1 } else { 0 });
    // Retrying the same operation must finish, proving no cancelled transaction
    // still holds its unique-key locks. Never fabricate a replacement operation.
    let retry = tokio::time::timeout(Duration::from_secs(10), async {
        let item = prepared(
            "user_demo",
            "cancel_op",
            "report1",
            "tx1",
            NotMine,
            "snapshot1",
        );
        let mut tx = p.begin().await.unwrap();
        let result = value(
            stage_feedback(&mut tx, &scope("user_demo"), &item)
                .await
                .unwrap(),
        );
        tx.commit().await.unwrap();
        result
    })
    .await
    .expect("original operation must remain recoverable");
    assert_eq!(retry["replayed"], committed);
    let recovered = value(
        load_feedback(&p, &scope("user_demo"), "cancel_op")
            .await
            .unwrap()
            .unwrap(),
    );
    if let Some(original) = original {
        assert_eq!(value(original), recovered);
        assert_eq!(staged["event"], recovered["event"]);
        assert_eq!(retry["event"], recovered["event"]);
    }
    assert_eq!(recovered["evidence"]["public_operation_id"], "cancel_op");
    assert_eq!(recovered["lock_authorized"], false);
    assert_eq!(counts(&p).await, (1, 1));
    cleanup(p, s).await;
}

#[tokio::test]
async fn task_abort_before_public_commit_leaves_no_feedback_and_allows_original_retry() {
    cancellation_at_commit_boundary(false).await;
}

#[tokio::test]
async fn task_abort_after_public_commit_preserves_original_feedback_and_event() {
    cancellation_at_commit_boundary(true).await;
}

// Breaks if the typed handoff uses the purchase operation, drops the composite
// reference, invents card authority, or exposes raw evidence in audit details.
#[tokio::test]
async fn typed_projection_preserves_saved_facts_and_minimizes_audit_candidate() {
    let (p, s) = database().await;
    for (i, (choice, tx_id, disposition)) in [
        (NotMine, "card_tx", CardDisposition::AssociationUnverified),
        (Mine, "card_tx", CardDisposition::StopMine),
        (Unsure, "card_tx", CardDisposition::StopUnsure),
        (NotMine, "no_card", CardDisposition::StopNoCard),
    ]
    .into_iter()
    .enumerate()
    {
        let owner = format!("user_{i}");
        let op = format!("feedback_{i}");
        let item = prepared_with_reference(
            &owner,
            &op,
            "report1",
            choice.clone(),
            "snapshot1",
            safeflow_billing_source_reader::spending::SourceKey {
                source_id: "fictional-pg".into(),
                account_id: "account_001".into(),
                transaction_id: tx_id.into(),
            },
        );
        let mut tx = p.begin().await.unwrap();
        stage_feedback(&mut tx, &scope(&owner), &item)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let saved = load_feedback(&p, &scope(&owner), &op)
            .await
            .unwrap()
            .unwrap();
        let projection = saved.checked_projection(&scope(&owner)).unwrap();
        assert_eq!(projection.operation_id(), op);
        assert_eq!(projection.owner_user_id(), owner);
        assert_eq!(projection.choice(), &choice);
        assert_eq!(projection.reference().source_id, "fictional-pg");
        assert_eq!(projection.reference().account_id, "account_001");
        assert_eq!(projection.reference().transaction_id, tx_id);
        assert_eq!(
            projection.card_id(),
            if tx_id == "card_tx" {
                Some("fictional_card")
            } else {
                None
            }
        );
        assert_eq!(projection.card_disposition(), disposition);
        assert_eq!(projection.report_id(), "report1");
        assert_eq!(projection.snapshot_id(), "snapshot1");
        assert_eq!(
            projection.report_account_ids(),
            &["account_001", "account_002"]
        );
        assert_eq!(projection.calculated_at(), "2026-10-19T12:00:00+08:00");
        assert!(!projection.rule_version().is_empty());
        let audit = value(projection.audit_candidate());
        assert_eq!(audit["feedback_operation_id"], op);
        assert_eq!(audit["choice"], value(&choice));
        assert_eq!(audit["state"], "SAVED");
        assert_eq!(audit["persisted"], true);
        assert_eq!(audit["lock_authorized"], false);
        assert_eq!(audit.as_object().unwrap().len(), 6);
        assert_eq!(
            audit["local_event"]["namespace"],
            "billing_feedback_events_v1"
        );
        assert_eq!(
            audit["local_event"]["event_id"],
            value(&saved)["event"]["event_id"]
        );
        assert_eq!(
            audit["local_event"]["recorded_at"],
            value(&saved)["event"]["recorded_at"]
        );
        assert_eq!(
            audit["local_event"]["event_kind"],
            "BILLING_FEEDBACK_RECORDED_LOCAL_V1"
        );
        assert_eq!(audit["local_event"].as_object().unwrap().len(), 4);
        let encoded = serde_json::to_string(&audit).unwrap();
        for secret in [
            &owner,
            "account_001",
            "fictional_card",
            "original_purchase_op",
            "30.00",
            "fictional_merchant",
            "snapshot1",
        ] {
            assert!(!encoded.contains(secret), "audit candidate leaked {secret}");
        }
    }
    cleanup(p, s).await;
}

// Breaks if either cached projection or fresh read ignores the full report scope.
#[tokio::test]
async fn typed_projection_rechecks_revoked_and_restored_scope() {
    let (p, s) = database().await;
    let item = prepared(
        "user_demo",
        "typed_op",
        "report1",
        "card_tx",
        NotMine,
        "snapshot1",
    );
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &scope("user_demo"), &item)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let saved = load_feedback(&p, &scope("user_demo"), "typed_op")
        .await
        .unwrap()
        .unwrap();
    let original = value(
        saved
            .checked_projection(&scope("user_demo"))
            .unwrap()
            .audit_candidate(),
    );
    for denied in [
        TrustedScope::from_verified_context("user_demo", ["account_001"]).unwrap(),
        scope("other"),
    ] {
        assert_eq!(
            saved.checked_projection(&denied).unwrap_err(),
            StoreError::ResourceUnavailable
        );
        assert!(load_feedback(&p, &denied, "typed_op")
            .await
            .unwrap()
            .is_none());
    }
    assert!(load_feedback(&p, &scope("user_demo"), "missing")
        .await
        .unwrap()
        .is_none());
    let restored = load_feedback(&p, &scope("user_demo"), "typed_op")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        value(
            restored
                .checked_projection(&scope("user_demo"))
                .unwrap()
                .audit_candidate()
        ),
        original
    );
    cleanup(p, s).await;
}

// Breaks if the adapter conflates a bare transaction ID across source/account.
#[tokio::test]
async fn typed_projection_keeps_same_local_transaction_distinct() {
    use safeflow_billing_source_reader::spending::SourceKey;
    let (p, s) = database().await;
    for (i, (source, account)) in [
        ("source_a", "account_001"),
        ("source_b", "account_001"),
        ("source_a", "account_002"),
    ]
    .into_iter()
    .enumerate()
    {
        let op = format!("op_{i}");
        let item = prepared_with_reference(
            "user_demo",
            &op,
            &format!("report_{i}"),
            NotMine,
            "snapshot1",
            SourceKey {
                source_id: source.into(),
                account_id: account.into(),
                transaction_id: "card_tx".into(),
            },
        );
        let mut tx = p.begin().await.unwrap();
        stage_feedback(&mut tx, &scope("user_demo"), &item)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let saved = load_feedback(&p, &scope("user_demo"), &op)
            .await
            .unwrap()
            .unwrap();
        let projection = saved.checked_projection(&scope("user_demo")).unwrap();
        assert_eq!(projection.operation_id(), op);
        assert_eq!(projection.reference().source_id, source);
        assert_eq!(projection.reference().account_id, account);
        assert_eq!(projection.reference().transaction_id, "card_tx");
    }
    assert_eq!(counts(&p).await, (3, 3));
    cleanup(p, s).await;
}

// Deliberately corrupt only this test's private schema. Matching SQL columns and
// report headers must not make incomplete evidence a usable typed handoff.
#[tokio::test]
async fn typed_projection_rejects_incomplete_evidence_instead_of_inventing_facts() {
    let (p, s) = database().await;
    let item = prepared(
        "user_demo",
        "typed_op",
        "report1",
        "card_tx",
        NotMine,
        "snapshot1",
    );
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &scope("user_demo"), &item)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let saved = load_feedback(&p, &scope("user_demo"), "typed_op")
        .await
        .unwrap()
        .unwrap();
    let original = value(saved)["evidence"].clone();
    sqlx::raw_sql("ALTER TABLE billing_feedback_v1 DISABLE TRIGGER USER; ALTER TABLE billing_report_bindings_v1 DISABLE TRIGGER USER;")
        .execute(&p).await.unwrap();
    for (pointer, replacement) in [
        ("/request/account_ids", json!([])),
        ("/request/account_ids", json!(["account_002"])),
        (
            "/request/account_ids",
            json!(["account_001", "account_001"]),
        ),
        ("/transaction/card_id", json!("")),
        ("/snapshot_id", json!("")),
        ("/rule_version", json!("")),
        ("/calculated_at", json!("not-a-time")),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = replacement;
        let header = json!({"owner_user_id":bad["owner_user_id"],"report_id":bad["report_id"],
            "request":bad["request"],"source_id":bad["reference"]["source_id"],"snapshot_id":bad["snapshot_id"],
            "rule_version":bad["rule_version"],"calculated_at":bad["calculated_at"]});
        sqlx::query("UPDATE billing_report_bindings_v1 SET header=$1")
            .bind(sqlx::types::Json(header))
            .execute(&p)
            .await
            .unwrap();
        sqlx::query("UPDATE billing_feedback_v1 SET payload=$1")
            .bind(sqlx::types::Json(bad))
            .execute(&p)
            .await
            .unwrap();
        let loaded = load_feedback(&p, &scope("user_demo"), "typed_op")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.checked_projection(&scope("user_demo")).unwrap_err(),
            StoreError::CorruptRecord,
            "{pointer}"
        );
    }
    cleanup(p, s).await;
}

// The report may include a valid long non-target account: only the selected
// feedback reference has a <=200-byte SQL column, not all header accounts.
#[tokio::test]
async fn typed_projection_accepts_valid_long_non_target_report_account() {
    use safeflow_billing_source_reader::{feedback::*, query::*, spending::*};
    struct Input(Page);
    impl PagedSource for Input {
        fn source_id(&self) -> &str {
            "fictional-long-account"
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
    let long = "a".repeat(201);
    let owner = TrustedScope::from_verified_context("user_demo", ["account_001", &long]).unwrap();
    let request: Request =
        serde_json::from_value(json!({"account_ids":["account_001",long],"window":{
        "as_of":"2026-10-19T12:00:00+08:00","start_inclusive":"2026-10-01T00:00:00+08:00",
        "end_exclusive":"2026-10-19T12:00:00+08:00","timezone":"Asia/Shanghai"}}))
        .unwrap();
    let row = serde_json::from_value(
        json!({"transaction_id":"tx1","account_id":"account_001","amount":"30.00",
        "currency":"CNY","direction":"OUT","transaction_type":"CONSUMPTION","status":"COMPLETED",
        "completed_at":"2026-10-10T00:00:00Z","merchant_id":null,"card_id":null}),
    )
    .unwrap();
    let report = analyze_spending(
        &owner,
        &request,
        &Input(Page {
            snapshot_id: "snapshot1".into(),
            owner_user_id: "user_demo".into(),
            account_ids: request.account_ids.clone(),
            window: request.window.clone(),
            items: vec![row],
            has_more: false,
            next_cursor: None,
            range_complete: true,
            total_unique_rows: Some(1),
        }),
        Limits::default(),
        &ReportMetadata {
            report_id: "report1".into(),
            calculated_at: request.window.as_of.clone(),
        },
    )
    .unwrap();
    let item = prepare_feedback(
        &owner,
        &report,
        "long_account_op",
        &FeedbackIntent {
            report_id: "report1".into(),
            transaction: SourceKey {
                source_id: "fictional-long-account".into(),
                account_id: "account_001".into(),
                transaction_id: "tx1".into(),
            },
            choice: NotMine,
        },
    )
    .unwrap();
    let (p, s) = database().await;
    let mut tx = p.begin().await.unwrap();
    stage_feedback(&mut tx, &owner, &item).await.unwrap();
    tx.commit().await.unwrap();
    let saved = load_feedback(&p, &owner, "long_account_op")
        .await
        .unwrap()
        .unwrap();
    let projection = saved.checked_projection(&owner).unwrap();
    assert_eq!(projection.operation_id(), "long_account_op");
    assert_eq!(
        projection.report_account_ids(),
        &["account_001".to_owned(), long]
    );
    assert_eq!(projection.reference().account_id, "account_001");
    cleanup(p, s).await;
}
