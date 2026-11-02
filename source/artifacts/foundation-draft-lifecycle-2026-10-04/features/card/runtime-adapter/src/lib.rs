//! Proposed module adapter; does not register tools or choose security levels.
//! Only authenticated foundation contexts may supply identity. The data source
//! must be a trusted, complete, versioned read provider. No writes or permits.

use safeflow_card_facts::account_restrictions::AccountUse;
use safeflow_card_facts::account_service::{
    AccountDataSource, AccountPage, AccountQueryIssue, AccountQueryService,
};
use safeflow_card_facts::service::SourceUnavailable;
use safeflow_contracts::{
    AgentTool, CheckId, ContractFuture, DomainPolicy, ExecutionContext, ExecutionObservation,
    FindingKind, PolicyVersion, PostExecuteContext, ReasonCode, SecurityContext, SecurityError,
    SecurityFinding, SourceName, ToolDefinition, ToolError, ToolExecutionAuthorization, ToolName,
    ToolResult, ToolResultRef, ToolResultStatus,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

// Proposed names/versions, not an approved C-CARD/C-REG public schema.
pub const GET_ACCOUNT_CARD_FACTS: &str = "get_account_card_facts";
pub const CARD_FACTS_SCHEMA_VERSION: u32 = 1;
pub const CARD_FACTS_POLICY_VERSION: &str = "card-facts-policy-v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    account_id: String,
}

fn parse_input(arguments: Value) -> Result<Input, &'static str> {
    let input: Input = serde_json::from_value(arguments).map_err(|_| "VALIDATION_ERROR")?;
    if input.account_id.is_empty()
        || input.account_id.chars().count() > 128
        || input
            .account_id
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("VALIDATION_ERROR");
    }
    Ok(input)
}

struct SharedSource<S>(Arc<S>);

impl<S: AccountDataSource> AccountDataSource for SharedSource<S> {
    fn read_page(
        &self,
        user: &str,
        account: &str,
        cursor: Option<&str>,
        revision: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        self.0.read_page(user, account, cursor, revision)
    }

    fn read_current_revision(
        &self,
        user: &str,
        account: &str,
    ) -> Result<String, SourceUnavailable> {
        self.0.read_current_revision(user, account)
    }
}

pub struct GetAccountCardFactsTool<S> {
    service: AccountQueryService<SharedSource<S>>,
}

impl<S: AccountDataSource> GetAccountCardFactsTool<S> {
    pub fn new(source: Arc<S>) -> Self {
        Self {
            service: AccountQueryService::new(SharedSource(source)),
        }
    }
}

impl<S: AccountDataSource + Send + Sync> AgentTool for GetAccountCardFactsTool<S> {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            tool_name(),
            "Read owned account cards and public lock facts; does not authorize spending",
            json!({
                "type": "object",
                "properties": {"account_id": {
                    "type": "string", "minLength": 1, "maxLength": 128,
                    "pattern": "^[^\\s\\u0000-\\u001F\\u007F-\\u009F]+$"
                }},
                "required": ["account_id"],
                "additionalProperties": false
            }),
            CARD_FACTS_SCHEMA_VERSION,
            false,
            "READ_ACCOUNT_CARD_FACTS",
        )
        .expect("static card facts definition")
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            if !matches!(authorization, ToolExecutionAuthorization::ReadOnly) {
                return Err(tool_error("READ_ONLY_AUTHORIZATION_REQUIRED"));
            }
            let input = parse_input(arguments).map_err(tool_error)?;
            // Fresh read at execution even after policy evaluation; no cached
            // precheck or user-provided ownership/lock facts are accepted.
            let facts = self
                .service
                .query(
                    context.authenticated_user_id.as_str(),
                    Some(&input.account_id),
                )
                .map_err(|issue| tool_error(issue_code(issue)))?;
            let cards: Vec<Value> = facts
                .cards
                .iter()
                .map(|card| {
                    json!({
                        "card_id": card.card_id,
                        "account_id": card.account_id,
                        "last_four": card.last_four,
                        "active_locks": card.active_locks.iter().map(|lock| json!({
                            "lock_id": lock.id, "public_reason": lock.reason
                        })).collect::<Vec<_>>()
                    })
                })
                .collect();
            let data = json!({
                "scope": "CARD_LOCK_FACTS_ONLY",
                "account_id": facts.account_id,
                "revision": facts.revision,
                "cards": cards,
                "active_card_lock_ids": facts.active_card_lock_ids,
                "card_lock_restrictions": {
                    "all_outgoing": facts.outgoing_restricted(),
                    "transfer": facts.restricts(AccountUse::Transfer),
                    "card_spend": facts.restricts(AccountUse::CardSpend),
                    "automatic_debit": facts.restricts(AccountUse::AutomaticDebit),
                    "other_outgoing": facts.restricts(AccountUse::OtherOutgoing),
                    "query": facts.restricts(AccountUse::Query),
                    "receive": facts.restricts(AccountUse::Receive)
                },
                "unlock_route": "OFFLINE_BANK",
                "all_active_locks_must_be_released": true
            });
            Ok(ToolResult::new(
                tool_name(),
                context.operation_id.clone(),
                result_ref(&context.operation_id),
                ToolResultStatus::Succeeded,
                data,
            ))
        })
    }
}

