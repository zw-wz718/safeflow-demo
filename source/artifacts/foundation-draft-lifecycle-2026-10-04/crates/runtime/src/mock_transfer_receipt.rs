//! Pre-integration comparison of the received transfer-owner proposal, not a funds adapter.
use safeflow_bank_gateway::{CnyCurrency, DecimalAmount};
use safeflow_contracts::{ExecutionObservation, OperationId, ResourceId, TaskId, ToolResultStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const MOCK_TRANSFER_PROPOSAL_SOURCE_REVISION: &str = "2a72b5d7ebfd7c77520b936333b2e13f8eccdcaf";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedRequest {
    task_id: TaskId,
    operation_id: OperationId,
    draft_id: ResourceId,
    draft_version: u32,
    payer_account_id: ResourceId,
    payee_id: ResourceId,
    amount: DecimalAmount,
    currency: CnyCurrency,
    remark: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceivedReceipt {
    proposal_version: u16,
    task_id: TaskId,
    operation_id: OperationId,
    draft_id: ResourceId,
    draft_version: u32,
    payer_account_id: ResourceId,
    payee_id: ResourceId,
    amount: DecimalAmount,
    currency: CnyCurrency,
    status: ToolResultStatus,
    // Value keeps these nullable fields mandatory on the proposal wire.
    reason_code: Value,
    completed_at: Value,
}

/// A comparison of untrusted proposal fields. This is not identity, durable binding,
/// an ExecutionObservation, PostExecuteAssessment, or permission to submit/retry.
/// ```compile_fail
/// let forged: safeflow_runtime::MockTransferReceiptProposalReview =
///     serde_json::from_str("{}").unwrap();
/// ```
pub struct MockTransferReceiptProposalReview {
    receipt: ReceivedReceipt,
}
#[derive(Serialize)]
pub struct ReceiptProposalSummary<'a> {
    scope: &'static str,
    reference_source_revision: &'static str,
    binding_scope: &'static str,
    task_id: &'a TaskId,
    business_operation_id: &'a OperationId,
    draft_id: &'a ResourceId,
    draft_version: u32,
    declared_status: ToolResultStatus,
    declared_reason_code: &'a Value,
    declared_completed_at: &'a Value,
    post_execute_assessed: bool,
    requires_authoritative_readback: bool,
    unproved_bindings: [&'static str; 4],
}

impl MockTransferReceiptProposalReview {
    pub fn summary(&self) -> ReceiptProposalSummary<'_> {
        ReceiptProposalSummary {
            scope: "UNVERIFIED_RECEIPT_PROPOSAL",
            reference_source_revision: MOCK_TRANSFER_PROPOSAL_SOURCE_REVISION,
            binding_scope: "RECEIPT_FIELDS_ONLY",
            task_id: &self.receipt.task_id,
            business_operation_id: &self.receipt.operation_id,
            draft_id: &self.receipt.draft_id,
            draft_version: self.receipt.draft_version,
            declared_status: self.receipt.status,
            declared_reason_code: &self.receipt.reason_code,
            declared_completed_at: &self.receipt.completed_at,
            post_execute_assessed: false,
            requires_authoritative_readback: true,
            unproved_bindings: [
                "REMARK",
                "AUTHENTICATED_USER",
                "IDEMPOTENCY_KEY",
                "DURABLE_PRE_SUBMIT_PUBLIC_OPERATION_MAPPING_AND_REFERENCES",
            ],
        }
    }

    /// Compares an existing observation; never manufactures its attempt/result refs.
    /// A later query can legitimately settle an earlier unknown attempt. This method
    /// does not prove receipt authenticity or perform historical state transitions.
    pub fn compare_existing_observation(
        &self,
        observation: &ExecutionObservation,
        phase: ReceiptObservationPhase,
    ) -> Result<ReceiptObservationComparison, ReceiptProposalError> {
        let Some(business_ref) = observation.business_operation_ref() else {
            return if phase == ReceiptObservationPhase::LaterQuery {
                Ok(ReceiptObservationComparison::EarlierAttemptNotComparable)
            } else {
                Err(ReceiptProposalError::ObservationConflict)
            };
        };
        if business_ref.as_str() != self.receipt.operation_id.as_str() {
            return Err(ReceiptProposalError::BindingMismatch(
                "business_operation_ref",
            ));
        }
        match observation {
            ExecutionObservation::ResultReceived { .. } => {}
            ExecutionObservation::OutcomeUnknown { .. } => {
                if phase == ReceiptObservationPhase::SameSubmissionAttempt
                    && (self.receipt.status != ToolResultStatus::Processing
                        || self.receipt.reason_code.as_str() != Some("OUTCOME_UNKNOWN"))
                {
                    return Err(ReceiptProposalError::ObservationConflict);
                }
            }
            ExecutionObservation::NotSubmitted { .. } => unreachable!("no business ref"),
        }
        Ok(match phase {
            ReceiptObservationPhase::SameSubmissionAttempt => {
                ReceiptObservationComparison::SameAttemptFieldsCompatible
            }
            ReceiptObservationPhase::LaterQuery => {
                ReceiptObservationComparison::LaterQueryFieldsCompatible
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptObservationPhase {
    SameSubmissionAttempt,
    LaterQuery,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptObservationComparison {
    SameAttemptFieldsCompatible,
    LaterQueryFieldsCompatible,
    EarlierAttemptNotComparable,
}

/// Both inputs are proposal JSON, not trusted identity or a persisted submission claim.
/// Only fields actually echoed by receipt v1 can be compared. In particular, a matching
/// draft_version is the business StoredDraft.plan_version, not a Runtime plan version.
pub fn review_mock_transfer_receipt_proposal(
    expected_request_json: &[u8],
    received_receipt_json: &[u8],
) -> Result<MockTransferReceiptProposalReview, ReceiptProposalError> {
    let expected: ExpectedRequest = serde_json::from_value(
        crate::planning_intake::parse_json(expected_request_json)
            .map_err(|_| ReceiptProposalError::InvalidJson)?,
    )
    .map_err(|_| ReceiptProposalError::InvalidRequest)?;
    let receipt: ReceivedReceipt = serde_json::from_value(
        crate::planning_intake::parse_json(received_receipt_json)
            .map_err(|_| ReceiptProposalError::InvalidJson)?,
    )
    .map_err(|_| ReceiptProposalError::InvalidReceipt)?;
    for id in [
        expected.task_id.as_str(),
        expected.operation_id.as_str(),
        expected.draft_id.as_str(),
        expected.payer_account_id.as_str(),
        expected.payee_id.as_str(),
    ] {
        valid_id(id).map_err(|_| ReceiptProposalError::InvalidRequest)?;
    }
    if expected.draft_version == 0
        || expected.remark.chars().count() > 200
        || expected.remark.chars().any(char::is_control)
        || !positive_cents(&expected.amount)
    {
        return Err(ReceiptProposalError::InvalidRequest);
    }
    if receipt.proposal_version != 1 {
        return Err(ReceiptProposalError::UnsupportedVersion);
    }
    for id in [
        receipt.task_id.as_str(),
        receipt.operation_id.as_str(),
        receipt.draft_id.as_str(),
        receipt.payer_account_id.as_str(),
        receipt.payee_id.as_str(),
    ] {
        valid_id(id).map_err(|_| ReceiptProposalError::InvalidReceipt)?;
    }
    if receipt.draft_version == 0 || !positive_cents(&receipt.amount) {
        return Err(ReceiptProposalError::InvalidReceipt);
    }
    macro_rules! same {
        ($field:ident) => {
            if expected.$field != receipt.$field {
                return Err(ReceiptProposalError::BindingMismatch(stringify!($field)));
            }
        };
    }
    same!(task_id);
    same!(operation_id);
    same!(draft_id);
    same!(draft_version);
    same!(payer_account_id);
    same!(payee_id);
    same!(amount);
    same!(currency);
    let valid_state = match receipt.status {
        ToolResultStatus::Succeeded => {
            receipt.reason_code.is_null()
                && receipt.completed_at.as_str().is_some_and(|time| {
                    chrono::DateTime::parse_from_rfc3339(time)
                        .is_ok_and(|parsed| parsed.offset().local_minus_utc() == 0)
                })
        }
        ToolResultStatus::Failed => {
            receipt.completed_at.is_null()
                && matches!(
                    receipt.reason_code.as_str(),
                    Some(
                        "BANK_DECLINED"
                            | "INSUFFICIENT_FUNDS"
                            | "ACCOUNT_OUTGOING_BLOCKED"
                            | "RECEIVER_BALANCE_OVERFLOW"
                    )
                )
        }
        ToolResultStatus::Processing => {
            receipt.completed_at.is_null()
                && matches!(
                    receipt.reason_code.as_str(),
                    Some("AWAITING_BANK_RESULT" | "OUTCOME_UNKNOWN")
                )
        }
    };
    if !valid_state {
        return Err(ReceiptProposalError::InvalidReceipt);
    }
    Ok(MockTransferReceiptProposalReview { receipt })
}
fn valid_id(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        Err(())
    } else {
        Ok(())
    }
}
fn positive_cents(amount: &DecimalAmount) -> bool {
    let Some((whole, fraction)) = amount.as_str().split_once('.') else {
        return false;
    };
    whole
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(100))
        .and_then(|v| fraction.parse::<u64>().ok().and_then(|f| v.checked_add(f)))
        .is_some_and(|v| v > 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptProposalError {
    InvalidJson,
    InvalidRequest,
    InvalidReceipt,
    UnsupportedVersion,
    BindingMismatch(&'static str),
    ObservationConflict,
}
impl fmt::Display for ReceiptProposalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => f.write_str("RECEIPT_PROPOSAL_JSON_REJECTED"),
            Self::InvalidRequest => f.write_str("RECEIPT_PROPOSAL_EXPECTED_REQUEST_REJECTED"),
            Self::InvalidReceipt => f.write_str("RECEIPT_PROPOSAL_RECEIPT_REJECTED"),
            Self::UnsupportedVersion => f.write_str("RECEIPT_PROPOSAL_VERSION_UNSUPPORTED"),
            Self::BindingMismatch(field) => write!(f, "RECEIPT_PROPOSAL_BINDING_MISMATCH:{field}"),
            Self::ObservationConflict => f.write_str("RECEIPT_PROPOSAL_OBSERVATION_CONFLICT"),
        }
    }
}
impl std::error::Error for ReceiptProposalError {}

#[cfg(test)]
#[path = "mock_transfer_receipt_tests.rs"]
mod tests;
