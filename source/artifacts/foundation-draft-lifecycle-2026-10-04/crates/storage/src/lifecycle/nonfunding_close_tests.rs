use super::*;

fn short_service(db:&Db)->LifecycleService<'_>{
    let mut s=db.service().with_transfer_sql_bridge_for_backend();
    s.confirmations=ConfirmationService::for_lifecycle_backend_with_clock(
        db.repository.clone(),db.sessions.clone(),ConfirmationConfig::new(Duration::from_secs(2)).unwrap(),Arc::new(ServerClock));
    s
}
async fn expires(c:&LifecycleView){
    let remaining=c.confirmation.as_ref().unwrap().expires_at-chrono::Utc::now();
    if let Ok(duration)=remaining.to_std(){tokio::time::sleep(duration+Duration::from_millis(30)).await;}
}
async fn marker_count(db:&Db)->i64{
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE details->>'schema'=$1").bind(DRAFT_CLOSE).fetch_one(&db.sql).await.unwrap()
}
async fn close(db:&Db,s:&LifecycleService<'_>,c:&LifecycleView)->Result<Value,StorageError>{
    let before=db.claim_status().await;
    let r=s.close_unknown_nonfunding_draft(&session(),&c.task.task_id,&c.operation.operation_id,&version(c)).await;
    assert_eq!(db.claim_status().await,before);r
}

#[tokio::test]
#[ignore="owned PostgreSQL, real lifecycle claim and in-flight second transaction"]
async fn closure_expired_claim_runtime_second_transaction_and_queued_payment_never_enter(){
    let db=Db::new().await;
    let mut s=short_service(&db);
    let c=db.confirmed_using(&s).await;
    let pause=Arc::new(ExecutionPause::default());
    s.execution_schedule.after_claim_commit=Some(pause.clone());
    let session=session();let expected=version(&c);let reference=&c.confirmation.as_ref().unwrap().confirmation_ref;
    let mut running=Box::pin(s.execute(&session,&c.task.task_id,&c.operation.operation_id,reference,&expected));
    tokio::select!{ _=&mut running=>panic!("must wait after durable claim"),_=reached(&pause)=>{} }
    assert_eq!(db.calls(),0);assert_eq!(db.claim_status().await,vec!["CLAIMED"]);
    assert!(close(&db,&s,&c).await.is_err(),"preflight expiry alone cannot close an unexpired confirmation");
    expires(&c).await;
    // Queue the actual public close behind the task row; queue the actual
    // payment preparation behind its owner lock. This is not close-then-call.
    let mut gate=db.sql.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE").bind(c.task.task_id.as_str()).execute(&mut *gate).await.unwrap();
    let mut closing=Box::pin(s.close_unknown_nonfunding_draft(&session,&c.task.task_id,&c.operation.operation_id,&expected));
    tokio::select!{_=&mut closing=>panic!("task lock not held"),_=tokio::time::sleep(Duration::from_millis(80))=>{}}
    let payments=crate::PaymentPreparationService::for_trusted_backend(db.repository.clone(),db.sessions.clone(),ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),db.assembly.runtime(),DecisionConfig::new(Duration::from_secs(300),"close-payment",contracts()).unwrap());
    let fake=crate::PaymentPreparationRequest{selection_ticket:c.selection_ticket.clone(),task_revision:c.task.revision,plan_version:c.task.plan.plan_version,facts_version:c.task.facts_version,draft_id:ResourceId::parse("absent_initial_draft").unwrap(),draft_version:1};
    let mut payment=Box::pin(payments.prepare(&session,&c.task.task_id,fake));
    tokio::select!{_=&mut payment=>panic!("payment must be queued concurrently"),_=tokio::time::sleep(Duration::from_millis(80))=>{}}
    gate.commit().await.unwrap();
    assert_eq!(closing.await.unwrap()["closed_nonfunding_draft"],true);
    assert_eq!(payment.await.err(),Some(StorageError::ExecutionOperationConflict));
    pause.release.notify_one();
    assert_eq!(running.await.err(),Some(StorageError::ExecutionOperationConflict));
    assert_eq!(db.calls(),0);
    assert_eq!(marker_count(&db).await,1);
    close(&db,&s,&c).await.unwrap();assert_eq!(marker_count(&db).await,1);
    let mut wrong=version(&c);wrong.operation_revision+=1;
    assert!(s.close_unknown_nonfunding_draft(&session,&c.task.task_id,&c.operation.operation_id,&wrong).await.is_err());
    assert!(s.close_unknown_nonfunding_draft(&SessionRef::parse("unavailable_session").unwrap(),&c.task.task_id,&c.operation.operation_id,&expected).await.is_err());
    let resumed=SessionRef::parse("resumed_own_session").unwrap();
    let fresh=short_service(&db); // fresh service, no cached close state
    fresh.close_unknown_nonfunding_draft(&resumed,&c.task.task_id,&c.operation.operation_id,&expected).await.unwrap();
    let v=fresh.query(&resumed,&c.task.task_id,Some(&c.operation.operation_id)).await.unwrap();
    assert!(v.receipt.is_none()&&v.unknown_operation.is_some());assert_eq!(v.task,c.task);
    let directory=db.repository.bank_recovery_for_backend(&c.task.owner_id).await.unwrap();
    assert_eq!(directory["closed_nonfunding_tasks"],json!([c.task.task_id]));
    assert_eq!(db.claim_status().await,vec!["CLAIMED"]);
    drop(fresh);drop(s);db.finish().await;
}

