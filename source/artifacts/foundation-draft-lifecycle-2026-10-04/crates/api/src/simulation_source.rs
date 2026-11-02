//! Native-only assembly over the existing ApiState, sessions, bank and Runtime.
use crate::ApiState;
use safeflow_billing_source_reader::query::Request;
use safeflow_contracts::SessionRef;
use safeflow_storage::*;
impl ApiState {
    pub async fn persistent_subscription_reminder_host_for_backend(&self,session:&SessionRef,config:SubscriptionReminderConfig)->Result<SubscriptionReminderHost,StorageError>{
        self.0.subscription.persistent_reminder_host_for_backend(session,self.0.subscription_public_scope.as_ref().ok_or(StorageError::DependencyUnavailable)?,self.0.bank.clone(),config).await
    }
    pub async fn persistent_subscription_reminder_view_for_backend(&self,session:&SessionRef,attempt:Option<&str>)->Result<serde_json::Value,StorageError>{
        self.0.subscription.persistent_reminder_view_for_backend(session,self.0.subscription_public_scope.as_ref().ok_or(StorageError::DependencyUnavailable)?,self.0.bank.as_ref(),attempt).await
    }
    /// Native/current-session only. No scheduler, tool or HTTP route is enabled.
    pub async fn subscription_reminder_host_for_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        config: SubscriptionReminderConfig,
    ) -> Result<SubscriptionReminderHost, StorageError> {
        self.0
            .subscription
            .reminder_host_for_backend(session, host, self.0.bank.clone(), config)
            .await
    }
    pub fn stop_native_simulation_session(&self, session: &SessionRef) -> Result<(), StorageError> {
        if self.0.card_safety_host.is_some(){return Err(StorageError::Invalid("use async stop_native_card_session with durable revocation"));}
        self.0
            .sessions
            .revoke(session)
            .map_err(|_| StorageError::Session)
    }
    pub async fn initialize_simulation_source_for_backend(
        &self,
        session: &SessionRef,
        source: &PgSimulationSource,
    ) -> Result<(), StorageError> {
        self.0
            .subscription
            .initialize_simulation_for_host(session, self.0.bank.as_ref(), source)
            .await
    }
    pub async fn capture_simulation_subscription_for_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        source: &PgSimulationSource,
        request: &Request,
    ) -> Result<SubscriptionRegistrationCapture, StorageError> {
        self.subscription_writes_for_source()?
            .capture_simulation_registration_for_backend(session, host, source, request)
            .await
    }
    pub async fn prepare_simulation_subscription_for_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        source: &PgSimulationSource,
        request: &Request,
        index: usize,
        nonce: &str,
    ) -> Result<SubscriptionPreparation, StorageError> {
        let writes = self.subscription_writes_for_source()?;
        let capture = writes
            .capture_simulation_registration_for_backend(session, host, source, request)
            .await?;
        let selected = writes
            .select_registration_from_capture_for_trusted_backend(session, host, capture, index)
            .await?;
        writes
            .prepare_registration_technical_for_trusted_backend(session, host, nonce, &selected)
            .await
    }
    pub(super) fn subscription_writes_for_source(
        &self,
    ) -> Result<SubscriptionWriteService<'_>, StorageError> {
        SubscriptionWriteService::for_trusted_backend(
            self.0.repository.clone(),
            self.0.sessions.clone(),
            self.0.bank.clone(),
            self.0.runtime.runtime(),
            ConfirmationConfig::new(std::time::Duration::from_secs(60))?,
            PermitConfig::new(std::time::Duration::from_secs(30))?,
        )
    }
}
