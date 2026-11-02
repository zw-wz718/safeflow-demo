//! Isolated decorators of the delivered payee adapter; no new business implementation.
use crate::ReadOnlyToolRegistration;
use safeflow_bank_gateway::*;
use safeflow_contracts::*;
use safeflow_runtime::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

pub(crate) struct Probe {
    pub calls: Arc<AtomicUsize>,
    pub users: Arc<Mutex<Vec<UserId>>>,
    pub registration: ReadOnlyToolRegistration,
}
pub(crate) fn probe(bank: Arc<dyn BankGateway>, alias: bool) -> Probe {
    let name = ToolName::parse(if alias {
        "payees_registration_probe"
    } else {
        FIND_PAYEES
    })
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let users = Arc::new(Mutex::new(vec![]));
    Probe {
        calls: calls.clone(),
        users: users.clone(),
        registration: ReadOnlyToolRegistration {
            tool: Arc::new(ObservedTool {
                inner: FindPayeesTool::new(bank.clone()),
                name: name.clone(),
                calls,
                users,
            }),
            policy: Arc::new(ObservedPolicy {
                inner: FindPayeesPolicy::new(bank),
                name: name.clone(),
            }),
            policy_version: PolicyVersion::parse(PAYEES_POLICY_VERSION).unwrap(),
            compatible_schema_version: BANK_QUERY_SCHEMA_VERSION,
            profile: ToolSecurityProfile {
                tool_name: name.clone(),
                tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
                policy_version: PolicyVersion::parse(PAYEES_POLICY_VERSION).unwrap(),
                minimum_level: SecurityLevel::Green,
            },
            input: PreflightInputContract {
                tool_name: name,
                tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
                policy_version: PolicyVersion::parse(PAYEES_POLICY_VERSION).unwrap(),
                projection_revision: "bounded-direct-input-v2".into(),
            },
            approved_action: "READ_PAYEES".into(),
            implementation_revision: "delivered-payees-probe-v1".into(),
        },
    }
}
struct ObservedTool {
    inner: FindPayeesTool,
    name: ToolName,
    calls: Arc<AtomicUsize>,
    users: Arc<Mutex<Vec<UserId>>>,
}
impl AgentTool for ObservedTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        definition.name = self.name.clone();
        definition
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: serde_json::Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.users
            .lock()
            .unwrap()
            .push(context.authenticated_user_id.clone());
        Box::pin(async move {
            let mut result = self
                .inner
                .execute(context, authorization, arguments)
                .await?;
            result.tool_name = self.name.clone();
            Ok(result)
        })
    }
}
struct ObservedPolicy {
    inner: FindPayeesPolicy,
    name: ToolName,
}
impl DomainPolicy for ObservedPolicy {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let mut original = context.clone();
            original.tool_name = ToolName::parse(FIND_PAYEES).unwrap();
            self.inner.evaluate(&original).await
        })
    }
    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let mut original = context.clone();
            original.tool_name = ToolName::parse(FIND_PAYEES).unwrap();
            self.inner.verify_execution(&original).await
        })
    }
}
