// Actual executable shared read coverage. Backend inputs stay in local output.
fn shared_cli(ns: &str, owner: &str, q: Value, role: &str, ok: bool) -> Result<Value> {
    let file = query_path("shared-business-query.json")?;
    std::fs::write(&file, serde_json::to_vec(&q)?)?;
    let options =
        PgConnectOptions::from_str(&std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL")?)?;
    let pw = std::env::var("POSTGRES_PASSWORD")?;
    ensure(pw.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let url = format!(
        "postgres://{role}:{pw}@127.0.0.1:{}/safeflow_subscription_write_test",
        options.get_port()
    );
    let output = std::process::Command::new(std::env::var("SAFEFLOW_SUBSCRIPTION_QUERY_EXE")?)
        .args(["--run-fictional", ns, owner])
        .arg(file)
        .env("SAFEFLOW_SUBSCRIPTION_SHARED_QUERY_URL", url)
        .output()?;
    ensure(output.status.success() == ok)?;
    let v: Value = serde_json::from_slice(&output.stdout)?;
    ensure(v["fictional"] == true)?;
    if !v["notification_sent"].is_null() {
        ensure(v["notification_sent"] == false)?;
    }
    let text = v.to_string();
    for k in [
        "user_demo",
        "session_ref",
        "idempotency_key",
        "original_binding",
        "original_context",
        "source_position",
        "cycle_ref",
        "anchor_date",
        "period_index",
        "subscription_id",
        "postgres://",
    ] {
        ensure(!text.contains(k))?;
    }
    Ok(v)
}
fn business_original(request: &OriginalOperationRequestMock) -> Value {
    json!({"kind":"Original","request":request})
}
async fn shared_subscription_query_cases(
    c: &mut PgConnection,
    other: &mut PgConnection,
    schema: &str,
    sample: &Sample,
    base: &Value,
) -> Result<Vec<Value>> {
    let ns = format!("{schema}_qb");
    let role = format!("{schema}_select");
    c.execute(format!("CREATE SCHEMA \"{ns}\"").as_str())
        .await?;
    c.execute(ddl(&ns)?.as_str()).await?;
    c.execute(format!("GRANT USAGE ON SCHEMA \"{ns}\" TO \"{role}\"; GRANT SELECT ON \"{ns}\".subscription_runtime_records TO \"{role}\"").as_str()).await?;
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
    let init = init_request(&store)?;
    init_save(&store, c, other, &init).await?;
    let mut rows = vec![];

    let mut w = load(&store, c).await?;
    let registration = fail(
        w.prepare_registration_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            op("broad-register"),
            0,
            EXPIRES,
        )
        .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;
    let q = json!({"kind":"RegistrationPreview","draft_id":registration.draft_id,"context":registration.context,"now":NOW});
    let v = shared_cli(&ns, "user_demo", q, &role, true)?;
    ensure(v["action"] == "REGISTER_SUBSCRIPTION" && snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_registration_confirmation_preview_no_write",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_registration_mock(
            &registration.draft_id,
            &registration.context,
            NOW,
            &mut Gate::yes(),
        )
        .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let id = saved.list_subscriptions_mock()?.items[0]
        .subscription_id
        .clone();

    let mut w = load(&store, c).await?;
    let binding = fail(
        w.prepare_settings_mock(op("broad-settings"), &id, schedule(), NOW, EXPIRES)
            .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"SettingsPreview","context":binding.context,"now":NOW}),
        &role,
        true,
    )?;
    ensure(
        v["before"].is_null()
            && v["after"]["days_before"] == 3
            && snapshot(c, &ns).await? == before,
    )?;
    record(
        &mut rows,
        "shared_read_settings_confirmation_preview_no_write",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_settings_mock(&binding, NOW, &mut Gate::yes())
            .await,
    )?;
    workflow_commit(&store, c, other, w).await?;

    let mut w = load(&store, c).await?;
    let cancel = fail(
        w.prepare_intent_mock(
            op("broad-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"IntentPreview","draft_id":cancel.draft_id,"context":cancel.context,"now":NOW}),
        &role,
        true,
    )?;
    ensure(v["after"] == "CANCELLING" && snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_cancellation_intent_preview_no_merchant_claim_or_write",
        c,
        schema,
        base,
    )
    .await?;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_intent_mock(&cancel.draft_id, &cancel.context, NOW, &mut Gate::yes())
            .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let mut w = load(&store, c).await?;
    let revoke = intent(&mut w, "broad-revoke", &id, MockIntentKind::Revoke).await?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;

    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"ListSubscriptions"}),
        &role,
        true,
    )?;
    ensure(
        v["items"].as_array().is_some_and(|a| a.len() == 1)
            && v["items"][0]["service_display_name"].is_null(),
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_owned_subscription_list_without_internal_references",
        c,
        schema,
        base,
    )
    .await?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"CancellationStatus","subscription_id":id}),
        &role,
        true,
    )?;
    ensure(
        v["current_state"] == "ACTIVE"
            && v["history"].as_array().is_some_and(|a| a.len() == 2)
            && v["merchant_cancellation_confirmed"] == false,
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_current_cancellation_separate_from_both_historical_intents",
        c,
        schema,
        base,
    )
    .await?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"ReminderPlan","subscription_id":id}),
        &role,
        true,
    )?;
    ensure(
        v["schedule"]["days_before"] == 3
            && v["notification_sent"] == false
            && snapshot(c, &ns).await? == before,
    )?;
    record(
        &mut rows,
        "shared_read_saved_reminder_settings_are_not_delivery_result",
        c,
        schema,
        base,
    )
    .await?;

    let requests = [
        OriginalOperationRequestMock::registration(&registration)?,
        OriginalOperationRequestMock::reminder_settings(&binding)?,
        OriginalOperationRequestMock::intent(&cancel)?,
        OriginalOperationRequestMock::intent(&revoke)?,
    ];
    for (r, status) in requests.iter().zip([
        "REGISTRATION_RECORDED",
        "SETTINGS_RECORDED",
        "CANCELLATION_INTENT_RECORDED",
        "CANCELLATION_INTENT_REVOKED",
    ]) {
        ensure(
            shared_cli(&ns, "user_demo", business_original(r), &role, true)?["status"] == status,
        )?;
    }
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_four_original_business_receipts_without_reexecution",
        c,
        schema,
        base,
    )
    .await?;

    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"Initialization","request":init}),
        &role,
        true,
    )?;
    ensure(v["status"] == "INITIALIZATION_RECORDED")?;
    let mut empty = initial(None);
    empty["owner_user_id"] = json!("missing-owner");
    let missing =
        fail(store.prepare_initialization_mock("missing-owner", serde_json::from_value(empty)?))?;
    let v = shared_cli(
        &ns,
        "missing-owner",
        json!({"kind":"Initialization","request":missing}),
        &role,
        true,
    )?;
    ensure(v["status"] == "NEEDS_REVIEW" && snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_initialization_recorded_or_needs_review_never_creates",
        c,
        schema,
        base,
    )
    .await?;

    let mut w = load(&store, c).await?;
    let mut changed = schedule();
    changed.enabled = false;
    let unfinished = fail(
        w.prepare_settings_mock(op("broad-unfinished"), &id, changed, NOW, EXPIRES)
            .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;
    let q = json!({"kind":"PendingConfirmations","now":NOW});
    let v = shared_cli(&ns, "user_demo", q.clone(), &role, true)?;
    ensure(v["items"][0]["state"] == "READY")?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"PendingConfirmations","now":"2026-10-14T12:06:00+08:00"}),
        &role,
        true,
    )?;
    ensure(v["items"][0]["state"] == "EXPIRED" && snapshot(c, &ns).await? == before)?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"SettingsPreview","context":unfinished.context,"now":"2026-10-14T12:06:00+08:00"}),
        &role,
        false,
    )?;
    ensure(v["status"] == "DRAFT_EXPIRED")?;
    let mut w = load(&store, c).await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;
    let v = shared_cli(&ns, "user_demo", q, &role, true)?;
    ensure(v["items"][0]["state"] == "INVALIDATED" && snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_pending_ready_expired_and_invalidated_are_accurate",
        c,
        schema,
        base,
    )
    .await?;

    let mut wrong = serde_json::to_value(&requests[0])?;
    wrong["original_binding"]["context"]["session_ref"] = json!("different-session");
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"Original","request":wrong}),
        &role,
        false,
    )?;
    ensure(v["status"] == "REQUEST_CONFLICT")?;
    let v = shared_cli(
        &ns,
        "missing-owner",
        json!({"kind":"ListSubscriptions"}),
        &role,
        false,
    )?;
    ensure(v["status"] == "UNAVAILABLE")?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"ConfirmSubscription"}),
        &role,
        false,
    )?;
    ensure(v["status"] == "REQUEST_CONFLICT")?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_conflicting_binding_missing_owner_and_write_command_rejected",
        c,
        schema,
        base,
    )
    .await?;

    c.execute(
        format!("REVOKE SELECT ON \"{ns}\".subscription_runtime_records FROM \"{role}\"").as_str(),
    )
    .await?;
    let v = shared_cli(
        &ns,
        "user_demo",
        json!({"kind":"ListSubscriptions"}),
        &role,
        false,
    )?;
    ensure(v["status"] == "UNAVAILABLE")?;
    c.execute(
        format!("GRANT SELECT ON \"{ns}\".subscription_runtime_records TO \"{role}\"").as_str(),
    )
    .await?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "shared_read_select_only_cli_revoked_permission_not_healthy_empty",
        c,
        schema,
        base,
    )
    .await?;

    std::fs::write(
        query_path("restart-shared-business-queries.json")?,
        serde_json::to_vec(
            &json!({"subscription_id":id,"initialization":init,"originals":requests}),
        )?,
    )?;
    let _ = reader;
    record(
        &mut rows,
        "shared_read_restart_seed_keeps_original_queries_and_invalidated_draft",
        c,
        schema,
        base,
    )
    .await?;
    Ok(rows)
}
async fn shared_business_queries_after_restart(c: &mut PgConnection, schema: &str) -> Result<()> {
    let ns = format!("{schema}_qb");
    let role = format!("{schema}_select");
    let original: Value = serde_json::from_slice(&std::fs::read(query_path(
        "restart-shared-business-queries.json",
    )?)?)?;
    let before = snapshot(c, &ns).await?;
    ensure(
        shared_cli(
            &ns,
            "user_demo",
            json!({"kind":"ListSubscriptions"}),
            &role,
            true,
        )?["items"]
            .as_array()
            .is_some_and(|a| a.len() == 1),
    )?;
    ensure(
        shared_cli(
            &ns,
            "user_demo",
            json!({"kind":"CancellationStatus","subscription_id":original["subscription_id"]}),
            &role,
            true,
        )?["history"]
            .as_array()
            .is_some_and(|a| a.len() == 2),
    )?;
    ensure(
        shared_cli(
            &ns,
            "user_demo",
            json!({"kind":"ReminderPlan","subscription_id":original["subscription_id"]}),
            &role,
            true,
        )?["schedule"]["enabled"]
            == true,
    )?;
    ensure(
        shared_cli(
            &ns,
            "user_demo",
            json!({"kind":"PendingConfirmations","now":NOW}),
            &role,
            true,
        )?["items"][0]["state"]
            == "INVALIDATED",
    )?;
    ensure(
        shared_cli(
            &ns,
            "user_demo",
            json!({"kind":"Initialization","request":original["initialization"]}),
            &role,
            true,
        )?["status"]
            == "INITIALIZATION_RECORDED",
    )?;
    for (r, state) in original["originals"]
        .as_array()
        .ok_or("ORIGINALS_MISSING")?
        .iter()
        .zip([
            "REGISTRATION_RECORDED",
            "SETTINGS_RECORDED",
            "CANCELLATION_INTENT_RECORDED",
            "CANCELLATION_INTENT_REVOKED",
        ])
    {
        ensure(
            shared_cli(
                &ns,
                "user_demo",
                json!({"kind":"Original","request":r}),
                &role,
                true,
            )?["status"]
                == state,
        )?;
    }
    ensure(snapshot(c, &ns).await? == before)?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE").as_str())
        .await?;
    Ok(())
}
