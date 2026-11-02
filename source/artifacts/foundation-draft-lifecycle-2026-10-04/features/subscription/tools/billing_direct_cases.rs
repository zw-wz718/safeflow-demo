// Uses actual Jonathan collect() through the subscription-side assembly module.
async fn direct_billing_cases(c:&mut PgConnection,other:&mut PgConnection,schema:&str,base:&Value)->Result<Vec<Value>> {
    use safeflow_billing_source_reader::query as billing;
    let ns=format!("{schema}_bd");let empty_ns=format!("{schema}_bd_missing");
    for name in [&ns,&empty_ns] {c.execute(format!("CREATE SCHEMA \"{name}\"").as_str()).await?;c.execute(ddl(name)?.as_str()).await?;}
    let store=fail(ProposedLedgerStoreMock::new(&ns))?;init_save(&store,c,other,&init_request(&store)?).await?;let mut rows=vec![];
    let (scope,request,source)=billing_fixture::fixture("complete")?;let before=snapshot(c,&ns).await?;
    let result=fail(billing_direct::identify_from_billing_reader_mock(&store,other,&scope,&request,&source,billing::Limits::default()).await)?;
    ensure(result["read"]["pages_read"]==2 && result["recognition"]["matching_executed"]==true && result["recognition"]["groups"].as_array().ok_or("GROUPS_ARRAY")?.iter().any(|g|g["disposition"]=="NOT_IN_LEDGER"))?;
    ensure(snapshot(c,&ns).await?==before)?;
    std::fs::write(query_path("billing-direct-identify.json")?,serde_json::to_vec_pretty(&result)?)?;
    record(&mut rows,"billing_direct_actual_reader_to_saved_empty_ledger",c,schema,base).await?;
    let again=fail(billing_direct::identify_from_billing_reader_mock(&store,other,&scope,&request,&source,billing::Limits::default()).await)?;ensure(again==result && snapshot(c,&ns).await?==before)?;
    record(&mut rows,"billing_direct_repeat_read_has_no_registration_or_write",c,schema,base).await?;
    let failed=billing_direct::identify_from_billing_reader_mock(&store,other,&scope,&request,&billing_fixture::FailingSource,billing::Limits::default()).await;
    ensure(matches!(failed,Err(billing_direct::DirectReaderIssueMock::SourceUnavailable)) && snapshot(c,&ns).await?==before)?;
    let missing=fail(ProposedLedgerStoreMock::new(&empty_ns))?;ensure(matches!(billing_direct::identify_from_billing_reader_mock(&missing,other,&scope,&request,&source,billing::Limits::default()).await,Err(billing_direct::DirectReaderIssueMock::LedgerUnavailable)))?;
    record(&mut rows,"billing_direct_source_failure_or_missing_ledger_stops",c,schema,base).await?;
    let mut existing=journal(Some("ACTIVE"));let entry=&mut existing["initial"]["entries"][0];entry["source_id"]=json!("fictional-billing-input-v1");entry["merchant_id"]=json!("merchant_demo_video");entry["service_ref"]=json!("service_demo_video_1");
    seed(c,&ns,"user_demo",FICTIONAL_JOURNAL_ENCODING_V1,existing).await?;
    let known=fail(billing_direct::identify_from_billing_reader_mock(&store,other,&scope,&request,&source,billing::Limits::default()).await)?;ensure(known["recognition"]["groups"].as_array().ok_or("GROUPS_ARRAY")?.iter().any(|g|g["disposition"]=="ALREADY_MANAGED"))?;
    record(&mut rows,"billing_direct_current_registered_service_not_recommended_again",c,schema,base).await?;
    let (_,_,mut unknown)=billing_fixture::fixture("complete")?;
    for step in &mut unknown.steps {if let Some(items)=step["page"]["items"].as_array_mut() {for row in items {row.as_object_mut().ok_or("ROW_OBJECT")?.remove("service_ref");}}}
    let review=fail(billing_direct::identify_from_billing_reader_mock(&store,other,&scope,&request,&unknown,billing::Limits::default()).await)?;ensure(review["recognition"]["groups"].as_array().ok_or("GROUPS_ARRAY")?.iter().all(|g|g["disposition"]!="NOT_IN_LEDGER" && g["disposition"]!="ALREADY_MANAGED"))?;
    record(&mut rows,"billing_direct_missing_service_remains_review_without_guessing",c,schema,base).await?;
    c.execute(format!("DROP SCHEMA \"{ns}\" CASCADE; DROP SCHEMA \"{empty_ns}\" CASCADE").as_str()).await?;Ok(rows)
}
