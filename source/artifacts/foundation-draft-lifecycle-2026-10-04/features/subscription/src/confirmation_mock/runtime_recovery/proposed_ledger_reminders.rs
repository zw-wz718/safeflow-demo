//! Shared-table mock reminder workflow. No sender inside a business transaction.
//! Reservation and result each use the caller's existing CAS/commit/readback.
use super::*;

#[derive(Debug, Serialize)]
pub struct DueReminderPreviewMock {
    pub fictional: bool,
    pub reserved: bool,
    pub notification_sent: bool,
    /// Dry-run reservation projection, NOT a saved delivery report. Due entries
    /// appear in messages; projected Unknown does not mean an attempt occurred.
    pub report: DeliveryReport,
    /// Backend preview only, never an authorization or a user-facing raw payload.
    pub messages: Vec<ReminderMessage>,
}

/// Non-cloneable, non-serializable capability, created only by consuming a saved
/// reservation's pending write after independent committed readback.
pub struct CommittedReminderBatchMock {
    namespace: String,
    database_scope: DatabaseScope,
    owner: String,
    journal: Value,
    now: String,
    messages: Vec<ReminderMessage>,
}
/// Fixed mock outcomes tied to the entire original saved reservation. These are
/// not a receipt: result persistence and independent readback are still required.
pub struct SimulatedReminderBatchMock {
    reservation: CommittedReminderBatchMock,
    outcomes: Vec<DeliveryOutcome>,
}

