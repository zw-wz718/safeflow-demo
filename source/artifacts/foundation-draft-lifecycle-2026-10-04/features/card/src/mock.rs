//! Immutable in-memory provider for fictional card data, not a bank adapter.

use crate::service::{CardDataSource, SourceUnavailable};
use crate::Snapshot;

/// Keeps one coherent fictional snapshot across reads. It intentionally may
/// contain different users so the service's ownership checks remain necessary.
/// There is no mutation API, persistence, authentication or bank connectivity.
pub struct MockCardSource {
    snapshot: Snapshot,
}

impl MockCardSource {
    pub fn new(snapshot: Snapshot) -> Self {
        Self { snapshot }
    }
}

impl CardDataSource for MockCardSource {
    fn read_snapshot(
        &self,
        _trusted_user_id: &str,
        _trusted_card_id: &str,
    ) -> Result<Snapshot, SourceUnavailable> {
        // Keep malformed/duplicate/unavailable fixture data intact. This provider
        // must not silently repair it or turn failed lookups into empty success.
        Ok(self.snapshot.clone())
    }
}
