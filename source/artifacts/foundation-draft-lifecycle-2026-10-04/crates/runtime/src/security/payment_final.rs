//! Adopted backend policy plus transaction-local facts, evaluated by the same Kernel.
use super::*;
use crate::{Runtime, ToolSecurityProfile};
use std::any::Any;

/// Registered once at backend assembly. Implementations must reject wrong fact
/// types and check their exact context; neither facts nor policy are wire DTOs.
pub trait BackendFinalPolicy: Send + Sync {
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
        facts: &'a (dyn Any + Send + Sync),
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>>;
}
pub struct BackendFinalPolicyRegistration {
    pub(crate) profile: ToolSecurityProfile,
    implementation_revision: String,
    policy: Arc<dyn BackendFinalPolicy>,
}
impl BackendFinalPolicyRegistration {
    pub fn for_reviewed_backend(
        profile: ToolSecurityProfile,
        implementation_revision: &str,
        policy: Arc<dyn BackendFinalPolicy>,
    ) -> Result<Self, SecurityError> {
        if profile.tool_schema_version == 0
            || implementation_revision.trim().is_empty()
            || implementation_revision.len() > 256
        {
            return Err(contract_security_error("invalid final policy registration"));
        }
        Ok(Self {
            profile,
            implementation_revision: implementation_revision.into(),
            policy,
        })
    }
}
impl Runtime {
    pub fn final_payment_uses_session_resolver_for_backend(
        &self,
        sessions: &Arc<dyn SessionResolver>,
    ) -> bool {
        Arc::ptr_eq(&self.sessions, sessions)
    }
    pub(crate) fn validate_final_registration(
        &self,
        registration: &BackendFinalPolicyRegistration,
    ) -> Result<(), SecurityError> {
        let profile = &registration.profile;
        // No model Tool is required for backend composition. An already adopted
        // regular Tool's identity, Policy and higher gates remain binding.
        match (
            self.tools.get(&profile.tool_name),
            self.policies.definition(&profile.tool_name),
            self.catalog.get(&profile.tool_name),
        ) {
            (None, None, None) => Ok(()),
            (Some(tool), Some(policy), Some(adopted))
                if tool.definition().has_side_effect
                    && tool.definition().schema_version == profile.tool_schema_version
                    && policy.compatible_tool_schema_version == profile.tool_schema_version
                    && policy.policy_version == profile.policy_version
                    && adopted.tool_schema_version == profile.tool_schema_version
                    && adopted.policy_version == profile.policy_version =>
            {
                Ok(())
            }
            _ => Err(contract_security_error(
                "final payment registered configuration mismatch",
            )),
        }
    }
    pub fn validate_final_payment_binding_for_backend(
        &self,
        expected: &ToolSecurityProfile,
        implementation_revision: &str,
    ) -> Result<(), SecurityError> {
        let adopted = self
            .final_payment
            .as_ref()
            .ok_or_else(|| contract_security_error("final payment policy not adopted"))?;
        if adopted.profile.tool_name != expected.tool_name
            || adopted.profile.tool_schema_version != expected.tool_schema_version
            || adopted.profile.policy_version != expected.policy_version
            || adopted.implementation_revision != implementation_revision
            || self.final_kernel.is_none()
        {
            return Err(contract_security_error(
                "final payment implementation binding mismatch",
            ));
        }
        self.validate_final_registration(adopted)
    }
    /// A public decision is not a permit. The sealed storage issuer independently
    /// verifies the persisted original confirmation on save/consume/final guard.
    pub async fn final_payment_policy_for_backend(
        &self,
        session: &SessionRef,
        context: &SecurityContext,
        expected: &ToolSecurityProfile,
        implementation_revision: &str,
        facts: &(dyn Any + Send + Sync),
        used_cents: u64,
        current_cents: u64,
    ) -> Result<SecurityDecision, SecurityError> {
        self.validate_final_payment_binding_for_backend(expected, implementation_revision)?;
        self.final_kernel
            .as_ref()
            .ok_or_else(|| contract_security_error("final Kernel unavailable"))?
            .evaluate_final_payment(
                session,
                context,
                self.final_payment.as_ref().unwrap(),
                expected.minimum_level,
                facts,
                Some((used_cents, current_cents)),
            )
            .await
    }

