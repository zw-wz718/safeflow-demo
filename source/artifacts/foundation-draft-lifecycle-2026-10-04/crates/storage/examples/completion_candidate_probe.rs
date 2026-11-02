//! Local privileged persistence probe. No identity registration or funds writer.
//! Not wired to main/HTTP/Agent; requires explicitly selected existing namespace.
use safeflow_storage::{PaymentCompletionCandidateQueue, PgRepository};
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, Row};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("mode required")?;
    let namespace = args.next().ok_or("namespace required")?;
    // Testing tool is deliberately confined to isolated fixture namespaces.
    if !namespace.starts_with("sf_http_test_")
        || namespace.len() != 45
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("isolated test namespace required".into());
    }
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")?;
    let repo = PgRepository::connect_existing(&url, &namespace).await?;
    if mode == "claim" {
        let q = PaymentCompletionCandidateQueue::for_trusted_backend(repo.clone());
        let lease = q
            .claim(
                "recovery_job",
                "probe_label_not_principal",
                Duration::from_millis(200),
            )
            .await?;
        println!(
            "{}",
            json!({"pid":std::process::id(),"mode":mode,"epoch":lease.as_ref().map(|l|l.epoch()),"funds_authority":false})
        );
    } else {
        let path = namespace.clone();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |c, _| {
                let p = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(p)
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await?;
        if mode == "read" {
            let mut tx = pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            let row=sqlx::query("SELECT lease_epoch,(SELECT count(*) FROM payment_completion_candidate_grants) AS grants,(SELECT count(*) FROM payment_completion_candidate_decisions) AS decisions,(SELECT count(*) FROM payment_completion_candidate_receipts) AS receipts FROM payment_completion_candidate_jobs WHERE grant_id='recovery_job'").fetch_one(&mut *tx).await?;
            let mut facts = Vec::<serde_json::Value>::new();
            for table in [
                "payment_completion_candidate_grants",
                "payment_completion_candidate_decisions",
                "payment_completion_candidate_controls",
                "payment_completion_candidate_fences",
                "payment_completion_candidate_receipts",
                "public_execution_requests",
                "operations",
                "transfer_business_orders_v1",
                "transfer_fund_orders_v1",
                "transfer_accounts_v1",
                "transfer_ledger_v1",
                "audit_events",
                "public_audit_outbox",
            ] {
                facts.push(json!({"table":table,"rows":sqlx::query_scalar::<_,serde_json::Value>(&format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t")).fetch_one(&mut *tx).await?}));
            }
            let result = json!({"pid":std::process::id(),"epoch":row.try_get::<i64,_>("lease_epoch")?,"grants":row.try_get::<i64,_>("grants")?,"decisions":row.try_get::<i64,_>("decisions")?,"receipts":row.try_get::<i64,_>("receipts")?,"facts":facts,"funds_authority":false});
            tx.commit().await?;
            println!("{result}");
        } else {
            return Err("claim|read required".into());
        }
        pool.close().await;
    }
    repo.close().await;
    Ok(())
}
