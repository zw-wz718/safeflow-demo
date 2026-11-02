//! Borrowed-connection card catalog and freeze-history store. No pool, transaction control,
//! migration runner, authentication, permit or release write entry.
//! Echo must establish owner/task/operation protection and consume the proper
//! permit before a write callback. The assigned transaction check is NOT auth.
use safeflow_card_facts::{
    account_service::{AccountCardFacts, AccountDataSource, AccountPage, AccountQueryService},
    service::SourceUnavailable,
    Account, Card, LockRecord, LockSource, LockState,
};
use sqlx::{PgConnection, Row};
use std::{collections::HashSet, fmt};

pub mod backend;
pub mod card_selection;
pub mod catalog_inspection;
pub mod freeze;
pub mod freeze_completion;
pub mod freeze_preparation;
pub mod protection_history;

#[derive(Debug)]
pub enum StoreError {
    InvalidInput,
    TransactionRequired,
    ResourceUnavailable,
    CatalogUninitialized,
    Conflict,
    InconsistentRecord,
    SnapshotChanged,
    NeedsCoordination,
    RetryKeyNeedsCoordination,
    Database(sqlx::Error),
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InvalidInput => "invalid card reference",
            Self::TransactionRequired => "assigned caller transaction required",
            Self::ResourceUnavailable => "resource unavailable",
            Self::CatalogUninitialized => "complete card catalog unavailable",
            Self::Conflict => "card record conflict",
            Self::InconsistentRecord => "inconsistent card records",
            Self::SnapshotChanged => "card snapshot changed",
            Self::NeedsCoordination => "card history requires coordination",
            Self::RetryKeyNeedsCoordination => "public retry key mapping requires coordination",
            Self::Database(_) => "card database unavailable",
        };
        f.write_str(name)
    }
}
impl std::error::Error for StoreError {}
impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Trusted complete catalog for ONE account. Not Deserialize or client input;
/// the backend must prove completeness before calling initialize_catalog.
#[derive(Clone, Debug)]
pub struct CatalogCard {
    pub card_id: String,
    pub last_four: String,
}

/// Exclusively borrows the caller's already-assigned transaction connection.
/// No deref/connection getter: a scope cannot accidentally hand its borrowed
/// connection to another caller that commits/releases the account protection.
pub struct LockedCardAccount<'a> {
    conn: &'a mut PgConnection,
    owner: String,
    account: String,
    transaction_id: String,
}

fn valid_id(value: &str) -> bool {
    !value.trim().is_empty()
}
fn valid_card(card: &CatalogCard) -> bool {
    valid_id(&card.card_id)
        && card.last_four.len() == 4
        && card.last_four.bytes().all(|b| b.is_ascii_digit())
}

impl<'a> LockedCardAccount<'a> {
    /// Call AFTER the public owner/task/operation locks. For funds involving
    /// two accounts the caller first locks their full account set in stable
    /// order, then borrows this scope. Never invert those public lock orders.
    /// READ COMMITTED is required so the statement after a lock wait is fresh.
    pub async fn acquire(
        conn: &'a mut PgConnection,
        trusted_owner: &str,
        account_id: &str,
    ) -> Result<Self, StoreError> {
        if !valid_id(trusted_owner) || !valid_id(account_id) {
            return Err(StoreError::InvalidInput);
        }
        let (transaction_id, isolation): (Option<String>, String) = sqlx::query_as(
            "SELECT txid_current_if_assigned()::text,current_setting('transaction_isolation')",
        )
        .fetch_one(&mut *conn)
        .await?;
        let transaction_id = transaction_id.ok_or(StoreError::TransactionRequired)?;
        if isolation != "read committed" {
            return Err(StoreError::TransactionRequired);
        }
        let owned:Option<String>=sqlx::query_scalar(
            "SELECT account_id FROM transfer_accounts_v1 WHERE account_id=$1 AND owner_id=$2 FOR UPDATE"
        ).bind(account_id).bind(trusted_owner).fetch_optional(&mut *conn).await?;
        if owned.is_none() {
            return Err(StoreError::ResourceUnavailable);
        }
        let mut scope = Self {
            conn,
            owner: trusted_owner.into(),
            account: account_id.into(),
            transaction_id,
        };
        scope.check_transaction().await?;
        Ok(scope)
    }

