//! Backend adoption of a reviewed catalog provider. A DTO, tail or assigned
//! transaction is not proof of source authority. No HTTP/model initialization.
use crate::models::{db_version, hash, valid_id};
use crate::repository::guarded;
use crate::{PgRepository, StorageError};
use safeflow_card_facts::account_service::AccountCardFacts;
pub use safeflow_card_postgres_store::CatalogCard as BackendCatalogCard;
use safeflow_card_postgres_store::{CatalogCard, LockedCardAccount, StoreError};
use safeflow_contracts::{ContractFuture, ResourceId, SessionRef, UserId};
use safeflow_runtime::{SessionBinding, SessionResolver};
use sqlx::{PgConnection, Row};
use std::sync::Arc;

pub const CARD_POSTGRES_SOURCE_REVISION: &str =
    "2b7880f66a648e0df5559aaa62f79a1a19be63bb:readonly-minimal";

// Sealed backend observation. Deliberately neither wire data nor a permit.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CardObservationProof {
    identity: SessionBinding,
    account: ResourceId,
    facts_revision: String,
    adoption: AdoptionProof,
}
impl CardObservationProof {
    // Original-result reads may observe changed locks, while the adopted
    // catalog identity/version and authenticated namespace must remain bound.
    pub(crate) fn same_catalog(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.account == other.account
            && self.adoption == other.adoption
    }
}
#[derive(Clone, PartialEq, Eq)]
struct AdoptionProof {
    source_id: String,
    implementation_revision: String,
    ordinal: i64,
    source_revision: String,
    digest: String,
}

/// Data returned by an explicitly adopted BACKEND provider, never a wire DTO.
/// The provider must verify maintenance authorization, exact scope and complete
/// enumeration against its actual authority. Constructing this value is not auth.
pub struct BackendCardCatalog {
    pub owner: UserId,
    pub account: ResourceId,
    pub source_revision: String,
    pub cards: Vec<CatalogCard>,
}

pub trait BackendCardCatalogProvider: Send + Sync {
    /// None means unknown/unauthorized, NOT a complete empty catalog. Returning
    /// Some with zero cards requires affirmative complete-source evidence.
    fn authorized_catalog<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>>;
    /// Revalidate the same identity's current read authorization and complete
    /// catalog revision after waits, including ABA. Revocation/unknown scope or
    /// a provider failure must return Err, never its last cached revision.
    fn current_revision<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>>;
}

/// Approval is an explicit trusted application assembly decision, not a claim
/// accepted from a request. Startup currently binds NO provider: shared-demo and
/// member fixtures are not reviewed full catalogs. There is no sample provider
/// or environment/JSON loader in production.
#[derive(Clone)]
pub struct CardCatalogSourceBinding {
    source_id: String,
    implementation_revision: String,
    provider: Arc<dyn BackendCardCatalogProvider>,
}
impl CardCatalogSourceBinding {
    async fn observation_proof(
        &self,
        conn: &mut PgConnection,
        identity: &SessionBinding,
        account: &ResourceId,
    ) -> Result<AdoptionProof, StorageError> {
        // Latest for the ACCOUNT, never latest filtered by the desired source.
        let row = sqlx::query("SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1")
            .bind(account.as_str()).fetch_optional(conn).await?
            .ok_or(StorageError::DependencyUnavailable)?;
        let proof = AdoptionProof {
            source_id: row.try_get("source_id")?,
            implementation_revision: row.try_get("implementation_revision")?,
            ordinal: row.try_get("ordinal")?,
            source_revision: row.try_get("source_revision")?,
            digest: row.try_get("catalog_digest")?,
        };
        if row.try_get::<String, _>("owner_id")? != identity.user_id.as_str()
            || proof.source_id != self.source_id
            || proof.implementation_revision != self.implementation_revision
            || proof.ordinal < 1
        {
            return Err(StorageError::FactsConflict);
        }
        Ok(proof)
    }
    pub(crate) async fn verify_current(
        &self,
        conn: &mut PgConnection,
        identity: &SessionBinding,
        account: &ResourceId,
    ) -> Result<(), StorageError> {
        let revision:String=sqlx::query_scalar("SELECT source_revision FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 AND owner_id=$2 AND source_id=$3 AND implementation_revision=$4 ORDER BY ordinal DESC LIMIT 1")
            .bind(account.as_str()).bind(identity.user_id.as_str()).bind(&self.source_id).bind(&self.implementation_revision)
            .fetch_optional(conn).await?.ok_or(StorageError::DependencyUnavailable)?;
        if self.provider.current_revision(identity, account).await? != revision {
            return Err(StorageError::FactsConflict);
        }
        Ok::<(), StorageError>(())
    }
    pub fn for_reviewed_backend_provider(
        source_id: &str,
        implementation_revision: &str,
        provider: Arc<dyn BackendCardCatalogProvider>,
    ) -> Result<Self, StorageError> {
        valid_id(source_id)?;
        valid_id(implementation_revision)?;
        Ok(Self {
            source_id: source_id.into(),
            implementation_revision: implementation_revision.into(),
            provider,
        })
    }
}

