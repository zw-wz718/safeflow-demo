// Appended to the generated, isolated write acceptance program. Reuses its
// real foundation setup and helpers; not compiled into the business crate.
fn workflow_sample(sample: &Sample) -> Sample {
    let mut all = sample.clone();
    for suffix in ["b", "c"] {
        let mut extra = sample.snapshot.transactions.clone();
        for t in &mut extra {
            t.transaction_id.push_str(&format!("-{suffix}"));
            t.mock_service_ref = Some(format!("fictional-hannah-video-service-{suffix}"));
        }
        all.snapshot.transactions.extend(extra);
    }
    all
}
async fn workflow_commit(
    store: &ProposedLedgerStoreMock,
    c: &mut PgConnection,
    other: &mut PgConnection,
    work: ProposedLedgerWorkMock,
) -> Result<SubscriptionReadSnapshotMock> {
    c.execute("BEGIN").await?;
    let pending = save(store, c, work).await?;
    c.execute("COMMIT").await?;
    fail(pending.read_committed_on_connection_mock(other).await)
}
fn position(read: &SubscriptionReadSnapshotMock, operation: &str) -> Result<usize> {
    read.pending_confirmations_mock(NOW)?
        .items
        .into_iter()
        .find(|i| i.original_context.operation_id == operation)
        .map(|i| i.source_position)
        .ok_or("PENDING_NOT_FOUND".into())
}
async fn workflow_cases(
    c: &mut PgConnection,
    other: &mut PgConnection,
    foundation_schema: &str,
    sample: &Sample,
    foundation_before: &Value,
) -> Result<Vec<Value>> {
    let ns = format!("{foundation_schema}_wf");
    c.execute(format!("CREATE SCHEMA \"{ns}\"").as_str())
        .await?;
    c.execute(ddl(&ns)?.as_str()).await?;
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
    let mut rows = vec![];
    let all = workflow_sample(sample);
    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(None),
    )
    .await?;
    let mut w = load(&store, c).await?;
    let a = fail(
        w.prepare_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            op("wf-abandon-register"),
            0,
            EXPIRES,
        )
        .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let pos = position(&saved, "wf-abandon-register")?;
    let mut w = load(&store, c).await?;
    fail(w.abandon_pending_mock(pos, &a.context, NOW).await)?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    c.execute("ROLLBACK").await?;
    ensure(p.read_committed_on_connection_mock(other).await.is_err())?;
    ensure(
        view(&reader, c)
            .await?
            .pending_confirmations_mock(NOW)?
            .items[0]
            .state
            == "READY",
    )?;
    record(
        &mut rows,
        "workflow_abandon_rollback_keeps_original_draft",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    fail(w.abandon_pending_mock(pos, &a.context, NOW).await)?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.pending_confirmations_mock(NOW)?.items[0].state == "ABANDONED"
            && saved.list_subscriptions_mock()?.items.is_empty(),
    )?;
    record(
        &mut rows,
        "workflow_abandon_committed_without_registration",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    fail(w.abandon_pending_mock(pos, &a.context, EXPIRES).await)?;
    let repeated = workflow_commit(&store, c, other, w).await?;
    ensure(repeated.version_mock() == saved.version_mock())?;
    let mut w = load(&store, c).await?;
    let mut gate = Gate::yes();
    ensure(
        w.confirm_registration_mock(&a.draft_id, &a.context, NOW, &mut gate)
            .await
            .is_err()
            && gate.calls == 0,
    )?;
    ensure(store.save_on_connection_mock(c, w).await.is_err())?;
    record(
        &mut rows,
        "workflow_abandon_replay_and_old_confirmation_rejection",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;

    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(None),
    )
    .await?;
    let mut w = load(&store, c).await?;
    let a = fail(
        w.prepare_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            op("wf-before"),
            0,
            EXPIRES,
        )
        .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let mut next = a.context.clone();
    next.plan_version += 1;
    let mut w = load(&store, c).await?;
    fail(
        w.reselect_registration_mock(
            &a,
            &all.trusted_context_fixture,
            &all.snapshot,
            1,
            next.clone(),
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    c.execute("ROLLBACK").await?;
    ensure(p.read_committed_on_connection_mock(other).await.is_err())?;
    let current = view(&reader, c).await?;
    ensure(
        current.pending_confirmations_mock(NOW)?.items.len() == 1
            && current
                .registration_preview_mock(&a.draft_id, &a.context, NOW)
                .is_ok(),
    )?;
    record(
        &mut rows,
        "workflow_reselection_rollback_preserves_A_confirmation",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let b = fail(
        w.reselect_registration_mock(
            &a,
            &all.trusted_context_fixture,
            &all.snapshot,
            1,
            next,
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    ensure(b.mode == "REPLACE_UNCOMMITTED")?;
    ensure(
        serde_json::to_value(&b)?
            .get("confirmation_binding")
            .is_none(),
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let pending = saved.pending_confirmations_mock(NOW)?;
    ensure(pending.items[0].state == "SUPERSEDED" && pending.items[1].state == "READY")?;
    let mut w = load(&store, c).await?;
    let mut gate = Gate::yes();
    ensure(
        w.confirm_registration_mock(&a.draft_id, &a.context, NOW, &mut gate)
            .await
            .is_err()
            && gate.calls == 0,
    )?;
    ensure(store.save_on_connection_mock(c, w).await.is_err())?;
    record(
        &mut rows,
        "workflow_reselection_before_commit_invalidates_old_A",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let b = b.confirmation_binding;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_registration_mock(&b.draft_id, &b.context, NOW, &mut Gate::yes())
            .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.list_subscriptions_mock()?.items.len() == 1
            && fail(saved.ledger_mock().list_mock("user_demo"))?[0].service_ref
                == b.candidate.mock_service_ref,
    )?;
    record(
        &mut rows,
        "workflow_reselection_saves_only_new_B",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;

    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(None),
    )
    .await?;
    let mut w = load(&store, c).await?;
    let a = reg(&mut w, &all, "wf-saved-A").await?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let req = OriginalOperationRequestMock::registration(&a)?;
    let original = saved.original_result_mock(&req)?;
    let mut w = load(&store, c).await?;
    let b = fail(
        w.reselect_registration_mock(
            &a,
            &all.trusted_context_fixture,
            &all.snapshot,
            1,
            op("wf-new-B"),
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    ensure(b.mode == "NEW_REGISTRATION_KEEP_ORIGINAL")?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.list_subscriptions_mock()?.items.len() == 1
            && saved.original_result_mock(&req)? == original,
    )?;
    record(
        &mut rows,
        "workflow_reselection_after_commit_keeps_saved_A",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let b = b.confirmation_binding;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_registration_mock(&b.draft_id, &b.context, NOW, &mut Gate::yes())
            .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.list_subscriptions_mock()?.items.len() == 2
            && saved.original_result_mock(&req)? == original,
    )?;
    record(
        &mut rows,
        "workflow_new_B_requires_independent_confirmation",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;

    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(None),
    )
    .await?;
    let mut w = load(&store, c).await?;
    let a = reg(&mut w, &all, "wf-unsaved-A").await?;
    ensure(
        w.reselect_registration_mock(
            &a,
            &all.trusted_context_fixture,
            &all.snapshot,
            1,
            op("wf-unsaved-B"),
            NOW,
            EXPIRES,
        )
        .await
        .is_err(),
    )?;
    ensure(
        store.save_on_connection_mock(c, w).await.is_err()
            && view(&reader, c)
                .await?
                .list_subscriptions_mock()?
                .items
                .is_empty(),
    )?;
    record(
        &mut rows,
        "workflow_unsaved_A_not_described_as_committed_reselection",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let a = reg(&mut w, &all, "wf-unsaved-reminder").await?;
    ensure(
        w.prepare_reminder_after_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            &a,
            op("wf-not-yet-saved"),
            NOW,
            EXPIRES,
        )
        .await
        .is_err(),
    )?;
    ensure(store.save_on_connection_mock(c, w).await.is_err())?;
    record(
        &mut rows,
        "workflow_unsaved_registration_cannot_prepare_followup_reminder",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;

    let mut w = load(&store, c).await?;
    let a = reg(&mut w, &all, "wf-reminder-A").await?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let id = saved.list_subscriptions_mock()?.items[0]
        .subscription_id
        .clone();
    let version = saved.version_mock();
    let mut missing = all.snapshot.clone();
    missing.transactions_read.state = safeflow_subscription_evidence::ReadKind::Unavailable;
    missing.transactions_read.is_complete = false;
    let mut w = load(&store, c).await?;
    let needs = fail(
        w.prepare_reminder_after_registration_mock(
            &all.trusted_context_fixture,
            &missing,
            &a,
            op("wf-missing-date"),
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    ensure(needs.state == "NEEDS_INPUT" && needs.confirmation_binding.is_none())?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.version_mock() == version
            && saved.list_subscriptions_mock()?.items.len() == 1
            && saved.reminder_plan_mock(&id)?.schedule.is_none(),
    )?;
    record(
        &mut rows,
        "workflow_missing_date_keeps_registration_without_draft",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let needs = fail(
        w.prepare_reminder_after_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            &a,
            op("wf-expired-date"),
            "2026-10-20T12:00:00+08:00",
            "2026-10-20T12:05:00+08:00",
        )
        .await,
    )?;
    ensure(needs.state == "NEEDS_INPUT")?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(saved.version_mock() == version)?;
    record(
        &mut rows,
        "workflow_past_suggestion_not_advanced_or_saved",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let ready = fail(
        w.prepare_reminder_after_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            &a,
            op("wf-initial-reminder"),
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    ensure(
        ready.state == "AWAITING_CONFIRMATION"
            && !ready.settings_saved
            && serde_json::to_value(&ready)?
                .get("confirmation_binding")
                .is_none(),
    )?;
    let binding = ready.confirmation_binding.ok_or("MISSING_BINDING")?;
    let expected_schedule = binding.schedule.clone();
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.reminder_plan_mock(&id)?.schedule.is_none()
            && saved.settings_preview_mock(&binding.context, NOW).is_ok(),
    )?;
    record(
        &mut rows,
        "workflow_followup_reminder_saved_as_draft_only",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let mut deny = Gate {
        calls: 0,
        outcome: MockGateOutcome::Denied,
    };
    ensure(
        w.confirm_settings_mock(&binding, NOW, &mut deny)
            .await
            .is_err()
            && store.save_on_connection_mock(c, w).await.is_err(),
    )?;
    ensure(
        view(&reader, c)
            .await?
            .reminder_plan_mock(&id)?
            .schedule
            .is_none(),
    )?;
    record(
        &mut rows,
        "workflow_followup_reminder_denial_keeps_registration",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_settings_mock(&binding, NOW, &mut Gate::yes())
            .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(saved.reminder_plan_mock(&id)?.schedule == Some(expected_schedule.clone()))?;
    record(
        &mut rows,
        "workflow_followup_reminder_requires_separate_gate",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    ensure(
        w.prepare_reminder_after_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            &a,
            op("wf-overwrite"),
            NOW,
            EXPIRES,
        )
        .await
        .is_err(),
    )?;
    ensure(store.save_on_connection_mock(c, w).await.is_err())?;
    record(
        &mut rows,
        "workflow_followup_cannot_overwrite_existing_preferences",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;

    let mut disabled = expected_schedule.clone();
    disabled.enabled = false;
    let mut w = load(&store, c).await?;
    let settings = fail(
        w.prepare_settings_mock(op("wf-abandon-settings"), &id, disabled, NOW, EXPIRES)
            .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let pos = position(&saved, "wf-abandon-settings")?;
    let mut w = load(&store, c).await?;
    fail(w.abandon_pending_mock(pos, &settings.context, NOW).await)?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(saved.reminder_plan_mock(&id)?.schedule == Some(expected_schedule.clone()))?;
    let mut w = load(&store, c).await?;
    let mut gate = Gate::yes();
    ensure(
        w.confirm_settings_mock(&settings, NOW, &mut gate)
            .await
            .is_err()
            && gate.calls == 0,
    )?;
    record(
        &mut rows,
        "workflow_abandon_settings_preserves_saved_preference",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    let cancel = fail(
        w.prepare_intent_mock(
            op("wf-abandon-cancel"),
            &id,
            MockIntentKind::Record,
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let pos = position(&saved, "wf-abandon-cancel")?;
    let mut w = load(&store, c).await?;
    fail(w.abandon_pending_mock(pos, &cancel.context, NOW).await)?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(saved.cancellation_status_mock(&id)?.history.is_empty())?;
    record(
        &mut rows,
        "workflow_abandon_cancellation_changes_no_business_state",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    let mut w = load(&store, c).await?;
    intent(&mut w, "wf-actual-cancel", &id, MockIntentKind::Record).await?;
    workflow_commit(&store, c, other, w).await?;
    let mut w = load(&store, c).await?;
    let revoke = fail(
        w.prepare_intent_mock(
            op("wf-abandon-revoke"),
            &id,
            MockIntentKind::Revoke,
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let pos = position(&saved, "wf-abandon-revoke")?;
    let mut w = load(&store, c).await?;
    fail(w.abandon_pending_mock(pos, &revoke.context, NOW).await)?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.cancellation_status_mock(&id)?.history.len() == 1
            && !saved
                .cancellation_status_mock(&id)?
                .merchant_cancellation_confirmed,
    )?;
    record(
        &mut rows,
        "workflow_abandon_revoke_preserves_prior_cancellation",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;

    // Persist all three new workflows for a second process after real restart.
    let mut w = load(&store, c).await?;
    let b = fail(
        w.reselect_registration_mock(
            &a,
            &all.trusted_context_fixture,
            &all.snapshot,
            1,
            op("wf-restart-B"),
            NOW,
            EXPIRES,
        )
        .await,
    )?
    .confirmation_binding;
    workflow_commit(&store, c, other, w).await?;
    let mut w = load(&store, c).await?;
    fail(
        w.confirm_registration_mock(&b.draft_id, &b.context, NOW, &mut Gate::yes())
            .await,
    )?;
    workflow_commit(&store, c, other, w).await?;
    let mut w = load(&store, c).await?;
    fail(
        w.prepare_reminder_after_registration_mock(
            &all.trusted_context_fixture,
            &all.snapshot,
            &b,
            op("wf-restart-reminder"),
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    fail(
        w.reselect_registration_mock(
            &a,
            &all.trusted_context_fixture,
            &all.snapshot,
            2,
            op("wf-restart-C"),
            NOW,
            EXPIRES,
        )
        .await,
    )?;
    let saved = workflow_commit(&store, c, other, w).await?;
    ensure(
        saved.list_subscriptions_mock()?.items.len() == 2
            && saved
                .pending_confirmations_mock(NOW)?
                .items
                .iter()
                .filter(|i| i.state == "ABANDONED")
                .count()
                == 3,
    )?;
    record(
        &mut rows,
        "workflow_restart_seed_preserves_abandonment_selection_and_reminder_draft",
        c,
        foundation_schema,
        foundation_before,
    )
    .await?;
    Ok(rows)
}
async fn workflow_after_restart(
    c: &mut PgConnection,
    other: &mut PgConnection,
    foundation_schema: &str,
) -> Result<()> {
    let ns = format!("{foundation_schema}_wf");
    let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
    let saved = view(&reader, c).await?;
    ensure(saved.list_subscriptions_mock()?.items.len() == 2)?;
    let items = fail(saved.ledger_mock().list_mock("user_demo"))?;
    let a = items
        .iter()
        .find(|e| e.service_ref.as_deref() == Some("fictional-hannah-video-service"))
        .ok_or("A_NOT_RESTORED")?;
    let b = items
        .iter()
        .find(|e| e.service_ref.as_deref() == Some("fictional-hannah-video-service-b"))
        .ok_or("B_NOT_RESTORED")?;
    let original_records = serde_json::to_value(saved.business_records_mock())?;
    let confirmed = original_records
        .as_array()
        .ok_or("RECORD_ARRAY_REQUIRED")?
        .iter()
        .find(|r| r["original_binding"]["context"]["operation_id"] == "wf-initial-reminder")
        .ok_or("ORIGINAL_SETTINGS_MISSING")?;
    let restored = saved
        .reminder_plan_mock(&a.subscription_id)?
        .schedule
        .ok_or("SCHEDULE_NOT_RESTORED")?;
    ensure(serde_json::to_value(restored)? == confirmed["original_binding"]["schedule"])?;
    ensure(
        saved
            .cancellation_status_mock(&a.subscription_id)?
            .history
            .len()
            == 1
            && saved
                .reminder_plan_mock(&a.subscription_id)?
                .schedule
                .is_some()
            && saved
                .reminder_plan_mock(&b.subscription_id)?
                .schedule
                .is_none(),
    )?;
    let pending = saved.pending_confirmations_mock(NOW)?;
    ensure(
        pending
            .items
            .iter()
            .filter(|i| i.state == "ABANDONED")
            .count()
            == 3
            && pending.items.iter().filter(|i| i.state == "READY").count() == 2,
    )?;
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let mut w = load(&store, c).await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    let saved = workflow_commit(&store, c, other, w).await?;
    let pending = saved.pending_confirmations_mock(NOW)?;
    ensure(
        pending
            .items
            .iter()
            .filter(|i| i.state == "ABANDONED")
            .count()
            == 3
            && pending
                .items
                .iter()
                .filter(|i| i.state == "INVALIDATED")
                .count()
                == 2
            && saved.list_subscriptions_mock()?.items.len() == 2,
    )?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE").as_str())
        .await?;
    Ok(())
}
