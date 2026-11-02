//! One allow-listed shared-store read dispatcher. Never a public Tool or auth.
use super::*;
use serde_json::json;
#[path = "discovery_read_views.rs"]
mod discovery_reads;

#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum SharedBusinessReadQueryMock {
    IdentifySubscriptions {
        context: MockContext,
        input: Box<Snapshot>,
    },
    SuggestReminderDate {
        context: MockContext,
        input: Box<Snapshot>,
        candidate_index: Option<usize>,
        subscription_id: Option<String>,
        now: String,
    },
    SubscriptionDetails {
        subscription_id: String,
        now: String,
    },
    ListSubscriptions {},
    CancellationStatus {
        subscription_id: String,
    },
    ReminderPlan {
        subscription_id: String,
    },
    PendingConfirmations {
        now: String,
    },
    RegistrationPreview {
        draft_id: String,
        context: MockOperationContext,
        now: String,
    },
    IntentPreview {
        draft_id: String,
        context: MockOperationContext,
        now: String,
    },
    SettingsPreview {
        context: MockOperationContext,
        now: String,
    },
    Original {
        request: OriginalOperationRequestMock,
    },
    Initialization {
        request: LedgerInitializationRequestMock,
    },
}
#[derive(Deserialize)]
#[serde(untagged)]
pub enum SharedSubscriptionReadQueryMock {
    Business(SharedBusinessReadQueryMock),
    Reminder(ReminderReadQueryMock),
}
pub fn shared_query_error_view_mock(issue: ProposedLedgerWriteIssue) -> Value {
    let issue = match issue {
        ProposedLedgerWriteIssue::Business(e) => e,
        ProposedLedgerWriteIssue::Conflict => RuntimeRecoveryIssue::OriginalRequestConflict,
        // Missing owner/permission/data must never become a healthy empty list.
        _ => RuntimeRecoveryIssue::Unavailable,
    };
    let draft_status = match issue {
        RuntimeRecoveryIssue::Business(MockIssue::DraftExpired)
        | RuntimeRecoveryIssue::Reminder("DRAFT_EXPIRED") => {
            Some(("DRAFT_EXPIRED", "这份确认内容已过期，请重新准备后核对。"))
        }
        RuntimeRecoveryIssue::OperationAbandoned => {
            Some(("DRAFT_ABANDONED", "这份待确认内容已放弃，不能继续确认。"))
        }
        RuntimeRecoveryIssue::RestartRequiresNewOperation => Some((
            "DRAFT_INVALIDATED",
            "这份确认内容已失效，请重新准备后核对。",
        )),
        RuntimeRecoveryIssue::Business(MockIssue::StaleFacts | MockIssue::PlanConflict)
        | RuntimeRecoveryIssue::Reminder("STALE_FACTS" | "PLAN_CHANGED") => {
            Some(("DRAFT_CHANGED", "相关资料或确认内容已变化，请重新核对。"))
        }
        _ => None,
    };
    if let Some((status, message)) = draft_status {
        return json!({"fictional":true,"status":status,"message":message});
    }
    value(&OriginalOperationViewMock::issue_mock(issue)).expect("fixed safe view")
}
fn schedule_view(s: &Schedule) -> Value {
    json!({"cycle":s.cycle,"expected_date":s.expected_date.to_string(),"enabled":s.enabled,"days_before":s.days_before})
}
pub(super) fn business_view(
    s: &SubscriptionReadSnapshotMock,
    q: &SharedBusinessReadQueryMock,
) -> Result<Value> {
    match q {
        SharedBusinessReadQueryMock::IdentifySubscriptions { context, input } => {
            discovery_reads::identify(s, context, input)
        }
        SharedBusinessReadQueryMock::SuggestReminderDate {
            context,
            input,
            candidate_index,
            subscription_id,
            now,
        } => discovery_reads::date(
            s,
            context,
            input,
            *candidate_index,
            subscription_id.as_deref(),
            now,
        ),
        SharedBusinessReadQueryMock::SubscriptionDetails {
            subscription_id,
            now,
        } => discovery_reads::details(s, subscription_id, now),
        SharedBusinessReadQueryMock::ListSubscriptions {} => {
            let v = s.list_subscriptions_mock()?;
            Ok(
                json!({"fictional":true,"items":v.items.iter().map(|i|json!({"current_state":i.current_state,
                "service_data_state":i.service_data_state,"service_display_name":null,"message":i.message})).collect::<Vec<_>>()}),
            )
        }
        SharedBusinessReadQueryMock::CancellationStatus { subscription_id } => {
            let v = s.cancellation_status_mock(subscription_id)?;
            Ok(
                json!({"fictional":true,"current_state":v.current_state,"history":v.history,
                "merchant_cancellation_confirmed":false,"message":v.message}),
            )
        }
        SharedBusinessReadQueryMock::ReminderPlan { subscription_id } => {
            let v = s.reminder_plan_mock(subscription_id)?;
            Ok(
                json!({"fictional":true,"data_state":v.data_state,"schedule":v.schedule.as_ref().map(schedule_view),
                "active_kind":v.active_kind,"current_attempt":v.current_attempt,"current_attempt_count":v.current_attempt_count,"retries_remaining":v.retries_remaining,
                "saved_cycle_outcomes":v.saved_cycle_outcomes.iter().map(|a|json!({"kind":a.kind,"last_attempt_date":a.last_attempt_date,"state":a.state,"attempt_count":a.attempt_count,"retries_remaining":a.retries_remaining})).collect::<Vec<_>>(),
                "notification_sent":false,"message":v.message}),
            )
        }
        SharedBusinessReadQueryMock::PendingConfirmations { now } => {
            let v = s.pending_confirmations_mock(now)?;
            Ok(
                json!({"fictional":true,"items":v.items.iter().map(|i|json!({"action":i.action,"state":i.state,"expires_at":i.expires_at})).collect::<Vec<_>>()}),
            )
        }
        SharedBusinessReadQueryMock::RegistrationPreview {
            draft_id,
            context,
            now,
        } => value(&s.registration_preview_mock(draft_id, context, now)?),
        SharedBusinessReadQueryMock::IntentPreview {
            draft_id,
            context,
            now,
        } => {
            let v = s.intent_preview_mock(draft_id, context, now)?;
            Ok(
                json!({"fictional":true,"kind":v.kind,"before":v.before,"after":v.after,"expires_at":v.expires_at,"message":v.message}),
            )
        }
        SharedBusinessReadQueryMock::SettingsPreview { context, now } => {
            let v = s.settings_preview_mock(context, now)?;
            Ok(
                json!({"fictional":true,"before":v.before.as_ref().map(schedule_view),"after":schedule_view(&v.after),
                "changed_fields":v.changed_fields.iter().map(|field|match *field {
                    "anchor_date"|"period_index"=>"cycle",other=>other
                }).collect::<std::collections::BTreeSet<_>>(),"expires_at":v.expires_at,"message":v.message}),
            )
        }
        SharedBusinessReadQueryMock::Original { request } => value(&s.original_view_mock(request)?),
        SharedBusinessReadQueryMock::Initialization { .. } => {
            Err(RuntimeRecoveryIssue::OriginalRequestConflict)
        }
    }
}
impl ProposedLedgerStoreMock {
    /// Caller supplies a trusted namespace/owner and controls read isolation.
    /// Only SELECT-capable methods are dispatched; no fallback initialization.
    pub async fn query_on_connection_mock(
        &self,
        c: &mut PgConnection,
        owner: &str,
        q: &SharedSubscriptionReadQueryMock,
    ) -> WriteResult<Value> {
        if let SharedSubscriptionReadQueryMock::Business(
            SharedBusinessReadQueryMock::Initialization { request },
        ) = q
        {
            return match self
                .lookup_initialization_on_connection_mock(c, owner, request)
                .await?
            {
                Some(_) => Ok(
                    json!({"fictional":true,"status":"INITIALIZATION_RECORDED","message":"查到这份初始资料已保存；后续登记和提醒等记录保留，当前内容请查询台账。"}),
                ),
                None => Ok(
                    json!({"fictional":true,"status":"NEEDS_REVIEW","message":"尚未查到这份初始资料的保存结果，请核对；不要据此自动重新创建台账。"}),
                ),
            };
        }
        let s = ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(c, owner)
            .await
            .map_err(ProposedLedgerWriteIssue::Read)?;
        match q {
            SharedSubscriptionReadQueryMock::Business(q) => {
                business_view(&s, q).map_err(Into::into)
            }
            SharedSubscriptionReadQueryMock::Reminder(q) => {
                value(&s.reminder_read_view_mock(q)?).map_err(Into::into)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    fn fixture(count: Option<usize>) -> SubscriptionReadSnapshotMock {
        let mut j = super::super::super::raw_records_tests::actual_fixture_journal();
        if let Some(n) = count {
            j.commands.truncate(n);
        }
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            j.commands.len() as i64 + 1,
            value(&j).unwrap(),
        )
        .unwrap()
    }
    fn safe(v: &Value) {
        let s = v.to_string();
        for k in [
            "user_demo",
            "session_ref",
            "idempotency_key",
            "original_context",
            "original_binding",
            "source_position",
            "cycle_ref",
            "anchor_date",
            "period_index",
            "subscription_id",
            "postgres://",
        ] {
            assert!(!s.contains(k), "{k}");
        }
    }
    #[test]
    fn shared_parser_accepts_business_and_legacy_reminder_requests_and_rejects_extra() {
        for q in [
            r#"{"kind":"ListSubscriptions"}"#,
            r#"{"kind":"CancellationStatus","subscription_id":"x"}"#,
            r#"{"kind":"ReminderPlan","subscription_id":"x"}"#,
            r#"{"kind":"PendingConfirmations","now":"x"}"#,
        ] {
            assert!(matches!(
                serde_json::from_str::<SharedSubscriptionReadQueryMock>(q).unwrap(),
                SharedSubscriptionReadQueryMock::Business(_)
            ));
        }
        assert!(matches!(
            serde_json::from_str::<SharedSubscriptionReadQueryMock>(
                r#"{"kind":"ReminderHistory"}"#
            )
            .unwrap(),
            SharedSubscriptionReadQueryMock::Reminder(_)
        ));
        for bad in [
            r#"{"kind":"ListSubscriptions","write":true}"#,
            r#"{"kind":"ReminderReviews","send":true}"#,
            r#"{"kind":"Initialize"}"#,
        ] {
            assert!(serde_json::from_str::<SharedSubscriptionReadQueryMock>(bad).is_err());
        }
    }
    #[test]
    fn current_queries_hide_backend_references_and_preserve_local_cancel_meaning() {
        let s = fixture(None);
        let id = s.ledger_mock().list_mock("user_demo").unwrap()[0]
            .subscription_id
            .clone();
        let before = value(&s.journal).unwrap();
        for q in [
            SharedBusinessReadQueryMock::ListSubscriptions {},
            SharedBusinessReadQueryMock::CancellationStatus {
                subscription_id: id.clone(),
            },
            SharedBusinessReadQueryMock::ReminderPlan {
                subscription_id: id,
            },
        ] {
            safe(&business_view(&s, &q).unwrap());
        }
        let v = business_view(&s, &SharedBusinessReadQueryMock::ListSubscriptions {}).unwrap();
        assert!(v["items"][0]["service_display_name"].is_null());
        assert_eq!(before, value(&s.journal).unwrap());
    }
    #[test]
    fn pending_ready_expired_and_invalidated_are_readonly_safe_views() {
        let s = fixture(Some(1));
        let before = value(&s.journal).unwrap();
        let q = SharedBusinessReadQueryMock::PendingConfirmations { now: NOW.into() };
        let v = business_view(&s, &q).unwrap();
        safe(&v);
        assert_eq!(v["items"][0]["state"], "READY");
        let v = business_view(
            &s,
            &SharedBusinessReadQueryMock::PendingConfirmations {
                now: "2026-10-14T12:06:00+08:00".into(),
            },
        )
        .unwrap();
        assert_eq!(v["items"][0]["state"], "EXPIRED");
        assert_eq!(before, value(&s.journal).unwrap());
        let mut j = s.journal.clone();
        j.commands.push(Command::Restart);
        let s = SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            j.commands.len() as i64 + 1,
            value(&j).unwrap(),
        )
        .unwrap();
        let v = business_view(&s, &q).unwrap();
        assert_eq!(v["items"][0]["state"], "INVALIDATED");
        safe(&v);
    }
    #[test]
    fn all_three_confirmation_summaries_are_safe_and_do_not_confirm() {
        for (n, kind) in [(1, "register"), (3, "settings"), (5, "intent")] {
            let s = fixture(Some(n));
            let before = value(&s.journal).unwrap();
            let q = match s.journal.commands.last().unwrap() {
                Command::PrepareRegistration {
                    operation, binding, ..
                } => SharedBusinessReadQueryMock::RegistrationPreview {
                    draft_id: binding["draft_id"].as_str().unwrap().into(),
                    context: operation.clone(),
                    now: NOW.into(),
                },
                Command::PrepareSettings { context, .. } => {
                    SharedBusinessReadQueryMock::SettingsPreview {
                        context: context.clone(),
                        now: NOW.into(),
                    }
                }
                Command::PrepareIntent {
                    context, binding, ..
                } => SharedBusinessReadQueryMock::IntentPreview {
                    draft_id: binding["draft_id"].as_str().unwrap().into(),
                    context: context.clone(),
                    now: NOW.into(),
                },
                _ => panic!("unexpected fixture {kind}"),
            };
            safe(&business_view(&s, &q).unwrap());
            assert_eq!(before, value(&s.journal).unwrap());
        }
    }
    #[test]
    fn four_original_business_results_use_existing_full_binding_checks() {
        let s = fixture(None);
        for record in s.business_records_mock() {
            let request:OriginalOperationRequestMock=serde_json::from_value(json!({"command_kind":record.original_command["kind"],"original_binding":record.original_binding})).unwrap();
            let v = business_view(&s, &SharedBusinessReadQueryMock::Original { request }).unwrap();
            safe(&v);
            assert!(
                v["status"].as_str().unwrap().ends_with("RECORDED")
                    || v["status"] == "CANCELLATION_INTENT_REVOKED"
            );
        }
    }
    #[test]
    fn shared_errors_distinguish_invalid_drafts_and_never_echo_backend_errors() {
        for (e, status) in [
            (
                RuntimeRecoveryIssue::Business(MockIssue::DraftExpired),
                "DRAFT_EXPIRED",
            ),
            (RuntimeRecoveryIssue::OperationAbandoned, "DRAFT_ABANDONED"),
            (
                RuntimeRecoveryIssue::RestartRequiresNewOperation,
                "DRAFT_INVALIDATED",
            ),
        ] {
            let v = shared_query_error_view_mock(e.into());
            safe(&v);
            assert_eq!(v["status"], status);
        }
        let v = shared_query_error_view_mock(ProposedLedgerWriteIssue::Read(
            ProposedLedgerReadIssue::NotFound,
        ));
        assert_eq!(v["status"], "UNAVAILABLE");
        safe(&v);
    }
}
