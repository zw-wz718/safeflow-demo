//! Real HTTP + adopted PG bank + OpenAI wire fixture; never a real model/key.
use super::*;
use safeflow_storage::{PgUnifiedFictionalSource,UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION,TeamConsumptionSource,BillingRecoveryConfig};
use std::sync::atomic::{AtomicUsize,Ordering};
fn ok(reply:Reply)->Value{assert!(reply.status.is_success(),"{} {}",reply.status,reply.json);reply.json}

async fn provider(State(calls):State<Arc<AtomicUsize>>,Json(request):Json<Value>)->(StatusCode,Json<Value>){
    calls.fetch_add(1,Ordering::SeqCst);
    assert_eq!(request["model"],"deepseek/deepseek-v4.1-flash");
    assert_eq!(request["response_format"]["type"],"json_object");
    let system=request["messages"][0]["content"].as_str().unwrap();
    let protocol:Value=serde_json::from_str(system.split("Protocol data: ").nth(1).unwrap()).unwrap();
    assert_eq!(protocol["controlled_task_data"]["scope"],"PROJECT_SIMULATION_READ_ONLY_BANK");
    for d in protocol["approved_tool_definitions"].as_array().unwrap(){
        assert!(!["execute_transfer","execute_confirmed_payment","create_mock_transfer_draft","modify_mock_transfer_draft","cancel_mock_transfer_draft"].contains(&d["name"].as_str().unwrap()));
    }
    let message=request["messages"][1]["content"].as_str().unwrap();
    if message=="failure" {return (StatusCode::BAD_GATEWAY,Json(json!({"provider_error":"DO_NOT_RELAY_PROVIDER_DETAILS"})));}
    if message=="timeout" {tokio::time::sleep(Duration::from_secs(2)).await;}
    let tool=match message {"write"=>"create_mock_transfer_draft","unlock"=>"unlock_card","unknown"=>"submit_transfer",_=>"get_account_balance"};
    let account=if message=="foreign" {json!("account_002")}else{protocol["controlled_task_data"]["selection"]["account_id"].clone()};
    let ctx=&protocol["planning_context"];
    let result=json!({"contract_version":1,"understanding":{"intent":"BANK_QUERY","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},"plan":{"contract_version":1,"task_id":ctx["task_id"],"plan_version":ctx["plan_version"],"nodes":[{"type":"TOOL","id":"balance","tool":tool,"arguments":{"account_id":account},"depends_on":[]}]},"clarification":null});
    (StatusCode::OK,Json(json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]})))
}
async fn command(s:&Server,c:&str,p:&str,v:Value)->Reply{s.call(Method::POST,p,Some(v.to_string()),Some(c),None,Some("http://127.0.0.1:5174"),true).await}
fn message(text:&str)->Value{json!({"message":text,"selection":{"account_id":"account_001","payee_id":null,"payee_query":null},"context":null})}
async fn bank_server(planner:Option<safeflow_runtime::OpenAiGatewayConfig>)->Server{bank_server_mode(planner,false).await}
pub(super) async fn bank_server_mode(planner:Option<safeflow_runtime::OpenAiGatewayConfig>,conversation:bool)->Server{
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let url=std::env::var("SAFEFLOW_BANK_LLM_TEST_API_URL").expect("owned fixture API URL");
    let ns=std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let source=PgUnifiedFictionalSource::open(PgRepository::connect_existing(&url,&ns).await.unwrap(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await.unwrap();
    let team=TeamConsumptionSource::open(&source,&UserId::parse("user_001").unwrap()).await.unwrap();let spec=team.persistent_for_backend().spec().clone();
    let feedback=BillingFeedbackConfig::for_reviewed_backend(ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),PermitConfig::new(Duration::from_secs(60)).unwrap()).with_recovery_v2(BillingRecoveryConfig::new(&std::env::var("SAFEFLOW_BANK_LLM_TEST_REALM").unwrap_or_else(|_|"878031a0-d397-46b2-b335-c1944f269a5a".into()),&ns,&spec.source_id,&spec.revision,"bank-site-monthly-v1",8,128,60,180,180,360,true).unwrap());
    let mut config=ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));config.bind=address;config.demo_code_display=true;
    let mut state=if let Some(planner)=planner{if conversation {ApiState::for_unified_bank_site_with_conversation(source,config,UserId::parse("user_001").unwrap(),Zeroizing::new("bank-llm-test-controller-unique-123456".into()),feedback,planner).await.unwrap()}else{ApiState::for_unified_bank_site_with_planner(source,config,UserId::parse("user_001").unwrap(),Zeroizing::new("bank-llm-test-controller-unique-123456".into()),feedback,planner).await.unwrap()}}else{ApiState::for_unified_bank_site(source,config,UserId::parse("user_001").unwrap(),Zeroizing::new("bank-llm-test-controller-unique-123456".into()),feedback).await.unwrap()};
    if conversation{let native=state.simulation_session_for_native_host(SECRET).unwrap();state.enable_team_subscription_writes_for_backend(&native,false).await.unwrap();state.stop_native_card_session(&native).await.unwrap();}
    let app=router(state.clone());let handle=tokio::spawn(async move{axum::serve(listener,app).await.unwrap();});Server{state,address,handle}
}
#[tokio::test]
#[ignore="requires a new owned adopted bank PG fixture; no real provider/key"]
async fn real_http_bank_commandcode_readonly_and_failure_boundaries(){
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let endpoint=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let calls=Arc::new(AtomicUsize::new(0));let app=Router::new().route("/v1/chat/completions",post(provider)).with_state(calls.clone());let provider_handle=tokio::spawn(async move{axum::serve(listener,app).await.unwrap();});
    let gateway=||safeflow_runtime::OpenAiGatewayConfig::new(&endpoint,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-not-real".into()),Duration::from_secs(1)).unwrap();
    let off=bank_server(None).await;let cookie=off.login().await;
    let status=ok(off.call(Method::GET,"/api/agent/status",None,Some(&cookie),None,None,false).await);assert_eq!(status["configured"],false);
    assert_eq!(command(&off,&cookie,"/api/agent/messages",message("查询我的余额")).await.status,StatusCode::SERVICE_UNAVAILABLE);assert_eq!(calls.load(Ordering::SeqCst),0);off.handle.abort();
    let server=bank_server(Some(gateway())).await;let cookie=server.login().await;
    let status=ok(server.call(Method::GET,"/api/agent/status",None,Some(&cookie),None,None,false).await);assert_eq!(status["planning_mode"],"OPENAI_COMPATIBLE");assert_eq!(status["draft_enabled"],false);assert_eq!(status["funds_execution_enabled"],false);
    let overview=ok(server.call(Method::GET,"/api/bank/overview",None,Some(&cookie),None,None,false).await);assert_eq!(overview["real_llm"],true);assert_eq!(overview["real_bank"],false);
    let proposed=ok(command(&server,&cookie,"/api/agent/messages",message("查询我的余额")).await);assert_eq!(proposed["kind"],"READ");assert_eq!(proposed["can_start"],true);let id=proposed["task"]["task_id"].as_str().unwrap();
    let t=&proposed["task"];let version=json!({"task_revision":t["revision"],"plan_version":t["plan_version"],"facts_version":t["facts_version"]});
    let completed=ok(command(&server,&cookie,&format!("/api/agent/tasks/{id}/execute"),version.clone()).await);assert_eq!(completed["task"]["task_state"],"COMPLETED");assert_eq!(completed["results"][0]["data"]["available_balance"],"1288.00");assert_eq!(completed["results"][0]["data"]["account"]["account_id"],"account_001");
    let replay=command(&server,&cookie,&format!("/api/agent/tasks/{id}/execute"),version).await;assert!(replay.status.is_success()||replay.status==StatusCode::CONFLICT);assert_eq!(calls.load(Ordering::SeqCst),1);
    for text in ["write","unlock","unknown"]{let r=command(&server,&cookie,"/api/agent/messages",message(text)).await;assert_eq!(r.status,StatusCode::UNPROCESSABLE_ENTITY);}
    let foreign=ok(command(&server,&cookie,"/api/agent/messages",message("foreign")).await);assert_eq!(foreign["kind"],"CLARIFY");
    let mut selected=message("read");selected["selection"]["account_id"]=json!("account_002");let before=calls.load(Ordering::SeqCst);assert_eq!(command(&server,&cookie,"/api/agent/messages",selected).await.status,StatusCode::FORBIDDEN);assert_eq!(calls.load(Ordering::SeqCst),before);
    for text in ["failure","timeout"]{let before=calls.load(Ordering::SeqCst);let r=command(&server,&cookie,"/api/agent/messages",message(text)).await;assert_eq!(r.status,StatusCode::SERVICE_UNAVAILABLE);assert_eq!(calls.load(Ordering::SeqCst),before+1);assert!(!r.json.to_string().contains("DO_NOT_RELAY_PROVIDER_DETAILS"));assert!(!r.json.to_string().contains("fixture-key-not-real"));}
    let draft=ok(command(&server,&cookie,"/api/bank/payment-draft",json!({"nonce":uuid::Uuid::new_v4().to_string()})).await);assert_eq!(draft["parameters"]["amount"],"100.00");
    let path=format!("/api/sim/draft-lifecycles/{}/operations/{}",draft["task_id"].as_str().unwrap(),draft["operation_id"].as_str().unwrap());
    let prepared=ok(command(&server,&cookie,&format!("{path}/prepare"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"})).await);
    super::bank_conversation_tests::capture("legacy_recipient_prepared",&prepared);
    let legacy_unprepared=ok(command(&server,&cookie,"/api/bank/payment-draft",json!({"nonce":uuid::Uuid::new_v4().to_string()})).await);
    super::bank_conversation_tests::capture("legacy_recipient_unprepared",&legacy_unprepared);
    let r=command(&server,&cookie,&format!("{path}/execute"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":prepared["confirmation"]["confirmation_ref"],"task_revision":prepared["task_revision"],"operation_revision":prepared["operation_revision"],"plan_version":prepared["plan_version"],"facts_version":prepared["facts_version"]})).await;assert!(!r.status.is_success());
    // Card prepare is still the original HTTP route; no code/confirmation/permit.
    let card=ok(command(&server,&cookie,"/api/card-safety",json!({"kind":"PREPARE","account_id":"account_001","card_id":"demo_card_salary_v1","nonce":uuid::Uuid::new_v4().to_string()})).await);
    assert!(!command(&server,&cookie,"/api/card-safety",json!({"kind":"EXECUTE_FREEZE","operation_id":card["operation_id"]})).await.status.is_success());
    let after=ok(server.call(Method::GET,"/api/bank/overview",None,Some(&cookie),None,None,false).await);assert_eq!(after["accounts"][0]["balance"]["available_balance"],overview["accounts"][0]["balance"]["available_balance"]);
    server.handle.abort();
    let before=calls.load(Ordering::SeqCst);let restarted=bank_server(Some(gateway())).await;let cookie=restarted.login().await;
    let recovered=ok(restarted.call(Method::GET,&format!("/api/agent/tasks/{id}"),None,Some(&cookie),None,None,false).await);
    assert_eq!(recovered["task"]["task_id"],id);assert_eq!(recovered["results"][0]["data"]["available_balance"],"1288.00");assert_eq!(calls.load(Ordering::SeqCst),before);
    restarted.handle.abort();provider_handle.abort();
}

#[tokio::test]
#[ignore="requires a new owned adopted PG; local provider fixture only"]
async fn site_finish_server_timing_covers_model_plan_tools_and_errors(){
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let calls=Arc::new(AtomicUsize::new(0));
    let app=Router::new().route("/v1/chat/completions",post(provider)).with_state(calls.clone());
    let provider_handle=tokio::spawn(async move{axum::serve(listener,app).await.unwrap();});
    let gateway=safeflow_runtime::OpenAiGatewayConfig::new(&endpoint,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-not-real".into()),Duration::from_secs(1)).unwrap();
    let server=bank_server(Some(gateway)).await;
    fn metric(reply:&Reply,name:&str)->f64 {
        let raw=reply.headers.get("server-timing").unwrap().to_str().unwrap();
        assert!(!raw.contains("fixture") && !raw.contains("account") && !raw.contains("http"));
        raw.split(", ").find_map(|part|part.strip_prefix(&format!("{name};dur="))).unwrap().parse::<f64>().unwrap()
    }
    let denied=server.call(Method::GET,"/api/bank/overview",None,None,None,None,false).await;
    assert_eq!(denied.status,StatusCode::UNAUTHORIZED);assert!(metric(&denied,"total")>=0.0);
    assert_eq!(denied.headers["x-content-type-options"],"nosniff");
    let cookie=server.login().await;
    let proposed=command(&server,&cookie,"/api/agent/messages",message("查询我的余额")).await;
    assert!(metric(&proposed,"model")>0.0);assert!(metric(&proposed,"plan")>=0.0);
    assert!(metric(&proposed,"total")>=metric(&proposed,"model"));
    let v=ok(proposed);let task=&v["task"];let id=task["task_id"].as_str().unwrap();
    let done=command(&server,&cookie,&format!("/api/agent/tasks/{id}/execute"),json!({"task_revision":task["revision"],"plan_version":task["plan_version"],"facts_version":task["facts_version"]})).await;
    assert!(metric(&done,"tools")>0.0);assert!(metric(&done,"total")>=metric(&done,"tools"));
    assert_eq!(ok(done)["results"][0]["data"]["available_balance"],"1288.00");
    let failure=command(&server,&cookie,"/api/agent/messages",message("failure")).await;
    assert_eq!(failure.status,StatusCode::SERVICE_UNAVAILABLE);
    assert!(metric(&failure,"model")>0.0);assert!(metric(&failure,"plan")>=0.0);
    assert_eq!(calls.load(Ordering::SeqCst),2);
    server.handle.abort();provider_handle.abort();
}
