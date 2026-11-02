//! Fictional shared-table write adapter. Caller owns connection and transaction.
//! No pool creation, DDL, permission issuance, sender or public registration.
use super::*;

#[path = "proposed_ledger_initialization.rs"]
mod initialization;
pub use initialization::{LedgerInitializationPreviewMock, LedgerInitializationRequestMock};
#[path = "shared_subscription_queries.rs"]
mod shared_queries;
pub use shared_queries::{
    shared_query_error_view_mock, SharedBusinessReadQueryMock, SharedSubscriptionReadQueryMock,
};
#[path = "discovery_selection.rs"]
mod discovery_selection;
pub use discovery_selection::{PreparedDiscoveryRegistrationMock, SavedDiscoverySelectionMock};

#[path = "proposed_ledger_reminders.rs"]
mod reminders;
pub use reminders::{
    CommittedReminderBatchMock, DueReminderPreviewMock, SimulatedReminderBatchMock,
};

#[path = "subscription_changes.rs"]
mod subscription_changes;
pub use subscription_changes::{
    PreparedSubscriptionChangeMock, SubscriptionChangeBindingMock, SubscriptionChangeRequestMock,
};

#[path = "shared_recovery_handoff.rs"]
mod shared_recovery;
pub use shared_recovery::{
    SharedBusinessRecordCursorMock, SharedBusinessRecordPageMock,
    SharedSubscriptionWriteHandoffMock, SharedWriteCompletionViewMock,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposedLedgerWriteIssue {
    Read(ProposedLedgerReadIssue),
    Business(RuntimeRecoveryIssue),
    Unavailable,
    Conflict,
    Poisoned,
    VerificationConnectionRequired,
    VerificationDatabaseMismatch,
    CommitNotObserved,
}
impl std::fmt::Display for ProposedLedgerWriteIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProposedLedgerWriteIssue {}
type WriteResult<T> = std::result::Result<T, ProposedLedgerWriteIssue>;
impl From<RuntimeRecoveryIssue> for ProposedLedgerWriteIssue {
    fn from(issue: RuntimeRecoveryIssue) -> Self {
        Self::Business(issue)
    }
}

pub struct ProposedLedgerStoreMock {
    namespace: String,
}
/// Private in-memory work only: preparation bindings are provisional, and
/// confirmation methods return no successful receipt before committed readback.
pub struct ProposedLedgerWorkMock {
    namespace: String,
    database_scope: DatabaseScope,
    base_version: i64,
    base_journal: Value,
    runtime: SubscriptionRuntimeMock,
    poisoned: bool,
}
/// An UPDATE is not a commit. Dropping this object does not commit or publish.
#[must_use]
pub struct PendingProposedLedgerWriteMock {
    namespace: String,
    database_scope: DatabaseScope,
    owner: String,
    writer_pid: i32,
    expected: Value,
    fresh_reminder_reservation: bool,
}
type DatabaseScope = (String, Option<String>, Option<i32>);
async fn database_scope(connection: &mut PgConnection) -> WriteResult<DatabaseScope> {
    sqlx::query_as("SELECT current_database(),inet_server_addr()::text,inet_server_port()")
        .fetch_one(connection)
        .await
        .map_err(|_| ProposedLedgerWriteIssue::Unavailable)
}
impl ProposedLedgerStoreMock {
    pub fn new(namespace: &str) -> WriteResult<Self> {
        ProposedLedgerReaderMock::new(namespace).map_err(ProposedLedgerWriteIssue::Read)?;
        Ok(Self {
            namespace: namespace.into(),
        })
    }
    /// Missing owner, unknown encoding or incomplete data never becomes empty.
    pub async fn load_work_on_connection_mock(
        &self,
        connection: &mut PgConnection,
        trusted_owner: &str,
    ) -> WriteResult<ProposedLedgerWorkMock> {
        let saved = ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(connection, trusted_owner)
            .await
            .map_err(ProposedLedgerWriteIssue::Read)?;
        Ok(ProposedLedgerWorkMock {
            namespace: self.namespace.clone(),
            database_scope: database_scope(connection).await?,
            base_version: saved.version,
            base_journal: value(&saved.journal)?,
            poisoned: false,
            runtime: SubscriptionRuntimeMock {
                pool: None,
                user: saved.owner,
                version: saved.version,
                journal: saved.journal,
                state: saved.state,
                staged: true,
            },
        })
    }
    /// Consumes the work so it cannot be saved twice. Does not begin/end a
    /// transaction. Caller MUST roll back its transaction after any failure.
    pub async fn save_on_connection_mock(
        &self,
        connection: &mut PgConnection,
        work: ProposedLedgerWorkMock,
    ) -> WriteResult<PendingProposedLedgerWriteMock> {
        work.ready()?;
        if work.namespace != self.namespace {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        if database_scope(connection).await? != work.database_scope {
            return Err(ProposedLedgerWriteIssue::VerificationDatabaseMismatch);
        }
        let expected = value(&work.runtime.journal)?;
        // Replay validation is still mandatory, even for locally constructed work.
        decode_journal(&work.runtime.user, work.runtime.version, expected.clone())?;
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| ProposedLedgerWriteIssue::Unavailable)?;
        let query = format!("UPDATE \"{}\".subscription_runtime_records SET storage_version=$1,journal=$2 WHERE owner_id=$3 AND storage_version=$4 AND encoding_version=$5 AND journal=$6",self.namespace);
        let changed = sqlx::query(&query)
            .bind(work.runtime.version)
            .bind(&expected)
            .bind(&work.runtime.user)
            .bind(work.base_version)
            .bind(FICTIONAL_JOURNAL_ENCODING_V1)
            .bind(work.base_journal)
            .execute(&mut *connection)
            .await
            .map_err(|_| ProposedLedgerWriteIssue::Unavailable)?
            .rows_affected();
        if changed != 1 {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        Ok(PendingProposedLedgerWriteMock {
            namespace: self.namespace.clone(),
            database_scope: work.database_scope,
            owner: work.runtime.user,
            writer_pid,
            expected,
            fresh_reminder_reservation: work.base_version.checked_add(1)
                == Some(work.runtime.version)
                && matches!(
                    work.runtime.journal.commands.last(),
                    Some(Command::ReserveScan { .. })
                ),
        })
    }
}
impl ProposedLedgerWorkMock {
    fn ready(&self) -> WriteResult<()> {
        if self.poisoned {
            Err(ProposedLedgerWriteIssue::Poisoned)
        } else {
            Ok(())
        }
    }
    fn finish<T>(&mut self, before: usize, outcome: Result<T>) -> WriteResult<T> {
        match outcome {
            Err(issue) => {
                self.poisoned = true;
                Err(issue.into())
            }
            Ok(output) => {
                // Exact replays are validated by the adopted business method;
                // they must not create extra command/event positions or versions.
                if let Some(last) = self.runtime.journal.commands.get(before) {
                    let duplicate =
                        self.runtime.journal.commands[..before].iter().any(|old| {
                            match (old, last) {
                                (
                                    Command::Register {
                                        draft: a,
                                        context: b,
                                        receipt: c,
                                        ..
                                    },
                                    Command::Register {
                                        draft: x,
                                        context: y,
                                        receipt: z,
                                        ..
                                    },
                                )
                                | (
                                    Command::Intent {
                                        draft: a,
                                        context: b,
                                        receipt: c,
                                        ..
                                    },
                                    Command::Intent {
                                        draft: x,
                                        context: y,
                                        receipt: z,
                                        ..
                                    },
                                ) => a == x && b == y && c == z,
                                (
                                    Command::Settings {
                                        binding: a,
                                        receipt: b,
                                        ..
                                    },
                                    Command::Settings {
                                        binding: x,
                                        receipt: y,
                                        ..
                                    },
                                ) => a == x && b == y,
                                _ => false,
                            }
                        });
                    if duplicate {
                        self.runtime.journal.commands.truncate(before);
                        self.runtime.version -= 1;
                        let (_, state) = decode_journal(
                            &self.runtime.user,
                            self.runtime.version,
                            value(&self.runtime.journal)?,
                        )?;
                        self.runtime.state = state;
                    }
                }
                Ok(output)
            }
        }
    }
    #[allow(clippy::too_many_arguments)] // Full backend evidence and operation binding stay explicit.
    pub async fn prepare_registration_mock(
        &mut self,
        context: &MockContext,
        input: &Snapshot,
        operation: MockOperationContext,
        index: usize,
        expires: &str,
    ) -> WriteResult<MockConfirmationBinding> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .prepare_registration_mock(context, input, operation, index, expires)
            .await;
        self.finish(before, result)
    }
    pub async fn confirm_registration_mock(
        &mut self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockGateVerifier,
    ) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .register_mock(draft, context, now, gate)
            .await
            .map(|_| ());
        self.finish(before, result)
    }
    pub async fn prepare_intent_mock(
        &mut self,
        context: MockOperationContext,
        id: &str,
        intent: MockIntentKind,
        now: &str,
        expires: &str,
    ) -> WriteResult<MockIntentBinding> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .prepare_intent_mock(context, id, intent, now, expires)
            .await;
        self.finish(before, result)
    }
    pub async fn confirm_intent_mock(
        &mut self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
        gate: &mut impl MockIntentGateVerifier,
    ) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .commit_intent_mock(draft, context, now, gate)
            .await
            .map(|_| ());
        self.finish(before, result)
    }
    pub async fn prepare_settings_mock(
        &mut self,
        context: MockOperationContext,
        id: &str,
        schedule: Schedule,
        now: &str,
        expires: &str,
    ) -> WriteResult<ScheduleBinding> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .prepare_settings_mock(context, id, schedule, now, expires)
            .await;
        self.finish(before, result)
    }
    pub async fn confirm_settings_mock(
        &mut self,
        binding: &ScheduleBinding,
        now: &str,
        gate: &mut impl MockScheduleGate,
    ) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .confirm_settings_mock(binding, now, gate)
            .await
            .map(|_| ());
        self.finish(before, result)
    }
    /// Discard only an unexecuted draft. No successful control receipt leaves
    /// staged memory; query the pending list after committed readback instead.
    pub async fn abandon_pending_mock(
        &mut self,
        position: usize,
        context: &MockOperationContext,
        now: &str,
    ) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self
            .runtime
            .abandon_pending_mock(position, context, now)
            .await
            .map(|_| ());
        self.finish(before, result)
    }
    fn original_in_base(&self, binding: &MockConfirmationBinding) -> Result<OriginalResultMock> {
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            &self.runtime.user,
            self.base_version,
            self.base_journal.clone(),
        )?
        .original_result_mock(&OriginalOperationRequestMock::registration(binding)?)
    }
    #[allow(clippy::too_many_arguments)] // Original binding, new evidence/selection, operation and deadlines.
    pub async fn reselect_registration_mock(
        &mut self,
        original: &MockConfirmationBinding,
        context: &MockContext,
        input: &Snapshot,
        index: usize,
        next_operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> WriteResult<ReselectionResultMock> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = async {
            let working = SubscriptionReadSnapshotMock::from_stored_journal_mock(
                &self.runtime.user,
                self.runtime.version,
                value(&self.runtime.journal)?,
            )?
            .original_result_mock(&OriginalOperationRequestMock::registration(original)?)?;
            // A registration completed only inside this unsaved work must not
            // be described as an already-saved A. Read back A first.
            if matches!(working, OriginalResultMock::Recorded { .. })
                && matches!(
                    self.original_in_base(original)?,
                    OriginalResultMock::NeedsReview
                )
            {
                return Err(RuntimeRecoveryIssue::CommitNotObserved);
            }
            self.runtime
                .reselect_registration_mock(
                    original,
                    context,
                    input,
                    index,
                    next_operation,
                    now,
                    expires,
                )
                .await
        }
        .await;
        self.finish(before, result)
    }
    #[allow(clippy::too_many_arguments)] // Full original registration and independent reminder confirmation scope.
    pub async fn prepare_reminder_after_registration_mock(
        &mut self,
        context: &MockContext,
        input: &Snapshot,
        registration: &MockConfirmationBinding,
        operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> WriteResult<ReminderPreparationMock> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = async {
            if matches!(
                self.original_in_base(registration)?,
                OriginalResultMock::NeedsReview
            ) {
                return Err(RuntimeRecoveryIssue::CommitNotObserved);
            }
            self.runtime
                .prepare_reminder_after_registration_inner_mock(
                    context,
                    input,
                    registration,
                    operation,
                    now,
                    expires,
                )
                .await
        }
        .await;
        self.finish(before, result)
    }
    /// Explicit local startup action, separate from ordinary reads. Reuses the
    /// original pending invalidation while preserving saved business/attempts.
    pub async fn invalidate_pending_for_restart_mock(&mut self) -> WriteResult<()> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = self.runtime.append(Command::Restart).await;
        self.finish(before, result)
    }
}
impl PendingProposedLedgerWriteMock {
    async fn verify_reader_mock(&self, connection: &mut PgConnection) -> WriteResult<()> {
        if database_scope(connection).await? != self.database_scope {
            return Err(ProposedLedgerWriteIssue::VerificationDatabaseMismatch);
        }
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(connection)
            .await
            .map_err(|_| ProposedLedgerWriteIssue::Unavailable)?;
        if pid == self.writer_pid {
            return Err(ProposedLedgerWriteIssue::VerificationConnectionRequired);
        }
        Ok(())
    }
    /// Use another database connection after caller commit. Same-backend reads
    /// could see uncommitted writes and are rejected even if the values match.
    /// Later valid appends are allowed, but the complete expected prefix must
    /// still be present. No receipt is synthesized from staged memory.
    pub async fn read_committed_on_connection_mock(
        &self,
        connection: &mut PgConnection,
    ) -> WriteResult<SubscriptionReadSnapshotMock> {
        self.verify_reader_mock(connection).await?;
        let saved = ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(connection, &self.owner)
            .await
            .map_err(ProposedLedgerWriteIssue::Read)?;
        let live = value(&saved.journal)?;
        let expected = self.expected["commands"]
            .as_array()
            .ok_or(ProposedLedgerWriteIssue::CommitNotObserved)?;
        let actual = live["commands"]
            .as_array()
            .ok_or(ProposedLedgerWriteIssue::CommitNotObserved)?;
        if live["initial"] != self.expected["initial"] || !actual.starts_with(expected) {
            return Err(ProposedLedgerWriteIssue::CommitNotObserved);
        }
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_writer_keeps_namespace_validation_and_does_not_accept_mock_or_public_alias() {
        for name in [
            "public",
            "pg_catalog",
            "safeflow_subscription_mock_v1",
            "a.b",
            "",
        ] {
            assert!(ProposedLedgerStoreMock::new(name).is_err());
        }
        assert!(ProposedLedgerStoreMock::new("sub_write_fixture").is_ok());
    }
}
