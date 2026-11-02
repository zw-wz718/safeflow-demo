//! Internal inventory inspection over the EXISTING complete readonly reader.
//! This is neither a public view nor a source-completeness attestation. The
//! trusted assembler still supplies identity, schema and an adopted catalog
//! provider. A local database revision is not an upstream provider version.
use crate::card_selection::{CardSelectionIssue, ReadOnlyCardList};
use sqlx::PgConnection;

/// Cannot be constructed from client JSON, serialized or logged with Debug.
/// Knowing these internal IDs grants no payment/freeze permission. Keep this
/// object in the trusted backend; use ReadOnlyCardList::view for user display.
pub struct ReadOnlyCatalogInspection {
    list: ReadOnlyCardList,
}

impl ReadOnlyCatalogInspection {
    /// The existing display allow-list, from this same complete read. This
    /// does not expose IDs or confer selection/execution authority.
    pub fn view(&self) -> crate::card_selection::CardListView {
        self.list.view()
    }

    /// Borrows independently authorized READ ONLY / READ COMMITTED access.
    /// Reuses full SQL collection, ownership and before/after version checks;
    /// no pool, transaction control, writes, truncation or unknown-to-empty
    /// fallback. Success describes the installed local catalog at this instant,
    /// not proof that all upstream cards were originally supplied.
    pub async fn read_on_connection(
        connection: &mut PgConnection,
        trusted_owner: &str,
        account: &str,
    ) -> Result<Self, CardSelectionIssue> {
        Ok(Self {
            list: ReadOnlyCardList::read_on_connection(connection, trusted_owner, account).await?,
        })
    }

    pub fn owner_id_for_backend(&self) -> &str {
        &self.list.owner
    }

    pub fn account_id_for_backend(&self) -> &str {
        &self.list.facts.account_id
    }

    /// Account-local monotonic catalog/lock revision; NOT a source provenance
    /// receipt, cross-database identity, confirmation binding or permit.
    pub fn local_revision_for_backend(&self) -> &str {
        &self.list.facts.revision
    }

    /// Full exact ID set from this read, including cards with identical tails.
    /// No tail-to-ID lookup or default selection is introduced.
    pub fn card_ids_for_backend(&self) -> impl ExactSizeIterator<Item = &str> {
        self.list
            .facts
            .cards
            .iter()
            .map(|card| card.card_id.as_str())
    }

    pub fn active_lock_ids_for_backend(&self) -> &[String] {
        &self.list.facts.active_card_lock_ids
    }
}
