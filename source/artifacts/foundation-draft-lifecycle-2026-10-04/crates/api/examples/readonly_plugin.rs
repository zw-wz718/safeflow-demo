//! Run with SAFEFLOW_TEST_DATABASE_URL. --check validates public startup and HTTP status;
//! --serve runs the loopback simulation until Ctrl-C. No model/key is read or enabled.
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use safeflow_api::{router, ApiConfig, ApiState, ReadOnlyToolRegistration};
use safeflow_bank_gateway::{
    FindPayeesPolicy, FindPayeesTool, BANK_QUERY_SCHEMA_VERSION, FIND_PAYEES, PAYEES_POLICY_VERSION,
};
use safeflow_contracts::{PolicyVersion, SecurityLevel, ToolName};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{PreflightInputContract, ToolSecurityProfile};
use safeflow_storage::PgRepository;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use zeroize::Zeroizing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = Zeroizing::new(
        std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .map_err(|_| "isolated test database required")?,
    );
    let namespace = format!("sf_plugin_example_{}", uuid::Uuid::new_v4().simple());
    let repo = PgRepository::connect(&url, &namespace).await?;
    repo.migrate().await?;
    let bank = Arc::new(MockBankGateway::new());
    let name = ToolName::parse(FIND_PAYEES)?;
    let policy = PolicyVersion::parse(PAYEES_POLICY_VERSION)?;
    let registration = ReadOnlyToolRegistration {
        tool: Arc::new(FindPayeesTool::new(bank.clone())),
        policy: Arc::new(FindPayeesPolicy::new(bank.clone())),
        policy_version: policy.clone(),
        compatible_schema_version: BANK_QUERY_SCHEMA_VERSION,
        profile: ToolSecurityProfile {
            tool_name: name.clone(),
            tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
            policy_version: policy.clone(),
            minimum_level: SecurityLevel::Green,
        },
        input: PreflightInputContract {
            tool_name: name,
            tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
            policy_version: policy,
            projection_revision: "bounded-direct-input-v2".into(),
        },
        approved_action: "READ_PAYEES".into(),
        implementation_revision: "bank-gateway-delivered-payees-v1".into(),
    };
    // Explicit local demonstration secret, unrelated to LLM credentials or production login.
    let config =
        ApiConfig::local_simulation(Zeroizing::new("isolated-plugin-example-secret".into()));
    let address = config.bind;
    let state = ApiState::for_project_simulation_with_read_only_tools(
        repo.clone(),
        config,
        bank,
        vec![registration],
    )?;
    let app = router(state);
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sim/session")
                .header(header::HOST, address.to_string())
                .header(header::ORIGIN, "http://127.0.0.1:5174")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-safeflow-request", "1")
                .body(Body::from(
                    json!({"login_secret":"isolated-plugin-example-secret"}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()?
        .split(';')
        .next()
        .ok_or("missing cookie")?
        .to_owned();
    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agent/status")
                .header(header::HOST, address.to_string())
                .header(header::COOKIE, cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value = serde_json::from_slice(&to_bytes(status.into_body(), 8192).await?)?;
    assert_eq!(
        status["tools"],
        json!(["find_payees", "get_account_balance"])
    );
    println!("PROJECT_SIMULATION: public read-only startup and HTTP tool visibility verified; model disabled");
    if std::env::args().any(|arg| arg == "--serve") {
        let listener = tokio::net::TcpListener::bind(address).await?;
        println!("PROJECT_SIMULATION: http://{address}; Ctrl-C stops and removes this example's isolated schema");
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
    }
    repo.close().await;
    let admin = sqlx::PgPool::connect(&url).await?;
    sqlx::query(&format!("DROP SCHEMA {namespace} CASCADE"))
        .execute(&admin)
        .await?;
    admin.close().await;
    Ok(())
}
