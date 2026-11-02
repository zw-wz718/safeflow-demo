//! Fictional PostgreSQL handoff evidence only, not a card provider or HTTP route.
#[path = "../tests/support/feedback.rs"]
mod support;
use safeflow_billing_source_reader::{
    feedback::FeedbackChoice::*, feedback_store::*, TrustedScope,
};
use serde_json::{json, Map, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};

async fn collect(pool: &PgPool) -> Result<Value, Box<dyn std::error::Error>> {
    let owner = support::scope("user_demo");
    let mut cases = Map::new();
    for (label, transaction, choice) in [
        ("not_mine_with_card", "card_tx", NotMine),
        ("not_mine_no_card", "no_card_tx", NotMine),
        ("mine", "mine_tx", Mine),
        ("unsure", "unsure_tx", Unsure),
    ] {
        let operation = format!("card_handoff_{label}_v1");
        let prepared = support::prepared(
            "user_demo",
            &operation,
            "card_handoff_report_v1",
            transaction,
            choice,
            "fictional_card_handoff_snapshot_v1",
        );
        let mut tx = pool.begin().await?;
        stage_feedback(&mut tx, &owner, &prepared).await?;
        tx.commit().await?; // Fictional demo transaction, NOT the public executor.
        let saved = load_feedback(pool, &owner, &operation)
            .await?
            .ok_or("committed fictional feedback missing")?;
        cases.insert(label.into(), serde_json::to_value(saved)?);
    }
    let original = "card_handoff_not_mine_with_card_v1";
    let other = load_feedback(pool, &support::scope("other_demo_user"), original).await?;
    // Removing even the other account in the original report blocks this read.
    let revoked = TrustedScope::from_verified_context("user_demo", ["account_001"])?;
    let revoked = load_feedback(pool, &revoked, original).await?;
    let missing = load_feedback(pool, &owner, "card_handoff_missing_v1").await?;
    let pending = support::prepared(
        "user_demo",
        "card_handoff_uncommitted_v1",
        "card_handoff_report_v1",
        "pending_demo_tx",
        NotMine,
        "fictional_card_handoff_snapshot_v1",
    );
    let mut tx = pool.begin().await?;
    stage_feedback(&mut tx, &owner, &pending).await?;
    let uncommitted_visible = load_feedback(pool, &owner, "card_handoff_uncommitted_v1")
        .await?
        .is_some();
    tx.rollback().await?;
    // A deliberately closed test pool yields an actual database lookup error.
    let closed = PgPoolOptions::new().connect_lazy("postgres://localhost/billing_test_unused")?;
    closed.close().await;
    let database_error = load_feedback(&closed, &owner, original).await.unwrap_err();
    Ok(json!({"fictional":true,"public_integration_verified":false,
        "contract":"LOCAL_BILLING_SAVED_FEEDBACK_DEMO_V1", "cases":cases,
        "visibility":{"other_owner":other,"revoked_account":revoked,"not_found":missing,
            "uncommitted_visible":uncommitted_visible,"database_error":database_error.to_string()}}))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--fictional-demo"] {
        return Err(
            "usage: feedback_card_handoff --fictional-demo; writes isolated billing_test_* DB only"
                .into(),
        );
    }
    let url = std::env::var("BILLING_TEST_DATABASE_URL")?;
    let admin = PgPoolOptions::new().connect(&url).await?;
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    if !name.starts_with("billing_test_") {
        return Err("refuse non-test database".into());
    }
    sqlx::query("CREATE SCHEMA IF NOT EXISTS billing_card_handoff_demo_v1")
        .execute(&admin)
        .await?;
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO billing_card_handoff_demo_v1")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT to_regclass('billing_card_handoff_demo_v1.billing_feedback_v1')::text",
    )
    .fetch_one(&pool)
    .await?;
    if existing.is_none() {
        sqlx::raw_sql(include_str!("../sql/feedback-v1.sql"))
            .execute(&pool)
            .await?;
    }
    let output = collect(&pool).await?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    pool.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    #[tokio::test]
    async fn handoff_preserves_saved_evidence_and_denies_foreign_or_revoked_reads() {
        let url = std::env::var("BILLING_TEST_DATABASE_URL")
            .expect("isolated billing_test_* DB required");
        let admin = PgPoolOptions::new().connect(&url).await.unwrap();
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&admin)
            .await
            .unwrap();
        assert!(name.starts_with("billing_test_"));
        let schema = format!("billing_card_test_{}", std::process::id());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |conn, _| {
                let query = format!("SET search_path TO {path}");
                Box::pin(async move {
                    sqlx::query(&query).execute(conn).await?;
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
        let result = collect(&pool).await;
        if let Ok(ref actual) = result {
            assert_eq!(actual["fictional"], true);
            assert_eq!(actual["public_integration_verified"], false);
            assert_eq!(
                actual["cases"]["not_mine_with_card"]["evidence"]["choice"],
                "NOT_MINE"
            );
            assert_eq!(
                actual["cases"]["not_mine_with_card"]["evidence"]["transaction"]["card_id"],
                "fictional_card"
            );
            assert_eq!(
                actual["cases"]["not_mine_no_card"]["card_handoff"],
                "BLOCKED_NO_CARD"
            );
            assert_eq!(actual["cases"]["mine"]["evidence"]["choice"], "MINE");
            assert_eq!(actual["cases"]["unsure"]["evidence"]["choice"], "UNSURE");
            for case in actual["cases"].as_object().unwrap().values() {
                assert_eq!(case["state"], "SAVED");
                assert_eq!(case["persisted"], true);
                assert_eq!(case["lock_authorized"], false);
                assert_eq!(case["evidence"]["owner_user_id"], "user_demo");
                assert_eq!(case["evidence"]["reference"]["source_id"], "fictional-pg");
                assert_eq!(case["evidence"]["transaction"]["account_id"], "account_001");
            }
            assert_eq!(
                actual["visibility"],
                json!({"other_owner":null,
                "revoked_account":null,"not_found":null,"uncommitted_visible":false,
                "database_error":"Database"})
            );
            assert_eq!(
                actual,
                &collect(&pool).await.unwrap(),
                "rerun keeps original event identity/time"
            );
            let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM billing_feedback_v1), (SELECT count(*) FROM billing_feedback_events_v1)")
                .fetch_one(&pool).await.unwrap();
            assert_eq!(counts, (4, 4));
        }
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        result.unwrap();
    }
}
