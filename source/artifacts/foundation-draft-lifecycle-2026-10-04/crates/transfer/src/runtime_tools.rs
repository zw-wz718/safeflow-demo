//! Mock-only adapters over the existing transfer service; no funds capability.
use crate::{
    Clock, DraftChanges, DraftInput, DraftStatus, IntegrationError, PayeeSelection, StoredDraft,
    TransferDrafts,
};
use chrono::{SecondsFormat, TimeZone, Utc};
use futures::lock::Mutex as AsyncMutex;
use safeflow_bank_gateway::{
    BankGateway, CnyCurrency, DecimalAmount, FindPayeesInput, FindPayeesOutput,
    GetAccountBalanceInput,
};
use safeflow_contracts::{
    AgentTool, CheckId, ContractFuture, DomainPolicy, ExecutionContext, ExecutionObservation,
    FindingKind, MockDraftAction, PolicyVersion, PostExecuteContext, ReasonCode, ResourceId,
    SecurityContext, SecurityError, SecurityFinding, SourceName, TaskId, ToolDefinition, ToolError,
    ToolExecutionAuthorization, ToolName, ToolResult, ToolResultRef, ToolResultStatus, UserId,
    MOCK_DRAFT_CREATE_ACTION, MOCK_DRAFT_CREATE_TOOL, MOCK_DRAFT_TOOL_SCHEMA_VERSION,
};
use safeflow_runtime::MockDraftAuthority;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const MOCK_DRAFT_SCHEMA_VERSION: u32 = MOCK_DRAFT_TOOL_SCHEMA_VERSION;
pub const MOCK_DRAFT_POLICY_VERSION: &str = "mock-draft-policy-v1";
pub const MOCK_DRAFT_LIFECYCLE_POLICY_VERSION: &str = "mock-draft-lifecycle-policy-v2";

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockDraftChanges {
    #[serde(default, deserialize_with = "present")]
    pub payer_account_id: Option<ResourceId>,
    #[serde(default, deserialize_with = "present")]
    pub payee_id: Option<ResourceId>,
    #[serde(default, deserialize_with = "present")]
    pub amount: Option<DecimalAmount>,
    #[serde(default, deserialize_with = "present")]
    pub currency: Option<CnyCurrency>,
    #[serde(default, deserialize_with = "present")]
    pub remark: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModifyMockDraftInput {
    pub draft_id: ResourceId,
    pub expected_version: u32,
    pub changes: MockDraftChanges,
}

impl ModifyMockDraftInput {
    fn parse(arguments: Value) -> Result<Self, ToolError> {
        let input: Self =
            serde_json::from_value(arguments).map_err(|_| error("INVALID_DRAFT_INPUT"))?;
        validate_target(&input.draft_id, input.expected_version)?;
        let changes = &input.changes;
        if changes.payer_account_id.is_none()
            && changes.payee_id.is_none()
            && changes.amount.is_none()
            && changes.currency.is_none()
            && changes.remark.is_none()
        {
            return Err(error("EMPTY_DRAFT_CHANGES"));
        }
        for id in [&changes.payer_account_id, &changes.payee_id]
            .into_iter()
            .flatten()
        {
            validate_id(id)?;
        }
        if let Some(amount) = &changes.amount {
            validate_amount(amount)?;
        }
        if let Some(remark) = &changes.remark {
            validate_remark(remark)?;
        }
        Ok(input)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelMockDraftInput {
    pub draft_id: ResourceId,
    pub expected_version: u32,
}

impl CancelMockDraftInput {
    fn parse(arguments: Value) -> Result<Self, ToolError> {
        let input: Self =
            serde_json::from_value(arguments).map_err(|_| error("INVALID_DRAFT_INPUT"))?;
        validate_target(&input.draft_id, input.expected_version)?;
        Ok(input)
    }
}

fn validate_id(id: &ResourceId) -> Result<(), ToolError> {
    GetAccountBalanceInput {
        account_id: id.clone(),
    }
    .validate()
    .map_err(|_| error("INVALID_RESOURCE_ID"))
}
fn validate_target(id: &ResourceId, version: u32) -> Result<(), ToolError> {
    validate_id(id)?;
    if version == 0 {
        return Err(error("INVALID_DRAFT_VERSION"));
    }
    Ok(())
}
fn validate_remark(remark: &str) -> Result<(), ToolError> {
    if remark.chars().count() > 140 || remark.chars().any(char::is_control) {
        return Err(error("INVALID_DRAFT_REMARK"));
    }
    Ok(())
}
fn validate_amount(amount: &DecimalAmount) -> Result<(), ToolError> {
    // The unchanged draft core stores integer cents in u64; reject overflow
    // before it could replace a complete draft with NeedsInformation.
    let (whole, fraction) = amount.as_str().split_once('.').expect("canonical decimal");
    let cents = whole
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(100))
        .and_then(|n| n.checked_add(fraction.parse::<u64>().ok()?));
    if matches!(cents, None | Some(0)) {
        return Err(error("INVALID_DRAFT_AMOUNT"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMockDraftInput {
    pub payer_account_id: ResourceId,
    pub payee_id: ResourceId,
    pub amount: DecimalAmount,
    pub currency: CnyCurrency,
    #[serde(default)]
    pub remark: String,
    #[serde(default)]
    pub replaces: Option<Value>,
}

impl CreateMockDraftInput {
    fn parse(arguments: Value) -> Result<Self, ToolError> {
        let input: Self =
            serde_json::from_value(arguments).map_err(|_| error("INVALID_DRAFT_INPUT"))?;
        for id in [&input.payer_account_id, &input.payee_id] {
            GetAccountBalanceInput {
                account_id: id.clone(),
            }
            .validate()
            .map_err(|_| error("INVALID_RESOURCE_ID"))?;
        }
        if input.amount.as_str() == "0.00"
            || input.replaces.is_some()
            || input.remark.chars().count() > 140
            || input.remark.chars().any(char::is_control)
        {
            return Err(error("INVALID_DRAFT_INPUT"));
        }
        Ok(input)
    }
}

type CandidateKey = (UserId, TaskId, ResourceId);
type Receipt = (ExecutionContext, MockDraftAction, StoredDraft);

pub struct MockDraftStore<C> {
    bank: Arc<dyn BankGateway>,
    drafts: AsyncMutex<TransferDrafts<C>>,
    candidates: Mutex<HashMap<CandidateKey, String>>,
    receipts: Mutex<HashMap<ToolResultRef, Receipt>>,
}

impl<C: Clock + Send + Sync> MockDraftStore<C> {
    pub fn new(
        bank: Arc<dyn BankGateway>,
        clock: C,
        draft_ttl_seconds: u64,
    ) -> Result<Self, IntegrationError> {
        Ok(Self {
            drafts: AsyncMutex::new(TransferDrafts::new(bank.clone(), clock, draft_ttl_seconds)?),
            bank,
            candidates: Mutex::new(HashMap::new()),
            receipts: Mutex::new(HashMap::new()),
        })
    }

    /// Trusted lifecycle assembly only; NOT a Tool or a user confirmation. Read
    /// the current locked SQL record first. Restores no authority, receipts or
    /// candidate cache. Changing payee later requires a fresh find_candidates.
    pub async fn restore_for_backend(
        &self,
        context: &ExecutionContext,
        recovery: crate::DraftRestore,
    ) -> Result<StoredDraft, ToolError> {
        let mut drafts = self.drafts.lock().await;
        let mut candidates = self
            .candidates
            .lock()
            .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?;
        let restored = drafts
            .restore_for_backend(context, recovery)
            .map_err(|e| failure("DRAFT_RESTORE_REJECTED", e))?;
        candidates.retain(|(user, task, _), _| {
            user != &context.authenticated_user_id || task != &context.task_id
        });
        Ok(restored)
    }

    /// Trusted server lookup, not a client-provided list or a default selection.
    pub async fn find_candidates(
        &self,
        context: &ExecutionContext,
        query: &str,
    ) -> Result<FindPayeesOutput, ToolError> {
        FindPayeesInput {
            query: query.into(),
        }
        .validate()
        .map_err(|_| error("INVALID_PAYEE_QUERY"))?;
        let output = self
            .bank
            .find_payees(&context.authenticated_user_id, query)
            .await
            .map_err(|e| error(e.reason_code()))?;
        output.validate().map_err(|e| error(e.reason_code()))?;
        let mut candidates = self
            .candidates
            .lock()
            .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?;
        candidates.retain(|(user, task, _), _| {
            user != &context.authenticated_user_id || task != &context.task_id
        });
        for payee in &output.candidates {
            candidates.insert(
                (
                    context.authenticated_user_id.clone(),
                    context.task_id.clone(),
                    payee.payee_id.clone(),
                ),
                query.into(),
            );
        }
        Ok(output)
    }

    fn query(&self, context: &ExecutionContext, payee: &ResourceId) -> Result<String, ToolError> {
        self.candidates
            .lock()
            .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?
            .get(&(
                context.authenticated_user_id.clone(),
                context.task_id.clone(),
                payee.clone(),
            ))
            .cloned()
            .ok_or_else(|| error("PAYEE_NOT_IN_TRUSTED_CANDIDATES"))
    }

    async fn validate(
        &self,
        context: &ExecutionContext,
        input: &CreateMockDraftInput,
    ) -> Result<String, ToolError> {
        let query = self.query(context, &input.payee_id)?;
        self.validate_fields(context, input, &query).await?;
        Ok(query)
    }

    async fn validate_fields(
        &self,
        context: &ExecutionContext,
        input: &CreateMockDraftInput,
        query: &str,
    ) -> Result<(), ToolError> {
        let balance = self
            .bank
            .get_account_balance(&context.authenticated_user_id, &input.payer_account_id)
            .await
            .map_err(|e| error(e.reason_code()))?;
        balance
            .validate_for(&input.payer_account_id)
            .map_err(|e| error(e.reason_code()))?;
        let payees = self
            .bank
            .find_payees(&context.authenticated_user_id, query)
            .await
            .map_err(|e| error(e.reason_code()))?;
        payees.validate().map_err(|e| error(e.reason_code()))?;
        if !payees
            .candidates
            .iter()
            .any(|p| p.payee_id == input.payee_id)
        {
            return Err(error("PAYEE_NOT_IN_TRUSTED_CANDIDATES"));
        }
        Ok(())
    }

    async fn prepare_modify(
        &self,
        context: &ExecutionContext,
        input: &ModifyMockDraftInput,
    ) -> Result<(CreateMockDraftInput, String), ToolError> {
        let (saved, prior_query) = {
            let mut drafts = self.drafts.lock().await;
            let saved = drafts
                .current_for_review(context, &input.draft_id, input.expected_version)
                .map_err(|e| failure("DRAFT_MODIFY_REJECTED", e))?;
            let query = drafts.selections[&context.task_id]
                .query
                .clone()
                .ok_or_else(|| error("PAYEE_QUERY_MISSING"))?;
            (saved, query)
        };
        let details = saved
            .details
            .ok_or_else(|| error("DRAFT_DETAILS_MISSING"))?;
        let changes = &input.changes;
        let query = match &changes.payee_id {
            Some(id) => self.query(context, id)?,
            None => prior_query,
        };
        let effective = CreateMockDraftInput {
            payer_account_id: changes
                .payer_account_id
                .clone()
                .unwrap_or(details.account.account_id),
            payee_id: changes.payee_id.clone().unwrap_or(details.payee.payee_id),
            amount: changes.amount.clone().unwrap_or(details.amount),
            currency: changes.currency.unwrap_or(details.currency),
            remark: changes.remark.clone().unwrap_or(details.remark),
            replaces: None,
        };
        validate_amount(&effective.amount)?;
        self.validate_fields(context, &effective, &query).await?;
        Ok((effective, query))
    }

    async fn validate_cancel(
        &self,
        context: &ExecutionContext,
        input: &CancelMockDraftInput,
    ) -> Result<(), ToolError> {
        let saved = self.read(context, &input.draft_id).await?;
        if saved.plan_version != input.expected_version {
            return Err(error("DRAFT_VERSION_CONFLICT"));
        }
        if saved.status == DraftStatus::Expired {
            return Err(error("DRAFT_EXPIRED"));
        }
        Ok(())
    }

    pub async fn read(
        &self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
    ) -> Result<StoredDraft, ToolError> {
        self.drafts
            .lock()
            .await
            .read(context, draft_id)
            .map_err(|e| failure("DRAFT_READ_FAILED", e))
    }

    /// Read-only reconciliation of this mock task after an unknown/failed result;
    /// never resubmits the business operation.
    pub async fn read_created_for_task(
        &self,
        context: &ExecutionContext,
    ) -> Result<Option<StoredDraft>, ToolError> {
        let mut drafts = self.drafts.lock().await;
        let Some(selection) = drafts.selections.get(&context.task_id) else {
            return Ok(None);
        };
        if selection.user != context.authenticated_user_id {
            return Err(error("DRAFT_ACCESS_DENIED"));
        }
        let Some(id) = selection.draft_id.clone() else {
            return Ok(None);
        };
        drafts
            .read(context, &id)
            .map(Some)
            .map_err(|_| error("DRAFT_READ_FAILED"))
    }

    pub fn saved_receipt_count(&self) -> Result<usize, ToolError> {
        Ok(self
            .receipts
            .lock()
            .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?
            .len())
    }

    async fn verify_receipt(&self, context: &PostExecuteContext) -> Result<(), ToolError> {
        let ExecutionObservation::ResultReceived {
            tool_result_ref, ..
        } = &context.observation
        else {
            return Err(error("DRAFT_RESULT_NOT_RECEIVED"));
        };
        let (execution, action, expected) = self
            .receipts
            .lock()
            .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?
            .get(tool_result_ref)
            .cloned()
            .ok_or_else(|| error("DRAFT_RECEIPT_NOT_FOUND"))?;
        if context.tool_name.as_str() != action.tool_name()
            || context.operation_id != execution.operation_id
            || context.task_id != execution.task_id
            || context.node_id != execution.node_id
            || context.authenticated_user_id != execution.authenticated_user_id
        {
            return Err(error("DRAFT_RECEIPT_BINDING_MISMATCH"));
        }
        let actual = self.read(&execution, &expected.draft_id).await?;
        let expected_status = match action {
            MockDraftAction::Create | MockDraftAction::Modify => DraftStatus::ReadyToConfirm,
            MockDraftAction::Cancel => DraftStatus::Cancelled,
        };
        if actual != expected || actual.status != expected_status {
            return Err(error("DRAFT_READBACK_MISMATCH_OR_EXPIRED"));
        }
        Ok(())
    }
}

pub struct CreateMockDraftTool<C> {
    store: Arc<MockDraftStore<C>>,
    authority: Arc<MockDraftAuthority>,
}

impl<C> CreateMockDraftTool<C> {
    pub fn new(store: Arc<MockDraftStore<C>>, authority: Arc<MockDraftAuthority>) -> Self {
        Self { store, authority }
    }
}

impl<C: Clock + Send + Sync + 'static> AgentTool for CreateMockDraftTool<C> {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            name(),
            "Save a confirmed mock in-memory draft; never submit funds",
            json!({
                "type": "object", "properties": {
                    "payer_account_id": {"type": "string"}, "payee_id": {"type": "string"},
                    "amount": {"type": "string", "pattern": "^(0|[1-9][0-9]*)\\.[0-9]{2}$"},
                    "currency": {"type": "string", "enum": ["CNY"]},
                    "remark": {"type": "string", "maxLength": 140}, "replaces": {"type": "null"}
                }, "required": ["payer_account_id", "payee_id", "amount", "currency"],
                "additionalProperties": false
            }),
            MOCK_DRAFT_SCHEMA_VERSION,
            true,
            MOCK_DRAFT_CREATE_ACTION,
        )
        .expect("static mock definition")
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let ToolExecutionAuthorization::MockDraftWriteV1(permit) = authorization else {
                return Err(error("MOCK_DRAFT_AUTHORIZATION_REQUIRED"));
            };
            let input = CreateMockDraftInput::parse(arguments.clone())?;
            self.authority
                .claim_tool(permit, context, &name(), &arguments)
                .map_err(|_| error("MOCK_DRAFT_PERMIT_REJECTED"))?;
            let query = self.store.validate(context, &input).await?;
            let mut drafts = self.store.drafts.lock().await;
            let progress = drafts
                .create_guarded(
                    context,
                    DraftInput {
                        payer_account_id: Some(input.payer_account_id),
                        payee_query: Some(query),
                        payee: Some(PayeeSelection::Id(input.payee_id)),
                        amount: Some(input.amount.as_str().into()),
                    },
                    &input.remark,
                    || {
                        self.authority
                            .lock_claim_for_save(permit, context, &name(), &arguments)
                            .map_err(|_| IntegrationError::AuthorizationRejected)
                    },
                )
                .await
                .map_err(|e| failure("DRAFT_SAVE_FAILED", e))?;
            let saved = progress.saved.ok_or_else(|| error("DRAFT_NOT_SAVED"))?;
            let readback = drafts
                .read(context, &saved.draft_id)
                .map_err(|_| error("DRAFT_READBACK_FAILED"))?;
            if saved != readback || saved.status != DraftStatus::ReadyToConfirm {
                return Err(error("DRAFT_READBACK_MISMATCH"));
            }
            let data = draft_json(&saved, context.plan_version)?;
            let result_ref = ToolResultRef::parse(format!(
                "mock_draft_result_{}",
                context
                    .idempotency_key
                    .as_ref()
                    .ok_or_else(|| error("MISSING_REQUEST_ID"))?
            ))
            .map_err(|_| error("INVALID_RESULT_ID"))?;
            self.store
                .receipts
                .lock()
                .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?
                .insert(
                    result_ref.clone(),
                    (context.clone(), MockDraftAction::Create, saved),
                );
            let result = ToolResult::new(
                name(),
                context.operation_id.clone(),
                result_ref,
                ToolResultStatus::Succeeded,
                data,
            );
            self.authority
                .record_tool_result(permit, context, &arguments, &result)
                .map_err(|_| error("DRAFT_RESULT_REGISTRATION_FAILED"))?;
            Ok(result)
        })
    }
}

pub struct CreateMockDraftPolicy<C> {
    store: Arc<MockDraftStore<C>>,
    name: ToolName,
}

/// Two separately registered storage Tools, with no configurable arbitrary action.
pub struct MockDraftLifecycleTool<C> {
    store: Arc<MockDraftStore<C>>,
    authority: Arc<MockDraftAuthority>,
    action: MockDraftAction,
}

impl<C> MockDraftLifecycleTool<C> {
    pub fn modify(store: Arc<MockDraftStore<C>>, authority: Arc<MockDraftAuthority>) -> Self {
        Self {
            store,
            authority,
            action: MockDraftAction::Modify,
        }
    }
    pub fn cancel(store: Arc<MockDraftStore<C>>, authority: Arc<MockDraftAuthority>) -> Self {
        Self {
            store,
            authority,
            action: MockDraftAction::Cancel,
        }
    }
}

impl<C: Clock + Send + Sync + 'static> AgentTool for MockDraftLifecycleTool<C> {
    fn definition(&self) -> ToolDefinition {
        let mut schema = json!({
            "type": "object", "properties": {
                "draft_id": {"type": "string"},
                "expected_version": {"type": "integer", "minimum": 1, "maximum": u32::MAX}
            }, "required": ["draft_id", "expected_version"], "additionalProperties": false
        });
        if self.action == MockDraftAction::Modify {
            schema["properties"]["changes"] = json!({
                "type": "object", "minProperties": 1, "additionalProperties": false,
                "properties": {
                    "payer_account_id": {"type": "string"}, "payee_id": {"type": "string"},
                    "amount": {"type": "string", "pattern": "^(0|[1-9][0-9]*)\\.[0-9]{2}$"},
                    "currency": {"type": "string", "enum": ["CNY"]},
                    "remark": {"type": "string", "maxLength": 140}
                }
            });
            schema["required"]
                .as_array_mut()
                .expect("schema")
                .push(json!("changes"));
        }
        ToolDefinition::new(
            ToolName::parse(self.action.tool_name()).expect("static Tool"),
            "Explicitly confirmed mock in-memory draft lifecycle; no funds",
            schema,
            MOCK_DRAFT_SCHEMA_VERSION,
            true,
            self.action.action(),
        )
        .expect("static mock definition")
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let ToolExecutionAuthorization::MockDraftWriteV1(permit) = authorization else {
                return Err(error("MOCK_DRAFT_AUTHORIZATION_REQUIRED"));
            };
            let tool_name = ToolName::parse(self.action.tool_name()).expect("static Tool");
            self.authority
                .claim_tool(permit, context, &tool_name, &arguments)
                .map_err(|_| error("MOCK_DRAFT_PERMIT_REJECTED"))?;
            let (saved, expected_version) = match self.action {
                MockDraftAction::Modify => {
                    let input = ModifyMockDraftInput::parse(arguments.clone())?;
                    let (effective, query) = self.store.prepare_modify(context, &input).await?;
                    let mut drafts = self.store.drafts.lock().await;
                    let progress = drafts
                        .modify_guarded(
                            context,
                            &input.draft_id,
                            input.expected_version,
                            DraftChanges {
                                input: DraftInput {
                                    payer_account_id: Some(effective.payer_account_id),
                                    payee_query: Some(query),
                                    payee: Some(PayeeSelection::Id(effective.payee_id)),
                                    amount: Some(effective.amount.as_str().into()),
                                },
                                currency: None,
                                remark: input.changes.remark,
                            },
                            || {
                                self.authority
                                    .lock_claim_for_save(permit, context, &tool_name, &arguments)
                                    .map_err(|_| IntegrationError::AuthorizationRejected)
                            },
                        )
                        .await
                        .map_err(|e| failure("DRAFT_MODIFY_FAILED", e))?;
                    let saved = progress.saved.ok_or_else(|| error("DRAFT_NOT_SAVED"))?;
                    let readback = drafts
                        .read(context, &input.draft_id)
                        .map_err(|_| error("DRAFT_READBACK_FAILED"))?;
                    if saved != readback
                        || saved.status != DraftStatus::ReadyToConfirm
                        || saved.plan_version
                            != input
                                .expected_version
                                .checked_add(1)
                                .ok_or_else(|| error("DRAFT_VERSION_OVERFLOW"))?
                    {
                        return Err(error("DRAFT_READBACK_MISMATCH"));
                    }
                    (saved, input.expected_version)
                }
                MockDraftAction::Cancel => {
                    let input = CancelMockDraftInput::parse(arguments.clone())?;
                    let mut drafts = self.store.drafts.lock().await;
                    let saved = drafts
                        .cancel_guarded(context, &input.draft_id, input.expected_version, || {
                            self.authority
                                .lock_claim_for_save(permit, context, &tool_name, &arguments)
                                .map_err(|_| IntegrationError::AuthorizationRejected)
                        })
                        .map_err(|e| failure("DRAFT_CANCEL_FAILED", e))?;
                    let readback = drafts
                        .read(context, &input.draft_id)
                        .map_err(|_| error("DRAFT_READBACK_FAILED"))?;
                    if saved != readback
                        || saved.status != DraftStatus::Cancelled
                        || saved.plan_version != input.expected_version
                    {
                        return Err(error("DRAFT_READBACK_MISMATCH"));
                    }
                    (saved, input.expected_version)
                }
                MockDraftAction::Create => return Err(error("DRAFT_TOOL_MISMATCH")),
            };
            let mut data = draft_json(&saved, context.plan_version)?;
            data["action"] = json!(self.action.action());
            data["expected_version"] = json!(expected_version);
            let result_ref = ToolResultRef::parse(format!(
                "mock_draft_result_{}",
                context
                    .idempotency_key
                    .as_ref()
                    .ok_or_else(|| error("MISSING_REQUEST_ID"))?
            ))
            .map_err(|_| error("INVALID_RESULT_ID"))?;
            self.store
                .receipts
                .lock()
                .map_err(|_| error("DRAFT_STORE_UNAVAILABLE"))?
                .insert(result_ref.clone(), (context.clone(), self.action, saved));
            let result = ToolResult::new(
                tool_name,
                context.operation_id.clone(),
                result_ref,
                ToolResultStatus::Succeeded,
                data,
            );
            self.authority
                .record_tool_result(permit, context, &arguments, &result)
                .map_err(|_| error("DRAFT_RESULT_REGISTRATION_FAILED"))?;
            Ok(result)
        })
    }
}

