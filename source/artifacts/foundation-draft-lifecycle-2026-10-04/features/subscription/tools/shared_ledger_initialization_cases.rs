// First-time fictional owner records, using caller-owned actual transactions.
fn init_request(store: &ProposedLedgerStoreMock) -> Result<LedgerInitializationRequestMock> {
    fail(store.prepare_initialization_mock("user_demo", serde_json::from_value(initial(None))?))
}
async fn clear_init(c: &mut PgConnection, ns: &str) -> Result<()> {
    c.execute(format!("DELETE FROM \"{ns}\".subscription_runtime_records").as_str())
        .await?;
    Ok(())
}
async fn init_save(
    store: &ProposedLedgerStoreMock,
    c: &mut PgConnection,
    other: &mut PgConnection,
    request: &LedgerInitializationRequestMock,
) -> Result<SubscriptionReadSnapshotMock> {
    c.execute("BEGIN").await?;
    let p = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", request)
            .await,
    )?;
    c.execute("COMMIT").await?;
    fail(p.read_committed_on_connection_mock(other).await)
}
async fn initialization_cases(
    c: &mut PgConnection,
    other: &mut PgConnection,
    schema: &str,
    sample: &Sample,
    base: &Value,
) -> Result<Vec<Value>> {
    let ns = format!("{schema}_ini");
    let role = format!("{schema}_select");
    c.execute(format!("CREATE SCHEMA \"{ns}\"").as_str())
        .await?;
    c.execute(ddl(&ns)?.as_str()).await?;
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
    let request = init_request(&store)?;
    let mut rows = vec![];
    ensure(
        store
            .lookup_initialization_on_connection_mock(c, "user_demo", &request)
            .await?
            .is_none(),
    )?;
    ensure(serde_json::to_value(request.preview_mock()?)?["known_empty"] == true)?;
    record(
        &mut rows,
        "initialization_preview_and_missing_lookup_do_not_create_empty_row",
        c,
        schema,
        base,
    )
    .await?;

    let mut bad = serde_json::to_value(&request)?;
    bad["initial"]["read"]["state"] = json!("UNAVAILABLE");
    let bad: LedgerInitializationRequestMock = serde_json::from_value(bad)?;
    ensure(
        store
            .initialize_on_connection_mock(c, "user_demo", &bad)
            .await
            .is_err(),
    )?;
    ensure(
        store
            .lookup_initialization_on_connection_mock(c, "user_demo", &request)
            .await?
            .is_none(),
    )?;
    record(
        &mut rows,
        "initialization_unavailable_input_not_empty_even_when_deserialized",
        c,
        schema,
        base,
    )
    .await?;

    c.execute("BEGIN").await?;
    let p = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
    )?;
    ensure(p.read_committed_on_connection_mock(other).await.is_err())?;
    c.execute("ROLLBACK").await?;
    ensure(
        store
            .lookup_initialization_on_connection_mock(c, "user_demo", &request)
            .await?
            .is_none(),
    )?;
    record(
        &mut rows,
        "initialization_uncommitted_and_rollback_have_no_published_success",
        c,
        schema,
        base,
    )
    .await?;

    let read = init_save(&store, c, other, &request).await?;
    ensure(read.version_mock() == 1 && read.list_subscriptions_mock()?.items.is_empty())?;
    record(
        &mut rows,
        "initialization_complete_known_empty_is_committed_and_read_back",
        c,
        schema,
        base,
    )
    .await?;

    let p = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
    )?;
    ensure(matches!(
        p.read_committed_on_connection_mock(c).await,
        Err(ProposedLedgerWriteIssue::VerificationConnectionRequired)
    ))?;
    ensure(
        p.read_committed_on_connection_mock(other)
            .await?
            .version_mock()
            == 1,
    )?;
    record(
        &mut rows,
        "initialization_same_backend_cannot_publish_success",
        c,
        schema,
        base,
    )
    .await?;

    let before = snapshot(c, &ns).await?;
    let _ = init_save(&store, c, other, &request).await?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_same_request_keeps_version_and_original_data",
        c,
        schema,
        base,
    )
    .await?;

    let different = fail(store.prepare_initialization_mock(
        "user_demo",
        serde_json::from_value(initial(Some("ACTIVE")))?,
    ))?;
    ensure(matches!(
        store
            .initialize_on_connection_mock(c, "user_demo", &different)
            .await,
        Err(ProposedLedgerWriteIssue::Conflict)
    ))?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_different_original_data_cannot_replace_existing_row",
        c,
        schema,
        base,
    )
    .await?;

    ensure(
        store
            .initialize_on_connection_mock(c, "other-owner", &request)
            .await
            .is_err(),
    )?;
    let other_scope = fail(ProposedLedgerStoreMock::new(&format!("{ns}_other")))?;
    ensure(
        other_scope
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await
            .is_err(),
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_wrong_owner_or_namespace_rejected_before_insert",
        c,
        schema,
        base,
    )
    .await?;

    // Preserve missing service fields in known nonempty input; do not invent IDs.
    clear_init(c, &ns).await?;
    let mut partial = initial(Some("ACTIVE"));
    partial["entries"][0]["service_ref"] = Value::Null;
    let partial =
        fail(store.prepare_initialization_mock("user_demo", serde_json::from_value(partial)?))?;
    let saved = init_save(&store, c, other, &partial).await?;
    ensure(saved.list_subscriptions_mock()?.items[0].service_data_state == "NEEDS_REVIEW")?;
    ensure(
        fail(saved.ledger_mock().list_mock("user_demo"))?[0]
            .service_ref
            .is_none(),
    )?;
    record(
        &mut rows,
        "initialization_known_nonempty_missing_service_stays_reviewable",
        c,
        schema,
        base,
    )
    .await?;

    clear_init(c, &ns).await?;
    c.execute("BEGIN").await?;
    other.execute("BEGIN").await?;
    let first = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
    )?;
    let (second, commit) = tokio::join!(
        store.initialize_on_connection_mock(other, "user_demo", &request),
        c.execute("COMMIT")
    );
    commit?;
    let second = fail(second)?;
    other.execute("COMMIT").await?;
    ensure(
        first
            .read_committed_on_connection_mock(other)
            .await?
            .version_mock()
            == 1
            && second
                .read_committed_on_connection_mock(c)
                .await?
                .version_mock()
                == 1,
    )?;
    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM \"{ns}\".subscription_runtime_records"
    ))
    .fetch_one(&mut *c)
    .await?;
    ensure(count == 1)?;
    record(
        &mut rows,
        "initialization_two_transactions_same_input_create_exactly_one_owner_row",
        c,
        schema,
        base,
    )
    .await?;

    clear_init(c, &ns).await?;
    c.execute("BEGIN").await?;
    other.execute("BEGIN").await?;
    let first = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
    )?;
    let (second, commit) = tokio::join!(
        store.initialize_on_connection_mock(other, "user_demo", &different),
        c.execute("COMMIT")
    );
    commit?;
    ensure(matches!(second, Err(ProposedLedgerWriteIssue::Conflict)))?;
    other.execute("ROLLBACK").await?;
    ensure(
        first
            .read_committed_on_connection_mock(other)
            .await?
            .list_subscriptions_mock()?
            .items
            .is_empty(),
    )?;
    record(
        &mut rows,
        "initialization_two_transactions_conflicting_input_keep_winner",
        c,
        schema,
        base,
    )
    .await?;

    clear_init(c, &ns).await?;
    c.execute("BEGIN").await?;
    let _lost = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
    )?;
    c.execute("COMMIT").await?;
    let before = snapshot(c, &ns).await?;
    ensure(
        store
            .lookup_initialization_on_connection_mock(other, "user_demo", &request)
            .await?
            .is_some(),
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_committed_reply_lost_uses_readonly_original_lookup",
        c,
        schema,
        base,
    )
    .await?;

    c.execute(format!("UPDATE \"{ns}\".subscription_runtime_records SET encoding_version='unsupported-fixture-codec'").as_str()).await?;
    let before = snapshot(c, &ns).await?;
    ensure(matches!(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
        Err(ProposedLedgerWriteIssue::Read(
            ProposedLedgerReadIssue::UnsupportedEncoding
        ))
    ))?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_unknown_existing_codec_is_never_overwritten",
        c,
        schema,
        base,
    )
    .await?;

    clear_init(c, &ns).await?;
    let mut malformed = journal(None);
    malformed["unknown_field"] = json!(true);
    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        malformed,
    )
    .await?;
    let before = snapshot(c, &ns).await?;
    ensure(matches!(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
        Err(ProposedLedgerWriteIssue::Read(
            ProposedLedgerReadIssue::InvalidStoredRecord
        ))
    ))?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_damaged_existing_journal_is_not_reset",
        c,
        schema,
        base,
    )
    .await?;

    clear_init(c, &ns).await?;
    init_save(&store, c, other, &request).await?;
    c.execute(format!("GRANT USAGE ON SCHEMA \"{ns}\" TO \"{role}\"; GRANT SELECT ON \"{ns}\".subscription_runtime_records TO \"{role}\"").as_str()).await?;
    let before = snapshot(c, &ns).await?;
    c.execute(format!("SET ROLE \"{role}\"").as_str()).await?;
    ensure(
        store
            .lookup_initialization_on_connection_mock(c, "user_demo", &request)
            .await?
            .is_some(),
    )?;
    let write = store
        .initialize_on_connection_mock(c, "user_demo", &request)
        .await;
    c.execute("RESET ROLE").await?;
    ensure(matches!(write, Err(ProposedLedgerWriteIssue::Unavailable)))?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_select_only_role_can_lookup_but_cannot_insert",
        c,
        schema,
        base,
    )
    .await?;

    let absent = format!("{schema}_abs");
    c.execute(format!("CREATE SCHEMA \"{absent}\"").as_str())
        .await?;
    let absent_store = fail(ProposedLedgerStoreMock::new(&absent))?;
    let absent_request = init_request(&absent_store)?;
    ensure(matches!(
        absent_store
            .initialize_on_connection_mock(c, "user_demo", &absent_request)
            .await,
        Err(ProposedLedgerWriteIssue::Unavailable)
    ))?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname=$1")
        .bind(&absent)
        .fetch_one(&mut *c)
        .await?;
    ensure(count == 0)?;
    c.execute(format!("DROP SCHEMA \"{absent}\"").as_str())
        .await?;
    record(
        &mut rows,
        "initialization_missing_table_is_unavailable_never_automatic_ddl",
        c,
        schema,
        base,
    )
    .await?;

    // Fresh init followed by real module business; retry must retain all of it.
    let mut w = load(&store, c).await?;
    reg(&mut w, sample, "init-register").await?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let id = saved.list_subscriptions_mock()?.items[0]
        .subscription_id
        .clone();
    let mut w = load(&store, c).await?;
    settings(&mut w, "init-reminder", &id, schedule()).await?;
    workflow_commit(&store, c, other, w).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let noop = fail(
        store
            .initialize_on_connection_mock(c, "user_demo", &request)
            .await,
    )?;
    ensure(
        noop.into_committed_reminder_batch_mock(other)
            .await
            .is_err(),
    )?;
    let batch = fail(
        token
            .simulate_on_connection_mock(c, &mut ReminderSink::new(DeliveryOutcome::Unknown))
            .await,
    )?;
    reminder_finish(&store, c, other, batch).await?;
    let mut w = load(&store, c).await?;
    intent(&mut w, "init-cancel", &id, MockIntentKind::Record).await?;
    fail(
        w.prepare_intent_mock(
            op("init-unfinished-revoke"),
            &id,
            MockIntentKind::Revoke,
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let before = snapshot(c, &ns).await?;
    let saved = init_save(&store, c, other, &request).await?;
    ensure(
        saved.list_subscriptions_mock()?.items.len() == 1
            && saved.cancellation_status_mock(&id)?.history.len() == 1,
    )?;
    ensure(
        saved.reminder_plan_mock(&id)?.saved_cycle_outcomes[0].state
            == ReminderAttemptStateMock::NeedsReview,
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    record(
        &mut rows,
        "initialization_retry_preserves_business_pending_and_unknown_without_minting_sender",
        c,
        schema,
        base,
    )
    .await?;
    std::fs::write(
        query_path("restart-initialization.json")?,
        serde_json::to_vec(&request)?,
    )?;
    record(
        &mut rows,
        "initialization_restart_seed_retains_original_request_and_all_later_facts",
        c,
        schema,
        base,
    )
    .await?;
    let _ = reader;
    Ok(rows)
}
async fn initialization_after_restart(
    c: &mut PgConnection,
    other: &mut PgConnection,
    schema: &str,
) -> Result<()> {
    let ns = format!("{schema}_ini");
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let request: LedgerInitializationRequestMock =
        serde_json::from_slice(&std::fs::read(query_path("restart-initialization.json")?)?)?;
    let before = snapshot(c, &ns).await?;
    let saved = fail(
        store
            .lookup_initialization_on_connection_mock(c, "user_demo", &request)
            .await,
    )?
    .ok_or("INITIALIZATION_NOT_RESTORED")?;
    let id = &saved.list_subscriptions_mock()?.items[0].subscription_id;
    ensure(
        saved.cancellation_status_mock(id)?.history.len() == 1
            && !saved
                .cancellation_status_mock(id)?
                .merchant_cancellation_confirmed,
    )?;
    ensure(
        saved.reminder_plan_mock(id)?.saved_cycle_outcomes[0].state
            == ReminderAttemptStateMock::NeedsReview,
    )?;
    ensure(
        saved
            .pending_confirmations_mock(NOW)?
            .items
            .iter()
            .any(|i| i.action == "REVOKE_INTENT" && i.state == "READY"),
    )?;
    ensure(snapshot(c, &ns).await? == before)?;
    let _ = init_save(&store, c, other, &request).await?;
    ensure(snapshot(c, &ns).await? == before)?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE").as_str())
        .await?;
    Ok(())
}
