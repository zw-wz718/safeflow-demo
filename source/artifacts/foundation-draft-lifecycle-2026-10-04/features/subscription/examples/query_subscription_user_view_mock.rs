//! Local fictional presentation example, not an authenticated customer API.
#[cfg(feature = "postgres-mock-store")]
#[derive(serde::Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum CurrentQuery {
    PendingConfirmations {
        now: String,
    },
    IdentifySubscriptions {
        context: safeflow_subscription_evidence::MockContext,
        input: Box<safeflow_subscription_evidence::Snapshot>,
    },
    SuggestReminderDate {
        context: safeflow_subscription_evidence::MockContext,
        input: Box<safeflow_subscription_evidence::Snapshot>,
        candidate_index: Option<usize>,
        subscription_id: Option<String>,
        now: String,
    },
    RegistrationPreview {
        draft_id: String,
        context: safeflow_subscription_evidence::confirmation_mock::MockOperationContext,
        now: String,
    },
    IntentPreview {
        draft_id: String,
        context: safeflow_subscription_evidence::confirmation_mock::MockOperationContext,
        now: String,
    },
    SettingsPreview {
        context: safeflow_subscription_evidence::confirmation_mock::MockOperationContext,
        now: String,
    },
    ListSubscriptions,
    CancellationStatus {
        subscription_id: String,
    },
    ReminderPlan {
        subscription_id: String,
    },
}
#[cfg(feature = "postgres-mock-store")]
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum UserQuery {
    Current(CurrentQuery),
    Reminder(safeflow_subscription_evidence::confirmation_mock::ReminderReadQueryMock),
    Original(safeflow_subscription_evidence::confirmation_mock::OriginalOperationRequestMock),
}

#[cfg(all(test, feature = "postgres-mock-store"))]
mod reminder_parser_tests {
    use super::*;
    #[test]
    fn reminder_reads_use_safe_reminder_dispatch_and_reject_extra_fields() {
        for json in [
            r#"{"kind":"DueReminders","now":"2026-10-14T12:00:00+08:00"}"#,
            r#"{"kind":"ReminderReviews"}"#,
            r#"{"kind":"ReminderHistory"}"#,
        ] {
            assert!(matches!(
                serde_json::from_str::<UserQuery>(json).unwrap(),
                UserQuery::Reminder(_)
            ));
        }
        assert!(
            serde_json::from_str::<UserQuery>(r#"{"kind":"ReminderReviews","send":true}"#).is_err()
        );
    }
}
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}

#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use safeflow_subscription_evidence::confirmation_mock::OriginalOperationViewMock;
    let (view, succeeded) = match run().await {
        Ok(view) => (view, true),
        Err(issue) => (
            serde_json::to_value(OriginalOperationViewMock::issue_mock(issue))
                .expect("fixed safe view"),
            false,
        ),
    };
    // No backend envelope, PID, database version or raw evidence. Current list
    // retains the owned subscription reference solely for selecting a record.
    match serde_json::to_string(&view) {
        Ok(json) => println!("{json}"),
        Err(_) => return std::process::ExitCode::FAILURE,
    }
    if succeeded {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(feature = "postgres-mock-store")]
async fn run() -> Result<
    serde_json::Value,
    safeflow_subscription_evidence::confirmation_mock::RuntimeRecoveryIssue,
> {
    use safeflow_subscription_evidence::confirmation_mock::{
        RuntimeRecoveryIssue as Issue, SubscriptionReadSnapshotMock,
    };
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 || args[0] != "--run-fictional" || args[1].trim().is_empty() {
        return Err(Issue::OriginalRequestConflict);
    }
    let raw = std::fs::read_to_string(&args[2]).map_err(|_| Issue::OriginalRequestConflict)?;
    let query: UserQuery =
        serde_json::from_str(&raw).map_err(|_| Issue::OriginalRequestConflict)?;
    let url =
        std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL").map_err(|_| Issue::Unavailable)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .map_err(|_| Issue::Unavailable)?;
    let mut tx = pool.begin().await.map_err(|_| Issue::Unavailable)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|_| Issue::Unavailable)?;
    let snapshot = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut tx, &args[1]).await?;
    let view = match query {
        UserQuery::Reminder(query) => {
            serde_json::to_value(snapshot.reminder_read_view_mock(&query)?)
        }
        UserQuery::Current(CurrentQuery::PendingConfirmations { now }) => {
            serde_json::to_value(snapshot.pending_confirmations_mock(&now)?)
        }
        UserQuery::Current(CurrentQuery::IdentifySubscriptions { context, input }) => {
            serde_json::to_value(snapshot.identify_subscriptions_mock(&context, &input)?)
        }
        UserQuery::Current(CurrentQuery::SuggestReminderDate {
            context,
            input,
            candidate_index,
            subscription_id,
            now,
        }) => serde_json::to_value(snapshot.suggest_reminder_date_mock(
            &context,
            &input,
            candidate_index,
            subscription_id.as_deref(),
            &now,
        )?),
        UserQuery::Current(CurrentQuery::RegistrationPreview {
            draft_id,
            context,
            now,
        }) => serde_json::to_value(snapshot.registration_preview_mock(&draft_id, &context, &now)?),
        UserQuery::Current(CurrentQuery::IntentPreview {
            draft_id,
            context,
            now,
        }) => serde_json::to_value(snapshot.intent_preview_mock(&draft_id, &context, &now)?),
        UserQuery::Current(CurrentQuery::SettingsPreview { context, now }) => {
            serde_json::to_value(snapshot.settings_preview_mock(&context, &now)?)
        }
        UserQuery::Original(request) => {
            serde_json::to_value(snapshot.original_view_mock(&request)?)
        }
        UserQuery::Current(CurrentQuery::ListSubscriptions) => {
            serde_json::to_value(snapshot.list_subscriptions_mock()?)
        }
        UserQuery::Current(CurrentQuery::CancellationStatus { subscription_id }) => {
            serde_json::to_value(snapshot.cancellation_status_mock(&subscription_id)?)
        }
        UserQuery::Current(CurrentQuery::ReminderPlan { subscription_id }) => {
            serde_json::to_value(snapshot.reminder_plan_mock(&subscription_id)?)
        }
    }
    .map_err(|_| Issue::Unavailable)?;
    tx.rollback().await.map_err(|_| Issue::Unavailable)?;
    pool.close().await;
    Ok(view)
}
