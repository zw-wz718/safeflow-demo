//! Trusted same-capture source boundary. No HTTP/model DTO, reconstructed
//! source JSON, implicit initial source or public write grant is exposed.
use super::*;
use safeflow_subscription_evidence::{MockContext, Snapshot};
use std::sync::Mutex;

// The preview is read-only; only this object retains the exact original capture.
// No Clone/Serialize/Deserialize, public constructor or public source fields.
pub struct SubscriptionRegistrationCapture {
    pub(super) simulation: Option<crate::simulation_source::SimulationFence>,
    captured: crate::subscription_billing_reader::CollectedBillingEvidenceMock,
    preview: Value,
    display_identities: Vec<(String, Option<String>)>,
    issuer: uuid::Uuid,
    session: SessionRef,
    namespace: String,
    source: CapturedSource,
}
struct CapturedSource {
    simulation: Option<crate::simulation_source::SimulationFence>,
    identity: SessionBinding,
    scope: Value,
    version: i64,
    digest: ArgumentsHash,
    host_instance: String,
    host_generation: i64,
    reviewed_at: chrono::DateTime<chrono::Utc>,
    expires: chrono::DateTime<chrono::Utc>,
}
impl SubscriptionRegistrationCapture {
    /// Borrow the same preview, then explicitly choose an index from it. This
    /// safe view cannot reconstruct the source or authorize a write.
    pub fn preview(&self) -> &Value {
        &self.preview
    }
    pub(super) fn display_identities(&self) -> &[(String, Option<String>)] {
        &self.display_identities
    }
    #[cfg(test)]
    pub(super) fn expire_for_test(&mut self) {
        self.source.expires = chrono::Utc::now() - chrono::Duration::seconds(1);
    }
}

/// In-process capability: private fields, no Clone/Serialize/Deserialize. Dropping
/// an uncommitted handle (including process restart) loses that selection. Only
/// an already committed immutable preparation can subsequently resume by nonce.
pub struct SubscriptionRegistrationSelection {
    pub(super) simulation: Option<crate::simulation_source::SimulationFence>,
    opaque: Mutex<Option<SavedDiscoverySelectionMock>>,
    issuer: uuid::Uuid,
    session: SessionRef,
    identity: SessionBinding,
    namespace: String,
    host_instance: String,
    host_generation: i64,
    scope: Value,
    version: i64,
    journal_digest: ArgumentsHash,
    selected_at: chrono::DateTime<chrono::Utc>,
    pub(super) expires: chrono::DateTime<chrono::Utc>,
    pub(super) request: Value,
}

