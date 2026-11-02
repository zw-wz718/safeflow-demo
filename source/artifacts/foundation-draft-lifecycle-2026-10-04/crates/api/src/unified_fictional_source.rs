//! Native-only adoption assembly. No request can select owner/source/provider.
use crate::{ApiConfig, ApiState};
use safeflow_contracts::{ResourceId, SessionRef, UserId};
use safeflow_runtime::SessionResolver;
use safeflow_storage::{
    CardCatalogService, CardCatalogSourceBinding, PgUnifiedFictionalSource, StorageError,
    UNIFIED_FICTIONAL_IMPLEMENTATION, UNIFIED_FICTIONAL_SOURCE,
};
use std::sync::Arc;

impl ApiState {
    pub async fn reopen_team_subscription_reads_for_backend(&mut self,session:&SessionRef)->Result<(),StorageError>{
        if Arc::strong_count(&self.0)!=1{return Err(StorageError::TransactionScope);}
        let team=self.0.team_source.as_ref().ok_or(StorageError::DependencyUnavailable)?;
        let host=self.0.subscription.current_host_for_backend(session).await?;
        let scope=self.0.subscription.open_team_public_scope_for_backend(session,&host,team,self.0.bank.as_ref()).await?;
        Arc::get_mut(&mut self.0).ok_or(StorageError::TransactionScope)?.subscription_public_scope=Some(scope);Ok(())
    }
    /// Source is already adopted in a dedicated new DB. Explicit native owner
    /// goes through the existing SimulationSessions; no fixture/client login.
    pub async fn for_unified_fictional_source(
        source: PgUnifiedFictionalSource,
        config: ApiConfig,
        owner: UserId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::unified_source_assembly(source, config, owner, vec![], None).await
    }
    pub async fn for_unified_team_consumption(
        source: PgUnifiedFictionalSource,
        config: ApiConfig,
        owner: UserId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let team = safeflow_storage::TeamConsumptionSource::open(&source, &owner).await?;
        let billing = crate::billing::team_billing_registration(Arc::new(source.clone()), &team)?;
        Self::unified_source_assembly(source, config, owner, vec![billing], Some(team)).await
    }
    async fn unified_source_assembly(
        source: PgUnifiedFictionalSource,
        config: ApiConfig,
        owner: UserId,
        tools: Vec<crate::ReadOnlyToolRegistration>,
        team: Option<safeflow_storage::TeamConsumptionSource>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        source.complete_accounts(&owner).await?;
        // New source adoption grants none of these unrelated capabilities.
        if config.payment_execution.is_some()
            || config.billing_feedback.is_some()
            || config.draft_confirmation.is_some()
            || config.card_read_source.is_some()
            || config.llm_gateway.is_some()
            || config.agent_fixed_plan_enabled
        {
            return Err("unified source assembly is read-only".into());
        }
        let repo = source.repository_for_isolated_backend();
        let mut config = config;
        config.card_read_source = Some(CardCatalogSourceBinding::for_reviewed_backend_provider(
            UNIFIED_FICTIONAL_SOURCE,
            UNIFIED_FICTIONAL_IMPLEMENTATION,
            Arc::new(source.clone()),
        )?);
        // Missing consumption is deliberately NOT a LocalBillingSource or empty ledger.
        let mut state = Self::for_project_simulation_with_owner_and_tools(
            repo,
            config,
            owner,
            Arc::new(source.clone()),
            tools,
        )?;
        let inner = Arc::get_mut(&mut state.0).ok_or("unified assembly already shared")?;
        inner.unified_source = Some(source);
        inner.team_source = team;
        Ok(state)
    }
    pub async fn initialize_team_initial_for_backend(
        &self,
        session: &SessionRef,
    ) -> Result<(), StorageError> {
        let team = self
            .0
            .team_source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        self.initialize_simulation_source_for_backend(session, &team.persistent_for_backend())
            .await
    }
    /// Explicit native startup only. The caller must first initialize the actual
    /// adopted empty Journal. Each server process establishes its own host generation.
    pub async fn enable_team_subscription_writes_for_backend(
        &mut self, session:&SessionRef, install:bool,
    ) -> Result<(),StorageError> {
        if Arc::strong_count(&self.0)!=1 {return Err(StorageError::TransactionScope);}
        let team=self.0.team_source.as_ref().ok_or(StorageError::DependencyUnavailable)?;
        if !self.lookup_team_initial_for_backend(session).await? {return Err(StorageError::DependencyUnavailable);}
        let host=self.0.subscription.startup_for_host(session).await?;
        let scope=if install {
            self.0.subscription.install_team_public_scope_for_backend(session,&host,team,self.0.bank.as_ref()).await?
        } else {
            self.0.subscription.open_team_public_scope_for_backend(session,&host,team,self.0.bank.as_ref()).await?
        };
        Arc::get_mut(&mut self.0).ok_or(StorageError::TransactionScope)?.subscription_public_scope=Some(scope);
        Ok(())
    }
    pub fn team_public_writes_for_backend(&self)->Result<safeflow_storage::SubscriptionPublicWrites<'_>,StorageError>{
        self.subscription_writes_for_source()?.for_team_public_scope(self.0.subscription_public_scope.as_ref().ok_or(StorageError::DependencyUnavailable)?)
    }
    pub async fn lookup_team_initial_for_backend(
        &self,
        session: &SessionRef,
    ) -> Result<bool, StorageError> {
        let team = self
            .0
            .team_source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        self.0
            .subscription
            .lookup_simulation_initial_for_backend(session, &team.persistent_for_backend())
            .await
    }
    /// Native explicit selection only. No registration business action is enabled.
    pub async fn prepare_team_subscription_for_backend(
        &self,
        session: &SessionRef,
        host: &safeflow_storage::SubscriptionHost,
        index: usize,
        nonce: &str,
    ) -> Result<safeflow_storage::SubscriptionPreparation, StorageError> {
        let team = self
            .0
            .team_source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let writes = self.subscription_writes_for_source()?;
        let capture = writes
            .capture_team_registration_for_backend(session, host, team)
            .await?;
        let selected = writes
            .select_registration_from_capture_for_trusted_backend(session, host, capture, index)
            .await?;
        writes
            .prepare_registration_technical_for_trusted_backend(session, host, nonce, &selected)
            .await
    }
    pub async fn team_subscription_preview_for_backend(
        &self,
        session: &SessionRef,
    ) -> Result<serde_json::Value, StorageError> {
        let team = self
            .0
            .team_source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        self.0
            .subscription
            .identify_team_for_backend(session, self.0.bank.as_ref(), team)
            .await
    }
    /// Reuses the same real session issuer and existing atomic member catalog
    /// initializer. This is a separate explicit backend action, not HTTP write.
    pub async fn initialize_unified_catalog_for_backend(
        &self,
        session: &SessionRef,
        account: &ResourceId,
    ) -> Result<String, StorageError> {
        let source = self
            .0
            .unified_source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let binding = CardCatalogSourceBinding::for_reviewed_backend_provider(
            UNIFIED_FICTIONAL_SOURCE,
            UNIFIED_FICTIONAL_IMPLEMENTATION,
            Arc::new(source.clone()),
        )?;
        CardCatalogService::for_trusted_backend(
            self.0.repository.clone(),
            self.0.sessions.clone(),
            Some(binding),
        )
        .initialize(session, account)
        .await
    }
    pub async fn unified_accounts_for_backend(
        &self,
        session: &SessionRef,
    ) -> Result<Vec<safeflow_bank_gateway::MaskedAccountView>, StorageError> {
        let identity = self
            .0
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        let source = self
            .0
            .unified_source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let result = source.complete_accounts(&identity.user_id).await?;
        if self
            .0
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?
            != identity
        {
            return Err(StorageError::Session);
        }
        Ok(result)
    }
    pub async fn unified_balance_for_backend(
        &self,
        session: &SessionRef,
        account: &ResourceId,
    ) -> Result<safeflow_bank_gateway::GetAccountBalanceOutput, StorageError> {
        let identity = self
            .0
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if self.0.unified_source.is_none() {
            return Err(StorageError::DependencyUnavailable);
        }
        let result = self
            .0
            .bank
            .get_account_balance(&identity.user_id, account)
            .await
            .map_err(|e| match e {
                safeflow_bank_gateway::BankError::ResourceNotOwned => StorageError::NotFound,
                safeflow_bank_gateway::BankError::Unavailable => {
                    StorageError::DependencyUnavailable
                }
                _ => StorageError::CorruptRecord,
            });
        if self
            .0
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?
            != identity
        {
            return Err(StorageError::Session);
        }
        result
    }
}
