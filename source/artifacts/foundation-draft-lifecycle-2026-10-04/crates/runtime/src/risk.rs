//! Central Demo scoring. Only backend assembly can enable a documented observation format.
use safeflow_contracts::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const CENTRAL_SCORING_VERSION: &str = "central-compute-v1";
pub const DEMO_RISK_VERSION: &str = "risk-v0.1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MissingRiskDataPolicy {
    SkipWithAudit,
    FailClosed,
    RequireStrongAuth,
    Reauthenticate,
    AddFixedRisk10,
}

#[derive(Clone, Debug, Serialize)]
pub struct RiskCheckDefinition {
    pub check_id: &'static str,
    pub policy_version: &'static str,
    pub weight: u32,
    pub threshold: &'static str,
    pub window_ms: Option<i64>,
    pub dedup_basis: &'static str,
    pub missing_data_policy: MissingRiskDataPolicy,
    pub applies_to: &'static str,
    pub data_source: &'static str,
    pub distinct_count_threshold: Option<u64>,
}

/// Parameters from specification 6.3, not production bank policy.
pub fn demo_risk_definitions() -> Vec<RiskCheckDefinition> {
    use MissingRiskDataPolicy::*;
    [
        (
            "COMMON_NEW_DEVICE_30D",
            15,
            "no successful session in 30d",
            Some(2_592_000_000),
            "user_id+device_id",
            SkipWithAudit,
            "*",
            "Device Service",
            None,
        ),
        (
            "COMMON_AUTH_FAILURE_10M",
            20,
            "2 distinct failed challenges",
            Some(600_000),
            "auth_challenge_id",
            FailClosed,
            "*",
            "Auth Service",
            Some(2),
        ),
        (
            "COMMON_HIGH_RISK_OP_10M",
            20,
            "3 distinct high risk operations",
            Some(600_000),
            "operation_id",
            RequireStrongAuth,
            "*",
            "RiskEvent Repository",
            Some(3),
        ),
        (
            "COMMON_STALE_SESSION_12H",
            10,
            "last full authentication >12h",
            None,
            "session_id",
            Reauthenticate,
            "*",
            "Session Store",
            None,
        ),
        (
            "TRANSFER_FIRST_PAYEE_90D",
            15,
            "no successful transfer in 90d",
            Some(7_776_000_000),
            "user_id+payee_id",
            AddFixedRisk10,
            "execute_transfer",
            "TransferRepository",
            None,
        ),
        (
            "TRANSFER_AMOUNT_DEVIATION_90D",
            20,
            "amount >3x median and >=500 CNY",
            Some(7_776_000_000),
            "operation_id+facts_version",
            AddFixedRisk10,
            "execute_transfer",
            "TransferRepository",
            None,
        ),
        (
            "TRANSFER_VELOCITY_10M",
            20,
            "3 distinct transfer operations",
            Some(600_000),
            "operation_id",
            RequireStrongAuth,
            "execute_transfer",
            "TransferRepository",
            Some(3),
        ),
        (
            "COMMON_RESOURCE_ENUM_10M",
            15,
            "20 distinct resource IDs",
            Some(600_000),
            "resource_id",
            SkipWithAudit,
            "*",
            "RiskEvent Repository",
            Some(20),
        ),
    ]
    .into_iter()
    .map(
        |(
            check_id,
            weight,
            threshold,
            window_ms,
            dedup_basis,
            missing_data_policy,
            applies_to,
            data_source,
            distinct_count_threshold,
        )| RiskCheckDefinition {
            check_id,
            policy_version: DEMO_RISK_VERSION,
            weight,
            threshold,
            window_ms,
            dedup_basis,
            missing_data_policy,
            applies_to,
            data_source,
            distinct_count_threshold,
        },
    )
    .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RiskBinding {
    tool: ToolName,
    check_id: String,
    source: SourceName,
}

#[derive(Default)]
pub struct CentralPolicyCatalog {
    bindings: Vec<RiskBinding>,
}
impl CentralPolicyCatalog {
    /// Trusted Rust assembly only. No weights, thresholds or versions accepted from Findings.
    /// Count observations use the existing Value unsigned integer and stable event_ids.
    pub fn enable_distinct_count_for_backend(
        &mut self,
        check_id: &str,
        tool: ToolName,
        source: SourceName,
    ) -> Result<(), SecurityError> {
        let rule = rule(check_id)?;
        if rule.distinct_count_threshold.is_none()
            || rule.applies_to != "*" && rule.applies_to != tool.as_str()
            || !safe_risk_reference(source.as_str())
            || self
                .bindings
                .iter()
                .any(|b| b.tool == tool && b.check_id == check_id)
        {
            return Err(invalid(
                "unsupported observation, scope, source or duplicate binding",
            ));
        }
        self.bindings.push(RiskBinding {
            tool,
            check_id: check_id.into(),
            source,
        });
        self.bindings
            .sort_by(|a, b| (&a.tool, &a.check_id).cmp(&(&b.tool, &b.check_id)));
        Ok(())
    }

    pub(crate) fn bound_tools(&self) -> impl Iterator<Item = &ToolName> {
        self.bindings.iter().map(|b| &b.tool)
    }

    pub fn enabled_for(&self, tool: &ToolName) -> bool {
        self.bindings.iter().any(|b| &b.tool == tool)
    }

    pub fn metadata(&self) -> Value {
        json!({"version":CENTRAL_SCORING_VERSION,"mode":"COMPUTE_AUDIT_ONLY",
            "parameter_basis":"Demo specification v0.1 section 6.3",
            "phase":"PRE_AUTH","score_cap":100,"tier_starts":[0,20,40,70],
            "observation":"distinct-event-count-v1","definitions":demo_risk_definitions(),
            "bindings":self.bindings,"unbound_status":"DISABLED_NOT_ASSESSED"})
    }

    /// The sole scorer. No event creation, mutable counters, business IO or gate upgrades.
    pub(crate) fn evaluate(
        &self,
        context: &SecurityContext,
        findings: Vec<SecurityFinding>,
    ) -> Result<(Vec<EvaluatedFinding>, u32), SecurityError> {
        let mut evaluated = Vec::new();
        let mut signals = BTreeMap::<String, SecurityFinding>::new();
        if findings.len() > 16 {
            return Err(invalid("too many Findings"));
        }
        for finding in findings {
            finding
                .validate()
                .map_err(|_| invalid("invalid Finding window"))?;
            if finding.kind != FindingKind::RiskSignal {
                if demo_risk_definitions()
                    .iter()
                    .any(|r| r.check_id == finding.check_id.as_str())
                {
                    return Err(invalid("risk rule emitted as non-risk Finding"));
                }
                evaluated.push(EvaluatedFinding {
                    finding,
                    applied_weight: None,
                    score_delta: 0,
                });
                continue;
            }
            let rule = rule(finding.check_id.as_str())?;
            let binding = self
                .bindings
                .iter()
                .find(|b| b.tool == context.tool_name && b.check_id == rule.check_id)
                .ok_or_else(|| invalid("risk rule not enabled for this tool"))?;
            if context.phase != SecurityPhase::PreAuth || finding.source != binding.source {
                return Err(invalid("wrong scoring phase or source"));
            }
            let mut finding = finding;
            validate_count_observation(&finding, &context.operation_id, context.facts_version)?;
            finding.event_ids.sort();
            finding.event_ids.dedup();
            finding.evidence_refs.sort();
            finding.evidence_refs.dedup();
            if let Some(prior) = signals.get(rule.check_id) {
                if prior != &finding {
                    return Err(invalid("conflicting duplicate risk observation"));
                }
            } else {
                signals.insert(rule.check_id.into(), finding);
            }
        }
        if context.phase == SecurityPhase::PreAuth {
            for binding in self.bindings.iter().filter(|b| b.tool == context.tool_name) {
                if !signals.contains_key(&binding.check_id) {
                    signals.insert(
                        binding.check_id.clone(),
                        SecurityFinding {
                            check_id: CheckId::parse(&binding.check_id).expect("registered check"),
                            policy_version: PolicyVersion::parse(DEMO_RISK_VERSION)
                                .expect("static version"),
                            source: binding.source.clone(),
                            kind: FindingKind::RiskSignal,
                            reason_code: ReasonCode::parse("MISSING_AUXILIARY_DATA")
                                .expect("static reason"),
                            evidence_refs: vec![],
                            observed_value: None,
                            event_ids: vec![],
                            dedup_key: None,
                            window_start_ms: None,
                            window_end_ms: None,
                        },
                    );
                }
            }
        }
        for finding in signals.into_values() {
            let rule = rule(finding.check_id.as_str())?;
            let delta = if let Some(value) = &finding.observed_value {
                if value.as_u64().expect("validated integer")
                    >= rule.distinct_count_threshold.expect("enabled count")
                {
                    rule.weight
                } else {
                    0
                }
            } else {
                match rule.missing_data_policy {
                    MissingRiskDataPolicy::SkipWithAudit => 0,
                    // Compute-only mode cannot silently introduce new authentication paths.
                    _ => {
                        return Err(invalid(
                            "required risk source unavailable; no zero-risk fallback",
                        ))
                    }
                }
            };
            evaluated.push(EvaluatedFinding {
                finding,
                applied_weight: Some(rule.weight),
                score_delta: delta,
            });
        }
        let score = capped_score(evaluated.iter().map(|f| f.score_delta));
        // Preserve the existing 16-Finding budget for unconfigured legacy tools.
        // Their assembly metadata still explicitly declares DISABLED_NOT_ASSESSED.
        if evaluated.len() == 16 && !self.enabled_for(&context.tool_name) {
            return Ok((evaluated, score));
        }
        evaluated.push(EvaluatedFinding {
            finding: SecurityFinding {
                check_id: CheckId::parse("COMMON-17").expect("static check"),
                policy_version: PolicyVersion::parse(CENTRAL_SCORING_VERSION)
                    .expect("static version"),
                source: SourceName::parse("security_kernel").expect("static source"),
                kind: FindingKind::Informational,
                reason_code: ReasonCode::parse(if context.phase != SecurityPhase::PreAuth {
                    "RISK_SCORING_NOT_APPLICABLE_PHASE"
                } else if !self.enabled_for(&context.tool_name) {
                    "RISK_SCORING_DISABLED_NOT_ASSESSED"
                } else if evaluated
                    .iter()
                    .any(|f| f.applied_weight.is_some() && f.finding.observed_value.is_none())
                {
                    "RISK_SCORING_PARTIAL_COMPUTE_AUDIT_ONLY"
                } else {
                    "RISK_SCORING_COMPUTE_AUDIT_ONLY"
                })
                .expect("static reason"),
                evidence_refs: vec![],
                observed_value: None,
                event_ids: vec![],
                dedup_key: None,
                window_start_ms: None,
                window_end_ms: None,
            },
            applied_weight: None,
            score_delta: 0,
        });
        if evaluated.len() > 16 {
            return Err(invalid("too many evaluated Findings"));
        }
        Ok((evaluated, score))
    }
}

fn rule(check_id: &str) -> Result<RiskCheckDefinition, SecurityError> {
    demo_risk_definitions()
        .into_iter()
        .find(|r| r.check_id == check_id)
        .ok_or_else(|| invalid("unknown central risk rule"))
}

/// Opaque redacted references only, never raw device/resource identifiers or credentials.
pub fn safe_risk_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
}