#[derive(Clone)]
pub struct CardCatalogService {
    repository: PgRepository,
    sessions: Arc<dyn SessionResolver>,
    source: Option<CardCatalogSourceBinding>,
}
impl CardCatalogService {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        source: Option<CardCatalogSourceBinding>,
    ) -> Self {
        Self {
            repository,
            sessions,
            source,
        }
    }
    fn identity(&self, session: &SessionRef) -> Result<SessionBinding, StorageError> {
        let identity = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        db_version(identity.binding_version)?;
        valid_id(identity.user_id.as_str())?;
        Ok(identity)
    }
    fn guard(&self, session: &SessionRef, identity: &SessionBinding) -> Result<(), StorageError> {
        if self.identity(session)? != *identity {
            return Err(StorageError::Session);
        }
        Ok::<(), StorageError>(())
    }
    /// An authorized provider supplies the entire catalog; callers cannot pass
    /// cards, owner, completeness or Allowed. This method owns commit/rollback.
    pub async fn initialize(
        &self,
        session: &SessionRef,
        account: &ResourceId,
    ) -> Result<String, StorageError> {
        self.adopt(session, account, None).await
    }
    /// Only one expressly supplied provider mapping may be appended to an
    /// already adopted catalog. Not a way to initialize an unknown account.
    pub async fn register_mapping(
        &self,
        session: &SessionRef,
        account: &ResourceId,
        card: &ResourceId,
    ) -> Result<String, StorageError> {
        self.adopt(session, account, Some(card)).await
    }
    async fn adopt(
        &self,
        session: &SessionRef,
        account: &ResourceId,
        new_card: Option<&ResourceId>,
    ) -> Result<String, StorageError> {
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let source = self
            .source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let catalog = guarded(
            &guard,
            source.provider.authorized_catalog(&identity, account),
        )
        .await?
        .ok_or(StorageError::DependencyUnavailable)?;
        if catalog.owner != identity.user_id || catalog.account != *account {
            return Err(StorageError::NotFound);
        }
        valid_id(&catalog.source_revision)?;
        let digest = catalog_digest(
            catalog
                .cards
                .iter()
                .map(|c| (c.card_id.as_str(), c.last_four.as_str())),
        )?;
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        guarded(
            &guard,
            crate::daily_usage::protect_owner(&mut tx, &identity.user_id),
        )
        .await?;
        // Explicit assignment is a transaction prerequisite, not authentication.
        sqlx::query("SELECT txid_current()")
            .execute(&mut *tx)
            .await?;
        let mut locked =
            LockedCardAccount::acquire(&mut tx, identity.user_id.as_str(), account.as_str())
                .await
                .map_err(card_store_error)?;
        let revision = if let Some(card_id) = new_card {
            // Refuse pre-provisioned but unadopted member catalogs as well.
            drop(locked);
            read_adopted_card_facts(&mut tx, &identity.user_id, account, Some(source)).await?;
            let new_mapping = catalog
                .cards
                .iter()
                .find(|c| c.card_id == card_id.as_str())
                .ok_or(StorageError::NotFound)?;
            locked =
                LockedCardAccount::acquire(&mut tx, identity.user_id.as_str(), account.as_str())
                    .await
                    .map_err(card_store_error)?;
            locked
                .register_card(new_mapping)
                .await
                .map_err(card_store_error)?
        } else {
            locked
                .initialize_catalog(&catalog.cards)
                .await
                .map_err(card_store_error)?
        };
        let facts = locked.read_facts().await.map_err(card_store_error)?;
        if facts_digest(&facts)? != digest {
            return Err(StorageError::FactsConflict);
        }
        drop(locked);
        // Append source provenance atomically with the real member catalog. A
        // failed authorization/revision/DB check leaves neither behind.
        let previous = sqlx::query("SELECT * FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1")
            .bind(account.as_str()).fetch_optional(&mut *tx).await?;
        let same = previous.as_ref().is_some_and(|r| {
            r.get::<String, _>("source_id") == source.source_id
                && r.get::<String, _>("implementation_revision") == source.implementation_revision
                && r.get::<String, _>("source_revision") == catalog.source_revision
                && r.get::<String, _>("catalog_digest") == digest
        });
        if !same {
            if previous.as_ref().is_some_and(|r| {
                r.get::<String, _>("source_id") != source.source_id
                    || r.get::<String, _>("implementation_revision")
                        != source.implementation_revision
            }) {
                return Err(StorageError::FactsConflict);
            }
            sqlx::query("INSERT INTO public_card_catalog_adoptions_v1(account_id,owner_id,ordinal,source_id,implementation_revision,source_revision,catalog_digest) SELECT $1,$2,COALESCE(MAX(ordinal),0)+1,$3,$4,$5,$6 FROM public_card_catalog_adoptions_v1 WHERE account_id=$1")
                .bind(account.as_str()).bind(identity.user_id.as_str()).bind(&source.source_id)
                .bind(&source.implementation_revision).bind(&catalog.source_revision).bind(digest)
                .execute(&mut *tx).await?;
        }
        if guarded(&guard, source.provider.current_revision(&identity, account)).await?
            != catalog.source_revision
        {
            return Err(StorageError::FactsConflict);
        }
        guarded(&guard, tx.commit()).await?;
        Ok(revision)
    }
    /// Fresh read in READ COMMITTED with real account protection. Protection
    /// ends at this method's commit: the returned value is only an observation.
    pub async fn observe(
        &self,
        session: &SessionRef,
        account: &ResourceId,
    ) -> Result<AccountCardFacts, StorageError> {
        let identity = self.identity(session)?;
        let guard = || self.guard(session, &identity);
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        guarded(
            &guard,
            crate::daily_usage::protect_owner(&mut tx, &identity.user_id),
        )
        .await?;
        sqlx::query("SELECT txid_current()")
            .execute(&mut *tx)
            .await?;
        let facts = guarded(
            &guard,
            read_adopted_card_facts(&mut tx, &identity.user_id, account, self.source.as_ref()),
        )
        .await?;
        guarded(
            &guard,
            self.source
                .as_ref()
                .ok_or(StorageError::DependencyUnavailable)?
                .verify_current(&mut tx, &identity, account),
        )
        .await?;
        guarded(&guard, tx.commit()).await?;
        Ok(facts)
    }
}

