//! Local provider wire + real owned PG + HTTP; no real supplier or credentials.
use super::*;
use std::{str::FromStr, sync::atomic::{AtomicUsize, Ordering}};
fn ok(r: Reply) -> Value { assert!(r.status.is_success(),"{} {}",r.status,r.json);r.json }
async fn post_json(s:&Server,c:&str,path:&str,v:Value)->Reply {
    s.call(Method::POST,path,Some(v.to_string()),Some(c),None,Some("http://127.0.0.1:5174"),true).await
}
async fn get(s:&Server,c:&str,path:&str)->Value {ok(s.call(Method::GET,path,None,Some(c),None,None,false).await)}
fn input(id:&str,revision:u64,message:&str)->Value {json!({"conversation_id":id,"revision":revision,"message":message,"account_id":"account_001"})}
async fn provider(State(calls):State<Arc<AtomicUsize>>,Json(r):Json<Value>)->(StatusCode,Json<Value>) {
    calls.fetch_add(1,Ordering::SeqCst);
    let raw=r.to_string();assert!(!raw.contains("9900000000000001024"));
    let protocol:Value=serde_json::from_str(r["messages"][0]["content"].as_str().unwrap().split("Protocol data: ").nth(1).unwrap()).unwrap();
    assert_eq!(protocol["controlled_task_data"]["scope"],"PROJECT_SIMULATION_READ_ONLY_BANK");
    for d in protocol["approved_tool_definitions"].as_array().unwrap() {assert_eq!(d["has_side_effect"],false);}
    let input:Value=serde_json::from_str(r["messages"][1]["content"].as_str().unwrap()).unwrap();
    let history=input["untrusted_conversation"].as_array().unwrap();let latest=history.last().unwrap().as_str().unwrap();
    if latest=="timeout" {tokio::time::sleep(Duration::from_secs(3)).await;}
    if latest=="pending" {tokio::time::sleep(Duration::from_millis(500)).await;}
    if latest=="failure" {return (StatusCode::BAD_GATEWAY,Json(json!({"private_provider_detail":"DO_NOT_EXPOSE"})));}
    if latest=="clarify" {
        let result=json!({"contract_version":1,"understanding":{"intent":"BANK","entities":{"bank_service":"CARDS"},"missing_fields":[],"context_refs":[],"decision":"CLARIFY"},"plan":null,"clarification":{"question":"请说明需要查询的内容；转账或冻结须核对原任务。"}});
        return (StatusCode::OK,Json(json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]})));
    }
    let ctx=&protocol["planning_context"];
    let read=json!({"type":"TOOL","id":"balance","tool":"get_account_balance","arguments":{"account_id":"account_001"},"depends_on":[]});
    let write=json!({"type":"TOOL","id":"write","tool":"create_mock_transfer_draft","arguments":{"payer_account_id":"account_001","payee_id":"payee_008","amount":"100.00","currency":"CNY"},"depends_on":[]});
    let mut nodes=vec![read.clone()];
    match latest {
        "write" => nodes=vec![write.clone()],
        "mixed" => nodes.push(write.clone()),
        "disguised" => {nodes[0]["arguments"]["action"]=json!("EXECUTE_TRANSFER");},
        "unknown_field" => {nodes[0]["has_side_effect"]=json!(false);},
        "wait" => nodes.push(json!({"type":"USER_DECISION","id":"hidden","depends_on":["balance"]})),
        "foreign" => nodes[0]["arguments"]["account_id"]=json!("account_002"),
        "billing"|"billing_future" => { nodes[0]["tool"]=json!("analyze_local_basic_spending"); nodes[0]["arguments"]=json!({"input_version":1,"account_ids_json":"[\"account_001\"]","start_inclusive":"2026-10-01T00:00:00+08:00","end_exclusive":if latest=="billing_future" {"2026-10-30T00:00:00+08:00"}else{"2026-10-27T00:00:00+08:00"},"timezone":"Asia/Shanghai"}); },
        "unlock_card"|"cancel_mock_transfer_draft"|"modify_mock_transfer_draft"|"execute_transfer"|"execute_confirmed_payment"|"subscription_change"|"save_billing_feedback" => nodes[0]["tool"]=json!(latest),
        _ => {},
    }
    let mut result=json!({"contract_version":1,"understanding":{"intent":"READ_ONLY","entities":{"has_side_effect":false},"missing_fields":[],"context_refs":[],"decision":"PASS"},"plan":{"contract_version":1,"task_id":ctx["task_id"],"plan_version":ctx["plan_version"],"nodes":nodes},"clarification":null});
    if latest=="hidden_clarify" {result["understanding"]["decision"]=json!("CLARIFY");result["clarification"]=json!({"question":"safe"});result["plan"]["nodes"]=json!([write]);}
    (StatusCode::OK,Json(json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]})))
}
#[tokio::test(flavor="multi_thread", worker_threads=4)]
#[ignore="new owned adopted PG, synthetic local model, optional production UI"]
async fn independent_read_chat_unknown_money_and_hostile_plans_real_http() {
    let admin=std::env::var("SAFEFLOW_BANK_CHAT_ADMIN_URL").unwrap();let ns=std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let pool=PgPool::connect_with(sqlx::postgres::PgConnectOptions::from_str(&admin).unwrap().options([("search_path",ns.as_str())])).await.unwrap();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let endpoint=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let calls=Arc::new(AtomicUsize::new(0));let pc=calls.clone();
    let ph=tokio::spawn(async move {axum::serve(listener,Router::new().route("/v1/chat/completions",post(provider)).with_state(pc)).await.unwrap();});
    let gateway=||safeflow_runtime::OpenAiGatewayConfig::new(&endpoint,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-not-real".into()),Duration::from_secs(1)).unwrap();
    let s=super::bank_commandcode_tests::bank_server_mode(Some(gateway()),true).await;let c=s.login().await;
    // Real lifecycle claims stranded by a fixture-only settlement outage.
    // No production mutation or simulated bank funds execution is performed.
    sqlx::query("CREATE FUNCTION independent_read_outage() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'lifecycle_settled' THEN RAISE EXCEPTION 'fixture settlement outage'; END IF; RETURN NEW; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER independent_read_outage BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION independent_read_outage()").execute(&pool).await.unwrap();
    let mut originals=vec![];
    for _ in 0..2 {
        let d=ok(post_json(&s,&c,"/api/bank/payment-draft",json!({"nonce":uuid::Uuid::new_v4().to_string()})).await);
        let base=format!("/api/sim/draft-lifecycles/{}/operations/{}",d["task_id"].as_str().unwrap(),d["operation_id"].as_str().unwrap());
        let p=ok(post_json(&s,&c,&(base.clone()+"/prepare"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"})).await);
        let d=ok(post_json(&s,&c,&(base.clone()+"/confirm"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":p["confirmation"]["confirmation_ref"],"display_ticket":p["display_ticket"],"action":"CONFIRM"})).await);
        let r=post_json(&s,&c,&(base+"/execute"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":d["confirmation"]["confirmation_ref"],"task_revision":d["task_revision"],"operation_revision":d["operation_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"]})).await;
        assert!(!r.status.is_success());originals.push(d["task_id"].clone());
    }
    sqlx::query("DROP TRIGGER independent_read_outage ON audit_events").execute(&pool).await.unwrap();
    for id in &originals {assert_eq!(get(&s,&c,&format!("/api/agent/tasks/{}",id.as_str().unwrap())).await["draft"]["execution_status"],"RESULT_UNKNOWN");}
    let snapshot="SELECT jsonb_build_object('accounts',(SELECT jsonb_agg(to_jsonb(t) ORDER BY account_id) FROM transfer_accounts_v1 t),'drafts',(SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY draft_id),'[]') FROM transfer_drafts_v1 t),'claims',(SELECT jsonb_agg(to_jsonb(t) ORDER BY operation_id) FROM mock_draft_lifecycle_claims t),'orders',(SELECT count(*) FROM transfer_fund_orders_v1),'ledger',(SELECT count(*) FROM transfer_ledger_v1),'locks',(SELECT count(*) FROM card_locks_v1),'feedback',(SELECT count(*) FROM billing_feedback_v1),'subscriptions',(SELECT count(*) FROM subscription_public_bindings_v1),'grants',(SELECT count(*) FROM payment_completion_candidate_grants))";
    let before:Value=sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();
    let id=uuid::Uuid::new_v4().to_string();
    let first=ok(post_json(&s,&c,"/api/bank/read-conversation",input(&id,0,"请告诉我现在还能使用的资金有多少")).await);
    assert_eq!(first["view"]["task"]["task_state"],"COMPLETED");assert_eq!(first["view"]["results"][0]["data"]["available_balance"],"1288.00");
    super::bank_conversation_tests::capture("independent_read",&first);
    assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",input(&id,0,"again")).await.status,StatusCode::CONFLICT);
    let clarify=ok(post_json(&s,&c,"/api/bank/read-conversation",input(&id,1,"clarify")).await);assert_eq!(clarify["view"]["kind"],"CLARIFY");assert!(clarify["view"]["bank_service"].is_null());
    let again=ok(post_json(&s,&c,"/api/bank/read-conversation",input(&id,2,"再核对一下可用资金")).await);assert_eq!(again["view"]["task"]["task_state"],"COMPLETED");
    let bill=ok(post_json(&s,&c,"/api/bank/read-conversation",input(&uuid::Uuid::new_v4().to_string(),0,"billing")).await);
    assert_eq!(bill["view"]["task"]["task_state"],"COMPLETED");assert_eq!(bill["view"]["results"][0]["tool_name"],"analyze_local_basic_spending");
    super::bank_conversation_tests::capture("independent_billing",&bill);
    assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",input(&uuid::Uuid::new_v4().to_string(),0,"billing_future")).await.status,StatusCode::FORBIDDEN);
    for text in ["write","mixed","disguised","unknown_field","wait","hidden_clarify","unlock_card","cancel_mock_transfer_draft","modify_mock_transfer_draft","execute_transfer","execute_confirmed_payment","subscription_change","save_billing_feedback"] {
        let id=uuid::Uuid::new_v4().to_string();let count:i64=sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&pool).await.unwrap();
        let r=post_json(&s,&c,"/api/bank/read-conversation",input(&id,0,text)).await;
        assert_eq!(r.status,StatusCode::UNPROCESSABLE_ENTITY,"{text} {}",r.json);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM tasks").fetch_one(&pool).await.unwrap(),count);
        let after:Value=sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();assert_eq!(before,after);
    }
    for field in ["context","mode","scope","selection"] {
        let mut v=input(&uuid::Uuid::new_v4().to_string(),0,"read");v[field]=json!("READ_ONLY");
        assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",v).await.status,StatusCode::BAD_REQUEST);
    }
    let mut v=input(&uuid::Uuid::new_v4().to_string(),0,"read");v["account_id"]=json!("account_002");let n=calls.load(Ordering::SeqCst);
    assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",v).await.status,StatusCode::FORBIDDEN);assert_eq!(calls.load(Ordering::SeqCst),n);
    let mut unresolved=vec![];
    for text in ["failure","timeout"] {
        let id=uuid::Uuid::new_v4().to_string();let n=calls.load(Ordering::SeqCst);
        let r=post_json(&s,&c,"/api/bank/read-conversation",input(&id,0,text)).await;assert_eq!(r.status,StatusCode::SERVICE_UNAVAILABLE);assert!(!r.json.to_string().contains("DO_NOT_EXPOSE"));
        assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",input(&id,0,text)).await.status,StatusCode::CONFLICT);
        let j=get(&s,&c,&format!("/api/bank/read-conversation/{id}")).await;assert_eq!(j["uncertain"],true);assert!(j["view"].is_null());assert_eq!(calls.load(Ordering::SeqCst),n+1);
        unresolved.push(id);
    }
    let journal=get(&s,&c,&format!("/api/bank/read-conversation/{id}")).await;assert_eq!(journal["view"]["task"]["task_id"],again["view"]["task"]["task_id"]);
    let other=s.login().await;
    assert_eq!(s.call(Method::GET,&format!("/api/bank/read-conversation/{id}"),None,Some(&other),None,None,false).await.status,StatusCode::FORBIDDEN);
    assert_eq!(s.call(Method::GET,&format!("/api/bank/read-conversation/{id}"),None,None,None,None,false).await.status,StatusCode::UNAUTHORIZED);
    let directory=get(&s,&c,"/api/bank/recovery").await;assert_eq!(directory["payment_tasks"].as_array().unwrap().len(),2);
    // Read journal is bounded independently of the money recovery directory.
    let bounded=uuid::Uuid::new_v4().to_string();
    for revision in 0..12 { assert_eq!(ok(post_json(&s,&c,"/api/bank/read-conversation",input(&bounded,revision,"clarify")).await)["revision"],revision+1); }
    let count=calls.load(Ordering::SeqCst);
    assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",input(&bounded,12,"clarify")).await.status,StatusCode::CONFLICT);
    assert_eq!(calls.load(Ordering::SeqCst),count);
    let status=get(&s,&c,"/api/agent/status").await;assert_eq!(status["draft_enabled"],true);assert_eq!(status["payment_planning_enabled"],true);
    let outbox:i64=sqlx::query_scalar("SELECT count(*) FROM public_audit_outbox").fetch_one(&pool).await.unwrap();assert!(outbox>0);
    super::bank_conversation_tests::capture("independent_summary",&json!({"originals":originals,"before":before,"after":sqlx::query_scalar::<_,Value>(snapshot).fetch_one(&pool).await.unwrap(),"outbox":outbox,"hostile_plans_rejected":13,"provider_calls":calls.load(Ordering::SeqCst)}));
    // Optional full production page against this actual bank HTTP + PG + provider.
    if let Ok(script)=std::env::var("SAFEFLOW_INDEPENDENT_UI_SCRIPT") {
        let address=s.address;
        let result=tokio::task::spawn_blocking(move||std::process::Command::new("node.exe").arg(script).env("SF_TEST_BANK",format!("http://{address}")).status().unwrap()).await.unwrap();
        assert!(result.success(),"production UI with actual PG HTTP failed");
    }
    let after:Value=sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();assert_eq!(before,after);
    // Review regression: rejected wrapped input must reserve no journal slot,
    // call no model/tool, and create no task/Outbox event. Include JSON escaping.
    let count=calls.load(Ordering::SeqCst);
    let tasks:i64=sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&pool).await.unwrap();
    let outbox_before:i64=sqlx::query_scalar("SELECT count(*) FROM public_audit_outbox").fetch_one(&pool).await.unwrap();
    for n in 0..300 {
        let rejected=uuid::Uuid::new_v4().to_string();
        let text=if n%2==0 {"a".repeat(4000)}else{"\"".repeat(2000)};
        assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",input(&rejected,0,&text)).await.status,StatusCode::BAD_REQUEST);
        assert_eq!(s.call(Method::GET,&format!("/api/bank/read-conversation/{rejected}"),None,Some(&c),None,None,false).await.status,StatusCode::NOT_FOUND);
    }
    // Fail only after including accepted history, without changing that history
    // or revision. The single new message is below the first-turn prompt limit.
    let accepted=get(&s,&c,&format!("/api/bank/read-conversation/{id}")).await;
    let accumulated="a".repeat(3650);
    assert_eq!(post_json(&s,&c,"/api/bank/read-conversation",input(&id,3,&accumulated)).await.status,StatusCode::BAD_REQUEST);
    assert_eq!(get(&s,&c,&format!("/api/bank/read-conversation/{id}")).await,accepted);
    assert_eq!(calls.load(Ordering::SeqCst),count);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM tasks").fetch_one(&pool).await.unwrap(),tasks);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM public_audit_outbox").fetch_one(&pool).await.unwrap(),outbox_before);
    assert_eq!(sqlx::query_scalar::<_,Value>(snapshot).fetch_one(&pool).await.unwrap(),before);
    assert_eq!(ok(post_json(&s,&c,"/api/bank/read-conversation",input(&id,3,"请继续核对余额")).await)["revision"],4);
    let relogin=s.login().await;
    assert_eq!(ok(post_json(&s,&relogin,"/api/bank/read-conversation",input(&uuid::Uuid::new_v4().to_string(),0,"余额是多少")).await)["view"]["task"]["task_state"],"COMPLETED");
    // A real in-flight turn remains reserved. A duplicate cannot replace it.
    let pending=uuid::Uuid::new_v4().to_string();let n=calls.load(Ordering::SeqCst);
    let (a,b)=tokio::join!(post_json(&s,&c,"/api/bank/read-conversation",input(&pending,0,"pending")),async {
        for _ in 0..100 {if calls.load(Ordering::SeqCst)>n {break;}tokio::time::sleep(Duration::from_millis(2)).await;}
        let duplicate=post_json(&s,&c,"/api/bank/read-conversation",input(&pending,0,"clarify")).await;
        let invalid=post_json(&s,&c,"/api/bank/read-conversation",input(&pending,0,&"a".repeat(4000))).await;
        (duplicate,invalid)
    });
    assert_eq!(ok(a)["revision"],1);assert_eq!(b.0.status,StatusCode::CONFLICT);assert_eq!(b.1.status,StatusCode::BAD_REQUEST);
    assert_eq!(calls.load(Ordering::SeqCst),n+1);
    assert_eq!(get(&s,&c,&format!("/api/bank/read-conversation/{pending}")).await["revision"],1);
    let n=calls.load(Ordering::SeqCst);
    for failed in &unresolved {
        assert_eq!(get(&s,&c,&format!("/api/bank/read-conversation/{failed}")).await["uncertain"],true);
    }
    assert_eq!(calls.load(Ordering::SeqCst),n);
    // Use actual accepted CLARIFY turns to reach the original global bound.
    // This happens after UI tests, so it cannot consume their test capacity.
    let mut accepted_ids=std::collections::HashSet::new();let n=calls.load(Ordering::SeqCst);
    for _ in 0..257 {
        let candidate=uuid::Uuid::new_v4().to_string();
        let r=post_json(&s,&relogin,"/api/bank/read-conversation",input(&candidate,0,"clarify")).await;
        if r.status==StatusCode::CONFLICT {break;}
        assert_eq!(ok(r)["revision"],1);accepted_ids.insert(candidate);
    }
    assert!(!accepted_ids.is_empty() && accepted_ids.len()<256);
    assert_eq!(calls.load(Ordering::SeqCst)-n,accepted_ids.len());
    assert_eq!(post_json(&s,&relogin,"/api/bank/read-conversation",input(&uuid::Uuid::new_v4().to_string(),0,"clarify")).await.status,StatusCode::CONFLICT);
    // Capacity handling must preserve earlier accepted and unresolved turns.
    assert_eq!(get(&s,&c,&format!("/api/bank/read-conversation/{id}")).await["revision"],4);
    assert_eq!(get(&s,&c,&format!("/api/bank/read-conversation/{bounded}")).await["revision"],12);
    assert_eq!(get(&s,&c,&format!("/api/bank/read-conversation/{pending}")).await["revision"],1);
    super::bank_conversation_tests::capture("journal_repair",&json!({"rejected_new_ids":300,"rejected_GET":"NOT_FOUND","no_model_tasks_or_outbox_on_rejection":true,"existing_revision_preserved":true,"relogin_read_completed":true,"pending_duplicate_preserved":true,"capacity_additional_accepted":accepted_ids.len(),"capacity_limit_preserved":256,"turn_limit_preserved":12,"money_before":before,"money_after":sqlx::query_scalar::<_,Value>(snapshot).fetch_one(&pool).await.unwrap()}));
    s.handle.abort();let restarted=super::bank_commandcode_tests::bank_server_mode(Some(gateway()),true).await;let rc=restarted.login().await;
    for id in originals {assert_eq!(get(&restarted,&rc,&format!("/api/agent/tasks/{}",id.as_str().unwrap())).await["draft"]["execution_status"],"RESULT_UNKNOWN");}
    assert_eq!(restarted.call(Method::GET,&format!("/api/bank/read-conversation/{id}"),None,Some(&rc),None,None,false).await.status,StatusCode::NOT_FOUND);
    // A fresh process journal still accepts exactly 256 genuine conversations.
    let n=calls.load(Ordering::SeqCst);let mut first_id=String::new();
    for index in 0..256 {
        let id=uuid::Uuid::new_v4().to_string();if index==0 {first_id=id.clone();}
        assert_eq!(ok(post_json(&restarted,&rc,"/api/bank/read-conversation",input(&id,0,"clarify")).await)["revision"],1);
    }
    assert_eq!(post_json(&restarted,&rc,"/api/bank/read-conversation",input(&uuid::Uuid::new_v4().to_string(),0,"clarify")).await.status,StatusCode::CONFLICT);
    assert_eq!(calls.load(Ordering::SeqCst),n+256);
    assert_eq!(get(&restarted,&rc,&format!("/api/bank/read-conversation/{first_id}")).await["revision"],1);
    super::bank_conversation_tests::capture("journal_exact_capacity",&json!({"accepted":256,"next_status":409,"provider_calls":256,"earliest_preserved":true}));
    restarted.handle.abort();ph.abort();pool.close().await;
}
