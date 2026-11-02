use crate::{
    DecisionId, OperationFingerprint, OperationId, PermitId, SecurityOutcome, ToolName, UserId,
};
use serde::Serialize;
use std::fmt;

pub const EXECUTION_PERMIT_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionPermitStatus {
    Active,
    Consumed,
    Revoked,
    Expired,
}

#[derive(Debug, Serialize)]
pub struct ExecutionPermit {
    contract_version: u16,
    permit_id: PermitId,
    operation_id: OperationId,
    decision_id: DecisionId,
    user_id: UserId,
    tool_name: ToolName,
    fingerprint: OperationFingerprint,
    facts_version: u32,
    issued_at_ms: i64,
    expires_at_ms: i64,
    status: ExecutionPermitStatus,
    consumed_at_ms: Option<i64>,
}

#[derive(Debug)]
pub struct PermitIssuanceRequest {
    pub permit_id: PermitId,
    pub operation_id: OperationId,
    pub decision_id: DecisionId,
    pub user_id: UserId,
    pub tool_name: ToolName,
    pub fingerprint: OperationFingerprint,
    pub facts_version: u32,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub security_outcome: SecurityOutcome,
}

#[derive(Debug)]
pub struct PermitUseRequest {
    pub operation_id: OperationId,
    pub user_id: UserId,
    pub tool_name: ToolName,
    pub fingerprint: OperationFingerprint,
    pub facts_version: u32,
    pub now_ms: i64,
}

impl ExecutionPermit {
    pub fn issue(request: PermitIssuanceRequest) -> Result<Self, PermitError> {
        if request.security_outcome != SecurityOutcome::ReadyToExecute {
            return Err(PermitError::DecisionNotReady(request.security_outcome));
        }
        if request.facts_version == 0 {
            return Err(PermitError::ZeroFactsVersion);
        }
        if request.issued_at_ms < 0 || request.expires_at_ms <= request.issued_at_ms {
            return Err(PermitError::InvalidValidityWindow);
        }

        Ok(Self {
            contract_version: EXECUTION_PERMIT_CONTRACT_VERSION,
            permit_id: request.permit_id,
            operation_id: request.operation_id,
            decision_id: request.decision_id,
            user_id: request.user_id,
            tool_name: request.tool_name,
            fingerprint: request.fingerprint,
            facts_version: request.facts_version,
            issued_at_ms: request.issued_at_ms,
            expires_at_ms: request.expires_at_ms,
            status: ExecutionPermitStatus::Active,
            consumed_at_ms: None,
        })
    }

    pub fn validate_for_use(&self, request: &PermitUseRequest) -> Result<(), PermitError> {
        if self.status != ExecutionPermitStatus::Active {
            return Err(PermitError::NotActive(self.status));
        }
        if request.now_ms < 0 {
            return Err(PermitError::InvalidCurrentTime);
        }
        if request.now_ms >= self.expires_at_ms {
            return Err(PermitError::Expired);
        }
        if self.operation_id != request.operation_id {
            return Err(PermitError::BindingMismatch("operation_id"));
        }
        if self.user_id != request.user_id {
            return Err(PermitError::BindingMismatch("user_id"));
        }
        if self.tool_name != request.tool_name {
            return Err(PermitError::BindingMismatch("tool_name"));
        }
        if self.fingerprint != request.fingerprint {
            return Err(PermitError::BindingMismatch("operation_fingerprint"));
        }
        if self.facts_version != request.facts_version {
            return Err(PermitError::BindingMismatch("facts_version"));
        }
        Ok(())
    }

    /// Marks the in-memory representation consumed.
    ///
    /// Persistence adapters must perform the equivalent conditional update in
    /// the same database transaction as the business submission.
    pub fn consume(mut self, request: &PermitUseRequest) -> Result<Self, PermitError> {
        self.validate_for_use(request)?;
        self.status = ExecutionPermitStatus::Consumed;
        self.consumed_at_ms = Some(request.now_ms);
        Ok(self)
    }

    pub fn revoke(mut self) -> Result<Self, PermitError> {
        if self.status != ExecutionPermitStatus::Active {
            return Err(PermitError::NotActive(self.status));
        }
        self.status = ExecutionPermitStatus::Revoked;
        Ok(self)
    }

    pub fn permit_id(&self) -> &PermitId {
        &self.permit_id
    }

    pub fn decision_id(&self) -> &DecisionId {
        &self.decision_id
    }

    pub fn status(&self) -> ExecutionPermitStatus {
        self.status
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermitError {
    DecisionNotReady(SecurityOutcome),
    ZeroFactsVersion,
    InvalidValidityWindow,
    InvalidCurrentTime,
    NotActive(ExecutionPermitStatus),
    Expired,
    BindingMismatch(&'static str),
}

impl fmt::Display for PermitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecisionNotReady(outcome) => {
                write!(formatter, "cannot issue permit from {outcome:?}")
            }
            Self::ZeroFactsVersion => {
                formatter.write_str("facts_version must be greater than zero")
            }
            Self::InvalidValidityWindow => {
                formatter.write_str("permit expiry must be later than a non-negative issue time")
            }
            Self::InvalidCurrentTime => formatter.write_str("current time must be non-negative"),
            Self::NotActive(status) => write!(formatter, "permit is not active: {status:?}"),
            Self::Expired => formatter.write_str("permit has expired"),
            Self::BindingMismatch(field) => write!(formatter, "permit binding mismatch: {field}"),
        }
    }
}

impl std::error::Error for PermitError {}
