//! The existing read-only tool contract, backed by adopted SQL card facts.
//! No catalog input/write route and no payment authorization from observations.
use crate::ReadOnlyToolRegistration;
use safeflow_card_facts::account_restrictions::AccountUse;
use safeflow_card_runtime_adapter::{
    CARD_FACTS_POLICY_VERSION, CARD_FACTS_SCHEMA_VERSION, GET_ACCOUNT_CARD_FACTS,
};
use safeflow_contracts::*;
use safeflow_runtime::{PreflightInputContract, ToolSecurityProfile};
use safeflow_storage::{
    CardCatalogSourceBinding, DatabaseCardReader, PgRepository, CARD_POSTGRES_SOURCE_REVISION,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    account_id: ResourceId,
}
struct Adapter {
    reader: DatabaseCardReader,
    name: ToolName,
}
fn input(value: Value) -> Result<Input, ToolError> {
    serde_json::from_value(value).map_err(|_| error("VALIDATION_ERROR"))
}
fn error(code: &str) -> ToolError {
    ToolError {
        reason_code: ReasonCode::parse(code).unwrap(),
        message: "Unable to confirm the requested database card facts".into(),
    }
}
fn result_ref(op: &OperationId) -> ToolResultRef {
    ToolResultRef::parse(format!("card_facts_result_{op}")).unwrap()
}
fn findings(ok: bool) -> Vec<SecurityFinding> {
    if ok {
        return vec![];
    }
    vec![SecurityFinding {
        check_id: CheckId::parse("CARD-READ-01").unwrap(),
        policy_version: PolicyVersion::parse(CARD_FACTS_POLICY_VERSION).unwrap(),
        source: SourceName::parse("card_database_policy").unwrap(),
        kind: FindingKind::HardDeny,
        reason_code: ReasonCode::parse("CARD_DATABASE_SOURCE_UNAVAILABLE").unwrap(),
        evidence_refs: vec![],
        observed_value: None,
        event_ids: vec![],
        dedup_key: None,
        window_start_ms: None,
        window_end_ms: None,
    }]
}
impl Adapter {
    async fn read(
        &self,
        owner: &UserId,
        account: &ResourceId,
    ) -> Result<safeflow_card_facts::account_service::AccountCardFacts, ToolError> {
        let session = safeflow_runtime::AuthenticatedReadSession::current_for_backend()
            .map_err(|_| error("READ_SESSION_UNAVAILABLE"))?;
        if session
            .check()
            .map_err(|_| error("READ_SESSION_UNAVAILABLE"))?
            .user_id
            != *owner
        {
            return Err(error("READ_SESSION_MISMATCH"));
        }
        self.reader
            .observe_authenticated_session(&session, account)
            .await
            .map_err(|_| error("CARD_DATABASE_SOURCE_UNAVAILABLE"))
    }
}
impl AgentTool for Adapter {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(self.name.clone(),"Read adopted database card facts; point-in-time observation only",
            json!({"type":"object","properties":{"account_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[^\\s\\u0000-\\u001F\\u007F-\\u009F]+$"}},"required":["account_id"],"additionalProperties":false}),1,false,"READ_ACCOUNT_CARD_FACTS").unwrap()
    }
    fn execute<'a>(
        &'a self,
        ctx: &'a ExecutionContext,
        auth: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            if !matches!(auth, ToolExecutionAuthorization::ReadOnly) {
                return Err(error("READ_ONLY_AUTHORIZATION_REQUIRED"));
            }
            let input = input(arguments)?;
            let facts = self
                .read(&ctx.authenticated_user_id, &input.account_id)
                .await
                .map_err(|_| error("CARD_DATABASE_SOURCE_UNAVAILABLE"))?;
            let cards:Vec<Value>=facts.cards.iter().map(|c|json!({"card_id":c.card_id,"account_id":c.account_id,"last_four":c.last_four,
                "active_locks":c.active_locks.iter().map(|l|json!({"lock_id":l.id,"public_reason":l.reason})).collect::<Vec<_>>()})).collect();
            Ok(ToolResult::new(
                self.name.clone(),
                ctx.operation_id.clone(),
                result_ref(&ctx.operation_id),
                ToolResultStatus::Succeeded,
                json!({"scope":"CARD_LOCK_FACTS_ONLY","observation_scope":"CURRENT_OBSERVATION_ONLY","account_id":facts.account_id,"revision":facts.revision,
                "cards":cards,"active_card_lock_ids":facts.active_card_lock_ids,"card_lock_restrictions":{"all_outgoing":facts.outgoing_restricted(),
                "transfer":facts.restricts(AccountUse::Transfer),"card_spend":facts.restricts(AccountUse::CardSpend),
                "automatic_debit":facts.restricts(AccountUse::AutomaticDebit),"other_outgoing":facts.restricts(AccountUse::OtherOutgoing),
                "query":facts.restricts(AccountUse::Query),"receive":facts.restricts(AccountUse::Receive)},
                "unlock_route":"OFFLINE_BANK","all_active_locks_must_be_released":true,"funds_authorized":false}),
            ))
        })
    }
}
impl DomainPolicy for Adapter {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        ctx: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let ok = if ctx.tool_name != self.name {
                false
            } else if let Ok(input) = input(ctx.normalized_arguments.clone()) {
                self.read(&ctx.authenticated_user_id, &input.account_id)
                    .await
                    .is_ok()
            } else {
                false
            };
            Ok(findings(ok))
        })
    }
    fn verify_execution<'a>(
        &'a self,
        ctx: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            Ok(findings(
                ctx.tool_name == self.name
                    && matches!(&ctx.observation,ExecutionObservation::ResultReceived{tool_result_ref,..} if *tool_result_ref==result_ref(&ctx.operation_id)),
            ))
        })
    }
}
pub fn database_card_facts_registration_for_backend(
    repository: PgRepository,
    source: Option<CardCatalogSourceBinding>,
    approved_profile: ToolSecurityProfile,
) -> ReadOnlyToolRegistration {
    let adapter = Arc::new(Adapter {
        reader: DatabaseCardReader::for_trusted_backend(repository, source),
        name: ToolName::parse(GET_ACCOUNT_CARD_FACTS).unwrap(),
    });
    ReadOnlyToolRegistration {
        tool: adapter.clone(),
        policy: adapter,
        policy_version: PolicyVersion::parse(CARD_FACTS_POLICY_VERSION).unwrap(),
        compatible_schema_version: CARD_FACTS_SCHEMA_VERSION,
        input: PreflightInputContract {
            tool_name: approved_profile.tool_name.clone(),
            tool_schema_version: approved_profile.tool_schema_version,
            policy_version: approved_profile.policy_version.clone(),
            projection_revision: "bounded-direct-input-v2".into(),
        },
        profile: approved_profile,
        approved_action: "READ_ACCOUNT_CARD_FACTS".into(),
        implementation_revision: format!("{CARD_POSTGRES_SOURCE_REVISION}:public-card-database-v1"),
    }
}
