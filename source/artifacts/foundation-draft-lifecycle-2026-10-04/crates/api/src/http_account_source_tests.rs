//! Local contract probes only; no production identity or external bank adapter.
use super::*;
use safeflow_bank_gateway::{
    BankError, BankGateway, DecimalAmount, FindPayeesOutput, GetAccountBalanceOutput,
};
use safeflow_contracts::{ContractFuture, ResourceId};
use safeflow_mock_bank::MockBankGateway;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum BalanceReply {
    Local,
    Error(BankError),
    WrongAccount,
    UnmaskedAccount,
}
#[derive(Debug, PartialEq)]
enum Call {
    Balance(UserId, ResourceId),
    Payees(UserId, String),
}
struct PausedLookup {
    entered: Notify,
    release: Notify,
}
struct LocalSource {
    local: MockBankGateway,
    balance_reply: Mutex<BalanceReply>,
    payee_error: Mutex<Option<BankError>>,
    pause: Mutex<Option<Arc<PausedLookup>>>,
    calls: Mutex<Vec<Call>>,
}
impl LocalSource {
    fn new() -> Self {
        Self {
            local: MockBankGateway::new(),
            balance_reply: Mutex::new(BalanceReply::Local),
            payee_error: Mutex::new(None),
            pause: Mutex::new(None),
            calls: Mutex::new(vec![]),
        }
    }
    fn balance_calls(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, Call::Balance(..)))
            .count()
    }
    fn payee_calls(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, Call::Payees(..)))
            .count()
    }
    fn check_identity(&self) {
        for call in self.calls.lock().unwrap().iter() {
            let user = match call {
                Call::Balance(user, _) | Call::Payees(user, _) => user,
            };
            assert_eq!(user.as_str(), "user_001");
        }
    }
}
impl BankGateway for LocalSource {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Balance(user.clone(), account.clone()));
            let reply = *self.balance_reply.lock().unwrap();
            let pause = self.pause.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.entered.notify_one();
                pause.release.notified().await;
            }
            if let BalanceReply::Error(error) = reply {
                return Err(error);
            }
            let mut output = self.local.get_account_balance(user, account).await?;
            // A distinct valid local result detects an accidental Tool/Store Mock fallback.
            output.available_balance = DecimalAmount::parse("2048.00").unwrap();
            match reply {
                BalanceReply::WrongAccount => {
                    output.account.account_id = ResourceId::parse("account_002").unwrap()
                }
                BalanceReply::UnmaskedAccount => {
                    output.account.masked_account_no = "invalid-unmasked-test-response".into()
                }
                _ => {}
            }
            Ok(output)
        })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Payees(user.clone(), query.into()));
            if let Some(error) = *self.payee_error.lock().unwrap() {
                return Err(error);
            }
            self.local.find_payees(user, query).await
        })
    }
}

fn config() -> ApiConfig {
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.agent_fixed_plan_enabled = true;
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            600,
            "account-source-local-test",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    config
}
async fn server(db: &Db, bank: Option<Arc<dyn BankGateway>>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = config();
    config.bind = address;
    let state = match bank {
        Some(bank) => ApiState::for_project_simulation_with_bank(db.repo.clone(), config, bank),
        None => ApiState::for_project_simulation(db.repo.clone(), config),
    }
    .unwrap();
    let app = router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        state,
        address,
        handle,
    }
}
async fn command(server: &Server, cookie: &str, path: &str, input: Value) -> Reply {
    server
        .call(
            Method::POST,
            path,
            Some(input.to_string()),
            Some(cookie),
            None,
            None,
            true,
        )
        .await
}
fn ok(reply: Reply) -> Value {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    reply.json
}
fn read(account: &str) -> Value {
    json!({"sample":"READ_WAIT","selection":{"account_id":account}})
}
fn create() -> Value {
    json!({"sample":"CREATE_200","selection":{"account_id":"account_001","payee_query":"小李","payee_id":"payee_008"}})
}
fn version(view: &Value) -> Value {
    json!({"task_revision":view["task"]["revision"],"plan_version":view["task"]["plan_version"],"facts_version":view["task"]["facts_version"]})
}
async fn no_tasks(db: &Db) {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn account_source_trait_injection_reaches_both_runtime_scopes_without_pg() {
    let bank = Arc::new(LocalSource::new());
    let sessions = Arc::new(
        SimulationSessions::new(
            Zeroizing::new(SECRET.into()),
            UserId::parse("user_001").unwrap(),
            Duration::from_secs(300),
        )
        .unwrap(),
    );
    let cookie = sessions.issue(SECRET, None).unwrap();
    let cookie = Cookie::parse(cookie).unwrap();
    let reference = token_reference(cookie.value()).unwrap();
    for scope in [
        assembly::AssemblyScope::Balance,
        assembly::AssemblyScope::AgentLifecycle,
    ] {
        let audit = Arc::new(safeflow_runtime::InMemoryAuditRecorder::new());
        let draft = draft_confirmation::assemble(
            sessions.clone(),
            audit.clone(),
            bank.clone(),
            config().draft_confirmation.unwrap(),
            &[],
        )
        .unwrap();
        let assembled = if scope == assembly::AssemblyScope::Balance {
            assembly::assemble(scope, sessions.clone(), audit.clone(), bank.clone(), None).unwrap()
        } else {
            draft.lifecycle_runtime.unwrap()
        };
        let before = bank.balance_calls();
        let result = assembled
            .runtime()
            .execute(
                assembly::proposal(&TaskId::parse("local_probe").unwrap()),
                reference.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            safeflow_runtime::RuntimeOutcome::WaitingUser { .. }
        ));
        assert_eq!(
            bank.balance_calls() - before,
            3,
            "two Policy phases and one Tool use the injected source"
        );
        assert_eq!(
            audit
                .events()
                .unwrap()
                .iter()
                .filter(|e| e.kind == AuditEventKind::ToolStarted)
                .count(),
            1
        );
    }
    bank.check_identity();
}

