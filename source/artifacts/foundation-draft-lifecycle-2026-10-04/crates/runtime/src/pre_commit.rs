use crate::security::pre_commit_unavailable;
use crate::Runtime;
use safeflow_contracts::{PreCommitSecurityRequestV2, ReasonCode, SecurityDecision, SecurityError};

impl Runtime {
    /// Backend-only conditional request boundary. No trusted verifier is assembled yet.
    /// Does not adapt v2 to a v1 Policy context, execute a Tool, or issue a Permit.
    pub async fn evaluate_pre_commit_v2_for_backend(
        &self,
        request: PreCommitSecurityRequestV2,
    ) -> Result<SecurityDecision, SecurityError> {
        request.validate().map_err(|error| SecurityError {
            reason_code: ReasonCode::parse("INVALID_PRE_COMMIT_CONTRACT")
                .expect("static reason code"),
            message: error.to_string(),
        })?;
        self.sessions.resolve_binding(&request.session_ref)?;
        Err(pre_commit_unavailable())
    }
}
