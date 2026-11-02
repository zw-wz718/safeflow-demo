//! Real fixed-source native boundary; every invocation needs a NEW owned DB.
use safeflow_api::{ApiConfig,ApiState};
use safeflow_contracts::{SessionRef,UserId};
use safeflow_storage::*;
use serde_json::{json,Value};
use sqlx::{postgres::PgPoolOptions,PgPool};
use std::time::Duration;
const NS:&str="sf_unified_public_native";
const SECRET:&str="step10-native-only-isolated-secret";
async fn setup()->(ApiState,PgUnifiedFictionalSource,PgPool,SessionRef){
    let url=std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("new owned PG database");
    let identity=PgUnifiedFictionalSource::provision_new_database(&url,NS,
        UnifiedFictionalDocument::read_adopted_v1(include_bytes!("../../../../../fixtures/transfer/unified_demo_source_v1.json")).unwrap()).await.unwrap();
    let owner=UserId::parse("user_001").unwrap();
    let source=TeamConsumptionSource::import(&identity,&owner,&TeamConsumptionDocument::fixed_adopted().unwrap()).await.unwrap();
    source.persistent_for_backend().set_active(true).await.unwrap();
    let mut state=ApiState::for_unified_team_consumption(identity.clone(),ApiConfig::local_simulation(zeroize::Zeroizing::new(SECRET.into())),owner).await.unwrap();
    let session=state.simulation_session_for_native_host(SECRET).unwrap();
    state.initialize_team_initial_for_backend(&session).await.unwrap();
    state.enable_team_subscription_writes_for_backend(&session,true).await.unwrap();
    let pool=PgPoolOptions::new().max_connections(4).after_connect(|c,_|Box::pin(async move{sqlx::query("SET search_path=sf_unified_public_native").execute(c).await?;Ok(())})).connect(&url).await.unwrap();
    (state,identity,pool,session)
}
async fn register(st:&ApiState,s:&SessionRef,amount:i64)->SubscriptionPreparation{
    let w=st.team_public_writes_for_backend().unwrap();let capture=w.capture(s).await.unwrap();
    let group=capture["preview"]["recognition"]["groups"].as_array().unwrap().iter().find(|g|g["charges"].as_array().is_some_and(|a|!a.is_empty() && a.iter().all(|t|t["amount_minor_units"]==amount))).unwrap();
    let p=w.prepare(s,&uuid::Uuid::new_v4().to_string(),SubscriptionPublicRequest::Registration{capture_ref:capture["capture_ref"].as_str().unwrap().into(),candidate_index:group["selection_index"].as_u64().unwrap() as usize}).await.unwrap();
    finish(st,s,&p).await;p
}
async fn finish(st:&ApiState,s:&SessionRef,p:&SubscriptionPreparation)->Value{
    let w=st.team_public_writes_for_backend().unwrap();w.confirm(s,&p.task.task_id,&p.confirmation.confirmation_ref).await.unwrap();
    w.execute(s,&p.task.task_id,&p.operation.operation_id,&p.confirmation.confirmation_ref).await.unwrap()
}
async fn change(st:&ApiState,s:&SessionRef,action:&str,service:&str,days:u8)->SubscriptionPreparation{
    let w=st.team_public_writes_for_backend().unwrap();let list=w.list(s).await.unwrap();
    let reference=list["items"].as_array().unwrap().iter().find(|v|v["service_display_name"]==service).unwrap()["selection_ref"].as_str().unwrap().to_string();
    let req=match action {
        "settings"=>SubscriptionPublicRequest::ReminderSettings{selection_ref:reference,enabled:true,days_before:days},
        "cancel"=>SubscriptionPublicRequest::RecordCancellation{selection_ref:reference},
        "revoke"=>SubscriptionPublicRequest::RevokeCancellation{selection_ref:reference},
        _=>panic!("test action"),
    };
    w.prepare(s,&uuid::Uuid::new_v4().to_string(),req).await.unwrap()
}
async fn graph(pool:&PgPool)->Value{
    let names=["subscription_runtime_records","subscription_public_bindings_v1","subscription_public_evidence_v1","subscription_preparations_v1","tasks","operations","server_confirmations","execution_permits","audit_events","public_audit_outbox","public_subscription_a2_links_v1"];
    let mut result=serde_json::Map::new();
    for name in names {let v:Value=sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb) FROM {name} t")).fetch_one(pool).await.unwrap();result.insert(name.into(),v);}
    Value::Object(result)
}
#[tokio::test]
async fn public_native_same_capture_scope_calendar_and_original_history(){
    let (st,identity,pool,s)=setup().await;
    register(&st,&s,1990).await;register(&st,&s,990).await;
    let p=change(&st,&s,"settings","演示视频服务",3).await;
    let w=st.team_public_writes_for_backend().unwrap();let view=w.preparation_view(&s,&p).await.unwrap();
    assert_eq!(view["summary"]["reminder_date"],"2026-11-02");assert!(view["summary"]["expires_at"].is_null());
    let original=finish(&st,&s,&p).await;
    let m=change(&st,&s,"settings","演示音乐服务",3).await;
    assert_eq!(w.preparation_view(&s,&m).await.unwrap()["summary"]["reminder_date"],"2026-11-12");finish(&st,&s,&m).await;
    for action in ["cancel","revoke","settings"]{let next=change(&st,&s,action,"演示视频服务",1).await;finish(&st,&s,&next).await;}
    assert_eq!(w.query(&s,&p.operation.operation_id).await.unwrap(),original);
    let new_session=st.simulation_session_for_native_host(SECRET).unwrap();assert_eq!(w.query(&new_session,&p.operation.operation_id).await.unwrap(),original);
    assert!(w.confirm(&new_session,&p.task.task_id,&p.confirmation.confirmation_ref).await.is_err());
    for raw in [json!({"kind":"REGISTRATION","capture_ref":"x","candidate_index":0,"owner":"user_001"}),json!({"kind":"REMINDER_SETTINGS","selection_ref":"x","enabled":true,"days_before":3,"schedule":{}})] {
        assert!(serde_json::from_value::<SubscriptionPublicRequest>(raw).is_err());
    }
    let before=graph(&pool).await;
    let source=TeamConsumptionSource::open(&identity,&UserId::parse("user_001").unwrap()).await.unwrap();source.persistent_for_backend().set_active(false).await.unwrap();
    assert!(w.capture(&s).await.is_err());assert_eq!(graph(&pool).await,before);
    drop(w);identity.repository_for_isolated_backend().close().await;pool.close().await;
}
#[tokio::test]
async fn public_native_final_member_await_session_revocation_rolls_back_every_fact(){
    let (st,identity,pool,s)=setup().await;register(&st,&s,1990).await;
    let p=change(&st,&s,"settings","演示视频服务",3).await;
    st.team_public_writes_for_backend().unwrap().confirm(&s,&p.task.task_id,&p.confirmation.confirmation_ref).await.unwrap();
    let before=graph(&pool).await;
    sqlx::raw_sql("CREATE FUNCTION owned_stage_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(1680000037,10); RETURN NEW; END $$; CREATE TRIGGER owned_stage_wait BEFORE UPDATE ON subscription_runtime_records FOR EACH ROW EXECUTE FUNCTION owned_stage_wait()").execute(&pool).await.unwrap();
    let mut blocker=pool.begin().await.unwrap();sqlx::query("SELECT pg_advisory_xact_lock(1680000037,10)").execute(&mut *blocker).await.unwrap();
    let other=st.clone();let session=s.clone();let task=p.task.task_id.clone();let op=p.operation.operation_id.clone();let confirmation=p.confirmation.confirmation_ref.clone();
    let running=tokio::spawn(async move {other.team_public_writes_for_backend().unwrap().execute(&session,&task,&op,&confirmation).await});
    tokio::time::timeout(Duration::from_secs(8),async{loop{let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory' AND query LIKE '%subscription_runtime_records%' AND pid<>pg_backend_pid())").fetch_one(&pool).await.unwrap();if waiting{break}tokio::time::sleep(Duration::from_millis(10)).await;}}).await.expect("actual member save must wait");
    st.stop_native_simulation_session(&s).unwrap();blocker.commit().await.unwrap();
    assert_eq!(running.await.unwrap(),Err(StorageError::Session));assert_eq!(graph(&pool).await,before);
    sqlx::raw_sql("DROP TRIGGER owned_stage_wait ON subscription_runtime_records; DROP FUNCTION owned_stage_wait()").execute(&pool).await.unwrap();
    identity.repository_for_isolated_backend().close().await;pool.close().await;
}
