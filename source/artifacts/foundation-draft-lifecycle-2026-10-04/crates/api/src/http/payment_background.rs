//! New opt-in local fiction contract. No producer decisions on customer HTTP.
use super::*;
use safeflow_contracts::{ConfirmationRef, OperationId};
use safeflow_storage::{
    PaymentBackgroundCode, PaymentBackgroundDisplay, PaymentBackgroundHost, PaymentOriginalReader,
};
use serde_json::json;

impl ApiState {
    pub async fn stop_native_background_session(&self, s: &SessionRef) -> Result<(), StorageError> {
        self.0.sessions.revoke(s).map_err(|_| StorageError::Session)
    }
    pub async fn for_unified_payment_background(
        source: PgUnifiedFictionalSource,
        config: ApiConfig,
        owner: UserId,
        controller: Zeroizing<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::bank_payment_assembly(source,config,owner,controller,vec![],None,None).await
    }
    pub(crate) async fn bank_payment_assembly(
        source:PgUnifiedFictionalSource,mut config:ApiConfig,owner:UserId,controller:Zeroizing<String>,
        tools:Vec<crate::ReadOnlyToolRegistration>,team:Option<TeamConsumptionSource>,feedback:Option<BillingFeedbackConfig>,
    )->Result<Self,Box<dyn std::error::Error>>{
        if controller.as_str() == config.login_secret.as_str()
            || config.payment_execution.is_some()
            || config.llm_gateway.is_some()
            || config.billing_feedback.is_some()
        {
            return Err("isolated explicit payment assembly required".into());
        }
        source
            .repository_for_isolated_backend()
            .verify_payment_background_api_login()
            .await?;
        source.complete_accounts(&owner).await?;
        let host = PaymentBackgroundHost::for_isolated_native(source.clone(), controller)?;
        let catalog = CardCatalogSourceBinding::for_reviewed_backend_provider(
            safeflow_storage::UNIFIED_FICTIONAL_SOURCE,
            safeflow_storage::UNIFIED_FICTIONAL_IMPLEMENTATION,
            Arc::new(source.clone()),
        )?;
        config.card_read_source = Some(catalog.clone());
        if config.draft_confirmation.is_none() {
            config.draft_confirmation = Some(
                DraftConfirmationConfig::new(
                    Duration::from_secs(300),
                    Duration::from_secs(300),
                    600,
                    "isolated-background-payment-v1",
                )?
                .with_lifecycle_v2()?,
            );
        }
        config.payment_execution = Some(PaymentExecutionConfig::for_reviewed_project_simulation(
            Some(catalog),
            safeflow_runtime::ToolSecurityProfile {
                tool_name: safeflow_contracts::ToolName::parse("execute_confirmed_payment")?,
                tool_schema_version: 1,
                policy_version: safeflow_contracts::PolicyVersion::parse(
                    "transfer-funds-business-2026-10-18-v1",
                )?,
                minimum_level: safeflow_contracts::SecurityLevel::Red,
            },
            PermitConfig::new(Duration::from_secs(60))?,
            safeflow_transfer_integration::mock_execution::MockBehavior::Pending,
        )?);
        config.billing_feedback=feedback;
        let mut state = Self::for_project_simulation_with_owner_and_tools(
            source.repository_for_isolated_backend(),
            config,
            owner,
            Arc::new(source.clone()),
            tools,
        )?;
        let inner = Arc::get_mut(&mut state.0).ok_or("assembly shared")?;
        inner.unified_source = Some(source);
        inner.team_source=team;
        inner.payment_background_host = Some(host);
        Ok(state)
    }
    pub fn payment_background_for_backend(
        &self,
    ) -> Result<PaymentExecutionService<'_>, StorageError> {
        let config = self
            .0
            .payment_execution
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let runtime = self
            .0
            .draft
            .as_ref()
            .and_then(|d| d.lifecycle_runtime.as_ref())
            .ok_or(StorageError::DependencyUnavailable)?;
        PaymentExecutionService::for_project_simulation(
            self.0.repository.clone(),
            self.0.sessions.clone(),
            runtime.runtime(),
            config.source.clone(),
            config.profile.clone(),
            config.permit.clone(),
            config.behavior,
        )?
        .with_background_host(
            self.0
                .payment_background_host
                .clone()
                .ok_or(StorageError::DependencyUnavailable)?,
        )
    }
    pub async fn payment_background_challenge_for_native_controller(
        &self,
        secret: &str,
        s: &SessionRef,
        task: &TaskId,
        p: &OperationId,
        consent: &str,
    ) -> Result<Option<PaymentBackgroundCode>, StorageError> {
        self.payment_background_for_backend()?
            .background_challenge_for_native_controller(secret, s, task, p, consent)
            .await
    }
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum Action {
    Display {},
    Confirm {
        shown: PaymentBackgroundDisplay,
    },
    VerifyCode {
        consent_id: String,
        code: String,
    },
    Execute {
        consent_id: String,
        confirmation_ref: ConfirmationRef,
        display_ticket: String,
    },
    Revoke {
        execution_operation_id: OperationId,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    contract_version: u16,
    scope: String,
    action: Action,
}
pub(super) async fn action(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    if request.uri().query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let Path((task, p)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let task = parse_task(&task)?;
    let p = payment_preparation::operation(&p)?;
    let body: Command = payment_preparation::input(request).await?;
    if body.contract_version != 1 || body.scope != "EXPLICIT_PAYMENT_BACKGROUND_V1" {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    let service = state
        .payment_background_for_backend()
        .map_err(payment_preparation::error)?;
    let result=tokio::time::timeout(state.0.request_timeout,async{
        match body.action {
            Action::Display{}=>serde_json::to_value(service.display_background_delegation(&identity.reference,&task,&p).await?).map_err(|_|StorageError::CorruptRecord),
            Action::Confirm{shown}=>{service.confirm_background_delegation(&identity.reference,&task,&p,&shown).await?;Ok(json!({"delegation_confirmed":true,"business_executed":false}))},
            Action::VerifyCode{consent_id,code}=>{let valid=service.verify_background_code(&identity.reference,&task,&p,&consent_id,Zeroizing::new(code)).await?;Ok(json!({"verified":valid,"mode":"SIMULATED_PAYMENT_AUTH","business_executed":false}))},
            Action::Execute{consent_id,confirmation_ref,display_ticket}=>serde_json::to_value(service.execute_with_background_delegation(&identity.reference,&task,&p,&confirmation_ref,&display_ticket,&consent_id).await?).map_err(|_|StorageError::CorruptRecord),
            Action::Revoke{execution_operation_id}=>{
                // Selectors must match this owner's original task/preparation.
                let original=PaymentOriginalReader::for_trusted_backend(state.0.repository.clone(),state.0.sessions.clone()).query(&identity.reference,&task,&p).await?;
                if original.binding.execution_operation_id.as_ref()!=Some(&execution_operation_id){return Err(StorageError::NotFound);}
                state.0.repository.revoke_payment_background(state.0.sessions.clone(),&identity.reference,&execution_operation_id).await?;
                Ok(json!({"delegation_revoked":true,"funds_cancelled":false}))
            },
        }
    }).await.map_err(|_|ApiError::unavailable().with_task(&task))?;
    identity.check(&state)?;
    result.map(Json).map_err(|e| background_error(e).with_task(&task))
}

// Distinguish an explicit authentication refusal from an unavailable service.
// FormalAuthUnavailable intentionally covers both missing and expired proof;
// do not infer which one occurred or weaken the underlying storage guard.
fn background_error(error: StorageError) -> ApiError {
    let reason = match error {
        StorageError::FormalAuthUnavailable => "PAYMENT_AUTHENTICATION_REQUIRED",
        StorageError::AuthChallengeInactive => "PAYMENT_CODE_INACTIVE",
        StorageError::AuthExpired => "PAYMENT_AUTHENTICATION_EXPIRED",
        StorageError::ConfirmationRequired | StorageError::ConfirmationExpired
        | StorageError::ConfirmationRevoked => "PAYMENT_CONFIRMATION_INVALID",
        StorageError::PermitExpired | StorageError::PermitNotActive => "PAYMENT_PERMIT_INACTIVE",
        other => return payment_preparation::error(other),
    };
    ApiError { status: StatusCode::CONFLICT, reason, task_id: None }
}