#[tokio::test]
async fn account_source_denied_policy_prevents_tool_execution_without_mock_fallback() {
    let bank = Arc::new(LocalSource::new());
    *bank.balance_reply.lock().unwrap() = BalanceReply::Error(BankError::ResourceNotOwned);
    let sessions = Arc::new(
        SimulationSessions::new(
            Zeroizing::new(SECRET.into()),
            UserId::parse("user_001").unwrap(),
            Duration::from_secs(300),
        )
        .unwrap(),
    );
    let cookie = sessions.issue(SECRET, None).unwrap();
    let cookie = Cookie::parse(cookie).unwrap();
    let reference = token_reference(cookie.value()).unwrap();
    let audit = Arc::new(safeflow_runtime::InMemoryAuditRecorder::new());
    let assembled = assembly::assemble(
        assembly::AssemblyScope::Balance,
        sessions,
        audit.clone(),
        bank.clone(),
        None,
    )
    .unwrap();
    assert!(matches!(
        assembled
            .runtime()
            .execute(
                assembly::proposal(&TaskId::parse("denied_probe").unwrap()),
                reference
            )
            .await,
        Err(RuntimeError::SecurityGate(
            safeflow_contracts::SecurityOutcome::Blocked
        ))
    ));
    assert_eq!(bank.balance_calls(), 1);
    assert!(!audit
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditEventKind::ToolStarted));
    bank.check_identity();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; local source only"]
