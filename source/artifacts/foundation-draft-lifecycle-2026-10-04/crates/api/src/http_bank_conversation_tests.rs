//! Conversational wire fixture + real owned PG + original business services.
use super::*;
use safeflow_storage::{
    PaymentBackgroundActor, PaymentBackgroundDeployment, PaymentBackgroundRole,
    PgUnifiedFictionalSource, UNIFIED_FICTIONAL_REVISION, UNIFIED_FICTIONAL_SOURCE,
};
use std::{
    str::FromStr,
    sync::atomic::{AtomicUsize, Ordering},
};
fn ok(r: Reply) -> Value {
    assert!(r.status.is_success(), "{} {}", r.status, r.json);
    r.json
}
async fn command(s: &Server, c: &str, p: &str, v: Value) -> Reply {
    s.call(
        Method::POST,
        p,
        Some(v.to_string()),
        Some(c),
        None,
        Some("http://127.0.0.1:5174"),
        true,
    )
    .await
}
async fn query(s: &Server, c: &str, p: &str) -> Value {
    ok(s.call(Method::GET, p, None, Some(c), None, None, false)
        .await)
}
async fn provider(State(calls): State<Arc<AtomicUsize>>, Json(r): Json<Value>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    assert!(!r.to_string().contains("9900000000000001024"));
    assert!(!r.to_string().contains("simulation_card_number"));
    let protocol: Value = serde_json::from_str(
        r["messages"][0]["content"]
            .as_str()
            .unwrap()
            .split("Protocol data: ")
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    let ctx = &protocol["planning_context"];
    let data = &protocol["controlled_task_data"];
    assert_eq!(data["scope"], "PROJECT_SIMULATION_NO_FUNDS");
    let input: Value = serde_json::from_str(r["messages"][1]["content"].as_str().unwrap()).unwrap();
    let history = input["untrusted_conversation"].as_array().unwrap();
    let latest = history.last().unwrap().as_str().unwrap();
    let all = history
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!all.contains("fixture-otp-secret"));
    if latest.contains("迟到") { tokio::time::sleep(Duration::from_millis(250)).await; }
    if latest.contains("模型失败") {
        return Json(json!({"fixture_error":"unavailable"}));
    }
    if latest.contains("超时") {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    if latest == "我要修改金额" {
        assert!(!data["current_draft"].is_null());
        let result = json!({"contract_version":1,"understanding":{"intent":"BANK","entities":{},"missing_fields":[],"context_refs":[],"decision":"CLARIFY"},"plan":null,"clarification":{"question":"改成多少元？"}});
        return Json(
            json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]}),
        );
    }
    let service = if latest.contains("银行卡") {
        Some("CARDS")
    } else if latest.contains("订阅") {
        Some("SUBSCRIPTIONS")
    } else if latest.contains("月度") {
        Some("BILLS")
    } else {
        None
    };
    let (tool, mut args) = if latest.contains("余额") {
        (
            "get_account_balance",
            json!({"account_id":data["selection"]["account_id"]}),
        )
    } else if latest.contains("取消") {
        (
            "cancel_mock_transfer_draft",
            json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"]}),
        )
    } else if latest.contains("改成") || (latest == "200元" && !data["current_draft"].is_null())
    {
        (
            "modify_mock_transfer_draft",
            json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"],"changes":{"amount":"200.00"}}),
        )
    } else if data["selection"]["payee_id"].is_null() {
        ("find_payees", json!({"query":"小李"}))
    } else {
        (
            "create_mock_transfer_draft",
            json!({"payer_account_id":data["selection"]["account_id"],"payee_id":data["selection"]["payee_id"],"amount":if latest == "200元" {"200.00"} else {"100.00"},"currency":"CNY"}),
        )
    };
    if tool == "create_mock_transfer_draft" {
        if latest.contains("空备注") {args["remark"] = json!(""); args["replaces"] = Value::Null;}
        if latest.contains("指定备注") {args["remark"] = json!("用户指定备注");}
        if latest.contains("未知字段") {args["unexpected"] = json!(true);}
        if latest.contains("错误替换") {args["replaces"] = json!({"draft_id":"untrusted"});}
    }
    let clarify =
        service.is_some() || (!all.contains("100") && tool == "create_mock_transfer_draft");
    let result = if clarify {
        json!({"contract_version":1,"understanding":{"intent":"BANK","entities":service.map(|v|json!({"bank_service":v})).unwrap_or(json!({})),"missing_fields":[],"context_refs":[],"decision":"CLARIFY"},"plan":null,"clarification":{"question":if service.is_some(){"请在下方核对实际数据并选择操作。"}else{"请补充转账金额。"}}})
    } else {
        json!({"contract_version":1,"understanding":{"intent":"BANK","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},"plan":{"contract_version":1,"task_id":ctx["task_id"],"plan_version":ctx["plan_version"],"nodes":[{"type":"TOOL","id":"proposal","tool":tool,"arguments":args,"depends_on":[]}]},"clarification":null})
    };
    Json(
        json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]}),
    )
}
fn input(id: &str, revision: u64, message: &str, selected: bool, context: Value) -> Value {
    json!({"conversation_id":id,"revision":revision,"message":message,"selection":{"account_id":"account_001","payee_id":if selected{json!("payee_008")}else{Value::Null},"payee_query":if selected{json!("小李")}else{Value::Null}},"context":context})
}
fn context(d: &Value) -> Value {
    json!({"task_id":d["task_id"],"task_revision":d["task_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"],"selection_ticket":d["selection_ticket"]})
}
async fn save_draft(s: &Server, c: &str, d: &Value) -> Value {
    let path = format!(
        "/api/sim/draft-lifecycles/{}/operations/{}",
        d["task_id"].as_str().unwrap(),
        d["operation_id"].as_str().unwrap()
    );
    let p = ok(command(
        s,
        c,
        &(path.clone() + "/prepare"),
        json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"}),
    )
    .await);
    if d["action"] != "CANCEL_MOCK_DRAFT" {
        assert_eq!(p["confirmation_recipient"]["display_name"], "李明远");
        assert_eq!(p["confirmation_recipient"]["simulation_card_number"], "9900000000000001024");
        assert_eq!(p["confirmation_recipient"]["masked_account_no"], "**** 1024");
    }
    capture("prepared_draft", &p);
    capture(
        &format!(
            "lifecycle_{}_{}_prepared",
            d["task_id"].as_str().unwrap(),
            d["plan_version"]
        ),
        &p,
    );
    let confirmed=ok(command(s,c,&(path.clone()+"/confirm"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":p["confirmation"]["confirmation_ref"],"display_ticket":p["display_ticket"],"action":"CONFIRM"})).await);
    capture("confirmed_draft", &confirmed);
    capture(
        &format!(
            "lifecycle_{}_{}_confirmed",
            d["task_id"].as_str().unwrap(),
            d["plan_version"]
        ),
        &confirmed,
    );
    let reply=command(s,c,&(path+"/execute"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":confirmed["confirmation"]["confirmation_ref"],"task_revision":confirmed["task_revision"],"operation_revision":confirmed["operation_revision"],"plan_version":confirmed["plan_version"],"facts_version":confirmed["facts_version"]})).await;
    if !reply.status.is_success() {
        let store=&s.state.0.draft.as_ref().unwrap().store;
        let context=safeflow_contracts::ExecutionContext {
            task_id: TaskId::parse(d["task_id"].as_str().unwrap()).unwrap(),
            node_id: safeflow_contracts::NodeId::parse(format!("action_{}",d["plan_version"])).unwrap(),
            operation_id: safeflow_contracts::OperationId::parse(d["operation_id"].as_str().unwrap()).unwrap(),
            authenticated_user_id: UserId::parse("user_001").unwrap(),
            plan_version: d["plan_version"].as_u64().unwrap() as u32, idempotency_key: None,
        };
        let memory=store.read_created_for_task(&context).await.unwrap();
        capture("execution_failure",&json!({"http_status":reply.status.as_u16(),"response":reply.json,"memory_receipts":store.saved_receipt_count().unwrap(),"memory_draft_exists":memory.is_some(),"task_id":d["task_id"],"operation_id":d["operation_id"]}));
    }
    let result=ok(reply);
    capture(
        &format!(
            "lifecycle_{}_{}_saved",
            d["task_id"].as_str().unwrap(),
            d["plan_version"]
        ),
        &result,
    );
    result
}
pub(super) fn capture(key: &str, value: &Value) {
    if let Ok(path) = std::env::var("SAFEFLOW_BANK_CHAT_UI_FIXTURE") {
        let mut data: Value = std::fs::read(&path)
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or(json!({"kind":"OWNED_LOCAL_PROTOCOL_FIXTURE_NOT_LIVE_ACCEPTANCE"}));
        if data.get(key).is_none() {
            data[key] = value.clone();
        }
        std::fs::write(path, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
    }
}
#[tokio::test]
#[ignore = "new owned adopted PG, synthetic provider only"]
async fn bank_conversation_chinese_clarification_payment_original_and_services() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(calls.clone());
    let ph = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let gateway = || {
        safeflow_runtime::OpenAiGatewayConfig::new(
            &endpoint,
            "deepseek/deepseek-v4.1-flash",
            Zeroizing::new("fixture-key-not-real".into()),
            Duration::from_secs(2),
        )
        .unwrap()
    };
    let s = super::bank_commandcode_tests::bank_server_mode(Some(gateway()), true).await;
    let cookie = s.login().await;
    let id = uuid::Uuid::new_v4().to_string();
    // Start naturally without a manually selected payee: original find_payees.
    let first = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 0, "给小李转账", false, Value::Null),
    )
    .await);
    assert_eq!(first["view"]["kind"], "READ");
    assert_eq!(first["payee_query"], "小李");
    let t = &first["view"]["task"];
    let found=ok(command(&s,&cookie,&format!("/api/agent/tasks/{}/execute",t["task_id"].as_str().unwrap()),json!({"task_revision":t["revision"],"plan_version":t["plan_version"],"facts_version":t["facts_version"]})).await);
    assert_eq!(
        found["results"][0]["data"]["candidates"][0]["payee_id"],
        "payee_008"
    );
    assert_eq!(found["results"][0]["data"]["candidates"][0]["display_name"], "李明远");
    assert!(!found.to_string().contains("9900000000000001024"));
    capture("lookup", &first);
    capture("lookup_result", &found);
    assert_eq!(
        command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&id, 0, "重复", false, Value::Null)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let clarify = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 1, "使用所选收款人", true, Value::Null),
    )
    .await);
    assert_eq!(clarify["view"]["kind"], "CLARIFY");
    let proposed = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 2, "100元", true, Value::Null),
    )
    .await);
    assert_eq!(proposed["view"]["kind"], "DRAFT");
    capture("created", &proposed);
    let d = save_draft(&s, &cookie, &proposed["view"]["draft"]).await;
    capture("saved_draft", &d);
    assert_eq!(d["execution_status"], "SUCCEEDED");
    assert!(d.get("confirmation_recipient").is_none());
    assert!(d["parameters"].get("remark").is_none());
    assert!(d["parameters"].get("replaces").is_none());
    assert_eq!(d["parameters"]["amount"], "100.00");
    let task = d["task_id"].as_str().unwrap();
    let p=ok(command(&s,&cookie,&format!("/api/sim/payment-preparations/{task}"),json!({"contract_version":1,"scope":"PAYMENT_INTENT_ONLY","source":{"selection_ticket":d["selection_ticket"],"task_revision":d["task_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"],"draft_id":d["receipt"]["draft_id"],"draft_version":d["receipt"]["draft_version"]}})).await);
    capture("prepared_payment", &p);
    let base = format!(
        "/api/sim/payment-preparations/{task}/{}",
        p["operation"]["operation_id"].as_str().unwrap()
    );
    let bg = base.clone() + "/background";
    let execution = |display: &Value| json!({"contract_version":1,"scope":"EXPLICIT_PAYMENT_BACKGROUND_V1","action":{"kind":"EXECUTE","consent_id":display["consent_id"],"confirmation_ref":p["confirmation_ref"],"display_ticket":p["display_ticket"]}});
    assert!(!command(
        &s,
        &cookie,
        &bg,
        execution(&json!({"consent_id":"not-authority"}))
    )
    .await
    .status
    .is_success());
    let confirmed_payment=ok(command(&s,&cookie,&(base.clone()+"/confirm"),json!({"contract_version":1,"scope":"PAYMENT_INTENT_ONLY","confirmation_ref":p["confirmation_ref"],"display_ticket":p["display_ticket"],"action":"CONFIRM"})).await);
    capture("confirmed_payment", &confirmed_payment);
    let display=ok(command(&s,&cookie,&bg,json!({"contract_version":1,"scope":"EXPLICIT_PAYMENT_BACKGROUND_V1","action":{"kind":"DISPLAY"}})).await);
    capture("background_display", &display);
    ok(command(&s,&cookie,&bg,json!({"contract_version":1,"scope":"EXPLICIT_PAYMENT_BACKGROUND_V1","action":{"kind":"CONFIRM","shown":display}})).await);
    assert!(!command(&s, &cookie, &bg, execution(&display))
        .await
        .status
        .is_success());
    let code=ok(command(&s,&cookie,"/api/demo-verification",json!({"kind":"PAYMENT","task_id":task,"operation_id":p["operation"]["operation_id"],"consent_id":display["consent_id"]})).await);
    let verified=ok(command(&s,&cookie,&bg,json!({"contract_version":1,"scope":"EXPLICIT_PAYMENT_BACKGROUND_V1","action":{"kind":"VERIFY_CODE","consent_id":display["consent_id"],"code":code["code"]}})).await);
    assert_eq!(verified["verified"], true);
    // Separate real SCRAM principals complete this exact original; no API
    // response, timer or model output is used as an arrival receipt.
    let url = std::env::var("SAFEFLOW_BANK_CHAT_ADMIN_URL").unwrap();
    let ns = std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let repo = PgRepository::connect_existing(&url, &ns).await.unwrap();
    let source = PgUnifiedFictionalSource::open(
        repo.clone(),
        UNIFIED_FICTIONAL_SOURCE,
        UNIFIED_FICTIONAL_REVISION,
    )
    .await
    .unwrap();
    let deploy = PaymentBackgroundDeployment::for_isolated_backend(source);
    let mut actors = vec![];
    for role in [
        PaymentBackgroundRole::Producer,
        PaymentBackgroundRole::Worker,
    ] {
        let name = format!("sf_chat_{}", uuid::Uuid::new_v4().simple());
        let password = Zeroizing::new(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ));
        deploy
            .register_new_login(&name, role, password.clone())
            .await
            .unwrap();
        if role == PaymentBackgroundRole::Worker {
            deploy
                .enable_final_completion_for_login(&name)
                .await
                .unwrap();
        }
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .username(&name)
            .password(&password);
        actors.push((
            name,
            PaymentBackgroundActor::authenticate(options, &ns, role)
                .await
                .unwrap(),
        ));
    }
    let run = uuid::Uuid::new_v4();
    deploy
        .enable_automatic_simulation(run, &actors[0].0, &actors[1].0)
        .await
        .unwrap();
    let accepted = ok(command(&s, &cookie, &bg, execution(&display)).await);
    assert_eq!(accepted["operation"]["state"], "PROCESSING");
    capture("accepted", &accepted);
    // Manual ChatPayment preparation is not an agent intent: agent GET remains
    // DRAFT, so restoration must independently GET and verify payment state.
    assert!(p["agent_binding"].is_null());
    let readonly_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::postgres::PgConnectOptions::from_str(&url)
                .unwrap()
                .options([("search_path", ns.as_str())]),
        )
        .await
        .unwrap();
    let snapshot_sql = "SELECT json_build_object('orders',(SELECT count(*) FROM transfer_fund_orders_v1),'ledger',(SELECT count(*) FROM transfer_ledger_v1),'balance',(SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='account_001'))";
    let before_reads: Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(&readonly_pool)
        .await
        .unwrap();
    for index in 0..2 {
        let projection = query(&s, &cookie, &format!("/api/agent/tasks/{task}")).await;
        let preparations = query(
            &s,
            &cookie,
            &format!("/api/sim/payment-preparations/{task}"),
        )
        .await;
        let current = query(&s, &cookie, &(base.clone() + "/funds-result")).await;
        assert_eq!(projection["kind"], "DRAFT");
        assert!(projection["payment_flow"].is_null());
        assert_eq!(projection["draft"]["task_id"], d["task_id"]);
        assert_eq!(projection["draft"]["plan_version"], d["plan_version"]);
        assert_eq!(preparations.as_array().unwrap().len(), 1);
        assert!(preparations[0]["agent_binding"].is_null());
        assert_eq!(current["state"], "PROCESSING");
        assert_eq!(current["task_id"], d["task_id"]);
        assert_eq!(
            current["preparation_operation_id"],
            p["operation"]["operation_id"]
        );
        capture(&format!("repair2_processing_agent_{index}"), &projection);
        capture("repair2_processing_preparations", &preparations);
        capture("repair2_processing_funds", &current);
    }
    let after_reads: Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(&readonly_pool)
        .await
        .unwrap();
    assert_eq!(before_reads, after_reads);
    let read_summary = json!({"manual_agent_binding":p["agent_binding"],"agent_kind":"DRAFT","payment_flow":null,"task":task,"plan_version":d["plan_version"],"funds_state":"PROCESSING","before":before_reads,"after":after_reads,"repeated_gets_only":true});
    capture("repair2_projection_summary", &read_summary);
    println!("REPAIR2_MANUAL_PROJECTION {}", read_summary);
    readonly_pool.close().await;

    let again = command(&s, &cookie, &bg, execution(&display)).await;
    assert!(again.status.is_success() || again.status == StatusCode::CONFLICT);
    assert_eq!(
        actors[0]
            .1
            .automatic_simulation_tick(run, "")
            .await
            .unwrap()["decisions"],
        1
    );
    assert_eq!(
        actors[1]
            .1
            .automatic_simulation_tick(run, "")
            .await
            .unwrap()["completed"],
        1
    );
    let funds = query(&s, &cookie, &(base.clone() + "/funds-result")).await;
    assert_eq!(funds["state"], "SUCCEEDED");
    assert_eq!(funds["transfer"]["amount"], "100.00");
    capture("funds", &funds);
    capture(
        "repair2_terminal_agent",
        &query(&s, &cookie, &format!("/api/agent/tasks/{task}")).await,
    );

    capture(
        "original_result",
        &query(&s, &cookie, &(base.clone() + "/original-result")).await,
    );
    let balance = query(&s, &cookie, "/api/bank/overview").await;
    assert_eq!(
        balance["accounts"][0]["balance"]["available_balance"],
        "1188.00"
    );
    for text in ["查看银行卡", "查看订阅", "查看月度账单"] {
        let cid = uuid::Uuid::new_v4().to_string();
        let v = ok(command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&cid, 0, text, false, Value::Null),
        )
        .await);
        assert!(v["view"]["bank_service"].is_string());
    }
    // Initial failure and limits do not prevent a distinct explicit new request.
    for text in ["模型失败", "超时"] {
        let failed = uuid::Uuid::new_v4().to_string();
        assert!(!command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&failed, 0, text, false, Value::Null)
        )
        .await
        .status
        .is_success());
        let journal = query(&s, &cookie, &format!("/api/bank/conversation/{failed}")).await;
        assert_eq!(journal["uncertain"], true);
        assert_eq!(journal["task_id"], s.state.0.repository.chat_request_id_for_backend(&failed,1).unwrap().as_str());
    }
    let fresh_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        ok(command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&fresh_id, 0, "查询余额", false, Value::Null)
        )
        .await)["view"]["kind"],
        "READ"
    );
    let boundary = uuid::Uuid::new_v4().to_string();
    for revision in 0..12 {
        assert_eq!(
            ok(command(
                &s,
                &cookie,
                "/api/bank/conversation",
                input(&boundary, revision, "查看银行卡", false, Value::Null)
            )
            .await)["view"]["kind"],
            "CLARIFY"
        );
    }
    assert_eq!(
        command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&boundary, 12, "查看银行卡", false, Value::Null)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let oversized = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&oversized, 0, &"中".repeat(1400), false, Value::Null)
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    exercise_feedback_and_subscriptions(&s, &cookie).await;
    let cards = query(&s, &cookie, "/api/bank/cards").await;
    capture("cards", &cards);
    let card_read=ok(command(&s,&cookie,"/api/card-read",json!({"kind":"QUERY","account_id":"account_001","request_ref":uuid::Uuid::new_v4().to_string()})).await);
    capture("card_read", &card_read);
    assert_eq!(cards["items"][0]["card_id"], "demo_card_salary_v1");
    assert_eq!(cards["items"].as_array().unwrap().len(), 1);
    // Other session cannot inherit the journal or its stale turn.
    let other = s.login().await;
    assert_eq!(
        s.call(
            Method::GET,
            &format!("/api/bank/conversation/{id}"),
            None,
            Some(&other),
            None,
            None,
            false
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let cid = uuid::Uuid::new_v4().to_string();
    let initial = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&cid, 0, "给小李转100元", true, Value::Null),
    )
    .await);
    let saved = save_draft(&s, &cookie, &initial["view"]["draft"]).await;
    let changed = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&cid, 1, "改成200元", true, context(&saved)),
    )
    .await);
    assert_eq!(
        changed["view"]["draft"]["parameters"]["changes"]["amount"],
        "200.00"
    );
    assert_eq!(
        command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&cid, 2, "取消", true, context(&saved))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let modified = save_draft(&s, &cookie, &changed["view"]["draft"]).await;
    let cancel_id = uuid::Uuid::new_v4().to_string();
    let cancel = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&cancel_id, 0, "取消", true, context(&modified)),
    )
    .await);
    let cancelled = save_draft(&s, &cookie, &cancel["view"]["draft"]).await;
    assert_eq!(cancelled["task_state"], "CANCELLED");
    s.handle.abort();
    let before = calls.load(Ordering::SeqCst);
    let restarted = super::bank_commandcode_tests::bank_server_mode(Some(gateway()), true).await;
    let fresh = restarted.login().await;
    assert_eq!(
        query(&restarted, &fresh, &(base + "/funds-result")).await,
        funds
    );
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert_eq!(
        query(&restarted, &fresh, &format!("/api/agent/tasks/{task}")).await["draft"]["parameters"]
            ["amount"],
        "100.00"
    );
    assert_eq!(
        restarted
            .call(
                Method::GET,
                &format!("/api/bank/conversation/{id}"),
                None,
                Some(&fresh),
                None,
                None,
                false
            )
            .await
            .status,
        StatusCode::CONFLICT
    );
    for (_, actor) in actors {
        actor.close().await;
    }
    repo.close().await;
    restarted.handle.abort();
    ph.abort();
}

