//! Own provisioned47 only. Test clock exists solely under cfg(test).
use super::*;
use crate::*;
use safeflow_runtime::SessionResolver;
use safeflow_contracts::SecurityError;
struct Sessions(AtomicBool);
impl SessionResolver for Sessions {
 fn resolve(&self,s:&SessionRef)->Result<UserId,SecurityError>{Ok(self.resolve_binding(s)?.user_id)}
 fn resolve_binding(&self,_:&SessionRef)->Result<SessionBinding,SecurityError>{
  if self.0.load(Ordering::SeqCst){Err(SecurityError{reason_code:safeflow_contracts::ReasonCode::parse("SESSION_MISSING").unwrap(),message:"expired".into()})}
  else{Ok(SessionBinding{user_id:UserId::parse("user_001").unwrap(),binding_version:1})}
 }
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
#[ignore]
async fn adopted47_dedicated_identity_due_unknown_restart_and_session() {
 let url=std::env::var("SAFEFLOW_TEST47_DATABASE_URL").expect("owned47 only");
 let repo=PgRepository::connect_existing(&url,"sf_unified_noon_acceptance").await.unwrap();
 let source=PgUnifiedFictionalSource::open(repo.clone(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await.unwrap();
 let owner=UserId::parse("user_001").unwrap();let team=TeamConsumptionSource::open(&source,&owner).await.unwrap();
 assert_eq!(team.calendar_for_backend()["adoption"]["approved_next_period"]["revision"],TEAM_CALENDAR_NEXT_REVISION);
 let bank:Arc<dyn BankGateway>=Arc::new(source);
 let sessions=Arc::new(Sessions(AtomicBool::new(false)));let session=SessionRef::parse("owned47_test_session").unwrap();
 let service=SubscriptionService::for_trusted_backend(repo.clone(),sessions.clone());
 let host=service.current_host_for_backend(&session).await.unwrap();
 let scope=service.open_team_public_scope_for_backend(&session,&host,&team,bank.as_ref()).await.unwrap();
 let config=SubscriptionReminderConfig{owner,accounts:vec![ResourceId::parse("account_001").unwrap()],max_messages:128,max_ledger_entries:256,scan_timeout:Duration::from_secs(5),cycles:1,interval:Duration::from_secs(1)};
 let runner=service.persistent_reminder_host_for_backend(&session,&scope,bank.clone(),config.clone()).await.unwrap();
 let stop=SubscriptionReminderStop::default();
 *runner.probe.clock.lock().unwrap()=Some("2026-11-02T04:00:00Z".into());
 let (a,b)=tokio::join!(runner.scan_once(&stop),runner.scan_once(&stop));
 let a=a.unwrap();let b=b.unwrap();assert_eq!(a.due_count+b.due_count,1);
 let got=if a.due_count==1{a}else{b};
 assert_eq!(got.due_count,1);assert_eq!(got.simulated_count,1);assert!(got.result_saved);assert!(!got.notification_sent);
 let id=&got.original_attempt_refs[0];
 let before=service.persistent_reminder_view_for_backend(&session,&scope,bank.as_ref(),Some(id)).await.unwrap();
 assert_eq!(before["items"][0]["channel_state"],"PERSISTED_SIMULATION");
 assert_eq!(before["items"][0]["original"]["items"][0]["status"],"SIMULATED_SUCCESS");
 assert_eq!(runner.scan_once(&stop).await.unwrap().due_count,0);
 // Stop after the real ReserveScan transaction commits, before any sink call.
 // This produces a real unresolved original attempt without changing SQL/time.
 *runner.probe.clock.lock().unwrap()=Some("2026-11-12T04:00:00Z".into());
 let entered=Arc::new(tokio::sync::Notify::new());let release=Arc::new(tokio::sync::Notify::new());
 *runner.probe.point.lock().unwrap()=Some(("reserve-committed",entered.clone(),release.clone()));
 let stopper=tokio::spawn({let stop=stop.clone();async move{entered.notified().await;stop.stop();release.notify_one();}});
 assert!(runner.scan_once(&stop).await.is_err());stopper.await.unwrap();
 let view=service.persistent_reminder_view_for_backend(&session,&scope,bank.as_ref(),None).await.unwrap();
 assert_eq!(view["items"].as_array().unwrap().len(),2);
 let unknown=view["items"].as_array().unwrap().iter().find(|v|v["attempt_ref"]!=*id).unwrap();
 assert_eq!(unknown["channel_state"],"NOT_OBSERVED");
 assert_eq!(unknown["original"]["items"][0]["status"],"NEEDS_REVIEW");
 // New backend instance, same actual host generation, no initialize/restart.
 let service2=SubscriptionService::for_trusted_backend(repo.clone(),sessions.clone());
 let host2=service2.current_host_for_backend(&session).await.unwrap();
 let scope2=service2.open_team_public_scope_for_backend(&session,&host2,&team,bank.as_ref()).await.unwrap();
 let runner2=service2.persistent_reminder_host_for_backend(&session,&scope2,bank.clone(),config).await.unwrap();
 *runner2.probe.clock.lock().unwrap()=Some("2026-11-12T04:00:00Z".into());
 assert_eq!(runner2.scan_once(&SubscriptionReminderStop::default()).await.unwrap().due_count,0);
 assert_eq!(before,service2.persistent_reminder_view_for_backend(&session,&scope2,bank.as_ref(),Some(id)).await.unwrap());
 sessions.0.store(true,Ordering::SeqCst);
 assert!(runner2.scan_once(&SubscriptionReminderStop::default()).await.is_err());
 println!("CALENDAR47_ROLE_FUNCTIONAL due=1 persisted=1 duplicate=0 unresolved=1 restart_resend=0 old_receipt_equal=true session_revoked=true external_notification=false");
 repo.close().await;
}