fn preview(snapshot: &SubscriptionReadSnapshotMock, now: &str) -> Result<DueReminderPreviewMock> {
    let mut sink = ReserveSink::default();
    let report = snapshot
        .state
        .reminders
        .with_current_rules_mock()
        .scan_mock(&snapshot.state.ledger, &snapshot.owner, now, &mut sink)
        .map_err(RuntimeRecoveryIssue::Reminder)?;
    Ok(DueReminderPreviewMock {
        fictional: true,
        reserved: false,
        notification_sent: false,
        report,
        messages: sink.messages,
    })
}
impl SubscriptionReadSnapshotMock {
    /// Pure preview: no command, rollover, attempt or last-scan time is saved.
    pub fn due_reminders_mock(&self, now: &str) -> Result<DueReminderPreviewMock> {
        preview(self, now)
    }
}
impl ProposedLedgerWorkMock {
    /// Only a freshly loaded, saved configuration may reserve. No sink is called
    /// and no token escapes before caller commit and independent observation.
    pub async fn reserve_reminders_mock(&mut self, now: &str) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = async {
            if self.runtime.version != self.base_version || self.runtime.state.scan_base.is_some() {
                return Err(RuntimeRecoveryIssue::Conflict);
            }
            let mut sink = ReserveSink::default();
            let report = self
                .runtime
                .state
                .reminders
                .with_current_rules_mock()
                .scan_mock(
                    &self.runtime.state.ledger,
                    &self.runtime.user,
                    now,
                    &mut sink,
                )
                .map_err(RuntimeRecoveryIssue::Reminder)?;
            self.runtime
                .append(Command::ReserveScan {
                    reminder_rules: Some(1),
                    now: now.into(),
                    messages: sink
                        .messages
                        .iter()
                        .map(value)
                        .collect::<Result<Vec<_>>>()?,
                    report: value(&report)?,
                })
                .await
        }
        .await;
        self.finish(before, result)
    }

    /// Does not send, begin/commit, or return an unsaved delivery receipt.
    /// Any intervening write/restart invalidates this batch; its saved Unknown
    /// remains available for review, never converted into an automatic resend.
    pub async fn record_reminder_results_mock(
        &mut self,
        batch: SimulatedReminderBatchMock,
    ) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = async {
            let r = &batch.reservation;
            if self.namespace != r.namespace
                || self.database_scope != r.database_scope
                || self.runtime.user != r.owner
                || self.base_journal != r.journal
                || value(&self.runtime.journal)? != r.journal
            {
                return Err(RuntimeRecoveryIssue::Conflict);
            }
            let (now, base, count) = self
                .runtime
                .state
                .scan_base
                .as_ref()
                .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
            if now != &r.now || *count != batch.outcomes.len() || *count != r.messages.len() {
                return Err(RuntimeRecoveryIssue::Conflict);
            }
            let mut fixed = RecordedSink {
                outcomes: batch.outcomes.clone(),
                index: 0,
            };
            let report = base
                .clone()
                .scan_mock(
                    &self.runtime.state.ledger,
                    &self.runtime.user,
                    now,
                    &mut fixed,
                )
                .map_err(RuntimeRecoveryIssue::Reminder)?;
            if fixed.index != *count {
                return Err(RuntimeRecoveryIssue::InvalidJournal);
            }
            self.runtime
                .append(Command::ResolveScan {
                    now: r.now.clone(),
                    outcomes: batch.outcomes,
                    report: value(&report)?,
                })
                .await
        }
        .await;
        self.finish(before, result)
    }
}
impl PendingProposedLedgerWriteMock {
    /// Consuming self prevents minting two usable mock sender tokens from one
    /// reservation. Rolled-back/uncommitted or same-backend reads cannot mint it.
    pub async fn into_committed_reminder_batch_mock(
        self,
        connection: &mut PgConnection,
    ) -> WriteResult<CommittedReminderBatchMock> {
        if !self.fresh_reminder_reservation {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        let saved = self.read_committed_on_connection_mock(connection).await?;
        let journal = value(&saved.journal)?;
        if journal != self.expected {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        let Some(Command::ReserveScan { now, messages, .. }) = saved.journal.commands.last() else {
            return Err(ProposedLedgerWriteIssue::Conflict);
        };
        let (_, base, _) = saved
            .state
            .scan_base
            .as_ref()
            .ok_or(ProposedLedgerWriteIssue::Conflict)?;
        let mut sink = ReserveSink::default();
        base.clone()
            .scan_mock(&saved.state.ledger, &saved.owner, now, &mut sink)
            .map_err(|e| ProposedLedgerWriteIssue::Business(RuntimeRecoveryIssue::Reminder(e)))?;
        if sink
            .messages
            .iter()
            .map(value)
            .collect::<Result<Vec<_>>>()?
            != *messages
        {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        Ok(CommittedReminderBatchMock {
            namespace: self.namespace,
            database_scope: self.database_scope,
            owner: saved.owner,
            journal,
            now: now.clone(),
            messages: sink.messages,
        })
    }
}
impl CommittedReminderBatchMock {
    /// Fixed in-app simulation ONLY. Revalidate current saved reservation before
    /// any mock call. This is not a distributed send fence or real transport.
    /// Trusted assembly calls this outside its business transaction.
    pub async fn simulate_on_connection_mock(
        self,
        connection: &mut PgConnection,
        sink: &mut impl MockInAppSink,
    ) -> WriteResult<SimulatedReminderBatchMock> {
        if database_scope(connection).await? != self.database_scope {
            return Err(ProposedLedgerWriteIssue::VerificationDatabaseMismatch);
        }
        let saved = ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(connection, &self.owner)
            .await
            .map_err(ProposedLedgerWriteIssue::Read)?;
        if value(&saved.journal)? != self.journal {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        let outcomes = self
            .messages
            .iter()
            .map(|m| sink.simulate_in_app(m))
            .collect();
        Ok(SimulatedReminderBatchMock {
            reservation: self,
            outcomes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    fn fixture() -> SubscriptionReadSnapshotMock {
        let j = super::super::super::raw_records_tests::actual_fixture_journal();
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            j.commands.len() as i64 + 1,
            value(&j).unwrap(),
        )
        .unwrap()
    }
    fn work() -> ProposedLedgerWorkMock {
        let s = fixture();
        ProposedLedgerWorkMock {
            namespace: "fictional_reminders".into(),
            database_scope: ("fictional-db".into(), None, None),
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
        }
    }
    #[test]
    fn due_preview_is_repeatable_and_does_not_change_journal_or_attempts() {
        let s = fixture();
        let before = value(&s.journal).unwrap();
        let a = s.due_reminders_mock(NOW).unwrap();
        let b = s.due_reminders_mock(NOW).unwrap();
        assert_eq!(a.messages.len(), 1);
        assert_eq!(value(&a).unwrap(), value(&b).unwrap());
        assert!(!a.reserved && !a.notification_sent && !a.report.database_connected);
        assert_eq!(before, value(&s.journal).unwrap());
        assert!(s
            .reminders_mock()
            .plan_status_mock(s.ledger_mock(), "user_demo", &a.messages[0].subscription_id)
            .unwrap()
            .saved_cycle_outcomes
            .is_empty());
    }
    #[test]
    fn preview_rejects_invalid_clock_without_changing_saved_state() {
        let s = fixture();
        let before = value(&s.journal).unwrap();
        assert!(s.due_reminders_mock("invalid").is_err());
        assert_eq!(before, value(&s.journal).unwrap());
    }
    #[tokio::test]
    async fn reservation_is_only_a_staged_unknown_not_a_delivery_receipt() {
        let mut w = work();
        w.reserve_reminders_mock(NOW).await.unwrap();
        assert_eq!(w.runtime.version, w.base_version + 1);
        assert!(matches!(
            w.runtime.journal.commands.last(),
            Some(Command::ReserveScan { .. })
        ));
        let s = SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            w.runtime.version,
            value(&w.runtime.journal).unwrap(),
        )
        .unwrap();
        assert!(s.due_reminders_mock(NOW).unwrap().messages.is_empty());
        let id = &s.ledger_mock().list_mock("user_demo").unwrap()[0].subscription_id;
        assert_eq!(
            s.reminder_plan_mock(id).unwrap().saved_cycle_outcomes[0].state,
            crate::reminder_mock::ReminderAttemptStateMock::NeedsReview
        );
    }
    #[tokio::test]
    async fn repeated_reservation_poisons_work_instead_of_duplicating_attempts() {
        let mut w = work();
        w.reserve_reminders_mock(NOW).await.unwrap();
        assert!(w.reserve_reminders_mock(NOW).await.is_err());
        assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
        assert_eq!(w.runtime.version, w.base_version + 1);
    }
    #[tokio::test]
    async fn reservation_rejects_unsaved_configuration_and_preserves_base() {
        let mut w = work();
        let base = w.base_journal.clone();
        w.invalidate_pending_for_restart_mock().await.unwrap();
        assert!(w.reserve_reminders_mock(NOW).await.is_err());
        assert_eq!(w.base_journal, base);
        assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
    }
}
