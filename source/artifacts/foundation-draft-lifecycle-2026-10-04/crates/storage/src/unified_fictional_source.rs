//! Native-only immutable v1 source over adopted funds and real SQL reads.
//! No uploader, identity authority, background worker or payment permission.
use crate::{
    unified_fictional_document::*, BackendCardCatalog, BackendCardCatalogProvider,
    BackendCatalogCard, PgRepository, StorageError,
};
use safeflow_bank_gateway::{
    BankError, BankGateway, FindPayeesInput, FindPayeesOutput, GetAccountBalanceOutput,
    MaskedAccountView,
};
use safeflow_contracts::{ContractFuture, ResourceId, UserId};
use safeflow_runtime::SessionBinding;
use safeflow_transfer_integration::funds_store::FundsStore;
use sqlx::{PgConnection, Row};
use std::sync::Arc;

#[derive(Clone)]
pub struct PgUnifiedFictionalSource {
    repository: PgRepository,
    document: Arc<UnifiedFictionalDocument>,
    recipient_profile: Option<crate::ConfirmationRecipient>,
}
impl PgUnifiedFictionalSource {
    /// Preflight BEFORE schema creation or migration. Existing databases with
    /// any application relations or this namespace are refused without writes.
    pub async fn provision_new_database(
        url: &str,
        namespace: &str,
        document: UnifiedFictionalDocument,
    ) -> Result<Self, StorageError> {
        if !namespace.starts_with("sf_unified_")
            || namespace.len() > 63
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(StorageError::Invalid("unified isolated namespace"));
        }
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await?;
        let result=async {
            let mut lock=admin.acquire().await?;
            sqlx::query("SELECT pg_advisory_lock(741260032)").execute(&mut *lock).await?;
            let occupied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','v','m','f') AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_') OR EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
                .bind(namespace).fetch_one(&mut *lock).await?;
            if occupied {return Err(StorageError::Invalid("new empty database required before migration"));}
            // namespace alphabet validated above. CREATE without IF NOT EXISTS.
            sqlx::query(&format!("CREATE SCHEMA \"{namespace}\"")).execute(&mut *lock).await?;
            let repo=PgRepository::connect_existing(url,namespace).await?;
            let provision=async {repo.migrate().await?;Self::initialize_new_isolated(repo.clone(),document).await}.await;
            if provision.is_err(){repo.close().await;}
            sqlx::query("SELECT pg_advisory_unlock(741260032)").execute(&mut *lock).await?;
            provision
        }.await;
        // Physically close the only preflight connection, releasing its session
        // lock on every error as well; never return a locked connection to pool.
        admin.close().await;
        result
    }
    /// Fails on ANY existing application rows, not only known account IDs.
    /// Caller must provision a dedicated NEW database and apply public migrations.
    /// On an uncertain commit, use open/inspect; never retry by clearing state.
    pub async fn initialize_new_isolated(
        repository: PgRepository,
        document: UnifiedFictionalDocument,
    ) -> Result<Self, StorageError> {
        let mut tx = repository.pool.begin().await?;
        // Reject other application schemas/tables in this dedicated database.
        let foreign:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','v','m','f') AND n.nspname <> $1 AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_')")
            .bind(&repository.namespace).fetch_one(&mut *tx).await?;
        if foreign {
            return Err(StorageError::Invalid("dedicated new database required"));
        }
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname=$1 ORDER BY tablename",
        )
        .bind(&repository.namespace)
        .fetch_all(&mut *tx)
        .await?;
        if tables.is_empty() {
            return Err(StorageError::Migration);
        }
        let quoted = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let names = tables
            .iter()
            .map(|t| format!("{}.{}", quoted(&repository.namespace), quoted(t)))
            .collect::<Vec<_>>();
        // Also serializes concurrent bootstraps; no ON CONFLICT reset semantics.
        sqlx::query(&format!(
            "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
            names.join(",")
        ))
        .execute(&mut *tx)
        .await?;
        for (t, name) in tables.iter().zip(&names) {
            if t == "_sqlx_migrations" {
                continue;
            }
            let occupied: bool =
                sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM {name})"))
                    .fetch_one(&mut *tx)
                    .await?;
            if occupied {
                return Err(StorageError::Invalid("existing database must not be reset"));
            }
        }
        let plan = document.plan()?;
        // Existing member transaction interface; no member source/SQL changes.
        for a in plan.accounts() {
            FundsStore::provision_account(
                &mut tx,
                &a.owner_id,
                &a.account.account_id,
                &a.opening_balance,
            )
            .await
            .map_err(crate::transfer_sql::business_error)?;
        }
        let r = plan.route();
        FundsStore::provision_payee(&mut tx, &r.owner_id, &r.payee_id, &r.recipient_account_id)
            .await
            .map_err(crate::transfer_sql::business_error)?;
        sqlx::query("INSERT INTO public_unified_fictional_source_v1(singleton,source_id,source_revision,artifact_commit,artifact_sha256,implementation_revision,original_bytes,active) VALUES(1,$1,$2,$3,$4,$5,$6,true)")
            .bind(UNIFIED_FICTIONAL_SOURCE).bind(UNIFIED_FICTIONAL_REVISION).bind(UNIFIED_FICTIONAL_COMMIT)
            .bind(UNIFIED_FICTIONAL_SHA256).bind(UNIFIED_FICTIONAL_IMPLEMENTATION).bind(&document.raw)
            .execute(&mut *tx).await?;
        let source = Self {
            repository: repository.clone(),
            document: Arc::new(document),
            recipient_profile: None,
        };
        source.check(&mut tx).await?;
        tx.commit().await?;
        Ok(source)
    }
    /// Exact source and revision are mandatory trusted configuration.
    pub async fn open(
        repository: PgRepository,
        source_id: &str,
        revision: &str,
    ) -> Result<Self, StorageError> {
        if source_id != UNIFIED_FICTIONAL_SOURCE || revision != UNIFIED_FICTIONAL_REVISION {
            return Err(StorageError::Invalid("unified source/revision mismatch"));
        }
        let bytes: Vec<u8> = sqlx::query_scalar(
            "SELECT original_bytes FROM public_unified_fictional_source_v1 WHERE singleton=1",
        )
        .fetch_optional(&repository.pool)
        .await?
        .ok_or(StorageError::DependencyUnavailable)?;
        let document = UnifiedFictionalDocument::read_adopted_v1(&bytes)?;
        let mut profile_tx = repository.simulation_source_pool.begin().await?;
        let recipient_profile = PgRepository::recipient_profile_on(&mut profile_tx, &document.data.transfer_plan.route.owner_id, &document.data.transfer_plan.payee.payee_id).await?;
        profile_tx.commit().await?;
        let source = Self {
            repository,
            document: Arc::new(document),
            recipient_profile,
        };
        let mut tx = source.repository.simulation_source_pool.begin().await?;
        source.check(&mut tx).await?;
        tx.commit().await?;
        Ok(source)
    }
    pub(crate) async fn check(&self, c: &mut PgConnection) -> Result<(), StorageError> {
        self.check_inner(c,true).await
    }
    pub(crate) async fn check_readonly(&self,c:&mut PgConnection)->Result<(),StorageError>{
        self.check_inner(c,false).await
    }
    async fn check_inner(&self,c:&mut PgConnection,lock:bool)->Result<(),StorageError>{
        let r = sqlx::query(if lock {
            "SELECT * FROM public_unified_fictional_source_v1 WHERE singleton=1 FOR SHARE"
        } else {"SELECT * FROM public_unified_fictional_source_v1 WHERE singleton=1"})
        .fetch_optional(&mut *c)
        .await?
        .ok_or(StorageError::DependencyUnavailable)?;
        self.check_source_row(&r)?;
        // A host pins the adopted display version at open. Maintenance cannot
        // silently change in-flight business reads; reopen the host afterwards.
        if PgRepository::recipient_profile_on(c, &self.document.data.transfer_plan.route.owner_id, &self.document.data.transfer_plan.payee.payee_id).await? != self.recipient_profile {
            return Err(StorageError::FactsConflict);
        }

        // Exact adopted membership: do not turn unknown accounts into an empty scope.
        let rows=sqlx::query("SELECT account_id,owner_id,opening_cents FROM transfer_accounts_v1 ORDER BY account_id")
            .fetch_all(&mut *c).await?;
        if rows.len() != 3 {
            return Err(StorageError::FactsConflict);
        }
        for row in rows {
            let id: String = row.try_get("account_id")?;
            let a = self
                .document
                .data
                .transfer_plan
                .accounts
                .iter()
                .find(|a| a.account.account_id.as_str() == id)
                .ok_or(StorageError::FactsConflict)?;
            if row.try_get::<String, _>("owner_id")? != a.owner_id.as_str()
                || row.try_get::<i64, _>("opening_cents")? != opening_cents(&a.opening_balance)?
            {
                return Err(StorageError::FactsConflict);
            }
        }
        let routes = sqlx::query(
            "SELECT owner_id,payee_id,recipient_account_id FROM transfer_payee_routes_v1",
        )
        .fetch_all(&mut *c)
        .await?;
        let expected = &self.document.data.transfer_plan.route;
        if routes.len() != 1
            || routes[0].try_get::<String, _>("owner_id")? != expected.owner_id.as_str()
            || routes[0].try_get::<String, _>("payee_id")? != expected.payee_id.as_str()
            || routes[0].try_get::<String, _>("recipient_account_id")?
                != expected.recipient_account_id.as_str()
        {
            return Err(StorageError::FactsConflict);
        }
        Ok(())
    }
    // Pure validation of one already-fetched statement snapshot. Callers must
    // fetch ALL read-authority projections in that same SQL statement.
    fn check_source_row(&self, r: &sqlx::postgres::PgRow) -> Result<(), StorageError> {
        if !r.try_get::<bool, _>("active")?
            || r.try_get::<String, _>("source_id")? != UNIFIED_FICTIONAL_SOURCE
            || r.try_get::<String, _>("source_revision")? != UNIFIED_FICTIONAL_REVISION
            || r.try_get::<String, _>("artifact_commit")? != UNIFIED_FICTIONAL_COMMIT
            || r.try_get::<String, _>("artifact_sha256")? != UNIFIED_FICTIONAL_SHA256
            || r.try_get::<String, _>("implementation_revision")?
                != UNIFIED_FICTIONAL_IMPLEMENTATION
            || r.try_get::<Vec<u8>, _>("original_bytes")? != self.document.raw
        {
            return Err(StorageError::DependencyUnavailable);
        }
        Ok(())
    }
    pub(crate) fn check_card_read_snapshot(
        &self,
        r: &sqlx::postgres::PgRow,
    ) -> Result<(), StorageError> {
        self.check_source_row(r)?;
        let actual = r.try_get::<sqlx::types::Json<Vec<(String, String, i64)>>, _>("read_accounts")?.0;
        let mut expected = self.document.data.transfer_plan.accounts.iter().map(|a| {
            Ok((a.account.account_id.as_str().to_owned(), a.owner_id.as_str().to_owned(), opening_cents(&a.opening_balance)?))
        }).collect::<Result<Vec<_>, StorageError>>()?;
        expected.sort_by(|a,b| a.0.cmp(&b.0));
        let routes = r.try_get::<sqlx::types::Json<Vec<(String, String, String)>>, _>("read_routes")?.0;
        let route = &self.document.data.transfer_plan.route;
        if actual != expected || routes != vec![(route.owner_id.as_str().to_owned(), route.payee_id.as_str().to_owned(), route.recipient_account_id.as_str().to_owned())] {
            return Err(StorageError::FactsConflict);
        }
        Ok(())
    }
    /// Holds the exact adopted source SHARE lease and current account owner
    /// row through the caller transaction. No client-selected scope/Allowed.
    pub(crate) async fn protect_card_scope(&self, c: &mut PgConnection, owner: &UserId, account: &ResourceId) -> Result<(), StorageError> {
        if !self.document.complete_accounts(owner)?.contains(account) { return Err(StorageError::NotFound); }
        // Account protection is shared with the member card backend and funds.
        let actual:String=sqlx::query_scalar("SELECT owner_id FROM transfer_accounts_v1 WHERE account_id=$1 FOR UPDATE")
            .bind(account.as_str()).fetch_optional(&mut *c).await?.ok_or(StorageError::NotFound)?;
        if actual!=owner.as_str() { return Err(StorageError::NotFound); }
        self.check(c).await
    }
    pub async fn complete_accounts(
        &self,
        owner: &UserId,
    ) -> Result<Vec<MaskedAccountView>, StorageError> {
        let ids = self.document.complete_accounts(owner)?;
        let mut tx = self.repository.simulation_source_pool.begin().await?;
        self.check(&mut tx).await?;
        let result = self
            .document
            .data
            .transfer_plan
            .accounts
            .iter()
            .filter(|a| ids.contains(&a.account.account_id))
            .map(|a| a.account.clone())
            .collect();
        tx.commit().await?;
        Ok(result)
    }
    /// One-way technical withdrawal. No reactivation, replacement, deletion or ABA.
    /// It is not a general directory-maintenance contract or real bank revocation.
    pub async fn deactivate_for_isolated_backend(&self) -> Result<(), StorageError> {
        sqlx::query("UPDATE public_unified_fictional_source_v1 SET active=false WHERE singleton=1 AND active")
            .execute(&self.repository.pool).await?;
        Ok(())
    }
    pub fn consumption_source(&self) -> Result<(), StorageError> {
        self.document.consumption_source()
    }
    pub fn initial_subscription_ledger(&self) -> Result<(), StorageError> {
        self.document.initial_subscription_ledger()
    }
    pub fn repository_for_isolated_backend(&self) -> PgRepository {
        self.repository.clone()
    }
}
impl BankGateway for PgUnifiedFictionalSource {
    fn get_account_balance<'a>(
        &'a self,
        owner: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            let a = self
                .document
                .data
                .transfer_plan
                .accounts
                .iter()
                .find(|a| a.owner_id == *owner && a.account.account_id == *account)
                .ok_or(BankError::ResourceNotOwned)?;
            let mut tx = self
                .repository
                .simulation_source_pool
                .begin()
                .await
                .map_err(|_| BankError::Unavailable)?;
            self.check(&mut tx)
                .await
                .map_err(|_| BankError::Unavailable)?;
            let funds = FundsStore::funds(&mut tx, owner, account)
                .await
                .map_err(|_| BankError::InvalidResponse)?;
            let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *tx)
                .await
                .map_err(|_| BankError::Unavailable)?;
            let result = GetAccountBalanceOutput {
                account: a.account.clone(),
                available_balance: funds.available,
                as_of: now.to_rfc3339(),
            };
            result.validate_for(account)?;
            tx.commit().await.map_err(|_| BankError::Unavailable)?;
            Ok(result)
        })
    }
    fn find_payees<'a>(
        &'a self,
        owner: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async move {
            FindPayeesInput {
                query: query.into(),
            }
            .validate()?;
            self.document
                .complete_accounts(owner)
                .map_err(|_| BankError::ResourceNotOwned)?;
            let mut tx = self
                .repository
                .simulation_source_pool
                .begin()
                .await
                .map_err(|_| BankError::Unavailable)?;
            self.check(&mut tx)
                .await
                .map_err(|_| BankError::Unavailable)?;
            let plan = &self.document.data.transfer_plan;
            let profile = &self.recipient_profile;
            let mut payee = plan.payee.clone();
            if let Some(profile) = profile {payee.display_name = profile.display_name.clone();}
            // The original alias remains a lookup for restoring immutable old drafts.
            // Directory output always uses the adopted name and the existing mask.
            let candidates = if plan.route.owner_id == *owner
                && (payee.display_name.contains(query) || plan.payee.display_name.contains(query)) {
                vec![payee]
            } else {vec![]};
            let result = FindPayeesOutput { candidates };
            result.validate()?;
            tx.commit().await.map_err(|_| BankError::Unavailable)?;
            Ok(result)
        })
    }
}
impl BackendCardCatalogProvider for PgUnifiedFictionalSource {
    fn authorized_catalog<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        Box::pin(async move {
            if identity.binding_version == 0 {
                return Err(StorageError::Session);
            }
            let Some(c) = self
                .document
                .data
                .card_catalogs
                .iter()
                .find(|c| c.owner == identity.user_id && c.account == *account)
            else {
                return Ok(None);
            };
            let mut tx = self.repository.simulation_source_pool.begin().await?;
            self.check(&mut tx).await?;
            let result = BackendCardCatalog {
                owner: c.owner.clone(),
                account: c.account.clone(),
                source_revision: c.source_revision.clone(),
                cards: c
                    .cards
                    .iter()
                    .map(|card| BackendCatalogCard {
                        card_id: card.card_id.as_str().into(),
                        last_four: card.last_four.clone(),
                    })
                    .collect(),
            };
            tx.commit().await?;
            Ok(Some(result))
        })
    }
    fn current_revision<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            self.authorized_catalog(identity, account)
                .await?
                .map(|c| c.source_revision)
                .ok_or(StorageError::DependencyUnavailable)
        })
    }
}