    /// Backend-owned non-funding composition. Uses the SAME Kernel and adopted
    /// higher gates, without inventing an amount or entering the funds policy.
    pub async fn final_nonfunding_policy_for_backend(
        &self,
        session: &SessionRef,
        context: &SecurityContext,
        registration: &BackendFinalPolicyRegistration,
        facts: &(dyn Any + Send + Sync),
    ) -> Result<SecurityDecision, SecurityError> {
        self.validate_final_registration(registration)?;
        self.final_kernel
            .as_ref()
            .ok_or_else(|| contract_security_error("final Kernel unavailable"))?
            .evaluate_final_payment(
                session,
                context,
                registration,
                std::cmp::max(SecurityLevel::Yellow, registration.profile.minimum_level),
                facts,
                None,
            )
            .await
    }
}
impl DeterministicSecurityKernel {
    async fn evaluate_final_payment(
        &self,
        session: &SessionRef,
        context: &SecurityContext,
        adopted: &BackendFinalPolicyRegistration,
        requested_minimum: SecurityLevel,
        facts: &(dyn Any + Send + Sync),
        funding: Option<(u64, u64)>,
    ) -> Result<SecurityDecision, SecurityError> {
        let before = self.sessions.resolve_binding(session)?;
        let profile = &adopted.profile;
        if before.binding_version == 0
            || context.authenticated_user_id != before.user_id
            || context.contract_version != SECURITY_CONTRACT_VERSION
            || context.facts_version == 0
            || context.plan_version == 0
            || context.phase != SecurityPhase::PreCommit
            || context.tool_name != profile.tool_name
            || funding.is_some_and(|(_, current)| current == 0)
            || context.confirmation_ref.is_none()
            || context.idempotency_key.is_none()
            || context.auth_grant_ref.is_some()
            || crate::runtime::hash_arguments(&context.normalized_arguments)
                != context.arguments_hash
        {
            return Err(contract_security_error("final payment binding mismatch"));
        }
        let mut minimum_level = std::cmp::max(requested_minimum, profile.minimum_level);
        if let Some((used, current)) = funding {
            let combined = used
                .checked_add(current)
                .ok_or_else(|| contract_security_error("amount overflow"))?;
            minimum_level = std::cmp::max(
                minimum_level,
                if combined > 100_000 {
                    SecurityLevel::Red
                } else {
                    SecurityLevel::Yellow
                },
            );
        }
        let mut findings = Vec::new();
        if let Some(registered) = self.catalog.get(&profile.tool_name) {
            minimum_level = std::cmp::max(minimum_level, registered.minimum_level);
            let policy = self
                .policies
                .get(&profile.tool_name)
                .ok_or_else(|| contract_security_error("adopted policy missing"))?;
            findings.extend(policy.evaluate(context).await?);
        }
        findings.extend(adopted.policy.evaluate(context, facts).await?);
        let outcome = match decide_outcome(&findings, true, minimum_level) {
            SecurityOutcome::ReadyToConfirm => SecurityOutcome::ReadyToExecute,
            other => other,
        };
        let (findings, risk_score) = self.catalog.risk.evaluate(context, findings)?;
        if self.sessions.resolve_binding(session)? != before {
            return Err(contract_security_error("session changed"));
        }
        let decision = SecurityDecision {
            contract_version: SECURITY_CONTRACT_VERSION,
            decision_id: self.next_decision_id(),
            operation_id: context.operation_id.clone(),
            facts_version: context.facts_version,
            policy_version: profile.policy_version.clone(),
            outcome,
            minimum_level,
            risk_score,
            risk_tier: crate::risk_tier_for_score(risk_score),
            required_gates: required_gates(minimum_level, true),
            findings,
            expires_at_ms: i64::MAX,
        };
        decision.validate().map_err(contract_security_error)?;
        Ok(decision)
    }
}

