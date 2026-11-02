//! Isolated bank site. Credentials and challenges stay on private native stdin/stdout.
use safeflow_api::{router,ApiConfig,ApiState,BillingFeedbackConfig,PublicDemoConfig};
use safeflow_contracts::{OperationId,UserId,TaskId};
use safeflow_storage::*;
use serde::Deserialize;
use serde_json::json;
use std::{io::{BufRead,Write},time::Duration};
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {schema:String,owner:String,bind:String,frontend_origin:String,recovery_realm:String,confirmation_seconds:u64,permit_seconds:u64,
    #[serde(default)] public_demo:Option<PublicDemoConfig>,
    #[serde(default)] demo_code_display:bool,
    #[serde(default)] llm:Option<LlmConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmConfig { enabled:bool,endpoint:String,model:String,timeout_seconds:u64 }
impl LlmConfig {
    fn gateway(&self,key:Option<String>)->Result<Option<safeflow_runtime::OpenAiGatewayConfig>,Box<dyn std::error::Error>> {
        // Exact provider choice is a native deployment decision, never HTTP input.
        if self.endpoint!="https://api.commandcode.ai/provider/v1/chat/completions"
            || self.model!="deepseek/deepseek-v4.1-flash"
            || !(1..=60).contains(&self.timeout_seconds) {return Err("bank LLM configuration rejected".into());}
        if !self.enabled {if key.is_some(){return Err("disabled LLM must not receive key".into());}return Ok(None);}
        let key=Zeroizing::new(key.ok_or("enabled LLM requires private key")?);
        Ok(Some(safeflow_runtime::OpenAiGatewayConfig::new(&self.endpoint,&self.model,key,Duration::from_secs(self.timeout_seconds)).map_err(|_|"bank LLM configuration rejected")?))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {database_url:String,login_secret:String,controller_secret:String,api_role:Option<String>,api_password:Option<String>,#[serde(default)] llm_api_key:Option<String>}
#[derive(Deserialize)]
#[serde(tag="kind",rename_all="SCREAMING_SNAKE_CASE",deny_unknown_fields)]
enum Command {
    Challenge {cookie:String,operation_id:OperationId,task_id:TaskId,consent_id:String,controller_secret:String},
    CardChallenge {cookie:String,operation_id:OperationId,controller_secret:String},
    Stop {},
}
fn display(v:serde_json::Value){println!("{v}");std::io::stdout().flush().expect("native display");}
#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>>{
    let a:Vec<_>=std::env::args().collect();
    if a.len()!=3 || !matches!(a[2].as_str(),"provision"|"serve"){return Err("usage: bank_site <nonsecret-config.json> <provision|serve>; private credentials on stdin".into());}
    let c:Config=serde_json::from_slice(&std::fs::read(&a[1])?)?;
    if c.confirmation_seconds!=300 || c.permit_seconds!=60{return Err("this isolated candidate supports explicit 300s confirmation and 60s permit only".into());}
    if c.public_demo.is_some() && c.owner!="user_001" {return Err("public demonstration uses fixed user_001 only".into());}
    let mut lines=std::io::stdin().lock().lines();
    let raw=Zeroizing::new(lines.next().ok_or("private credentials required")??);
    let secrets:Credentials=serde_json::from_str(&raw)?;drop(raw);
    let planner=match &c.llm {
        Some(llm) if a[2]=="serve"=>llm.gateway(secrets.llm_api_key)?,
        _=>{if secrets.llm_api_key.is_some(){return Err("LLM key not accepted without enabled serve configuration".into());}None},
    };
    let url=Zeroizing::new(secrets.database_url);let login=Zeroizing::new(secrets.login_secret);let control=Zeroizing::new(secrets.controller_secret);
    let mut api=ApiConfig::local_simulation(login.clone());api.bind=c.bind.parse()?;api.frontend_origin=c.frontend_origin.clone();api.request_timeout=Duration::from_secs(30);api.public_demo=c.public_demo.clone();api.demo_code_display=c.demo_code_display;api.validate()?;
    let source=if a[2]=="provision"{
        PgUnifiedFictionalSource::provision_new_database(&url,&c.schema,UnifiedFictionalDocument::read_adopted_v1(include_bytes!("../../../../../fixtures/transfer/unified_demo_source_v1.json"))?).await?
    }else{PgUnifiedFictionalSource::open(PgRepository::connect_existing(&url,&c.schema).await?,UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await?};
    let repo=source.repository_for_isolated_backend();
    if a[2]=="provision"{
        let doc=TeamConsumptionDocument::fixed_adopted()?;
        for name in ["user_001","user_002","demo_user_li_v1"]{
            let owner=UserId::parse(name)?;
            TeamConsumptionSource::import(&source,&owner,&doc).await?.persistent_for_backend().set_active(true).await?;
            let mut state=ApiState::for_unified_team_consumption(source.clone(),ApiConfig::local_simulation(login.clone()),owner).await?;
            let session=state.simulation_session_for_native_host(&login)?;
            state.initialize_team_initial_for_backend(&session).await?;
            for account in state.unified_accounts_for_backend(&session).await?{state.initialize_unified_catalog_for_backend(&session,&account.account_id).await?;}
            state.enable_team_subscription_writes_for_backend(&session,true).await?;
            state.stop_native_simulation_session(&session)?;
        }
        let final_card=CardFinalDeployment::for_isolated_backend(source.clone());final_card.activate_exact(&[],&[]).await?;
        let role=secrets.api_role.ok_or("new API role required")?;let password=Zeroizing::new(secrets.api_password.ok_or("new API password required")?);
        PaymentBackgroundDeployment::for_isolated_backend(source.clone()).register_new_api_login(&role,password).await?;
        final_card.adopt_payment_api_login_for_bank_site(&role).await?;
        // New isolated administrator grants only this assembly's explicit public
        // read settlement/billing/subscription writers; no producer/final funds grants.
        if !role.starts_with("sf_") || !role.bytes().all(|v|v.is_ascii_lowercase()||v.is_ascii_digit()||v==b'_'){return Err("role".into());}
        let pool=sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await?;
        for (privileges,tables) in [
            ("SELECT,INSERT,UPDATE",&["runtime_read_claims","runtime_read_continuations","billing_prepare_requests_v2","billing_prepare_attempts_v2","billing_prepare_rate_v2","billing_prepare_rate_clock_v2","subscription_host_generations_v1","subscription_runtime_records","subscription_preparations_v1","subscription_public_bindings_v1"][..]),
            ("SELECT,INSERT",&["public_billing_reports_v1","billing_report_bindings_v1","subscription_public_evidence_v1","public_subscription_a2_links_v1","billing_feedback_v1","billing_feedback_events_v1","public_billing_feedback_results_v1","public_billing_audit_links_v1","subscription_reminder_attempts_v1","subscription_simulated_notifications_v1"][..]),
        ]{for table in tables{sqlx::query(&format!("GRANT {privileges} ON \"{}\".{table} TO \"{role}\"",c.schema)).execute(&pool).await?;}}
        for (table,column) in [("billing_prepare_realms_v2","singleton"),("simulation_source_heads_v1","generation"),("public_unified_fictional_source_v1","singleton")]{sqlx::query(&format!("GRANT UPDATE({column}) ON \"{}\".{table} TO \"{role}\"",c.schema)).execute(&pool).await?;}
        sqlx::query(&format!("GRANT DELETE ON \"{}\".billing_prepare_rate_v2 TO \"{role}\"",c.schema)).execute(&pool).await?;
        for function in ["verify_subscription_a2_v1(text)","subscription_a2_details_v1(jsonb)","payment_completion_rows_v1(text,jsonb)"]{sqlx::query(&format!("GRANT EXECUTE ON FUNCTION \"{}\".{function} TO \"{role}\"",c.schema)).execute(&pool).await?;}
        repo.require_draft_balance_revision_for_backend().await?;
    let team=TeamConsumptionSource::open(&source,&UserId::parse(&c.owner)?).await?;let pg=team.persistent_for_backend();let spec=pg.spec();
        repo.install_billing_recovery_for_reviewed_backend(&BillingRecoveryConfig::new(&c.recovery_realm,&c.schema,&spec.source_id,&spec.revision,"bank-site-monthly-v1",8,128,60,180,180,360,true)?).await?;
        pool.close().await;display(json!({"provisioned":true,"new_isolated_database":true,"mode":"PROJECT_SIMULATION","real_services":false}));repo.close().await;return Ok(());
    }
    repo.require_draft_balance_revision_for_backend().await?;
    let team=TeamConsumptionSource::open(&source,&UserId::parse(&c.owner)?).await?;
    let spec=team.persistent_for_backend().spec().clone();
    let feedback=BillingFeedbackConfig::for_reviewed_backend(ConfirmationConfig::new(Duration::from_secs(c.confirmation_seconds))?,PermitConfig::new(Duration::from_secs(c.permit_seconds))?)
        .with_recovery_v2(BillingRecoveryConfig::new(&c.recovery_realm,&c.schema,&spec.source_id,&spec.revision,"bank-site-monthly-v1",8,128,60,180,180,360,true)?);
    let llm_enabled=planner.is_some();
    let mut state=if let Some(planner)=planner {
        ApiState::for_unified_bank_site_with_conversation(source,api,UserId::parse(&c.owner)?,control.clone(),feedback,planner).await?
    }else{ApiState::for_unified_bank_site(source,api,UserId::parse(&c.owner)?,control.clone(),feedback).await?};
    let session=state.simulation_session_for_native_host(&login)?;
    state.enable_team_subscription_writes_for_backend(&session,false).await?;
    state.stop_native_card_session(&session).await?;
    let listener=tokio::net::TcpListener::bind(&c.bind).await?;
    let (tx,rx)=tokio::sync::oneshot::channel();let app=state.clone();
    let server=tokio::spawn(async move{axum::serve(listener,router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async{let _=rx.await;}).await});
    display(json!({"ready":true,"mode":"PROJECT_SIMULATION","bind":c.bind,"native_challenges_only":!c.demo_code_display,"same_device_demo_codes":c.demo_code_display,"audit_sender_configured":false,"real_llm":llm_enabled}));
    for line in lines{
        let raw=Zeroizing::new(line?);let command=serde_json::from_str::<Command>(&raw);drop(raw);
        let result=async{match command{
            Ok(Command::Stop{})=>Ok(None),
            Ok(Command::Challenge{cookie,operation_id,task_id,consent_id,controller_secret})=>{
                let secret=Zeroizing::new(controller_secret);let mut h=axum::http::HeaderMap::new();h.insert(axum::http::header::COOKIE,cookie.parse().map_err(|_|StorageError::Session)?);
                let session=state.billing_feedback_session_for_backend(&h)?;
                let code=state.payment_background_challenge_for_native_controller(&secret,&session,&task_id,&operation_id,&consent_id).await?;
                Ok(Some(json!({"code":code.as_ref().map(|v|v.expose_for_native_display()),"one_time_native_display":true})))
            },
            Ok(Command::CardChallenge{cookie,operation_id,controller_secret})=>{
                let secret=Zeroizing::new(controller_secret);let mut h=axum::http::HeaderMap::new();h.insert(axum::http::header::COOKIE,cookie.parse().map_err(|_|StorageError::Session)?);
                let session=state.billing_feedback_session_for_backend(&h)?;
                let mut delivery=state.card_challenge_for_native_controller(&secret,&session,&operation_id).await?;
                let code=delivery.take_secret();Ok(Some(json!({"challenge_id":delivery.challenge_id,"code":code.as_ref().map(|v|v.expose_once_for_native_display()),"one_time_native_display":true})))
            },
            Err(_)=>Err(StorageError::Invalid("native command")),
        }}.await;
        match result{Ok(None)=>break,Ok(Some(v))=>display(v),Err(_)=>display(json!({"error":"NATIVE_CHALLENGE_UNAVAILABLE"}))}
    }
    let _=tx.send(());server.await??;repo.close().await;Ok(())
}