    async fn check_transaction(&mut self) -> Result<(), StoreError> {
        let id: Option<String> = sqlx::query_scalar("SELECT txid_current_if_assigned()::text")
            .fetch_one(&mut *self.conn)
            .await?;
        if id.as_deref() != Some(self.transaction_id.as_str()) {
            return Err(StoreError::TransactionRequired);
        }
        Ok(())
    }

    async fn revision(&mut self) -> Result<i64, StoreError> {
        sqlx::query_scalar("SELECT revision FROM card_account_versions_v1 WHERE account_id=$1")
            .bind(&self.account)
            .fetch_optional(&mut *self.conn)
            .await?
            .ok_or(StoreError::CatalogUninitialized)
    }

    async fn cards(&mut self) -> Result<Vec<Card>, StoreError> {
        let rows=sqlx::query("SELECT card_id,account_id,last_four FROM card_catalog_v1 WHERE account_id=$1 ORDER BY card_id")
            .bind(&self.account).fetch_all(&mut *self.conn).await?;
        rows.into_iter()
            .map(|r| {
                Ok(Card {
                    id: r.try_get("card_id")?,
                    account_id: r.try_get("account_id")?,
                    last_four: r.try_get("last_four")?,
                })
            })
            .collect()
    }

    /// Initialize from a complete trusted catalog once. Repeating the exact
    /// catalog is a no-op; a different catalog is NOT an implicit replacement.
    /// Empty is allowed only when the trusted backend proved it complete.
    pub async fn initialize_catalog(
        &mut self,
        catalog: &[CatalogCard],
    ) -> Result<String, StoreError> {
        self.check_transaction().await?;
        let mut ids = HashSet::new();
        if catalog
            .iter()
            .any(|c| !valid_card(c) || !ids.insert(c.card_id.as_str()))
        {
            return Err(StoreError::InvalidInput);
        }
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM card_account_versions_v1 WHERE account_id=$1")
                .bind(&self.account)
                .fetch_optional(&mut *self.conn)
                .await?;
        if let Some(revision) = existing {
            let cards = self.cards().await?;
            if cards.len() != catalog.len()
                || catalog.iter().any(|c| {
                    !cards
                        .iter()
                        .any(|r| r.id == c.card_id && r.last_four == c.last_four)
                })
            {
                return Err(StoreError::Conflict);
            }
            return Ok(version(revision));
        }
        // Check every global card ID before beginning the catalog insert. Do not
        // silently move a card from another account or partially replace data.
        for card in catalog {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM card_catalog_v1 WHERE card_id=$1)")
                    .bind(&card.card_id)
                    .fetch_one(&mut *self.conn)
                    .await?;
            if exists {
                return Err(StoreError::Conflict);
            }
        }
        sqlx::query("INSERT INTO card_account_versions_v1(account_id) VALUES($1)")
            .bind(&self.account)
            .execute(&mut *self.conn)
            .await?;
        for card in catalog {
            sqlx::query(
                "INSERT INTO card_catalog_v1(card_id,account_id,last_four) VALUES($1,$2,$3)",
            )
            .bind(&card.card_id)
            .bind(&self.account)
            .bind(&card.last_four)
            .execute(&mut *self.conn)
            .await?;
        }
        Ok(version(self.revision().await?))
    }

    /// Add one explicitly trusted new mapping; does not mark an unknown account
    /// complete. Existing mapping and tail must match exactly for a no-op.
    pub async fn register_card(&mut self, card: &CatalogCard) -> Result<String, StoreError> {
        self.check_transaction().await?;
        if !valid_card(card) {
            return Err(StoreError::InvalidInput);
        }
        self.revision().await?;
        let row = sqlx::query("SELECT account_id,last_four FROM card_catalog_v1 WHERE card_id=$1")
            .bind(&card.card_id)
            .fetch_optional(&mut *self.conn)
            .await?;
        if let Some(row) = row {
            if row.try_get::<&str, _>("account_id")? != self.account
                || row.try_get::<&str, _>("last_four")? != card.last_four
            {
                return Err(StoreError::Conflict);
            }
        } else {
            sqlx::query(
                "INSERT INTO card_catalog_v1(card_id,account_id,last_four) VALUES($1,$2,$3)",
            )
            .bind(&card.card_id)
            .bind(&self.account)
            .bind(&card.last_four)
            .execute(&mut *self.conn)
            .await?;
        }
        Ok(version(self.revision().await?))
    }

    /// Actual complete database read while the common account row is held.
    /// Includes released history; no pagination limit can hide a later lock.
    /// Success is card facts, not overall account health or a payment permit.
    pub async fn read_facts(&mut self) -> Result<AccountCardFacts, StoreError> {
        self.check_transaction().await?;
        let facts = read_complete_facts(self.conn, &self.owner, &self.account).await?;
        self.check_transaction().await?;
        Ok(facts)
    }
}

