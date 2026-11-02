//! Public original-query adoption, using the unchanged actual member read/save path.
use super::*;
use safeflow_contracts::OperationId;

#[tokio::test]
#[ignore = "owned billing_test_* PostgreSQL; correlate actual SELECTs with owned server log"]
async fn billing_connection_public_query_preserves_original_result_without_source_or_write() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, source, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let f = service(&server);
    let s = session(&cookie);
    let p = f.prepare(&s, intent(original)).await.unwrap();
    f.confirm(&s, &p).await.unwrap();
    let saved = f
        .execute(
            &s,
            &p.task.task_id,
            &p.operation.operation_id,
            &p.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    let before = snapshot(&db).await;
    for _ in 0..2 {
        let read = f
            .query(&s, &p.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.saved, saved.saved);
        assert_eq!(read.saved["evidence"]["request"], original["request"]);
        assert_eq!(read.saved["lock_authorized"], false);
        assert!(read.replayed);
    }
    assert!(f
        .query(
            &s,
            &OperationId::parse("billing_connection_missing").unwrap()
        )
        .await
        .unwrap()
        .is_none());
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    if let Ok(root) = std::env::var("SAFEFLOW_CONNECTION_EVIDENCE") {
        std::fs::write(std::path::Path::new(&root).join("public-reader.json"),
            serde_json::to_vec_pretty(&json!({"schema":db.namespace,"operation":p.operation.operation_id,
                "original_request":original["request"],"source_reads":1,"query_counts_unchanged":true,
                "lock_authorized":false,"returned_saved":saved.saved})).unwrap()).unwrap();
    }
    server.stop();
    db.finish().await;
}
