use safeflow_api::{
    router, serve_with_public_audit, ApiConfig, ApiState, AuditWorkerConfig,
    DraftConfirmationConfig,
};
use safeflow_runtime::OpenAiGatewayConfig;
use safeflow_storage::{PgRepository, PublicAuditOutbox};
use safeflow_transfer_integration::shared_demo::{DemoRecipient, SharedDemoPlan};
use std::sync::Arc;
use zeroize::Zeroizing;

fn shared_demo_settings(
    enabled: Option<&str>,
    owner: Option<&str>,
    account: Option<&str>,
) -> Result<Option<SharedDemoPlan>, Box<dyn std::error::Error>> {
    match enabled {
        None | Some("0") if owner.is_none() && account.is_none() => Ok(None),
        Some("1") => Ok(Some(SharedDemoPlan::prepare(DemoRecipient {
            owner_id: safeflow_contracts::UserId::parse(
                owner.ok_or("explicit demo recipient owner required")?,
            )?,
            account_id: safeflow_contracts::ResourceId::parse(
                account.ok_or("explicit demo recipient account required")?,
            )?,
        })?)),
        _ => Err("invalid or conflicting shared demo server configuration".into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let audit_worker = AuditWorkerConfig::from_env()?;
    let secret = Zeroizing::new(
        std::env::var("SAFEFLOW_SIM_LOGIN_SECRET")
            .map_err(|_| "simulation login secret configuration required")?,
    );
    let mut config = ApiConfig::local_simulation(secret);
    if let Ok(bind) = std::env::var("SAFEFLOW_API_ADDR") {
        config.bind = bind.parse().map_err(|_| "invalid loopback bind address")?;
    }
    if let Ok(origin) = std::env::var("SAFEFLOW_FRONTEND_ORIGIN") {
        config.frontend_origin = origin;
    }
    if let Ok(ttl) = std::env::var("SAFEFLOW_SIM_SESSION_TTL_SECS") {
        config.session_ttl =
            std::time::Duration::from_secs(ttl.parse().map_err(|_| "invalid session lifetime")?);
    }
    if let Ok(timeout) = std::env::var("SAFEFLOW_API_TIMEOUT_SECS") {
        config.request_timeout = std::time::Duration::from_secs(
            timeout.parse().map_err(|_| "invalid request deadline")?,
        );
    }
    match std::env::var("SAFEFLOW_SIM_DRAFT_CONFIRMATION_ENABLED").as_deref() {
        Ok("1") => {
            let seconds = |name: &str| -> Result<u64, &'static str> {
                std::env::var(name)
                    .map_err(|_| "explicit draft confirmation settings required")?
                    .parse()
                    .map_err(|_| "invalid draft confirmation lifetime")
            };
            let revision = std::env::var("SAFEFLOW_SIM_DRAFT_DEPLOYMENT_REVISION")
                .map_err(|_| "draft deployment revision required")?;
            config.draft_confirmation = Some(DraftConfirmationConfig::new(
                std::time::Duration::from_secs(seconds("SAFEFLOW_SIM_CONFIRMATION_TTL_SECS")?),
                std::time::Duration::from_secs(seconds("SAFEFLOW_SIM_DECISION_TTL_SECS")?),
                seconds("SAFEFLOW_SIM_DRAFT_STORE_TTL_SECS")?,
                &revision,
            )?);
        }
        Ok("0") | Err(_) => {}
        _ => return Err("invalid draft confirmation enable setting".into()),
    }
    match std::env::var("SAFEFLOW_SIM_DRAFT_LIFECYCLE_V2_ENABLED").as_deref() {
        Ok("1") => {
            config.draft_confirmation = Some(
                config
                    .draft_confirmation
                    .take()
                    .ok_or("draft lifecycle requires explicit draft configuration")?
                    .with_lifecycle_v2()?,
            );
        }
        Ok("0") | Err(_) => {}
        _ => return Err("invalid draft lifecycle enable setting".into()),
    }
    config.agent_fixed_plan_enabled =
        match std::env::var("SAFEFLOW_AGENT_FIXED_PLAN_ENABLED").as_deref() {
            Ok("1") => true,
            Ok("0") | Err(_) => false,
            _ => return Err("invalid fixed plan enable setting".into()),
        };
    if config.agent_fixed_plan_enabled {
        if std::env::var("SAFEFLOW_LLM_ENABLED").as_deref() == Ok("1") {
            return Err("choose one Agent planning mode".into());
        }
        println!(
            "Agent planning: explicit FIXED_PLAN / MockLlm samples; no natural language model"
        );
    } else {
        match std::env::var("SAFEFLOW_LLM_ENABLED").as_deref() {
            Ok("1") => {
                let endpoint = std::env::var("SAFEFLOW_LLM_ENDPOINT");
                let model = std::env::var("SAFEFLOW_LLM_MODEL");
                let key = std::env::var("SAFEFLOW_LLM_API_KEY").map(Zeroizing::new);
                let timeout = std::env::var("SAFEFLOW_LLM_TIMEOUT_SECS");
                if let (Ok(endpoint), Ok(model), Ok(key), Ok(timeout)) =
                    (endpoint, model, key, timeout)
                {
                    let timeout = timeout
                        .parse::<u64>()
                        .map_err(|_| "invalid LLM timeout configuration")?;
                    config.llm_gateway = Some(
                        OpenAiGatewayConfig::new(
                            &endpoint,
                            &model,
                            key,
                            std::time::Duration::from_secs(timeout),
                        )
                        .map_err(|_| "invalid LLM Gateway configuration")?,
                    );
                } else {
                    println!("LLM Gateway not configured: explicit endpoint, model, API key and timeout required");
                }
            }
            Ok("0") | Err(_) => {}
            _ => return Err("invalid LLM enable setting".into()),
        }
    }
    config.validate()?;
    let demo_plan = shared_demo_settings(
        std::env::var("SAFEFLOW_SHARED_DEMO_ENABLED")
            .ok()
            .as_deref(),
        std::env::var("SAFEFLOW_SHARED_DEMO_RECIPIENT_OWNER")
            .ok()
            .as_deref(),
        std::env::var("SAFEFLOW_SHARED_DEMO_RECIPIENT_ACCOUNT")
            .ok()
            .as_deref(),
    )?;
    if config.bind.ip().to_string() != "127.0.0.1" || config.bind.port() == 0 {
        return Err("127.0.0.1 and nonzero port required".into());
    }
    let bind = config.bind;
    let url = Zeroizing::new(
        std::env::var("SAFEFLOW_DATABASE_URL")
            .map_err(|_| "local database configuration required")?,
    );
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated project schema required")?;
    let suffix = schema
        .strip_prefix("sf_http_sim_")
        .ok_or("use sf_http_sim_<32 lower hex> schema")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid isolated schema".into());
    }
    let repo = PgRepository::connect(&url, &schema).await?;
    let result = async {
        repo.migrate().await?;
        // A database consumer is installed, but NO trusted catalog provider is
        // configured here. Shared-demo accounts/routes are not card mapping proof.
        // Unknown catalogs remain unavailable, never initialized as empty.
        let card_registration = safeflow_api::database_card_facts_registration_for_backend(
            repo.clone(),
            None,
            safeflow_runtime::ToolSecurityProfile {
                tool_name: safeflow_contracts::ToolName::parse("get_account_card_facts")?,
                tool_schema_version: 1,
                policy_version: safeflow_contracts::PolicyVersion::parse("card-facts-policy-v1")?,
                minimum_level: safeflow_contracts::SecurityLevel::Green,
            },
        );
        let bank: Arc<dyn safeflow_bank_gateway::BankGateway> = if let Some(plan) = demo_plan {
            repo.initialize_shared_demo(&plan).await?;
            Arc::new(
                safeflow_storage::TransferSqlBankGateway::for_trusted_backend(
                    repo.clone(),
                    Arc::new(safeflow_mock_bank::MockBankGateway::new()),
                ),
            )
        } else {
            Arc::new(safeflow_mock_bank::MockBankGateway::new())
        };
        let state = ApiState::for_project_simulation_with_read_only_tools(
            repo.clone(),
            config,
            bank,
            vec![card_registration],
        )?;
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|_| "local HTTP port unavailable")?;
        println!("SafeFlow project simulation HTTP: http://{bind}");
        serve_with_public_audit(
            listener,
            router(state),
            PublicAuditOutbox::for_trusted_backend(repo.clone()),
            audit_worker,
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
        )
        .await
        .map_err(|_| "local HTTP transport unavailable")?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    repo.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn demo_requires_explicit_server_mapping_and_enablement() {
        assert!(shared_demo_settings(None, None, None).unwrap().is_none());
        for (enable, owner, account) in [
            (Some("1"), None, None),
            (Some("1"), Some("configured-recipient"), None),
            (
                Some("0"),
                Some("configured-recipient"),
                Some("configured-account"),
            ),
            (Some("yes"), None, None),
            (Some("1"), Some("user_001"), Some("configured-account")),
            (Some("1"), Some("configured-recipient"), Some("account_002")),
        ] {
            assert!(shared_demo_settings(enable, owner, account).is_err());
        }
        let plan = shared_demo_settings(
            Some("1"),
            Some("configured-recipient"),
            Some("configured-account"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.route().recipient_account_id.as_str(),
            "configured-account"
        );
    }
}
