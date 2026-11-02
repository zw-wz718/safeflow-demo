//! Staged owned-PG acceptance; clock injection remains existing cfg(test) only.
//! Python reads real public HTTP between fresh test/backend processes.
use super::*;
use crate::*;
use safeflow_contracts::SecurityError;
use safeflow_runtime::SessionResolver;
struct Sessions;
impl SessionResolver for Sessions {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(UserId::parse("user_001").unwrap())
    }
    fn resolve_binding(&self, s: &SessionRef) -> Result<SessionBinding, SecurityError> {
        Ok(SessionBinding { user_id: self.resolve(s)?, binding_version: 1 })
    }
}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
#[ignore]
async fn monthly47_repair1_staged_owned_pg() {
    let stage=std::env::var("SAFEFLOW_TEST47_STAGE").expect("owned test stage");
    let url=std::env::var("SAFEFLOW_TEST47_DATABASE_URL").expect("owned47 only");
    let repo=PgRepository::connect_existing(&url,"sf_unified_noon_acceptance").await.unwrap();
    let source=PgUnifiedFictionalSource::open(repo.clone(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await.unwrap();
    let owner=UserId::parse("user_001").unwrap();
    let team=TeamConsumptionSource::open(&source,&owner).await.unwrap();
    let bank:Arc<dyn BankGateway>=Arc::new(source);
    let session=SessionRef::parse("owned47_monthly_session").unwrap();
    let service=SubscriptionService::for_trusted_backend(repo.clone(),Arc::new(Sessions));
    let host=service.current_host_for_backend(&session).await.unwrap();
    let scope=service.open_team_public_scope_for_backend(&session,&host,&team,bank.as_ref()).await.unwrap();
    let config=SubscriptionReminderConfig {owner,accounts:vec![ResourceId::parse("account_001").unwrap()],max_messages:128,max_ledger_entries:256,scan_timeout:Duration::from_secs(5),cycles:1,interval:Duration::from_secs(1)};
    let runner=service.persistent_reminder_host_for_backend(&session,&scope,bank.clone(),config).await.unwrap();
    let stop=SubscriptionReminderStop::default();
    let mut reports=Vec::new();
    let stamp=match stage.as_str() {
        "SNAPSHOT"=>None,
        "VIDEO"=>Some("2026-11-02T04:00:00Z"),
        "VIDEO_EDITED"=>Some("2026-11-03T04:00:00Z"),
        "MUSIC_UNKNOWN"|"RESTART"=>Some("2026-11-12T04:00:00Z"),
        "LATE"=>Some("2026-11-28T04:00:00Z"),
        _=>panic!("unknown stage"),
    };
    if let Some(stamp)=stamp {
        *runner.probe.clock.lock().unwrap()=Some(stamp.into());
        if stage=="MUSIC_UNKNOWN" {
            let entered=Arc::new(tokio::sync::Notify::new());
            let release=Arc::new(tokio::sync::Notify::new());
            *runner.probe.point.lock().unwrap()=Some(("reserve-committed",entered.clone(),release.clone()));
            // Keep the first invocation alive after committed reservation while
            // a second invocation observes the real pending attempt. It must
            // neither roll over twice nor reserve/send a duplicate.
            let observer=async {
                entered.notified().await;
                let started=std::time::Instant::now();
                let duplicate=match runner.scan_once(&SubscriptionReminderStop::default()).await {
                    Ok(scan)=>{
                        assert_eq!(scan.due_count,0);assert_eq!(scan.simulated_count,0);
                        serde_json::json!({"concurrent_reservation":"NO_DUPLICATE","scan":scan})
                    }
                    Err(StorageError::Database)=>serde_json::json!({"concurrent_reservation":"BOUNDED_DATABASE_REJECTION","seconds":started.elapsed().as_secs_f64()}),
                    Err(other)=>panic!("unexpected concurrent error: {other:?}"),
                };
                stop.stop();release.notify_one();duplicate
            };
            let (first,duplicate)=tokio::join!(runner.scan_once(&stop),observer);
            assert!(first.is_err());reports.push(duplicate);
        } else {
            let (a,b)=tokio::join!(runner.scan_once(&stop),runner.scan_once(&stop));
            let a=a.unwrap();let b=b.unwrap();
            let want=usize::from(stage=="VIDEO"||stage=="VIDEO_EDITED");
            assert_eq!(a.due_count+b.due_count,want,"concurrent duplicate");
            assert_eq!(a.simulated_count+b.simulated_count,want);
            assert!(!a.notification_sent&&!b.notification_sent);
            reports.push(serde_json::to_value(a).unwrap());reports.push(serde_json::to_value(b).unwrap());
        }
    }
    // Typed replay of the real stored Journal, then a private clone-only
    // preparation obtains CURRENT settings_revision. No command is persisted,
    // no confirmation is minted, and HTTP gets no invented revision field.
    let mut c=repo.pool.acquire().await.unwrap();
    let saved=service.load(&mut c,"user_001").await.unwrap();
    let ledger=saved.ledger_mock();
    let mut plans=Vec::new();
    for item in ledger.list_mock("user_001").unwrap() {
        let plan=saved.reminder_plan_mock(&item.subscription_id).unwrap();
        let schedule=saved.reminders_mock().current_schedule_mock(ledger,"user_001",&item.subscription_id).unwrap().clone();
        let mut reminders=saved.reminders_mock().clone();
        let op=MockOperationContext {user_id:"user_001".into(),session_ref:"private-clone-only".into(),task_id:"private-clone-only".into(),node_id:"private-clone-only".into(),operation_id:format!("private-clone-{}",item.subscription_id),idempotency_key:format!("private-clone-{}",item.subscription_id),plan_version:1};
        let binding=reminders.prepare_mock(ledger,op,&item.subscription_id,schedule,"2026-11-28T12:00:00+08:00","2026-11-28T12:05:00+08:00").unwrap();
        plans.push(serde_json::json!({"subscription_id":item.subscription_id,"settings_revision":binding.settings_revision,"plan":plan}));
    }
    let preview=saved.due_reminders_mock("2026-11-28T12:00:00+08:00").unwrap();
    assert!(!preview.reserved&&!preview.notification_sent);
    let attempts=service.persistent_reminder_view_for_backend(&session,&scope,bank.as_ref(),None).await.unwrap();
    println!("MONTHLY47_EVIDENCE={}",serde_json::json!({"stage":stage,"storage_version":saved.version_mock(),"plans":plans,"attempts":attempts,"reports":reports,"late_readonly_preview":preview,"calendar_dec5":team.calendar_at(chrono::DateTime::parse_from_rfc3339("2026-11-06T00:00:00+08:00").unwrap().with_timezone(&chrono::Utc)),"calendar_dec15":team.calendar_at(chrono::DateTime::parse_from_rfc3339("2026-11-16T00:00:00+08:00").unwrap().with_timezone(&chrono::Utc))}));
    drop(c);repo.close().await;
}
