//! Real registered bank tools + actual HTTP/PG reads; local protocol provider only.
use super::*;
use std::{collections::HashSet,net::SocketAddr,sync::Mutex};
use axum::extract::ConnectInfo;

#[derive(Default)]
struct Trace{peers:Mutex<HashSet<SocketAddr>>,requests:Mutex<Vec<Value>>}
async fn provider(State(t):State<Arc<Trace>>,ConnectInfo(peer):ConnectInfo<SocketAddr>,Json(request):Json<Value>)->Json<Value>{
    t.peers.lock().unwrap().insert(peer);t.requests.lock().unwrap().push(request.clone());
    let p:Value=serde_json::from_str(request["messages"][0]["content"].as_str().unwrap().split("Protocol data: ").nth(1).unwrap()).unwrap();
    let ctx=&p["planning_context"];
    let result=json!({"contract_version":1,"understanding":{"intent":"BALANCE_QUERY","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},
        "plan":{"contract_version":1,"task_id":ctx["task_id"],"plan_version":ctx["plan_version"],"nodes":[{"type":"TOOL","id":"balance","tool":"get_account_balance","arguments":{"account_id":"account_001"},"depends_on":[]}]},"clarification":null});
    Json(json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]}))
}
#[tokio::test]
#[ignore="new owned adopted PG with original minimum API role and local protocol provider"]
async fn site_repairs_full_bank_registry_compact_protocol_single_call_and_actual_read(){
    let t=Arc::new(Trace::default());let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let app=Router::new().route("/v1/chat/completions",post(provider)).with_state(t.clone());
    let ph=tokio::spawn(async move{axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();});
    let gateway=safeflow_runtime::OpenAiGatewayConfig::new(&url,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-no-provider-access".into()),Duration::from_secs(2)).unwrap();
    let s=bank_commandcode_tests::bank_server_mode(Some(gateway),true).await;let c=s.login().await;
    for _ in 0..2{
        let r=s.call(Method::POST,"/api/bank/conversation",Some(json!({"conversation_id":uuid::Uuid::new_v4().to_string(),"revision":0,"message":"查询我的余额","selection":{"account_id":"account_001","payee_id":null,"payee_query":null},"context":null}).to_string()),Some(&c),None,Some("http://127.0.0.1:5174"),true).await;
        assert_eq!(r.status,StatusCode::OK,"{}",r.json);assert_eq!(r.json["view"]["kind"],"READ");
        let task=&r.json["view"]["task"];
        let done=s.call(Method::POST,&format!("/api/agent/tasks/{}/execute",task["task_id"].as_str().unwrap()),Some(json!({"task_revision":task["revision"],"plan_version":task["plan_version"],"facts_version":task["facts_version"]}).to_string()),Some(&c),None,Some("http://127.0.0.1:5174"),true).await;
        assert_eq!(done.status,StatusCode::OK,"{}",done.json);
        assert_eq!(done.json["task"]["task_state"],"COMPLETED");
        assert_eq!(done.json["results"][0]["tool_name"],"get_account_balance");
    }
    let requests=t.requests.lock().unwrap();assert_eq!(requests.len(),2);assert_eq!(t.peers.lock().unwrap().len(),1);
    let p:Value=serde_json::from_str(requests[0]["messages"][0]["content"].as_str().unwrap().split("Protocol data: ").nth(1).unwrap()).unwrap();
    let tools=p["approved_tool_definitions"].as_array().unwrap();assert!(tools.len()>3);
    let mut expanded=p.clone();
    for (index,tool) in expanded["approved_tool_definitions"].as_array_mut().unwrap().iter_mut().enumerate(){
        assert_eq!(tool["arguments_variant"],index);
        let schema=p.pointer(&format!("/planning_result_json_schema/$defs/plan/properties/nodes/items/oneOf/{index}/properties/arguments")).unwrap();
        assert!(schema.is_object());tool.as_object_mut().unwrap().remove("arguments_variant");tool["input_schema"]=schema.clone();
    }
    let compact=serde_json::to_vec(&p).unwrap().len();let old=serde_json::to_vec(&expanded).unwrap().len();assert!(compact<old);
    println!("FULL_BANK_PROTOCOL {}",json!({"registered_tools":tools.len(),"requests":2,"tcp_connections":1,"compact_protocol_bytes":compact,"same_protocol_duplicated_schema_bytes":old,"saved_bytes":old-compact,"real_provider":false,"actual_bank_read":true}));
    drop(requests);s.handle.abort();ph.abort();
}
