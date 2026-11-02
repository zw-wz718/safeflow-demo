//! Typed backend preparation only. Reuses adopted intent/settings rules and gates.
//! No public registration, authentication, transaction control or notification.
use super::*;
use serde_json::json;

/// Backend-owned target mapping and full checked schedule, never authorization.
/// Not deserializable: the host must resolve current identity and target first.
pub enum SubscriptionChangeRequestMock {
    CreateCancellationIntent {
        subscription_id: String,
    },
    RevokeCancellationIntent {
        subscription_id: String,
    },
    UpdateReminderSettings {
        subscription_id: String,
        schedule: Schedule,
    },
}
/// Exact backend binding for the existing confirmation methods. Never UI JSON.
pub enum SubscriptionChangeBindingMock {
    Intent(MockIntentBinding),
    Settings(ScheduleBinding),
}
impl SubscriptionChangeBindingMock {
    fn request(&self) -> Result<OriginalOperationRequestMock> {
        match self {
            Self::Intent(b) => OriginalOperationRequestMock::intent(b),
            Self::Settings(b) => OriginalOperationRequestMock::reminder_settings(b),
        }
    }
    fn matches_preparation(&self, commands: &[Command]) -> Result<bool> {
        let expected = match self {
            Self::Intent(b) => value(b)?,
            Self::Settings(b) => value(b)?,
        };
        Ok(commands.iter().any(|command| match (self, command) {
            (Self::Intent(_), Command::PrepareIntent { binding, .. })
            | (Self::Settings(_), Command::PrepareSettings { binding, .. }) => binding == &expected,
            _ => false,
        }))
    }
    fn preview(&self, saved: &SubscriptionReadSnapshotMock, now: &str) -> Result<Value> {
        if !self.matches_preparation(&saved.journal.commands)? {
            return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
        }
        let q = match self {
            Self::Intent(b) => SharedBusinessReadQueryMock::IntentPreview {
                draft_id: b.draft_id.clone(),
                context: b.context.clone(),
                now: now.into(),
            },
            Self::Settings(b) => {
                if saved.state.settings.get(&b.context.operation_id) != Some(b) {
                    return Err(RuntimeRecoveryIssue::Reminder("PLAN_CHANGED"));
                }
                SharedBusinessReadQueryMock::SettingsPreview {
                    context: b.context.clone(),
                    now: now.into(),
                }
            }
        };
        shared_queries::business_view(saved, &q)
    }
}