async fn real_http_account_source_selection_runtime_and_draft_share_one_gateway() {
    let db = Db::new().await;
    let bank = Arc::new(LocalSource::new());
    let server = server(&db, Some(bank.clone())).await;
    let trait_bank: Arc<dyn BankGateway> = bank.clone();
    assert!(Arc::ptr_eq(&server.state.0.bank, &trait_bank));
    assert!(
        bank.calls.lock().unwrap().is_empty(),
        "assembly must not query the source"
    );
    let cookie = server.login().await;
    let initial = ok(command(&server, &cookie, "/api/agent/samples", read("account_001")).await);
    assert_eq!(bank.balance_calls(), 1);
    let path = format!(
        "/api/agent/tasks/{}/execute",
        initial["task"]["task_id"].as_str().unwrap()
    );
    let waiting = ok(command(&server, &cookie, &path, version(&initial)).await);
    assert_eq!(waiting["task"]["task_state"], "WAITING_USER");
    assert_eq!(
        waiting["results"][0]["data"]["available_balance"],
        "2048.00"
    );
    assert!(
        bank.balance_calls() >= 4,
        "selection, Policy and Tool must use the injected source"
    );

    // The original Balance-scope HTTP route must also use this instance.
    let before_legacy = bank.balance_calls();
    let legacy = server.new_task(&cookie).await;
    assert_eq!(legacy["task_state"], "WAITING_USER");
    assert!(bank.balance_calls() >= before_legacy + 3);
    let draft =
        ok(command(&server, &cookie, "/api/agent/samples", create()).await)["draft"].clone();
    assert!(bank.payee_calls() > 0);
    let before = (bank.balance_calls(), bank.payee_calls());
    let prepared = ok(command(
        &server,
        &cookie,
        &format!(
            "/api/agent/tasks/{}/prepare",
            draft["task_id"].as_str().unwrap()
        ),
        json!({"operation_id":draft["operation_id"]}),
    )
    .await)["draft"]
        .clone();
    assert_eq!(prepared["next_action"]["kind"], "CONFIRM");
    let operation = format!(
        "/api/sim/draft-lifecycles/{}/operations/{}",
        draft["task_id"].as_str().unwrap(),
        draft["operation_id"].as_str().unwrap()
    );
    let confirmed = ok(command(&server, &cookie, &format!("{operation}/confirm"), json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":prepared["confirmation"]["confirmation_ref"],"display_ticket":prepared["display_ticket"],"action":"CONFIRM"})).await);
    let executed = ok(command(&server, &cookie, &format!("{operation}/execute"), json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":confirmed["confirmation"]["confirmation_ref"],"task_revision":confirmed["task_revision"],"operation_revision":confirmed["operation_revision"],"plan_version":confirmed["plan_version"],"facts_version":confirmed["facts_version"]})).await);
    assert_eq!(executed["execution_status"], "SUCCEEDED");
    assert_eq!(executed["mode"], "PROJECT_SIMULATION");
    assert!(bank.balance_calls() > before.0 && bank.payee_calls() > before.1);
    bank.check_identity();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; local source only"]
async fn real_http_account_source_errors_and_invalid_outputs_never_fall_back() {
    let db = Db::new().await;
    for (behavior, status, reason) in [
        (
            BalanceReply::Error(BankError::ResourceNotOwned),
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        ),
        (
            BalanceReply::Error(BankError::ValidationError),
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
        ),
        (
            BalanceReply::Error(BankError::Unavailable),
            StatusCode::SERVICE_UNAVAILABLE,
            "DEPENDENCY_UNAVAILABLE",
        ),
        (
            BalanceReply::Error(BankError::InvalidResponse),
            StatusCode::SERVICE_UNAVAILABLE,
            "DEPENDENCY_UNAVAILABLE",
        ),
        (
            BalanceReply::WrongAccount,
            StatusCode::SERVICE_UNAVAILABLE,
            "DEPENDENCY_UNAVAILABLE",
        ),
        (
            BalanceReply::UnmaskedAccount,
            StatusCode::SERVICE_UNAVAILABLE,
            "DEPENDENCY_UNAVAILABLE",
        ),
    ] {
        let bank = Arc::new(LocalSource::new());
        *bank.balance_reply.lock().unwrap() = behavior;
        let server = server(&db, Some(bank.clone())).await;
        let cookie = server.login().await;
        let reply = command(&server, &cookie, "/api/agent/samples", read("account_001")).await;
        assert_eq!(reply.status, status, "{}", reply.json);
        assert_eq!(reply.json["error"]["reason_code"], reason);
        for private in [
            "account_001",
            "account_002",
            "2048.00",
            "invalid-unmasked-test-response",
        ] {
            assert!(!reply.json.to_string().contains(private));
        }
        assert_eq!(bank.balance_calls(), 1);
        assert_eq!(server.calls(), 0);
        bank.check_identity();
        no_tasks(&db).await;
        server.stop();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; local source only"]
async fn real_http_account_source_rejects_other_accounts_and_request_selected_identity() {
    let db = Db::new().await;
    let bank = Arc::new(LocalSource::new());
    let server = server(&db, Some(bank.clone())).await;
    let cookie = server.login().await;
    for account in ["account_002", "missing_account"] {
        let reply = command(&server, &cookie, "/api/agent/samples", read(account)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.json["error"]["reason_code"], "FORBIDDEN");
        assert!(!reply.json.to_string().contains(account));
    }
    let calls = bank.balance_calls();
    for field in ["user_id", "owner", "bank_provider"] {
        for nested in [true, false] {
            let mut input = read("account_001");
            if nested {
                input["selection"][field] = json!("user_002");
            } else {
                input[field] = json!("user_002");
            }
            assert_eq!(
                command(&server, &cookie, "/api/agent/samples", input)
                    .await
                    .status,
                StatusCode::BAD_REQUEST
            );
        }
    }
    assert_eq!(bank.balance_calls(), calls);
    let unauthenticated = server
        .call(
            Method::POST,
            "/api/agent/samples",
            Some(read("account_001").to_string()),
            None,
            None,
            None,
            true,
        )
        .await;
    assert_eq!(unauthenticated.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bank.balance_calls(), calls);
    bank.check_identity();
    no_tasks(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; deterministic paused lookup"]
async fn real_http_account_source_rechecks_session_after_successful_and_failed_lookup() {
    let db = Db::new().await;
    for behavior in [
        BalanceReply::Local,
        BalanceReply::Error(BankError::Unavailable),
    ] {
        let bank = Arc::new(LocalSource::new());
        *bank.balance_reply.lock().unwrap() = behavior;
        let pause = Arc::new(PausedLookup {
            entered: Notify::new(),
            release: Notify::new(),
        });
        *bank.pause.lock().unwrap() = Some(pause.clone());
        let server = server(&db, Some(bank.clone())).await;
        let cookie = server.login().await;
        let reference = token_reference(Cookie::parse(&cookie).unwrap().value()).unwrap();
        let address = server.address;
        let request = tokio::spawn(async move {
            Server::call_at(
                address,
                Method::POST,
                "/api/agent/samples",
                Some(read("account_001").to_string()),
                Some(&cookie),
                None,
                None,
                true,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
            .await
            .unwrap();
        server.state.0.sessions.revoke(&reference).unwrap();
        pause.release.notify_one();
        let reply = tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.json["error"]["reason_code"], "NOT_AUTHENTICATED");
        assert_eq!(bank.balance_calls(), 1);
        assert_eq!(server.calls(), 0);
        no_tasks(&db).await;
        server.stop();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; local source only"]
async fn real_http_account_source_policy_rechecks_the_injected_source_at_execution() {
    let db = Db::new().await;
    let bank = Arc::new(LocalSource::new());
    let server = server(&db, Some(bank.clone())).await;
    let cookie = server.login().await;
    let initial = ok(command(&server, &cookie, "/api/agent/samples", read("account_001")).await);
    assert_eq!(bank.balance_calls(), 1);
    *bank.balance_reply.lock().unwrap() = BalanceReply::Error(BankError::ResourceNotOwned);
    let path = format!(
        "/api/agent/tasks/{}/execute",
        initial["task"]["task_id"].as_str().unwrap()
    );
    let reply = command(&server, &cookie, &path, version(&initial)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.json);
    assert!(bank.balance_calls() > 1);
    assert_eq!(server.calls(), 0);
    bank.check_identity();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; local source only"]
async fn real_http_account_source_draft_candidate_failure_does_not_fall_back() {
    let db = Db::new().await;
    let bank = Arc::new(LocalSource::new());
    *bank.payee_error.lock().unwrap() = Some(BankError::Unavailable);
    let server = server(&db, Some(bank.clone())).await;
    let cookie = server.login().await;
    let reply = command(&server, &cookie, "/api/agent/samples", create()).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.json["error"]["reason_code"], "DEPENDENCY_UNAVAILABLE");
    assert_eq!(bank.payee_calls(), 1);
    assert_eq!(server.calls(), 0);
    bank.check_identity();
    no_tasks(&db).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP; default source compatibility"]
async fn real_http_account_source_default_constructor_and_startup_failure_remain_explicit() {
    let db = Db::new().await;
    let bank = Arc::new(LocalSource::new());
    let mut invalid = config();
    invalid.bind = "0.0.0.0:8787".parse().unwrap();
    assert!(
        ApiState::for_project_simulation_with_bank(db.repo.clone(), invalid, bank.clone()).is_err()
    );
    assert!(bank.calls.lock().unwrap().is_empty());
    let server = server(&db, None).await;
    let cookie = server.login().await;
    let initial = ok(command(&server, &cookie, "/api/agent/samples", read("account_001")).await);
    let path = format!(
        "/api/agent/tasks/{}/execute",
        initial["task"]["task_id"].as_str().unwrap()
    );
    let waiting = ok(command(&server, &cookie, &path, version(&initial)).await);
    assert_eq!(
        waiting["results"][0]["data"]["available_balance"],
        "1288.00"
    );
    assert_eq!(waiting["mode"], "PROJECT_SIMULATION");
    server.stop();
    db.finish().await;
}
