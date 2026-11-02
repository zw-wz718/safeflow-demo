//! Read-only adapter over the EXISTING shared_runtime_storage proposal.
//! This codec deliberately remains fictional. No public migration, session
//! authentication, permit issuance, write, restart or notification occurs here.
use super::*;
use crate::managed_ledger_mock::Disposition;

pub const FICTIONAL_JOURNAL_ENCODING_V1: &str = "safeflow.subscription.fictional-journal.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposedLedgerReadIssue {
    InvalidNamespace,
    ResourceUnavailable,
    NotFound,
    Unavailable,
    UnsupportedEncoding,
    InvalidStoredRecord,
    BusinessValidationFailed,
}

/// Trusted assembly chooses the namespace and supplies an authorized owner.
/// A namespace or owner string is not authorization. All data uses the existing
/// fictional decoder, including its ownership, completeness and journal checks.
pub struct ProposedLedgerReaderMock {
    select: String,
}

#[derive(Debug, Serialize)]
pub struct ProposedLedgerDiscoveryMock {
    pub fictional: bool,
    pub production_registered: bool,
    pub ledger_version: i64,
    pub matching_executed: bool,
    pub reason: &'static str,
    pub already_managed_count: usize,
    /// Original selection indexes are retained; this read does not create drafts.
    pub groups: Vec<CandidateViewMock>,
}

impl ProposedLedgerReaderMock {
    pub fn new(namespace: &str) -> std::result::Result<Self, ProposedLedgerReadIssue> {
        if namespace.is_empty()
            || namespace.len() > 63
            || !namespace
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase())
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            || namespace == "public"
            || namespace == "information_schema"
            || namespace.starts_with("pg_")
            || namespace == "safeflow_subscription_mock_v1"
        {
            return Err(ProposedLedgerReadIssue::InvalidNamespace);
        }
        Ok(Self {
            select: format!("SELECT storage_version,encoding_version,journal FROM \"{namespace}\".subscription_runtime_records WHERE owner_id=$1"),
        })
    }

    /// Exactly one SELECT on the caller's SQLx 0.7.4 connection. No pool,
    /// BEGIN/COMMIT, search_path mutation, DDL, fallback or automatic retry.
    /// A missing owner row MUST NOT initialize an empty ledger.
    pub async fn load_on_connection_mock(
        &self,
        connection: &mut PgConnection,
        trusted_owner: &str,
    ) -> std::result::Result<SubscriptionReadSnapshotMock, ProposedLedgerReadIssue> {
        if trusted_owner.trim().is_empty() {
            return Err(ProposedLedgerReadIssue::ResourceUnavailable);
        }
        let row: Option<(i64, String, Value)> = sqlx::query_as(&self.select)
            .bind(trusted_owner)
            .fetch_optional(connection)
            .await
            .map_err(|_| ProposedLedgerReadIssue::Unavailable)?;
        let (version, encoding, journal) = row.ok_or(ProposedLedgerReadIssue::NotFound)?;
        if encoding != FICTIONAL_JOURNAL_ENCODING_V1 {
            return Err(ProposedLedgerReadIssue::UnsupportedEncoding);
        }
        SubscriptionReadSnapshotMock::from_stored_journal_mock(trusted_owner, version, journal)
            .map_err(|_| ProposedLedgerReadIssue::InvalidStoredRecord)
    }

    /// Reuse adopted recognition and current-state rules. Hide only exact active
    /// matches; cancelling/error/uncertain cases keep their existing evidence.
    pub async fn identify_on_connection_mock(
        &self,
        connection: &mut PgConnection,
        context: &MockContext,
        consumption: &Snapshot,
    ) -> std::result::Result<ProposedLedgerDiscoveryMock, ProposedLedgerReadIssue> {
        let snapshot = self
            .load_on_connection_mock(connection, &context.user_id)
            .await?;
        let view = snapshot
            .identify_subscriptions_mock(context, consumption)
            .map_err(|_| ProposedLedgerReadIssue::BusinessValidationFailed)?;
        let already_managed_count = view
            .groups
            .iter()
            .filter(|g| g.disposition == Disposition::AlreadyManaged)
            .count();
        Ok(ProposedLedgerDiscoveryMock {
            fictional: true,
            production_registered: false,
            ledger_version: snapshot.version_mock(),
            matching_executed: view.matching_executed,
            reason: view.reason,
            already_managed_count,
            groups: view
                .groups
                .into_iter()
                .filter(|g| g.disposition != Disposition::AlreadyManaged)
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn namespace_is_one_explicit_identifier_never_a_search_path_or_mock_alias() {
        for name in [
            "",
            "public",
            "information_schema",
            "pg_catalog",
            "A",
            "a.b",
            "a,b",
            "a\";drop schema public;--",
            "safeflow_subscription_mock_v1",
            &"a".repeat(64),
        ] {
            assert!(matches!(
                ProposedLedgerReaderMock::new(name),
                Err(ProposedLedgerReadIssue::InvalidNamespace)
            ));
        }
        assert!(ProposedLedgerReaderMock::new("approved_subscription_1").is_ok());
    }
}
