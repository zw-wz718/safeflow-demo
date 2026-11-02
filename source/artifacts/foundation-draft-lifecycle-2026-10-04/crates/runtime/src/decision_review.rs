//! Backend-only preflight evidence, never Tool execution or Permit authority.
use crate::decision_input::{
    validate_direct_object, validate_direct_schema, validate_nested_object, validate_nested_schema,
};
use crate::runtime::hash_arguments;
use crate::{validate_tool_policy_bindings, Runtime, RuntimeError, SessionBinding};
use safeflow_contracts::{
    AgentPlan, OperationSecurityRequest, PlanAcceptanceDecision, PlanAcceptanceRequest, PlanNode,
    PolicyVersion, SecurityDecision, SecurityPhase, ToolName,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Explicit backend approval of a complete, direct input projection for a registered tool.
/// This is configuration, not a client-supplied permission or an OperationFingerprint.
#[derive(Clone, Debug, Serialize)]
pub struct PreflightInputContract {
    pub tool_name: ToolName,
    pub tool_schema_version: u32,
    pub policy_version: PolicyVersion,
    pub projection_revision: String,
}

/// Actual Kernel output; no public constructor or deserialization.
/// ```compile_fail
/// let forged: safeflow_runtime::VerifiedPreflightDecision =
///     serde_json::from_value(serde_json::json!({"outcome":"READY_TO_EXECUTE"})).unwrap();
/// ```
pub struct VerifiedPreflightDecision {
    decision: SecurityDecision,
    plan_review: PlanAcceptanceDecision,
    binding: SessionBinding,
    request_digest: safeflow_contracts::ArgumentsHash,
}
impl VerifiedPreflightDecision {
    pub fn decision(&self) -> &SecurityDecision {
        &self.decision
    }
    pub fn plan_review(&self) -> &PlanAcceptanceDecision {
        &self.plan_review
    }
    pub fn session_binding(&self) -> &SessionBinding {
        &self.binding
    }
    pub fn request_digest(&self) -> &safeflow_contracts::ArgumentsHash {
        &self.request_digest
    }
}

fn unavailable() -> RuntimeError {
    RuntimeError::RecoveryReview("approved direct preflight input/assembly unavailable".into())
}
fn source_digest(source: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(source.replace("\r\n", "\n").as_bytes())
    )
}
impl Runtime {
    /// Check the existing projection grammar at startup without inventing business inputs.
    pub fn validate_input_projection_for_backend(
        &self,
        contract: &PreflightInputContract,
    ) -> Result<(), RuntimeError> {
        let definition = self
            .tools
            .get(&contract.tool_name)
            .ok_or_else(unavailable)?
            .definition();
        if contract.projection_revision == safeflow_contracts::PAYMENT_LOCATOR_PROJECTION
            && (contract.tool_name.as_str() != safeflow_contracts::PAYMENT_INTENT_TOOL
                || definition.input_schema != safeflow_contracts::PaymentDraftLocator::schema())
        {
            return Err(unavailable());
        }
        if contract.projection_revision == "bounded-direct-input-v2" {
            validate_nested_schema(&definition.input_schema)
        } else {
            validate_direct_schema(&definition.input_schema)
        }
        .map_err(|_| unavailable())
    }

    pub fn lifecycle_source_metadata_for_backend(&self) -> Result<Value, RuntimeError> {
        let mut metadata = self.preflight_source_metadata_for_backend()?;
        metadata["source_contract"] = json!("trusted-runtime-lifecycle-v2");
        metadata["nested_input_source_digest"] =
            source_digest(include_str!("decision_input_v2.rs")).into();
        metadata["action_runtime_source_digest"] = source_digest(include_str!("runtime.rs")).into();
        metadata["action_authority_source_digest"] =
            source_digest(include_str!("mock_draft.rs")).into();
        Ok(metadata)
    }

    pub fn preflight_source_metadata_for_backend(&self) -> Result<Value, RuntimeError> {
        validate_tool_policy_bindings(&self.tools, &self.policies, &self.catalog)
            .map_err(|_| unavailable())?;
        let mut metadata = json!({"source_contract":"trusted-runtime-preflight-v1",
            "pattern_contract":"ecmascript-safe-subset-v1","regex_version":"1.10.4","regex_syntax_version":"0.8.11",
            "tools":self.tools.definitions(),"policies":self.policies.definitions(),
            "profiles":self.catalog.profiles(),
            "central_scoring":self.catalog.risk.metadata(),
            "scoring_source_digest":source_digest(include_str!("risk.rs")),
            "registry_source_digest":source_digest(include_str!("registry.rs")),
            "kernel_source_digest":source_digest(include_str!("security.rs")),
            "startup_source_digest":source_digest(include_str!("startup.rs")),
            "review_source_digest":source_digest(include_str!("decision_review.rs")),
            "input_source_digest":source_digest(include_str!("decision_input.rs"))});
        if let Some(registration) = &self.backend_registration {
            metadata["backend_registration"] = registration.clone();
        }
        Ok(metadata)
    }

