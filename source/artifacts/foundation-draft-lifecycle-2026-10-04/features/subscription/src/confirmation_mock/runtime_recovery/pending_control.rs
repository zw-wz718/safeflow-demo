//! Pending workflow metadata only. No deletion or reversal of completed business.
use super::*;

struct Prepared {
    scope: &'static str,
    action: &'static str,
    context: MockOperationContext,
    binding: Value,
}
fn prepared(command: &Command) -> Option<Prepared> {
    let (scope, action, context, binding) = match command {
        Command::PrepareRegistration {
            operation, binding, ..
        } => ("ledger", "REGISTER", operation, binding),
        Command::PrepareIntent {
            context,
            binding,
            intent,
            ..
        } => (
            "ledger",
            if *intent == MockIntentKind::Record {
                "CANCEL_INTENT"
            } else {
                "REVOKE_INTENT"
            },
            context,
            binding,
        ),
        Command::PrepareSettings {
            context, binding, ..
        } => ("reminder", "SETTINGS", context, binding),
        _ => return None,
    };
    Some(Prepared {
        scope,
        action,
        context: context.clone(),
        binding: binding.clone(),
    })
}
pub(super) fn validate_reference(prior: &[Command], command: &Command) -> Result<()> {
    if let Command::AbandonPending {
        position,
        scope,
        context,
        binding,
        ..
    } = command
    {
        let original = prior
            .get(*position)
            .and_then(prepared)
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
        if original.scope != scope || &original.context != context || &original.binding != binding {
            return Err(RuntimeRecoveryIssue::InvalidJournal);
        }
    }
    Ok(())
}
pub(super) fn validate_abandon(
    state: &State,
    owner: &str,
    scope: &str,
    context: &MockOperationContext,
    binding: &Value,
    now: &str,
) -> Result<()> {
    state.guard(scope, context, owner)?;
    if !state.pending.contains(&(
        scope.into(),
        context.operation_id.clone(),
        context.idempotency_key.clone(),
    )) {
        return Err(RuntimeRecoveryIssue::Conflict);
    }
    let start = binding["prepared_at"]
        .as_str()
        .or_else(|| binding["as_of"].as_str())
        .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
    let parse = |v| {
        chrono::DateTime::parse_from_rfc3339(v)
            .map_err(|_| RuntimeRecoveryIssue::Business(MockIssue::InvalidContext))
    };
    if parse(now)? < parse(start)? {
        return Err(RuntimeRecoveryIssue::Business(MockIssue::InvalidContext));
    }
    if scope == "reminder" {
        let current = state
            .settings
            .get(&context.operation_id)
            .ok_or(RuntimeRecoveryIssue::Conflict)?;
        if &current.context != context || value(current)? != *binding {
            return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
        }
        match state
            .reminders
            .settings_preview_mock(&state.ledger, current, now)
        {
            Ok(_) | Err("DRAFT_EXPIRED" | "STALE_FACTS") => Ok(()),
            Err(reason) => Err(RuntimeRecoveryIssue::Reminder(reason)),
        }
    } else if scope == "ledger" {
        let draft = binding["draft_id"]
            .as_str()
            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
        let outcome = if binding.get("candidate").is_some() {
            let current = state
                .ledger
                .drafts
                .get(draft)
                .ok_or(RuntimeRecoveryIssue::Conflict)?;
            if value(current)? != *binding {
                return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
            }
            state
                .ledger
                .registration_preview_mock(draft, context, now)
                .map(|_| ())
        } else {
            let current = state
                .ledger
                .intent_drafts
                .get(draft)
                .ok_or(RuntimeRecoveryIssue::Conflict)?;
            if value(current)? != *binding {
                return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
            }
            state
                .ledger
                .intent_preview_mock(draft, context, now)
                .map(|_| ())
        };
        match outcome {
            Ok(())
            | Err(MockIssue::DraftExpired | MockIssue::StaleFacts | MockIssue::StateConflict) => {
                Ok(())
            }
            Err(issue) => Err(RuntimeRecoveryIssue::Business(issue)),
        }
    } else {
        Err(RuntimeRecoveryIssue::InvalidJournal)
    }
}

