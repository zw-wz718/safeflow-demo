//! Consumption input plus current saved ledger. No new recognition rules.
use super::*;
use crate::managed_ledger_mock::Disposition;
use crate::recognition::{Cycle, CycleStatus};
use crate::reminder_mock::{Prediction, SettingsPreviewMock};

#[derive(Debug, Serialize)]
pub struct CandidateViewMock {
    pub selection_index: usize,
    pub disposition: Disposition,
    pub reason: &'static str,
    pub message: &'static str,
    pub charges: Vec<ChargePreviewMock>,
    pub matching_cycles: Vec<Cycle>,
    pub matched_subscription_ids: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct DiscoveryViewMock {
    pub fictional: bool,
    pub matching_executed: bool,
    pub reason: &'static str,
    pub groups: Vec<CandidateViewMock>,
}
#[derive(Debug, Serialize)]
pub struct DateSuggestionViewMock {
    pub fictional: bool,
    pub state: &'static str,
    pub reason: Option<&'static str>,
    pub prediction: Option<Prediction>,
    pub suggested_schedule: Option<Schedule>,
    pub message: &'static str,
}
impl DateSuggestionViewMock {
    fn needs_input(reason: &'static str) -> Self {
        Self {
            fictional: true,
            state: "NEEDS_INPUT",
            reason: Some(reason),
            prediction: None,
            suggested_schedule: None,
            message: "日期建议不可用，请核对消费资料、周期或日期；不猜日期、不保存提醒设置。",
        }
    }
}
impl SubscriptionReadSnapshotMock {
    fn check_discovery_owner(&self, context: &MockContext) -> Result<()> {
        if context.user_id != self.owner {
            return Err(RuntimeRecoveryIssue::Business(
                MockIssue::ResourceUnavailable,
            ));
        }
        Ok(())
    }
    pub fn identify_subscriptions_mock(
        &self,
        context: &MockContext,
        input: &Snapshot,
    ) -> Result<DiscoveryViewMock> {
        self.check_discovery_owner(context)?;
        let review = self
            .state
            .ledger
            .discover_mock(context, input)
            .map_err(RuntimeRecoveryIssue::Business)?;
        Ok(DiscoveryViewMock {
            fictional: true,
            matching_executed: review.matching_executed,
            reason: review.reason,
            groups: review
                .groups
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    let message = match item.disposition {
                        Disposition::NotInLedger => "发现疑似订阅，需要另行核对并确认登记。",
                        Disposition::AlreadyManaged => "已有对应登记，不重复登记。",
                        _ => public_review_message(item.reason),
                    };
                    CandidateViewMock {
                        selection_index: index,
                        disposition: item.disposition,
                        reason: item.reason,
                        message,
                        charges: item
                            .group
                            .actual_charges
                            .iter()
                            .map(|charge| ChargePreviewMock {
                                amount_minor_units: charge.amount_minor_units,
                                currency: item.group.currency,
                                completed_at: charge.completed_at.clone(),
                            })
                            .collect(),
                        matching_cycles: item
                            .group
                            .cycles
                            .iter()
                            .filter(|c| c.status == CycleStatus::Matches)
                            .map(|c| c.cycle)
                            .collect(),
                        matched_subscription_ids: item.matched_subscription_ids,
                    }
                })
                .collect(),
        })
    }
    /// Exactly one target: candidate index OR existing subscription reference.
    pub fn suggest_reminder_date_mock(
        &self,
        context: &MockContext,
        input: &Snapshot,
        candidate_index: Option<usize>,
        subscription_id: Option<&str>,
        now: &str,
    ) -> Result<DateSuggestionViewMock> {
        self.check_discovery_owner(context)?;
        if candidate_index.is_some() == subscription_id.is_some() {
            return Ok(DateSuggestionViewMock::needs_input("TARGET_NOT_UNIQUE"));
        }
        let parse = |s| chrono::DateTime::parse_from_rfc3339(s);
        let (Ok(now), Ok(as_of)) = (parse(now), parse(&input.window.as_of)) else {
            return Ok(DateSuggestionViewMock::needs_input("TIME_INVALID"));
        };
        if as_of > now {
            return Ok(DateSuggestionViewMock::needs_input("TIME_INVALID"));
        }
        let prediction = if let Some(index) = candidate_index {
            crate::reminder_mock::predict_candidate_mock(&self.state.ledger, context, input, index)
        } else {
            crate::reminder_mock::predict_mock(
                &self.state.ledger,
                context,
                input,
                subscription_id.expect("exclusive target"),
            )
        };
        let prediction = match prediction {
            Ok(p) => p,
            Err(reason) => return Ok(DateSuggestionViewMock::needs_input(reason)),
        };
        let today = now
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).expect("business offset"))
            .date_naive();
        if prediction.expected_date < today {
            return Ok(DateSuggestionViewMock::needs_input("DATE_NEEDS_INPUT"));
        }
        let schedule = Schedule::from_prediction(&prediction);
        if let Err(reason) = schedule.validate() {
            return Ok(DateSuggestionViewMock::needs_input(reason));
        }
        Ok(DateSuggestionViewMock {
            fictional: true,
            state: "READY",
            reason: None,
            prediction: Some(prediction),
            suggested_schedule: Some(schedule),
            message: "依据已有扣款给出的日期建议，不是商户承诺；提醒设置仍需单独确认后保存。",
        })
    }
}
#[derive(Debug, Serialize)]
pub struct ReminderPreparationMock {
    pub fictional: bool,
    pub subscription_id: String,
    pub state: &'static str,
    pub date_suggestion: DateSuggestionViewMock,
    pub preview: Option<SettingsPreviewMock>,
    pub settings_saved: bool,
    /// Backend-only exact binding for the existing independent confirmation API.
    #[serde(skip)]
    pub confirmation_binding: Option<ScheduleBinding>,
}
impl SubscriptionRuntimeMock {
    #[allow(clippy::too_many_arguments)] // Explicit original binding, source, operation and time.
    pub async fn prepare_reminder_after_registration_mock(
        &mut self,
        context: &MockContext,
        input: &Snapshot,
        registration: &MockConfirmationBinding,
        operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> Result<ReminderPreparationMock> {
        if self.staged {
            return Err(RuntimeRecoveryIssue::StagedOperationUnsupported);
        }
        self.prepare_reminder_after_registration_inner_mock(
            context,
            input,
            registration,
            operation,
            now,
            expires,
        )
        .await
    }
    // Shared work is already confined to in-memory staging. The public standalone
    // method above retains its staged rejection; this path never owns a commit.
    pub(super) async fn prepare_reminder_after_registration_inner_mock(
        &mut self,
        context: &MockContext,
        input: &Snapshot,
        registration: &MockConfirmationBinding,
        operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> Result<ReminderPreparationMock> {
        if context.user_id != self.user || operation.user_id != self.user {
            return Err(RuntimeRecoveryIssue::Business(
                MockIssue::ResourceUnavailable,
            ));
        }
        self.state.guard("reminder", &operation, &self.user)?;
        let snapshot = SubscriptionReadSnapshotMock {
            owner: self.user.clone(),
            version: self.version,
            state: self.state.clone(),
            business_records: collect_business_records(&self.journal)?,
            journal: self.journal.clone(),
        };
        let request = OriginalOperationRequestMock::registration(registration)?;
        let receipt = match snapshot.original_result_mock(&request)? {
            OriginalResultMock::Recorded { original_receipt } => original_receipt,
            OriginalResultMock::NeedsReview => return Err(RuntimeRecoveryIssue::NotFound),
        };
        let id = receipt["subscription_id"]
            .as_str()
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?
            .to_owned();
        // This convenience path only initializes a plan; never overwrite an
        // existing user preference. Existing edits use the explicit settings API.
        if self
            .state
            .reminders
            .current_schedule_mock(&self.state.ledger, &self.user, &id)
            .is_ok()
        {
            return Err(RuntimeRecoveryIssue::Reminder(
                "SETTINGS_ALREADY_CONFIGURED",
            ));
        }
        let suggestion =
            snapshot.suggest_reminder_date_mock(context, input, None, Some(&id), now)?;
        let Some(schedule) = suggestion.suggested_schedule.clone() else {
            return Ok(ReminderPreparationMock {
                fictional: true,
                subscription_id: id,
                state: "NEEDS_INPUT",
                date_suggestion: suggestion,
                preview: None,
                settings_saved: false,
                confirmation_binding: None,
            });
        };
        let binding = self
            .prepare_settings_mock(operation, &id, schedule, now, expires)
            .await?;
        let preview = self.settings_preview_mock(&binding.context, now)?;
        Ok(ReminderPreparationMock {
            fictional: true,
            subscription_id: id,
            state: "AWAITING_CONFIRMATION",
            date_suggestion: suggestion,
            preview: Some(preview),
            settings_saved: false,
            confirmation_binding: Some(binding),
        })
    }
}
