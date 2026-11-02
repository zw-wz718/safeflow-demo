//! Trusted-backend, read-only recovery and evidence pagination. No execution.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OriginalOperationRequestMock {
    command_kind: String,
    original_binding: Value,
}
impl OriginalOperationRequestMock {
    pub fn registration(binding: &MockConfirmationBinding) -> Result<Self> {
        Ok(Self {
            command_kind: "Register".into(),
            original_binding: value(binding)?,
        })
    }
    pub fn intent(binding: &MockIntentBinding) -> Result<Self> {
        Ok(Self {
            command_kind: "Intent".into(),
            original_binding: value(binding)?,
        })
    }
    pub fn reminder_settings(binding: &ScheduleBinding) -> Result<Self> {
        Ok(Self {
            command_kind: "Settings".into(),
            original_binding: value(binding)?,
        })
    }
    pub(super) fn from_record(record: &RawBusinessRecordMock) -> Result<Self> {
        let kind = record.original_command["kind"]
            .as_str()
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
        Ok(Self {
            command_kind: kind.into(),
            original_binding: record.original_binding.clone(),
        })
    }
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OriginalResultMock {
    Recorded { original_receipt: Value },
    NeedsReview,
}

/// Allow-listed historical outcome only. No raw receipt, binding or identifiers.
/// Constructed from a validated snapshot; not from an untrusted receipt JSON.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct OriginalOperationViewMock {
    fictional: bool,
    status: &'static str,
    message: &'static str,
}
impl OriginalOperationViewMock {
    fn new(status: &'static str, message: &'static str) -> Self {
        Self {
            fictional: true,
            status,
            message,
        }
    }
    pub fn issue_mock(issue: RuntimeRecoveryIssue) -> Self {
        match issue {
            RuntimeRecoveryIssue::NotFound => Self::new(
                "NEEDS_REVIEW",
                "目前未查到可核对的订阅资料，请核对原请求；不要据此重复提交。",
            ),
            RuntimeRecoveryIssue::OriginalRequestConflict => {
                Self::new("REQUEST_CONFLICT", "本次查询与原请求不一致，请核对原请求。")
            }
            _ => Self::new(
                "UNAVAILABLE",
                "暂时无法查询订阅结果，请稍后核对原请求；不要重复提交。",
            ),
        }
    }
}
impl SubscriptionReadSnapshotMock {
    pub fn original_view_mock(
        &self,
        request: &OriginalOperationRequestMock,
    ) -> Result<OriginalOperationViewMock> {
        match self.original_result_mock(request)? {
            OriginalResultMock::NeedsReview => Ok(OriginalOperationViewMock::new(
                "NEEDS_REVIEW",
                "原请求的结果仍待核对，请勿重复提交。",
            )),
            OriginalResultMock::Recorded { .. } => match request.command_kind.as_str() {
                "Register" => Ok(OriginalOperationViewMock::new(
                    "REGISTRATION_RECORDED",
                    "查询到这次订阅登记已完成。",
                )),
                "Settings" => Ok(OriginalOperationViewMock::new(
                    "SETTINGS_RECORDED",
                    "查询到这次提醒设置已保存；不代表提醒已发送。",
                )),
                "Intent" if request.original_binding["kind"] == "RECORD" => {
                    Ok(OriginalOperationViewMock::new(
                        "CANCELLATION_INTENT_RECORDED",
                        "查询到这次取消意图已记录；尚未确认商户已取消。",
                    ))
                }
                "Intent" if request.original_binding["kind"] == "REVOKE" => {
                    Ok(OriginalOperationViewMock::new(
                        "CANCELLATION_INTENT_REVOKED",
                        "查询到这次取消意图已撤回；不代表商户服务已恢复。",
                    ))
                }
                _ => Err(RuntimeRecoveryIssue::InvalidJournal),
            },
        }
    }
}
impl SubscriptionReadSnapshotMock {
    /// Compare the entire original binding, not a caller-selected operation ID.
    /// Original session references remain original; current identity is checked by
    /// the trusted caller, not re-issued by this historical read.
    pub fn original_result_mock(
        &self,
        request: &OriginalOperationRequestMock,
    ) -> Result<OriginalResultMock> {
        let expected: MockOperationContext =
            serde_json::from_value(request.original_binding["context"].clone())
                .map_err(|_| RuntimeRecoveryIssue::OriginalRequestConflict)?;
        if expected.user_id != self.owner
            || !valid_operation(&expected)
            || !matches!(
                request.command_kind.as_str(),
                "Register" | "Intent" | "Settings"
            )
        {
            return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
        }
        let reminder = request.command_kind == "Settings";
        for record in &self.business_records {
            let actual_kind = record.original_command["kind"]
                .as_str()
                .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
            if (actual_kind == "Settings") != reminder {
                continue;
            }
            let actual: MockOperationContext =
                serde_json::from_value(record.original_binding["context"].clone())
                    .map_err(|_| RuntimeRecoveryIssue::InvalidJournal)?;
            if actual.operation_id == expected.operation_id
                || actual.idempotency_key == expected.idempotency_key
            {
                if actual_kind != request.command_kind
                    || record.original_binding != request.original_binding
                {
                    return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
                }
                return Ok(OriginalResultMock::Recorded {
                    original_receipt: record.original_command["receipt"].clone(),
                });
            }
        }
        // Absence is never proof that the transaction failed or may be retried.
        Ok(OriginalResultMock::NeedsReview)
    }
}
impl<T> PendingRuntimeWriteMock<T> {
    /// Save these read-only probes before commit/publish. They convey no grant.
    pub fn recovery_requests_mock(&self) -> Result<Vec<OriginalOperationRequestMock>> {
        let records = collect_business_records(&self.candidate.journal)?;
        let start = usize::try_from(self.base_version - 1)
            .map_err(|_| RuntimeRecoveryIssue::InvalidJournal)?;
        let tail = self
            .candidate
            .journal
            .commands
            .get(start..)
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
        let mut requests = Vec::new();
        for command in tail {
            let (kind, operation) = match command {
                Command::Register { context, .. } => ("Register", &context.operation_id),
                Command::Intent { context, .. } => ("Intent", &context.operation_id),
                Command::Settings { operation, .. } => ("Settings", operation),
                _ => continue,
            };
            let record = records
                .iter()
                .find(|record| {
                    record.original_command["kind"] == kind
                        && record.original_binding["context"]["operation_id"] == operation.as_str()
                })
                .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
            let request = OriginalOperationRequestMock::from_record(record)?;
            if !requests.contains(&request) {
                requests.push(request);
            }
        }
        Ok(requests)
    }
}
impl SubscriptionRuntimeMock {
    /// Reads on a separate connection after a lost/uncertain reply. It neither
    /// publishes stale runtime state nor starts recovery, sends or executes again.
    pub async fn recover_original_mock(
        &self,
        request: &OriginalOperationRequestMock,
    ) -> Result<OriginalResultMock> {
        if self.staged {
            return Err(RuntimeRecoveryIssue::StagedOperationUnsupported);
        }
        let mut connection = self
            .pool
            .as_ref()
            .ok_or(RuntimeRecoveryIssue::StagedOperationUnsupported)?
            .acquire()
            .await
            .map_err(|_| RuntimeRecoveryIssue::Unavailable)?;
        let saved =
            SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &self.user)
                .await?;
        saved.original_result_mock(request)
    }
}

