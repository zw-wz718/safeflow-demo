//! New feature functional test in its own newly provisioned fixed-team database.
use super::*;
use crate::*;
use safeflow_contracts::SecurityError;
use safeflow_runtime::{SessionResolver,RuntimeBootstrap,ToolRegistry,DomainPolicyRegistry,ToolSecurityCatalog,InMemoryAuditRecorder};
use std::sync::atomic::AtomicBool;
struct Session{revoked:AtomicBool}
impl SessionResolver for Session{
 fn resolve(&self,s:&SessionRef)->Result<UserId,SecurityError>{Ok(self.resolve_binding(s)?.user_id)}
 fn resolve_binding(&self,_:&SessionRef)->Result<SessionBinding,SecurityError>{if self.revoked.load(Ordering::SeqCst){Err(SecurityError{reason_code:safeflow_contracts::ReasonCode::parse("SESSION_MISSING").unwrap(),message:"stopped".into()})}else{Ok(SessionBinding{user_id:UserId::parse("user_001").unwrap(),binding_version:1})}}
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
#[ignore]
async fn persistent_team_reminders_save_query_restart_and_deduplicate(){
 let url=std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("own fresh DB only");
 let ns="sf_unified_persistent_reminder";
 let doc=UnifiedFictionalDocument::read_adopted_v1(&std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").unwrap()).unwrap()).unwrap();
 let identity=PgUnifiedFictionalSource::provision_new_database(&url,ns,doc).await.unwrap();
 let repo=identity.repository_for_isolated_backend();let owner=UserId::parse("user_001").unwrap();
 let team=TeamConsumptionSource::import(&identity,&owner,&TeamConsumptionDocument::fixed_adopted().unwrap()).await.unwrap();
 team.persistent_for_backend().set_active(true).await.unwrap();
 let sessions=Arc::new(Session{revoked:AtomicBool::new(false)});let session=SessionRef::parse("native_persistent_test").unwrap();
 let bank:Arc<dyn BankGateway>=Arc::new(identity);
 let service=SubscriptionService::for_trusted_backend(repo.clone(),sessions.clone());
 service.initialize_simulation_for_host(&session,bank.as_ref(),&team.persistent_for_backend()).await.unwrap();
 let host=service.startup_for_host(&session).await.unwrap();
 let scope=service.install_team_public_scope_for_backend(&session,&host,&team,bank.as_ref()).await.unwrap();
 let runtime=RuntimeBootstrap::build(ToolRegistry::new(),DomainPolicyRegistry::new(),ToolSecurityCatalog::new(),sessions.clone(),Arc::new(InMemoryAuditRecorder::new())).unwrap();
 let writes=||SubscriptionWriteService::for_trusted_backend(repo.clone(),sessions.clone(),bank.clone(),runtime.runtime(),ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),PermitConfig::new(Duration::from_secs(30)).unwrap()).unwrap().for_team_public_scope(&scope).unwrap();
 for amount in [1990,990]{
  let capture=writes().capture(&session).await.unwrap();
  let groups=capture["preview"]["recognition"]["groups"].as_array().unwrap();
  let index=groups.iter().find(|g|g["charges"].as_array().is_some_and(|c|!c.is_empty()&&c.iter().all(|r|r["amount_minor_units"]==amount))).unwrap()["selection_index"].as_u64().unwrap() as usize;
  let p=writes().prepare(&session,&uuid::Uuid::new_v4().to_string(),SubscriptionPublicRequest::Registration{capture_ref:capture["capture_ref"].as_str().unwrap().into(),candidate_index:index}).await.unwrap();
  writes().confirm(&session,&p.task.task_id,&p.confirmation.confirmation_ref).await.unwrap();
  writes().execute(&session,&p.task.task_id,&p.operation.operation_id,&p.confirmation.confirmation_ref).await.unwrap();
 }
 for name in ["视频","音乐"]{
  let list=writes().list(&session).await.unwrap();
  let item=list["items"].as_array().unwrap().iter().find(|i|i["service_display_name"].as_str().is_some_and(|s|s.contains(name))).unwrap();
  let p=writes().prepare(&session,&uuid::Uuid::new_v4().to_string(),SubscriptionPublicRequest::ReminderSettings{selection_ref:item["selection_ref"].as_str().unwrap().into(),enabled:true,days_before:3}).await.unwrap();
  writes().confirm(&session,&p.task.task_id,&p.confirmation.confirmation_ref).await.unwrap();writes().execute(&session,&p.task.task_id,&p.operation.operation_id,&p.confirmation.confirmation_ref).await.unwrap();
 }
 let config=SubscriptionReminderConfig{owner,accounts:vec![ResourceId::parse("account_001").unwrap()],max_messages:16,max_ledger_entries:64,scan_timeout:Duration::from_secs(20),cycles:1,interval:Duration::from_secs(1)};
 let runner=service.persistent_reminder_host_for_backend(&session,&scope,bank.clone(),config.clone()).await.unwrap();
 let stop=SubscriptionReminderStop::default();
 let today=runner.scan_once(&stop).await.unwrap();assert_eq!(today.due_count,0);assert!(!today.notification_sent);
 *runner.probe.clock.lock().unwrap()=Some("2026-11-02T04:00:00Z".into());
 let got=runner.scan_once(&stop).await.unwrap();assert_eq!(got.due_count,1);assert_eq!(got.simulated_count,1);assert!(got.result_saved);assert_eq!(got.original_attempt_refs.len(),1);
 let id=&got.original_attempt_refs[0];let original=service.persistent_reminder_view_for_backend(&session,&scope,bank.as_ref(),Some(id)).await.unwrap();
 assert_eq!(original["items"][0]["channel_state"],"PERSISTED_SIMULATION");assert_eq!(original["items"][0]["original"]["items"][0]["status"],"SIMULATED_SUCCESS");
 assert!(!original.to_string().contains("original_request"));assert!(!original.to_string().contains("cycle_ref"));
 assert_eq!(runner.scan_once(&stop).await.unwrap().due_count,0);
 // A normal local storage failure after channel persistence must retain the
 // original attempt and never resend after restart. Not a permission probe.
 sqlx::query("CREATE FUNCTION owned_result_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'owned result storage interruption'; END $$").execute(&repo.pool).await.unwrap();
 sqlx::query("CREATE TRIGGER owned_result_failure BEFORE UPDATE ON subscription_runtime_records FOR EACH ROW WHEN ((NEW.journal->'commands'->-1->>'kind')='ResolveScan') EXECUTE FUNCTION owned_result_failure()").execute(&repo.pool).await.unwrap();
 *runner.probe.clock.lock().unwrap()=Some("2026-11-12T04:00:00Z".into());
 assert!(runner.scan_once(&stop).await.is_err());
 sqlx::query("DROP TRIGGER owned_result_failure ON subscription_runtime_records").execute(&repo.pool).await.unwrap();
 sqlx::query("DROP FUNCTION owned_result_failure()").execute(&repo.pool).await.unwrap();
 let after_failure=service.persistent_reminder_view_for_backend(&session,&scope,bank.as_ref(),None).await.unwrap();
 assert_eq!(after_failure["items"].as_array().unwrap().len(),2);
 let unknown=after_failure["items"].as_array().unwrap().iter().find(|i|i["attempt_ref"]!=*id).unwrap();
 assert_eq!(unknown["channel_state"],"PERSISTED_SIMULATION");
 assert_eq!(unknown["original"]["items"][0]["status"],"NEEDS_REVIEW");
 let replacement=service.startup_for_host(&session).await.unwrap();
 assert!(runner.scan_once(&stop).await.is_err());
 let next_scope=service.open_team_public_scope_for_backend(&session,&replacement,&team,bank.as_ref()).await.unwrap();
 assert_eq!(original,service.persistent_reminder_view_for_backend(&session,&next_scope,bank.as_ref(),Some(id)).await.unwrap());
 let next=service.persistent_reminder_host_for_backend(&session,&next_scope,bank.clone(),config).await.unwrap();
 *next.probe.clock.lock().unwrap()=Some("2026-11-12T04:00:00Z".into());assert_eq!(next.scan_once(&stop).await.unwrap().due_count,0);
 stop.stop();assert!(next.scan_once(&stop).await.is_err());
 let count:i64=sqlx::query_scalar("SELECT count(*) FROM subscription_simulated_notifications_v1").fetch_one(&repo.pool).await.unwrap();assert_eq!(count,2);
 sessions.revoked.store(true,Ordering::SeqCst);assert!(service.persistent_reminder_view_for_backend(&session,&next_scope,bank.as_ref(),Some(id)).await.is_err());
 println!("PERSISTENT_TEAM_FUNCTIONAL: today=0 future_success=1 channel_saved_journal_unknown=1 receipts=2 duplicate=0 restart_original_equal=true stopped=true logout_rejected=true");
 repo.close().await;
}
