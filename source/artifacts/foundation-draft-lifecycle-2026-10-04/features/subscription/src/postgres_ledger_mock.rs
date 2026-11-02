//! PostgreSQL storage of immutable fictional snapshots in an isolated namespace.
//! No production identity, business writes, authorization, scheduler or audit.
use crate::{
    ledger_source_mock::{load_owned_ledger_mock, MockLedgerSource, MockLedgerSourceError},
    managed_ledger_mock::LedgerFixture,
};
use serde_json::Value;
use sqlx::PgPool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockPostgresIssue {
    Unavailable,
    NotFound,
    InvalidSnapshot,
    Conflict,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockSnapshotSave {
    Stored,
    AlreadyStored,
}
pub struct PostgresLedgerMock {
    pool: PgPool,
}
struct SnapshotSource(LedgerFixture);
impl MockLedgerSource for SnapshotSource {
    fn read_owned_mock(&self, _: &str) -> Result<LedgerFixture, MockLedgerSourceError> {
        Ok(self.0.clone())
    }
}
fn validate(user: &str, fixture: &LedgerFixture) -> Result<(), MockPostgresIssue> {
    load_owned_ledger_mock(&SnapshotSource(fixture.clone()), user)
        .map(|_| ())
        .map_err(|_| MockPostgresIssue::InvalidSnapshot)
}
impl PostgresLedgerMock {
    /// Caller provides a dedicated local simulation database; never shared state.
    pub fn from_pool_for_local_simulation(pool: PgPool) -> Self {
        Self { pool }
    }
    /// Explicit test setup only, not an automatic production migration.
    pub async fn initialize_local_schema(&self) -> Result<(), MockPostgresIssue> {
        sqlx::raw_sql(include_str!("../sql/ledger_snapshot_mock_v1.proposed.sql"))
            .execute(&self.pool)
            .await
            .map_err(|_| MockPostgresIssue::Unavailable)?;
        Ok(())
    }
    /// Fixture setup only. Same snapshot/content is idempotent; changed content
    /// with the same identity is rejected, never silently overwrites history.
    pub async fn save_fixture_mock(
        &self,
        user: &str,
        fixture: &LedgerFixture,
    ) -> Result<MockSnapshotSave, MockPostgresIssue> {
        validate(user, fixture)?;
        let id = fixture
            .snapshot_id
            .as_deref()
            .ok_or(MockPostgresIssue::InvalidSnapshot)?;
        let value =
            serde_json::to_value(fixture).map_err(|_| MockPostgresIssue::InvalidSnapshot)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| MockPostgresIssue::Unavailable)?;
        let inserted = sqlx::query_scalar::<_, Value>(
            "INSERT INTO safeflow_subscription_mock_v1.ledger_snapshots (owner_id,snapshot_id,snapshot) VALUES ($1,$2,$3) ON CONFLICT (owner_id,snapshot_id) DO NOTHING RETURNING snapshot",
        ).bind(user).bind(id).bind(&value).fetch_optional(&mut *tx).await
            .map_err(|_| MockPostgresIssue::Unavailable)?;
        let outcome = if inserted.is_some() {
            MockSnapshotSave::Stored
        } else {
            let original: Value = sqlx::query_scalar(
                "SELECT snapshot FROM safeflow_subscription_mock_v1.ledger_snapshots WHERE owner_id=$1 AND snapshot_id=$2",
            ).bind(user).bind(id).fetch_one(&mut *tx).await
                .map_err(|_| MockPostgresIssue::Unavailable)?;
            if original != value {
                return Err(MockPostgresIssue::Conflict);
            }
            MockSnapshotSave::AlreadyStored
        };
        tx.commit()
            .await
            .map_err(|_| MockPostgresIssue::Unavailable)?;
        Ok(outcome)
    }
    /// Reads one exact owner/snapshot. Missing and failed reads are distinct and
    /// neither is fabricated into an empty ledger. This restores rows only.
    pub async fn read_owned_fixture_mock(
        &self,
        user: &str,
        snapshot_id: &str,
    ) -> Result<LedgerFixture, MockPostgresIssue> {
        if user.trim().is_empty() || snapshot_id.trim().is_empty() {
            return Err(MockPostgresIssue::InvalidSnapshot);
        }
        let value = sqlx::query_scalar::<_, Value>(
            "SELECT snapshot FROM safeflow_subscription_mock_v1.ledger_snapshots WHERE owner_id=$1 AND snapshot_id=$2",
        ).bind(user).bind(snapshot_id).fetch_optional(&self.pool).await
            .map_err(|_| MockPostgresIssue::Unavailable)?
            .ok_or(MockPostgresIssue::NotFound)?;
        let fixture: LedgerFixture =
            serde_json::from_value(value).map_err(|_| MockPostgresIssue::InvalidSnapshot)?;
        if fixture.snapshot_id.as_deref() != Some(snapshot_id) {
            return Err(MockPostgresIssue::InvalidSnapshot);
        }
        validate(user, &fixture)?;
        Ok(fixture)
    }
}