#[derive(Debug, Serialize)]
pub struct PendingConfirmationItemMock {
    pub source_position: usize,
    pub action: &'static str,
    pub state: &'static str,
    pub subscription_id: Option<String>,
    pub expires_at: String,
    /// Trusted backend selection context only, omitted from human/model JSON.
    #[serde(skip)]
    pub original_context: MockOperationContext,
}
#[derive(Debug, Serialize)]
pub struct PendingConfirmationsMock {
    pub fictional: bool,
    pub items: Vec<PendingConfirmationItemMock>,
}
fn pending_view(
    journal: &Journal,
    state: &State,
    owner: &str,
    now: &str,
) -> Result<PendingConfirmationsMock> {
    chrono::DateTime::parse_from_rfc3339(now)
        .map_err(|_| RuntimeRecoveryIssue::Business(MockIssue::InvalidContext))?;
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();
    for (position, command) in journal.commands.iter().enumerate() {
        let Some(data) = prepared(command) else {
            continue;
        };
        let identity = (
            data.scope.to_owned(),
            data.context.operation_id.clone(),
            data.context.idempotency_key.clone(),
        );
        if journal.commands.iter().any(|c| match c {
            Command::Register { context, .. } | Command::Intent { context, .. } => {
                data.scope == "ledger"
                    && context.operation_id == identity.1
                    && context.idempotency_key == identity.2
            }
            Command::Settings { binding, .. } => {
                data.scope == "reminder"
                    && binding["context"]["operation_id"] == identity.1
                    && binding["context"]["idempotency_key"] == identity.2
            }
            _ => false,
        }) {
            continue;
        }
        if !seen.insert((
            data.scope.to_owned(),
            value(&data.context)?.to_string(),
            data.binding.to_string(),
        )) {
            continue;
        }
        // Idempotent prepare replays can have several journal positions for one
        // identical draft. Display its abandonment once, regardless of which
        // exact, validated prepare position the backend used.
        let abandoned = journal.commands.iter().any(|c| {
            matches!(c,Command::AbandonPending{scope,context,binding,..}
                if scope == data.scope && context == &data.context && binding == &data.binding)
        });
        let replaced = journal.commands[position + 1..]
            .iter()
            .filter_map(prepared)
            .any(|next| {
                next.scope == data.scope
                    && next.context.operation_id == data.context.operation_id
                    && next.binding != data.binding
            });
        let restarted = journal.commands[position + 1..]
            .iter()
            .any(|c| matches!(c, Command::Restart));
        let status = if abandoned {
            "ABANDONED"
        } else if replaced {
            "SUPERSEDED"
        } else if restarted {
            "INVALIDATED"
        } else {
            if state.guard(data.scope, &data.context, owner).is_err() {
                "INVALIDATED"
            } else if data.scope == "reminder" {
                let current = state
                    .settings
                    .get(&data.context.operation_id)
                    .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
                match state
                    .reminders
                    .settings_preview_mock(&state.ledger, current, now)
                {
                    Ok(_) => "READY",
                    Err("DRAFT_EXPIRED") => "EXPIRED",
                    Err(_) => "INVALIDATED",
                }
            } else {
                let draft = data.binding["draft_id"]
                    .as_str()
                    .ok_or(RuntimeRecoveryIssue::InvalidJournal)?;
                let result = if data.action == "REGISTER" {
                    state
                        .ledger
                        .registration_preview_mock(draft, &data.context, now)
                        .map(|_| ())
                } else {
                    state
                        .ledger
                        .intent_preview_mock(draft, &data.context, now)
                        .map(|_| ())
                };
                match result {
                    Ok(()) => "READY",
                    Err(MockIssue::DraftExpired) => "EXPIRED",
                    Err(_) => "INVALIDATED",
                }
            }
        };
        items.push(PendingConfirmationItemMock {
            source_position: position,
            action: data.action,
            state: status,
            subscription_id: data.binding["target"]["subscription_id"]
                .as_str()
                .map(str::to_owned),
            expires_at: data.binding["expires_at"]
                .as_str()
                .ok_or(RuntimeRecoveryIssue::InvalidJournal)?
                .into(),
            original_context: data.context,
        });
    }
    Ok(PendingConfirmationsMock {
        fictional: true,
        items,
    })
}
impl SubscriptionReadSnapshotMock {
    pub fn pending_confirmations_mock(&self, now: &str) -> Result<PendingConfirmationsMock> {
        pending_view(&self.journal, &self.state, &self.owner, now)
    }
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AbandonPendingReceiptMock {
    pub fictional: bool,
    pub source_position: usize,
    pub abandoned_at: String,
    pub business_changed: bool,
}
impl SubscriptionRuntimeMock {
    pub fn pending_confirmations_mock(&self, now: &str) -> Result<PendingConfirmationsMock> {
        pending_view(&self.journal, &self.state, &self.user, now)
    }
    pub async fn abandon_pending_mock(
        &mut self,
        position: usize,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<AbandonPendingReceiptMock> {
        if context.user_id != self.user {
            return Err(RuntimeRecoveryIssue::Business(
                MockIssue::ResourceUnavailable,
            ));
        }
        let original = self
            .journal
            .commands
            .get(position)
            .and_then(prepared)
            .ok_or(RuntimeRecoveryIssue::NotFound)?;
        if &original.context != context {
            return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
        }
        if let Some(original_time) = self.journal.commands.iter().find_map(|c| match c {
            Command::AbandonPending {
                position: p,
                context: c,
                now,
                ..
            } if *p == position && c == context => Some(now.clone()),
            _ => None,
        }) {
            return Ok(AbandonPendingReceiptMock {
                fictional: true,
                source_position: position,
                abandoned_at: original_time,
                business_changed: false,
            });
        }
        self.append(Command::AbandonPending {
            position,
            scope: original.scope.into(),
            context: context.clone(),
            binding: original.binding,
            now: now.into(),
        })
        .await?;
        Ok(AbandonPendingReceiptMock {
            fictional: true,
            source_position: position,
            abandoned_at: now.into(),
            business_changed: false,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct ReselectionResultMock {
    pub fictional: bool,
    pub mode: &'static str,
    pub original_subscription_id: Option<String>,
    pub preview: RegistrationPreviewMock,
    #[serde(skip)]
    pub confirmation_binding: MockConfirmationBinding,
}
impl SubscriptionRuntimeMock {
    #[allow(clippy::too_many_arguments)] // Original binding, new source/selection, operation and deadlines remain explicit.
    pub async fn reselect_registration_mock(
        &mut self,
        original: &MockConfirmationBinding,
        context: &MockContext,
        input: &Snapshot,
        index: usize,
        next_operation: MockOperationContext,
        now: &str,
        expires: &str,
    ) -> Result<ReselectionResultMock> {
        self.state.guard("ledger", &original.context, &self.user)?;
        if context.user_id != self.user || next_operation.user_id != self.user {
            return Err(RuntimeRecoveryIssue::Business(
                MockIssue::ResourceUnavailable,
            ));
        }
        let read = SubscriptionReadSnapshotMock {
            owner: self.user.clone(),
            version: self.version,
            state: self.state.clone(),
            business_records: collect_business_records(&self.journal)?,
            journal: self.journal.clone(),
        };
        let result =
            read.original_result_mock(&OriginalOperationRequestMock::registration(original)?)?;
        let (mode, saved) = match result {
            OriginalResultMock::Recorded { original_receipt } => {
                if next_operation.operation_id == original.context.operation_id
                    || next_operation.idempotency_key == original.context.idempotency_key
                {
                    return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
                }
                (
                    "NEW_REGISTRATION_KEEP_ORIGINAL",
                    Some(
                        original_receipt["subscription_id"]
                            .as_str()
                            .ok_or(RuntimeRecoveryIssue::InvalidJournal)?
                            .to_owned(),
                    ),
                )
            }
            OriginalResultMock::NeedsReview => {
                if self.state.ledger.drafts.get(&original.draft_id) != Some(original) {
                    return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
                }
                self.state
                    .ledger
                    .registration_preview_mock(&original.draft_id, &original.context, now)
                    .map_err(RuntimeRecoveryIssue::Business)?;
                let mut expected = original.context.clone();
                expected.plan_version = next_operation.plan_version;
                if next_operation != expected
                    || next_operation.plan_version <= original.context.plan_version
                {
                    return Err(RuntimeRecoveryIssue::OriginalRequestConflict);
                }
                ("REPLACE_UNCOMMITTED", None)
            }
        };
        let mut preflight = self.state.ledger.clone();
        let candidate = preflight
            .prepare_mock(context, input, next_operation.clone(), index, expires)
            .map_err(RuntimeRecoveryIssue::Business)?;
        preflight
            .registration_preview_mock(&candidate.draft_id, &candidate.context, now)
            .map_err(RuntimeRecoveryIssue::Business)?;
        let binding = self
            .prepare_registration_mock(context, input, next_operation, index, expires)
            .await?;
        let preview = self.registration_preview_mock(&binding.draft_id, &binding.context, now)?;
        Ok(ReselectionResultMock {
            fictional: true,
            mode,
            original_subscription_id: saved,
            preview,
            confirmation_binding: binding,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    fn partial(count: usize) -> Journal {
        let mut journal = super::super::raw_records_tests::actual_fixture_journal();
        journal.commands.truncate(count);
        journal
    }
    fn read(journal: &Journal) -> Result<(Journal, State)> {
        decode_journal(
            "user_demo",
            journal.commands.len() as i64 + 1,
            value(journal)?,
        )
    }
    #[test]
    fn pending_list_distinguishes_expiry_restart_and_completed_operations_without_internal_context()
    {
        let journal = partial(1);
        let (_, state) = read(&journal).unwrap();
        let ready = pending_view(&journal, &state, "user_demo", NOW).unwrap();
        assert_eq!(ready.items[0].state, "READY");
        assert_eq!(
            pending_view(&journal, &state, "user_demo", "2026-10-14T12:05:00+08:00")
                .unwrap()
                .items[0]
                .state,
            "EXPIRED"
        );
        let json = serde_json::to_string(&ready).unwrap();
        for field in [
            "original_context",
            "session_ref",
            "idempotency_key",
            "owner_user_id",
        ] {
            assert!(!json.contains(field));
        }
        let mut restarted = journal;
        restarted.commands.push(Command::Restart);
        let (_, state) = read(&restarted).unwrap();
        assert_eq!(
            pending_view(&restarted, &state, "user_demo", NOW)
                .unwrap()
                .items[0]
                .state,
            "INVALIDATED"
        );
        let done = partial(8);
        let (_, state) = read(&done).unwrap();
        assert!(pending_view(&done, &state, "user_demo", NOW)
            .unwrap()
            .items
            .is_empty());
    }
    #[test]
    fn abandoned_journal_validates_exact_reference_context_scope_and_cannot_forge_completion() {
        let mut journal = partial(1);
        let data = prepared(&journal.commands[0]).unwrap();
        let command = Command::AbandonPending {
            position: 0,
            scope: data.scope.into(),
            context: data.context.clone(),
            binding: data.binding.clone(),
            now: NOW.into(),
        };
        journal.commands.push(command.clone());
        let (_, state) = read(&journal).unwrap();
        assert_eq!(
            pending_view(&journal, &state, "user_demo", NOW)
                .unwrap()
                .items[0]
                .state,
            "ABANDONED"
        );
        assert!(collect_business_records(&journal).unwrap().is_empty());
        assert_eq!(
            state.guard("ledger", &data.context, "user_demo"),
            Err(RuntimeRecoveryIssue::OperationAbandoned)
        );
        let mut replayed = partial(1);
        replayed.commands.push(replayed.commands[0].clone());
        let mut alias = command.clone();
        if let Command::AbandonPending { position, .. } = &mut alias {
            *position = 1;
        }
        replayed.commands.push(alias);
        let (_, restored) = read(&replayed).unwrap();
        let list = pending_view(&replayed, &restored, "user_demo", NOW).unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].state, "ABANDONED");
        for field in ["position", "scope", "context", "binding", "now"] {
            let mut raw = value(&command).unwrap();
            match field {
                "position" => raw[field] = serde_json::json!(999),
                "scope" => raw[field] = serde_json::json!("reminder"),
                "context" => raw[field]["plan_version"] = serde_json::json!(99),
                "binding" => raw[field]["candidate"] = serde_json::json!({}),
                _ => raw[field] = serde_json::json!("2026-01-01T00:00:00+08:00"),
            }
            let mut forged = partial(1);
            forged.commands.push(serde_json::from_value(raw).unwrap());
            assert!(read(&forged).is_err());
        }
        let mut completed = partial(2);
        completed.commands.push(command);
        assert!(read(&completed).is_err());
    }
}
