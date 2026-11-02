//! Fictional evidence in an isolated real PostgreSQL schema; no silent skips.
#![cfg(feature = "postgres")]
#[path = "support/feedback.rs"]
mod support;
use safeflow_billing_source_reader::{
    feedback::FeedbackChoice::*, feedback_store::*, TrustedScope,
};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, types::Json, PgPool};
use std::sync::atomic::{AtomicUsize, Ordering};
use support::*;

static NEXT: AtomicUsize = AtomicUsize::new(0);
async fn database() -> (PgPool, String) {
    let url = std::env::var("BILLING_TEST_DATABASE_URL").expect("isolated billing_test_* required");
    let admin = PgPoolOptions::new().connect(&url).await.unwrap();
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert!(db.starts_with("billing_test_"));
    let schema = format!(
        "connection_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    );
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(3)
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
async fn save(p: &PgPool) {
    let item = prepared(
        "user_demo",
        "op",
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
}

// A second connection, COMMIT, or wrong projection identity breaks this test.
#[tokio::test]
async fn connection_reads_uncommitted_original_four_branches_without_committing() {
    let (p, s) = database().await;
    for (choice, txid, card) in [
        (Mine, "card_tx", Some("fictional_card")),
        (NotMine, "card_tx", Some("fictional_card")),
        (Unsure, "card_tx", Some("fictional_card")),
        (NotMine, "no_card", None),
    ] {
        let item = prepared(
            "user_demo",
            "op",
            "report1",
            txid,
            choice.clone(),
            "snapshot1",
        );
        let mut tx = p.begin().await.unwrap();
        let staged = serde_json::to_value(
            stage_feedback(&mut tx, &scope("user_demo"), &item)
                .await
                .unwrap(),
        )
        .unwrap();
        let before: i64 = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let owned = load_feedback_on_connection(&mut tx, &scope("user_demo"), "op")
            .await
            .unwrap()
            .unwrap();
        let v = owned.checked_projection(&scope("user_demo")).unwrap();
        assert_eq!(v.operation_id(), "op");
        assert_eq!(v.owner_user_id(), "user_demo");
        assert_eq!(v.choice(), &choice);
        assert_eq!(v.card_id(), card);
        assert_eq!(v.reference().source_id, "fictional-pg");
        assert_eq!(v.reference().account_id, "account_001");
        assert_eq!(v.reference().transaction_id, txid);
        assert_eq!(v.report_id(), "report1");
        assert_eq!(v.snapshot_id(), "snapshot1");
        assert_eq!(v.calculated_at(), "2026-10-19T12:00:00+08:00");
        assert!(!v.rule_version().is_empty());
        assert_eq!(v.local_event_namespace(), "billing_feedback_events_v1");
        assert_eq!(v.local_event_kind(), "BILLING_FEEDBACK_RECORDED_LOCAL_V1");
        assert_eq!(
            v.local_event_id(),
            staged["event"]["event_id"].as_i64().unwrap()
        );
        assert_eq!(
            v.local_event_recorded_at(),
            staged["event"]["recorded_at"].as_str().unwrap()
        );
        assert!(load_feedback(&p, &scope("user_demo"), "op")
            .await
            .unwrap()
            .is_none());
        let after: i64 = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(before, after);
        tx.rollback().await.unwrap();
        // Owned observation survives a connection borrow, NOT a commit proof.
        assert_eq!(
            owned
                .checked_projection(&scope("user_demo"))
                .unwrap()
                .operation_id(),
            "op"
        );
        assert!(load_feedback(&p, &scope("user_demo"), "op")
            .await
            .unwrap()
            .is_none());
    }
    cleanup(p, s).await;
}

// Detect persistent writes, transaction termination and caller-savepoint damage.
// The strict SELECT-only guarantee also requires reviewing the reader's SQL.
#[tokio::test]
async fn connection_reader_works_read_only_and_autocommit_preserving_old_api() {
    let (p, s) = database().await;
    save(&p).await;
    let original = serde_json::to_value(
        load_feedback(&p, &scope("user_demo"), "op")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let mut tx = p.begin().await.unwrap();
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SAVEPOINT caller_point")
        .execute(&mut *tx)
        .await
        .unwrap();
    let owned = load_feedback_on_connection(&mut tx, &scope("user_demo"), "op")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        owned
            .checked_projection(&scope("user_demo"))
            .unwrap()
            .choice(),
        &NotMine
    );
    sqlx::query("ROLLBACK TO SAVEPOINT caller_point")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut c = p.acquire().await.unwrap();
    assert!(
        load_feedback_on_connection(&mut c, &scope("user_demo"), "op")
            .await
            .unwrap()
            .is_some()
    );
    let current = load_feedback(&p, &scope("user_demo"), "op")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(&current).unwrap(), original);
    assert_eq!(
        current
            .checked_projection(&scope("user_demo"))
            .unwrap()
            .card_disposition(),
        CardDisposition::AssociationUnverified
    );
    drop(c);
    cleanup(p, s).await;
}

// Missing/unauthorized are None, not database/invalid-input errors; cached scope is rechecked.
#[tokio::test]
async fn connection_visibility_errors_and_cached_scope_are_distinct() {
    let (p, s) = database().await;
    save(&p).await;
    let mut c = p.acquire().await.unwrap();
    let owned = load_feedback_on_connection(&mut c, &scope("user_demo"), "op")
        .await
        .unwrap()
        .unwrap();
    for denied in [
        scope("other"),
        TrustedScope::from_verified_context("user_demo", ["account_001"]).unwrap(),
    ] {
        assert!(load_feedback_on_connection(&mut c, &denied, "op")
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            owned.checked_projection(&denied),
            Err(StoreError::ResourceUnavailable)
        ));
    }
    assert!(
        load_feedback_on_connection(&mut c, &scope("user_demo"), "missing")
            .await
            .unwrap()
            .is_none()
    );
    for bad in [String::new(), "x".repeat(201)] {
        assert!(matches!(
            load_feedback_on_connection(&mut c, &scope("user_demo"), &bad).await,
            Err(StoreError::InvalidInput)
        ));
    }
    let bad_scope = scope(&"x".repeat(201));
    assert!(matches!(
        owned.checked_projection(&bad_scope),
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        load_feedback_on_connection(&mut c, &bad_scope, "op").await,
        Err(StoreError::InvalidInput)
    ));
    assert_eq!(
        owned
            .checked_projection(&scope("user_demo"))
            .unwrap()
            .report_id(),
        "report1"
    );
    sqlx::query("SET search_path TO pg_catalog")
        .execute(&mut *c)
        .await
        .unwrap();
    assert!(matches!(
        load_feedback_on_connection(&mut c, &scope("user_demo"), "op").await,
        Err(StoreError::Database)
    ));
    drop(c);
    cleanup(p, s).await;
}

// Corrupt data is test-only; allow edits ONLY in this newly created isolated schema.
async fn allow_test_damage(p: &PgPool) {
    sqlx::raw_sql("ALTER TABLE billing_feedback_v1 DISABLE TRIGGER billing_feedback_immutable_v1; ALTER TABLE billing_report_bindings_v1 DISABLE TRIGGER billing_report_immutable_v1; ALTER TABLE billing_feedback_events_v1 DISABLE TRIGGER billing_event_immutable_v1;")
        .execute(p).await.unwrap();
}
async fn replace_payload(p: &PgPool, payload: &Value) {
    sqlx::query("UPDATE billing_feedback_v1 SET payload=$1 WHERE public_operation_id='op'")
        .bind(Json(payload))
        .execute(p)
        .await
        .unwrap();
    let header = json!({"owner_user_id":payload["owner_user_id"],"report_id":payload["report_id"],"request":payload["request"],"source_id":payload["reference"]["source_id"],"snapshot_id":payload["snapshot_id"],"rule_version":payload["rule_version"],"calculated_at":payload["calculated_at"]});
    sqlx::query("UPDATE billing_report_bindings_v1 SET header=$1")
        .bind(Json(header))
        .execute(p)
        .await
        .unwrap();
}

// Missing metadata validation, accidental sorting, or a 200-byte non-target limit breaks this.
#[tokio::test]
async fn connection_preserves_full_request_order_and_long_non_target_account() {
    let (p, s) = database().await;
    save(&p).await;
    allow_test_damage(&p).await;
    let Json(mut payload): Json<Value> =
        sqlx::query_scalar("SELECT payload FROM billing_feedback_v1")
            .fetch_one(&p)
            .await
            .unwrap();
    let long = "z".repeat(201);
    payload["request"]["account_ids"] = json!([long, "account_002", "account_001"]);
    replace_payload(&p, &payload).await;
    let current = TrustedScope::from_verified_context(
        "user_demo",
        [long.as_str(), "account_001", "account_002"],
    )
    .unwrap();
    let mut c = p.acquire().await.unwrap();
    let owned = load_feedback_on_connection(&mut c, &current, "op")
        .await
        .unwrap()
        .unwrap();
    let v = owned.checked_projection(&current).unwrap();
    assert_eq!(
        v.report_account_ids(),
        &[long, "account_002".into(), "account_001".into()]
    );
    assert_eq!(
        serde_json::to_value(v.request()).unwrap(),
        payload["request"]
    );
    assert_eq!(
        v.request().window.start_inclusive,
        "2026-10-01T00:00:00+08:00"
    );
    drop(c);
    cleanup(p, s).await;
}

// Even matching header/payload must not bypass complete metadata validation.
#[tokio::test]
async fn connection_rejects_damaged_metadata_columns_header_and_event() {
    let (p, s) = database().await;
    save(&p).await;
    allow_test_damage(&p).await;
    let Json(original): Json<Value> = sqlx::query_scalar("SELECT payload FROM billing_feedback_v1")
        .fetch_one(&p)
        .await
        .unwrap();
    let cases = [
        ("/request/account_ids", json!([])),
        (
            "/request/account_ids",
            json!(["account_001", "account_001"]),
        ),
        ("/request/account_ids", json!(["account_002"])),
        ("/snapshot_id", json!("")),
        ("/rule_version", json!("")),
        ("/calculated_at", json!("bad")),
        ("/transaction/card_id", json!("")),
        ("/transaction/transaction_id", json!("other")),
        ("/reference/transaction_id", json!("other")),
        ("/choice", json!("INVALID")),
    ];
    for (pointer, value) in cases {
        let mut damaged = original.clone();
        *damaged.pointer_mut(pointer).unwrap() = value;
        replace_payload(&p, &damaged).await;
        let mut c = p.acquire().await.unwrap();
        assert!(
            matches!(
                load_feedback_on_connection(&mut c, &scope("user_demo"), "op").await,
                Err(StoreError::CorruptRecord)
            ),
            "{pointer}"
        );
    }
    replace_payload(&p, &original).await;
    for sql in [
        "UPDATE billing_report_bindings_v1 SET header='{}'",
        "UPDATE billing_feedback_events_v1 SET recorded_at='infinity'",
        "DELETE FROM billing_feedback_events_v1",
    ] {
        let mut tx = p.begin().await.unwrap();
        sqlx::query(sql).execute(&mut *tx).await.unwrap();
        assert!(
            matches!(
                load_feedback_on_connection(&mut tx, &scope("user_demo"), "op").await,
                Err(StoreError::CorruptRecord)
            ),
            "{sql}"
        );
        tx.rollback().await.unwrap();
    }
    cleanup(p, s).await;
}
