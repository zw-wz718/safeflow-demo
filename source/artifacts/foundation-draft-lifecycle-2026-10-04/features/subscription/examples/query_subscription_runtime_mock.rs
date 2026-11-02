//! Trusted local backend inspection of fictional data, never an authenticated API.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}

#[cfg(feature = "postgres-mock-store")]
#[derive(serde::Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Query {
    DueReminders {
        now: String,
    },
    ReminderReviews,
    ReminderHistory,
    OriginalReminder {
        request: safeflow_subscription_evidence::confirmation_mock::OriginalReminderRequestMock,
    },
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
    Original {
        request: safeflow_subscription_evidence::confirmation_mock::OriginalOperationRequestMock,
    },
    BusinessPage {
        cursor: Option<safeflow_subscription_evidence::confirmation_mock::BusinessRecordCursorMock>,
        limit: usize,
    },
}

#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(result) => {
            println!("{result}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "postgres-mock-store")]
async fn run() -> Result<serde_json::Value, String> {
    use safeflow_subscription_evidence::confirmation_mock::SubscriptionReadSnapshotMock;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 || args[0] != "--run-fictional" || args[1].trim().is_empty() {
        return Err("USAGE: --run-fictional fictional-owner query.json".into());
    }
    let raw = std::fs::read_to_string(&args[2]).map_err(|_| "QUERY_FILE_UNAVAILABLE")?;
    let query: Query = serde_json::from_str(&raw).map_err(|_| "INVALID_QUERY")?;
    let url = std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL")
        .map_err(|_| "DEDICATED_DATABASE_REQUIRED")?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .map_err(|_| "DATABASE_UNAVAILABLE")?;
    let mut tx = pool.begin().await.map_err(|_| "DATABASE_UNAVAILABLE")?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|_| "READ_ONLY_TRANSACTION_UNAVAILABLE")?;
    let snapshot = SubscriptionReadSnapshotMock::load_on_connection_mock(&mut tx, &args[1])
        .await
        .map_err(|e| format!("{e:?}"))?;
    let result = match query {
        Query::DueReminders { now } => serde_json::to_value(
            snapshot
                .due_reminders_mock(&now)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::ReminderReviews => serde_json::to_value(
            snapshot
                .reminders_needing_review_mock()
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::ReminderHistory => serde_json::to_value(
            snapshot
                .reminder_history_mock()
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::OriginalReminder { request } => serde_json::to_value(
            snapshot
                .original_reminder_result_mock(&request)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::PendingConfirmations { now } => serde_json::to_value(
            snapshot
                .pending_confirmations_mock(&now)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::IdentifySubscriptions { context, input } => serde_json::to_value(
            snapshot
                .identify_subscriptions_mock(&context, &input)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::SuggestReminderDate {
            context,
            input,
            candidate_index,
            subscription_id,
            now,
        } => serde_json::to_value(
            snapshot
                .suggest_reminder_date_mock(
                    &context,
                    &input,
                    candidate_index,
                    subscription_id.as_deref(),
                    &now,
                )
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::RegistrationPreview {
            draft_id,
            context,
            now,
        } => serde_json::to_value(
            snapshot
                .registration_preview_mock(&draft_id, &context, &now)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::IntentPreview {
            draft_id,
            context,
            now,
        } => serde_json::to_value(
            snapshot
                .intent_preview_mock(&draft_id, &context, &now)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::SettingsPreview { context, now } => serde_json::to_value(
            snapshot
                .settings_preview_mock(&context, &now)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::ListSubscriptions => serde_json::to_value(
            snapshot
                .list_subscriptions_mock()
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::CancellationStatus { subscription_id } => serde_json::to_value(
            snapshot
                .cancellation_status_mock(&subscription_id)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::ReminderPlan { subscription_id } => serde_json::to_value(
            snapshot
                .reminder_plan_mock(&subscription_id)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::Original { request } => serde_json::to_value(
            snapshot
                .original_result_mock(&request)
                .map_err(|e| format!("{e:?}"))?,
        ),
        Query::BusinessPage { cursor, limit } => serde_json::to_value(
            snapshot
                .business_record_page_mock(cursor.as_ref(), limit)
                .map_err(|e| format!("{e:?}"))?,
        ),
    }
    .map_err(|_| "RESULT_ENCODING_FAILED")?;
    tx.rollback().await.map_err(|_| "READ_ONLY_CLOSE_FAILED")?;
    pool.close().await;
    Ok(serde_json::json!({
        "mode":"LOCAL_FICTIONAL_READ_ONLY_QUERY", "fictional":true,
        "production_registered":false, "authenticated_api":false,
        "database_written":false, "business_reexecuted":false, "notification_sent":false,
        "pid":std::process::id(), "version":snapshot.version_mock(), "result":result
    }))
}

#[cfg(all(test, feature = "postgres-mock-store"))]
mod tests {
    use super::*;
    #[test]
    fn rejects_write_commands_and_unknown_query_fields() {
        for raw in [
            r#"{"kind":"Register"}"#,
            r#"{"kind":"BusinessPage","limit":1,"initialize":true}"#,
            r#"{"kind":"BusinessPage","limit":-1}"#,
        ] {
            assert!(serde_json::from_str::<Query>(raw).is_err());
        }
    }
}
