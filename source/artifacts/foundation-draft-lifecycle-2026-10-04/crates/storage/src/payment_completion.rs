//! Unadopted local completion persistence/queue. A lease is NOT authority.
//! No registration, grant issuance, decision receiver or funds writer exists.
use crate::{models::valid_id, PgRepository, StorageError};
use chrono::{DateTime, Utc};
use sqlx::Row;
use std::sync::Arc;
use std::time::Duration;

/// Operational label and epoch only. It is not an authenticated principal and
/// cannot be passed to PermitTransaction or any funds entry point.
#[derive(Debug, Clone)]
pub struct PaymentCompletionCandidateLease {
    scope: Arc<()>,
    grant_id: String,
    owner: String,
    epoch: i64,
    until: DateTime<Utc>,
}
impl PartialEq for PaymentCompletionCandidateLease {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.scope, &other.scope)
            && self.grant_id == other.grant_id
            && self.owner == other.owner
            && self.epoch == other.epoch
            && self.until == other.until
    }
}
impl Eq for PaymentCompletionCandidateLease {}
impl PaymentCompletionCandidateLease {
    pub fn grant_id(&self) -> &str {
        &self.grant_id
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
    pub fn until(&self) -> DateTime<Utc> {
        self.until
    }
}

/// Backend native scheduler mechanism only; never installed by main/HTTP/Agent.
/// No enqueue API: only a future reviewed atomic acceptance builder may create
/// real jobs. Direct SQL test rows prove queue mechanics, not deployed authority.
/// Each constructor fixes a separate process-local repository scope. Share a
/// queue by cloning it; a new queue/process must acquire its own token.
/// Controls are observed after the job lock but are NOT held through COMMIT.
/// A concurrent pause/revoke can therefore invalidate a just-returned lease.
/// is_current is a read-only observation, never a linearizable funds guard.
#[derive(Clone)]
pub struct PaymentCompletionCandidateQueue {
    repository: PgRepository,
    scope: Arc<()>,
}
impl PaymentCompletionCandidateQueue {
    pub fn for_trusted_backend(repository: PgRepository) -> Self {
        Self {
            repository,
            scope: Arc::new(()),
        }
    }
    pub async fn claim(
        &self,
        grant_id: &str,
        worker_label: &str,
        ttl: Duration,
    ) -> Result<Option<PaymentCompletionCandidateLease>, StorageError> {
        self.update(grant_id, worker_label, ttl, None).await
    }
    /// Renew changes the epoch too. The old token immediately becomes stale.
    pub async fn renew(
        &self,
        lease: &PaymentCompletionCandidateLease,
        ttl: Duration,
    ) -> Result<Option<PaymentCompletionCandidateLease>, StorageError> {
        // Scope denotes this immutable repository-bearing queue and its clones,
        // NOT a namespace string (which can collide across databases). A new
        // queue, even for the same physical DB, must acquire a fresh token.
        if !Arc::ptr_eq(&self.scope, &lease.scope) {
            return Err(StorageError::TransactionScope);
        }
        self.update(&lease.grant_id, &lease.owner, ttl, Some(lease.epoch))
            .await
    }
    async fn update(
        &self,
        id: &str,
        worker: &str,
        ttl: Duration,
        epoch: Option<i64>,
    ) -> Result<Option<PaymentCompletionCandidateLease>, StorageError> {
        valid_id(id)?;
        valid_id(worker)?;
        let ms = i64::try_from(ttl.as_millis()).map_err(|_| StorageError::Invalid("lease TTL"))?;
        if !(1..=300_000).contains(&ms) || ttl.subsec_nanos() % 1_000_000 != 0 {
            return Err(StorageError::Invalid(
                "lease requires 1..300000 whole milliseconds",
            ));
        }
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *tx)
            .await?;
        // Acquire ONLY the job. Evaluate the DB clock AFTER a contending lock
        // wait; statement_timestamp/CTE before FOR UPDATE would extend stale rights.
        let row = sqlx::query(
            "SELECT * FROM payment_completion_candidate_jobs WHERE grant_id=$1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let previous: i64 = row.try_get("lease_epoch")?;
        let until: Option<DateTime<Utc>> = row.try_get("lease_until")?;
        let owner: Option<String> = row.try_get("lease_owner")?;
        let eligible = if let Some(epoch) = epoch {
            previous == epoch && owner.as_deref() == Some(worker) && until.is_some_and(|u| u > now)
        } else {
            until.is_none_or(|u| u <= now)
        };
        let control:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_completion_candidate_controls c JOIN payment_completion_candidate_grants g ON g.grant_id=c.grant_id WHERE c.grant_id=$1 AND NOT c.paused AND NOT c.revoked AND g.completion_until>clock_timestamp())")
            .bind(id).fetch_one(&mut *tx).await?;
        if !eligible || !control {
            tx.commit().await?;
            return Ok(None);
        }
        let next = previous
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        let row=sqlx::query("UPDATE payment_completion_candidate_jobs SET lease_epoch=$2,lease_owner=$3,leased_at=$4,lease_until=$4+($5::bigint*interval '1 millisecond') WHERE grant_id=$1 RETURNING lease_until")
            .bind(id).bind(next).bind(worker).bind(now).bind(ms).fetch_one(&mut *tx).await?;
        let lease = PaymentCompletionCandidateLease {
            scope: self.scope.clone(),
            grant_id: id.into(),
            owner: worker.into(),
            epoch: next,
            until: row.try_get("lease_until")?,
        };
        // No result is exposed before COMMIT. A COMMIT error is StorageError::Database,
        // not permission to assume acquisition; reacquire/query the original job.
        tx.commit().await?;
        Ok(Some(lease))
    }
    /// Pure read of this token, no touch/renew/cleanup or funds permission.
    pub async fn is_current(
        &self,
        lease: &PaymentCompletionCandidateLease,
    ) -> Result<bool, StorageError> {
        if !Arc::ptr_eq(&self.scope, &lease.scope) {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let result=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_completion_candidate_jobs j JOIN payment_completion_candidate_controls c ON c.grant_id=j.grant_id JOIN payment_completion_candidate_grants g ON g.grant_id=j.grant_id WHERE j.grant_id=$1 AND j.lease_owner=$2 AND j.lease_epoch=$3 AND j.lease_until>clock_timestamp() AND NOT c.paused AND NOT c.revoked AND g.completion_until>clock_timestamp())")
            .bind(&lease.grant_id).bind(&lease.owner).bind(lease.epoch).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(result)
    }
}
