use super::*;

fn short(db:&Db)->LifecycleService<'_>{
 let mut s=db.service().with_transfer_sql_bridge_for_backend();
 s.confirmations=ConfirmationService::for_lifecycle_backend_with_clock(db.repository.clone(),db.sessions.clone(),ConfirmationConfig::new(Duration::from_secs(2)).unwrap(),Arc::new(ServerClock));s
}
fn replacement()->IntentReplacement{IntentReplacement{task_id:new_task_id(),facts_version:7,parameters:json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"300.00","currency":"CNY","remark":"new explicit transfer","replaces":null})}}
async fn count(db:&Db)->i64{sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE details->>'schema'='UNEXECUTED_DRAFT_TERMINATED_V1'").fetch_one(&db.sql).await.unwrap()}
async fn end(db:&Db,s:&LifecycleService<'_>,v:&LifecycleView,r:Option<IntentReplacement>)->Result<Value,StorageError>{
 let before=db.claim_status().await;
 let result=s.terminate_unexecuted_intent(&session(),&v.task.task_id,&v.operation.operation_id,&version(v),r).await;
 assert_eq!(before,db.claim_status().await);result
}
#[tokio::test]
#[ignore="owned PostgreSQL, actual confirmation deadline and late calls"]
async fn intent_expiry_preserves_original_and_rejects_late_prepare_execute_and_wrong_session(){
 let db=Db::new().await;let s=short(&db);let c=db.confirmed_using(&s).await;
 assert!(end(&db,&s,&c,None).await.is_err());
 let until=c.confirmation.as_ref().unwrap().expires_at-chrono::Utc::now();
 if let Ok(d)=until.to_std(){tokio::time::sleep(d+Duration::from_millis(30)).await;}
 assert_eq!(count(&db).await,0,"ordinary query does not end a draft");
 s.query(&session(),&c.task.task_id,None).await.unwrap();assert_eq!(count(&db).await,0);
 let first=end(&db,&s,&c,None).await.unwrap();assert_eq!(first["reason"],"EXPIRED");
 let mut tx=db.sql.begin().await.unwrap();
 assert_eq!(super::super::reject_closed_nonfunding_draft(&mut tx,&c.task.task_id).await.err(),Some(StorageError::DraftIntentExpired));tx.rollback().await.unwrap();
 assert_eq!(end(&db,&s,&c,None).await.unwrap(),first);assert_eq!(count(&db).await,1);
 assert_eq!(s.prepare(&session(),&c.task.task_id,&c.operation.operation_id).await.err(),Some(StorageError::DraftIntentExpired));
 assert_eq!(s.act(&session(),&c.task.task_id,&c.operation.operation_id,&c.confirmation.as_ref().unwrap().confirmation_ref,c.display_ticket.as_ref().unwrap(),PlanningConfirmationAction::Confirm).await.err(),Some(StorageError::DraftIntentExpired));
 assert_eq!(s.execute(&session(),&c.task.task_id,&c.operation.operation_id,&c.confirmation.as_ref().unwrap().confirmation_ref,&version(&c)).await.err(),Some(StorageError::DraftIntentExpired));
 assert!(s.terminate_unexecuted_intent(&SessionRef::parse("wrong_session").unwrap(),&c.task.task_id,&c.operation.operation_id,&version(&c),None).await.is_err());
 let resumed=short(&db);let v=resumed.query(&SessionRef::parse("resumed_own_session").unwrap(),&c.task.task_id,None).await.unwrap();
 assert_eq!(v.task,c.task);assert_eq!(v.operation,c.operation);assert_eq!(v.intent_termination,Some(first));assert_eq!(db.calls(),0);
 db.finish().await;
}
#[tokio::test]
#[ignore="owned PostgreSQL, bound replacement and transaction rollback"]
async fn intent_replacement_is_atomic_bound_idempotent_and_does_not_invent_initial_ttl(){
 let db=Db::new().await;let s=short(&db);let c=db.confirmed_using(&s).await;
 let collision=IntentReplacement{task_id:c.task.task_id.clone(),..replacement()};
 assert!(end(&db,&s,&c,Some(collision)).await.is_err());assert_eq!(count(&db).await,0);
 let r=replacement();let first=end(&db,&s,&c,Some(r.clone())).await.unwrap();
 assert_eq!(first["replacement"],json!(r));
 assert_eq!(end(&db,&s,&c,Some(r.clone())).await.unwrap(),first);
 let mut changed=r.clone();changed.parameters["amount"]=json!("301.00");assert!(end(&db,&s,&c,Some(changed)).await.is_err());
 let next=s.query(&session(),&r.task_id,None).await.unwrap();assert_eq!(next.parameters,r.parameters);
 assert!(end(&db,&s,&next,None).await.is_err(),"no made-up TTL without original confirmation");
 assert_eq!(count(&db).await,1);assert_eq!(db.calls(),0);
 let other=db.confirmed_using(&s).await;
 assert!(end(&db,&s,&other,Some(r)).await.is_err(),"existing replacement target cannot be hijacked");
 assert_eq!(count(&db).await,1,"failed new insertion rolled back the old termination too");
 db.finish().await;
}
#[tokio::test]
#[ignore="owned PostgreSQL, simultaneous execution claim and replacement"]
async fn intent_claim_wins_then_neither_expiry_nor_replacement_releases_unknown(){
 let db=Db::new().await;let mut s=short(&db);let c=db.confirmed_using(&s).await;
 let pause=Arc::new(ExecutionPause::default());s.execution_schedule.after_claim_commit=Some(pause.clone());
 let se=session();let v=version(&c);let reference=&c.confirmation.as_ref().unwrap().confirmation_ref;
 let mut running=Box::pin(s.execute(&se,&c.task.task_id,&c.operation.operation_id,reference,&v));
 tokio::select!{_=&mut running=>panic!("must pause after durable claim"),_=reached(&pause)=>{}}
 assert!(end(&db,&s,&c,Some(replacement())).await.is_err());
 tokio::time::sleep(Duration::from_millis(2100)).await;
 assert!(end(&db,&s,&c,None).await.is_err());assert_eq!(count(&db).await,0);
 pause.release.notify_one();assert!(running.await.is_err());assert_eq!(db.calls(),0);
 db.finish().await;
}
#[tokio::test]
#[ignore="owned PostgreSQL, execution replay observes a concurrently committed termination"]
async fn intent_termination_wins_against_old_execution_snapshot(){
 let db=Db::new().await;let mut s=short(&db);let c=db.confirmed_using(&s).await;
 let pause=Arc::new(ExecutionPause::default());s.execution_schedule.after_empty_replay=Some(pause.clone());
 let se=session();let v=version(&c);let reference=&c.confirmation.as_ref().unwrap().confirmation_ref;
 let mut running=Box::pin(s.execute(&se,&c.task.task_id,&c.operation.operation_id,reference,&v));
 tokio::select!{_=&mut running=>panic!("must pause after replay read"),_=reached(&pause)=>{}}
 end(&db,&s,&c,Some(replacement())).await.unwrap();pause.release.notify_one();
 assert!(running.await.is_err());assert_eq!(db.calls(),0);assert!(db.claim_status().await.is_empty());
 db.finish().await;
}
#[tokio::test]
#[ignore="owned PostgreSQL, 11 prior unconfirmed drafts plus a genuine unknown"]
async fn intent_multiple_historical_unconfirmed_drafts_can_end_without_creating_more_tasks(){
 let db=Db::new().await;let s=short(&db);let mut originals=Vec::new();
 for _ in 0..11 {
  let c=s.create(&session(),&new_task_id(),replacement().parameters,7).await.unwrap();originals.push(c);
 }
 let unknown=db.confirmed_using(&s).await;
 assert!(s.execute(&session(),&unknown.task.task_id,&unknown.operation.operation_id,&unknown.confirmation.as_ref().unwrap().confirmation_ref,&version(&unknown)).await.is_err());
 for c in &originals {
  assert!(c.intent_expires_at.is_none());
  let n=s.abandon_unexecuted_intent(&session(),&c.task.task_id,&c.operation.operation_id,&version(c)).await.unwrap();assert_eq!(n["reason"],"ABANDONED");
  s.abandon_unexecuted_intent(&session(),&c.task.task_id,&c.operation.operation_id,&version(c)).await.unwrap();
 }
 assert!(s.abandon_unexecuted_intent(&session(),&unknown.task.task_id,&unknown.operation.operation_id,&version(&unknown)).await.is_err());
 assert_eq!(count(&db).await,11);
 let tasks:i64=sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&db.sql).await.unwrap();assert_eq!(tasks,12);
 let dir=db.repository.bank_recovery_for_backend(&unknown.task.owner_id).await.unwrap();assert_eq!(dir["ended_draft_intents"].as_array().unwrap().len(),11);
 assert_eq!(db.claim_status().await,vec!["CLAIMED"]);db.finish().await;
}

