//! Isolated public Finding-contract probe, not member business data or device risk.
use safeflow_contracts::*;
use safeflow_runtime::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub const TOOL: &str = "central_count_probe";
pub const POLICY: &str = "central-probe-v1";
pub const CHECK: &str = "COMMON_RESOURCE_ENUM_10M";
pub const SOURCE: &str = "public_contract_probe";

pub struct Probe {
    pub tool: ToolName,
    pub findings: Vec<SecurityFinding>,
    pub policy_calls: Arc<AtomicUsize>,
}
impl AgentTool for Probe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.tool.clone(),
            "public contract probe",
            json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            1,
            false,
            "PUBLIC_CONTRACT_PROBE",
        )
        .unwrap()
    }
    fn execute<'a>(
        &'a self,
        _: &'a ExecutionContext,
        _: ToolExecutionAuthorization<'a>,
        _: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        panic!("security evaluation must never execute the probe")
    }
}
impl DomainPolicy for Probe {
    fn tool_name(&self) -> &ToolName {
        &self.tool
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        self.policy_calls.fetch_add(1, Ordering::SeqCst);
        let findings = if context.phase == SecurityPhase::PreAuth {
            self.findings
                .iter()
                .cloned()
                .map(|mut f| {
                    if f.kind == FindingKind::RiskSignal && f.observed_value.is_some() {
                        f.dedup_key = Some(format!(
                            "{}:{}:{}",
                            context.operation_id, f.check_id, context.facts_version
                        ));
                    }
                    f
                })
                .collect()
        } else {
            vec![]
        };
        Box::pin(async move { Ok(findings) })
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        panic!("no business execution in the public contract probe")
    }
}
pub fn count_finding(check: &str, count: u64) -> SecurityFinding {
    SecurityFinding {
        check_id: CheckId::parse(check).unwrap(),
        policy_version: PolicyVersion::parse(DEMO_RISK_VERSION).unwrap(),
        source: SourceName::parse(SOURCE).unwrap(),
        kind: FindingKind::RiskSignal,
        reason_code: ReasonCode::parse("DISTINCT_EVENTS_OBSERVED").unwrap(),
        evidence_refs: vec![EvidenceRef::parse("redacted_count_snapshot_1").unwrap()],
        observed_value: Some(json!(count)),
        event_ids: (0..count)
            .map(|n| EventId::parse(format!("opaque_event_{n}")).unwrap())
            .collect(),
        dedup_key: None,
        window_start_ms: Some(1_791_171_400_000),
        window_end_ms: Some(1_791_172_000_000),
    }
}
pub fn assemble(
    sessions: Arc<dyn SessionResolver>,
    level: SecurityLevel,
    checks: &[&str],
    findings: Vec<SecurityFinding>,
    calls: Arc<AtomicUsize>,
) -> RuntimeAssembly {
    let name = ToolName::parse(TOOL).unwrap();
    let probe = Arc::new(Probe {
        tool: name.clone(),
        findings,
        policy_calls: calls,
    });
    let mut tools = ToolRegistry::new();
    tools.register(probe.clone()).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(probe, PolicyVersion::parse(POLICY).unwrap(), 1)
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: name.clone(),
            tool_schema_version: 1,
            policy_version: PolicyVersion::parse(POLICY).unwrap(),
            minimum_level: level,
        })
        .unwrap();
    for check in checks {
        catalog
            .risk_catalog_for_backend()
            .enable_distinct_count_for_backend(
                check,
                name.clone(),
                SourceName::parse(SOURCE).unwrap(),
            )
            .unwrap();
    }
    RuntimeBootstrap::build(
        tools,
        policies,
        catalog,
        sessions,
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap()
}
pub fn input_contract() -> PreflightInputContract {
    PreflightInputContract {
        tool_name: ToolName::parse(TOOL).unwrap(),
        tool_schema_version: 1,
        policy_version: PolicyVersion::parse(POLICY).unwrap(),
        projection_revision: "central-probe-direct-v1".into(),
    }
}