pub(crate) async fn read_revision(
    connection: &mut PgConnection,
    account: &str,
) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT revision FROM card_account_versions_v1 WHERE account_id=$1")
        .bind(account)
        .fetch_optional(connection)
        .await?
        .ok_or(StoreError::CatalogUninitialized)
}
async fn read_cards(connection: &mut PgConnection, account: &str) -> Result<Vec<Card>, StoreError> {
    let rows=sqlx::query("SELECT card_id,account_id,last_four FROM card_catalog_v1 WHERE account_id=$1 ORDER BY card_id")
        .bind(account).fetch_all(connection).await?;
    rows.into_iter()
        .map(|r| {
            Ok(Card {
                id: r.try_get("card_id")?,
                account_id: r.try_get("account_id")?,
                last_four: r.try_get("last_four")?,
            })
        })
        .collect()
}
/// Shared full-facts SQL/projection. Writers hold the common account lock;
/// completion readers must enforce a read-only boundary and bracket the whole
/// original-record/facts read with this same monotonic account version.
pub(crate) async fn read_complete_facts(
    connection: &mut PgConnection,
    owner: &str,
    account: &str,
) -> Result<AccountCardFacts, StoreError> {
    let before = read_revision(connection, account).await?;
    let cards = read_cards(connection, account).await?;
    let rows=sqlx::query("SELECT l.lock_id,l.card_id,l.source,l.state FROM card_locks_v1 l JOIN card_catalog_v1 c ON c.card_id=l.card_id WHERE c.account_id=$1 ORDER BY l.lock_id")
            .bind(account).fetch_all(&mut *connection).await?;
    let locks = rows
        .into_iter()
        .map(|r| {
            let source = match r.try_get::<&str, _>("source")? {
                "USER_SELF" => LockSource::UserSelf,
                "ANOMALY_DISPUTE" => LockSource::AnomalyDispute,
                "RISK_CONTROL" => LockSource::RiskControl,
                _ => return Err(StoreError::InconsistentRecord),
            };
            let state = match r.try_get::<&str, _>("state")? {
                "ACTIVE" => LockState::Active,
                "RELEASED" => LockState::Released,
                _ => return Err(StoreError::InconsistentRecord),
            };
            Ok(LockRecord {
                id: r.try_get("lock_id")?,
                card_id: r.try_get("card_id")?,
                source,
                state,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    if read_revision(connection, account).await? != before {
        return Err(StoreError::SnapshotChanged);
    }
    let page = AccountPage {
        account: Account {
            id: account.to_owned(),
            user_id: owner.to_owned(),
        },
        revision: version(before),
        total_cards: cards.len(),
        total_locks: locks.len(),
        cards,
        locks,
        next_cursor: None,
    };
    AccountQueryService::new(CompletePage(page))
        .query(owner, Some(account))
        .map_err(|_| StoreError::InconsistentRecord)
}

fn version(revision: i64) -> String {
    format!("card-pg-v1:{revision}")
}

/// Pure projection reuse only. Actual SQL/revision checks occurred above under
/// the retained database account lock, not in this in-memory adapter.
struct CompletePage(AccountPage);
impl AccountDataSource for CompletePage {
    fn read_page(
        &self,
        user: &str,
        account: &str,
        cursor: Option<&str>,
        expected: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        if user != self.0.account.user_id
            || account != self.0.account.id
            || cursor.is_some()
            || expected.is_some()
        {
            return Err(SourceUnavailable);
        }
        Ok(self.0.clone())
    }
    fn read_current_revision(
        &self,
        user: &str,
        account: &str,
    ) -> Result<String, SourceUnavailable> {
        if user != self.0.account.user_id || account != self.0.account.id {
            return Err(SourceUnavailable);
        }
        Ok(self.0.revision.clone())
    }
}