/// Provisional safe summary. All evidence fields are private and skipped in JSON.
/// The pending draft must be saved/committed/read back before it is called saved.
#[derive(Serialize)]
pub struct PreparedSubscriptionChangeMock {
    fictional: bool,
    state: &'static str,
    saved: bool,
    business_changed: bool,
    preview: Value,
    #[serde(skip)]
    namespace: String,
    #[serde(skip)]
    database_scope: DatabaseScope,
    #[serde(skip)]
    owner: String,
    #[serde(skip)]
    binding: SubscriptionChangeBindingMock,
}
impl PreparedSubscriptionChangeMock {
    /// The host retains this privately for the existing separate confirmation.
    pub fn confirmation_binding_mock(&self) -> &SubscriptionChangeBindingMock {
        &self.binding
    }
    /// Persist with original backend operation mapping, never put in a model/UI.
    pub fn original_request_mock(&self) -> Result<OriginalOperationRequestMock> {
        self.binding.request()
    }
    fn check_pending(&self, pending: &PendingProposedLedgerWriteMock) -> WriteResult<()> {
        if self.namespace != pending.namespace
            || self.database_scope != pending.database_scope
            || self.owner != pending.owner
        {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        let j: Journal = serde_json::from_value(pending.expected.clone())
            .map_err(|_| ProposedLedgerWriteIssue::Conflict)?;
        if !self.binding.matches_preparation(&j.commands)? {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        Ok(())
    }
    /// After a separately confirmed change was saved, verify committed facts.
    /// A draft alone yields NEEDS_REVIEW, not a completed business receipt.
    pub async fn read_committed_result_on_connection_mock(
        &self,
        pending: &PendingProposedLedgerWriteMock,
        reader: &mut PgConnection,
    ) -> WriteResult<Value> {
        self.check_pending(pending)?;
        let saved = pending.read_committed_on_connection_mock(reader).await?;
        Ok(value(
            &saved.original_view_mock(&self.original_request_mock()?)?,
        )?)
    }

    /// Same database, different connection, complete committed prefix, then LIVE
    /// draft validation. An old saved prefix is not permission to use stale UI.
    pub async fn read_committed_preview_on_connection_mock(
        &self,
        pending: &PendingProposedLedgerWriteMock,
        reader: &mut PgConnection,
        now: &str,
    ) -> WriteResult<Value> {
        self.check_pending(pending)?;
        let saved = pending.read_committed_on_connection_mock(reader).await?;
        let preview = self.binding.preview(&saved, now)?;
        Ok(
            json!({"fictional":true,"state":"AWAITING_CONFIRMATION","saved":true,"business_changed":false,"preview":preview}),
        )
    }
}
impl ProposedLedgerWorkMock {
    /// Prepare three adopted business changes; never execute or call a gate.
    /// Errors poison the work, so callers cannot save a partially prepared draft.
    pub async fn prepare_subscription_change_mock(
        &mut self,
        request: SubscriptionChangeRequestMock,
        operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> WriteResult<PreparedSubscriptionChangeMock> {
        self.ready()?;
        let before = self.runtime.journal.commands.len();
        let result = async {
            let binding = match request {
                SubscriptionChangeRequestMock::CreateCancellationIntent { subscription_id } => {
                    SubscriptionChangeBindingMock::Intent(
                        self.runtime
                            .prepare_intent_mock(
                                operation,
                                &subscription_id,
                                MockIntentKind::Record,
                                now,
                                expires,
                            )
                            .await?,
                    )
                }
                SubscriptionChangeRequestMock::RevokeCancellationIntent { subscription_id } => {
                    SubscriptionChangeBindingMock::Intent(
                        self.runtime
                            .prepare_intent_mock(
                                operation,
                                &subscription_id,
                                MockIntentKind::Revoke,
                                now,
                                expires,
                            )
                            .await?,
                    )
                }
                SubscriptionChangeRequestMock::UpdateReminderSettings {
                    subscription_id,
                    schedule,
                } => SubscriptionChangeBindingMock::Settings(
                    self.runtime
                        .prepare_settings_mock(operation, &subscription_id, schedule, now, expires)
                        .await?,
                ),
            };
            let saved = SubscriptionReadSnapshotMock::from_stored_journal_mock(
                &self.runtime.user,
                self.runtime.version,
                value(&self.runtime.journal)?,
            )?;
            let preview = binding.preview(&saved, now)?;
            Ok(PreparedSubscriptionChangeMock {
                fictional: true,
                state: "AWAITING_CONFIRMATION",
                saved: false,
                business_changed: false,
                preview,
                namespace: self.namespace.clone(),
                database_scope: self.database_scope.clone(),
                owner: self.runtime.user.clone(),
                binding,
            })
        }
        .await;
        self.finish(before, result)
    }
}
impl ProposedLedgerStoreMock {
    /// Read original facts only. Missing result is NEEDS_REVIEW, never retry advice.
    /// The trusted caller supplies identity and exact persisted backend binding.
    pub async fn query_change_result_on_connection_mock(
        &self,
        reader: &mut PgConnection,
        owner: &str,
        request: &OriginalOperationRequestMock,
    ) -> WriteResult<Value> {
        self.query_on_connection_mock(
            reader,
            owner,
            &SharedSubscriptionReadQueryMock::Business(SharedBusinessReadQueryMock::Original {
                request: request.clone(),
            }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognition::Cycle;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    const END: &str = "2026-10-14T12:05:00+08:00";
    fn work(prefix: usize) -> ProposedLedgerWorkMock {
        let mut j = super::super::super::raw_records_tests::actual_fixture_journal();
        j.commands.truncate(prefix);
        let s = SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            prefix as i64 + 1,
            value(&j).unwrap(),
        )
        .unwrap();
        ProposedLedgerWorkMock {
            namespace: "sub_write_check_changes".into(),
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
    fn snapshot(w: &ProposedLedgerWorkMock) -> SubscriptionReadSnapshotMock {
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            &w.runtime.user,
            w.runtime.version,
            value(&w.runtime.journal).unwrap(),
        )
        .unwrap()
    }
    fn id(w: &ProposedLedgerWorkMock) -> String {
        w.runtime.ledger_mock().list_mock("user_demo").unwrap()[0]
            .subscription_id
            .clone()
    }
    fn op() -> MockOperationContext {
        MockOperationContext {
            user_id: "user_demo".into(),
            session_ref: "fictional-change-session".into(),
            task_id: "task".into(),
            node_id: "change".into(),
            operation_id: "change-test".into(),
            idempotency_key: "fictional-change-key".into(),
            plan_version: 1,
        }
    }
    fn schedule() -> Schedule {
        Schedule {
            cycle: Cycle::CalendarMonth,
            anchor_date: NaiveDate::from_ymd_opt(2026, 9, 17).unwrap(),
            period_index: 1,
            expected_date: NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
            enabled: true,
            days_before: 1,
        }
    }
    fn request(kind: usize, id: String) -> SubscriptionChangeRequestMock {
        match kind {
            0 => SubscriptionChangeRequestMock::CreateCancellationIntent {
                subscription_id: id,
            },
            1 => SubscriptionChangeRequestMock::RevokeCancellationIntent {
                subscription_id: id,
            },
            _ => SubscriptionChangeRequestMock::UpdateReminderSettings {
                subscription_id: id,
                schedule: schedule(),
            },
        }
    }
    fn safe(v: &Value) {
        let text = v.to_string();
        for hidden in [
            "user_demo",
            "subscription_id",
            "account_001",
            "session_ref",
            "fictional-change-session",
            "idempotency_key",
            "fictional-change-key",
            "database_scope",
            "namespace",
            "binding",
            "anchor_date",
            "period_index",
            "source_id",
            "merchant_id",
            "service_ref",
            "journal",
        ] {
            assert!(!text.contains(hidden), "{hidden}");
        }
    }
    fn pending(w: &ProposedLedgerWorkMock) -> PendingProposedLedgerWriteMock {
        PendingProposedLedgerWriteMock {
            namespace: w.namespace.clone(),
            database_scope: w.database_scope.clone(),
            owner: w.runtime.user.clone(),
            writer_pid: 123,
            expected: value(&w.runtime.journal).unwrap(),
            fresh_reminder_reservation: false,
        }
    }
    #[tokio::test]
    async fn three_change_preparations_keep_business_and_history_unchanged() {
        for kind in 0..3 {
            let mut w = work(if kind == 1 { 6 } else { 4 });
            let before = snapshot(&w);
            let target = id(&w);
            let old_ledger = value(&before.ledger_mock().list_mock("user_demo").unwrap()).unwrap();
            let old_history = value(&before.cancellation_status_mock(&target).unwrap()).unwrap();
            let old_plan = value(&before.reminder_plan_mock(&target).unwrap()).unwrap();
            let old_len = w.runtime.journal.commands.len();
            let prepared = w
                .prepare_subscription_change_mock(request(kind, target.clone()), op(), NOW, END)
                .await
                .unwrap();
            let shown = value(&prepared).unwrap();
            safe(&shown);
            assert_eq!(shown["saved"], false);
            assert_eq!(shown["business_changed"], false);
            let after = snapshot(&w);
            assert_eq!(
                value(&after.ledger_mock().list_mock("user_demo").unwrap()).unwrap(),
                old_ledger
            );
            assert_eq!(
                value(&after.cancellation_status_mock(&target).unwrap()).unwrap(),
                old_history
            );
            assert_eq!(
                value(&after.reminder_plan_mock(&target).unwrap()).unwrap(),
                old_plan
            );
            assert_eq!(w.runtime.journal.commands.len(), old_len + 1);
            assert_eq!(
                after
                    .pending_confirmations_mock(NOW)
                    .unwrap()
                    .items
                    .iter()
                    .filter(|p| p.state == "READY")
                    .count(),
                1
            );
            assert_eq!(
                value(
                    &after
                        .original_view_mock(&prepared.original_request_mock().unwrap())
                        .unwrap()
                )
                .unwrap()["status"],
                "NEEDS_REVIEW"
            );
            if kind != 2 {
                assert!(shown["preview"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("取消"));
            }
        }
    }
    #[tokio::test]
    async fn settings_preview_shows_exact_before_after_without_saving() {
        let mut w = work(4);
        let target = id(&w);
        let prepared = w
            .prepare_subscription_change_mock(request(2, target), op(), NOW, END)
            .await
            .unwrap();
        let v = value(&prepared).unwrap();
        safe(&v);
        assert_eq!(v["preview"]["before"]["days_before"], 3);
        assert_eq!(v["preview"]["after"]["days_before"], 1);
        assert_eq!(v["preview"]["changed_fields"], json!(["days_before"]));
        assert!(prepared.binding.preview(&snapshot(&w), END).is_err());
    }
    #[tokio::test]
    async fn foreign_owner_missing_target_and_invalid_state_poison_without_new_commands() {
        for kind in 0..3 {
            for failure in 0..3 {
                let mut w = work(if kind == 1 { 6 } else { 4 });
                let mut operation = op();
                let target = if failure == 0 {
                    "other-subscription".into()
                } else {
                    id(&w)
                };
                let old = w.runtime.journal.commands.len();
                if failure == 1 {
                    operation.user_id = "other-owner".into();
                }
                let r = if failure == 2 && kind < 2 {
                    request(1 - kind, target)
                } else {
                    request(kind, target)
                };
                let end = if failure == 2 && kind == 2 { NOW } else { END };
                assert!(w
                    .prepare_subscription_change_mock(r, operation, NOW, end)
                    .await
                    .is_err());
                assert_eq!(w.runtime.journal.commands.len(), old);
                assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
            }
        }
    }
    #[tokio::test]
    async fn invalid_reminder_lead_and_changed_cycle_do_not_save_settings() {
        for kind in 0..2 {
            let mut w = work(4);
            let target = id(&w);
            let mut s = schedule();
            if kind == 0 {
                s.days_before = 31;
            } else {
                s.period_index = 2;
                s.expected_date = NaiveDate::from_ymd_opt(2026, 11, 17).unwrap();
            }
            let old = w.runtime.journal.commands.len();
            assert!(w
                .prepare_subscription_change_mock(
                    SubscriptionChangeRequestMock::UpdateReminderSettings {
                        subscription_id: target,
                        schedule: s
                    },
                    op(),
                    NOW,
                    END
                )
                .await
                .is_err());
            assert_eq!(w.runtime.journal.commands.len(), old);
            assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
        }
    }
    #[tokio::test]
    async fn pending_must_contain_exact_preparation_and_same_store_owner() {
        let mut w = work(4);
        let target = id(&w);
        let old = pending(&w);
        let prepared = w
            .prepare_subscription_change_mock(request(0, target), op(), NOW, END)
            .await
            .unwrap();
        assert!(prepared.check_pending(&old).is_err());
        let current = pending(&w);
        prepared.check_pending(&current).unwrap();
        for kind in 0..4 {
            let mut p = pending(&w);
            match kind {
                0 => p.namespace = "other".into(),
                1 => p.database_scope.0 = "other".into(),
                2 => p.owner = "other".into(),
                _ => {
                    p.expected["commands"]
                        .as_array_mut()
                        .unwrap()
                        .last_mut()
                        .unwrap()["binding"]["expires_at"] = json!(NOW)
                }
            }
            assert!(prepared.check_pending(&p).is_err());
        }
    }
    #[tokio::test]
    async fn restarted_abandoned_or_replaced_draft_never_gets_current_preview() {
        for kind in 0..3 {
            let mut w = work(4);
            let target = id(&w);
            let prepared = w
                .prepare_subscription_change_mock(request(2, target.clone()), op(), NOW, END)
                .await
                .unwrap();
            match kind {
                0 => w.invalidate_pending_for_restart_mock().await.unwrap(),
                1 => {
                    let p = snapshot(&w)
                        .pending_confirmations_mock(NOW)
                        .unwrap()
                        .items
                        .into_iter()
                        .find(|p| p.state == "READY")
                        .unwrap();
                    w.abandon_pending_mock(p.source_position, &op(), NOW)
                        .await
                        .unwrap();
                }
                _ => {
                    let mut next = op();
                    next.plan_version += 1;
                    let mut s = schedule();
                    s.enabled = false;
                    w.prepare_settings_mock(next, &target, s, NOW, END)
                        .await
                        .unwrap();
                }
            }
            assert!(prepared.binding.preview(&snapshot(&w), NOW).is_err());
        }
    }
    #[tokio::test]
    async fn result_missing_or_binding_changed_never_becomes_success() {
        let mut w = work(4);
        let target = id(&w);
        let prepared = w
            .prepare_subscription_change_mock(request(0, target), op(), NOW, END)
            .await
            .unwrap();
        let req = prepared.original_request_mock().unwrap();
        assert_eq!(
            value(&snapshot(&w).original_view_mock(&req).unwrap()).unwrap()["status"],
            "NEEDS_REVIEW"
        );
        let mut raw = value(&req).unwrap();
        raw["original_binding"]["context"]["user_id"] = json!("other");
        let changed: OriginalOperationRequestMock = serde_json::from_value(raw).unwrap();
        assert!(snapshot(&w).original_view_mock(&changed).is_err());
    }
    struct Gate {
        expected: Value,
        calls: usize,
        allow: bool,
    }
    impl Gate {
        fn verify(&mut self, b: &impl Serialize) -> MockGateOutcome {
            self.calls += 1;
            if self.allow && value(b).unwrap() == self.expected {
                MockGateOutcome::Confirmed
            } else {
                MockGateOutcome::Denied
            }
        }
    }
    impl MockIntentGateVerifier for Gate {
        fn verify_and_consume_mock_intent(
            &mut self,
            b: &MockIntentBinding,
            _: &str,
        ) -> MockGateOutcome {
            self.verify(b)
        }
    }
    impl MockScheduleGate for Gate {
        fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
            self.verify(b)
        }
    }
    async fn confirm(
        w: &mut ProposedLedgerWorkMock,
        p: &PreparedSubscriptionChangeMock,
        now: &str,
        g: &mut Gate,
    ) -> WriteResult<()> {
        match p.confirmation_binding_mock() {
            SubscriptionChangeBindingMock::Intent(b) => {
                w.confirm_intent_mock(&b.draft_id, &b.context, now, g).await
            }
            SubscriptionChangeBindingMock::Settings(b) => w.confirm_settings_mock(b, now, g).await,
        }
    }
    fn gate(p: &PreparedSubscriptionChangeMock, allow: bool) -> Gate {
        let expected = match p.confirmation_binding_mock() {
            SubscriptionChangeBindingMock::Intent(b) => value(b).unwrap(),
            SubscriptionChangeBindingMock::Settings(b) => value(b).unwrap(),
        };
        Gate {
            expected,
            calls: 0,
            allow,
        }
    }
    #[tokio::test]
    async fn explicit_confirmation_reuses_existing_gates_results_and_duplicate_rules() {
        for kind in 0..3 {
            let mut w = work(if kind == 1 { 6 } else { 4 });
            let target = id(&w);
            let p = w
                .prepare_subscription_change_mock(request(kind, target), op(), NOW, END)
                .await
                .unwrap();
            let req = p.original_request_mock().unwrap();
            let mut g = gate(&p, true);
            assert_eq!(g.calls, 0);
            confirm(&mut w, &p, NOW, &mut g).await.unwrap();
            assert_eq!(g.calls, 1);
            let s = snapshot(&w);
            let result = value(&s.original_view_mock(&req).unwrap()).unwrap();
            safe(&result);
            assert_eq!(
                result["status"],
                [
                    "CANCELLATION_INTENT_RECORDED",
                    "CANCELLATION_INTENT_REVOKED",
                    "SETTINGS_RECORDED"
                ][kind]
            );
            let old = value(&w.runtime.journal).unwrap();
            let calls = g.calls;
            confirm(&mut w, &p, NOW, &mut g).await.unwrap();
            assert_eq!(g.calls, calls);
            assert_eq!(value(&w.runtime.journal).unwrap(), old);
            let reconstructed = SubscriptionReadSnapshotMock::from_stored_journal_mock(
                "user_demo",
                s.version,
                value(&s.journal).unwrap(),
            )
            .unwrap();
            assert_eq!(
                value(&reconstructed.original_view_mock(&req).unwrap()).unwrap(),
                result
            );
        }
    }
    #[tokio::test]
    async fn original_result_rejects_same_operation_with_changed_full_binding() {
        for kind in 0..3 {
            let mut w = work(if kind == 1 { 6 } else { 4 });
            let target = id(&w);
            let p = w
                .prepare_subscription_change_mock(request(kind, target), op(), NOW, END)
                .await
                .unwrap();
            let mut g = gate(&p, true);
            confirm(&mut w, &p, NOW, &mut g).await.unwrap();
            let mut raw = value(&p.original_request_mock().unwrap()).unwrap();
            raw["original_binding"]["expires_at"] = json!("2026-10-14T12:06:00+08:00");
            let changed: OriginalOperationRequestMock = serde_json::from_value(raw).unwrap();
            assert_eq!(
                snapshot(&w).original_view_mock(&changed),
                Err(RuntimeRecoveryIssue::OriginalRequestConflict)
            );
        }
    }
    #[tokio::test]
    async fn stale_business_facts_reject_previews_before_new_authorization() {
        for kind in [0, 2] {
            let mut w = work(4);
            let target = id(&w);
            let p = w
                .prepare_subscription_change_mock(request(kind, target.clone()), op(), NOW, END)
                .await
                .unwrap();
            // Another explicit intent changes current ledger facts. This is only
            // an in-memory fictional replay; real database concurrency is deferred.
            let mut other = op();
            other.operation_id = "other-change".into();
            other.idempotency_key = "other-key".into();
            let b = w
                .prepare_intent_mock(other, &target, MockIntentKind::Record, NOW, END)
                .await
                .unwrap();
            let mut g = Gate {
                expected: value(&b).unwrap(),
                calls: 0,
                allow: true,
            };
            w.confirm_intent_mock(&b.draft_id, &b.context, NOW, &mut g)
                .await
                .unwrap();
            assert!(p.binding.preview(&snapshot(&w), NOW).is_err());
            assert_eq!(
                value(
                    &snapshot(&w)
                        .original_view_mock(&p.original_request_mock().unwrap())
                        .unwrap()
                )
                .unwrap()["status"],
                "NEEDS_REVIEW"
            );
        }
    }
    #[tokio::test]
    async fn denied_or_expired_confirmation_changes_no_business_facts() {
        for kind in 0..3 {
            for expired in [false, true] {
                let mut w = work(if kind == 1 { 6 } else { 4 });
                let target = id(&w);
                let p = w
                    .prepare_subscription_change_mock(request(kind, target), op(), NOW, END)
                    .await
                    .unwrap();
                let mut g = gate(&p, expired);
                let old = value(&w.runtime.journal).unwrap();
                assert!(confirm(&mut w, &p, if expired { END } else { NOW }, &mut g)
                    .await
                    .is_err());
                assert_eq!(g.calls, if expired { 0 } else { 1 });
                assert_eq!(value(&w.runtime.journal).unwrap(), old);
                assert_eq!(w.ready(), Err(ProposedLedgerWriteIssue::Poisoned));
            }
        }
    }
    #[tokio::test]
    async fn zero_thirty_and_off_are_previews_not_saved_preferences() {
        for (days, enabled) in [(0, true), (30, true), (3, false)] {
            let mut w = work(4);
            let target = id(&w);
            let mut s = schedule();
            s.days_before = days;
            s.enabled = enabled;
            // September17 -> October17 is30 days; the existing limit is reused.
            let p = w
                .prepare_subscription_change_mock(
                    SubscriptionChangeRequestMock::UpdateReminderSettings {
                        subscription_id: target.clone(),
                        schedule: s,
                    },
                    op(),
                    NOW,
                    END,
                )
                .await
                .unwrap();
            let shown = value(&p).unwrap();
            assert_eq!(shown["preview"]["after"]["days_before"], days);
            assert_eq!(shown["preview"]["after"]["enabled"], enabled);
            assert_eq!(
                snapshot(&w)
                    .reminder_plan_mock(&target)
                    .unwrap()
                    .schedule
                    .unwrap()
                    .days_before,
                3
            );
        }
    }
}
