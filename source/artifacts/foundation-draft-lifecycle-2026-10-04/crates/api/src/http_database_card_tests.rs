//! Actual Agent HTTP/Runtime database consumer; all sources below are test-only.
use super::*;
use safeflow_contracts::{ContractFuture, PolicyVersion, ResourceId, SecurityLevel, ToolName};
use safeflow_runtime::{SessionBinding, ToolSecurityProfile};
use safeflow_storage::{
    BackendCardCatalog, BackendCardCatalogProvider, CardCatalogService, CardCatalogSourceBinding,
    StorageError,
};
use safeflow_transfer_integration::funds_store::FundsStore;
use std::sync::atomic::{AtomicBool, AtomicU64};

struct Catalog {
    fail: AtomicBool,
    revoked: AtomicBool,
    revision: AtomicU64,
    calls: AtomicU64,
    last_binding: AtomicU64,
}
impl BackendCardCatalogProvider for Catalog {
    fn authorized_catalog<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_binding
                .store(identity.binding_version, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(StorageError::DependencyUnavailable);
            }
            if self.revoked.load(Ordering::SeqCst) {
                return Ok(None);
            }
            Ok(Some(BackendCardCatalog {
                owner: identity.user_id.clone(),
                account: account.clone(),
                source_revision: format!(
                    "test_complete_zero_{}",
                    self.revision.load(Ordering::SeqCst)
                ),
                cards: vec![],
            }))
        })
    }
    fn current_revision<'a>(
        &'a self,
        identity: &'a SessionBinding,
        _: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_binding
                .store(identity.binding_version, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) || self.revoked.load(Ordering::SeqCst) {
                return Err(StorageError::DependencyUnavailable);
            }
            Ok(format!(
                "test_complete_zero_{}",
                self.revision.load(Ordering::SeqCst)
            ))
        })
    }
}
fn catalog() -> Arc<Catalog> {
    Arc::new(Catalog {
        fail: AtomicBool::new(false),
        revoked: AtomicBool::new(false),
        revision: AtomicU64::new(1),
        calls: AtomicU64::new(0),
        last_binding: AtomicU64::new(0),
    })
}
async fn server(db: &Db, provider: &Provider, source: Option<CardCatalogSourceBinding>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.llm_gateway = Some(
        safeflow_runtime::OpenAiGatewayConfig::new(
            &provider.endpoint,
            "local-protocol-only",
            Zeroizing::new("isolated-protocol-key".into()),
            Duration::from_secs(5),
        )
        .unwrap(),
    );
    let profile = ToolSecurityProfile {
        tool_name: ToolName::parse("get_account_card_facts").unwrap(),
        tool_schema_version: 1,
        policy_version: PolicyVersion::parse("card-facts-policy-v1").unwrap(),
        minimum_level: SecurityLevel::Green,
    };
    let registration =
        crate::database_card_facts_registration_for_backend(db.repo.clone(), source, profile);
    let state = ApiState::for_project_simulation_with_read_only_tools(
        db.repo.clone(),
        config,
        Arc::new(safeflow_mock_bank::MockBankGateway::new()),
        vec![registration],
    )
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
#[tokio::test]
#[ignore = "requires isolated PG; local protocol fixture, no model service"]
async fn database_cards_http_current_read_history_after_source_removed_and_startup_missing_source()
{
    let db = Db::new().await;
    let protocol = Provider::new().await;
    let catalog = catalog();
    let source = CardCatalogSourceBinding::for_reviewed_backend_provider(
        "isolated_catalog",
        "test_impl",
        catalog.clone(),
    )
    .unwrap();
    let live = server(&db, &protocol, Some(source.clone())).await;
    let cookie = live.login().await;
    let session = crate::session::token_reference(
        cookie.split(';').next().unwrap().split_once('=').unwrap().1,
    )
    .unwrap();
    let mut tx = db.scoped.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("user_001").unwrap(),
        &ResourceId::parse("account_001").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    CardCatalogService::for_trusted_backend(
        db.repo.clone(),
        live.state.0.sessions.clone(),
        Some(source),
    )
    .initialize(&session, &ResourceId::parse("account_001").unwrap())
    .await
    .unwrap();
    let initial = ok(command(
        &live,
        &cookie,
        "/api/agent/messages",
        json!({"message":"database cards"}),
    )
    .await);
    let id = initial["task"]["task_id"].as_str().unwrap();
    let result = ok(command(
        &live,
        &cookie,
        &format!("/api/agent/tasks/{id}/execute"),
        version(&initial),
    )
    .await);
    assert_eq!(result["task"]["task_state"], "WAITING_USER");
    assert_eq!(result["results"][0]["data"]["cards"], json!([]));
    assert_eq!(
        result["results"][0]["data"]["observation_scope"],
        "CURRENT_OBSERVATION_ONLY"
    );
    assert_eq!(result["results"][0]["data"]["funds_authorized"], false);
    let text = result["results"][0]["data"].to_string();
    assert!(!text.contains("event_scope") && !text.contains("original_key"));
    live.stop();
    catalog.fail.store(true, Ordering::SeqCst);
    let calls = catalog.calls.load(Ordering::SeqCst);
    let restart = server(&db, &protocol, None).await;
    let cookie = restart.login().await;
    let recovered = ok(query(&restart, &cookie, id).await);
    assert_eq!(recovered["results"], result["results"]);
    assert_eq!(catalog.calls.load(Ordering::SeqCst), calls);
    let initial = ok(command(
        &restart,
        &cookie,
        "/api/agent/messages",
        json!({"message":"database cards"}),
    )
    .await);
    let denied = command(
        &restart,
        &cookie,
        &format!(
            "/api/agent/tasks/{}/execute",
            initial["task"]["task_id"].as_str().unwrap()
        ),
        version(&initial),
    )
    .await;
    assert!(!denied.status.is_success());
    assert_eq!(catalog.calls.load(Ordering::SeqCst), calls);
    restart.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PG; actual HTTP provider invalidation regression"]
async fn database_cards_http_same_provider_changed_revoked_failed_refuse_new_reads_but_keep_history(
) {
    for mode in ["revision", "revoked", "failure"] {
        let db = Db::new().await;
        let protocol = Provider::new().await;
        let catalog = catalog();
        let source = CardCatalogSourceBinding::for_reviewed_backend_provider(
            "same_catalog",
            "same_impl",
            catalog.clone(),
        )
        .unwrap();
        let live = server(&db, &protocol, Some(source.clone())).await;
        let cookie = live.login().await;
        let session = crate::session::token_reference(
            cookie.split(';').next().unwrap().split_once('=').unwrap().1,
        )
        .unwrap();
        let mut tx = db.scoped.begin().await.unwrap();
        FundsStore::provision_account(
            &mut tx,
            &UserId::parse("user_001").unwrap(),
            &ResourceId::parse("account_001").unwrap(),
            &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        CardCatalogService::for_trusted_backend(
            db.repo.clone(),
            live.state.0.sessions.clone(),
            Some(source),
        )
        .initialize(&session, &ResourceId::parse("account_001").unwrap())
        .await
        .unwrap();
        let initial = ok(command(
            &live,
            &cookie,
            "/api/agent/messages",
            json!({"message":"database cards"}),
        )
        .await);
        let id = initial["task"]["task_id"].as_str().unwrap();
        let result = ok(command(
            &live,
            &cookie,
            &format!("/api/agent/tasks/{id}/execute"),
            version(&initial),
        )
        .await);
        assert_eq!(result["results"][0]["data"]["cards"], json!([]));
        assert_eq!(
            catalog.last_binding.load(Ordering::SeqCst),
            live.state
                .0
                .sessions
                .resolve_binding(&session)
                .unwrap()
                .binding_version
        );
        match mode {
            "revision" => {
                catalog.revision.store(2, Ordering::SeqCst);
            }
            "revoked" => {
                catalog.revoked.store(true, Ordering::SeqCst);
            }
            _ => {
                catalog.fail.store(true, Ordering::SeqCst);
            }
        }
        let calls = catalog.calls.load(Ordering::SeqCst);
        assert_eq!(
            ok(query(&live, &cookie, id).await)["results"],
            result["results"]
        );
        assert_eq!(
            catalog.calls.load(Ordering::SeqCst),
            calls,
            "history must not read revoked provider"
        );
        let fresh = ok(command(
            &live,
            &cookie,
            "/api/agent/messages",
            json!({"message":"database cards"}),
        )
        .await);
        let denied = command(
            &live,
            &cookie,
            &format!(
                "/api/agent/tasks/{}/execute",
                fresh["task"]["task_id"].as_str().unwrap()
            ),
            version(&fresh),
        )
        .await;
        assert!(
            !denied.status.is_success(),
            "{mode}: stale source returned a current result"
        );
        assert!(
            catalog.calls.load(Ordering::SeqCst) > calls,
            "actual HTTP policy must revalidate provider"
        );
        let calls = catalog.calls.load(Ordering::SeqCst);
        assert_eq!(
            ok(query(&live, &cookie, id).await)["results"],
            result["results"]
        );
        assert_eq!(catalog.calls.load(Ordering::SeqCst), calls);
        live.stop();
        db.finish().await;
    }
}
