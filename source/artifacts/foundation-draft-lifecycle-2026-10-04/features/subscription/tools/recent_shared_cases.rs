// New real-database coverage for the three offline-only batches. Test harness only.
async fn recent_shared_cases(c:&mut PgConnection,other:&mut PgConnection,schema:&str,sample:&Sample,base:&Value)->Result<Vec<Value>> {
    let ns=format!("{schema}_rc");let flow=format!("{schema}_flow");
    for name in [&ns,&flow] {c.execute(format!("CREATE SCHEMA \"{name}\"").as_str()).await?;c.execute(ddl(name)?.as_str()).await?;}
    let output=std::process::Command::new(std::env::var("SAFEFLOW_SUBSCRIPTION_WORKFLOW_EXE")?).args(["--run-fictional",&flow])
        .env("SAFEFLOW_SUBSCRIPTION_SHARED_QUERY_URL",std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL")?).output()?;
    // Keep child failures visible without ever printing connection environment.
    ensure(output.status.success())?;
    let chain:Value=serde_json::from_slice(&output.stdout)?;
    ensure(chain["database_executed"]==true && chain["exported_business_record_count"]==4 && chain["current_subscription"]["current_state"]=="ACTIVE" && chain["notification_sent"]==false && chain["audit_received"]==false)?;
    let chain_file=query_path("recent-shared-full-workflow.json")?;std::fs::write(chain_file,serde_json::to_vec_pretty(&chain)?)?;
    let mut rows=vec![];record(&mut rows,"recent_full_shared_workflow_separate_process",c,schema,base).await?;
    let store=fail(ProposedLedgerStoreMock::new(&ns))?;let reader=fail(ProposedLedgerReaderMock::new(&ns))?;
    init_save(&store,c,other,&init_request(&store)?).await?;
    let before=snapshot(c,&ns).await?;
    let v=fail(store.query_on_connection_mock(other,"user_demo",&SharedSubscriptionReadQueryMock::Business(SharedBusinessReadQueryMock::IdentifySubscriptions {context:sample.trusted_context_fixture.clone(),input:Box::new(sample.snapshot.clone())})).await)?;
    ensure(v["groups"][0]["disposition"]=="NOT_IN_LEDGER")?;
    let date=fail(store.query_on_connection_mock(other,"user_demo",&SharedSubscriptionReadQueryMock::Business(SharedBusinessReadQueryMock::SuggestReminderDate {context:sample.trusted_context_fixture.clone(),input:Box::new(sample.snapshot.clone()),candidate_index:Some(0),subscription_id:None,now:NOW.into()})).await)?;
    ensure(date["state"]=="READY" && snapshot(c,&ns).await?==before)?;
    record(&mut rows,"recent_shared_recognition_date_no_write",c,schema,base).await?;
    let selection=fail(store.select_discovery_candidate_on_connection_mock(other,"user_demo",&sample.trusted_context_fixture,&sample.snapshot,0,NOW).await)?;
    c.execute("BEGIN").await?;let mut w=load(&store,c).await?;
    fail(w.prepare_registration_from_selection_mock(selection,op("recent-rollback"),NOW,EXPIRES).await)?;
    let p=save(&store,c,w).await?;c.execute("ROLLBACK").await?;
    ensure(p.read_committed_on_connection_mock(other).await.is_err() && snapshot(c,&ns).await?==before)?;
    record(&mut rows,"recent_selected_registration_rollback_no_effect",c,schema,base).await?;
    let stale=fail(store.select_discovery_candidate_on_connection_mock(other,"user_demo",&sample.trusted_context_fixture,&sample.snapshot,0,NOW).await)?;
    c.execute("BEGIN").await?;let p=fail(store.stage_startup_recovery_on_connection_mock(c,"user_demo").await)?;c.execute("COMMIT").await?;fail(p.read_committed_startup_summary_on_connection_mock(other,NOW).await)?;
    let mut w=load(&store,c).await?;ensure(w.prepare_registration_from_selection_mock(stale,op("recent-stale"),NOW,EXPIRES).await.is_err())?;
    record(&mut rows,"recent_selected_candidate_rejects_changed_saved_facts",c,schema,base).await?;
    let selection=fail(store.select_discovery_candidate_on_connection_mock(other,"user_demo",&sample.trusted_context_fixture,&sample.snapshot,0,NOW).await)?;
    let mut w=load(&store,c).await?;let prepared=fail(w.prepare_registration_from_selection_mock(selection,op("recent-register"),NOW,EXPIRES).await)?;
    let b=prepared.confirmation_binding;let saved=workflow_commit(&store,c,other,w).await?;saved.registration_preview_mock(&b.draft_id,&b.context,NOW)?;
    let mut w=load(&store,c).await?;fail(w.confirm_registration_mock(&b.draft_id,&b.context,NOW,&mut Gate::yes()).await)?;
    c.execute("BEGIN").await?;let h=fail(store.save_with_handoff_on_connection_mock(c,w).await)?;let mut requests=h.original_requests_mock().to_vec();c.execute("COMMIT").await?;
    let v=serde_json::to_value(h.complete_on_connection_mock(other,Ok::<(),()>(())).await)?;ensure(v["state"]=="COMMITTED_VERIFIED")?;
    let id=view(&reader,other).await?.list_subscriptions_mock()?.items[0].subscription_id.clone();
    record(&mut rows,"recent_selected_registration_commit_independent_readback",c,schema,base).await?;
    for (kind,label) in [(0,"recent_unified_settings_rollback_confirm"),(1,"recent_cancel_uncertain_commit_read_only"),(2,"recent_unified_revoke_commit_result")] {
        let action=match kind {0=>SubscriptionChangeRequestMock::UpdateReminderSettings {subscription_id:id.clone(),schedule:schedule()},1=>SubscriptionChangeRequestMock::CreateCancellationIntent {subscription_id:id.clone()},_=>SubscriptionChangeRequestMock::RevokeCancellationIntent {subscription_id:id.clone()}};
        let mut w=load(&store,c).await?;let prepared=fail(w.prepare_subscription_change_mock(action,op(&format!("recent-change-{kind}")),NOW,EXPIRES).await)?;
        if kind==0 {
            let before=snapshot(c,&ns).await?;c.execute("BEGIN").await?;let p=save(&store,c,w).await?;c.execute("ROLLBACK").await?;ensure(prepared.read_committed_preview_on_connection_mock(&p,other,NOW).await.is_err() && snapshot(c,&ns).await?==before)?;
            w=load(&store,c).await?;
            let _=fail(w.prepare_subscription_change_mock(SubscriptionChangeRequestMock::UpdateReminderSettings {subscription_id:id.clone(),schedule:schedule()},op("recent-change-0"),NOW,EXPIRES).await)?;
        }
        c.execute("BEGIN").await?;let p=save(&store,c,w).await?;c.execute("COMMIT").await?;fail(prepared.read_committed_preview_on_connection_mock(&p,other,NOW).await)?;
        let mut w=load(&store,c).await?;
        match prepared.confirmation_binding_mock() {SubscriptionChangeBindingMock::Intent(b)=>fail(w.confirm_intent_mock(&b.draft_id,&b.context,NOW,&mut Gate::yes()).await)?,SubscriptionChangeBindingMock::Settings(b)=>fail(w.confirm_settings_mock(b,NOW,&mut Gate::yes()).await)?};
        c.execute("BEGIN").await?;let h=fail(store.save_with_handoff_on_connection_mock(c,w).await)?;requests.extend_from_slice(h.original_requests_mock());c.execute("COMMIT").await?;
        let v=serde_json::to_value(h.complete_on_connection_mock(other,if kind==1 {Err(())} else {Ok(())}).await)?;
        ensure(v["state"]==if kind==1 {"READ_ONLY_RECOVERY"} else {"COMMITTED_VERIFIED"})?;ensure(v["original_results"][0]["status"]==["SETTINGS_RECORDED","CANCELLATION_INTENT_RECORDED","CANCELLATION_INTENT_REVOKED"][kind])?;
        record(&mut rows,label,c,schema,base).await?;
    }
    let w=load(&store,c).await?;c.execute("BEGIN").await?;let h=fail(store.save_with_handoff_on_connection_mock(c,w).await)?;
    let v=serde_json::to_value(h.complete_on_connection_mock(c,Ok::<(),()>(())).await)?;ensure(v["state"]=="READ_ONLY_RECOVERY" && v["saved_prefix_verified"]==false)?;c.execute("ROLLBACK").await?;
    record(&mut rows,"recent_handoff_rejects_same_uncommitted_writer",c,schema,base).await?;
    let first=fail(store.business_record_page_on_connection_mock(other,"user_demo",None,2).await)?;ensure(first.records.len()==2 && first.has_more)?;
    let cursor:SharedBusinessRecordCursorMock=serde_json::from_value(serde_json::to_value(&first.next_cursor)?)?;
    let next=fail(store.business_record_page_on_connection_mock(other,"user_demo",Some(&cursor),10).await)?;ensure(next.records.len()==2 && !next.has_more)?;
    let mut wrong=serde_json::to_value(&next.next_cursor)?;wrong["namespace"]=json!("other");let wrong:SharedBusinessRecordCursorMock=serde_json::from_value(wrong)?;ensure(store.business_record_page_on_connection_mock(other,"user_demo",Some(&wrong),2).await.is_err())?;
    std::fs::write(query_path("recent-cursor.json")?,serde_json::to_vec(&next.next_cursor)?)?;std::fs::write(query_path("recent-original-requests.json")?,serde_json::to_vec(&requests)?)?;
    record(&mut rows,"recent_shared_cursor_roundtrip_and_store_isolation",c,schema,base).await?;
    let mut w=load(&store,c).await?;let _prepared=fail(w.prepare_subscription_change_mock(SubscriptionChangeRequestMock::CreateCancellationIntent {subscription_id:id.clone()},op("recent-startup-draft"),NOW,EXPIRES).await)?;workflow_commit(&store,c,other,w).await?;
    let before=snapshot(c,&ns).await?;c.execute("BEGIN").await?;let p=fail(store.stage_startup_recovery_on_connection_mock(c,"user_demo").await)?;c.execute("ROLLBACK").await?;ensure(p.read_committed_startup_summary_on_connection_mock(other,NOW).await.is_err() && snapshot(c,&ns).await?==before)?;
    record(&mut rows,"recent_startup_rollback_preserves_pending",c,schema,base).await?;
    c.execute("BEGIN").await?;let p=fail(store.stage_startup_recovery_on_connection_mock(c,"user_demo").await)?;c.execute("COMMIT").await?;let shown=fail(p.read_committed_startup_summary_on_connection_mock(other,NOW).await)?;
    ensure(shown["business_record_count"]==4 && shown["pending"].as_array().ok_or("PENDING_ARRAY")?.iter().any(|p|p["state"]=="INVALIDATED"))?;
    ensure(view(&reader,other).await?.list_subscriptions_mock()?.items[0].current_state==safeflow_subscription_evidence::managed_ledger_mock::LedgerStatus::Active)?;
    record(&mut rows,"recent_startup_commit_invalidates_only_pending",c,schema,base).await?;
    Ok(rows)
}
async fn recent_shared_after_restart(c:&mut PgConnection,other:&mut PgConnection,schema:&str)->Result<()> {
    let ns=format!("{schema}_rc");let flow=format!("{schema}_flow");let store=fail(ProposedLedgerStoreMock::new(&ns))?;
    let cursor:SharedBusinessRecordCursorMock=serde_json::from_slice(&std::fs::read(query_path("recent-cursor.json")?)?)?;
    ensure(fail(store.business_record_page_on_connection_mock(c,"user_demo",Some(&cursor),10).await)?.records.is_empty())?;
    let reqs:Vec<OriginalOperationRequestMock>=serde_json::from_slice(&std::fs::read(query_path("recent-original-requests.json")?)?)?;ensure(reqs.len()==4)?;
    for request in reqs {let v=fail(store.query_change_result_on_connection_mock(c,"user_demo",&request).await)?;ensure(v["status"]!="NEEDS_REVIEW" && v["status"]!="UNAVAILABLE")?;}
    c.execute("BEGIN").await?;let p=fail(store.stage_startup_recovery_on_connection_mock(c,"user_demo").await)?;c.execute("COMMIT").await?;let v=fail(p.read_committed_startup_summary_on_connection_mock(other,NOW).await)?;ensure(v["business_record_count"]==4)?;
    let owner:String=sqlx::query_scalar(&format!("SELECT owner_id FROM \"{flow}\".subscription_runtime_records")).fetch_one(&mut *c).await?;
    let saved=fail(fail(ProposedLedgerReaderMock::new(&flow))?.load_on_connection_mock(c,&owner).await)?;ensure(saved.business_records_mock().len()==4 && saved.list_subscriptions_mock()?.items[0].current_state==safeflow_subscription_evidence::managed_ledger_mock::LedgerStatus::Active)?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE; DROP SCHEMA \"{flow}\" CASCADE").as_str()).await?;Ok(())
}
