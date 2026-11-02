//! Read saved reminder facts only. A lookup is never permission to send/retry.
use super::*;
use crate::reminder_mock::{DeliveryOutcome, ReminderAttemptStateMock, ReminderKind};

/// Trusted backend evidence, not an authenticated request or public attempt ID.
/// The complete saved prefix prevents matching a different history at the same
/// position. Never expose this serialized object to the model/customer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalReminderRequestMock {
    owner: String,
    position: usize,
    prefix: Value,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReminderBatchStateMock {
    AwaitingResult,
    InterruptedNeedsReview,
    ResultRecorded,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReminderHistoryItemMock {
    pub subscription_id: String,
    pub kind: ReminderKind,
    pub cycle_ref: String,
    pub expected_date: String,
    pub state: ReminderAttemptStateMock,
}
#[derive(Clone, Debug, Serialize)]
pub struct OriginalReminderResultMock {
    pub fictional: bool,
    pub notification_sent: bool,
    pub source_position: usize,
    pub reserved_at: String,
    pub batch_state: ReminderBatchStateMock,
    pub items: Vec<ReminderHistoryItemMock>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReminderReviewItemMock {
    pub source_position: usize,
    pub reserved_at: String,
    pub reason: &'static str,
    pub item: ReminderHistoryItemMock,
}

/// Dates, allow-listed status/kind and fixed explanations only. No owner,
/// operation, subscription, cycle, journal, position, raw request or receipt.
#[derive(Debug, Serialize)]
pub struct ReminderQueryItemViewMock {
    expected_date: String,
    reserved_at: Option<String>,
    kind: &'static str,
    status: &'static str,
    message: &'static str,
}
#[derive(Debug, Serialize)]
pub struct ReminderQueryViewMock {
    fictional: bool,
    notification_sent: bool,
    items: Vec<ReminderQueryItemViewMock>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ReminderReadQueryMock {
    DueReminders {
        now: String,
    },
    ReminderReviews {},
    ReminderHistory {},
    OriginalReminder {
        request: OriginalReminderRequestMock,
    },
}

fn kind_label(kind: ReminderKind) -> &'static str {
    match kind {
        ReminderKind::ChargeAdvance => "普通续费提醒",
        ReminderKind::CancellationCheck => "取消意图后的扣款核对",
    }
}
fn view_item(item: &ReminderHistoryItemMock, at: Option<String>) -> ReminderQueryItemViewMock {
    let (status, message) = match item.state {
        ReminderAttemptStateMock::SimulatedSuccess => (
            "SIMULATED_SUCCESS",
            "本次模拟提醒处理成功；不代表真实消息已送达。",
        ),
        ReminderAttemptStateMock::RetryExhausted => (
            "RETRY_EXHAUSTED",
            "本周期的补发次数已用完；未确认真实消息送达，请核对提醒及扣款安排。",
        ),
        ReminderAttemptStateMock::Failed => (
            "FAILED",
            "本次模拟处理明确失败；查询不会重试，后续按原提醒规则处理。",
        ),
        _ => (
            "NEEDS_REVIEW",
            "这次提醒结果待核对；不能当成已送达，也不要据此重新发送。",
        ),
    };
    ReminderQueryItemViewMock {
        expected_date: item.expected_date.clone(),
        reserved_at: at,
        kind: kind_label(item.kind),
        status,
        message,
    }
}

impl SubscriptionReadSnapshotMock {
    pub fn original_reminder_request_mock(
        &self,
        position: usize,
    ) -> Result<OriginalReminderRequestMock> {
        if !matches!(
            self.journal.commands.get(position),
            Some(Command::ReserveScan { .. })
        ) {
            return Err(RuntimeRecoveryIssue::NotFound);
        }
        let mut prefix = self.journal.clone();
        prefix.commands.truncate(position + 1);
        Ok(OriginalReminderRequestMock {
            owner: self.owner.clone(),
            position,
            prefix: value(&prefix)?,
        })
    }
    pub fn original_reminder_result_mock(
        &self,
        request: &OriginalReminderRequestMock,
    ) -> Result<OriginalReminderResultMock> {
        if request.owner != self.owner {
            return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
        }
        let actual = self.original_reminder_request_mock(request.position)?;
        if request.prefix != actual.prefix {
            return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
        }
        self.reminder_batch_at(request.position)
    }
    fn reminder_batch_at(&self, position: usize) -> Result<OriginalReminderResultMock> {
        let Some(Command::ReserveScan { now, messages, .. }) = self.journal.commands.get(position)
        else {
            return Err(RuntimeRecoveryIssue::NotFound);
        };
        let mut state = ReminderBatchStateMock::AwaitingResult;
        let mut outcomes = None;
        for command in self.journal.commands.iter().skip(position + 1) {
            match command {
                Command::ResolveScan { outcomes: o, .. } => {
                    state = ReminderBatchStateMock::ResultRecorded;
                    outcomes = Some(o);
                    break;
                }
                Command::Restart => {
                    state = ReminderBatchStateMock::InterruptedNeedsReview;
                    break;
                }
                Command::ReserveScan { .. } => return Err(RuntimeRecoveryIssue::InvalidJournal),
                _ => {}
            }
        }
        let required = |v: &Value, key: &str| {
            v[key]
                .as_str()
                .map(String::from)
                .ok_or(RuntimeRecoveryIssue::InvalidJournal)
        };
        let items = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let kind = match m["kind"].as_str() {
                    Some("CHARGE_ADVANCE") => ReminderKind::ChargeAdvance,
                    Some("CANCELLATION_CHECK") => ReminderKind::CancellationCheck,
                    _ => return Err(RuntimeRecoveryIssue::InvalidJournal),
                };
                let outcome = outcomes
                    .map(|o| {
                        o.get(i)
                            .copied()
                            .ok_or(RuntimeRecoveryIssue::InvalidJournal)
                    })
                    .transpose()?;
                Ok(ReminderHistoryItemMock {
                    subscription_id: required(m, "subscription_id")?,
                    kind,
                    cycle_ref: required(m, "cycle_ref")?,
                    expected_date: required(m, "expected_date")?,
                    state: outcome.unwrap_or(DeliveryOutcome::Unknown).into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(OriginalReminderResultMock {
            fictional: true,
            notification_sent: false,
            source_position: position,
            reserved_at: now.clone(),
            batch_state: state,
            items,
        })
    }
    pub fn reminder_history_mock(&self) -> Result<Vec<OriginalReminderResultMock>> {
        self.journal
            .commands
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, Command::ReserveScan { .. }))
            .map(|(i, _)| self.reminder_batch_at(i))
            .collect()
    }
    pub fn reminders_needing_review_mock(&self) -> Result<Vec<ReminderReviewItemMock>> {
        Ok(self
            .reminder_history_mock()?
            .into_iter()
            .flat_map(|batch| {
                let reason = match batch.batch_state {
                    ReminderBatchStateMock::AwaitingResult => "RESERVATION_UNRESOLVED",
                    ReminderBatchStateMock::InterruptedNeedsReview => "RESTARTED_WITHOUT_RESULT",
                    ReminderBatchStateMock::ResultRecorded => "RESULT_UNKNOWN",
                };
                batch
                    .items
                    .into_iter()
                    .filter(|i| i.state == ReminderAttemptStateMock::NeedsReview)
                    .map(move |item| ReminderReviewItemMock {
                        source_position: batch.source_position,
                        reserved_at: batch.reserved_at.clone(),
                        reason,
                        item,
                    })
            })
            .collect())
    }
    pub fn reminder_read_view_mock(
        &self,
        query: &ReminderReadQueryMock,
    ) -> Result<ReminderQueryViewMock> {
        let items=match query {
            ReminderReadQueryMock::DueReminders {now}=>self.due_reminders_mock(now)?.messages.into_iter().map(|m|
                ReminderQueryItemViewMock {expected_date:m.expected_date.to_string(),reserved_at:None,
                    kind:kind_label(m.kind),status:"PLANNED",message:"按当前资料列入待提醒计划；尚未领取或发送。商户和服务名称仍需按可信资料核对。"}
            ).collect(),
            ReminderReadQueryMock::ReminderReviews {} =>self.reminders_needing_review_mock()?.iter()
                .map(|r|view_item(&r.item,Some(r.reserved_at.clone()))).collect(),
            ReminderReadQueryMock::ReminderHistory {} =>self.reminder_history_mock()?.iter().flat_map(|b|
                b.items.iter().map(|i|view_item(i,Some(b.reserved_at.clone())))).collect(),
            ReminderReadQueryMock::OriginalReminder {request}=> {
                let batch=self.original_reminder_result_mock(request)?;
                batch.items.iter().map(|i|view_item(i,Some(batch.reserved_at.clone()))).collect()
            }
        };
        Ok(ReminderQueryViewMock {
            fictional: true,
            notification_sent: false,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    fn initial() -> Journal {
        super::super::raw_records_tests::actual_fixture_journal()
    }
    fn read(j: &Journal) -> SubscriptionReadSnapshotMock {
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            j.commands.len() as i64 + 1,
            value(j).unwrap(),
        )
        .unwrap()
    }
    fn reserve(mut j: Journal) -> (Journal, usize) {
        let s = read(&j);
        let mut sink = ReserveSink::default();
        let report = s
            .state
            .reminders
            .clone()
            .scan_mock(&s.state.ledger, "user_demo", NOW, &mut sink)
            .unwrap();
        let pos = j.commands.len();
        j.commands.push(Command::ReserveScan {
            reminder_rules: None,
            now: NOW.into(),
            messages: sink.messages.iter().map(|m| value(m).unwrap()).collect(),
            report: value(&report).unwrap(),
        });
        (j, pos)
    }
    fn resolve(mut j: Journal, outcome: DeliveryOutcome) -> Journal {
        let s = read(&j);
        let (_, base, count) = s.state.scan_base.as_ref().unwrap();
        let outcomes = vec![outcome; *count];
        let mut sink = RecordedSink {
            outcomes: outcomes.clone(),
            index: 0,
        };
        let report = base
            .clone()
            .scan_mock(&s.state.ledger, "user_demo", NOW, &mut sink)
            .unwrap();
        j.commands.push(Command::ResolveScan {
            now: NOW.into(),
            outcomes,
            report: value(&report).unwrap(),
        });
        j
    }
    #[test]
    fn exact_original_reservation_lookup_is_readonly_and_waits_for_result() {
        let (j, pos) = reserve(initial());
        let s = read(&j);
        let request = s.original_reminder_request_mock(pos).unwrap();
        let before = value(&s.journal).unwrap();
        let got = s.original_reminder_result_mock(&request).unwrap();
        assert_eq!(got.batch_state, ReminderBatchStateMock::AwaitingResult);
        assert_eq!(got.items[0].state, ReminderAttemptStateMock::NeedsReview);
        assert_eq!(before, value(&s.journal).unwrap());
        assert!(!got.notification_sent);
    }
    #[test]
    fn original_success_remains_original_after_restart() {
        let (mut j, pos) = reserve(initial());
        let request = read(&j).original_reminder_request_mock(pos).unwrap();
        j = resolve(j, DeliveryOutcome::SimulatedSuccess);
        j.commands.push(Command::Restart);
        let s = read(&j);
        let got = s.original_reminder_result_mock(&request).unwrap();
        assert_eq!(got.batch_state, ReminderBatchStateMock::ResultRecorded);
        assert_eq!(
            got.items[0].state,
            ReminderAttemptStateMock::SimulatedSuccess
        );
        assert!(s.reminders_needing_review_mock().unwrap().is_empty());
    }
    #[test]
    fn changed_prefix_or_other_owner_cannot_match_original() {
        let (j, pos) = reserve(initial());
        let s = read(&j);
        let request = s.original_reminder_request_mock(pos).unwrap();
        let mut wrong = request.clone();
        wrong.owner = "someone-else".into();
        assert!(matches!(
            s.original_reminder_result_mock(&wrong),
            Err(RuntimeRecoveryIssue::OriginalRequestConflict)
        ));
        wrong = request;
        wrong.prefix["initial"]["snapshot_id"] = serde_json::json!("different-history");
        assert!(matches!(
            s.original_reminder_result_mock(&wrong),
            Err(RuntimeRecoveryIssue::OriginalRequestConflict)
        ));
    }
    #[test]
    fn missing_or_non_reservation_position_is_not_a_result() {
        let (j, _) = reserve(initial());
        let s = read(&j);
        assert!(s.original_reminder_request_mock(0).is_err());
        assert!(s.original_reminder_request_mock(usize::MAX).is_err());
    }
    #[test]
    fn restart_does_not_attach_next_empty_batch_result_to_old_unknown() {
        let (mut j, pos) = reserve(initial());
        let request = read(&j).original_reminder_request_mock(pos).unwrap();
        j.commands.push(Command::Restart);
        let (j, _) = reserve(j);
        let j = resolve(j, DeliveryOutcome::SimulatedSuccess);
        let s = read(&j);
        let old = s.original_reminder_result_mock(&request).unwrap();
        assert_eq!(
            old.batch_state,
            ReminderBatchStateMock::InterruptedNeedsReview
        );
        assert_eq!(old.items[0].state, ReminderAttemptStateMock::NeedsReview);
        let review = s.reminders_needing_review_mock().unwrap();
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].reason, "RESTARTED_WITHOUT_RESULT");
    }
    #[test]
    fn review_list_distinguishes_pending_and_recorded_unknown_and_excludes_failure() {
        for outcome in [
            None,
            Some(DeliveryOutcome::Unknown),
            Some(DeliveryOutcome::DefiniteFailure),
        ] {
            let (mut j, _) = reserve(initial());
            if let Some(o) = outcome {
                j = resolve(j, o);
            }
            let got = read(&j).reminders_needing_review_mock().unwrap();
            if outcome == Some(DeliveryOutcome::DefiniteFailure) {
                assert!(got.is_empty());
            } else {
                assert_eq!(got.len(), 1);
                assert_eq!(
                    got[0].reason,
                    if outcome.is_none() {
                        "RESERVATION_UNRESOLVED"
                    } else {
                        "RESULT_UNKNOWN"
                    }
                );
            }
        }
    }
    #[test]
    fn due_user_view_is_planned_not_the_dry_run_unknown_projection() {
        let s = read(&initial());
        let v = value(
            &s.reminder_read_view_mock(&ReminderReadQueryMock::DueReminders { now: NOW.into() })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["items"][0]["status"], "PLANNED");
        assert_eq!(v["notification_sent"], false);
        assert!(!v.to_string().contains("NEEDS_REVIEW"));
    }
    #[test]
    fn every_user_view_contains_only_allowlisted_fields_and_fixed_text() {
        let (j, pos) = reserve(initial());
        let s = read(&j);
        let request = s.original_reminder_request_mock(pos).unwrap();
        let before = value(&s.journal).unwrap();
        for q in [
            ReminderReadQueryMock::DueReminders { now: NOW.into() },
            ReminderReadQueryMock::ReminderReviews {},
            ReminderReadQueryMock::ReminderHistory {},
            ReminderReadQueryMock::OriginalReminder { request },
        ] {
            let v = value(&s.reminder_read_view_mock(&q).unwrap()).unwrap();
            assert_eq!(v.as_object().unwrap().len(), 3);
            for item in v["items"].as_array().unwrap() {
                assert_eq!(item.as_object().unwrap().len(), 5);
                for key in item.as_object().unwrap().keys() {
                    assert!(
                        ["expected_date", "reserved_at", "kind", "status", "message"]
                            .contains(&key.as_str())
                    );
                }
            }
            let text = v.to_string();
            for hidden in [
                "user_demo",
                "fictional-session",
                "fictional-key",
                "source_position",
                "cycle_ref",
                "subscription_id",
                "journal",
            ] {
                assert!(!text.contains(hidden), "{hidden}");
            }
        }
        assert_eq!(before, value(&s.journal).unwrap());
    }
    #[test]
    fn query_parser_rejects_unknown_fields_and_unknown_commands() {
        for bad in [
            r#"{"kind":"ReminderReviews","extra":1}"#,
            r#"{"kind":"DueReminders","now":"x","send":true}"#,
            r#"{"kind":"SendReminder"}"#,
        ] {
            assert!(serde_json::from_str::<ReminderReadQueryMock>(bad).is_err());
        }
        assert!(
            serde_json::from_str::<ReminderReadQueryMock>(r#"{"kind":"ReminderHistory"}"#).is_ok()
        );
    }
}