    pub fn has_central_scoring_for_backend(&self, tool: &ToolName) -> bool {
        self.catalog.risk.enabled_for(tool)
    }

    /// No references are resolved and no business projection is invented here.
    /// The approved Policy validates the business constraints during actual evaluation.
    pub fn direct_preflight_arguments_for_backend(
        &self,
        tool: &ToolName,
        arguments: &Value,
        contract: &PreflightInputContract,
    ) -> Result<Value, RuntimeError> {
        self.direct_arguments(tool, arguments, contract, false)
    }

    pub fn lifecycle_arguments_for_backend(
        &self,
        tool: &ToolName,
        arguments: &Value,
        contract: &PreflightInputContract,
    ) -> Result<Value, RuntimeError> {
        self.direct_arguments(tool, arguments, contract, true)
    }

    fn direct_arguments(
        &self,
        tool: &ToolName,
        arguments: &Value,
        contract: &PreflightInputContract,
        nested: bool,
    ) -> Result<Value, RuntimeError> {
        let definition = self.tools.get(tool).ok_or_else(unavailable)?.definition();
        let policy = self.policies.definition(tool).ok_or_else(unavailable)?;
        // A locator is only a planning input. Never treat it as complete direct security evidence.
        if contract.projection_revision == safeflow_contracts::PAYMENT_LOCATOR_PROJECTION {
            return Err(unavailable());
        }
        if contract.tool_name != *tool
            || contract.tool_schema_version != definition.schema_version
            || contract.policy_version != policy.policy_version
            || contract.projection_revision.is_empty()
            || contract.projection_revision.len() > 80
            || !contract
                .projection_revision
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err(unavailable());
        }
        if nested {
            if contract.projection_revision != "bounded-direct-input-v2" {
                return Err(unavailable());
            }
            validate_nested_object(&definition.input_schema, arguments)
        } else {
            validate_direct_object(&definition.input_schema, arguments)
        }
        .map_err(|_| unavailable())?;
        Ok(arguments.clone())
    }

    /// Storage builds this request from current database facts; no caller-supplied decision is saved.
    pub async fn review_preflight_security_for_backend(
        &self,
        plan: &AgentPlan,
        request: OperationSecurityRequest,
        contract: &PreflightInputContract,
    ) -> Result<VerifiedPreflightDecision, RuntimeError> {
        self.review_preflight(plan, request, contract, false).await
    }

    pub async fn review_lifecycle_security_for_backend(
        &self,
        plan: &AgentPlan,
        request: OperationSecurityRequest,
        contract: &PreflightInputContract,
    ) -> Result<VerifiedPreflightDecision, RuntimeError> {
        self.review_preflight(plan, request, contract, true).await
    }

    async fn review_preflight(
        &self,
        plan: &AgentPlan,
        request: OperationSecurityRequest,
        contract: &PreflightInputContract,
        nested: bool,
    ) -> Result<VerifiedPreflightDecision, RuntimeError> {
        if request.phase == SecurityPhase::PreCommit {
            return Err(unavailable());
        }
        self.preflight_source_metadata_for_backend()?;
        request.validate().map_err(|_| unavailable())?;
        if request.confirmation_ref.is_some()
            || request.auth_grant_ref.is_some()
            || request.idempotency_key.is_some()
            || plan.task_id != request.task_id
            || plan.plan_version != request.plan_version
            || hash_arguments(&request.normalized_arguments) != request.arguments_hash
        {
            return Err(unavailable());
        }
        let node = plan
            .nodes
            .iter()
            .find(|n| n.id() == &request.node_id)
            .ok_or_else(unavailable)?;
        if !matches!(node,PlanNode::Tool{tool,arguments,..} if *tool==request.tool_name && *arguments==request.normalized_arguments)
        {
            return Err(unavailable());
        }
        self.direct_arguments(
            &request.tool_name,
            &request.normalized_arguments,
            contract,
            nested,
        )?;
        let binding = self.session_binding_for_backend(&request.session_ref)?;
        let session = request.session_ref.clone();
        let request_digest =
            hash_arguments(&serde_json::to_value(&request).map_err(|_| unavailable())?);
        let review = self
            .security
            .validate_plan(PlanAcceptanceRequest::new(plan.clone()).map_err(|_| unavailable())?)
            .await
            .map_err(|_| unavailable())?;
        if !review.accepted {
            return Err(RuntimeError::PlanRejected);
        }
        if self.session_binding_for_backend(&session)? != binding {
            return Err(RuntimeError::SessionChanged);
        }
        let decision = self
            .security
            .evaluate(request)
            .await
            .map_err(|_| unavailable())?;
        if self.session_binding_for_backend(&session)? != binding {
            return Err(RuntimeError::SessionChanged);
        }
        Ok(VerifiedPreflightDecision {
            decision,
            plan_review: review,
            binding,
            request_digest,
        })
    }
}
