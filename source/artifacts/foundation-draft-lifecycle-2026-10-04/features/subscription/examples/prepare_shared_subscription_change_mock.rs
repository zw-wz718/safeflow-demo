//! Isolated local prepare/save/readback example. Never confirms business changes.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}
#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use safeflow_subscription_evidence::confirmation_mock::shared_query_error_view_mock;
    let (v, ok) = match demo::run().await {
        Ok(v) => (v, true),
        Err(e) => (shared_query_error_view_mock(e), false),
    };
    println!("{v}");
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
#[cfg(feature = "postgres-mock-store")]
mod demo {
    use safeflow_subscription_evidence::{
        confirmation_mock::*, recognition::Cycle, reminder_mock::Schedule,
    };
    use serde::Deserialize;
    use serde_json::Value;
    use sqlx::Connection;
    use std::str::FromStr;
    type Issue = ProposedLedgerWriteIssue;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        fictional: bool,
        production_registered: bool,
        provenance: String,
        operation: MockOperationContext,
        now: String,
        expires: String,
        change: Change,
    }
    #[derive(Deserialize)]
    #[serde(tag = "kind", deny_unknown_fields)]
    enum Change {
        CreateCancellationIntent {
            subscription_id: String,
        },
        RevokeCancellationIntent {
            subscription_id: String,
        },
        UpdateReminderSettings {
            subscription_id: String,
            schedule: Plan,
        },
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        cycle: Cycle,
        anchor_date: String,
        period_index: u32,
        expected_date: String,
        enabled: bool,
        days_before: u8,
    }
    fn parse(raw: &str, owner: &str) -> Result<Request, Issue> {
        let r: Request = serde_json::from_str(raw).map_err(|_| Issue::Conflict)?;
        if !r.fictional
            || r.production_registered
            || r.provenance != "HANNAH_OWN_FICTIONAL_SUBSCRIPTION_CHANGE"
            || owner.trim().is_empty()
            || r.operation.user_id != owner
        {
            return Err(Issue::Conflict);
        }
        Ok(r)
    }
    fn change(c: Change) -> Result<SubscriptionChangeRequestMock, Issue> {
        Ok(match c {
            Change::CreateCancellationIntent { subscription_id } => {
                SubscriptionChangeRequestMock::CreateCancellationIntent { subscription_id }
            }
            Change::RevokeCancellationIntent { subscription_id } => {
                SubscriptionChangeRequestMock::RevokeCancellationIntent { subscription_id }
            }
            Change::UpdateReminderSettings {
                subscription_id,
                schedule: p,
            } => {
                let date = |s: &str| {
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| Issue::Conflict)
                };
                let schedule = Schedule {
                    cycle: p.cycle,
                    anchor_date: date(&p.anchor_date)?,
                    period_index: p.period_index,
                    expected_date: date(&p.expected_date)?,
                    enabled: p.enabled,
                    days_before: p.days_before,
                };
                schedule.validate().map_err(|_| Issue::Conflict)?;
                SubscriptionChangeRequestMock::UpdateReminderSettings {
                    subscription_id,
                    schedule,
                }
            }
        })
    }
    pub async fn run() -> Result<Value, Issue> {
        let a: Vec<_> = std::env::args().skip(1).collect();
        if a.len() != 4 || a[0] != "--run-fictional" || !a[1].starts_with("sub_write_check_") {
            return Err(Issue::Conflict);
        }
        let r = parse(
            &std::fs::read_to_string(&a[3]).map_err(|_| Issue::Conflict)?,
            &a[2],
        )?;
        let request = change(r.change)?;
        let options = sqlx::postgres::PgConnectOptions::from_str(
            &std::env::var("SAFEFLOW_SUBSCRIPTION_SHARED_QUERY_URL")
                .map_err(|_| Issue::Unavailable)?,
        )
        .map_err(|_| Issue::Unavailable)?;
        if options.get_host() != "127.0.0.1"
            || options.get_database() != Some("safeflow_subscription_write_test")
        {
            return Err(Issue::VerificationDatabaseMismatch);
        }
        let store = ProposedLedgerStoreMock::new(&a[1])?;
        let mut writer = sqlx::PgConnection::connect_with(&options)
            .await
            .map_err(|_| Issue::Unavailable)?;
        let mut reader = sqlx::PgConnection::connect_with(&options)
            .await
            .map_err(|_| Issue::Unavailable)?;
        let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
        let mut work = store.load_work_on_connection_mock(&mut tx, &a[2]).await?;
        let prepared = work
            .prepare_subscription_change_mock(request, r.operation, &r.now, &r.expires)
            .await?;
        // Retain original_request_mock() privately in the host before commit.
        // This standalone example ends after preparation; it has no confirmation route.
        let pending = store.save_on_connection_mock(&mut tx, work).await?;
        tx.commit().await.map_err(|_| Issue::Unavailable)?;
        // A failed readback after COMMIT is unknown, not proof of a rollback.
        prepared
            .read_committed_preview_on_connection_mock(&pending, &mut reader, &r.now)
            .await
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;
        fn input() -> Value {
            json!({"fictional":true,"production_registered":false,"provenance":"HANNAH_OWN_FICTIONAL_SUBSCRIPTION_CHANGE",
            "operation":{"user_id":"user_demo","session_ref":"fictional-session","task_id":"test","node_id":"prepare","operation_id":"fictional-change","idempotency_key":"fictional-change-key","plan_version":1},
            "now":"2026-10-14T12:00:00+08:00","expires":"2026-10-14T12:05:00+08:00",
            "change":{"kind":"CreateCancellationIntent","subscription_id":"fictional-owned"}})
        }
        #[test]
        fn only_three_preparation_actions_parse() {
            for kind in ["CreateCancellationIntent", "RevokeCancellationIntent"] {
                let mut v = input();
                v["change"]["kind"] = json!(kind);
                assert!(change(parse(&v.to_string(), "user_demo").unwrap().change).is_ok());
            }
            let mut v = input();
            v["change"]["kind"] = json!("ConfirmCancellation");
            assert!(parse(&v.to_string(), "user_demo").is_err());
        }
        #[test]
        fn supplied_confirmation_and_extra_top_level_fields_are_rejected() {
            for extra in ["confirmed", "permit", "send_now"] {
                let mut v = input();
                v[extra] = json!(true);
                assert!(parse(&v.to_string(), "user_demo").is_err());
                let mut v = input();
                v["change"][extra] = json!(true);
                assert!(parse(&v.to_string(), "user_demo").is_err());
            }
        }
        #[test]
        fn foreign_owner_and_nonfictional_inputs_are_rejected() {
            assert!(parse(&input().to_string(), "other").is_err());
            for key in ["fictional", "production_registered", "provenance"] {
                let mut v = input();
                v[key] = if key == "provenance" {
                    json!("unknown")
                } else {
                    json!(key != "fictional")
                };
                assert!(parse(&v.to_string(), "user_demo").is_err());
            }
        }
        #[test]
        fn settings_require_all_dates_and_exact_valid_schedule() {
            let mut v = input();
            v["change"] = json!({"kind":"UpdateReminderSettings","subscription_id":"fictional-owned","schedule":{"cycle":"CALENDAR_MONTH","anchor_date":"2026-09-17","period_index":1,"expected_date":"2026-10-17","enabled":false,"days_before":3}});
            let r = parse(&v.to_string(), "user_demo").unwrap();
            assert!(change(r.change).is_ok());
            v["change"]["schedule"]["days_before"] = json!(31);
            assert!(change(parse(&v.to_string(), "user_demo").unwrap().change).is_err());
            v["change"]["schedule"]
                .as_object_mut()
                .unwrap()
                .remove("expected_date");
            assert!(parse(&v.to_string(), "user_demo").is_err());
        }
    }
}
