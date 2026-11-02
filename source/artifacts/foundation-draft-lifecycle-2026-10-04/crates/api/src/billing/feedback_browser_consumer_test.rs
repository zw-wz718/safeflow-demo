//! Opt-in consumer validation host. Uses only existing isolated fictional inputs,
//! explicit test TTLs and an ephemeral API port; no production wiring.
use super::*;

#[tokio::test]
#[ignore = "isolated browser + PostgreSQL; requires explicit own evidence directory and origin"]
async fn billing_feedback_browser_consumer_host() {
    let root = std::path::PathBuf::from(std::env::var("SAFEFLOW_BILLS_BROWSER_ROOT").unwrap());
    assert!(root.is_absolute());
    assert!(!root.join("browser-state.json").exists());
    assert!(!root.join("stop-browser").exists());
    let origin = std::env::var("SAFEFLOW_FEEDBACK_BROWSER_ORIGIN").unwrap();
    assert!(origin.starts_with("http://127.0.0.1:"));
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, source, cookie, complete) = report(&db, &provider).await;
    let task = complete["task"]["task_id"].as_str().unwrap();
    let before_calls = source.calls.load(Ordering::SeqCst);
    std::fs::write(
        root.join("browser-state.json"),
        serde_json::to_vec_pretty(&json!({
            "api":format!("http://{}",server.address), "frontend":origin,
            "report_task_id":task, "namespace":db.namespace,
            "funds":false, "production_defaults_changed":false,
            "fixture":"unchanged member mixed / 8ff8c665:mixed",
            "report":complete
        }))
        .unwrap(),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while !root.join("stop-browser").exists() {
        assert!(std::time::Instant::now() < deadline, "browser host expired");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT public_operation_id, payload FROM billing_feedback_v1 ORDER BY public_operation_id",
    )
    .fetch_all(&db.scoped)
    .await
    .unwrap();
    let mut checks = vec![];
    for (operation, saved) in &rows {
        let response = ok(server
            .call(
                Method::GET,
                &format!("/api/billing-feedback/operations/{operation}"),
                None,
                Some(&cookie),
                None,
                None,
                false,
            )
            .await);
        assert_eq!(response["operation_id"], *operation);
        assert_eq!(response["saved"]["evidence"], *saved);
        assert_eq!(response["task_state"], "COMPLETED");
        assert_eq!(response["operation_state"], "SUCCEEDED");
        assert_eq!(response["lock_authorized"], false);
        checks.push(response);
    }
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        before_calls,
        "history consumer must not reread source"
    );
    let events = count(&db, "billing_feedback_events_v1").await;
    std::fs::write(
        root.join("browser-db-verification.json"),
        serde_json::to_vec_pretty(&json!({
            "feedback_rows":rows.len(), "member_events":events,
            "same_original_get_and_db_payload":checks, "source_not_rerun":true,
            "real_http":true, "real_isolated_postgresql":true
        }))
        .unwrap(),
    )
    .unwrap();
    server.handle.abort();
    provider.handle.abort();
    db.finish().await;
    std::fs::write(
        root.join("browser-schema-cleanup.json"),
        b"{\"own_schema_dropped\":true}",
    )
    .unwrap();
    assert_eq!(rows.len(), 1, "expected one explicit browser save");
    assert_eq!(events, 1);
}