/// Shared persistence boundary: only the approved scalar count representation may be stored.
pub fn validate_count_observation(
    finding: &SecurityFinding,
    operation: &OperationId,
    facts_version: u32,
) -> Result<(), SecurityError> {
    let rule = rule(finding.check_id.as_str())?;
    if finding.kind != FindingKind::RiskSignal
        || finding.policy_version.as_str() != DEMO_RISK_VERSION
        || rule.distinct_count_threshold.is_none()
        || !safe_risk_reference(finding.source.as_str())
        || !safe_risk_reference(finding.reason_code.as_str())
    {
        return Err(invalid("unapproved risk version, kind or representation"));
    }
    if finding.observed_value.is_none() {
        if finding.reason_code.as_str() != "MISSING_AUXILIARY_DATA"
            || !finding.event_ids.is_empty()
            || !finding.evidence_refs.is_empty()
            || finding.dedup_key.is_some()
            || finding.window_start_ms.is_some()
            || finding.window_end_ms.is_some()
        {
            return Err(invalid("missing observation carries inconsistent evidence"));
        }
        return Ok(());
    }
    let count = finding
        .observed_value
        .as_ref()
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("observation must be a distinct event count"))?;
    let mut events = finding.event_ids.clone();
    events.sort();
    events.dedup();
    let window = finding.window_start_ms.zip(finding.window_end_ms);
    if finding.event_ids.len() > 64
        || count != events.len() as u64
        || events.iter().any(|e| !safe_risk_reference(e.as_str()))
        || finding.evidence_refs.is_empty()
        || finding.evidence_refs.len() > 4
        || finding
            .evidence_refs
            .iter()
            .any(|e| !safe_risk_reference(e.as_str()))
        || finding.dedup_key.as_deref()
            != Some(format!("{}:{}:{}", operation, rule.check_id, facts_version).as_str())
        || !matches!(window, Some((start,end)) if start>=0 && end.checked_sub(start)==rule.window_ms)
    {
        return Err(invalid(
            "inconsistent evidence, events, window or dedup binding",
        ));
    }
    Ok(())
}

pub fn risk_tier_for_score(score: u32) -> RiskTier {
    match score {
        0..=19 => RiskTier::Low,
        20..=39 => RiskTier::Medium,
        40..=69 => RiskTier::High,
        _ => RiskTier::Critical,
    }
}
fn capped_score(contributions: impl Iterator<Item = u32>) -> u32 {
    contributions.fold(0u32, |sum, delta| sum.saturating_add(delta).min(100))
}
fn invalid(message: &str) -> SecurityError {
    SecurityError {
        reason_code: ReasonCode::parse("CENTRAL_RISK_INVALID_OR_UNAVAILABLE")
            .expect("static reason"),
        message: message.into(),
    }
}

#[cfg(test)]
#[path = "risk_tests.rs"]
mod tests;
