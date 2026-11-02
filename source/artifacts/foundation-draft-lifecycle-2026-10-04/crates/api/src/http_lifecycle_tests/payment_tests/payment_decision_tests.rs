//! Foundation mechanism tests; no member JSON or payment Policy implementation.
use super::*;

fn latest(v: &Value) -> &Value {
    v["security_decisions"].as_array().unwrap().last().unwrap()
}

fn assert_projected_invalidation(projected: &Value, persisted: &Value) {
    let projected_end = latest(projected)["ended_at_ms"].as_i64().unwrap();
    let persisted_end = latest(persisted)["ended_at_ms"].as_i64().unwrap();
    assert!(persisted_end >= projected_end);
    let mut expected = projected.clone();
    expected["security_decisions"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()["ended_at_ms"] = json!(persisted_end);
    assert_eq!(&expected, persisted);
}
async fn minimum_count(db: &Db) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM security_decisions WHERE source_mode='TRUSTED_PAYMENT_MINIMUM_V1'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap()
}
async fn evaluate(server: &Server, cookie: &str, payment: &Value) -> Value {
    ok(command(
        server,
        cookie,
        &format!("{}/evaluate", path(payment)),
        decision(payment, "CONFIRM"),
    )
    .await)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_decision_http_exact_boundary_and_above_are_real_kernel_results_without_funds_writes(
) {
    for (amount, level, outcome) in [
        ("1000.00", "YELLOW", "READY_TO_CONFIRM"),
        ("1000.01", "RED", "WAITING_AUTH"),
    ] {
        let db = Db::new().await;
        provision_payer(&db).await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let selected=select(&server,&cookie,&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":amount}})).await;
        let modified = run(&server, &cookie, &selected).await.1;
        let payment = prepared(&server, &cookie, &modified).await;
        assert_eq!(payment["decision_freshness"], "NOT_EVALUATED");
        assert_eq!(
            command(
                &server,
                &cookie,
                &format!("{}/evaluate", path(&payment)),
                decision(&payment, "CONFIRM")
            )
            .await
            .status,
            StatusCode::CONFLICT
        );
        let before = funds_state(&db).await;
        let result = decided(&server, &cookie, &payment, "CONFIRM").await;
        assert_eq!(result["decision_freshness"], "CURRENT_MINIMUM_ONLY");
        assert_eq!(latest(&result)["decision"]["minimum_level"], level);
        assert_eq!(latest(&result)["decision"]["outcome"], outcome);
        assert_eq!(
            latest(&result)["assembly_metadata"]["kernel"]["risk_mode"],
            "NOT_EVALUATED"
        );
        assert_eq!(
            latest(&result)["assembly_metadata"]["facts"]["tool_parameters"]["draft_version"],
            2
        );
        assert_eq!(result["operation"]["state"], "DRAFT");
        assert_eq!(result["funds_execution_enabled"], false);
        assert_eq!(funds_state(&db).await, before);
        assert_eq!(minimum_count(&db).await, 1);
        assert_eq!(ok(query(&server, &cookie, &path(&payment)).await), result);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual payment HTTP"]
async fn payment_decision_http_original_replay_and_explicit_refresh_are_idempotent() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    for (key, value) in [
        ("used_cents", json!(0)),
        ("risk_score", json!(0)),
        ("minimum_level", json!("YELLOW")),
        ("auth_grant_ref", json!("forged")),
    ] {
        let mut forged = decision(&payment, "CONFIRM");
        forged[key] = value;
        assert_eq!(
            command(
                &server,
                &cookie,
                &format!("{}/evaluate", path(&payment)),
                forged
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    let endpoint = format!("{}/confirm", path(&payment));
    let (a, b) = tokio::join!(
        command(&server, &cookie, &endpoint, decision(&payment, "CONFIRM")),
        command(&server, &cookie, &endpoint, decision(&payment, "CONFIRM"))
    );
    let a = ok(a);
    let b = ok(b);
    assert_eq!(a, b);
    assert_eq!(
        latest(&a)["assembly_metadata"]["facts"]["combined_cents"],
        20_000
    );
    assert_eq!(evaluate(&server, &cookie, &payment).await, a);
    assert_eq!(
        prepared(&server, &cookie, &created).await["security_decisions"],
        a["security_decisions"]
    );
    assert_eq!(minimum_count(&db).await, 1);
    assert_eq!(payment_count(&db).await, 1);
    assert_eq!(count(&db, "execution_permits").await, 0);
    // Persisted original submission marker: query original business ID, never re-add this payment.
    sqlx::query("UPDATE transfer_drafts_v1 SET submitted=true WHERE draft_id=$1")
        .bind(created["receipt"]["draft_id"].as_str().unwrap())
        .execute(&db.scoped)
        .await
        .unwrap();
    let history = ok(query(&server, &cookie, &path(&payment)).await);
    assert_eq!(
        history["decision_freshness"],
        "QUERY_ORIGINAL_BUSINESS_OPERATION"
    );
    assert_eq!(history["business_recovery_required"], true);
    assert_eq!(latest(&history)["status"], "INVALIDATED");
    assert_eq!(
        history["snapshot"]["transfer"]["operation_id"],
        a["snapshot"]["transfer"]["operation_id"]
    );
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/evaluate", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(minimum_count(&db).await, 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual configured decision expiry"]
async fn payment_decision_http_expired_judgment_retains_confirmation_requires_explicit_new_evaluation(
) {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(60),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(2),
                600,
                "payment-minimum-expiry",
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
    let first = decided(&server, &cookie, &payment, "CONFIRM").await;
    assert_eq!(
        latest(&first)["expires_at_ms"].as_i64().unwrap()
            - latest(&first)["evaluated_at_ms"].as_i64().unwrap(),
        2000
    );
    let before = funds_state(&db).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let stored_before = super::payment_execution_tests::persistent_state(&db).await;
    let history = ok(query(&server, &cookie, &path(&payment)).await);
    assert_eq!(
        super::payment_execution_tests::persistent_state(&db).await,
        stored_before
    );
    assert_eq!(history["confirmation_status"], "CONFIRMED");
    assert_eq!(
        history["decision_freshness"],
        "HISTORICAL_REEVALUATION_REQUIRED"
    );
    assert_eq!(latest(&history)["invalidation_reason"], "EXPIRED");
    let persisted = decided(&server, &cookie, &payment, "CONFIRM").await;
    assert_projected_invalidation(&history, &persisted);
    let refreshed = evaluate(&server, &cookie, &payment).await;
    assert_eq!(refreshed["decision_freshness"], "CURRENT_MINIMUM_ONLY");
    assert_ne!(
        latest(&refreshed)["stored_decision_id"],
        latest(&first)["stored_decision_id"]
    );
    assert_eq!(minimum_count(&db).await, 2);
    assert_eq!(funds_state(&db).await, before);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and current owned account"]
async fn payment_decision_http_missing_or_foreign_payer_cannot_confirm_or_save_judgment() {
    for foreign in [false, true] {
        let db = Db::new().await;
        if foreign {
            let mut tx = db.scoped.begin().await.unwrap();
            safeflow_transfer_integration::funds_store::FundsStore::provision_account(
                &mut tx,
                &UserId::parse("user_002").unwrap(),
                &safeflow_contracts::ResourceId::parse("account_001").unwrap(),
                &safeflow_bank_gateway::DecimalAmount::parse("1288.00").unwrap(),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let server = server(&db).await;
        let cookie = server.login().await;
        let created = draft(&server, &cookie).await;
        let payment = prepared(&server, &cookie, &created).await;
        let before = funds_state(&db).await;
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
        assert_eq!(minimum_count(&db).await, 0);
        let current = ok(query(&server, &cookie, &path(&payment)).await);
        assert_eq!(current["confirmation_status"], "PREPARED");
        assert_eq!(funds_state(&db).await, before);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and source refresh"]
async fn payment_decision_http_historical_lookup_does_not_silently_reprice_changed_account_scope() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    let first = decided(&server, &cookie, &payment, "CONFIRM").await;
    let mut tx = db.scoped.begin().await.unwrap();
    safeflow_transfer_integration::funds_store::FundsStore::provision_account(
        &mut tx,
        &UserId::parse("user_001").unwrap(),
        &safeflow_contracts::ResourceId::parse("payment_second_account").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let before = funds_state(&db).await;
    let stored_before = super::payment_execution_tests::persistent_state(&db).await;
    let historical = ok(query(&server, &cookie, &path(&payment)).await);
    assert_eq!(
        super::payment_execution_tests::persistent_state(&db).await,
        stored_before
    );
    assert_eq!(
        historical["decision_freshness"],
        "HISTORICAL_REEVALUATION_REQUIRED"
    );
    assert_eq!(
        latest(&historical)["stored_decision_id"],
        latest(&first)["stored_decision_id"]
    );
    assert_eq!(latest(&historical)["status"], "INVALIDATED");
    let persisted = decided(&server, &cookie, &payment, "CONFIRM").await;
    assert_projected_invalidation(&historical, &persisted);
    assert_eq!(minimum_count(&db).await, 1);
    let refreshed = evaluate(&server, &cookie, &payment).await;
    assert_eq!(refreshed["security_decisions"].as_array().unwrap().len(), 2);
    assert_eq!(refreshed["decision_freshness"], "CURRENT_MINIMUM_ONLY");
    assert_ne!(
        latest(&refreshed)["stored_decision_id"],
        latest(&first)["stored_decision_id"]
    );
    assert_eq!(
        latest(&refreshed)["assembly_metadata"]["facts"]["combined_cents"],
        20_000
    );
    assert_eq!(evaluate(&server, &cookie, &payment).await, refreshed);
    assert_eq!(funds_state(&db).await, before);
    // Current ownership changes cannot be covered by an old minimum judgment.
    db.scoped
        .execute(
            "UPDATE transfer_accounts_v1 SET owner_id='user_002' WHERE account_id='account_001'",
        )
        .await
        .unwrap();
    let unavailable = decided(&server, &cookie, &payment, "CONFIRM").await;
    assert_eq!(unavailable["decision_freshness"], "FACTS_UNAVAILABLE");
    assert_eq!(latest(&unavailable)["status"], "INVALIDATED");
    assert_eq!(
        ok(query(&server, &cookie, &path(&payment)).await),
        unavailable
    );
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/evaluate", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(minimum_count(&db).await, 2);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and transactional decision fault"]
async fn payment_decision_http_decision_audit_failure_rolls_back_confirmation_and_decision() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let created = draft(&server, &cookie).await;
    let payment = prepared(&server, &cookie, &created).await;
    let before = funds_state(&db).await;
    db.scoped.execute("CREATE FUNCTION fail_minimum_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details->>'source_mode'='TRUSTED_PAYMENT_MINIMUM_V1' THEN RAISE EXCEPTION 'isolated minimum audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_minimum_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_minimum_audit()").await.unwrap();
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", path(&payment)),
            decision(&payment, "CONFIRM")
        )
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(minimum_count(&db).await, 0);
    assert_eq!(
        ok(query(&server, &cookie, &path(&payment)).await)["confirmation_status"],
        "PREPARED"
    );
    db.scoped
        .execute("DROP TRIGGER fail_minimum_audit ON audit_events")
        .await
        .unwrap();
    let result = decided(&server, &cookie, &payment, "CONFIRM").await;
    assert_eq!(result["decision_freshness"], "CURRENT_MINIMUM_ONLY");
    assert_eq!(funds_state(&db).await, before);
    server.stop();
    db.finish().await;
}