async fn subscription_action(s: &Server, c: &str, request: Value) -> Value {
    let kind = request["kind"].as_str().unwrap().to_owned();
    let p = ok(command(
        s,
        c,
        "/api/subscriptions/change",
        json!({"kind":"PREPARE","nonce":uuid::Uuid::new_v4().to_string(),"request":request}),
    )
    .await);
    capture(&format!("sub_{kind}_prepared"), &p);
    let confirmed=ok(command(s,c,"/api/subscriptions/change",json!({"kind":"CONFIRM","task_id":p["task_id"],"confirmation_ref":p["confirmation_ref"],"explicitly_confirm":true})).await);
    capture(&format!("sub_{kind}_confirmed"), &confirmed);
    let result=ok(command(s,c,"/api/subscriptions/change",json!({"kind":"EXECUTE","task_id":p["task_id"],"operation_id":p["operation_id"],"confirmation_ref":p["confirmation_ref"]})).await);
    assert_eq!(result["domain_audit"]["status"], "VERIFIED_CANDIDATE_V1");
    capture(&format!("sub_{kind}_result"), &result);
    let original = ok(command(
        s,
        c,
        "/api/subscriptions/change",
        json!({"kind":"QUERY","operation_id":p["operation_id"]}),
    )
    .await);
    assert_eq!(original["domain_audit"]["status"], "VERIFIED_CANDIDATE_V1");
    capture(
        &format!("sub_{kind}_after"),
        &ok(command(s, c, "/api/subscriptions", json!({"kind":"LIST"})).await),
    );
    result
}
async fn exercise_feedback_and_subscriptions(s: &Server, c: &str) {
    let report = ok(command(s, c, "/api/team-demo/billing", json!({})).await);
    capture("billing_report", &report);
    let data = report["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["data"].get("monthly_report").is_some())
        .unwrap()["data"]
        .clone();
    let evidence = data["report"]["evidence"].as_array().unwrap();
    assert!(evidence.len() >= 3);
    let feedback_context = query(s, c, "/api/billing-feedback/v2/context").await;
    capture("feedback_context", &feedback_context);
    let realm = feedback_context["recovery_realm"].clone();
    for (row, choice) in evidence.iter().zip(["MINE", "NOT_MINE", "UNSURE"]) {
        let nonce = uuid::Uuid::new_v4().to_string();
        let p=ok(command(s,c,"/api/billing-feedback/v2/preparations",json!({"contract_version":2,"recovery_realm":realm,"nonce":nonce,"intent":{"input_version":1,"report_id":data["report"]["metadata"]["report_id"],"transaction":row["reference"],"choice":choice}})).await);
        assert_eq!(p["kind"], "CREATED");
        capture(&format!("feedback_{choice}_prepared"), &p);
        let body = json!({"contract_version":2,"recovery_realm":realm,"task_id":p["task_id"],"operation_id":p["operation_id"],"confirmation_ref":p["confirmation"]["confirmation_ref"]});
        let confirmed = ok(command(
            s,
            c,
            &format!("/api/billing-feedback/v2/preparations/{nonce}/confirm"),
            body.clone(),
        )
        .await);
        capture(&format!("feedback_{choice}_confirmed"), &confirmed);
        let result = ok(command(
            s,
            c,
            &format!("/api/billing-feedback/v2/preparations/{nonce}/execute"),
            body,
        )
        .await);
        assert_eq!(result["state"], "SAVED");
        assert_eq!(result["lock_authorized"], false);
        capture(&format!("feedback_{choice}_result"), &result);
        capture(
            &format!("feedback_{choice}_read"),
            &query(
                s,
                c,
                &format!(
                    "/api/billing-feedback/v2/preparations/{nonce}?recovery_realm={}",
                    realm.as_str().unwrap()
                ),
            )
            .await,
        );
    }
    let captured = ok(command(s, c, "/api/subscriptions/change", json!({"kind":"CAPTURE"})).await);
    capture("subscription_capture", &captured);
    let group = captured["preview"]["recognition"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["disposition"] == "NOT_IN_LEDGER")
        .expect("adopted fixture has real registration candidate");
    subscription_action(s,c,json!({"kind":"REGISTRATION","capture_ref":captured["capture_ref"],"candidate_index":group["selection_index"]})).await;
    for kind in [
        "REMINDER_SETTINGS",
        "RECORD_CANCELLATION",
        "REVOKE_CANCELLATION",
    ] {
        let ledger = ok(command(s, c, "/api/subscriptions", json!({"kind":"LIST"})).await);
        capture(&format!("sub_{kind}_ledger"), &ledger);
        let item = &ledger["items"][0];
        assert!(item["selection_ref"].is_string(), "ledger {ledger}");
        let mut request = json!({"kind":kind,"selection_ref":item["selection_ref"]});
        if kind == "REMINDER_SETTINGS" {
            request["enabled"] = json!(true);
            request["days_before"] = json!(1);
        }
        subscription_action(s, c, request).await;
    }
}

#[tokio::test]
#[ignore = "new owned adopted PG; repair regression, no real provider"]
async fn bank_conversation_repair_clarification_keeps_original_draft() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let app = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(Arc::new(AtomicUsize::new(0)));
    let ph = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let gateway = safeflow_runtime::OpenAiGatewayConfig::new(
        &endpoint,
        "deepseek/deepseek-v4.1-flash",
        Zeroizing::new("fixture-key-not-real".into()),
        Duration::from_secs(2),
    )
    .unwrap();
    let s = super::bank_commandcode_tests::bank_server_mode(Some(gateway), true).await;
    let cookie = s.login().await;
    let id = uuid::Uuid::new_v4().to_string();
    let created = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 0, "给小李转100元", true, Value::Null),
    )
    .await);
    capture("repair_created", &created);
    let saved = save_draft(&s, &cookie, &created["view"]["draft"]).await;
    capture("repair_saved_100", &saved);
    let task = saved["task_id"].as_str().unwrap();
    let original = query(&s, &cookie, &format!("/api/agent/tasks/{task}")).await;
    capture("repair_original_100", &original);
    let url = std::env::var("SAFEFLOW_BANK_CHAT_ADMIN_URL").unwrap();
    let ns = std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", ns.as_str())]);
    let pool = sqlx::PgPool::connect_with(options).await.unwrap();
    let snapshot="SELECT json_build_object('tasks',(SELECT count(*) FROM tasks),'drafts',(SELECT count(*) FROM transfer_drafts_v1),'orders',(SELECT count(*) FROM transfer_fund_orders_v1),'ledger',(SELECT count(*) FROM transfer_ledger_v1),'balance',(SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='account_001'))";
    let before: Value = sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();
    let clarify = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 1, "我要修改金额", true, context(&saved)),
    )
    .await);
    assert_eq!(clarify["view"]["kind"], "CLARIFY");
    assert_eq!(clarify["task_id"], saved["task_id"]);
    capture("repair_clarify", &clarify);
    let journal = query(&s, &cookie, &format!("/api/bank/conversation/{id}")).await;
    assert_eq!(journal["task_id"], saved["task_id"]);
    assert_eq!(journal["view"]["clarification"]["question"], "改成多少元？");
    capture("repair_journal", &journal);
    assert_eq!(
        command(
            &s,
            &cookie,
            "/api/bank/conversation",
            input(&id, 2, "200元", true, Value::Null)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    // Refresh semantics: get the journal only for display, then independently GET
    // the original business to obtain its current checked versions and ticket.
    let reread = query(&s, &cookie, &format!("/api/agent/tasks/{task}")).await;
    assert_eq!(reread["draft"]["task_id"], saved["task_id"]);
    assert_eq!(reread["draft"]["parameters"]["amount"], "100.00");
    let changed = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 2, "200元", true, context(&reread["draft"])),
    )
    .await);
    let d = &changed["view"]["draft"];
    assert_eq!(d["task_id"], saved["task_id"]);
    assert_eq!(
        d["plan_version"].as_u64().unwrap(),
        saved["plan_version"].as_u64().unwrap() + 1
    );
    assert_eq!(d["parameters"]["changes"]["amount"], "200.00");
    capture("repair_changed", &changed);
    let modified = save_draft(&s, &cookie, d).await;
    capture("repair_saved_200", &modified);
    assert_eq!(
        modified["receipt"]["draft_id"],
        saved["receipt"]["draft_id"]
    );
    assert_eq!(
        modified["receipt"]["draft_version"].as_u64().unwrap(),
        saved["receipt"]["draft_version"].as_u64().unwrap() + 1
    );
    let amount: String =
        sqlx::query_scalar("SELECT details->>'amount' FROM transfer_drafts_v1 WHERE task_id=$1")
            .bind(task)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(amount, "200.00");
    let after: Value = sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();
    assert_eq!(
        after, before,
        "clarification/modification must not create another task/draft or execute funds"
    );
    let old_token=command(&s,&cookie,&format!("/api/sim/draft-lifecycles/{task}/operations/{}/confirm",modified["operation_id"].as_str().unwrap()),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":saved["confirmation"]["confirmation_ref"],"display_ticket":saved["display_ticket"],"action":"CONFIRM"})).await;
    assert!(
        !old_token.status.is_success(),
        "old confirmation must not authorize the modified action"
    );
    let cancelled = ok(command(
        &s,
        &cookie,
        "/api/bank/conversation",
        input(&id, 3, "取消", true, context(&modified)),
    )
    .await);
    let cancelled = save_draft(&s, &cookie, &cancelled["view"]["draft"]).await;
    assert_eq!(cancelled["task_id"], saved["task_id"]);
    assert_eq!(cancelled["task_state"], "CANCELLED");
    let summary = json!({"original_task":saved["task_id"],"clarification_task":journal["task_id"],"modified_task":modified["task_id"],"original_draft":saved["receipt"]["draft_id"],"modified_draft":modified["receipt"]["draft_id"],"modified_amount":amount,"before":before,"after":after,"cancelled":true,"old_confirmation_rejected":true});
    capture("repair_summary", &summary);
    println!("REPAIR_BOUND_CLARIFICATION {}", summary);
    pool.close().await;
    s.handle.abort();
    ph.abort();
}

