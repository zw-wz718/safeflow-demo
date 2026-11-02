//! Two-process diagnostic only; not an Agent recovery entry point.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}

#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() {
    use chrono::NaiveDate;
    use safeflow_subscription_evidence::{
        confirmation_mock::*, managed_ledger_mock::*, postgres_ledger_mock::*, recognition::Cycle,
        reminder_mock::*, ReadKind, ReadState,
    };
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert!(args.len() == 2 && matches!(args[0].as_str(), "write" | "read"));
    let url = std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL")
        .expect("DEDICATED_DATABASE_REQUIRED");
    let pool = PgPoolOptions::new()
        .connect(&url)
        .await
        .unwrap_or_else(|_| panic!("DATABASE_UNAVAILABLE"));
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .expect("DATABASE_NAME_CHECK_FAILED");
    assert_eq!(db, "safeflow_subscription_mock_test");
    let store = PostgresLedgerMock::from_pool_for_local_simulation(pool.clone());
    let user = format!("fictional-restart-{}", args[1]);
    let context = |id: &str| MockOperationContext {
        user_id: user.clone(),
        session_ref: "fictional-session".into(),
        task_id: "fictional-task".into(),
        node_id: "fictional-node".into(),
        operation_id: id.into(),
        idempotency_key: format!("fictional-key-{id}"),
        plan_version: 1,
    };
    const NOW: &str = "2026-10-16T12:00:00+08:00";
    const EXPIRY: &str = "2026-10-16T12:05:00+08:00";
    let report = if args[0] == "write" {
        store.initialize_local_schema().await.unwrap();
        let mut fixture = LedgerFixture {
            schema_version: 1,
            fictional: true,
            production_registered: false,
            owner_user_id: user.clone(),
            snapshot_id: Some("before-operation".into()),
            read: ReadState {
                state: ReadKind::Complete,
                is_complete: true,
                next_cursor: None,
                reason: None,
            },
            entries: vec![LedgerEntry {
                subscription_id: "fictional-sub".into(),
                owner_user_id: user.clone(),
                source_id: Some("fictional-source".into()),
                merchant_id: Some("fictional-merchant".into()),
                service_ref: Some("fictional-service".into()),
                status: LedgerStatus::Active,
            }],
        };
        store.save_fixture_mock(&user, &fixture).await.unwrap();
        let mut ledger = MemoryConfirmationMock::from_fixture(fixture.clone());
        struct IntentGate(MockIntentBinding);
        impl MockIntentGateVerifier for IntentGate {
            fn verify_and_consume_mock_intent(
                &mut self,
                b: &MockIntentBinding,
                _: &str,
            ) -> MockGateOutcome {
                if *b == self.0 {
                    MockGateOutcome::Confirmed
                } else {
                    MockGateOutcome::Denied
                }
            }
        }
        let b = ledger
            .prepare_intent_mock(
                context("intent"),
                "fictional-sub",
                MockIntentKind::Record,
                NOW,
                EXPIRY,
            )
            .unwrap();
        ledger
            .commit_intent_mock(&b.draft_id, &b.context, NOW, &mut IntentGate(b.clone()))
            .unwrap();
        let mut reminders = MemoryReminderMock::default();
        let date = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        let binding = reminders
            .prepare_mock(
                &ledger,
                context("settings"),
                "fictional-sub",
                Schedule {
                    cycle: Cycle::CalendarMonth,
                    anchor_date: date,
                    period_index: 1,
                    expected_date: NaiveDate::from_ymd_opt(2026, 11, 16).unwrap(),
                    enabled: true,
                    days_before: 3,
                },
                NOW,
                EXPIRY,
            )
            .unwrap();
        struct ScheduleGate(ScheduleBinding);
        impl MockScheduleGate for ScheduleGate {
            fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
                if *b == self.0 {
                    MockGateOutcome::Confirmed
                } else {
                    MockGateOutcome::Denied
                }
            }
        }
        reminders
            .confirm_mock(&ledger, &binding, NOW, &mut ScheduleGate(binding.clone()))
            .unwrap();
        assert!(ledger.intent_result_mock(&user, "intent").is_ok());
        assert!(reminders
            .settings_result_mock(&ledger, &user, "settings")
            .is_ok());
        // Explicit diagnostic snapshot export, not an automatic business save.
        fixture.entries = ledger.list_mock(&user).unwrap().to_vec();
        fixture.snapshot_id = Some("explicit-after-operation".into());
        store.save_fixture_mock(&user, &fixture).await.unwrap();
        json!({"phase":"write", "pid":std::process::id(),
            "intent_receipt_present":true, "settings_receipt_present":true,
            "intent_events":ledger.intent_events_mock(&user).unwrap().len(),
            "explicit_snapshot_export":true})
    } else {
        let before = store
            .read_owned_fixture_mock(&user, "before-operation")
            .await
            .unwrap();
        assert_eq!(before.entries[0].status, LedgerStatus::Active);
        let saved = store
            .read_owned_fixture_mock(&user, "explicit-after-operation")
            .await
            .unwrap();
        let ledger = MemoryConfirmationMock::from_fixture(saved);
        let reminders = MemoryReminderMock::default();
        assert_eq!(
            ledger.list_mock(&user).unwrap()[0].status,
            LedgerStatus::Cancelling
        );
        assert_eq!(
            ledger.intent_result_mock(&user, "intent"),
            Err(MockIssue::ResourceUnavailable)
        );
        assert!(ledger.intent_events_mock(&user).unwrap().is_empty());
        assert_eq!(
            reminders.settings_result_mock(&ledger, &user, "settings"),
            Err("RESOURCE_UNAVAILABLE")
        );
        assert_eq!(
            reminders.current_schedule_mock(&ledger, &user, "fictional-sub"),
            Err("DATE_CONFIRMATION_REQUIRED")
        );
        json!({"phase":"read", "pid":std::process::id(),
            "initial_snapshot_still_active":true, "explicit_snapshot_status_restored":true,
            "intent_receipt_restored":false, "intent_events_restored":false,
            "reminder_settings_restored":false, "settings_receipt_restored":false,
            "full_business_recovery":false})
    };
    pool.close().await;
    println!("{}", report);
}