#[tokio::test]
#[ignore="owned PostgreSQL, actual Tool call completed but original SQL result unknown"]
async fn closure_tool_ran_without_sql_receipt_preserves_unknown_and_two_claim_shapes(){
    let db=Db::new().await;
    for _ in 0..2{
        let s=short_service(&db);let c=db.confirmed_using(&s).await;
        // The default display-only observer deliberately cannot return the
        // complete SQL bridge readback, AFTER the real simulated Tool runs.
        assert!(s.execute(&session(),&c.task.task_id,&c.operation.operation_id,&c.confirmation.as_ref().unwrap().confirmation_ref,&version(&c)).await.is_err());
        expires(&c).await;
        close(&db,&s,&c).await.unwrap();
        let original=s.query(&session(),&c.task.task_id,Some(&c.operation.operation_id)).await.unwrap();
        assert!(original.unknown_operation.is_some()&&original.receipt.is_none());
        assert_eq!(original.operation,c.operation);
    }
    assert_eq!(db.calls(),2);assert_eq!(marker_count(&db).await,2);
    assert_eq!(db.claim_status().await,vec!["CLAIMED","CLAIMED"]);
    db.finish().await;
}

#[tokio::test]
#[ignore="owned PostgreSQL, damaged or funding-related history cannot be released"]
async fn closure_missing_claim_and_funding_operation_and_corrupt_marker_are_fail_closed(){
    let db=Db::new().await;let s=short_service(&db);let c=db.confirmed_using(&s).await;
    expires(&c).await;
    assert!(close(&db,&s,&c).await.is_err());
    // No claim and a submitted/result-unknown operation may never be closed.
    sqlx::query("UPDATE operations SET state='PROCESSING',business_operation_ref='funding_unknown_fixture',revision=3 WHERE operation_id=$1").bind(c.operation.operation_id.as_str()).execute(&db.sql).await.unwrap();
    assert!(close(&db,&s,&c).await.is_err());assert_eq!(marker_count(&db).await,0);
    // A forged/damaged immutable marker is an availability error, not proof.
    let mut tx=db.sql.begin().await.unwrap();
    audit(&mut tx,&c.task,Some(&c.operation),PersistentAuditKind::CheckpointSaved,json!({"schema":DRAFT_CLOSE})).await.unwrap();tx.commit().await.unwrap();
    assert!(db.repository.bank_recovery_for_backend(&c.task.owner_id).await.is_err());
    drop(s);db.finish().await;
}