struct CompleteReadback(Arc<MockDraftStore<SystemClock>>);
impl LifecycleDraftObserver for CompleteReadback {
 fn observe<'a>(&'a self,c:&'a ExecutionContext)->ContractFuture<'a,Result<Option<LifecycleDraftObservation>,StorageError>> {
  Box::pin(async move {StoreObserver(self.0.clone()).observe(c).await})
 }
 fn stored<'a>(&'a self,c:&'a ExecutionContext,id:&'a ResourceId)->ContractFuture<'a,Result<safeflow_transfer_integration::StoredDraft,StorageError>> {
  Box::pin(async move {self.0.read(c,id).await.map_err(|_|StorageError::DependencyUnavailable)})
 }
}
async fn expired_preparation_fixture(db:&Db,task:&str)->(LifecycleView,crate::PaymentPreparationView){
 let mut s=db.service().with_transfer_sql_bridge_for_backend();s.observer=Arc::new(CompleteReadback(db.store.clone()));
 let id=TaskId::parse(task).unwrap();
 let context=ExecutionContext{task_id:id.clone(),node_id:NodeId::parse("candidate_lookup").unwrap(),operation_id:new_operation_id(),authenticated_user_id:safeflow_contracts::UserId::parse("user_001").unwrap(),plan_version:1,idempotency_key:None};
 db.store.find_candidates(&context,"小李").await.unwrap();
 let initial=s.create(&session(),&id,replacement().parameters,7).await.unwrap();
 let prepared=s.prepare(&session(),&id,&initial.operation.operation_id).await.unwrap();
 let c=s.act(&session(),&id,&initial.operation.operation_id,&prepared.confirmation.as_ref().unwrap().confirmation_ref,prepared.display_ticket.as_ref().unwrap(),PlanningConfirmationAction::Confirm).await.unwrap();
 let v=s.execute(&session(),&id,&c.operation.operation_id,&c.confirmation.as_ref().unwrap().confirmation_ref,&version(&c)).await.unwrap();
 let payment=crate::PaymentPreparationService::for_trusted_backend(db.repository.clone(),db.sessions.clone(),ConfirmationConfig::new(Duration::from_secs(2)).unwrap(),db.assembly.runtime(),DecisionConfig::new(Duration::from_secs(300),"isolated-lifecycle-replay-test",contracts()).unwrap());
 let receipt=v.receipt.as_ref().unwrap();
 let p=payment.prepare(&session(),&id,crate::PaymentPreparationRequest{selection_ticket:v.selection_ticket.clone(),task_revision:v.task.revision,plan_version:v.task.plan.plan_version,facts_version:v.task.facts_version,draft_id:receipt.draft_id.clone(),draft_version:receipt.draft_version}).await.unwrap();
 (v,p)
}
async fn original_records(db:&Db)->Value {
 sqlx::query_scalar("SELECT jsonb_build_object('tasks',(SELECT jsonb_agg(to_jsonb(t) ORDER BY task_id) FROM tasks t),'ops',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM operations t),'preparations',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM payment_preparations t),'claims',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM mock_draft_lifecycle_claims t),'drafts',(SELECT jsonb_agg(to_jsonb(t) ORDER BY draft_id) FROM transfer_drafts_v1 t),'funds',(SELECT jsonb_agg(to_jsonb(t)) FROM transfer_fund_orders_v1 t),'ledger',(SELECT jsonb_agg(to_jsonb(t)) FROM transfer_ledger_v1 t))").fetch_one(&db.sql).await.unwrap()
}
#[tokio::test]
#[ignore="owned PostgreSQL; only the three fixed expired/unsubmitted preparation cases"]
async fn three_fixed_expired_preparations_abandon_preserves_history_and_recovers(){
 for task in super::super::intent::EXPIRED_PREPARATION_TASKS {
  let db=Db::with_draft_ttl(4).await;let (v,p)=expired_preparation_fixture(&db,task).await;
  let s=db.service().with_transfer_sql_bridge_for_backend();
  assert!(s.abandon_unexecuted_intent(&session(),&v.task.task_id,&v.operation.operation_id,&version(&v)).await.is_err(),"unexpired preparation cannot be ended");
  let deadline=p.snapshot.draft_expires_at as i64;
  let wait=deadline-chrono::Utc::now().timestamp();if wait>=0{tokio::time::sleep(Duration::from_secs(wait as u64)+Duration::from_millis(100)).await;}
  assert!(s.terminate_unexecuted_intent(&session(),&v.task.task_id,&v.operation.operation_id,&version(&v),None).await.is_err(),"automatic expiration is not expanded");
  assert!(s.terminate_unexecuted_intent(&session(),&v.task.task_id,&v.operation.operation_id,&version(&v),Some(replacement())).await.is_err(),"replacement is not expanded");
  let before=original_records(&db).await;
  let se=session();let resumed=SessionRef::parse("resumed_own_session").unwrap();let expected=version(&v);
  let (a,b)=tokio::join!(s.abandon_unexecuted_intent(&se,&v.task.task_id,&v.operation.operation_id,&expected),s.abandon_unexecuted_intent(&resumed,&v.task.task_id,&v.operation.operation_id,&expected));
  let n=a.unwrap();assert_eq!(b.unwrap(),n);assert_eq!(n["reason"],"ABANDONED");assert_eq!(n["proof"]["scope"],"EXPIRED_UNSUBMITTED_PREPARATION");
  assert_eq!(count(&db).await,1);assert_eq!(before,original_records(&db).await);
  let read=s.query(&session(),&v.task.task_id,None).await.unwrap();assert_eq!(read.intent_termination,Some(n.clone()));
  let directory=db.repository.bank_recovery_for_backend(&v.task.owner_id).await.unwrap();assert_eq!(directory["ended_draft_intents"][0],n);
  let mut tx=db.sql.begin().await.unwrap();assert_eq!(super::super::reject_closed_nonfunding_draft(&mut tx,&v.task.task_id).await.err(),Some(StorageError::DraftIntentAbandoned));tx.rollback().await.unwrap();
  assert_eq!(before,original_records(&db).await);db.finish().await;
 }
}
#[tokio::test]
#[ignore="owned PostgreSQL; submitted flags and out-of-scope tasks remain blockers"]
async fn three_fixed_expired_preparations_reject_submitted_and_unlisted(){
 for (task,submitted) in [(super::super::intent::EXPIRED_PREPARATION_TASKS[0],true),("unlisted_expired_preparation",false)] {
  let db=Db::with_draft_ttl(4).await;let (v,p)=expired_preparation_fixture(&db,task).await;
  if submitted {sqlx::query("UPDATE transfer_drafts_v1 SET submitted=true WHERE task_id=$1").bind(task).execute(&db.sql).await.unwrap();}
  let wait=p.snapshot.draft_expires_at as i64-chrono::Utc::now().timestamp();if wait>=0{tokio::time::sleep(Duration::from_secs(wait as u64)+Duration::from_millis(100)).await;}
  let before=original_records(&db).await;let s=db.service().with_transfer_sql_bridge_for_backend();
  assert!(s.abandon_unexecuted_intent(&session(),&v.task.task_id,&v.operation.operation_id,&version(&v)).await.is_err());assert_eq!(count(&db).await,0);assert_eq!(before,original_records(&db).await);db.finish().await;
 }
}
