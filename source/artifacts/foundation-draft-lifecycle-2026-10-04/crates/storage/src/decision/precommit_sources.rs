//! Partial public source inspection. Business idempotency and execution authority are absent.
use super::*;
use safeflow_contracts::{IdempotencyKey, PreCommitAuthenticationV2, PreCommitSecurityRequestV2};

pub const PRE_COMMIT_PUBLIC_SOURCE_REPORT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingPreCommitSource {
    BusinessIdempotency,
    BusinessProjection,
    FinalApprovalRules,
    FormalAuthentication,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreCommitPublicSourceDisposition {
    RejectedMissingSources,
}

/// Partial inspection with an explicit rejection, never complete PRE_COMMIT evidence.
/// The request's idempotency key and any grant reference remain unverified declarations.
/// ```compile_fail
/// let forged: safeflow_storage::PreCommitPublicSourceReport =
///     serde_json::from_value(serde_json::json!({"verified":true})).unwrap();
/// ```
/// ```compile_fail
/// async fn promote(store: &safeflow_storage::PermitStore, request: &safeflow_contracts::PreCommitSecurityRequestV2, report: &safeflow_storage::PreCommitPublicSourceReport) {
///     store.save_verified(&request.session_ref, report).await;
/// }
/// ```
pub struct PreCommitPublicSourceReport {
    declaration: PreCommitSecurityRequestV2,
    sources: VerifiedPreflightConfirmation,
    missing: Vec<MissingPreCommitSource>,
}
impl PreCommitPublicSourceReport {
    pub fn contract_version(&self) -> u16 {
        PRE_COMMIT_PUBLIC_SOURCE_REPORT_VERSION
    }
    pub fn disposition(&self) -> PreCommitPublicSourceDisposition {
        PreCommitPublicSourceDisposition::RejectedMissingSources
    }
    pub fn request_declaration(&self) -> &PreCommitSecurityRequestV2 {
        &self.declaration
    }
    pub fn unverified_idempotency_key(&self) -> &IdempotencyKey {
        &self.declaration.idempotency_key
    }
    pub fn preflight_sources(&self) -> &VerifiedPreflightConfirmation {
        &self.sources
    }
    pub fn missing_sources(&self) -> &[MissingPreCommitSource] {
        &self.missing
    }
}

impl DecisionService<'_> {
    /// Read and recheck actual public sources in one transaction, reporting missing dependencies.
    /// A completed inspection is still RejectedMissingSources, including when no auth is required.
    /// No arbitrary success provider or business idempotency mapping is accepted or created.
    pub async fn inspect_precommit_public_sources(
        &self,
        request: &PreCommitSecurityRequestV2,
        decision: &StoredDecisionId,
    ) -> Result<PreCommitPublicSourceReport, StorageError> {
        request
            .validate()
            .map_err(|_| StorageError::Invalid("PRE_COMMIT request contract"))?;
        bounded(request, 65536)?;
        let mut unit = self.open(&request.session_ref, &request.task_id).await?;
        let result = async {
            let sources = unit
                .load_confirmation_sources(decision, &request.confirmation_ref, None)
                .await?;
            unit.check_precommit_request(request, &sources.0).await?;
            unit.expected_precommit = Some(request.clone());
            Ok(sources)
        }
        .await;
        match result {
            Ok((decision, confirmation, expires_at)) => {
                if let Err(error) = unit.final_guard(false).await {
                    unit.rollback().await;
                    return Err(error);
                }
                let checked_at = unit.last_precise_time.ok_or(StorageError::CorruptRecord)?;
                unit.tx
                    .take()
                    .ok_or(StorageError::TransactionAborted)?
                    .commit()
                    .await?;
                let mut missing = vec![
                    MissingPreCommitSource::BusinessIdempotency,
                    MissingPreCommitSource::BusinessProjection,
                    MissingPreCommitSource::FinalApprovalRules,
                ];
                if matches!(
                    request.authentication,
                    PreCommitAuthenticationV2::Required { .. }
                ) {
                    missing.push(MissingPreCommitSource::FormalAuthentication);
                }
                Ok(PreCommitPublicSourceReport {
                    declaration: request.clone(),
                    sources: VerifiedPreflightConfirmation {
                        decision,
                        confirmation,
                        checked_at,
                        expires_at,
                    },
                    missing,
                })
            }
            Err(error) => {
                unit.rollback().await;
                Err(error)
            }
        }
    }
}

impl DecisionUnit<'_> {
    pub(super) async fn check_precommit_request(
        &mut self,
        request: &PreCommitSecurityRequestV2,
        decision: &StoredSecurityDecision,
    ) -> Result<(), StorageError> {
        request
            .validate_for_required_gates(&decision.decision.required_gates)
            .map_err(|_| StorageError::Invalid("PRE_COMMIT authentication gates"))?;
        bounded(request, 65536)?;
        for id in [
            request.idempotency_key.as_str(),
            request.confirmation_ref.as_str(),
        ] {
            valid_id(id)?;
        }
        if let PreCommitAuthenticationV2::Required { auth_grant_ref } = &request.authentication {
            valid_id(auth_grant_ref.as_str())?;
        }
        let (binding, _, arguments) = self.snapshot(&decision.binding.operation_id).await?;
        if request.session_ref != self.session
            || request.task_id != binding.task_id
            || request.node_id != binding.node_id
            || request.operation_id != binding.operation_id
            || u64::from(request.facts_version) != binding.facts_version
            || request.plan_version != binding.plan_version
            || request.tool_name != binding.tool_name
            || request.arguments_hash != binding.arguments_hash
            || request.normalized_arguments != arguments
            || hash(&request.normalized_arguments)? != binding.arguments_hash
            || self
                .expected_confirmation
                .as_ref()
                .map(|c| &c.record.confirmation_ref)
                != Some(&request.confirmation_ref)
        {
            return Err(StorageError::DecisionBindingMismatch);
        }
        self.session_guard()?;
        Ok(())
    }
}
