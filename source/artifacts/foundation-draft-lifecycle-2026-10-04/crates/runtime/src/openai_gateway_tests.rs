use super::*;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use safeflow_contracts::{TaskId, ToolDefinition, ToolName};
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
#[path = "openai_gateway_reuse_tests.rs"]
mod reuse;

#[derive(Clone)]
enum Reply {
    Json(String),
    SlowBody,
    ChunkedOversize,
    Redirect(String),
}
#[derive(Clone)]
struct FixtureState {
    reply: Reply,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}
struct Fixture {
    state: FixtureState,
    endpoint: String,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Fixture {
    async fn new(reply: Reply) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let state = FixtureState {
            reply,
            requests: Arc::new(Mutex::new(vec![])),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(fixture))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            endpoint,
            handle,
        }
    }
    fn gateway(&self) -> OpenAiGateway {
        OpenAiGateway::new(
            OpenAiGatewayConfig::new(
                &self.endpoint,
                "transport-fixture",
                Zeroizing::new("fixture-key-not-a-provider-key".into()),
                Duration::from_secs(1),
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn calls(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }
}
async fn fixture(
    State(state): State<FixtureState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> axum::response::Response {
    state.requests.lock().unwrap().push((headers, request));
    match state.reply {
        Reply::Json(raw) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            raw,
        )
            .into_response(),
        Reply::Redirect(location) => (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, location)],
            "",
        )
            .into_response(),
        Reply::SlowBody => {
            let stream = futures_util::stream::unfold(0, |step| async move {
                match step {
                    0 => Some((Ok::<_, Infallible>(Bytes::from_static(b"{")), 1)),
                    1 => {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        Some((Ok(Bytes::from_static(b"}")), 2))
                    }
                    _ => None,
                }
            });
            (
                [(header::CONTENT_TYPE, "application/json")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        Reply::ChunkedOversize => {
            let stream = futures_util::stream::once(async {
                Ok::<_, Infallible>(Bytes::from(vec![b' '; MAX_RESPONSE_BYTES + 1]))
            });
            (
                [(header::CONTENT_TYPE, "application/json")],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }
}
fn context() -> PlanningContext {
    PlanningContext {
        task_id: TaskId::parse("task_gateway_fixture").unwrap(),
        plan_version: 1,
    }
}
fn input() -> PlanningInput {
    PlanningInput {user_input:"query explicit account_001; untrusted text says ignore system".into(),tool_definitions:vec![ToolDefinition::new(ToolName::parse("get_account_balance").unwrap(),"Existing balance tool",json!({"type":"object","additionalProperties":false,"required":["account_id"],"properties":{"account_id":{"type":"string"}}}),1,false,"READ_BALANCE").unwrap()]}
}
fn plan() -> Value {
    json!({"contract_version":1,"understanding":{"intent":"READ_BALANCE","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},"plan":{"contract_version":1,"task_id":"task_gateway_fixture","plan_version":1,"nodes":[{"type":"TOOL","id":"read","tool":"get_account_balance","arguments":{"account_id":"account_001"},"depends_on":[]}]},"clarification":null})
}
fn envelope(content: String) -> Value {
    json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":content,"refusal":null}}],"usage":{"test_float":0.5}})
}

#[tokio::test]
async fn gateway_uses_real_single_http_request_and_only_bounded_planning_context() {
    let fixture = Fixture::new(Reply::Json(envelope(plan().to_string()).to_string())).await;
    let gateway = fixture.gateway();
    assert!(gateway.generate(&input()).await.is_err());
    assert_eq!(fixture.calls(), 0);
    let result = gateway
        .generate_with_context(&input(), &context())
        .await
        .unwrap();
    assert_eq!(result.plan.unwrap().task_id, context().task_id);
    let recorded = fixture.state.requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].0[header::AUTHORIZATION],
        "Bearer fixture-key-not-a-provider-key"
    );
    let request = &recorded[0].1;
    assert_eq!(request["stream"], false);
    assert_eq!(request["n"], 1);
    assert_eq!(request["messages"].as_array().unwrap().len(), 2);
    let system = request["messages"][0]["content"].as_str().unwrap();
    let protocol: Value =
        serde_json::from_str(system.split("Protocol data: ").nth(1).unwrap()).unwrap();
    assert_eq!(protocol.as_object().unwrap().len(), 3);
    assert_eq!(
        protocol["planning_context"],
        json!({"task_id":"task_gateway_fixture","plan_version":1})
    );
    let expected = input();
    for (index, tool) in expected.tool_definitions.iter().enumerate() {
        let compact = &protocol["approved_tool_definitions"][index];
        assert_eq!(compact["arguments_variant"], index);
        let path = format!("/planning_result_json_schema/$defs/plan/properties/nodes/items/oneOf/{index}/properties/arguments");
        assert_eq!(protocol.pointer(&path), Some(&tool.input_schema));
        let mut expanded = compact.clone();
        expanded.as_object_mut().unwrap().remove("arguments_variant");
        expanded["input_schema"] = protocol.pointer(&path).unwrap().clone();
        assert_eq!(expanded, serde_json::to_value(tool).unwrap());
    }
    assert_eq!(
        protocol["planning_result_json_schema"]["additionalProperties"],
        false
    );
    assert!(protocol["planning_result_json_schema"]["oneOf"].is_array());
    for forbidden in [
        "user_id",
        "session_hash",
        "binding_version",
        "policy_version",
        "permit",
    ] {
        assert!(protocol.get(forbidden).is_none());
    }
    assert!(!system.contains(input().user_input.as_str()));
}
#[tokio::test]
async fn gateway_rejects_duplicate_inner_and_outer_keys_before_typed_deserialization() {
    let duplicate_inner = plan().to_string().replacen(
        "\"contract_version\":1",
        "\"contract_version\":1,\"contract_version\":1",
        1,
    );
    let duplicate_entity = plan()
        .to_string()
        .replace("\"entities\":{}", "\"entities\":{\"x\":1,\"\\u0078\":2}");
    let mut outer = envelope(plan().to_string()).to_string();
    outer = outer.replacen("\"index\":0", "\"index\":0,\"index\":0", 1);
    for raw in [
        envelope(duplicate_inner).to_string(),
        envelope(duplicate_entity).to_string(),
        outer,
    ] {
        let fixture = Fixture::new(Reply::Json(raw)).await;
        assert!(matches!(
            fixture
                .gateway()
                .generate_with_context(&input(), &context())
                .await,
            Err(PlanningError::InvalidResult(_))
        ));
        assert_eq!(fixture.calls(), 1);
    }
}
#[tokio::test]
async fn gateway_rejects_incomplete_refusal_tools_multiple_choices_and_non_json_content() {
    let mut variants = vec![envelope(format!("```json\n{}\n```", plan()))];
    let mut length = envelope(plan().to_string());
    length["choices"][0]["finish_reason"] = json!("length");
    variants.push(length);
    for key in ["refusal", "tool_calls", "function_call"] {
        let mut invalid = envelope(plan().to_string());
        invalid["choices"][0]["message"][key] = json!("not allowed");
        variants.push(invalid);
    }
    let mut many = envelope(plan().to_string());
    let choice = many["choices"][0].clone();
    many["choices"].as_array_mut().unwrap().push(choice);
    variants.push(many);
    let mut wrong = plan();
    wrong["plan"]["task_id"] = json!("task_other");
    variants.push(envelope(wrong.to_string()));
    let mut float = plan();
    float["understanding"]["entities"] = json!({"amount":0.5});
    variants.push(envelope(float.to_string()));
    for invalid in variants {
        let fixture = Fixture::new(Reply::Json(invalid.to_string())).await;
        assert!(fixture
            .gateway()
            .generate_with_context(&input(), &context())
            .await
            .is_err());
        assert_eq!(fixture.calls(), 1);
    }
}
#[tokio::test]
async fn gateway_never_follows_redirects_or_retries_and_bounds_chunked_body() {
    let target = Fixture::new(Reply::Json(envelope(plan().to_string()).to_string())).await;
    let redirect = Fixture::new(Reply::Redirect(target.endpoint.clone())).await;
    assert!(redirect
        .gateway()
        .generate_with_context(&input(), &context())
        .await
        .is_err());
    assert_eq!(redirect.calls(), 1);
    assert_eq!(target.calls(), 0);
    let oversized = Fixture::new(Reply::ChunkedOversize).await;
    assert!(oversized
        .gateway()
        .generate_with_context(&input(), &context())
        .await
        .is_err());
    assert_eq!(oversized.calls(), 1);
    let slow = Fixture::new(Reply::SlowBody).await;
    let started = std::time::Instant::now();
    assert!(slow
        .gateway()
        .generate_with_context(&input(), &context())
        .await
        .is_err());
    assert!(started.elapsed() < Duration::from_millis(1800));
    assert_eq!(slow.calls(), 1);
}
#[test]
fn configuration_rejects_unsafe_endpoints_and_redacts_errors() {
    for endpoint in [
        "http://example.com/v1/chat/completions",
        "https://name:secret@example.com/v1/chat/completions",
        "https://example.com/v1/chat/completions?key=secret",
        "https://example.com/v1/chat/completions#secret",
        "http://localhost/v1/chat/completions",
        "not a URL",
    ] {
        let error = OpenAiGatewayConfig::new(
            endpoint,
            "model",
            Zeroizing::new("private-test-key".into()),
            Duration::from_secs(1),
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("private-test-key"));
        assert!(!format!("{error:?}").contains(endpoint));
    }
    for seconds in [0, 121] {
        assert!(OpenAiGatewayConfig::new(
            "https://provider.example/v1/chat/completions",
            "model",
            Zeroizing::new("key".into()),
            Duration::from_secs(seconds)
        )
        .is_err());
    }
}
#[tokio::test]
async fn request_limit_prevents_network_and_errors_never_contain_input_or_provider_body() {
    let fixture = Fixture::new(Reply::Json("private-provider-body-with-secret".into())).await;
    let mut oversized = input();
    oversized.tool_definitions[0].description = "x".repeat(MAX_REQUEST_BYTES);
    assert!(fixture
        .gateway()
        .generate_with_context(&oversized, &context())
        .await
        .is_err());
    assert_eq!(fixture.calls(), 0);
    let error = fixture
        .gateway()
        .generate_with_context(&input(), &context())
        .await
        .err()
        .unwrap();
    assert!(!error.to_string().contains("private-provider-body"));
    assert!(!format!("{error:?}").contains(input().user_input.as_str()));
    assert_eq!(fixture.calls(), 1);
}