/// Async reader for the Runtime's already authenticated owner. This is not a
/// session resolver and must never be called with a request-selected owner.
#[derive(Clone)]
pub struct DatabaseCardReader {
    repository: PgRepository,
    source: Option<CardCatalogSourceBinding>,
}
impl DatabaseCardReader {
    pub(crate) async fn observe_for_navigation(
        &self,
        session: &SessionRef,
        sessions: &dyn SessionResolver,
        expected: &SessionBinding,
        account: &ResourceId,
    ) -> Result<CardObservationProof, StorageError> {
        let guard = || {
            if expected.binding_version == 0
                || sessions
                    .resolve_binding(session)
                    .map_err(|_| StorageError::Session)?
                    != *expected
            {
                return Err(StorageError::Session);
            }
            Ok::<(), StorageError>(())
        };
        guard()?;
        let source = self
            .source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?;
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(&guard, async {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
                .execute(&mut *tx)
                .await?;
            Ok::<(), StorageError>(())
        })
        .await?;
        guarded(
            &guard,
            crate::daily_usage::protect_owner(&mut tx, &expected.user_id),
        )
        .await?;
        guarded(&guard, async {
            sqlx::query("SELECT txid_current()")
                .execute(&mut *tx)
                .await?;
            Ok::<(), StorageError>(())
        })
        .await?;
        let facts = guarded(
            &guard,
            read_adopted_card_facts(&mut tx, &expected.user_id, account, Some(source)),
        )
        .await?;
        let adoption =
            guarded(&guard, source.observation_proof(&mut tx, expected, account)).await?;
        let current = guarded(&guard, source.provider.current_revision(expected, account)).await?;
        if current != adoption.source_revision || adoption.digest != facts_digest(&facts)? {
            return Err(StorageError::FactsConflict);
        }
        let after = guarded(&guard, source.observation_proof(&mut tx, expected, account)).await?;
        if after != adoption {
            return Err(StorageError::FactsConflict);
        }
        guarded(&guard, tx.commit()).await?;
        Ok(CardObservationProof {
            identity: expected.clone(),
            account: account.clone(),
            facts_revision: facts.revision,
            adoption,
        })
    }
    pub fn for_trusted_backend(
        repository: PgRepository,
        source: Option<CardCatalogSourceBinding>,
    ) -> Self {
        Self { repository, source }
    }
    pub async fn observe_authenticated_session(
        &self,
        session: &safeflow_runtime::AuthenticatedReadSession,
        account: &ResourceId,
    ) -> Result<AccountCardFacts, StorageError> {
        let identity = session.check().map_err(|_| StorageError::Session)?;
        let owner = &identity.user_id;
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        crate::daily_usage::protect_owner(&mut tx, owner).await?;
        sqlx::query("SELECT txid_current()")
            .execute(&mut *tx)
            .await?;
        let facts = read_adopted_card_facts(&mut tx, owner, account, self.source.as_ref()).await?;
        self.source
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)?
            .verify_current(&mut tx, &identity, account)
            .await?;
        session.check().map_err(|_| StorageError::Session)?;
        tx.commit().await?;
        session.check().map_err(|_| StorageError::Session)?;
        Ok(facts)
    }
}

