//! Fictional recovery demonstration, never a production write endpoint.
#[path = "../tests/support/feedback.rs"]
mod support;
use safeflow_billing_source_reader::{feedback::FeedbackChoice, feedback_store::*};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode != "seed" && mode != "recover" {
        return Err(
            "usage: feedback_postgres seed|recover; fictional billing_test_* DB only".into(),
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
    if mode == "seed" {
        sqlx::query("CREATE SCHEMA IF NOT EXISTS billing_restart_demo_v1")
            .execute(&admin)
            .await?;
    }
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO billing_restart_demo_v1")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    if mode == "seed" {
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT to_regclass('billing_restart_demo_v1.billing_feedback_v1')::text",
        )
        .fetch_one(&pool)
        .await?;
        if existing.is_none() {
            sqlx::raw_sql(include_str!("../sql/feedback-v1.sql"))
                .execute(&pool)
                .await?;
        }
        let prepared = support::prepared(
            "user_demo",
            "restart_demo_op1",
            "restart_demo_report1",
            "tx1",
            FeedbackChoice::Unsure,
            "fictional_restart_snapshot1",
        );
        let mut tx = pool.begin().await?;
        stage_feedback(&mut tx, &support::scope("user_demo"), &prepared).await?;
        // Simulates the public executor's commit only in this fictional example.
        // Real integration also needs permission/session, result and Outbox.
        tx.commit().await?;
    }
    // Recover is read-only and does not create schema or issue another write.
    let saved = load_feedback(&pool, &support::scope("user_demo"), "restart_demo_op1")
        .await?
        .ok_or("original feedback not found")?;
    println!("{}", serde_json::to_string_pretty(&saved)?);
    pool.close().await;
    Ok(())
}