#[tokio::test]
#[ignore="requires owned adopted PG; no real provider/key"]
async fn transfer_recipient_optional_fields_and_old_snapshot_reconfirmation() {
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let ph=tokio::spawn(async move {axum::serve(listener,Router::new().route("/v1/chat/completions",post(provider)).with_state(Arc::new(AtomicUsize::new(0)))).await.unwrap();});
    let gateway=||safeflow_runtime::OpenAiGatewayConfig::new(&endpoint,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-not-real".into()),Duration::from_secs(2)).unwrap();
    let s=super::bank_commandcode_tests::bank_server_mode(Some(gateway()),true).await;let cookie=s.login().await;
    let fixture:Value=serde_json::from_slice(&std::fs::read(std::env::var("SAFEFLOW_BANK_CHAT_UI_FIXTURE").unwrap()).unwrap()).unwrap();
    for key in ["legacy_recipient_prepared","legacy_recipient_unprepared"] {
        let old=&fixture[key];assert!(old.is_object());
        let task=old["task_id"].as_str().unwrap();
        let view=query(&s,&cookie,&format!("/api/sim/draft-lifecycles/{task}")).await;
        assert_eq!(view["next_action"]["kind"],"RECONFIRM_RECIPIENT");
        assert!(view.get("confirmation_recipient").is_none());
        assert!(view["confirmation"].is_null());
        let old_path=format!("/api/sim/draft-lifecycles/{task}/operations/{}",old["operation_id"].as_str().unwrap());
        assert!(!command(&s,&cookie,&(old_path.clone()+"/prepare"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"})).await.status.is_success());
        let replaced=ok(command(&s,&cookie,&format!("/api/sim/draft-lifecycles/{task}/actions"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","selection_ticket":view["selection_ticket"],"task_revision":view["task_revision"],"plan_version":view["plan_version"],"facts_version":view["facts_version"],"action":"CREATE_MOCK_DRAFT","parameters":view["parameters"]})).await);
        assert_eq!(replaced["plan_version"].as_u64().unwrap(),view["plan_version"].as_u64().unwrap()+1);
        let new_path=format!("/api/sim/draft-lifecycles/{task}/operations/{}",replaced["operation_id"].as_str().unwrap());
        let prepared=ok(command(&s,&cookie,&(new_path.clone()+"/prepare"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"})).await);
        assert_eq!(prepared["confirmation_recipient"]["display_name"],"李明远");
        assert_eq!(prepared["confirmation_recipient"]["simulation_card_number"],"9900000000000001024");
        if !old["confirmation"].is_null(){
            assert!(!command(&s,&cookie,&(new_path+"/confirm"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":old["confirmation"]["confirmation_ref"],"display_ticket":old["display_ticket"],"action":"CONFIRM"})).await.status.is_success());
        }
        capture(&format!("{key}_reconfirmed"),&prepared);
    }
    for message in ["100元", "100元空备注", "100元指定备注"] {
        let id=uuid::Uuid::new_v4().to_string();
        let proposed=ok(command(&s,&cookie,"/api/bank/conversation",input(&id,0,message,true,Value::Null)).await);
        let draft=save_draft(&s,&cookie,&proposed["view"]["draft"]).await;
        assert_eq!(draft["execution_status"],"SUCCEEDED");
        let task=draft["task_id"].as_str().unwrap();
        let reread=query(&s,&cookie,&format!("/api/agent/tasks/{task}")).await;
        assert_eq!(reread["draft"]["receipt"],draft["receipt"]);
        assert!(reread["draft"].get("confirmation_recipient").is_none());
        capture(&format!("optional_{message}"),&draft);
    }
    for message in ["100元未知字段","100元错误替换"] {
        let id=uuid::Uuid::new_v4().to_string();
        let r=command(&s,&cookie,"/api/bank/conversation",input(&id,0,message,true,Value::Null)).await;
        assert!(!r.status.is_success(),"strict registered contract must reject: {message}");
    }
    s.handle.abort();ph.abort();
}

#[tokio::test]
#[ignore="requires owned PG with deliberately reproduced pre-fix claims"]
async fn transfer_claim_restart_preserves_unknown_and_never_reexecutes() {
    let url=std::env::var("SAFEFLOW_BANK_CHAT_ADMIN_URL").unwrap();
    let ns=std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let options=sqlx::postgres::PgConnectOptions::from_str(&url).unwrap().options([("search_path",ns.as_str())]);
    let pool=sqlx::PgPool::connect_with(options).await.unwrap();
    let rows:Vec<(String,String,Value)>=sqlx::query_as("SELECT task_id,operation_id,source FROM mock_draft_lifecycle_claims WHERE status='CLAIMED' ORDER BY task_id").fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(),2);
    let snapshot="SELECT json_build_object('claims',(SELECT json_agg(row_to_json(c) ORDER BY operation_id) FROM mock_draft_lifecycle_claims c WHERE status='CLAIMED'),'drafts',(SELECT count(*) FROM transfer_drafts_v1),'orders',(SELECT count(*) FROM transfer_fund_orders_v1),'ledger',(SELECT count(*) FROM transfer_ledger_v1),'balance',(SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='account_001'))";
    let before:Value=sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let calls=Arc::new(AtomicUsize::new(0));let pc=calls.clone();
    let ph=tokio::spawn(async move{axum::serve(listener,Router::new().route("/v1/chat/completions",post(provider)).with_state(pc)).await.unwrap();});
    for _ in 0..2 {
        let gateway=safeflow_runtime::OpenAiGatewayConfig::new(&endpoint,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-not-real".into()),Duration::from_secs(2)).unwrap();
        let s=super::bank_commandcode_tests::bank_server_mode(Some(gateway),true).await;let cookie=s.login().await;
        for (task,operation,source) in &rows {
            let view=query(&s,&cookie,&format!("/api/agent/tasks/{task}")).await;
            let d=&view["draft"];
            assert_eq!(d["execution_status"],"RESULT_UNKNOWN");assert!(d["receipt"].is_null());
            assert_eq!(d["next_action"]["kind"],"QUERY_ORIGINAL_OPERATION");
            let path=format!("/api/sim/draft-lifecycles/{task}/operations/{operation}");
            let replay=ok(command(&s,&cookie,&(path.clone()+"/execute"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":source["confirmation"]["confirmation_ref"],"task_revision":source["task"]["revision"],"operation_revision":1,"plan_version":1,"facts_version":source["task"]["facts_version"]})).await);
            assert_eq!(replay["execution_status"],"RESULT_UNKNOWN");
            assert!(!command(&s,&cookie,&(path.clone()+"/prepare"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"})).await.status.is_success());
            assert!(!command(&s,&cookie,&(path+"/confirm"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":source["confirmation"]["confirmation_ref"],"display_ticket":"not-new-authority","action":"CONFIRM"})).await.status.is_success());
            assert_eq!(s.state.0.draft.as_ref().unwrap().store.saved_receipt_count().unwrap(),0);
            capture(&format!("unknown_{task}"),d);
        }
        s.handle.abort();
    }
    assert_eq!(calls.load(Ordering::SeqCst),0);
    let after:Value=sqlx::query_scalar(snapshot).fetch_one(&pool).await.unwrap();assert_eq!(before,after);
    capture("unknown_restart_summary",&json!({"unknown_claims":2,"host_instances":2,"claim_rows_unchanged":true,"drafts":after["drafts"],"orders":after["orders"],"ledger":after["ledger"],"provider_calls":0,"no_reexecution":true}));
    pool.close().await;ph.abort();
}

#[tokio::test]
#[ignore = "owned new PG only; no funds execution, provider or real credentials"]
async fn site_qa_p0_recovery_directory_and_subscription_retry() {
    let s = super::bank_commandcode_tests::bank_server_mode(None, true).await;
    let cookie = s.login().await;
    let admin = std::env::var("SAFEFLOW_BANK_CHAT_ADMIN_URL").unwrap();
    let ns = std::env::var("SAFEFLOW_BANK_LLM_TEST_SCHEMA").unwrap();
    let pool = sqlx::PgPool::connect_with(sqlx::postgres::PgConnectOptions::from_str(&admin).unwrap().options([("search_path", ns.as_str())])).await.unwrap();
    let snapshot = || async {
        sqlx::query_scalar::<_, Value>("SELECT jsonb_build_object('balance',(SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id='account_001'),'ledger',(SELECT count(*) FROM transfer_ledger_v1),'orders',(SELECT count(*) FROM transfer_fund_orders_v1),'locks',(SELECT count(*) FROM card_locks_v1 WHERE state='ACTIVE'),'feedback',(SELECT count(*) FROM billing_feedback_v1),'subscriptions',(SELECT count(*) FROM subscription_public_bindings_v1))").fetch_one(&pool).await.unwrap()
    };
    let before = snapshot().await;
    capture("p0_before", &before);
    assert_eq!(s.call(Method::GET,"/api/bank/recovery",None,None,None,None,false).await.status,StatusCode::UNAUTHORIZED);
    assert_eq!(s.call(Method::GET,"/api/bank/recovery?owner=user_002",None,Some(&cookie),None,None,false).await.status,StatusCode::BAD_REQUEST);
    assert_eq!(command(&s,&cookie,"/api/bank/recovery",json!({})).await.status,StatusCode::METHOD_NOT_ALLOWED);
    let draft = ok(command(&s,&cookie,"/api/bank/payment-draft",json!({"nonce":uuid::Uuid::new_v4().to_string()})).await);
    let saved = save_draft(&s,&cookie,&draft).await;
    capture("p0_saved_draft", &saved);
    let task=saved["task_id"].as_str().unwrap();
    let payment=ok(command(&s,&cookie,&format!("/api/sim/payment-preparations/{task}"),json!({"contract_version":1,"scope":"PAYMENT_INTENT_ONLY","source":{"selection_ticket":saved["selection_ticket"],"task_revision":saved["task_revision"],"plan_version":saved["plan_version"],"facts_version":saved["facts_version"],"draft_id":saved["receipt"]["draft_id"],"draft_version":saved["receipt"]["draft_version"]}})).await);
    capture("p0_prepared_payment", &payment);
    let first=query(&s,&cookie,"/api/bank/recovery").await;
    let session1=query(&s,&cookie,"/api/card-read/session").await;
    let other=s.login().await;
    let session2=query(&s,&other,"/api/card-read/session").await;
    assert_ne!(session1["context_ref"],session2["context_ref"]);
    let second=query(&s,&other,"/api/bank/recovery").await;
    assert_eq!(first,second);
    assert!(second["payment_tasks"].as_array().unwrap().contains(&saved["task_id"]));
    capture("p0_directory", &second);
    capture("p0_relogged_draft", &query(&s,&other,&format!("/api/sim/draft-lifecycles/{task}")).await);
    let base=format!("/api/sim/payment-preparations/{task}/{}",payment["operation"]["operation_id"].as_str().unwrap());
    capture("p0_relogged_payment", &query(&s,&other,&base).await);
    let funds=query(&s,&other,&(base.clone()+"/funds-result")).await;
    assert_eq!(funds["submitted"],false);
    capture("p0_unsubmitted_funds", &funds);
    // A restored selector is not a restored confirmation authority.
    assert!(!command(&s,&other,&(base+"/confirm"),json!({"contract_version":1,"scope":"PAYMENT_INTENT_ONLY","confirmation_ref":payment["confirmation_ref"],"display_ticket":payment["display_ticket"],"action":"CONFIRM"})).await.status.is_success());
    let foreign=s.state.0.repository.bank_recovery_for_backend(&UserId::parse("user_002").unwrap()).await.unwrap();
    assert_eq!(foreign["payment_tasks"],json!([]));
    assert_eq!(foreign["subscription_nonces"],json!([]));
    assert_ne!(foreign["context_ref"],second["context_ref"]);
    // Hold an owned dependency through HTTP timeout; rollback the test lock.
    let capture1=ok(command(&s,&other,"/api/subscriptions/change",json!({"kind":"CAPTURE"})).await);
    let group=capture1["preview"]["recognition"]["groups"].as_array().unwrap().iter().find(|g|g["disposition"]=="NOT_IN_LEDGER").unwrap();
    let nonce=uuid::Uuid::new_v4().to_string();
    let mut tx=pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE subscription_runtime_records IN ACCESS EXCLUSIVE MODE").execute(&mut *tx).await.unwrap();
    let failed=command(&s,&other,"/api/subscriptions/change",json!({"kind":"PREPARE","nonce":nonce,"request":{"kind":"REGISTRATION","capture_ref":capture1["capture_ref"],"candidate_index":group["selection_index"]}})).await;
    tx.rollback().await.unwrap();
    assert_eq!(failed.status,StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.json["error"]["reason_code"],"DEPENDENCY_UNAVAILABLE");
    capture("p0_subscription_prepare_failed",&failed.json);
    let missing=command(&s,&other,"/api/subscriptions/change",json!({"kind":"QUERY_NONCE","nonce":nonce})).await;
    assert_eq!(missing.status,StatusCode::NOT_FOUND);
    capture("p0_subscription_nonce_missing",&missing.json);
    ok(command(&s,&other,"/api/subscriptions",json!({"kind":"LIST"})).await);
    let fresh=ok(command(&s,&other,"/api/subscriptions/change",json!({"kind":"CAPTURE"})).await);
    assert_ne!(fresh["capture_ref"],capture1["capture_ref"]);
    capture("p0_subscription_fresh_capture",&fresh);
    let candidate=fresh["preview"]["recognition"]["groups"].as_array().unwrap().iter().find(|g|g["disposition"]=="NOT_IN_LEDGER").unwrap();
    let p=ok(command(&s,&other,"/api/subscriptions/change",json!({"kind":"PREPARE","nonce":nonce,"request":{"kind":"REGISTRATION","capture_ref":fresh["capture_ref"],"candidate_index":candidate["selection_index"]}})).await);
    capture("p0_subscription_prepared",&p);
    ok(command(&s,&other,"/api/subscriptions/change",json!({"kind":"CONFIRM","task_id":p["task_id"],"confirmation_ref":p["confirmation_ref"],"explicitly_confirm":true})).await);
    let result=ok(command(&s,&other,"/api/subscriptions/change",json!({"kind":"EXECUTE","task_id":p["task_id"],"operation_id":p["operation_id"],"confirmation_ref":p["confirmation_ref"]})).await);
    capture("p0_subscription_result",&result);
    assert_eq!(result["domain_audit"]["status"],"VERIFIED_CANDIDATE_V1");
    let fresh_cookie=s.login().await;
    let recovered=ok(command(&s,&fresh_cookie,"/api/subscriptions/change",json!({"kind":"QUERY_NONCE","nonce":nonce})).await);
    assert_eq!(recovered["operation_id"],p["operation_id"]);
    assert_eq!(recovered["can_continue"],false);
    capture("p0_subscription_relogged",&recovered);
    let directory=query(&s,&fresh_cookie,"/api/bank/recovery").await;
    assert!(directory["subscription_nonces"].as_array().unwrap().contains(&json!(nonce)));
    let after=snapshot().await;
    capture("p0_after",&after);
    for key in ["balance","ledger","orders","locks","feedback"] { assert_eq!(before[key],after[key],"{key} changed"); }
    assert_eq!(after["subscriptions"].as_i64().unwrap(),before["subscriptions"].as_i64().unwrap()+1);
    pool.close().await;s.handle.abort();
}

#[tokio::test]
#[ignore="new owned bank PG, local provider fixture only; no money execution"]
async fn orphan_chat_http_completed_turn_relogin_missing_journal_and_read() {
 let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
 let endpoint=format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
 let calls=Arc::new(AtomicUsize::new(0));let pc=calls.clone();
 let ph=tokio::spawn(async move{axum::serve(listener,Router::new().route("/v1/chat/completions",post(provider)).with_state(pc)).await.unwrap();});
 let gateway=||safeflow_runtime::OpenAiGatewayConfig::new(&endpoint,"deepseek/deepseek-v4.1-flash",Zeroizing::new("fixture-key-not-real".into()),Duration::from_secs(2)).unwrap();
 let s=super::bank_commandcode_tests::bank_server_mode(Some(gateway()),true).await;let cookie=s.login().await;
 let legacy=format!("bank_chat_{}_1",uuid::Uuid::new_v4());let path=format!("/api/bank/uncreated-chat-requests/{legacy}");
 assert_eq!(s.call(Method::GET,&path,None,None,None,None,false).await.status,StatusCode::UNAUTHORIZED);
 assert_eq!(s.call(Method::GET,&(path.clone()+"?owner=user_002"),None,Some(&cookie),None,None,false).await.status,StatusCode::BAD_REQUEST);
 let proof=query(&s,&cookie,&path).await;assert_eq!(proof["task_id"],legacy);assert_eq!(proof["context_ref"],query(&s,&cookie,"/api/bank/recovery").await["context_ref"]);
 let conv=uuid::Uuid::new_v4().to_string();let id=s.state.0.repository.chat_request_id_for_backend(&conv,1).unwrap();let path=format!("/api/bank/uncreated-chat-requests/{id}");
 assert!(!s.call(Method::GET,&path,None,Some(&cookie),None,None,false).await.status.is_success());
 let message=command(&s,&cookie,"/api/bank/conversation",input(&conv,0,"迟到查看银行卡",false,Value::Null));
 let in_flight=async {while calls.load(Ordering::SeqCst)==0 {tokio::task::yield_now().await;}
  assert!(!s.call(Method::GET,&path,None,Some(&cookie),None,None,false).await.status.is_success());};
 let (response,_)=tokio::join!(message,in_flight);let response=ok(response);assert_eq!(response["view"]["kind"],"CLARIFY");assert!(response["task_id"].is_null());
 let fresh=s.login().await;let completed=query(&s,&fresh,&path).await;assert_eq!(completed["proof_source"],"COMPLETED_SERVER_TURN");assert_eq!(completed["task_id"],id.as_str());
 // Both public creation routes delegate to the same Arc-sharing repository.
 let rejected=command(&s,&fresh,"/api/sim/draft-lifecycles",json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","task_id":id,"facts_version":1,"parameters":{"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null}})).await;
 assert!(!rejected.status.is_success());
 let read_conv=uuid::Uuid::new_v4().to_string();let read=ok(command(&s,&cookie,"/api/bank/conversation",input(&read_conv,0,"查询余额",false,Value::Null)).await);
 assert_eq!(read["view"]["kind"],"READ");let read_id=read["view"]["task"]["task_id"].as_str().unwrap();assert_eq!(query(&s,&cookie,&format!("/api/agent/tasks/{read_id}")).await["kind"],"READ");
 assert!(!s.call(Method::GET,&format!("/api/bank/uncreated-chat-requests/{read_id}"),None,Some(&cookie),None,None,false).await.status.is_success());
 s.handle.abort();let restarted=super::bank_commandcode_tests::bank_server_mode(Some(gateway()),true).await;let fresh=restarted.login().await;
 assert!(!restarted.call(Method::GET,&path,None,Some(&fresh),None,None,false).await.status.is_success());
 restarted.handle.abort();ph.abort();
}
