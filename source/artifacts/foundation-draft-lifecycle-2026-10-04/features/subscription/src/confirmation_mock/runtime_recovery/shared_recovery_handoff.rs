//! Shared-store caller handoff, incremental backend evidence and explicit startup.
//! No authentication, audit ACK, notification or transaction management.
use super::*;
use serde_json::json;

pub struct SharedSubscriptionWriteHandoffMock {
    pending: PendingProposedLedgerWriteMock,
    requests: Vec<OriginalOperationRequestMock>,
}
/// Safe completion view only; never returns raw bindings, receipts or write grants.
#[derive(Debug, Serialize)]
pub struct SharedWriteCompletionViewMock {
    fictional: bool,
    state: &'static str,
    reason: &'static str,
    saved_prefix_verified: bool,
    original_results: Vec<OriginalOperationViewMock>,
    continue_writes: bool,
    business_reexecuted: bool,
}
fn completion(
    committed: bool,
    verified: bool,
    requests: &[OriginalOperationRequestMock],
    snapshot: Option<&SubscriptionReadSnapshotMock>,
) -> SharedWriteCompletionViewMock {
    let success = committed && verified;
    SharedWriteCompletionViewMock {
        fictional: true,
        state: if success {
            "COMMITTED_VERIFIED"
        } else {
            "READ_ONLY_RECOVERY"
        },
        reason: if success {
            "COMMITTED_PREFIX_OBSERVED"
        } else if !committed {
            "COMMIT_RESULT_UNCERTAIN"
        } else {
            "POST_COMMIT_READBACK_UNCERTAIN"
        },
        saved_prefix_verified: verified,
        original_results: requests
            .iter()
            .map(|request| match snapshot {
                Some(s) => s
                    .original_view_mock(request)
                    .unwrap_or_else(OriginalOperationViewMock::issue_mock),
                None => OriginalOperationViewMock::issue_mock(RuntimeRecoveryIssue::Unavailable),
            })
            .collect(),
        continue_writes: false,
        business_reexecuted: false,
    }
}
impl ProposedLedgerWorkMock {
    /// Newly completed business records only. Preparations/startup/reminder scans
    /// are not completed registration/intent/settings records or fake audit events.
    pub fn new_original_requests_mock(&self) -> WriteResult<Vec<OriginalOperationRequestMock>> {
        self.ready()?;
        let start = usize::try_from(self.base_version - 1)
            .map_err(|_| ProposedLedgerWriteIssue::Conflict)?;
        if start > self.runtime.journal.commands.len() {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        let records = collect_business_records(&self.runtime.journal)?;
        let mut requests = Vec::new();
        for record in records
            .iter()
            .filter(|r| r.source_command_position >= start)
        {
            let request = OriginalOperationRequestMock::from_record(record)?;
            if !requests.contains(&request) {
                requests.push(request);
            }
        }
        Ok(requests)
    }
}
impl ProposedLedgerStoreMock {
    pub async fn save_with_handoff_on_connection_mock(
        &self,
        writer: &mut PgConnection,
        work: ProposedLedgerWorkMock,
    ) -> WriteResult<SharedSubscriptionWriteHandoffMock> {
        let requests = work.new_original_requests_mock()?;
        let pending = self.save_on_connection_mock(writer, work).await?;
        Ok(SharedSubscriptionWriteHandoffMock { pending, requests })
    }
    /// Backend evidence, not user/LLM JSON and not formal C-AUD/Outbox or ACK.
    pub async fn business_record_page_on_connection_mock(
        &self,
        reader: &mut PgConnection,
        owner: &str,
        cursor: Option<&SharedBusinessRecordCursorMock>,
        limit: usize,
    ) -> WriteResult<SharedBusinessRecordPageMock> {
        let scope = database_scope(reader).await?;
        let saved = ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(reader, owner)
            .await
            .map_err(ProposedLedgerWriteIssue::Read)?;
        page(&self.namespace, &scope, &saved, cursor, limit)
    }
    /// Explicit startup write. Never used as fallback on a failed ordinary read.
    /// No Pool, schema creation, COMMIT or notification; caller owns its transaction.
    pub async fn stage_startup_recovery_on_connection_mock(
        &self,
        writer: &mut PgConnection,
        owner: &str,
    ) -> WriteResult<PendingProposedLedgerWriteMock> {
        let mut work = self.load_work_on_connection_mock(writer, owner).await?;
        work.invalidate_pending_for_restart_mock().await?;
        self.save_on_connection_mock(writer, work).await
    }
}
impl SharedSubscriptionWriteHandoffMock {
    /// Save privately in the host's original operation mapping BEFORE commit.
    pub fn original_requests_mock(&self) -> &[OriginalOperationRequestMock] {
        &self.requests
    }
    /// Commit result is caller control flow, never authorization. Even if a lost
    /// commit reply is followed by a known receipt, this path stays read-only.
    pub async fn complete_on_connection_mock<E>(
        self,
        reader: &mut PgConnection,
        caller_commit: std::result::Result<(), E>,
    ) -> SharedWriteCompletionViewMock {
        let committed = caller_commit.is_ok();
        // Never inspect uncommitted work from the writer or a different store.
        if self.pending.verify_reader_mock(reader).await.is_err() {
            return completion(committed, false, &self.requests, None);
        }
        match self.pending.read_committed_on_connection_mock(reader).await {
            Ok(saved) => completion(committed, true, &self.requests, Some(&saved)),
            Err(_) => {
                let saved = match ProposedLedgerReaderMock::new(&self.pending.namespace) {
                    Ok(r) => r
                        .load_on_connection_mock(reader, &self.pending.owner)
                        .await
                        .ok(),
                    Err(_) => None,
                };
                completion(committed, false, &self.requests, saved.as_ref())
            }
        }
    }
}
/// Persistent trusted-backend checkpoint, scoped to database/namespace/owner.
/// Contains full original evidence. Not a public token, signature or receiver ACK.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedBusinessRecordCursorMock {
    namespace: String,
    database_scope: DatabaseScope,
    cursor: BusinessRecordCursorMock,
}
#[derive(Serialize)]
pub struct SharedBusinessRecordPageMock {
    pub records: Vec<RawBusinessRecordMock>,
    pub next_cursor: SharedBusinessRecordCursorMock,
    pub has_more: bool,
}
fn page(
    namespace: &str,
    scope: &DatabaseScope,
    saved: &SubscriptionReadSnapshotMock,
    cursor: Option<&SharedBusinessRecordCursorMock>,
    limit: usize,
) -> WriteResult<SharedBusinessRecordPageMock> {
    if cursor.is_some_and(|c| c.namespace != namespace || &c.database_scope != scope) {
        return Err(ProposedLedgerWriteIssue::Business(
            RuntimeRecoveryIssue::InvalidCursor,
        ));
    }
    let got = saved.business_record_page_mock(cursor.map(|c| &c.cursor), limit)?;
    Ok(SharedBusinessRecordPageMock {
        records: got.records,
        has_more: got.has_more,
        next_cursor: SharedBusinessRecordCursorMock {
            namespace: namespace.into(),
            database_scope: scope.clone(),
            cursor: got.next_cursor,
        },
    })
}
fn startup_summary(saved: &SubscriptionReadSnapshotMock, now: &str) -> Result<Value> {
    let pending = saved.pending_confirmations_mock(now)?;
    Ok(json!({"fictional":true,"startup_record_observed":true,
        "subscription_storage_readable":true,"full_banking_system_recovered":false,
        "subscription_count":saved.list_subscriptions_mock()?.items.len(),
        "business_record_count":saved.business_records_mock().len(),
        "pending":pending.items.iter().map(|p|json!({"action":p.action,"state":p.state,"expires_at":p.expires_at})).collect::<Vec<_>>(),
        "notification_sent":false,"merchant_cancellation_confirmed":false}))
}
impl PendingProposedLedgerWriteMock {
    fn check_startup_write_mock(&self) -> WriteResult<()> {
        let j: Journal = serde_json::from_value(self.expected.clone())
            .map_err(|_| ProposedLedgerWriteIssue::Conflict)?;
        if !matches!(j.commands.last(), Some(Command::Restart)) {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        Ok(())
    }
    /// Only a saved explicit startup action may produce this readback summary.
    pub async fn read_committed_startup_summary_on_connection_mock(
        &self,
        reader: &mut PgConnection,
        now: &str,
    ) -> WriteResult<Value> {
        self.check_startup_write_mock()?;
        let saved = self.read_committed_on_connection_mock(reader).await?;
        Ok(startup_summary(&saved, now)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    fn snap(n: usize) -> SubscriptionReadSnapshotMock {
        let mut j = super::super::super::raw_records_tests::actual_fixture_journal();
        j.commands.truncate(n);
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            j.commands.len() as i64 + 1,
            value(&j).unwrap(),
        )
        .unwrap()
    }
    fn work(n: usize, base: usize) -> ProposedLedgerWorkMock {
        let s = snap(n);
        let b = snap(base);
        ProposedLedgerWorkMock {
            namespace: "sub_write_check_recovery".into(),
            database_scope: scope(),
            base_version: b.version,
            base_journal: value(&b.journal).unwrap(),
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
    fn scope() -> DatabaseScope {
        ("fictional-db".into(), Some("127.0.0.1".into()), Some(5432))
    }
    fn requests(s: &SubscriptionReadSnapshotMock) -> Vec<OriginalOperationRequestMock> {
        s.business_records_mock()
            .iter()
            .map(OriginalOperationRequestMock::from_record)
            .collect::<Result<_>>()
            .unwrap()
    }
    fn safe(v: &Value) {
        let text = v.to_string();
        for hidden in [
            "user_demo",
            "session_ref",
            "idempotency_key",
            "original_binding",
            "original_command",
            "namespace",
            "database_scope",
            "subscription_id",
            "journal",
        ] {
            assert!(!text.contains(hidden), "{hidden}");
        }
    }
    #[test]
    fn newly_completed_requests_exclude_saved_history_and_drafts() {
        let w = work(8, 2);
        let req = w.new_original_requests_mock().unwrap();
        assert_eq!(req.len(), 3);
        assert_eq!(work(5, 4).new_original_requests_mock().unwrap().len(), 0);
        assert_eq!(work(8, 8).new_original_requests_mock().unwrap().len(), 0);
        let mut bad = work(8, 8);
        bad.poisoned = true;
        assert!(bad.new_original_requests_mock().is_err());
        let mut bad = work(4, 4);
        bad.base_version = 99;
        assert!(bad.new_original_requests_mock().is_err());
    }
    #[test]
    fn verified_completion_never_leaks_backend_evidence_or_enables_writes() {
        let s = snap(8);
        let v = value(&completion(true, true, &requests(&s), Some(&s))).unwrap();
        safe(&v);
        assert_eq!(v["state"], "COMMITTED_VERIFIED");
        assert_eq!(v["continue_writes"], false);
        assert_eq!(v["business_reexecuted"], false);
        assert_eq!(v["original_results"].as_array().unwrap().len(), 4);
    }
    #[test]
    fn uncertain_commit_stays_read_only_even_when_all_receipts_exist() {
        let s = snap(8);
        let v = value(&completion(false, true, &requests(&s), Some(&s))).unwrap();
        safe(&v);
        assert_eq!(v["state"], "READ_ONLY_RECOVERY");
        assert_eq!(v["reason"], "COMMIT_RESULT_UNCERTAIN");
        assert_eq!(v["saved_prefix_verified"], true);
        assert_eq!(v["continue_writes"], false);
        assert!(v["original_results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] != "NEEDS_REVIEW"));
    }
    #[test]
    fn absent_or_unavailable_readback_never_claims_business_success() {
        let full = snap(8);
        let empty = snap(0);
        for saved in [None, Some(&empty)] {
            let v = value(&completion(true, false, &requests(&full), saved)).unwrap();
            safe(&v);
            assert_eq!(v["state"], "READ_ONLY_RECOVERY");
            assert_eq!(v["saved_prefix_verified"], false);
            assert!(v["original_results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["status"]
                    == if saved.is_some() {
                        "NEEDS_REVIEW"
                    } else {
                        "UNAVAILABLE"
                    }));
        }
        let v = value(&completion(false, false, &[], None)).unwrap();
        assert_eq!(v["state"], "READ_ONLY_RECOVERY");
        assert_eq!(v["original_results"], json!([]));
    }
    #[test]
    fn partial_historical_readback_does_not_convert_whole_write_to_success() {
        let full = snap(8);
        let partial = snap(4);
        let v = value(&completion(true, false, &requests(&full), Some(&partial))).unwrap();
        assert_eq!(v["state"], "READ_ONLY_RECOVERY");
        assert_eq!(v["original_results"][0]["status"], "REGISTRATION_RECORDED");
        assert_eq!(v["original_results"][3]["status"], "NEEDS_REVIEW");
    }
    #[test]
    fn shared_pages_resume_after_serialization_and_append_without_changing_facts() {
        let early = snap(4);
        let full = snap(8);
        let before = value(&full.journal).unwrap();
        let first = page("fixture", &scope(), &early, None, 1).unwrap();
        assert_eq!(first.records.len(), 1);
        assert!(first.has_more);
        let cursor: SharedBusinessRecordCursorMock =
            serde_json::from_value(value(&first.next_cursor).unwrap()).unwrap();
        let second = page("fixture", &scope(), &full, Some(&cursor), 10).unwrap();
        assert_eq!(second.records.len(), 3);
        assert!(!second.has_more);
        assert_eq!(first.records[0], full.business_records_mock()[0]);
        assert_eq!(second.records[0], full.business_records_mock()[1]);
        let end = page("fixture", &scope(), &full, Some(&second.next_cursor), 10).unwrap();
        assert!(end.records.is_empty());
        assert_eq!(value(&full.journal).unwrap(), before);
    }
    #[test]
    fn shared_cursor_rejects_foreign_store_owner_truncation_and_changed_history() {
        let full = snap(8);
        let first = page("fixture", &scope(), &full, None, 2).unwrap();
        let c = &first.next_cursor;
        assert!(page("other", &scope(), &full, Some(c), 2).is_err());
        assert!(page(
            "fixture",
            &("other-db".into(), None, None),
            &full,
            Some(c),
            2
        )
        .is_err());
        assert!(page("fixture", &scope(), &snap(2), Some(c), 2).is_err());
        let mut raw = value(c).unwrap();
        raw["cursor"]["owner"] = json!("other");
        let wrong: SharedBusinessRecordCursorMock = serde_json::from_value(raw).unwrap();
        assert!(page("fixture", &scope(), &full, Some(&wrong), 2).is_err());
        let mut raw = value(c).unwrap();
        raw["cursor"]["exported_prefix"][0]["original_command"]["receipt"]["message"] =
            json!("changed");
        let wrong: SharedBusinessRecordCursorMock = serde_json::from_value(raw).unwrap();
        assert!(page("fixture", &scope(), &full, Some(&wrong), 2).is_err());
    }
    #[test]
    fn page_bounds_and_prepare_only_history_do_not_create_success_events() {
        assert!(page("fixture", &scope(), &snap(8), None, 0).is_err());
        assert!(page("fixture", &scope(), &snap(8), None, 1001).is_err());
        assert!(page("fixture", &scope(), &snap(1), None, 10)
            .unwrap()
            .records
            .is_empty());
        assert_eq!(
            page("fixture", &scope(), &snap(5), None, 10)
                .unwrap()
                .records
                .len(),
            2
        );
    }
    #[tokio::test]
    async fn explicit_startup_preserves_business_history_preferences_and_invalidates_only_pending()
    {
        let mut w = work(5, 5);
        let before = snap(5);
        let target = before.ledger_mock().list_mock("user_demo").unwrap()[0]
            .subscription_id
            .clone();
        let ledger = value(&before.ledger_mock().list_mock("user_demo").unwrap()).unwrap();
        let plan = value(&before.reminder_plan_mock(&target).unwrap()).unwrap();
        w.invalidate_pending_for_restart_mock().await.unwrap();
        let restored = SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            w.runtime.version,
            value(&w.runtime.journal).unwrap(),
        )
        .unwrap();
        assert_eq!(
            value(&restored.ledger_mock().list_mock("user_demo").unwrap()).unwrap(),
            ledger
        );
        assert_eq!(
            value(&restored.reminder_plan_mock(&target).unwrap()).unwrap(),
            plan
        );
        assert_eq!(
            restored.business_records_mock(),
            before.business_records_mock()
        );
        assert!(w.new_original_requests_mock().unwrap().is_empty());
        let shown = startup_summary(&restored, NOW).unwrap();
        safe(&shown);
        assert_eq!(shown["full_banking_system_recovered"], false);
        assert!(shown["pending"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["state"] == "INVALIDATED"));
    }
    #[test]
    fn ordinary_page_and_summary_reads_do_not_append_restart() {
        let s = snap(5);
        let before = value(&s.journal).unwrap();
        page("fixture", &scope(), &s, None, 10).unwrap();
        let _ = s.pending_confirmations_mock(NOW).unwrap();
        assert_eq!(before, value(&s.journal).unwrap());
        assert!(startup_summary(&s, "not-a-time").is_err());
    }
    #[test]
    fn ordinary_write_cannot_claim_startup_readback_and_conflicts_stay_safe() {
        let s = snap(8);
        let mut pending = PendingProposedLedgerWriteMock {
            namespace: "fixture".into(),
            database_scope: scope(),
            owner: "user_demo".into(),
            writer_pid: 123,
            expected: value(&s.journal).unwrap(),
            fresh_reminder_reservation: false,
        };
        assert!(pending.check_startup_write_mock().is_err());
        pending.expected["commands"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"Restart"}));
        pending.check_startup_write_mock().unwrap();
        let mut raw = value(&requests(&s)[0]).unwrap();
        raw["original_binding"]["context"]["session_ref"] = json!("changed");
        let changed: OriginalOperationRequestMock = serde_json::from_value(raw).unwrap();
        let v = value(&completion(true, false, &[changed], Some(&s))).unwrap();
        safe(&v);
        assert_eq!(v["original_results"][0]["status"], "REQUEST_CONFLICT");
        assert_eq!(v["state"], "READ_ONLY_RECOVERY");
    }
}