pub struct MockDraftLifecyclePolicy<C> {
    store: Arc<MockDraftStore<C>>,
    action: MockDraftAction,
    name: ToolName,
}
impl<C> MockDraftLifecyclePolicy<C> {
    pub fn modify(store: Arc<MockDraftStore<C>>) -> Self {
        Self::new(store, MockDraftAction::Modify)
    }
    pub fn cancel(store: Arc<MockDraftStore<C>>) -> Self {
        Self::new(store, MockDraftAction::Cancel)
    }
    fn new(store: Arc<MockDraftStore<C>>, action: MockDraftAction) -> Self {
        Self {
            store,
            action,
            name: ToolName::parse(action.tool_name()).expect("static Tool"),
        }
    }
}
impl<C: Clock + Send + Sync + 'static> DomainPolicy for MockDraftLifecyclePolicy<C> {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let result = async {
                if context.tool_name != self.name {
                    return Err(error("DRAFT_TOOL_MISMATCH"));
                }
                let execution = ExecutionContext {
                    authenticated_user_id: context.authenticated_user_id.clone(),
                    task_id: context.task_id.clone(),
                    node_id: context.node_id.clone(),
                    operation_id: context.operation_id.clone(),
                    plan_version: context.plan_version,
                    idempotency_key: None,
                };
                match self.action {
                    MockDraftAction::Modify => {
                        let input =
                            ModifyMockDraftInput::parse(context.normalized_arguments.clone())?;
                        self.store
                            .prepare_modify(&execution, &input)
                            .await
                            .map(|_| ())
                    }
                    MockDraftAction::Cancel => {
                        let input =
                            CancelMockDraftInput::parse(context.normalized_arguments.clone())?;
                        self.store.validate_cancel(&execution, &input).await
                    }
                    MockDraftAction::Create => Err(error("DRAFT_TOOL_MISMATCH")),
                }
            }
            .await;
            Ok(lifecycle_findings(result))
        })
    }
    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let result = if context.tool_name != self.name {
                Err(error("DRAFT_TOOL_MISMATCH"))
            } else {
                self.store.verify_receipt(context).await
            };
            Ok(lifecycle_findings(result))
        })
    }
}

