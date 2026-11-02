//! Direct, typed use of Jonathan's LOCAL reader. Assembly module, not a public Tool.
//! Requires both crates in the host; no identity issuance, database write or sender.
use safeflow_billing_source_reader::{query as billing, TrustedScope};
use safeflow_subscription_evidence::{
    confirmation_mock::{
        ProposedLedgerStoreMock, ProposedLedgerWriteIssue, SavedDiscoverySelectionMock,
        SharedBusinessReadQueryMock, SharedSubscriptionReadQueryMock,
    },
    MockContext, ReadKind, ReadState, Snapshot,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgConnection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DirectReaderIssueMock {
    SourceUnavailable,
    SourceIncomplete,
    ScopeUnavailable,
    InvalidInput,
    NotFictionalSource,
    LedgerUnavailable,
}
/// Opaque collected facts. Constructed only by calling the actual reader, not JSON.
/// No Clone/Deserialize; consumption is read afresh for each identify invocation.
pub struct CollectedBillingEvidenceMock {
    report: billing::Report,
    context: MockContext,
    snapshot: Snapshot,
}
fn canonical_amount(raw: &str) -> Result<String, DirectReaderIssueMock> {
    let (whole, fraction) = raw
        .split_once('.')
        .ok_or(DirectReaderIssueMock::InvalidInput)?;
    if fraction.len() != 2
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err(DirectReaderIssueMock::InvalidInput);
    }
    let cents = whole
        .parse::<u64>()
        .ok()
        .and_then(|w| w.checked_mul(100))
        .and_then(|w| fraction.parse::<u64>().ok().and_then(|f| w.checked_add(f)))
        .filter(|c| *c > 0)
        .ok_or(DirectReaderIssueMock::InvalidInput)?;
    Ok(format!("{}.{:02}", cents / 100, cents % 100))
}
pub fn collect_from_billing_mock(
    scope: &TrustedScope,
    request: &billing::Request,
    source: &impl billing::PagedSource,
    limits: billing::Limits,
) -> Result<CollectedBillingEvidenceMock, DirectReaderIssueMock> {
    // Scope constructor/source assertions are NOT production authentication.
    if !source.source_id().starts_with("fictional-") {
        return Err(DirectReaderIssueMock::NotFictionalSource);
    }
    collect_reviewed_facts(scope, request, source, limits)
}
/// Assembly-only bridge. The unforgeable reader is issued solely by the native
/// fixed-digest team SQL source, never a client Page or an arbitrary source ID.
pub(crate) fn collect_from_adopted_team_capture(
    scope: &TrustedScope,
    request: &billing::Request,
    source: &crate::team_consumption::TeamCaptureReader,
    limits: billing::Limits,
) -> Result<CollectedBillingEvidenceMock, DirectReaderIssueMock> {
    collect_reviewed_facts(scope, request, source, limits)
}
fn collect_reviewed_facts(
    scope: &TrustedScope,
    request: &billing::Request,
    source: &impl billing::PagedSource,
    limits: billing::Limits,
) -> Result<CollectedBillingEvidenceMock, DirectReaderIssueMock> {
    if billing::local_four_month_window(&request.window.as_of).as_ref() != Some(&request.window) {
        return Err(DirectReaderIssueMock::InvalidInput);
    }
    let report = billing::collect(scope, request, source, limits);
    match report.state {
        billing::ReadState::Complete if report.is_complete => {}
        billing::ReadState::DataIncomplete => return Err(DirectReaderIssueMock::SourceIncomplete),
        billing::ReadState::LookupUnavailable => {
            return Err(DirectReaderIssueMock::SourceUnavailable)
        }
        billing::ReadState::ResourceUnavailable => {
            return Err(DirectReaderIssueMock::ScopeUnavailable)
        }
        _ => return Err(DirectReaderIssueMock::InvalidInput),
    }
    if report.source_id != source.source_id()
        || report.next_cursor.is_some()
        || report.snapshot_id.as_deref().is_none_or(str::is_empty)
        || report.pages_read == 0
    {
        return Err(DirectReaderIssueMock::InvalidInput);
    }
    let context = MockContext {
        user_id: scope.user_id().into(),
        requested_account_ids: request.account_ids.clone(),
    };
    let mut transactions = Vec::new();
    // Replay all observed rows through subscription filtering, not only a supplied
    // eligible subset. Report retains original amounts and excluded evidence.
    for row in &report.observed_transactions {
        let value = json!({"source_id":report.source_id,"transaction_id":row.transaction_id,"account_id":row.account_id,
            "card_id":row.card_id,"amount":canonical_amount(row.amount.as_str())?,"currency":row.currency,
            "direction":row.direction,"transaction_type":row.transaction_type,"status":row.status,
            "completed_at":row.completed_at,"merchant_id":row.merchant_id,"mock_service_ref":row.service_ref,"category_code":row.category_code});
        transactions
            .push(serde_json::from_value(value).map_err(|_| DirectReaderIssueMock::InvalidInput)?);
    }
    let snapshot = Snapshot {
        window: serde_json::from_value(
            serde_json::to_value(&request.window)
                .map_err(|_| DirectReaderIssueMock::InvalidInput)?,
        )
        .map_err(|_| DirectReaderIssueMock::InvalidInput)?,
        accounts: request
            .account_ids
            .iter()
            .map(|id| safeflow_subscription_evidence::Account {
                account_id: id.clone(),
                user_id: scope.user_id().into(),
            })
            .collect(),
        transactions_read: ReadState {
            state: ReadKind::Complete,
            is_complete: true,
            next_cursor: None,
            reason: None,
        },
        managed_subscriptions_read: ReadState {
            state: ReadKind::Unavailable,
            is_complete: false,
            next_cursor: None,
            reason: Some("LOAD_CURRENT_SAVED_LEDGER_SEPARATELY".into()),
        },
        transactions,
    };
    Ok(CollectedBillingEvidenceMock {
        report,
        context,
        snapshot,
    })
}
pub async fn identify_from_billing_reader_mock(
    store: &ProposedLedgerStoreMock,
    reader: &mut PgConnection,
    scope: &TrustedScope,
    request: &billing::Request,
    source: &impl billing::PagedSource,
    limits: billing::Limits,
) -> Result<Value, DirectReaderIssueMock> {
    collect_from_billing_mock(scope, request, source, limits)?
        .identify_on_connection_mock(store, reader)
        .await
}
impl CollectedBillingEvidenceMock {
    /// Internal display identity from the same captured facts and owner ledger.
    /// This is not serialized or used to authorize registration.
    pub(crate) fn display_identities(
        &self,
        ledger: &safeflow_subscription_evidence::confirmation_mock::MemoryConfirmationMock,
    ) -> Result<Vec<(String, Option<String>)>, DirectReaderIssueMock> {
        let review = ledger.discover_mock(&self.context, &self.snapshot)
            .map_err(|_| DirectReaderIssueMock::LedgerUnavailable)?;
        Ok(review.groups.into_iter().map(|g| (g.group.merchant_id, g.group.mock_service_ref)).collect())
    }
    /// Consume this capture exactly once into a trusted backend callback.
    /// The original typed context/snapshot are MOVED, never reconstructed from
    /// a safe view or collected again. R may be an async future owning the facts.
    /// This is evidence handoff, not a public permit, identity or nonce issuer.
    pub fn consume_registration_source_mock<R>(
        self,
        index: usize,
        backend: impl FnOnce(MockContext, Snapshot, usize) -> R,
    ) -> R {
        backend(self.context, self.snapshot, index)
    }
    pub fn read_summary_mock(&self) -> Value {
        json!({"fictional":true,"production_registered":false,"pages_read":self.report.pages_read,
            "observed_rows":self.report.observed_transactions.len(),"duplicates_ignored":self.report.duplicates_ignored,
            "real_consumption_source_connected":false})
    }
    pub async fn identify_on_connection_mock(
        self,
        store: &ProposedLedgerStoreMock,
        reader: &mut PgConnection,
    ) -> Result<Value, DirectReaderIssueMock> {
        self.review_on_connection_mock(store, reader).await
    }
    /// Read-only review of these same collected facts before explicit selection.
    pub async fn review_on_connection_mock(
        &self,
        store: &ProposedLedgerStoreMock,
        reader: &mut PgConnection,
    ) -> Result<Value, DirectReaderIssueMock> {
        let summary = self.read_summary_mock();
        let review = store
            .query_on_connection_mock(
                reader,
                &self.context.user_id,
                &SharedSubscriptionReadQueryMock::Business(
                    SharedBusinessReadQueryMock::IdentifySubscriptions {
                        context: self.context.clone(),
                        input: Box::new(self.snapshot.clone()),
                    },
                ),
            )
            .await
            .map_err(|_| DirectReaderIssueMock::LedgerUnavailable)?;
        Ok(json!({"read":summary,"recognition":review}))
    }
    /// Explicit backend choice, not registration or confirmation authority.
    /// Existing selection code binds the exact current ledger and source facts.
    pub async fn select_on_connection_mock(
        &self,
        store: &ProposedLedgerStoreMock,
        reader: &mut PgConnection,
        index: usize,
        now: &str,
    ) -> Result<SavedDiscoverySelectionMock, ProposedLedgerWriteIssue> {
        store
            .select_discovery_candidate_on_connection_mock(
                reader,
                &self.context.user_id,
                &self.context,
                &self.snapshot,
                index,
                now,
            )
            .await
    }
    /// Date evidence stays coupled to the collected rows; missing dates are not guessed.
    pub fn suggest_date_mock(
        &self,
        saved: &safeflow_subscription_evidence::confirmation_mock::SubscriptionReadSnapshotMock,
        subscription_id: &str,
        now: &str,
    ) -> Result<
        safeflow_subscription_evidence::confirmation_mock::DateSuggestionViewMock,
        safeflow_subscription_evidence::confirmation_mock::RuntimeRecoveryIssue,
    > {
        saved.suggest_reminder_date_mock(
            &self.context,
            &self.snapshot,
            None,
            Some(subscription_id),
            now,
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing_fixture::fixture;
    #[test]
    fn consuming_callback_gets_original_all_rows_and_index_without_second_collect() {
        use std::cell::Cell;
        struct Counted<S> {
            source: S,
            calls: Cell<usize>,
        }
        impl<S: billing::PagedSource> billing::PagedSource for Counted<S> {
            fn source_id(&self) -> &str {
                self.source.source_id()
            }
            fn read_page(
                &self,
                s: &TrustedScope,
                r: &billing::Request,
                c: Option<&str>,
                v: Option<&str>,
            ) -> Result<billing::Page, billing::PageError> {
                self.calls.set(self.calls.get() + 1);
                self.source.read_page(s, r, c, v)
            }
        }
        let (scope, request, source) = fixture("complete").unwrap();
        let source = Counted {
            source,
            calls: Cell::new(0),
        };
        let capture =
            collect_from_billing_mock(&scope, &request, &source, billing::Limits::default())
                .unwrap();
        let expected_context = serde_json::to_value(&capture.context).unwrap();
        let expected_snapshot = serde_json::to_value(&capture.snapshot).unwrap();
        let calls = Cell::new(0);
        let got = capture.consume_registration_source_mock(17, |context, snapshot, index| {
            calls.set(calls.get() + 1);
            (
                serde_json::to_value(context).unwrap(),
                serde_json::to_value(snapshot).unwrap(),
                index,
            )
        });
        assert_eq!(got, (expected_context, expected_snapshot, 17));
        assert_eq!(calls.get(), 1);
        assert_eq!(source.calls.get(), 2);
    }
    #[test]
    fn failed_collection_cannot_enter_callback() {
        let (scope, request, source) = fixture("complete").unwrap();
        let calls = std::cell::Cell::new(0);
        let result = collect_from_billing_mock(
            &scope,
            &request,
            &source,
            billing::Limits {
                max_pages: 1,
                max_rows: 100,
            },
        )
        .map(|capture| {
            capture.consume_registration_source_mock(0, |_, _, _| calls.set(calls.get() + 1))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 0);
    }
    #[test]
    fn actual_reader_pages_feed_snapshot_and_keep_unknown_ledger() {
        let (scope, request, source) = fixture("complete").unwrap();
        let e = collect_from_billing_mock(&scope, &request, &source, billing::Limits::default())
            .unwrap();
        assert!(e.report.pages_read > 1);
        assert_eq!(
            e.snapshot.transactions.len(),
            e.report.observed_transactions.len()
        );
        assert_eq!(
            e.snapshot.managed_subscriptions_read.state,
            ReadKind::Unavailable
        );
        assert!(e
            .snapshot
            .transactions
            .iter()
            .any(|t| t.transaction_type
                != safeflow_subscription_evidence::TransactionType::Consumption));
        assert_eq!(e.context.user_id, scope.user_id());
    }
    #[test]
    fn incomplete_or_failed_read_never_yields_partial_consumption() {
        let (_, request, source) = fixture("complete").unwrap();
        let scope = TrustedScope::from_verified_context("user_demo", ["account_001"]).unwrap();
        assert!(matches!(
            collect_from_billing_mock(
                &scope,
                &request,
                &source,
                billing::Limits {
                    max_pages: 1,
                    max_rows: 100
                }
            ),
            Err(DirectReaderIssueMock::SourceIncomplete)
        ));
        let source = crate::billing_fixture::FailingSource;
        assert!(matches!(
            collect_from_billing_mock(&scope, &request, &source, billing::Limits::default()),
            Err(DirectReaderIssueMock::SourceUnavailable)
        ));
    }
    #[test]
    fn missing_category_stays_absent_and_reader_rejections_are_not_bypassed() {
        let (scope, request, mut source) = fixture("complete").unwrap();
        let first = source.steps[0]["page"]["items"][0].as_object_mut().unwrap();
        first.remove("category_code");
        let e = collect_from_billing_mock(&scope, &request, &source, billing::Limits::default())
            .unwrap();
        assert_eq!(e.snapshot.transactions[0].category_code, None);
        // The direct reader rejects this format; never bypass its validation.
        source.steps[0]["page"]["items"][0]["amount"] = json!("0019.90");
        assert!(matches!(
            collect_from_billing_mock(&scope, &request, &source, billing::Limits::default()),
            Err(DirectReaderIssueMock::InvalidInput)
        ));
    }
    #[test]
    fn cross_account_same_transaction_number_is_not_a_duplicate_or_global_id_error() {
        let (scope, mut request, mut source) = fixture("complete").unwrap();
        let _ = scope;
        request.account_ids.push("account_002".into());
        let scope =
            TrustedScope::from_verified_context("user_demo", ["account_001", "account_002"])
                .unwrap();
        for step in &mut source.steps {
            if let Some(page) = step.get_mut("page") {
                page["account_ids"] = json!(request.account_ids);
            }
        }
        let mut duplicate = source.steps[0]["page"]["items"][0].clone();
        duplicate["account_id"] = json!("account_002");
        source.steps[0]["page"]["items"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        let last = source.steps.last_mut().unwrap();
        let total = last["page"]["total_unique_rows"].as_u64().unwrap();
        last["page"]["total_unique_rows"] = json!(total + 1);
        let e = collect_from_billing_mock(&scope, &request, &source, billing::Limits::default())
            .unwrap();
        let a = &e.snapshot.transactions[0];
        let b = e
            .snapshot
            .transactions
            .iter()
            .find(|t| t.account_id == "account_002")
            .unwrap();
        assert_eq!(a.transaction_id, b.transaction_id);
        assert_ne!(a.account_id, b.account_id);
    }
    #[test]
    fn snapshot_owner_or_page_change_cannot_become_complete() {
        let (scope, request, mut source) = fixture("complete").unwrap();
        source.steps[1]["page"]["snapshot_id"] = json!("changed-snapshot");
        assert!(matches!(
            collect_from_billing_mock(&scope, &request, &source, billing::Limits::default()),
            Err(DirectReaderIssueMock::SourceIncomplete)
        ));
        let (scope, request, mut source) = fixture("complete").unwrap();
        source.steps[0]["page"]["owner_user_id"] = json!("other-owner");
        assert!(matches!(
            collect_from_billing_mock(&scope, &request, &source, billing::Limits::default()),
            Err(DirectReaderIssueMock::ScopeUnavailable)
        ));
    }
}
