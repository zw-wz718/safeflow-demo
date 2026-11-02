//! Public minimum-level calculation only; no payment DomainPolicy or Permit authority.
use super::*;
use crate::Runtime;
use safeflow_contracts::{EvaluatedFinding, OperationId};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const PAYMENT_MINIMUM_VERSION: &str = "payment-used-plus-current-v1";

/// Trusted backend assembly only. Deliberately not deserializable from a request.
pub struct PaymentMinimumInput {
    pub operation_id: OperationId,
    pub facts_version: u32,
    pub used_cents: u64,
    pub current_cents: u64,
}

pub struct VerifiedPaymentMinimum {
    decision: SecurityDecision,
    binding: SessionBinding,
}
impl VerifiedPaymentMinimum {
    pub fn decision(&self) -> &SecurityDecision {
        &self.decision
    }
    pub fn session_binding(&self) -> &SessionBinding {
        &self.binding
    }
}

impl DeterministicSecurityKernel {
    // Shares the Kernel's gate/outcome rules without registering a fictitious funds Tool/Policy.
    fn evaluate_payment_minimum(
        input: PaymentMinimumInput,
    ) -> Result<SecurityDecision, SecurityError> {
        let combined = input
            .used_cents
            .checked_add(input.current_cents)
            .ok_or_else(|| contract_security_error("payment amount overflow"))?;
        if input.current_cents == 0 || input.facts_version == 0 {
            return Err(contract_security_error("positive payment facts required"));
        }
        let minimum_level = if combined > 100_000 {
            SecurityLevel::Red
        } else {
            SecurityLevel::Yellow
        };
        let mut marker = plan_finding(
            "COMMON-17",
            "PAYMENT_MINIMUM_ONLY_DOMAIN_POLICY_NOT_EVALUATED",
        );
        marker.kind = FindingKind::Informational;
        marker.policy_version = PolicyVersion::parse(PAYMENT_MINIMUM_VERSION).unwrap();
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let decision = SecurityDecision {
            contract_version: SECURITY_CONTRACT_VERSION,
            decision_id: DecisionId::parse(format!(
                "payment_minimum_{}",
                SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ))
            .unwrap(),
            operation_id: input.operation_id,
            facts_version: input.facts_version,
            policy_version: PolicyVersion::parse(PAYMENT_MINIMUM_VERSION).unwrap(),
            outcome: decide_outcome(&[], true, minimum_level),
            minimum_level,
            // No risk observations are supplied on this scoped path. This is not a low-risk assertion.
            risk_score: 0,
            risk_tier: crate::risk_tier_for_score(0),
            required_gates: required_gates(minimum_level, true),
            findings: vec![EvaluatedFinding {
                finding: marker,
                applied_weight: None,
                score_delta: 0,
            }],
            expires_at_ms: i64::MAX,
        };
        decision.validate().map_err(contract_security_error)?;
        Ok(decision)
    }
}

impl Runtime {
    pub fn payment_minimum_metadata_for_backend(&self) -> Value {
        json!({"version":PAYMENT_MINIMUM_VERSION,"threshold_cents":100_000,
            "scope":"PUBLIC_PAYMENT_MINIMUM_ONLY","risk_mode":"NOT_EVALUATED",
            "domain_policy_evaluated":false,"funds_execution_enabled":false,
            "kernel_source_digest":format!("{:x}",Sha256::digest(include_str!("../security.rs").replace("\r\n","\n").as_bytes())),
            "minimum_source_digest":format!("{:x}",Sha256::digest(include_str!("payment_minimum.rs").replace("\r\n","\n").as_bytes()))})
    }

    pub fn review_payment_minimum_for_backend(
        &self,
        session: &SessionRef,
        input: PaymentMinimumInput,
    ) -> Result<VerifiedPaymentMinimum, crate::RuntimeError> {
        let binding = self.session_binding_for_backend(session)?;
        let decision = DeterministicSecurityKernel::evaluate_payment_minimum(input)
            .map_err(|e| crate::RuntimeError::Security(e.to_string()))?;
        if self.session_binding_for_backend(session)? != binding {
            return Err(crate::RuntimeError::SessionChanged);
        }
        Ok(VerifiedPaymentMinimum { decision, binding })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payment_minimum_boundaries_checked_add_and_not_authorization() {
        for (used, current, level, outcome) in [
            (
                0,
                20_000,
                SecurityLevel::Yellow,
                SecurityOutcome::ReadyToConfirm,
            ),
            (
                80_000,
                20_000,
                SecurityLevel::Yellow,
                SecurityOutcome::ReadyToConfirm,
            ),
            (
                80_001,
                20_000,
                SecurityLevel::Red,
                SecurityOutcome::WaitingAuth,
            ),
            (
                90_000,
                20_000,
                SecurityLevel::Red,
                SecurityOutcome::WaitingAuth,
            ),
        ] {
            let result =
                DeterministicSecurityKernel::evaluate_payment_minimum(PaymentMinimumInput {
                    operation_id: OperationId::parse("payment_test").unwrap(),
                    facts_version: 1,
                    used_cents: used,
                    current_cents: current,
                })
                .unwrap();
            assert_eq!(result.minimum_level, level);
            assert_eq!(result.outcome, outcome);
            assert_eq!(result.required_gates, required_gates(level, true));
            assert_ne!(result.outcome, SecurityOutcome::ReadyToExecute);
        }
        for (used, current, facts) in [(u64::MAX, 1, 1), (0, 0, 1), (0, 1, 0)] {
            assert!(
                DeterministicSecurityKernel::evaluate_payment_minimum(PaymentMinimumInput {
                    operation_id: OperationId::parse("payment_test").unwrap(),
                    facts_version: facts,
                    used_cents: used,
                    current_cents: current,
                })
                .is_err()
            );
        }
    }
}