pub(super) async fn scope(c: &mut PgConnection) -> Result<Value, StorageError> {
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('database',current_database(),'server_address',inet_server_addr()::text,'server_port',inet_server_port(),'schema',current_schema(),'schema_oid',(SELECT oid::bigint FROM pg_namespace WHERE nspname=current_schema()))")
        .fetch_one(c).await?)
}
async fn source_state(
    c: &mut PgConnection,
    owner: &str,
) -> Result<(i64, ArgumentsHash), StorageError> {
    let row = sqlx::query(
        "SELECT storage_version,journal FROM subscription_runtime_records WHERE owner_id=$1",
    )
    .bind(owner)
    .fetch_optional(c)
    .await?
    .ok_or(StorageError::NotFound)?;
    Ok((
        row.try_get("storage_version")?,
        hash(&row.try_get::<Value, _>("journal")?)?,
    ))
}
impl SubscriptionRegistrationSelection {
    pub(super) fn check_origin(
        &self,
        service: &SubscriptionWriteService<'_>,
        session: &SessionRef,
        identity: &SessionBinding,
    ) -> Result<(), StorageError> {
        if self.issuer != service.selection_issuer
            || self.session != *session
            || self.identity != *identity
            || self.namespace != service.read.repository.namespace
        {
            return Err(StorageError::FactsConflict);
        }
        service.read.check(session, identity)
    }
    pub(super) async fn check_current(
        &self,
        service: &SubscriptionWriteService<'_>,
        c: &mut PgConnection,
        host: &SubscriptionHost,
        session: &SessionRef,
        identity: &SessionBinding,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StorageError> {
        self.check_origin(service, session, identity)?;
        if let Some(fence) = &self.simulation {
            fence.lock(c).await?;
        }
        if now < self.selected_at {
            return Err(StorageError::Invalid("subscription clock moved backwards"));
        }
        if now >= self.expires {
            return Err(StorageError::ConfirmationExpired);
        }
        if host.instance != self.host_instance
            || host.generation != self.host_generation
            || scope(c).await? != self.scope
            || source_state(c, identity.user_id.as_str()).await?
                != (self.version, self.journal_digest.clone())
        {
            return Err(StorageError::FactsConflict);
        }
        Ok(())
    }
    pub(super) fn take(&self) -> Result<SavedDiscoverySelectionMock, StorageError> {
        self.opaque
            .lock()
            .map_err(|_| StorageError::FactsConflict)?
            .take()
            .ok_or(StorageError::FactsConflict)
    }
}

impl SubscriptionWriteService<'_> {
    /// Native technical preparation only, requires this issuer's live SQL
    /// source-bound selection. Public four-write adoption is unchanged.
    pub async fn prepare_registration_technical_for_trusted_backend(
        &self,
        s: &SessionRef,
        host: &SubscriptionHost,
        nonce: &str,
        selection: &SubscriptionRegistrationSelection,
    ) -> Result<SubscriptionPreparation, StorageError> {
        if selection.simulation.is_none() {
            return Err(StorageError::DependencyUnavailable);
        }
        tokio::time::timeout(
            crate::simulation_source::DEADLINE,
            self.prepare_technical(s, host, nonce, Preparation::Registration(selection)),
        )
        .await
        .map_err(|_| StorageError::ReconcileRequired)?
    }
    /// Real async SQL -> original same-capture reader. The live SHARE lease is
    /// held until final account/session validation; no safe JSON is decoded.
    pub async fn capture_simulation_registration_for_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        source: &crate::PgSimulationSource,
        request: &safeflow_billing_source_reader::query::Request,
    ) -> Result<SubscriptionRegistrationCapture, StorageError> {
        tokio::time::timeout(crate::simulation_source::DEADLINE, async {
            let identity = self.read.identity(session)?;
            let sql = self
                .read
                .capture_simulation_for_backend(session, self.bank.as_ref(), source, request)
                .await?;
            let mut capture = self
                .capture_registration_for_trusted_backend(
                    session,
                    host,
                    request,
                    &sql.reader(),
                    safeflow_billing_source_reader::query::Limits::default(),
                )
                .await?;
            self.read
                .check_simulation_accounts(
                    session,
                    self.bank.as_ref(),
                    &identity,
                    &request.account_ids,
                )
                .await?;
            sql.check_live()?;
            capture.simulation = Some(sql.fence.clone());
            Ok(capture)
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)?
    }
    /// Trusted in-process consumer of the real collector. The caller supplies
    /// an explicit fictional source; no default provider or ledger is installed.
    pub async fn capture_registration_for_trusted_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        request: &safeflow_billing_source_reader::query::Request,
        source: &impl safeflow_billing_source_reader::query::PagedSource,
        limits: safeflow_billing_source_reader::query::Limits,
    ) -> Result<SubscriptionRegistrationCapture, StorageError> {
        let (identity, captured) = self
            .read
            .collect_billing_for_backend(session, self.bank.as_ref(), request, source, limits)
            .await?;
        self.finish_registration_capture(session, host, identity, captured)
            .await
    }
    /// Fixed team source bridge; old fictional-prefix entry remains unchanged.
    pub async fn capture_team_registration_for_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        source: &crate::TeamConsumptionSource,
    ) -> Result<SubscriptionRegistrationCapture, StorageError> {
        tokio::time::timeout(crate::simulation_source::DEADLINE, async {
            let request = source.request();
            let identity = self.read.identity(session)?;
            let pg = source.persistent_for_backend();
            let sql = self
                .read
                .capture_simulation_for_backend(session, self.bank.as_ref(), &pg, &request)
                .await?;
            let scope = safeflow_billing_source_reader::TrustedScope::from_verified_context(
                identity.user_id.as_str(),
                request.account_ids.iter().map(String::as_str),
            )
            .map_err(|_| StorageError::NotFound)?;
            let captured = crate::subscription_billing_reader::collect_from_adopted_team_capture(
                &scope,
                &request,
                &source.reader(&sql),
                safeflow_billing_source_reader::query::Limits::default(),
            )
            .map_err(|_| StorageError::DependencyUnavailable)?;
            let mut capture = self
                .finish_registration_capture(session, host, identity.clone(), captured)
                .await?;
            self.read
                .check_simulation_accounts(
                    session,
                    self.bank.as_ref(),
                    &identity,
                    &request.account_ids,
                )
                .await?;
            sql.check_live()?;
            capture.simulation = Some(sql.fence.clone());
            capture.preview["service_calendar_evidence"] = source.calendar_for_backend();
            Ok(capture)
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)?
    }
    async fn finish_registration_capture(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        identity: SessionBinding,
        captured: crate::subscription_billing_reader::CollectedBillingEvidenceMock,
    ) -> Result<SubscriptionRegistrationCapture, StorageError> {
        let mut tx = self.read.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        self.read.check(session, &identity)?;
        self.read
            .check_host(&mut tx, host, identity.user_id.as_str())
            .await?;
        let db_scope = scope(&mut tx).await?;
        let (version, digest) = source_state(&mut tx, identity.user_id.as_str()).await?;
        let reviewed_at = chrono::Utc::now();
        let expires = reviewed_at
            .checked_add_signed(self.config.ttl)
            .ok_or(StorageError::ConfirmationExpired)?;
        let preview = captured
            .review_on_connection_mock(&self.read.store()?, &mut tx)
            .await
            .map_err(|_| StorageError::DependencyUnavailable)?;
        let saved = self.read.load(&mut tx, identity.user_id.as_str()).await?;
        let display_identities = captured.display_identities(saved.ledger_mock())
            .map_err(|_| StorageError::DependencyUnavailable)?;
        self.read.check(session, &identity)?;
        tx.commit().await?;
        self.read.check(session, &identity)?;
        Ok(SubscriptionRegistrationCapture {
            simulation: None,
            captured,
            preview,
            display_identities,
            issuer: self.selection_issuer,
            session: session.clone(),
            namespace: self.read.repository.namespace.clone(),
            source: CapturedSource {
                simulation: None,
                identity,
                scope: db_scope,
                version,
                digest,
                host_instance: host.instance.clone(),
                host_generation: host.generation,
                reviewed_at,
                expires,
            },
        })
    }
    /// Consumes exactly the capture whose preview was borrowed. All current
    /// scope/Journal/host checks run in the actual selection transaction.
    pub async fn select_registration_from_capture_for_trusted_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        capture: SubscriptionRegistrationCapture,
        index: usize,
    ) -> Result<SubscriptionRegistrationSelection, StorageError> {
        if capture.simulation.is_some() {
            tokio::time::timeout(
                crate::simulation_source::DEADLINE,
                self.select_registration_capture_inner(session, host, capture, index),
            )
            .await
            .map_err(|_| StorageError::DependencyUnavailable)?
        } else {
            self.select_registration_capture_inner(session, host, capture, index)
                .await
        }
    }
    async fn select_registration_capture_inner(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        capture: SubscriptionRegistrationCapture,
        index: usize,
    ) -> Result<SubscriptionRegistrationSelection, StorageError> {
        let identity = self.read.identity(session)?;
        if capture.issuer != self.selection_issuer
            || capture.session != *session
            || capture.source.identity != identity
            || capture.namespace != self.read.repository.namespace
        {
            return Err(StorageError::FactsConflict);
        }
        let mut proof = capture.source;
        proof.simulation = capture.simulation;
        capture
            .captured
            .consume_registration_source_mock(index, |context, snapshot, index| async move {
                self.select_registration_checked(
                    session,
                    host,
                    &context,
                    &snapshot,
                    index,
                    Some(&proof),
                )
                .await
            })
            .await
    }
    /// Backend must supply original typed context/snapshot from a reviewed
    /// trusted source, never decode human/model safe output. This does not wire
    /// A1, initialize a ledger, create a draft or enable any public write.
    pub async fn select_registration_for_trusted_backend(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        context: &MockContext,
        input: &Snapshot,
        index: usize,
    ) -> Result<SubscriptionRegistrationSelection, StorageError> {
        self.select_registration_checked(session, host, context, input, index, None)
            .await
    }
    async fn select_registration_checked(
        &self,
        session: &SessionRef,
        host: &SubscriptionHost,
        context: &MockContext,
        input: &Snapshot,
        index: usize,
        capture: Option<&CapturedSource>,
    ) -> Result<SubscriptionRegistrationSelection, StorageError> {
        let identity = self.read.identity(session)?;
        if capture.is_some_and(|capture| capture.identity != identity) {
            return Err(StorageError::FactsConflict);
        }
        if context.user_id != identity.user_id.as_str() || context.requested_account_ids.is_empty()
        {
            return Err(StorageError::NotFound);
        }
        for account in &context.requested_account_ids {
            let id = ResourceId::parse(account).map_err(|_| StorageError::Invalid("account"))?;
            let balance = self.bank.get_account_balance(&identity.user_id, &id).await;
            self.read.check(session, &identity)?;
            balance
                .map_err(|_| StorageError::NotFound)?
                .validate_for(&id)
                .map_err(|_| StorageError::CorruptRecord)?;
        }
        let mut tx = self.read.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        self.read
            .check_host(&mut tx, host, identity.user_id.as_str())
            .await?;
        if let Some(fence) = capture.and_then(|c| c.simulation.as_ref()) {
            fence.lock(&mut tx).await?;
        }
        let db_scope = scope(&mut tx).await?;
        let (version, journal_digest) = source_state(&mut tx, identity.user_id.as_str()).await?;
        let selected_at = chrono::Utc::now();
        let mut expires = selected_at
            .checked_add_signed(self.config.ttl)
            .ok_or(StorageError::ConfirmationExpired)?;
        if let Some(capture) = capture {
            if selected_at < capture.reviewed_at {
                return Err(StorageError::Invalid("subscription clock moved backwards"));
            }
            if selected_at >= capture.expires {
                return Err(StorageError::ConfirmationExpired);
            }
            if capture.scope != db_scope
                || capture.version != version
                || capture.digest != journal_digest
                || capture.host_instance != host.instance
                || capture.host_generation != host.generation
            {
                return Err(StorageError::FactsConflict);
            }
            expires = expires.min(capture.expires);
        }
        let opaque = self
            .read
            .store()?
            .select_discovery_candidate_on_connection_mock(
                &mut tx,
                identity.user_id.as_str(),
                context,
                input,
                index,
                &selected_at.to_rfc3339(),
            )
            .await
            .map_err(write_error)?;
        if let Some(fence) = capture.and_then(|c| c.simulation.as_ref()) {
            self.read
                .check_simulation_accounts(session, self.bank.as_ref(), &identity, fence.accounts())
                .await?;
        }
        self.read.check(session, &identity)?;
        tx.commit().await?;
        let mut request = json!({"action":"REGISTRATION","source_contract":if capture.is_some() {"36cdc62-same-capture-v1"} else {"c163-trusted-selection-v1"},
            "scope":db_scope,"namespace":self.read.repository.namespace,"owner":identity.user_id,
            "host_instance":host.instance,"host_generation":host.generation,"storage_version":version,
            "journal_digest":journal_digest,"context":context,"input":input,"index":index,
            "selected_at":selected_at.to_rfc3339(),"selection_expires_at":expires.to_rfc3339()});
        if let Some(capture) = capture {
            request["capture_reviewed_at"] = json!(capture.reviewed_at.to_rfc3339());
            if let Some(fence) = &capture.simulation {
                request["simulation_source"] = fence.provenance();
            }
        }
        Ok(SubscriptionRegistrationSelection {
            simulation: capture.and_then(|c| c.simulation.clone()),
            opaque: Mutex::new(Some(opaque)),
            issuer: self.selection_issuer,
            session: session.clone(),
            identity,
            namespace: self.read.repository.namespace.clone(),
            host_instance: host.instance.clone(),
            host_generation: host.generation,
            scope: db_scope,
            version,
            journal_digest,
            selected_at,
            expires,
            request,
        })
    }
    /// A2 remains closed, including nonce-only recovery of a committed draft.
    pub async fn recover_preparation(
        &self,
        session: &SessionRef,
        nonce: &str,
    ) -> Result<SubscriptionPreparation, StorageError> {
        self.read.require_write_adoption(session)?;
        self.recover_technical(session, nonce).await
    }
    pub(super) async fn recover_technical(
        &self,
        session: &SessionRef,
        nonce: &str,
    ) -> Result<SubscriptionPreparation, StorageError> {
        let identity = self.read.identity(session)?;
        let parsed = uuid::Uuid::parse_str(nonce)
            .map_err(|_| StorageError::Invalid("preparation nonce v1"))?;
        if parsed.is_nil() || parsed.to_string() != nonce {
            return Err(StorageError::Invalid("preparation nonce v1"));
        }
        let expected:Value=sqlx::query_scalar("SELECT p.request_identity FROM subscription_preparations_v1 p JOIN subscription_public_bindings_v1 b USING(business_ref) WHERE nonce=$1 AND owner_id=$2")
            .bind(nonce).bind(identity.user_id.as_str()).fetch_optional(&self.store.repository.subscription_read_pool).await?.ok_or(StorageError::NotFound)?;
        if expected["contract"] != "subscription-preparation-v1"
            || expected["namespace"] != self.read.repository.namespace
            || expected["owner"] != identity.user_id.as_str()
            || expected["binding_version"] != json!(identity.binding_version)
            || expected["session_digest"]
                != json!(hash(&(
                    "safeflow-confirmation-session:v1",
                    session.as_str()
                ))?)
            || expected["member_source"] != "c163"
        {
            return Err(StorageError::FactsConflict);
        }
        self.read.check(session, &identity)?;
        self.resume_preparation(session, nonce, &expected).await
    }
}
