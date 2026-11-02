//! Explicit backend candidate selection, then provisional registration preparation.
//! No confirmation gate, registration, transaction management or notification.
use super::*;
use crate::managed_ledger_mock::Disposition;

/// Opaque backend selection, not serializable/deserializable or authorization.
/// The source snapshot and exact store scope cannot be replaced by a JSON index.
pub struct SavedDiscoverySelectionMock {
    namespace: String,
    database_scope: DatabaseScope,
    owner: String,
    version: i64,
    journal: Value,
    context: MockContext,
    input: Snapshot,
    index: usize,
}
#[derive(Debug, Serialize)]
pub struct PreparedDiscoveryRegistrationMock {
    pub fictional: bool,
    pub state: &'static str,
    pub saved: bool,
    pub registered: bool,
    pub preview: RegistrationPreviewMock,
    /// Existing exact backend binding; never included in human/model JSON.
    #[serde(skip)]
    pub confirmation_binding: MockConfirmationBinding,
}
fn eligible(
    s: &SubscriptionReadSnapshotMock,
    context: &MockContext,
    input: &Snapshot,
    index: usize,
    now: &str,
) -> Result<()> {
    let parse = |v| {
        chrono::DateTime::parse_from_rfc3339(v)
            .map_err(|_| RuntimeRecoveryIssue::Business(MockIssue::InvalidContext))
    };
    if parse(now)? < parse(&input.window.as_of)? {
        return Err(RuntimeRecoveryIssue::Business(MockIssue::InvalidContext));
    }
    let view = s.identify_subscriptions_mock(context, input)?;
    if !view.matching_executed
        || !view
            .groups
            .get(index)
            .is_some_and(|g| g.disposition == Disposition::NotInLedger)
    {
        return Err(RuntimeRecoveryIssue::Business(MockIssue::NeedsInput));
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn selected(
    namespace: &str,
    scope: DatabaseScope,
    s: &SubscriptionReadSnapshotMock,
    context: &MockContext,
    input: &Snapshot,
    index: usize,
    now: &str,
) -> WriteResult<SavedDiscoverySelectionMock> {
    eligible(s, context, input, index, now)?;
    Ok(SavedDiscoverySelectionMock {
        namespace: namespace.into(),
        database_scope: scope,
        owner: s.owner.clone(),
        version: s.version,
        journal: value(&s.journal)?,
        context: context.clone(),
        input: input.clone(),
        index,
    })
}
impl ProposedLedgerStoreMock {
    /// Trusted caller controls committed source reads and identity. This captures
    /// only existing facts; neither creates a draft nor proves public permission.
    pub async fn select_discovery_candidate_on_connection_mock(
        &self,
        c: &mut PgConnection,
        owner: &str,
        context: &MockContext,
        input: &Snapshot,
        index: usize,
        now: &str,
    ) -> WriteResult<SavedDiscoverySelectionMock> {
        let s = ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(&mut *c, owner)
            .await
            .map_err(ProposedLedgerWriteIssue::Read)?;
        selected(
            &self.namespace,
            database_scope(c).await?,
            &s,
            context,
            input,
            index,
            now,
        )
    }
}
impl ProposedLedgerWorkMock {
    /// No new ledger entry is registered. The caller still saves/commits/reads
    /// the draft independently, then asks for separate explicit confirmation.
    pub async fn prepare_registration_from_selection_mock(
        &mut self,
        selection: SavedDiscoverySelectionMock,
        operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> WriteResult<PreparedDiscoveryRegistrationMock> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = async {
            if self.namespace != selection.namespace
                || self.database_scope != selection.database_scope
                || self.runtime.user != selection.owner
                || self.base_version != selection.version
                || self.base_journal != selection.journal
                || value(&self.runtime.journal)? != selection.journal
            {
                return Err(RuntimeRecoveryIssue::Conflict);
            }
            let parse = |v| {
                chrono::DateTime::parse_from_rfc3339(v)
                    .map_err(|_| RuntimeRecoveryIssue::Business(MockIssue::InvalidContext))
            };
            if parse(expires)? <= parse(now)? {
                return Err(RuntimeRecoveryIssue::Business(MockIssue::DraftExpired));
            }
            let s = SubscriptionReadSnapshotMock::from_stored_journal_mock(
                &self.runtime.user,
                self.runtime.version,
                value(&self.runtime.journal)?,
            )?;
            eligible(
                &s,
                &selection.context,
                &selection.input,
                selection.index,
                now,
            )?;
            let binding = self
                .runtime
                .prepare_registration_mock(
                    &selection.context,
                    &selection.input,
                    operation,
                    selection.index,
                    expires,
                )
                .await?;
            let preview =
                self.runtime
                    .registration_preview_mock(&binding.draft_id, &binding.context, now)?;
            Ok(PreparedDiscoveryRegistrationMock {
                fictional: true,
                state: "AWAITING_CONFIRMATION",
                saved: false,
                registered: false,
                preview,
                confirmation_binding: binding,
            })
        }
        .await;
        self.finish(before, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    const END: &str = "2026-10-14T12:05:00+08:00";
    fn sample() -> crate::Sample {
        let v: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/discovery_ready_input.v1.json"
        )))
        .unwrap();
        serde_json::from_value(v["sample"].clone()).unwrap()
    }
    fn setup() -> (ProposedLedgerWorkMock, SavedDiscoverySelectionMock) {
        let f = sample();
        let mut j = super::super::super::raw_records_tests::actual_fixture_journal();
        j.commands.clear();
        let s = SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            1,
            value(&j).unwrap(),
        )
        .unwrap();
        let scope = ("fictional-db".into(), None, None);
        let namespace = "fictional_selection";
        let selection = selected(
            namespace,
            scope.clone(),
            &s,
            &f.trusted_context_fixture,
            &f.snapshot,
            0,
            NOW,
        )
        .unwrap();
        let w = ProposedLedgerWorkMock {
            namespace: namespace.into(),
            database_scope: scope,
            base_version: s.version,
            base_journal: value(&s.journal).unwrap(),
            poisoned: false,
            runtime: SubscriptionRuntimeMock {
                pool: None,
                user: s.owner,
                version: s.version,
                journal: s.journal,
                state: s.state,
                staged: true,
            },
        };
        (w, selection)
    }
    fn op() -> MockOperationContext {
        MockOperationContext {
            user_id: "user_demo".into(),
            session_ref: "fictional-session".into(),
            task_id: "task".into(),
            node_id: "node".into(),
            operation_id: "selected-registration".into(),
            idempotency_key: "selected-key".into(),
            plan_version: 1,
        }
    }
    #[tokio::test]
    async fn selected_candidate_only_prepares_a_draft_and_safe_provisional_summary() {
        let (mut w, s) = setup();
        let got = w
            .prepare_registration_from_selection_mock(s, op(), NOW, END)
            .await
            .unwrap();
        assert!(!got.saved && !got.registered);
        assert!(w
            .runtime
            .ledger_mock()
            .list_mock("user_demo")
            .unwrap()
            .is_empty());
        let v = value(&got).unwrap();
        for hidden in [
            "confirmation_binding",
            "session_ref",
            "idempotency_key",
            "selected-key",
            "user_demo",
            "account_001",
        ] {
            assert!(!v.to_string().contains(hidden));
        }
        assert_eq!(w.runtime.journal.commands.len(), 1);
        assert!(matches!(
            w.runtime.journal.commands[0],
            Command::PrepareRegistration { .. }
        ));
    }
    #[tokio::test]
    async fn scope_mismatch_or_changed_saved_facts_reject_old_selection() {
        for change in 0..4 {
            let (mut w, mut s) = setup();
            match change {
                0 => s.namespace = "other".into(),
                1 => s.database_scope.0 = "other-db".into(),
                2 => s.owner = "other".into(),
                _ => s.version += 1,
            };
            assert!(w
                .prepare_registration_from_selection_mock(s, op(), NOW, END)
                .await
                .is_err());
            assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
            assert!(w.runtime.journal.commands.is_empty());
        }
    }
    #[tokio::test]
    async fn intervening_unsaved_changes_make_original_selection_stale() {
        let (mut w, s) = setup();
        w.invalidate_pending_for_restart_mock().await.unwrap();
        assert!(w
            .prepare_registration_from_selection_mock(s, op(), NOW, END)
            .await
            .is_err());
        assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
    }
    #[tokio::test]
    async fn foreign_operation_and_expired_time_do_not_prepare_or_register() {
        for foreign in [true, false] {
            let (mut w, s) = setup();
            let mut operation = op();
            if foreign {
                operation.user_id = "other".into();
            }
            let expires = if foreign { END } else { NOW };
            assert!(w
                .prepare_registration_from_selection_mock(s, operation, NOW, expires)
                .await
                .is_err());
            assert!(w.runtime.journal.commands.is_empty());
            assert!(w
                .runtime
                .ledger_mock()
                .list_mock("user_demo")
                .unwrap()
                .is_empty());
        }
    }
    #[test]
    fn selection_requires_complete_consumption_and_current_owner() {
        let (w, _) = setup();
        let f = sample();
        let s = SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            w.base_version,
            w.base_journal,
        )
        .unwrap();
        let mut input = f.snapshot.clone();
        input.transactions_read.is_complete = false;
        assert!(selected(
            "fictional_selection",
            w.database_scope.clone(),
            &s,
            &f.trusted_context_fixture,
            &input,
            0,
            NOW
        )
        .is_err());
        let mut context = f.trusted_context_fixture;
        context.user_id = "other".into();
        assert!(selected(
            "fictional_selection",
            w.database_scope,
            &s,
            &context,
            &f.snapshot,
            0,
            NOW
        )
        .is_err());
    }
}
