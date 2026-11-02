// Real PostgreSQL acceptance of daily timing, retry budget and historical replay.
async fn limits_attempt(store:&ProposedLedgerStoreMock,c:&mut PgConnection,other:&mut PgConnection,now:&str,outcome:DeliveryOutcome)->Result<SubscriptionReadSnapshotMock> {
    let batch=reminder_reserve(store,c,other,now).await?;let mut sink=ReminderSink::new(outcome);let done=fail(batch.simulate_on_connection_mock(other,&mut sink).await)?;reminder_finish(store,c,other,done).await
}
async fn limits_cases(c:&mut PgConnection,other:&mut PgConnection,schema:&str,base:&Value)->Result<Vec<Value>> {
    let ns=format!("{schema}_lim");let mut rows=vec![];
    for suffix in ["","_daily","_cycle","_cancel","_unknown","_legacy","_rollback","_bad"] {
        let name=format!("{ns}{suffix}");c.execute(format!("CREATE SCHEMA \"{name}\"").as_str()).await?;c.execute(ddl(&name)?.as_str()).await?;
    }
    let daily_ns=format!("{ns}_daily");seed(c,&daily_ns,"user_demo",FICTIONAL_JOURNAL_ENCODING_V1,journal(Some("ACTIVE"))).await?;let daily=fail(ProposedLedgerStoreMock::new(&daily_ns))?;
    let mut s=schedule();s.cycle=safeflow_subscription_evidence::recognition::Cycle::Day;s.anchor_date="2026-10-16".parse()?;s.expected_date="2026-10-17".parse()?;s.days_before=1;
    let mut w=load(&daily,c).await?;ensure(w.prepare_settings_mock(op("limit-daily-invalid"),"fictional-subscription-1",s.clone(),NOW,EXPIRES).await.is_err())?;
    record(&mut rows,"limits_daily_one_day_lead_rejected",c,schema,base).await?;
    s.days_before=0;let mut w=load(&daily,c).await?;settings(&mut w,"limit-daily-valid","fictional-subscription-1",s).await?;let saved=workflow_commit(&daily,c,other,w).await?;
    ensure(saved.due_reminders_mock("2026-10-16T12:00:00+08:00")?.messages.is_empty())?;
    record(&mut rows,"limits_daily_same_day_saved",c,schema,base).await?;
    reminder_seed(c,other,&ns,true).await?;let store=fail(ProposedLedgerStoreMock::new(&ns))?;let reader=fail(ProposedLedgerReaderMock::new(&ns))?;
    let first=limits_attempt(&store,c,other,NOW,DeliveryOutcome::DefiniteFailure).await?;let p=first.reminder_plan_mock("fictional-subscription-1")?;ensure(p.current_attempt_count==Some(1) && p.retries_remaining==Some(1))?;ensure(first.due_reminders_mock(NOW)?.messages.is_empty())?;
    record(&mut rows,"limits_first_failure_waits_for_next_day",c,schema,base).await?;
    let second=limits_attempt(&store,c,other,"2026-10-15T12:00:00+08:00",DeliveryOutcome::DefiniteFailure).await?;let p=second.reminder_plan_mock("fictional-subscription-1")?;ensure(p.current_attempt==Some(ReminderAttemptStateMock::RetryExhausted) && p.current_attempt_count==Some(2))?;
    record(&mut rows,"limits_second_failure_exhausts_retry",c,schema,base).await?;
    let third=limits_attempt(&store,c,other,"2026-10-16T12:00:00+08:00",DeliveryOutcome::SimulatedSuccess).await?;ensure(third.reminder_plan_mock("fictional-subscription-1")?.current_attempt_count==Some(2))?;
    record(&mut rows,"limits_third_scan_no_new_attempt",c,schema,base).await?;
    for (n,enabled) in [(0,false),(1,true)] {
        let now=format!("2026-10-16T13:0{n}:00+08:00");let mut s=schedule();s.enabled=enabled;let mut w=load(&store,c).await?;let b=fail(w.prepare_settings_mock(op(&format!("limit-toggle-{n}")),"fictional-subscription-1",s,&now,"2026-10-16T13:05:00+08:00").await)?;fail(w.confirm_settings_mock(&b,&now,&mut Gate::yes()).await)?;workflow_commit(&store,c,other,w).await?;
    }
    let saved=view(&reader,other).await?;let q=SharedSubscriptionReadQueryMock::Business(SharedBusinessReadQueryMock::ReminderPlan {subscription_id:"fictional-subscription-1".into()});let shown=fail(store.query_on_connection_mock(other,"user_demo",&q).await)?;
    ensure(saved.due_reminders_mock("2026-10-17T12:00:00+08:00")?.messages.is_empty() && shown["current_attempt_count"]==2 && shown["retries_remaining"]==0 && shown["current_attempt"]=="RETRY_EXHAUSTED")?;
    record(&mut rows,"limits_toggle_and_user_query_keep_exhausted_budget",c,schema,base).await?;
    let data:Value=sqlx::query_scalar(&format!("SELECT journal FROM \"{ns}\".subscription_runtime_records WHERE owner_id='user_demo'")).fetch_one(&mut *c).await?;
    let cycle_ns=format!("{ns}_cycle");seed(c,&cycle_ns,"user_demo",FICTIONAL_JOURNAL_ENCODING_V1,data).await?;let cycle=fail(ProposedLedgerStoreMock::new(&cycle_ns))?;let fresh=limits_attempt(&cycle,c,other,"2026-11-14T12:00:00+08:00",DeliveryOutcome::DefiniteFailure).await?;let p=fresh.reminder_plan_mock("fictional-subscription-1")?;ensure(p.current_attempt_count==Some(1) && p.saved_cycle_outcomes.iter().any(|a|a.attempt_count==2))?;
    record(&mut rows,"limits_new_cycle_has_independent_budget",c,schema,base).await?;
    let cn=format!("{ns}_cancel");reminder_seed(c,other,&cn,false).await?;let cancel=fail(ProposedLedgerStoreMock::new(&cn))?;let mut w=load(&cancel,c).await?;intent(&mut w,"limit-cancel","fictional-subscription-1",MockIntentKind::Record).await?;workflow_commit(&cancel,c,other,w).await?;
    limits_attempt(&cancel,c,other,"2026-10-17T12:00:00+08:00",DeliveryOutcome::DefiniteFailure).await?;limits_attempt(&cancel,c,other,"2026-10-18T12:00:00+08:00",DeliveryOutcome::DefiniteFailure).await?;let p=limits_attempt(&cancel,c,other,"2026-10-19T12:00:00+08:00",DeliveryOutcome::SimulatedSuccess).await?.reminder_plan_mock("fictional-subscription-1")?;ensure(p.current_attempt_count==Some(2) && p.current_attempt==Some(ReminderAttemptStateMock::RetryExhausted))?;
    record(&mut rows,"limits_cancel_late_recovery_also_stops_after_two",c,schema,base).await?;
    let un=format!("{ns}_unknown");reminder_seed(c,other,&un,true).await?;let unknown=fail(ProposedLedgerStoreMock::new(&un))?;limits_attempt(&unknown,c,other,NOW,DeliveryOutcome::DefiniteFailure).await?;let p=limits_attempt(&unknown,c,other,"2026-10-15T12:00:00+08:00",DeliveryOutcome::Unknown).await?.reminder_plan_mock("fictional-subscription-1")?;ensure(p.current_attempt_count==Some(2) && p.current_attempt==Some(ReminderAttemptStateMock::NeedsReview))?;
    record(&mut rows,"limits_unknown_retry_keeps_review_not_success",c,schema,base).await?;
    let old:Value=serde_json::from_str(include_str!(env!("SAFEFLOW_SUBSCRIPTION_LEGACY_RETRY_FIXTURE")))?;let ln=format!("{ns}_legacy");seed(c,&ln,"user_demo",FICTIONAL_JOURNAL_ENCODING_V1,old.clone()).await?;let legacy=fail(ProposedLedgerReaderMock::new(&ln))?;let saved=view(&legacy,other).await?;let id=saved.list_subscriptions_mock()?.items[0].subscription_id.clone();let p=saved.reminder_plan_mock(&id)?;ensure(p.current_attempt_count==Some(3) && p.retries_remaining==Some(0) && saved.due_reminders_mock("2026-10-17T12:00:00+08:00")?.messages.is_empty())?;
    record(&mut rows,"limits_legacy_three_attempts_read_without_refill",c,schema,base).await?;
    let rb=format!("{ns}_rollback");reminder_seed(c,other,&rb,true).await?;let rollback=fail(ProposedLedgerStoreMock::new(&rb))?;limits_attempt(&rollback,c,other,NOW,DeliveryOutcome::DefiniteFailure).await?;let before=snapshot(c,&rb).await?;let mut w=load(&rollback,c).await?;fail(w.reserve_reminders_mock("2026-10-15T12:00:00+08:00").await)?;c.execute("BEGIN").await?;let p=save(&rollback,c,w).await?;c.execute("ROLLBACK").await?;ensure(p.into_committed_reminder_batch_mock(other).await.is_err() && snapshot(c,&rb).await?==before)?;
    record(&mut rows,"limits_rolled_back_reservation_does_not_consume_retry",c,schema,base).await?;
    let bad_ns=format!("{ns}_bad");let mut bad=old;bad["commands"][2]["reminder_rules"]=json!(99);seed(c,&bad_ns,"user_demo",FICTIONAL_JOURNAL_ENCODING_V1,bad).await?;let bad=fail(ProposedLedgerReaderMock::new(&bad_ns))?;ensure(bad.load_on_connection_mock(other,"user_demo").await.is_err())?;
    record(&mut rows,"limits_unknown_policy_version_rejected",c,schema,base).await?;
    Ok(rows)
}
async fn limits_after_restart(c:&mut PgConnection,other:&mut PgConnection,schema:&str)->Result<()> {
    let ns=format!("{schema}_lim");
    for suffix in ["","_cancel","_unknown","_legacy","_rollback"] {
        let n=format!("{ns}{suffix}");let reader=fail(ProposedLedgerReaderMock::new(&n))?;let saved=view(&reader,c).await?;let id=saved.list_subscriptions_mock()?.items[0].subscription_id.clone();let p=saved.reminder_plan_mock(&id)?;
        let count=match suffix {"_legacy"=>3,"_rollback"=>1,_=>2};ensure(p.current_attempt_count==Some(count))?;
        if suffix=="_unknown" {ensure(p.current_attempt==Some(ReminderAttemptStateMock::NeedsReview))?;} else if suffix!="_rollback" {ensure(p.current_attempt==Some(ReminderAttemptStateMock::RetryExhausted))?;ensure(saved.due_reminders_mock(if suffix=="_cancel" {"2026-10-20T12:00:00+08:00"}else{"2026-10-17T12:00:00+08:00"})?.messages.is_empty())?;}
        let store=fail(ProposedLedgerStoreMock::new(&n))?;c.execute("BEGIN").await?;let pending=fail(store.stage_startup_recovery_on_connection_mock(c,"user_demo").await)?;c.execute("COMMIT").await?;fail(pending.read_committed_startup_summary_on_connection_mock(other,"2026-10-20T12:00:00+08:00").await)?;ensure(view(&reader,other).await?.reminder_plan_mock(&id)?.current_attempt_count==Some(count))?;
    }
    for suffix in ["","_daily","_cycle","_cancel","_unknown","_legacy","_rollback","_bad"] {c.execute(format!("DROP SCHEMA \"{ns}{suffix}\" CASCADE").as_str()).await?;}Ok(())
}
