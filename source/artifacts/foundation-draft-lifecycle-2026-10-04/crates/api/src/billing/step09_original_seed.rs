use super::*;

#[tokio::test]
#[ignore = "explicit owned step09 original business transaction; retain schema for sender"]
async fn billing_original_transaction() {
    assert_eq!(std::env::var("SAFEFLOW_STEP09_OWNED").unwrap(), "OWNED_CLUSTER_ONLY");
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, source, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    install(&db, original).await;
    let f = service(&server);
    let s = session(&cookie);
    let p = f.prepare(&s, intent(original)).await.unwrap();
    f.confirm(&s, &p).await.unwrap();
    let saved = f.execute(&s, &p.task.task_id, &p.operation.operation_id, &p.confirmation.confirmation_ref).await.unwrap();
    assert_eq!(saved.domain_audit, BillingDomainAuditStatus::VerifiedV1);
    assert_eq!(domain_count(&db).await, 1);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    let value = json!({"namespace":db.namespace,"operation_id":p.operation.operation_id,
        "saved":saved,"source":"existing report/prepare/confirm/member transaction/public event/Outbox"});
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(std::env::var("SAFEFLOW_STEP09_SEED").unwrap()).unwrap()
        .write_all(&serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    server.stop();
    db.repo.close().await;
    db.scoped.close().await;
    db.admin.close().await;
}