/// Final payment calls this on its SAME protected connection AFTER stable
/// payer/recipient locking; unlike observe it retains protection until COMMIT.
pub(crate) async fn read_adopted_card_facts(
    conn: &mut PgConnection,
    owner: &UserId,
    account: &ResourceId,
    source: Option<&CardCatalogSourceBinding>,
) -> Result<AccountCardFacts, StorageError> {
    let mut locked = LockedCardAccount::acquire(conn, owner.as_str(), account.as_str())
        .await
        .map_err(card_store_error)?;
    let facts = locked.read_facts().await.map_err(card_store_error)?;
    drop(locked);
    let source = source.ok_or(StorageError::DependencyUnavailable)?;
    let row = sqlx::query("SELECT owner_id,source_id,implementation_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1")
        .bind(account.as_str()).fetch_optional(conn).await?.ok_or(StorageError::DependencyUnavailable)?;
    if row.try_get::<String, _>("owner_id")? != owner.as_str()
        || row.try_get::<String, _>("source_id")? != source.source_id
        || row.try_get::<String, _>("implementation_revision")? != source.implementation_revision
        || row.try_get::<String, _>("catalog_digest")? != facts_digest(&facts)?
    {
        return Err(StorageError::FactsConflict);
    }
    Ok(facts)
}
fn catalog_digest<'a>(
    cards: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<String, StorageError> {
    let mut cards: Vec<_> = cards.collect();
    cards.sort_unstable();
    if cards.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(StorageError::FactsConflict);
    }
    Ok(hash(&cards)?.as_str().into())
}
fn facts_digest(facts: &AccountCardFacts) -> Result<String, StorageError> {
    catalog_digest(
        facts
            .cards
            .iter()
            .map(|c| (c.card_id.as_str(), c.last_four.as_str())),
    )
}
pub(crate) fn card_store_error(error: StoreError) -> StorageError {
    match error {
        StoreError::ResourceUnavailable => StorageError::NotFound,
        StoreError::Database(error) => error.into(),
        StoreError::CatalogUninitialized => StorageError::DependencyUnavailable,
        StoreError::TransactionRequired => StorageError::TransactionScope,
        StoreError::InvalidInput => StorageError::Invalid("invalid card mapping"),
        _ => StorageError::FactsConflict,
    }
}

#[cfg(test)]
mod tests;