fn lifecycle_findings(result: Result<(), ToolError>) -> Vec<SecurityFinding> {
    let mut findings = findings(result);
    for finding in &mut findings {
        finding.policy_version =
            PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).expect("static policy");
    }
    findings
}

impl<C> CreateMockDraftPolicy<C> {
    pub fn new(store: Arc<MockDraftStore<C>>) -> Self {
        Self {
            store,
            name: name(),
        }
    }
}

impl<C: Clock + Send + Sync + 'static> DomainPolicy for CreateMockDraftPolicy<C> {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let result = async {
                if context.tool_name != self.name {
                    return Err(error("DRAFT_TOOL_MISMATCH"));
                }
                let input = CreateMockDraftInput::parse(context.normalized_arguments.clone())?;
                let execution = ExecutionContext {
                    authenticated_user_id: context.authenticated_user_id.clone(),
                    task_id: context.task_id.clone(),
                    node_id: context.node_id.clone(),
                    operation_id: context.operation_id.clone(),
                    plan_version: context.plan_version,
                    idempotency_key: None,
                };
                self.store.validate(&execution, &input).await.map(|_| ())
            }
            .await;
            Ok(findings(result))
        })
    }

    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move { Ok(findings(self.store.verify_receipt(context).await)) })
    }
}

fn draft_json(draft: &StoredDraft, runtime_plan_version: u32) -> Result<Value, ToolError> {
    let details = draft
        .details
        .as_ref()
        .ok_or_else(|| error("DRAFT_DETAILS_MISSING"))?;
    let seconds = i64::try_from(draft.expires_at).map_err(|_| error("INVALID_DRAFT_EXPIRY"))?;
    let expires_at = Utc
        .timestamp_opt(seconds, 0)
        .single()
        .ok_or_else(|| error("INVALID_DRAFT_EXPIRY"))?
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    Ok(json!({
        "draft_id": draft.draft_id, "operation_id": draft.operation_id, "task_id": draft.task_id,
        "plan_version": draft.plan_version, "runtime_plan_version": runtime_plan_version,
        "status": match draft.status {
            DraftStatus::ReadyToConfirm => "READY_TO_CONFIRM",
            DraftStatus::Cancelled => "CANCELLED",
            DraftStatus::Expired => "EXPIRED",
            DraftStatus::NeedsInformation => "NEEDS_INFORMATION",
        }, "expires_at": expires_at,
        "payer_account_id": details.account.account_id, "payee_id": details.payee.payee_id,
        "amount": details.amount, "currency": details.currency, "remark": details.remark
    }))
}

fn name() -> ToolName {
    ToolName::parse(MOCK_DRAFT_CREATE_TOOL).expect("static Tool")
}
fn error(code: &str) -> ToolError {
    ToolError {
        reason_code: ReasonCode::parse(code).expect("static reason code"),
        message: code.into(),
    }
}
fn failure(code: &str, cause: impl std::fmt::Display) -> ToolError {
    let mut result = error(code);
    result.message = cause.to_string();
    result
}
fn findings(result: Result<(), ToolError>) -> Vec<SecurityFinding> {
    match result {
        Ok(()) => vec![],
        Err(error) => vec![SecurityFinding {
            check_id: CheckId::parse("MOCK-DRAFT-01").expect("static check"),
            policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION).expect("static policy"),
            source: SourceName::parse("mock_draft_policy").expect("static source"),
            kind: FindingKind::HardDeny,
            reason_code: error.reason_code,
            evidence_refs: vec![],
            observed_value: None,
            event_ids: vec![],
            dedup_key: None,
            window_start_ms: None,
            window_end_ms: None,
        }],
    }
}
