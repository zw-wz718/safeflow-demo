//! Conditional PRE_COMMIT request syntax, never proof of verified gates.
use crate::{
    ArgumentsHash, AuthGrantRef, ConfirmationRef, IdempotencyKey, NodeId, OperationId,
    SecurityGate, SecurityPhase, SessionRef, TaskId, ToolName,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const PRE_COMMIT_SECURITY_CONTRACT_VERSION: u16 = 2;

/// A request declaration. Neither variant establishes trusted authentication evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "requirement",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum PreCommitAuthenticationV2 {
    NotRequired {},
    Required { auth_grant_ref: AuthGrantRef },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreCommitSecurityRequestV2 {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub operation_id: OperationId,
    pub facts_version: u32,
    pub phase: SecurityPhase,
    pub tool_name: ToolName,
    pub plan_version: u32,
    pub session_ref: SessionRef,
    pub normalized_arguments: Value,
    pub arguments_hash: ArgumentsHash,
    pub idempotency_key: IdempotencyKey,
    pub confirmation_ref: ConfirmationRef,
    pub authentication: PreCommitAuthenticationV2,
}

impl PreCommitSecurityRequestV2 {
    /// Validate syntax only. References and declarations are not authorization.
    pub fn validate(&self) -> Result<(), PreCommitContractError> {
        if self.contract_version != PRE_COMMIT_SECURITY_CONTRACT_VERSION {
            return Err(PreCommitContractError::UnsupportedVersion(
                self.contract_version,
            ));
        }
        if self.phase != SecurityPhase::PreCommit {
            return Err(PreCommitContractError::InvalidPhase);
        }
        if self.facts_version == 0 || self.plan_version == 0 {
            return Err(PreCommitContractError::ZeroVersion);
        }
        if matches!(&self.authentication, PreCommitAuthenticationV2::Required { auth_grant_ref }
            if auth_grant_ref.as_str().starts_with("sim_grant_"))
        {
            return Err(PreCommitContractError::IsolatedAuthentication);
        }
        Ok(())
    }

    /// Gates must come from the actual trusted decision, never from this request or an LLM.
    /// This checks consistency only; the backend must still verify all bound source evidence.
    pub fn validate_for_required_gates(
        &self,
        gates: &[SecurityGate],
    ) -> Result<(), PreCommitContractError> {
        self.validate()?;
        let requires_authentication = match gates {
            [SecurityGate::ExplicitConfirmation] => false,
            [SecurityGate::ExplicitConfirmation, SecurityGate::StrongAuthentication] => true,
            _ => return Err(PreCommitContractError::InvalidRequiredGates),
        };
        if requires_authentication
            != matches!(
                self.authentication,
                PreCommitAuthenticationV2::Required { .. }
            )
        {
            return Err(PreCommitContractError::AuthenticationGateMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCommitContractError {
    UnsupportedVersion(u16),
    InvalidPhase,
    ZeroVersion,
    InvalidRequiredGates,
    AuthenticationGateMismatch,
    IsolatedAuthentication,
}

impl fmt::Display for PreCommitContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported PRE_COMMIT version: {version}")
            }
            Self::InvalidPhase => formatter.write_str("conditional request requires PRE_COMMIT"),
            Self::ZeroVersion => formatter.write_str("facts and plan versions must be positive"),
            Self::InvalidRequiredGates => formatter.write_str(
                "required gates must include confirmation in canonical order without duplicates",
            ),
            Self::AuthenticationGateMismatch => {
                formatter.write_str("authentication declaration contradicts trusted required gates")
            }
            Self::IsolatedAuthentication => {
                formatter.write_str("isolated simulation grant cannot authorize PRE_COMMIT")
            }
        }
    }
}

impl std::error::Error for PreCommitContractError {}
