// Isolated read/query acceptance. All records, roles and identities fictional.
fn query_path(name: &str) -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(std::env::var("SAFEFLOW_SUBSCRIPTION_QUERY_DIR")?).join(name))
}
fn original_query(request: &OriginalReminderRequestMock) -> Value {
    json!({"kind":"OriginalReminder","request":request})
}
fn query_cli(schema: &str, owner: &str, query: Value, role: &str, success: bool) -> Result<Value> {
    let file = query_path("query-input.json")?;
    std::fs::write(&file, serde_json::to_vec(&query)?)?;
    let options =
        PgConnectOptions::from_str(&std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL")?)?;
    let password = std::env::var("POSTGRES_PASSWORD")?;
    ensure(password.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let url = format!(
        "postgres://{role}:{password}@127.0.0.1:{}/safeflow_subscription_write_test",
        options.get_port()
    );
    let out = std::process::Command::new(std::env::var("SAFEFLOW_SUBSCRIPTION_QUERY_EXE")?)
        .args(["--run-fictional", schema, owner])
        .arg(&file)
        .env("SAFEFLOW_SUBSCRIPTION_SHARED_QUERY_URL", url)
        .output()?;
    ensure(out.status.success() == success)?;
    let view: Value = serde_json::from_slice(&out.stdout)?;
    let text = view.to_string();
    for hidden in [
        "user_demo",
        "fictional-session",
        "fictional-key",
        "source_position",
        "cycle_ref",
        "subscription_id",
        "journal",
        "postgres://",
    ] {
        ensure(!text.contains(hidden))?;
    }
    if success {
        ensure(view["notification_sent"] == false && view["fictional"] == true)?;
    }
    Ok(view)
}
async fn reminder_query_cases(
    c: &mut PgConnection,
    other: &mut PgConnection,
    schema: &str,
    base: &Value,
) -> Result<Vec<Value>> {
    let ns = format!("{schema}_rq");
    let role = format!("{schema}_select");
    c.execute(format!("CREATE SCHEMA \"{ns}\"").as_str())
        .await?;
    c.execute(ddl(&ns)?.as_str()).await?;
    let password = std::env::var("POSTGRES_PASSWORD")?;
    ensure(password.bytes().all(|b| b.is_ascii_hexdigit()))?;
    c.execute(format!("ALTER ROLE \"{role}\" LOGIN PASSWORD '{password}'; GRANT USAGE ON SCHEMA \"{ns}\" TO \"{role}\"; GRANT SELECT ON \"{ns}\".subscription_runtime_records TO \"{role}\"").as_str()).await?;
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
    let mut rows = vec![];
    reminder_seed(c, other, &ns, true).await?;
    let before = snapshot(c, &ns).await?;
    let due = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"DueReminders","now":NOW}),
        &role,
        true,
    )?;
    ensure(due["items"][0]["status"] == "PLANNED" && snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "reminder_query_cli_due_is_planned_and_readonly",
        c,
        schema,
        base,
    )
    .await?;

    let token = reminder_reserve(&store, c, other, NOW).await?;
    let saved = view(&reader, c).await?;
    let history = fail(saved.reminder_history_mock())?;
    let position = history.last().ok_or("HISTORY_MISSING")?.source_position;
    let request = fail(saved.original_reminder_request_mock(position))?;
    ensure(
        fail(saved.original_reminder_result_mock(&request))?.batch_state
            == ReminderBatchStateMock::AwaitingResult,
    )?;
    let before = snapshot(c, &ns).await?;
    let review = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"ReminderReviews"}),
        &role,
        true,
    )?;
    let original = query_cli(&ns, "user_demo", original_query(&request), &role, true)?;
    ensure(
        review["items"][0]["status"] == "NEEDS_REVIEW"
            && original["items"][0]["status"] == "NEEDS_REVIEW",
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "reminder_query_pending_original_and_review_without_write",
        c,
        schema,
        base,
    )
    .await?;

    let batch = fail(
        token
            .simulate_on_connection_mock(
                c,
                &mut ReminderSink::new(DeliveryOutcome::SimulatedSuccess),
            )
            .await,
    )?;
    reminder_finish(&store, c, other, batch).await?;
    let before = snapshot(c, &ns).await?;
    let original = query_cli(&ns, "user_demo", original_query(&request), &role, true)?;
    ensure(original["items"][0]["status"] == "SIMULATED_SUCCESS")?;
    let all = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"ReminderHistory"}),
        &role,
        true,
    )?;
    let reviews = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"ReminderReviews"}),
        &role,
        true,
    )?;
    ensure(
        all["items"][0]["status"] == "SIMULATED_SUCCESS"
            && reviews["items"].as_array().is_some_and(|a| a.is_empty()),
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "reminder_query_exact_saved_success_and_safe_history",
        c,
        schema,
        base,
    )
    .await?;

    let mut wrong = serde_json::to_value(&request)?;
    wrong["owner"] = json!("different-owner");
    let v = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"OriginalReminder","request":wrong}),
        &role,
        false,
    )?;
    ensure(v["status"] == "REQUEST_CONFLICT")?;
    record(
        &mut rows,
        "reminder_query_other_owner_original_rejected",
        c,
        schema,
        base,
    )
    .await?;

    let mut wrong = serde_json::to_value(&request)?;
    wrong["prefix"]["initial"]["snapshot_id"] = json!("different-prefix");
    let v = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"OriginalReminder","request":wrong}),
        &role,
        false,
    )?;
    ensure(v["status"] == "REQUEST_CONFLICT")?;
    record(
        &mut rows,
        "reminder_query_changed_prefix_rejected",
        c,
        schema,
        base,
    )
    .await?;

    let v = query_cli(
        &ns,
        "missing-owner",
        json!({"kind":"ReminderHistory"}),
        &role,
        false,
    )?;
    ensure(v["status"] == "UNAVAILABLE")?;
    record(
        &mut rows,
        "reminder_query_missing_ledger_not_empty_success",
        c,
        schema,
        base,
    )
    .await?;

    c.execute(
        format!("REVOKE SELECT ON \"{ns}\".subscription_runtime_records FROM \"{role}\"").as_str(),
    )
    .await?;
    let v = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"ReminderReviews"}),
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
        "reminder_query_permission_failure_not_empty_or_success",
        c,
        schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let batch = fail(
        token
            .simulate_on_connection_mock(c, &mut ReminderSink::new(DeliveryOutcome::Unknown))
            .await,
    )?;
    let saved = reminder_finish(&store, c, other, batch).await?;
    ensure(saved.reminders_needing_review_mock()?[0].reason == "RESULT_UNKNOWN")?;
    let v = query_cli(
        &ns,
        "user_demo",
        json!({"kind":"ReminderReviews"}),
        &role,
        true,
    )?;
    ensure(v["items"][0]["status"] == "NEEDS_REVIEW")?;
    record(
        &mut rows,
        "reminder_query_recorded_unknown_stays_review_not_delivered",
        c,
        schema,
        base,
    )
    .await?;

    let position = saved.reminder_history_mock()?[0].source_position;
    let original = saved.original_reminder_request_mock(position)?;
    let mut w = load(&store, c).await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    workflow_commit(&store, c, other, w).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let batch = fail(
        token
            .simulate_on_connection_mock(
                c,
                &mut ReminderSink::new(DeliveryOutcome::SimulatedSuccess),
            )
            .await,
    )?;
    let saved = reminder_finish(&store, c, other, batch).await?;
    ensure(
        saved.original_reminder_result_mock(&original)?.items[0].state
            == ReminderAttemptStateMock::NeedsReview,
    )?;
    let v = query_cli(&ns, "user_demo", original_query(&original), &role, true)?;
    ensure(v["items"][0]["status"] == "NEEDS_REVIEW")?;
    record(
        &mut rows,
        "reminder_query_later_empty_batch_cannot_rewrite_old_unknown",
        c,
        schema,
        base,
    )
    .await?;

    let mut restart_requests = Map::new();
    for suffix in ["s", "f", "u"] {
        let target = format!("{schema}_r{suffix}");
        let reader = fail(ProposedLedgerReaderMock::new(&target))?;
        c.execute(format!("GRANT USAGE ON SCHEMA \"{target}\" TO \"{role}\"; GRANT SELECT ON \"{target}\".subscription_runtime_records TO \"{role}\"").as_str()).await?;
        let saved = view(&reader, c).await?;
        let position = saved.reminder_history_mock()?[0].source_position;
        restart_requests.insert(
            suffix.into(),
            serde_json::to_value(saved.original_reminder_request_mock(position)?)?,
        );
    }
    std::fs::write(
        query_path("restart-reminder-requests.json")?,
        serde_json::to_vec(&restart_requests)?,
    )?;
    record(
        &mut rows,
        "reminder_query_original_request_saved_for_new_process_readback",
        c,
        schema,
        base,
    )
    .await?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE").as_str())
        .await?;
    Ok(rows)
}
async fn reminder_queries_after_restart(c: &mut PgConnection, schema: &str) -> Result<()> {
    let requests: Value = serde_json::from_slice(&std::fs::read(query_path(
        "restart-reminder-requests.json",
    )?)?)?;
    let role = format!("{schema}_select");
    for (suffix, state) in [
        ("s", "SIMULATED_SUCCESS"),
        ("f", "FAILED"),
        ("u", "NEEDS_REVIEW"),
    ] {
        let ns = format!("{schema}_r{suffix}");
        let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
        let before = snapshot(c, &ns).await?;
        let request: OriginalReminderRequestMock =
            serde_json::from_value(requests[suffix].clone())?;
        let saved = view(&reader, c).await?;
        ensure(saved.original_reminder_result_mock(&request)?.items.len() == 1)?;
        let view = query_cli(&ns, "user_demo", original_query(&request), &role, true)?;
        ensure(view["items"][0]["status"] == state && snapshot(c, &ns).await? == before)?;
    }
    Ok(())
}
