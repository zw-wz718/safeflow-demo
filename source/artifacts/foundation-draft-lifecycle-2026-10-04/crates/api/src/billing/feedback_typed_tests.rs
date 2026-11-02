//! Public committed-read checks, using existing fictional member rows. Deliberate
//! corruption is restricted to a fresh isolated test schema, never a migration.
use super::*;
use safeflow_billing_source_reader::{feedback_store::load_feedback, TrustedScope};

// Keep member columns/header internally coherent so its parser/projection will
// accept each row. The PUBLIC original request/report must detect the mismatch.
async fn replace_member_evidence(db: &Db, data: &Value) {
    let mut tx = db.scoped.begin().await.unwrap();
    tx.execute("ALTER TABLE billing_feedback_v1 DISABLE TRIGGER billing_feedback_immutable_v1")
        .await
        .unwrap();
    tx.execute(
        "ALTER TABLE billing_report_bindings_v1 DISABLE TRIGGER billing_report_immutable_v1",
    )
    .await
    .unwrap();
    let header = json!({"owner_user_id":data["owner_user_id"],"report_id":data["report_id"],
        "request":data["request"],"source_id":data["reference"]["source_id"],
        "snapshot_id":data["snapshot_id"],"rule_version":data["rule_version"],"calculated_at":data["calculated_at"]});
    sqlx::query("UPDATE billing_feedback_v1 SET payload=$1,source_id=$2,account_id=$3,transaction_id=$4 WHERE public_operation_id=$5")
        .bind(sqlx::types::Json(data)).bind(data["reference"]["source_id"].as_str().unwrap())
        .bind(data["reference"]["account_id"].as_str().unwrap())
        .bind(data["reference"]["transaction_id"].as_str().unwrap())
        .bind(data["public_operation_id"].as_str().unwrap()).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "UPDATE billing_report_bindings_v1 SET header=$1 WHERE owner_user_id=$2 AND report_id=$3",
    )
    .bind(sqlx::types::Json(header))
    .bind(data["owner_user_id"].as_str().unwrap())
    .bind(data["report_id"].as_str().unwrap())
    .execute(&mut *tx)
    .await
    .unwrap();
    // Migration 27 adds deferred integrity checks even to legacy member writes.
    // Drain those checks before DDL; this fixture has no v2 root to protect.
    tx.execute("SET CONSTRAINTS ALL IMMEDIATE").await.unwrap();
    tx.execute("ALTER TABLE billing_feedback_v1 ENABLE TRIGGER billing_feedback_immutable_v1")
        .await
        .unwrap();
    tx.execute("ALTER TABLE billing_report_bindings_v1 ENABLE TRIGGER billing_report_immutable_v1")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; coherent member records conflict with public original facts"]
