// Isolated actual PostgreSQL acceptance, appended after existing write/workflow
// probes. All business dates, identity, services and sends are fictional.
struct ReminderSink {
    outcome: DeliveryOutcome,
    messages: Vec<ReminderMessage>,
}
impl ReminderSink {
    fn new(outcome: DeliveryOutcome) -> Self {
        Self {
            outcome,
            messages: vec![],
        }
    }
}
impl MockInAppSink for ReminderSink {
    fn simulate_in_app(&mut self, m: &ReminderMessage) -> DeliveryOutcome {
        self.messages.push(m.clone());
        self.outcome
    }
}
async fn reminder_seed(
    c: &mut PgConnection,
    other: &mut PgConnection,
    ns: &str,
    enabled: bool,
) -> Result<()> {
    seed(
        c,
        ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(Some("ACTIVE")),
    )
    .await?;
    let store = fail(ProposedLedgerStoreMock::new(ns))?;
    let mut w = load(&store, c).await?;
    let mut s = schedule();
    s.enabled = enabled;
    settings(&mut w, "reminder-settings", "fictional-subscription-1", s).await?;
    workflow_commit(&store, c, other, w).await?;
    Ok(())
}
async fn reminder_reserve(
    store: &ProposedLedgerStoreMock,
    c: &mut PgConnection,
    other: &mut PgConnection,
    now: &str,
) -> Result<CommittedReminderBatchMock> {
    let mut w = load(store, c).await?;
    fail(w.reserve_reminders_mock(now).await)?;
    c.execute("BEGIN").await?;
    let p = save(store, c, w).await?;
    c.execute("COMMIT").await?;
    fail(p.into_committed_reminder_batch_mock(other).await)
}
async fn reminder_finish(
    store: &ProposedLedgerStoreMock,
    c: &mut PgConnection,
    other: &mut PgConnection,
    batch: SimulatedReminderBatchMock,
) -> Result<SubscriptionReadSnapshotMock> {
    let mut w = load(store, c).await?;
    fail(w.record_reminder_results_mock(batch).await)?;
    workflow_commit(store, c, other, w).await
}
fn reminder_state(s: &SubscriptionReadSnapshotMock) -> Result<ReminderAttemptStateMock> {
    s.reminder_plan_mock("fictional-subscription-1")?
        .saved_cycle_outcomes
        .first()
        .map(|a| a.state)
        .ok_or("REMINDER_ATTEMPT_MISSING".into())
}
async fn reminder_cases(
    c: &mut PgConnection,
    other: &mut PgConnection,
    foundation_schema: &str,
    base: &Value,
) -> Result<Vec<Value>> {
    let ns = format!("{foundation_schema}_rm");
    c.execute(format!("CREATE SCHEMA \"{ns}\"").as_str())
        .await?;
    c.execute(ddl(&ns)?.as_str()).await?;
    let store = fail(ProposedLedgerStoreMock::new(&ns))?;
    let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
    let mut rows = vec![];
    reminder_seed(c, other, &ns, true).await?;
    let mut w = load(&store, c).await?;
    let mut later = schedule();
    later.days_before = 2;
    settings(&mut w, "later-window", "fictional-subscription-1", later).await?;
    workflow_commit(&store, c, other, w).await?;
    let original = snapshot(c, &ns).await?;
    ensure(
        view(&reader, c)
            .await?
            .due_reminders_mock(NOW)?
            .messages
            .is_empty(),
    )?;
    ensure(snapshot(c, &ns).await? == original)?;
    record(
        &mut rows,
        "reminder_preview_before_window",
        c,
        foundation_schema,
        base,
    )
    .await?;
    reminder_seed(c, other, &ns, true).await?;
    let original = snapshot(c, &ns).await?;
    for (name, now, count) in [
        ("reminder_preview_due_window", NOW, 1),
        (
            "reminder_preview_inside_window",
            "2026-10-16T12:00:00+08:00",
            1,
        ),
        (
            "reminder_preview_charge_date",
            "2026-10-17T12:00:00+08:00",
            1,
        ),
        (
            "reminder_preview_no_expired_backlog",
            "2026-10-18T12:00:00+08:00",
            0,
        ),
    ] {
        let saved = view(&reader, c).await?;
        let a = fail(saved.due_reminders_mock(now))?;
        ensure(
            a.messages.len() == count
                && !a.reserved
                && !a.notification_sent
                && !a.report.notification_sent,
        )?;
        ensure(snapshot(c, &ns).await? == original)?;
        record(&mut rows, name, c, foundation_schema, base).await?;
    }
    reminder_seed(c, other, &ns, false).await?;
    ensure(
        view(&reader, c)
            .await?
            .due_reminders_mock(NOW)?
            .messages
            .is_empty(),
    )?;
    record(
        &mut rows,
        "reminder_disabled_not_due",
        c,
        foundation_schema,
        base,
    )
    .await?;
    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(Some("ACTIVE")),
    )
    .await?;
    ensure(
        view(&reader, c)
            .await?
            .due_reminders_mock(NOW)?
            .messages
            .is_empty(),
    )?;
    record(
        &mut rows,
        "reminder_missing_date_not_invented",
        c,
        foundation_schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let mut w = load(&store, c).await?;
    intent(
        &mut w,
        "reminder-cancel",
        "fictional-subscription-1",
        MockIntentKind::Record,
    )
    .await?;
    workflow_commit(&store, c, other, w).await?;
    let saved = view(&reader, c).await?;
    ensure(saved.due_reminders_mock(NOW)?.messages.is_empty())?;
    let due = saved.due_reminders_mock("2026-10-17T12:00:00+08:00")?;
    ensure(
        due.messages.len() == 1
            && due.messages[0].kind == ReminderKind::CancellationCheck
            && !saved
                .cancellation_status_mock("fictional-subscription-1")?
                .merchant_cancellation_confirmed,
    )?;
    record(
        &mut rows,
        "reminder_cancellation_check_on_expected_day_only",
        c,
        foundation_schema,
        base,
    )
    .await?;
    let late = saved.due_reminders_mock("2026-10-19T12:00:00+08:00")?;
    ensure(late.messages.len() == 1 && late.messages[0].kind == ReminderKind::CancellationCheck)?;
    record(
        &mut rows,
        "reminder_cancellation_late_check_preserves_original_date",
        c,
        foundation_schema,
        base,
    )
    .await?;

    seed(
        c,
        &ns,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(Some("ACTIVE")),
    )
    .await?;
    let mut w = load(&store, c).await?;
    let daily = Schedule {
        cycle: safeflow_subscription_evidence::recognition::Cycle::Day,
        anchor_date: "2026-10-13".parse()?,
        period_index: 1,
        expected_date: "2026-10-14".parse()?,
        enabled: true,
        days_before: 0,
    };
    settings(&mut w, "daily-reminder", "fictional-subscription-1", daily).await?;
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
    let read = reminder_finish(&store, c, other, batch).await?;
    ensure(read.due_reminders_mock(NOW)?.messages.is_empty())?;
    let tomorrow = read.due_reminders_mock("2026-10-15T12:00:00+08:00")?;
    ensure(
        tomorrow.messages.len() == 1
            && tomorrow.messages[0].expected_date.to_string() == "2026-10-15",
    )?;
    record(
        &mut rows,
        "reminder_daily_today_only_and_next_cycle_independent",
        c,
        foundation_schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let mut w = load(&store, c).await?;
    fail(w.reserve_reminders_mock(NOW).await)?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    ensure(p.into_committed_reminder_batch_mock(other).await.is_err())?;
    c.execute("ROLLBACK").await?;
    ensure(
        view(&reader, c)
            .await?
            .due_reminders_mock(NOW)?
            .messages
            .len()
            == 1,
    )?;
    record(
        &mut rows,
        "reminder_uncommitted_reservation_has_no_token",
        c,
        foundation_schema,
        base,
    )
    .await?;

    let mut w = load(&store, c).await?;
    fail(w.reserve_reminders_mock(NOW).await)?;
    c.execute("BEGIN").await?;
    let p = save(&store, c, w).await?;
    c.execute("ROLLBACK").await?;
    ensure(p.into_committed_reminder_batch_mock(other).await.is_err())?;
    record(
        &mut rows,
        "reminder_rollback_has_no_token_or_attempt",
        c,
        foundation_schema,
        base,
    )
    .await?;

    let mut w = load(&store, c).await?;
    fail(w.reserve_reminders_mock(NOW).await)?;
    let p = save(&store, c, w).await?;
    ensure(matches!(
        p.into_committed_reminder_batch_mock(c).await,
        Err(ProposedLedgerWriteIssue::VerificationConnectionRequired)
    ))?;
    record(
        &mut rows,
        "reminder_same_backend_cannot_mint_token",
        c,
        foundation_schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let mut winner = load(&store, c).await?;
    let mut loser = load(&store, other).await?;
    fail(winner.reserve_reminders_mock(NOW).await)?;
    fail(loser.reserve_reminders_mock(NOW).await)?;
    let p = save(&store, c, winner).await?;
    ensure(matches!(
        store.save_on_connection_mock(other, loser).await,
        Err(ProposedLedgerWriteIssue::Conflict)
    ))?;
    let token = fail(p.into_committed_reminder_batch_mock(other).await)?;
    // A no-op save of the already reserved row must not mint another capability.
    let unchanged = load(&store, c).await?;
    let copy = save(&store, c, unchanged).await?;
    ensure(
        copy.into_committed_reminder_batch_mock(other)
            .await
            .is_err(),
    )?;
    record(
        &mut rows,
        "reminder_noop_resave_cannot_mint_second_token",
        c,
        foundation_schema,
        base,
    )
    .await?;
    let mut sink = ReminderSink::new(DeliveryOutcome::SimulatedSuccess);
    let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
    ensure(sink.messages.len() == 1)?;
    let read = reminder_finish(&store, c, other, batch).await?;
    ensure(reminder_state(&read)? == ReminderAttemptStateMock::SimulatedSuccess)?;
    ensure(read.due_reminders_mock(NOW)?.messages.is_empty())?;
    record(
        &mut rows,
        "reminder_two_workers_only_one_reservation_and_mock_call",
        c,
        foundation_schema,
        base,
    )
    .await?;

    for (name, outcome, expected) in [
        (
            "reminder_saved_simulated_success",
            DeliveryOutcome::SimulatedSuccess,
            ReminderAttemptStateMock::SimulatedSuccess,
        ),
        (
            "reminder_saved_definite_failure",
            DeliveryOutcome::DefiniteFailure,
            ReminderAttemptStateMock::Failed,
        ),
        (
            "reminder_saved_unknown_no_automatic_retry",
            DeliveryOutcome::Unknown,
            ReminderAttemptStateMock::NeedsReview,
        ),
    ] {
        reminder_seed(c, other, &ns, true).await?;
        let token = reminder_reserve(&store, c, other, NOW).await?;
        ensure(reminder_state(&view(&reader, c).await?)? == ReminderAttemptStateMock::NeedsReview)?;
        let mut sink = ReminderSink::new(outcome);
        let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
        ensure(sink.messages.len() == 1)?;
        let read = reminder_finish(&store, c, other, batch).await?;
        ensure(
            reminder_state(&read)? == expected
                && !read
                    .reminder_plan_mock("fictional-subscription-1")?
                    .notification_sent,
        )?;
        ensure(read.due_reminders_mock(NOW)?.messages.is_empty())?;
        record(&mut rows, name, c, foundation_schema, base).await?;
        if outcome == DeliveryOutcome::DefiniteFailure {
            let next = "2026-10-15T12:00:00+08:00";
            ensure(read.due_reminders_mock(next)?.messages.len() == 1)?;
            sink.outcome = DeliveryOutcome::SimulatedSuccess;
            let token = reminder_reserve(&store, c, other, next).await?;
            let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
            reminder_finish(&store, c, other, batch).await?;
            ensure(
                sink.messages.len() == 2
                    && view(&reader, c)
                        .await?
                        .due_reminders_mock("2026-10-16T12:00:00+08:00")?
                        .messages
                        .is_empty(),
            )?;
            record(
                &mut rows,
                "reminder_window_retry_success_stops_duplicates",
                c,
                foundation_schema,
                base,
            )
            .await?;
        }
    }

    reminder_seed(c, other, &ns, true).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let mut w = load(&store, c).await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    workflow_commit(&store, c, other, w).await?;
    let mut sink = ReminderSink::new(DeliveryOutcome::SimulatedSuccess);
    ensure(
        token
            .simulate_on_connection_mock(c, &mut sink)
            .await
            .is_err()
            && sink.messages.is_empty(),
    )?;
    ensure(
        view(&reader, c)
            .await?
            .due_reminders_mock("2026-10-15T12:00:00+08:00")?
            .messages
            .is_empty(),
    )?;
    record(
        &mut rows,
        "reminder_restart_invalidates_old_token_before_mock_call",
        c,
        foundation_schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
    let mut w = load(&store, c).await?;
    fail(w.record_reminder_results_mock(batch).await)?;
    c.execute("BEGIN").await?;
    let pending = save(&store, c, w).await?;
    ensure(
        pending
            .read_committed_on_connection_mock(other)
            .await
            .is_err(),
    )?;
    c.execute("ROLLBACK").await?;
    ensure(reminder_state(&view(&reader, c).await?)? == ReminderAttemptStateMock::NeedsReview)?;
    record(
        &mut rows,
        "reminder_result_rollback_preserves_unknown",
        c,
        foundation_schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
    let mut w = load(&store, c).await?;
    fail(w.invalidate_pending_for_restart_mock().await)?;
    workflow_commit(&store, c, other, w).await?;
    let mut w = load(&store, c).await?;
    ensure(w.record_reminder_results_mock(batch).await.is_err())?;
    ensure(matches!(
        store.save_on_connection_mock(c, w).await,
        Err(ProposedLedgerWriteIssue::Poisoned)
    ))?;
    record(
        &mut rows,
        "reminder_late_result_rejected_after_restart",
        c,
        foundation_schema,
        base,
    )
    .await?;

    reminder_seed(c, other, &ns, true).await?;
    let token = reminder_reserve(&store, c, other, NOW).await?;
    let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
    let mut w = load(&store, c).await?;
    fail(w.record_reminder_results_mock(batch).await)?;
    c.execute("BEGIN").await?;
    let _lost = save(&store, c, w).await?;
    c.execute("COMMIT").await?;
    // Deliberately drop completion instead of publishing a successful reply.
    ensure(
        reminder_state(&view(&reader, other).await?)? == ReminderAttemptStateMock::SimulatedSuccess,
    )?;
    ensure(
        view(&reader, other)
            .await?
            .due_reminders_mock(NOW)?
            .messages
            .is_empty(),
    )?;
    record(
        &mut rows,
        "reminder_committed_result_reply_lost_read_original_no_resend",
        c,
        foundation_schema,
        base,
    )
    .await?;

    // Three extra schemas persist across an actual container restart. Unknown
    // deliberately has only a reservation: crash occurred before mock completion.
    for (suffix, outcome) in [
        ("s", Some(DeliveryOutcome::SimulatedSuccess)),
        ("f", Some(DeliveryOutcome::DefiniteFailure)),
        ("u", None),
    ] {
        let ns = format!("{foundation_schema}_r{suffix}");
        c.execute(format!("CREATE SCHEMA \"{ns}\"").as_str())
            .await?;
        c.execute(ddl(&ns)?.as_str()).await?;
        reminder_seed(c, other, &ns, true).await?;
        let store = fail(ProposedLedgerStoreMock::new(&ns))?;
        let token = reminder_reserve(&store, c, other, NOW).await?;
        if let Some(outcome) = outcome {
            let batch = fail(
                token
                    .simulate_on_connection_mock(c, &mut ReminderSink::new(outcome))
                    .await,
            )?;
            reminder_finish(&store, c, other, batch).await?;
        }
    }
    record(
        &mut rows,
        "reminder_restart_seeds_success_failure_and_unresolved_reservation",
        c,
        foundation_schema,
        base,
    )
    .await?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE").as_str())
        .await?;
    Ok(rows)
}
async fn reminders_after_restart(
    c: &mut PgConnection,
    other: &mut PgConnection,
    schema: &str,
) -> Result<()> {
    for (suffix, expected) in [
        ("s", ReminderAttemptStateMock::SimulatedSuccess),
        ("f", ReminderAttemptStateMock::Failed),
        ("u", ReminderAttemptStateMock::NeedsReview),
    ] {
        let ns = format!("{schema}_r{suffix}");
        let reader = fail(ProposedLedgerReaderMock::new(&ns))?;
        let read = view(&reader, c).await?;
        ensure(reminder_state(&read)? == expected)?;
        ensure(read.due_reminders_mock(NOW)?.messages.is_empty())?;
        let store = fail(ProposedLedgerStoreMock::new(&ns))?;
        let mut w = load(&store, c).await?;
        fail(w.invalidate_pending_for_restart_mock().await)?;
        let read = workflow_commit(&store, c, other, w).await?;
        ensure(reminder_state(&read)? == expected)?;
        ensure(
            read.due_reminders_mock("2026-10-15T12:00:00+08:00")?
                .messages
                .len()
                == usize::from(expected == ReminderAttemptStateMock::Failed),
        )?;
        if expected == ReminderAttemptStateMock::Failed {
            let mut sink = ReminderSink::new(DeliveryOutcome::SimulatedSuccess);
            let token = reminder_reserve(&store, c, other, "2026-10-15T12:00:00+08:00").await?;
            let batch = fail(token.simulate_on_connection_mock(c, &mut sink).await)?;
            let read = reminder_finish(&store, c, other, batch).await?;
            ensure(
                sink.messages.len() == 1
                    && reminder_state(&read)? == ReminderAttemptStateMock::SimulatedSuccess,
            )?;
        }
        c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE").as_str())
            .await?;
    }
    Ok(())
}