pub struct GetAccountCardFactsPolicy<S> {
    name: ToolName,
    service: AccountQueryService<SharedSource<S>>,
}

impl<S: AccountDataSource> GetAccountCardFactsPolicy<S> {
    pub fn new(source: Arc<S>) -> Self {
        Self {
            name: tool_name(),
            service: AccountQueryService::new(SharedSource(source)),
        }
    }
}

impl<S: AccountDataSource + Send + Sync> DomainPolicy for GetAccountCardFactsPolicy<S> {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let checked = (|| {
                if context.tool_name != self.name {
                    return Err("VALIDATION_ERROR");
                }
                let input = parse_input(context.normalized_arguments.clone())?;
                self.service
                    .query(
                        context.authenticated_user_id.as_str(),
                        Some(&input.account_id),
                    )
                    .map_err(issue_code)?;
                // Locks restrict outgoing, not this read. The central kernel
                // chooses the final outcome and minimum security level.
                Ok(())
            })();
            Ok(findings(checked))
        })
    }

    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            // This contract supplies references only, not result bodies. This
            // checks correlation, not a bank receipt or independently stored result.
            let matches = context.tool_name == self.name
                && matches!(
                    &context.observation,
                    ExecutionObservation::ResultReceived { tool_result_ref, .. }
                        if *tool_result_ref == result_ref(&context.operation_id)
                );
            Ok(findings(if matches {
                Ok(())
            } else {
                Err("CARD_RESULT_REFERENCE_MISMATCH")
            }))
        })
    }
}

fn tool_name() -> ToolName {
    ToolName::parse(GET_ACCOUNT_CARD_FACTS).expect("static tool name")
}

fn result_ref(operation_id: &safeflow_contracts::OperationId) -> ToolResultRef {
    ToolResultRef::parse(format!("card_facts_result_{operation_id}")).expect("valid operation ID")
}

fn issue_code(issue: AccountQueryIssue) -> &'static str {
    match issue {
        AccountQueryIssue::ResourceUnavailable => "CARD_RESOURCE_UNAVAILABLE",
        AccountQueryIssue::LookupUnavailable => "CARD_LOOKUP_UNAVAILABLE",
        AccountQueryIssue::IncompleteData => "CARD_DATA_INCOMPLETE",
        AccountQueryIssue::InconsistentData => "CARD_DATA_INCONSISTENT",
        AccountQueryIssue::SnapshotChanged => "CARD_SNAPSHOT_CHANGED",
    }
}

fn tool_error(code: &'static str) -> ToolError {
    ToolError {
        reason_code: ReasonCode::parse(code).expect("static reason code"),
        message: "Unable to confirm the requested card facts".to_owned(),
    }
}

fn findings(result: Result<(), &'static str>) -> Vec<SecurityFinding> {
    match result {
        Ok(()) => vec![],
        Err(code) => vec![SecurityFinding {
            check_id: CheckId::parse("CARD-READ-01").expect("static check ID"),
            policy_version: PolicyVersion::parse(CARD_FACTS_POLICY_VERSION)
                .expect("static version"),
            source: SourceName::parse("card_facts_policy").expect("static source"),
            kind: FindingKind::HardDeny,
            reason_code: ReasonCode::parse(code).expect("static reason"),
            evidence_refs: vec![],
            observed_value: None,
            event_ids: vec![],
            dedup_key: None,
            window_start_ms: None,
            window_end_ms: None,
        }],
    }
}