async fn billing_feedback_typed_query_rejects_coherent_saved_fact_conflicts() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    provider.arguments.lock().unwrap()["account_ids_json"] =
        json!("[\"account_001\",\"account_002\"]");
    let source = fixture::replay("mixed");
    let access = Arc::new(AccountAccess {
        denied: std::sync::atomic::AtomicBool::new(false),
    });
    let server = billing_server_with_feedback(
        &db,
        &provider,
        "user_demo",
        access.clone(),
        Some(fixture::approved(
            access.clone(),
            Arc::new(AccountSource(source.clone())),
            "8ff8c665:typed-scope-test",
        )),
        false,
        true,
    )
    .await;
    let cookie = server.login().await;
    let identity = session(&cookie);
    let view = initial(&server, &cookie, "report").await;
    let complete = ok(execute(&server, &cookie, &view).await);
    let original = &complete["results"][0]["data"]["report"];
    assert_eq!(
        original["request"]["account_ids"],
        json!(["account_001", "account_002"])
    );
    let marker: String =
        sqlx::query_scalar("SELECT archive->>'member_revision' FROM public_billing_reports_v1")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(marker, "8ff8c6657d724d4f69e7a3d69266b12ec148e0ea");
    let feedback = service(&server);
    let prepared = feedback.prepare(&identity, intent(original)).await.unwrap();
    feedback.confirm(&identity, &prepared).await.unwrap();
    let saved = feedback
        .execute(
            &identity,
            &prepared.task.task_id,
            &prepared.operation.operation_id,
            &prepared.confirmation.confirmation_ref,
        )
        .await
        .unwrap();
    let data: sqlx::types::Json<Value> =
        sqlx::query_scalar("SELECT payload FROM billing_feedback_v1")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    let scope =
        TrustedScope::from_verified_context("user_demo", ["account_001", "account_002"]).unwrap();
    let before = snapshot(&db).await;
    let mut checked = vec![];
    for name in [
        "choice",
        "source",
        "account",
        "transaction",
        "accounts_subset",
        "accounts_order",
        "window",
        "card",
        "snapshot",
        "rule",
        "calculated_at",
    ] {
        let mut changed = data.0.clone();
        match name {
            "choice" => changed["choice"] = json!("MINE"),
            "source" => changed["reference"]["source_id"] = json!("other_source"),
            "account" => {
                changed["reference"]["account_id"] = json!("account_002");
                changed["transaction"]["account_id"] = json!("account_002");
            }
            "transaction" => {
                changed["reference"]["transaction_id"] = json!("other_transaction");
                changed["transaction"]["transaction_id"] = json!("other_transaction");
            }
            "accounts_subset" => changed["request"]["account_ids"] = json!(["account_001"]),
            "accounts_order" => {
                changed["request"]["account_ids"] = json!(["account_002", "account_001"])
            }
            "window" => {
                changed["request"]["window"]["start_inclusive"] = json!("2026-10-02T00:00:00+08:00")
            }
            "card" => changed["transaction"]["card_id"] = json!("other_card"),
            "snapshot" => changed["snapshot_id"] = json!("other_snapshot"),
            "rule" => changed["rule_version"] = json!("other_rule"),
            "calculated_at" => changed["calculated_at"] = json!("2026-10-23T00:00:00Z"),
            _ => unreachable!(),
        }
        assert_ne!(changed, data.0, "{name}");
        replace_member_evidence(&db, &changed).await;
        // Neither member decode nor checked_projection itself rejects this case.
        let member = load_feedback(&db.scoped, &scope, prepared.operation.operation_id.as_str())
            .await
            .unwrap()
            .unwrap();
        member.checked_projection(&scope).unwrap();
        assert!(
            matches!(
                server
                    .state
                    .card_read_for_backend()
                    .query_feedback(
                        &identity,
                        &feedback,
                        &prepared.operation.operation_id,
                        &uuid::Uuid::new_v4().to_string()
                    )
                    .await,
                Err(StorageError::CorruptRecord)
            ),
            "new navigation accepted {name}"
        );
        assert!(
            matches!(
                feedback
                    .query(&identity, &prepared.operation.operation_id)
                    .await,
                Err(StorageError::CorruptRecord)
            ),
            "public query accepted {name}"
        );
        assert_eq!(snapshot(&db).await, before, "query wrote {name}");
        replace_member_evidence(&db, &data.0).await;
        assert_eq!(
            feedback
                .query(&identity, &prepared.operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .saved,
            saved.saved,
            "restore {name}"
        );
        checked.push(name);
    }
    // Revoke a NON-target original account, then restore it. No source reread.
    access.denied.store(true, Ordering::SeqCst);
    assert!(matches!(
        feedback
            .query(&identity, &prepared.operation.operation_id)
            .await,
        Err(StorageError::NotFound)
    ));
    access.denied.store(false, Ordering::SeqCst);
    assert_eq!(
        feedback
            .query(&identity, &prepared.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            .saved,
        saved.saved
    );
    let path = format!(
        "/api/billing-feedback/operations/{}",
        prepared.operation.operation_id.as_str()
    );
    let http = ok(server
        .call(Method::GET, &path, None, Some(&cookie), None, None, false)
        .await);
    assert_eq!(http["saved"], saved.saved);
    assert_eq!(http["lock_authorized"], false);
    assert!(http.get("audit_candidate").is_none());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    server.state.0.sessions.revoke(&identity).unwrap();
    assert!(matches!(
        feedback
            .query(&identity, &prepared.operation.operation_id)
            .await,
        Err(StorageError::Session)
    ));
    if let Ok(directory) = std::env::var("SAFEFLOW_FEEDBACK_EVIDENCE") {
        std::fs::write(std::path::Path::new(&directory).join("typed-query-conflicts.json"),
            serde_json::to_vec_pretty(&json!({"coherent_member_conflicts_rejected":checked,
                "each_restore_same_saved":true,"non_target_account_revoked_and_restored":true,
                "session_revoked":true,"query_counts_unchanged":true,"source_calls":1,
                "archive_member_revision":marker,"http_wire_compatible":true,"lock_authorized":false})).unwrap()).unwrap();
    }
    server.stop();
    db.finish().await;
}