/// Backend checkpoint only: not an authentication token or receiver ACK. The
/// exact exported prefix detects changed/truncated history without a new hash
/// dependency. Contains internal evidence, so store only in trusted backend state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BusinessRecordCursorMock {
    owner: String,
    observed_version: i64,
    exported_prefix: Vec<RawBusinessRecordMock>,
}
#[derive(Clone, Debug, Serialize)]
pub struct BusinessRecordPageMock {
    pub records: Vec<RawBusinessRecordMock>,
    pub next_cursor: BusinessRecordCursorMock,
    pub has_more: bool,
}
impl SubscriptionReadSnapshotMock {
    /// limit is a backend page bound, not a new product/business rule.
    pub fn business_record_page_mock(
        &self,
        cursor: Option<&BusinessRecordCursorMock>,
        limit: usize,
    ) -> Result<BusinessRecordPageMock> {
        if !(1..=1000).contains(&limit) {
            return Err(RuntimeRecoveryIssue::InvalidPageSize);
        }
        let start = if let Some(cursor) = cursor {
            if cursor.owner != self.owner
                || cursor.observed_version <= 0
                || cursor.observed_version > self.version
                || !self.business_records.starts_with(&cursor.exported_prefix)
            {
                return Err(RuntimeRecoveryIssue::InvalidCursor);
            }
            cursor.exported_prefix.len()
        } else {
            0
        };
        let end = start.saturating_add(limit).min(self.business_records.len());
        Ok(BusinessRecordPageMock {
            records: self.business_records[start..end].to_vec(),
            next_cursor: BusinessRecordCursorMock {
                owner: self.owner.clone(),
                observed_version: self.version,
                exported_prefix: self.business_records[..end].to_vec(),
            },
            has_more: end < self.business_records.len(),
        })
    }
}

