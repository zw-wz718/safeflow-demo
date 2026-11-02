//! Explicit self-card actions. Challenge delivery remains native-only.
use super::*;
use safeflow_contracts::{ConfirmationRef, OperationId, ResourceId};

impl ApiState {
    pub async fn for_unified_card_safety(
        source: PgUnifiedFictionalSource,
        config: ApiConfig,
        owner: UserId,
        controller_secret: Zeroizing<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if controller_secret.as_str() == config.login_secret.as_str() {
            return Err("controller and login credentials must differ".into());
        }
        let host = CardSafetyHost::for_isolated_native_host(controller_secret)?;
        let mut state = Self::for_unified_fictional_source(source, config, owner).await?;
        Arc::get_mut(&mut state.0)
            .ok_or("already shared")?
            .card_safety_host = Some(host);
        Ok(state)
    }
    pub fn card_safety_for_backend(&self) -> Result<CardSafetyService<'_>, StorageError> {
        CardSafetyService::for_isolated_backend(
            self.0
                .unified_source
                .clone()
                .ok_or(StorageError::DependencyUnavailable)?,
            self.0.sessions.clone(),
            self.0.runtime.runtime(),
            self.0
                .card_safety_host
                .clone()
                .ok_or(StorageError::DependencyUnavailable)?,
        )
    }
    /// Trusted native caller supplies controller credential separately from the
    /// cookie. Neither HTTP nor loopback/header values can call this capability.
    pub async fn card_challenge_for_native_controller(
        &self,
        controller_secret: &str,
        session: &SessionRef,
        operation: &OperationId,
    ) -> Result<CardChallengeDelivery, StorageError> {
        self.card_safety_for_backend()?
            .challenge_for_native_controller(controller_secret, session, operation)
            .await
    }
    pub async fn stop_native_card_session(&self, session: &SessionRef) -> Result<(), StorageError> {
        self.card_safety_for_backend()?
            .revoke_session(session)
            .await?;
        self.0
            .sessions
            .revoke(session)
            .map_err(|_| StorageError::Session)
    }
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum Action {
    Prepare {
        account_id: ResourceId,
        card_id: String,
        nonce: String,
    },
    Confirm {
        operation_id: OperationId,
        confirmation_ref: ConfirmationRef,
    },
    VerifyCode {
        operation_id: OperationId,
        challenge_id: String,
        code: String,
    },
    IssuePermit {
        operation_id: OperationId,
    },
    CheckPermit {
        operation_id: OperationId,
    },
    QueryNonce { nonce: String },
    Query {
        operation_id: OperationId,
    },
    QueryOriginal { operation_id: OperationId },
    ExecuteFreeze { operation_id: OperationId },
    RetryRolledBackFreeze { operation_id: OperationId },
    Cancel {
        operation_id: OperationId,
    },
}
pub(super) async fn action(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let action: Action = body(request).await?;
    identity.check(&state)?;
    if matches!(&action,Action::ExecuteFreeze{..}|Action::RetryRolledBackFreeze{..}) {
        let backend=state.clone();let session=identity.reference.clone();
        // Dropping the HTTP waiter must never cancel a submitted COMMIT. The
        // task retains the backend and still uses durable fences/final guards.
        // Timeout or disconnect is UNKNOWN to the client; query the original O.
        let execution=tokio::spawn(async move {
            let service=backend.card_safety_for_backend().map_err(ApiError::storage)?;
            let out=match action {
                Action::ExecuteFreeze{operation_id}=>service.execute_freeze(&session,&operation_id).await,
                Action::RetryRolledBackFreeze{operation_id}=>service.retry_rolled_back_freeze(&session,&operation_id).await,
                _=>unreachable!(),
            }.map_err(ApiError::storage)?;
            serde_json::to_value(out).map_err(|_|ApiError::unavailable())
        });
        let result=tokio::time::timeout(state.0.request_timeout,execution).await.map_err(|_|ApiError::unavailable())?.map_err(|_|ApiError::unavailable())??;
        identity.check(&state)?;return Ok(Json(result));
    }
    let result=tokio::time::timeout(state.0.request_timeout,async{
        let service=state.card_safety_for_backend().map_err(ApiError::storage)?;
        let s=&identity.reference;
        let value=match action{
            Action::Prepare{account_id,card_id,nonce}=>serde_json::to_value(service.prepare(s,&account_id,&card_id,&nonce).await.map_err(ApiError::storage)?),
            Action::Confirm{operation_id,confirmation_ref}=>serde_json::to_value(service.confirm(s,&operation_id,&confirmation_ref).await.map_err(ApiError::storage)?),
            Action::QueryNonce{nonce}=>serde_json::to_value(service.query_nonce(s,&nonce).await.map_err(ApiError::storage)?),
            Action::Query{operation_id}=>serde_json::to_value(service.query(s,&operation_id).await.map_err(ApiError::storage)?),
            Action::QueryOriginal{operation_id}=>serde_json::to_value(service.query_freeze_original(s,&operation_id).await.map_err(ApiError::storage)?),
            Action::ExecuteFreeze{..}|Action::RetryRolledBackFreeze{..}=>unreachable!(),
            Action::VerifyCode{operation_id,challenge_id,code}=>{
                let verified=service.verify_code(s,&operation_id,&challenge_id,Zeroizing::new(code)).await.map_err(ApiError::storage)?;
                Ok(serde_json::json!({"verified":verified,"mode":"ISOLATED_SIMULATION","business_executed":false}))
            },
            Action::IssuePermit{operation_id}=>{service.issue_permit(s,&operation_id).await.map_err(ApiError::storage)?;Ok(serde_json::json!({"permit_issued":true,"mode":"ISOLATED_SIMULATION","business_executed":false}))},
            Action::CheckPermit{operation_id}=>{service.check_permit(s,&operation_id).await.map_err(ApiError::storage)?;Ok(serde_json::json!({"permit_active":true,"business_executed":false}))},
            Action::Cancel{operation_id}=>{service.cancel(s,&operation_id).await.map_err(ApiError::storage)?;Ok(serde_json::json!({"cancelled":true,"business_executed":false}))},
        };
        value.map_err(|_|ApiError::unavailable())
    }).await.map_err(|_|ApiError::unavailable())??;
    identity.check(&state)?;
    Ok(Json(result))
}