#[cfg(test)]
mod nonfunding_tests {
    use super::*;
    use crate::{
        DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeBootstrap, StaticSessionResolver,
        ToolRegistry, ToolSecurityCatalog,
    };
    use safeflow_contracts::*;
    use serde_json::{json, Value};
    struct Sentinel;
    impl AgentTool for Sentinel {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                ToolName::parse("nonfunding_sentinel").unwrap(),
                "public mechanism",
                json!({"type":"object"}),
                1,
                true,
                "PUBLIC_TEST",
            )
            .unwrap()
        }
        fn execute<'a>(
            &'a self,
            _: &'a ExecutionContext,
            _: ToolExecutionAuthorization<'a>,
            _: Value,
        ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
            panic!("security must never execute a tool")
        }
    }
    impl DomainPolicy for Sentinel {
        fn tool_name(&self) -> &ToolName {
            static NAME: std::sync::OnceLock<ToolName> = std::sync::OnceLock::new();
            NAME.get_or_init(|| ToolName::parse("nonfunding_sentinel").unwrap())
        }
        fn evaluate<'a>(
            &'a self,
            _: &'a SecurityContext,
        ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn verify_execution<'a>(
            &'a self,
            _: &'a PostExecuteContext,
        ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
            panic!("not post execute")
        }
    }
    impl BackendFinalPolicy for Sentinel {
        fn evaluate<'a>(
            &'a self,
            _: &'a SecurityContext,
            facts: &'a (dyn Any + Send + Sync),
        ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
            Box::pin(async move {
                if facts.downcast_ref::<u32>() == Some(&1) {
                    Ok(vec![])
                } else {
                    Err(contract_security_error("wrong facts"))
                }
            })
        }
    }
    #[tokio::test]
    async fn nonfunding_same_kernel_keeps_higher_adopted_gates_and_rejects_wrong_facts() {
        for level in [
            SecurityLevel::Yellow,
            SecurityLevel::Red,
            SecurityLevel::Critical,
        ] {
            let sentinel = Arc::new(Sentinel);
            let name = DomainPolicy::tool_name(&*sentinel).clone();
            let version = PolicyVersion::parse("public-policy-v1").unwrap();
            let mut tools = ToolRegistry::new();
            tools.register(sentinel.clone()).unwrap();
            let mut policies = DomainPolicyRegistry::new();
            policies
                .register(sentinel.clone(), version.clone(), 1)
                .unwrap();
            let mut catalog = ToolSecurityCatalog::new();
            catalog
                .register(ToolSecurityProfile {
                    tool_name: name.clone(),
                    tool_schema_version: 1,
                    policy_version: version.clone(),
                    minimum_level: level,
                })
                .unwrap();
            let session = SessionRef::parse("nonfunding_session").unwrap();
            let owner = UserId::parse("owner").unwrap();
            let mut sessions = StaticSessionResolver::new();
            sessions.insert(session.clone(), owner.clone());
            let assembly = RuntimeBootstrap::build(
                tools,
                policies,
                catalog,
                Arc::new(sessions),
                Arc::new(InMemoryAuditRecorder::new()),
            )
            .unwrap();
            let registration = BackendFinalPolicyRegistration::for_reviewed_backend(
                ToolSecurityProfile {
                    tool_name: name.clone(),
                    tool_schema_version: 1,
                    policy_version: version,
                    minimum_level: SecurityLevel::Yellow,
                },
                "public-test-v1",
                sentinel,
            )
            .unwrap();
            let context = SecurityContext {
                contract_version: SECURITY_CONTRACT_VERSION,
                phase: SecurityPhase::PreCommit,
                task_id: TaskId::parse("task").unwrap(),
                node_id: NodeId::parse("node").unwrap(),
                operation_id: OperationId::parse("operation").unwrap(),
                facts_version: 1,
                tool_name: name,
                plan_version: 1,
                authenticated_user_id: owner,
                normalized_arguments: json!({}),
                arguments_hash: crate::runtime::hash_arguments(&json!({})),
                idempotency_key: Some(IdempotencyKey::parse("operation").unwrap()),
                confirmation_ref: Some(ConfirmationRef::parse("confirmation").unwrap()),
                auth_grant_ref: None,
            };
            let decision = assembly
                .runtime()
                .final_nonfunding_policy_for_backend(&session, &context, &registration, &1u32)
                .await
                .unwrap();
            assert_eq!(decision.minimum_level, level);
            assert_eq!(decision.required_gates, required_gates(level, true));
            if level > SecurityLevel::Yellow {
                assert_ne!(
                    decision.required_gates,
                    vec![SecurityGate::ExplicitConfirmation]
                );
            }
            assert!(assembly
                .runtime()
                .final_nonfunding_policy_for_backend(&session, &context, &registration, &2u32)
                .await
                .is_err());
        }
    }
}