/// Internal call-order wrapper. The caller still owns transaction commit and
/// authentication. An uncertain outcome only reads original facts and stops.
pub struct SubscriptionWriteHandoffMock<T> {
    pending: PendingRuntimeWriteMock<T>,
    requests: Vec<OriginalOperationRequestMock>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OriginalLookupAttemptMock {
    Read { result: OriginalResultMock },
    Unavailable,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReadOnlyWriteRecoveryMock {
    pub reason: &'static str,
    pub original_results: Vec<OriginalLookupAttemptMock>,
    pub continue_writes: bool,
    pub business_reexecuted: bool,
    pub runtime_refreshed: bool,
}
pub enum SubscriptionWriteCompletionMock<T> {
    Published(T),
    ReadOnlyRecovery(ReadOnlyWriteRecoveryMock),
}
impl<T> PendingRuntimeWriteMock<T> {
    pub fn into_handoff_mock(self) -> Result<SubscriptionWriteHandoffMock<T>> {
        let requests = self.recovery_requests_mock()?;
        Ok(SubscriptionWriteHandoffMock {
            pending: self,
            requests,
        })
    }
}
impl<T> SubscriptionWriteHandoffMock<T> {
    pub fn original_requests_mock(&self) -> &[OriginalOperationRequestMock] {
        &self.requests
    }
    /// The commit result is a backend control-flow value, not an authorization
    /// grant. A claimed success must still pass committed-fact readback.
    pub async fn complete_mock<E>(
        self,
        runtime: &mut SubscriptionRuntimeMock,
        caller_commit: std::result::Result<(), E>,
    ) -> SubscriptionWriteCompletionMock<T> {
        let reason = if caller_commit.is_ok() {
            match self.pending.publish_after_commit_mock(runtime).await {
                Ok(output) => return SubscriptionWriteCompletionMock::Published(output),
                Err(_) => "POST_COMMIT_READBACK_UNCERTAIN",
            }
        } else {
            "COMMIT_RESULT_UNCERTAIN"
        };
        let mut original_results = Vec::new();
        for request in &self.requests {
            original_results.push(match runtime.recover_original_mock(request).await {
                Ok(result) => OriginalLookupAttemptMock::Read { result },
                Err(_) => OriginalLookupAttemptMock::Unavailable,
            });
        }
        SubscriptionWriteCompletionMock::ReadOnlyRecovery(ReadOnlyWriteRecoveryMock {
            reason,
            original_results,
            continue_writes: false,
            business_reexecuted: false,
            runtime_refreshed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(command_count: usize) -> SubscriptionReadSnapshotMock {
        let mut journal = super::super::raw_records_tests::actual_fixture_journal();
        journal.commands.truncate(command_count);
        let version = journal.commands.len() as i64 + 1;
        let (journal, state) =
            decode_journal("user_demo", version, value(&journal).unwrap()).unwrap();
        SubscriptionReadSnapshotMock {
            owner: "user_demo".into(),
            version,
            state,
            business_records: collect_business_records(&journal).unwrap(),
            journal,
        }
    }
    #[test]
    fn exact_original_bindings_read_all_four_historical_receipts() {
        let saved = snapshot(8);
        for record in &saved.business_records {
            let request = OriginalOperationRequestMock::from_record(record).unwrap();
            assert_eq!(
                saved.original_result_mock(&request).unwrap(),
                OriginalResultMock::Recorded {
                    original_receipt: record.original_command["receipt"].clone()
                }
            );
        }
    }
    #[test]
    fn user_views_cover_four_original_outcomes_without_internal_evidence() {
        let saved = snapshot(8);
        let mut statuses = Vec::new();
        for record in &saved.business_records {
            let request = OriginalOperationRequestMock::from_record(record).unwrap();
            let view = serde_json::to_value(saved.original_view_mock(&request).unwrap()).unwrap();
            assert_eq!(view.as_object().unwrap().len(), 3);
            assert_eq!(view["fictional"], true);
            assert!(view.get("status").is_some());
            assert!(view.get("message").is_some());
            statuses.push(view["status"].as_str().unwrap().to_string());
            let text = view.to_string();
            for forbidden in [
                "user_demo",
                "session_ref",
                "idempotency_key",
                "original_binding",
                "original_receipt",
                "commands",
                "candidate",
            ] {
                assert!(!text.contains(forbidden));
            }
        }
        assert_eq!(
            statuses,
            vec![
                "REGISTRATION_RECORDED",
                "SETTINGS_RECORDED",
                "CANCELLATION_INTENT_RECORDED",
                "CANCELLATION_INTENT_REVOKED"
            ]
        );
        let request =
            OriginalOperationRequestMock::from_record(&saved.business_records[2]).unwrap();
        assert!(saved
            .original_view_mock(&request)
            .unwrap()
            .message
            .contains("尚未确认商户已取消"));
        let request =
            OriginalOperationRequestMock::from_record(&saved.business_records[3]).unwrap();
        assert!(saved
            .original_view_mock(&request)
            .unwrap()
            .message
            .contains("不代表商户服务已恢复"));
    }
    #[test]
    fn missing_outcome_and_query_errors_never_become_empty_or_success_views() {
        let full = snapshot(8);
        let request = OriginalOperationRequestMock::from_record(&full.business_records[0]).unwrap();
        assert_eq!(
            snapshot(0).original_view_mock(&request).unwrap().status,
            "NEEDS_REVIEW"
        );
        assert_eq!(
            OriginalOperationViewMock::issue_mock(RuntimeRecoveryIssue::NotFound).status,
            "NEEDS_REVIEW"
        );
        assert_eq!(
            OriginalOperationViewMock::issue_mock(RuntimeRecoveryIssue::OriginalRequestConflict)
                .status,
            "REQUEST_CONFLICT"
        );
        for error in [
            RuntimeRecoveryIssue::Unavailable,
            RuntimeRecoveryIssue::InvalidJournal,
            RuntimeRecoveryIssue::DedicatedDatabaseRequired,
        ] {
            let view = OriginalOperationViewMock::issue_mock(error);
            assert_eq!(view.status, "UNAVAILABLE");
            assert!(!view.message.contains("空台账"));
            assert!(!view.message.contains("已完成"));
        }
    }
    #[test]
    fn current_list_and_cancellation_progress_keep_history_after_revoke() {
        let cancelled = snapshot(6);
        let id = cancelled.list_subscriptions_mock().unwrap().items[0]
            .subscription_id
            .clone();
        let progress = cancelled.cancellation_status_mock(&id).unwrap();
        assert_eq!(progress.current_state, LedgerStatus::Cancelling);
        assert_eq!(progress.history.len(), 1);
        assert!(!progress.merchant_cancellation_confirmed);
        let revoked = snapshot(8);
        let progress = revoked.cancellation_status_mock(&id).unwrap();
        assert_eq!(progress.current_state, LedgerStatus::Active);
        assert_eq!(progress.history.len(), 2);
        assert_eq!(progress.history[0].kind, MockIntentKind::Record);
        assert_eq!(progress.history[1].kind, MockIntentKind::Revoke);
        assert!(matches!(
            revoked.cancellation_status_mock("other-subscription"),
            Err(RuntimeRecoveryIssue::NotFound)
        ));
    }
    #[test]
    fn current_queries_distinguish_empty_unavailable_incomplete_and_error_rows() {
        assert!(snapshot(0)
            .list_subscriptions_mock()
            .unwrap()
            .items
            .is_empty());
        let mut saved = snapshot(8);
        saved.state.ledger.ledger.entries[0].service_ref = None;
        assert_eq!(
            saved.list_subscriptions_mock().unwrap().items[0].service_data_state,
            "NEEDS_REVIEW"
        );
        saved.state.ledger.ledger.entries[0].status = LedgerStatus::Error;
        assert_eq!(
            saved.list_subscriptions_mock().unwrap().items[0].current_state,
            LedgerStatus::Error
        );
        saved.state.ledger.ledger.read.state = ReadKind::Unavailable;
        assert!(saved.list_subscriptions_mock().is_err());
    }
    #[test]
    fn current_reminder_queries_do_not_scan_and_do_not_expose_original_context() {
        let no_settings = snapshot(2);
        let id = no_settings.list_subscriptions_mock().unwrap().items[0]
            .subscription_id
            .clone();
        let view = no_settings.reminder_plan_mock(&id).unwrap();
        assert_eq!(view.data_state, "NOT_CONFIGURED");
        assert!(view.schedule.is_none());
        assert!(view.current_attempt.is_none());
        let saved = snapshot(6);
        let before = serde_json::to_value(saved.business_records_mock()).unwrap();
        let view = saved.reminder_plan_mock(&id).unwrap();
        assert_eq!(view.data_state, "CONFIGURED");
        assert_eq!(
            view.active_kind,
            Some(crate::reminder_mock::ReminderKind::CancellationCheck)
        );
        assert_eq!(
            view.current_attempt,
            Some(crate::reminder_mock::ReminderAttemptStateMock::NotAttempted)
        );
        let all = serde_json::json!([
            saved.list_subscriptions_mock().unwrap(),
            saved.cancellation_status_mock(&id).unwrap(),
            view
        ]);
        for forbidden in [
            "session_ref",
            "original_binding",
            "original_receipt",
            "owner_user_id",
            "merchant_id",
            "source_id",
            "service_ref",
            "idempotency_key",
        ] {
            assert!(!all.to_string().contains(forbidden));
        }
        assert_eq!(
            before,
            serde_json::to_value(saved.business_records_mock()).unwrap()
        );
    }
    fn discovery_sample() -> crate::Sample {
        crate::billing_query_mock::load_case(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../../fixtures/subscription/billing-consumption-1044e918.v1.json"
            )),
            "complete",
            MockContext {
                user_id: "user_demo".into(),
                requested_account_ids: vec!["account_001".into()],
            },
        )
        .unwrap()
    }
    #[test]
    fn discovery_entry_uses_current_ledger_and_preserves_review_or_incomplete_evidence() {
        let sample = discovery_sample();
        let empty = snapshot(0);
        let view = empty
            .identify_subscriptions_mock(&sample.trusted_context_fixture, &sample.snapshot)
            .unwrap();
        assert!(view.matching_executed);
        assert_eq!(
            view.groups[0].disposition,
            crate::managed_ledger_mock::Disposition::NotInLedger
        );
        let registered = snapshot(2);
        assert_eq!(
            registered
                .identify_subscriptions_mock(&sample.trusted_context_fixture, &sample.snapshot)
                .unwrap()
                .groups[0]
                .disposition,
            crate::managed_ledger_mock::Disposition::AlreadyManaged
        );
        let mut broken = sample.snapshot.clone();
        broken.transactions_read.state = ReadKind::Unavailable;
        broken.transactions_read.is_complete = false;
        assert!(
            !empty
                .identify_subscriptions_mock(&sample.trusted_context_fixture, &broken)
                .unwrap()
                .matching_executed
        );
        let mut other = sample.trusted_context_fixture.clone();
        other.user_id = "other".into();
        assert!(empty
            .identify_subscriptions_mock(&other, &sample.snapshot)
            .is_err());
    }
    #[test]
    fn date_entry_rejects_past_future_and_ambiguous_targets_without_planning_writes() {
        let root: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/discovery_ready_input.v1.json"
        )))
        .unwrap();
        assert_eq!(
            root["provenance"],
            "HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE"
        );
        let sample: crate::Sample = serde_json::from_value(root["sample"].clone()).unwrap();
        let saved = snapshot(0);
        let ready = saved
            .suggest_reminder_date_mock(
                &sample.trusted_context_fixture,
                &sample.snapshot,
                Some(0),
                None,
                "2026-10-14T12:00:00+08:00",
            )
            .unwrap();
        assert_eq!(ready.state, "READY");
        assert!(ready.prediction.unwrap().needs_user_confirmation);
        assert_eq!(ready.suggested_schedule.unwrap().days_before, 3);
        for (candidate, id, now) in [
            (Some(0), None, "2026-10-20T12:00:00+08:00"),
            (Some(0), Some("any"), "2026-10-14T12:00:00+08:00"),
            (None, None, "2026-10-14T12:00:00+08:00"),
            (Some(0), None, "2026-01-01T12:00:00+08:00"),
        ] {
            let view = saved
                .suggest_reminder_date_mock(
                    &sample.trusted_context_fixture,
                    &sample.snapshot,
                    candidate,
                    id,
                    now,
                )
                .unwrap();
            assert_eq!(view.state, "NEEDS_INPUT");
            assert!(view.prediction.is_none());
            assert!(view.suggested_schedule.is_none());
        }
        assert!(saved.list_subscriptions_mock().unwrap().items.is_empty());
    }
    #[test]
    fn changed_original_context_or_action_never_reads_any_of_four_successes() {
        let saved = snapshot(8);
        assert_eq!(saved.business_records.len(), 4);
        let before = value(&saved.journal).unwrap();
        let mut rejected = 0;
        for record in &saved.business_records {
            let original = OriginalOperationRequestMock::from_record(record).unwrap();
            for field in [
                "user_id",
                "session_ref",
                "task_id",
                "node_id",
                "operation_id",
                "idempotency_key",
                "plan_version",
            ] {
                let mut changed = original.clone();
                changed.original_binding["context"][field] = if field == "plan_version" {
                    Value::from(2)
                } else {
                    Value::from("foreign-or-different")
                };
                assert_eq!(
                    saved.original_result_mock(&changed),
                    Err(RuntimeRecoveryIssue::OriginalRequestConflict),
                    "{}: changed {field}",
                    original.command_kind
                );
                rejected += 1;
            }
            for kind in ["Register", "Settings", "Intent", "Unknown"] {
                if kind == original.command_kind {
                    continue;
                }
                let mut changed = original.clone();
                changed.command_kind = kind.into();
                assert!(!matches!(
                    saved.original_result_mock(&changed),
                    Ok(OriginalResultMock::Recorded { .. })
                ));
                assert!(!matches!(
                    saved.original_view_mock(&changed),
                    Ok(view) if view.status != "NEEDS_REVIEW"
                ));
                rejected += 1;
            }
            assert_eq!(
                saved.original_result_mock(&original).unwrap(),
                OriginalResultMock::Recorded {
                    original_receipt: record.original_command["receipt"].clone()
                }
            );
        }
        assert_eq!(rejected, 40);
        assert_eq!(value(&saved.journal).unwrap(), before);
    }
    #[test]
    fn changed_candidate_target_schedule_and_confirmation_are_conflicts() {
        let saved = snapshot(8);
        for (index, field, replacement) in [
            (
                0,
                "candidate",
                serde_json::json!({"mock_service_ref":"different"}),
            ),
            (
                0,
                "requested_account_ids",
                serde_json::json!(["foreign-account"]),
            ),
            (0, "expires_at", Value::from("2099-01-01T00:00:00Z")),
            (1, "schedule", serde_json::json!({"days_before":0})),
            (
                2,
                "target",
                serde_json::json!({"subscription_id":"different"}),
            ),
            (2, "kind", Value::from("REVOKE")),
        ] {
            let mut request =
                OriginalOperationRequestMock::from_record(&saved.business_records[index]).unwrap();
            request.original_binding[field] = replacement;
            assert_eq!(
                saved.original_result_mock(&request),
                Err(RuntimeRecoveryIssue::OriginalRequestConflict)
            );
        }
    }
    #[test]
    fn unresolved_read_then_recorded_result_never_reexecutes_or_changes_state() {
        let prepared = snapshot(1);
        let committed = snapshot(8);
        let request =
            OriginalOperationRequestMock::from_record(&committed.business_records[0]).unwrap();
        let before = value(&prepared.state.ledger.list_mock("user_demo").unwrap()).unwrap();
        assert_eq!(
            prepared.original_result_mock(&request).unwrap(),
            OriginalResultMock::NeedsReview
        );
        assert_eq!(
            value(&prepared.state.ledger.list_mock("user_demo").unwrap()).unwrap(),
            before
        );
        let resolved = committed.original_result_mock(&request).unwrap();
        assert!(matches!(resolved, OriginalResultMock::Recorded { .. }));
        assert_eq!(
            committed.state.ledger.list_mock("user_demo").unwrap().len(),
            1
        );
        assert_eq!(
            committed
                .state
                .ledger
                .intent_events_mock("user_demo")
                .unwrap()
                .len(),
            2
        );
        println!(
            "ORIGINAL_RECOVERY_FIXTURE_JSON:{}",
            serde_json::json!({"fictional":true,"database_executed":false,
            "before":OriginalResultMock::NeedsReview,"after":resolved,"business_reexecuted":false,"notification_sent":false})
        );
    }
    #[test]
    fn incremental_pages_walk_once_and_repeat_the_same_checkpoint_safely() {
        let saved = snapshot(8);
        let first = saved.business_record_page_mock(None, 2).unwrap();
        assert_eq!(first.records.len(), 2);
        assert!(first.has_more);
        let repeated = saved
            .business_record_page_mock(Some(&first.next_cursor), 2)
            .unwrap();
        let second = saved
            .business_record_page_mock(Some(&first.next_cursor), 2)
            .unwrap();
        assert_eq!(repeated.records, second.records);
        assert!(!second.has_more);
        let complete: Vec<_> = first.records.into_iter().chain(second.records).collect();
        assert_eq!(complete, saved.business_records);
        let empty = saved
            .business_record_page_mock(Some(&second.next_cursor), 2)
            .unwrap();
        assert!(empty.records.is_empty());
        assert!(!empty.has_more);
    }
    #[test]
    fn serialized_backend_checkpoint_continues_after_new_records() {
        let old = snapshot(4);
        let first = old.business_record_page_mock(None, 10).unwrap();
        let encoded = serde_json::to_string(&first.next_cursor).unwrap();
        let resumed: BusinessRecordCursorMock = serde_json::from_str(&encoded).unwrap();
        let current = snapshot(8);
        let next = current
            .business_record_page_mock(Some(&resumed), 10)
            .unwrap();
        assert_eq!(next.records, current.business_records[2..]);
        assert!(!next.has_more);
        println!(
            "BUSINESS_PAGE_FIXTURE_JSON:{}",
            serde_json::json!({"fictional":true,"database_executed":false,
            "first_records":first.records.len(),"new_records":next.records.len(),"has_more":next.has_more,"audit_acknowledged":false})
        );
    }
    #[test]
    fn foreign_future_changed_or_truncated_checkpoint_is_rejected() {
        let saved = snapshot(8);
        let cursor = saved
            .business_record_page_mock(None, 10)
            .unwrap()
            .next_cursor;
        let mut bad = cursor.clone();
        bad.owner = "foreign".into();
        assert!(matches!(
            saved.business_record_page_mock(Some(&bad), 1),
            Err(RuntimeRecoveryIssue::InvalidCursor)
        ));
        let mut bad = cursor.clone();
        bad.observed_version += 1;
        assert!(matches!(
            saved.business_record_page_mock(Some(&bad), 1),
            Err(RuntimeRecoveryIssue::InvalidCursor)
        ));
        let mut bad = cursor.clone();
        bad.exported_prefix[0].original_command["receipt"]["notification_sent"] = Value::Bool(true);
        assert!(matches!(
            saved.business_record_page_mock(Some(&bad), 1),
            Err(RuntimeRecoveryIssue::InvalidCursor)
        ));
        assert!(matches!(
            snapshot(4).business_record_page_mock(Some(&cursor), 1),
            Err(RuntimeRecoveryIssue::InvalidCursor)
        ));
    }
    #[test]
    fn invalid_page_bounds_are_rejected() {
        let saved = snapshot(8);
        for limit in [0, 1001, usize::MAX] {
            assert!(matches!(
                saved.business_record_page_mock(None, limit),
                Err(RuntimeRecoveryIssue::InvalidPageSize)
            ));
        }
    }
    #[tokio::test]
    async fn pending_write_retains_read_only_probes_before_publish_or_lost_reply() {
        let journal = super::super::raw_records_tests::actual_fixture_journal();
        let version = journal.commands.len() as i64 + 1;
        let (_, state) = decode_journal("user_demo", version, value(&journal).unwrap()).unwrap();
        let mut base = journal.clone();
        base.commands.clear();
        let pending = PendingRuntimeWriteMock {
            base_version: 1,
            base_journal: value(&base).unwrap(),
            output: (),
            candidate: SubscriptionRuntimeMock {
                pool: Some(
                    sqlx::postgres::PgPoolOptions::new()
                        .connect_lazy("postgres://localhost/unused-fictional")
                        .unwrap(),
                ),
                user: "user_demo".into(),
                version,
                journal,
                state,
                staged: true,
            },
        };
        let requests = pending.recovery_requests_mock().unwrap();
        assert_eq!(requests.len(), 4);
        drop(pending); // Dropping does not execute, commit or publish anything.
        let saved = snapshot(8);
        for request in requests {
            assert!(matches!(
                saved.original_result_mock(&request),
                Ok(OriginalResultMock::Recorded { .. })
            ));
        }
    }
}
