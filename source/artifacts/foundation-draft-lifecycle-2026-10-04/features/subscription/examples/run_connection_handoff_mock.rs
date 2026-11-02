//! Four caller-owned transactions over fictional data. No production permission.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}
#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match demo::run().await {
        Ok(report) => {
            println!("{report}");
            std::process::ExitCode::SUCCESS
        }
        Err(code) => {
            eprintln!("{code}");
            std::process::ExitCode::FAILURE
        }
    }
}
#[cfg(feature = "postgres-mock-store")]
mod demo {
    use safeflow_subscription_evidence::{
        confirmation_mock::*, managed_ledger_mock::*, reminder_mock::*, *,
    };
    use serde::Serialize;
    use serde_json::{json, Value};
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    const EXPIRY: &str = "2026-10-14T12:05:00+08:00";
    const INPUT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/discovery_ready_input.v1.json"
    ));
    type Result<T> = std::result::Result<T, &'static str>;
    struct FixtureGate {
        expected: Value,
        used: bool,
    }
    impl FixtureGate {
        fn new(binding: &impl Serialize) -> Self {
            Self {
                expected: serde_json::to_value(binding).expect("fixture binding"),
                used: false,
            }
        }
        fn check(&mut self, binding: &impl Serialize) -> MockGateOutcome {
            if !self.used && serde_json::to_value(binding).ok().as_ref() == Some(&self.expected) {
                self.used = true;
                MockGateOutcome::Confirmed
            } else {
                MockGateOutcome::Denied
            }
        }
    }
    impl MockGateVerifier for FixtureGate {
        fn verify_and_consume_mock_confirmation(
            &mut self,
            b: &MockConfirmationBinding,
            _: &str,
        ) -> MockGateOutcome {
            self.check(b)
        }
    }
    impl MockIntentGateVerifier for FixtureGate {
        fn verify_and_consume_mock_intent(
            &mut self,
            b: &MockIntentBinding,
            _: &str,
        ) -> MockGateOutcome {
            self.check(b)
        }
    }
    impl MockScheduleGate for FixtureGate {
        fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
            self.check(b)
        }
    }
    fn operation(user: &str, action: &str) -> MockOperationContext {
        MockOperationContext {
            user_id: user.into(),
            session_ref: "fictional-session".into(),
            task_id: "fictional-handoff".into(),
            node_id: action.into(),
            operation_id: action.into(),
            idempotency_key: format!("fictional-{action}"),
            plan_version: 1,
        }
    }
    fn fixture_sample(user: &str) -> Sample {
        let root: Value = serde_json::from_str(INPUT).expect("own fictional fixture");
        assert_eq!(
            root["provenance"],
            "HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE"
        );
        let mut sample: Sample =
            serde_json::from_value(root["sample"].clone()).expect("explicit fictional sample");
        sample.trusted_context_fixture.user_id = user.into();
        for account in &mut sample.snapshot.accounts {
            if account.user_id == "user_demo" {
                account.user_id = user.into();
            }
        }
        sample
    }
    pub async fn run() -> Result<Value> {
        if std::env::args().skip(1).collect::<Vec<_>>() != ["--run-fictional"] {
            return Err("USAGE: run_connection_handoff_mock --run-fictional");
        }
        let url = std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL")
            .map_err(|_| "DEDICATED_DATABASE_REQUIRED")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|_| "DATABASE_UNAVAILABLE")?;
        SubscriptionRuntimeMock::initialize_local_schema(&pool)
            .await
            .map_err(|_| "DEDICATED_SCHEMA_REQUIRED")?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "CLOCK_UNAVAILABLE")?
            .as_nanos();
        let owner = format!("fictional-handoff-{}-{nonce}", std::process::id());
        let fixture = LedgerFixture {
            schema_version: 1,
            fictional: true,
            production_registered: false,
            owner_user_id: owner.clone(),
            snapshot_id: Some("fictional-handoff".into()),
            read: ReadState {
                state: ReadKind::Complete,
                is_complete: true,
                next_cursor: None,
                reason: None,
            },
            entries: vec![],
        };
        let mut runtime =
            SubscriptionRuntimeMock::create_fixture_mock(pool.clone(), &owner, fixture)
                .await
                .map_err(|_| "FIXTURE_CREATE_FAILED")?;
        let mut capabilities = std::collections::BTreeSet::new();
        let mut initial_connection = pool.acquire().await.map_err(|_| "READ_CONNECTION_FAILED")?;
        let initial =
            SubscriptionReadSnapshotMock::load_on_connection_mock(&mut initial_connection, &owner)
                .await
                .map_err(|_| "INITIAL_READ_FAILED")?;
        let initial_count = initial
            .ledger_mock()
            .list_mock(&owner)
            .map_err(|_| "LEDGER_UNAVAILABLE")?
            .len();
        capabilities.insert("list_subscriptions");
        let sample = fixture_sample(&owner);
        let discovery = initial
            .ledger_mock()
            .discover_mock(&sample.trusted_context_fixture, &sample.snapshot)
            .map_err(|_| "DISCOVERY_FAILED")?;
        if !discovery.matching_executed || discovery.groups.is_empty() {
            return Err("DISCOVERY_NOT_READY");
        }
        capabilities.insert("identify_subscriptions");
        let discovery_evidence = json!({"input_provenance":"HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE","initial_subscription_count":initial_count,"review":discovery});
        drop(initial_connection);
        let mut steps = Vec::new();
        for action in ["register", "settings", "cancel", "revoke"] {
            let user = owner.clone();
            let mut tx = pool.begin().await.map_err(|_| "BEGIN_FAILED")?;
            let pending = runtime
                .stage_on_connection_mock(&mut tx, move |candidate| {
                    Box::pin(async move {
                        let receipt = match action {
                            "register" => {
                                let sample = fixture_sample(&user);
                                let binding = candidate
                                    .prepare_registration_mock(
                                        &sample.trusted_context_fixture,
                                        &sample.snapshot,
                                        operation(&user, action),
                                        0,
                                        EXPIRY,
                                    )
                                    .await?;
                                serde_json::to_value(
                                    candidate
                                        .register_mock(
                                            &binding.draft_id,
                                            &binding.context,
                                            NOW,
                                            &mut FixtureGate::new(&binding),
                                        )
                                        .await?,
                                )
                                .expect("receipt")
                            }
                            "settings" => {
                                let id = candidate
                                    .ledger_mock()
                                    .list_mock(&user)
                                    .expect("owned ledger")[0]
                                    .subscription_id
                                    .clone();
                                let sample = fixture_sample(&user);
                                let prediction = predict_mock(
                                    candidate.ledger_mock(),
                                    &sample.trusted_context_fixture,
                                    &sample.snapshot,
                                    &id,
                                )
                                .map_err(RuntimeRecoveryIssue::Reminder)?;
                                let schedule = Schedule::from_prediction(&prediction);
                                let binding = candidate
                                    .prepare_settings_mock(
                                        operation(&user, action),
                                        &id,
                                        schedule,
                                        NOW,
                                        EXPIRY,
                                    )
                                    .await?;
                                serde_json::to_value(
                                    candidate
                                        .confirm_settings_mock(
                                            &binding,
                                            NOW,
                                            &mut FixtureGate::new(&binding),
                                        )
                                        .await?,
                                )
                                .expect("receipt")
                            }
                            _ => {
                                let id = candidate
                                    .ledger_mock()
                                    .list_mock(&user)
                                    .expect("owned ledger")[0]
                                    .subscription_id
                                    .clone();
                                let kind = if action == "cancel" {
                                    MockIntentKind::Record
                                } else {
                                    MockIntentKind::Revoke
                                };
                                let binding = candidate
                                    .prepare_intent_mock(
                                        operation(&user, action),
                                        &id,
                                        kind,
                                        NOW,
                                        EXPIRY,
                                    )
                                    .await?;
                                serde_json::to_value(
                                    candidate
                                        .commit_intent_mock(
                                            &binding.draft_id,
                                            &binding.context,
                                            NOW,
                                            &mut FixtureGate::new(&binding),
                                        )
                                        .await?,
                                )
                                .expect("receipt")
                            }
                        };
                        Ok(receipt)
                    })
                })
                .await
                .map_err(|_| "STAGE_FAILED")?;
            // Real adapters put other required business/audit records here, before
            // the foundation commits. This example does not claim audit integration.
            let handoff = pending
                .into_handoff_mock()
                .map_err(|_| "HANDOFF_PREPARE_FAILED")?;
            let requests = handoff.original_requests_mock().to_vec();
            let receipt = match handoff.complete_mock(&mut runtime, tx.commit().await).await {
                SubscriptionWriteCompletionMock::Published(receipt) => receipt,
                SubscriptionWriteCompletionMock::ReadOnlyRecovery(recovery) => {
                    return Ok(
                        json!({"fictional":true,"database_execution":true,"production_registered":false,
                        "state":"READ_ONLY_RECOVERY","completed_steps":steps,"recovery":recovery,"notification_sent":false}),
                    );
                }
            };
            let mut connection = pool.acquire().await.map_err(|_| "READ_CONNECTION_FAILED")?;
            let saved =
                SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &owner)
                    .await
                    .map_err(|_| "ORIGINAL_READ_FAILED")?;
            let original = match action {
                "register" => serde_json::to_value(
                    saved
                        .ledger_mock()
                        .confirmation_result_mock(&owner, action)
                        .map_err(|_| "ORIGINAL_MISSING")?,
                ),
                "settings" => serde_json::to_value(
                    saved
                        .reminders_mock()
                        .settings_result_mock(saved.ledger_mock(), &owner, action)
                        .map_err(|_| "ORIGINAL_MISSING")?,
                ),
                _ => serde_json::to_value(
                    saved
                        .ledger_mock()
                        .intent_result_mock(&owner, action)
                        .map_err(|_| "ORIGINAL_MISSING")?,
                ),
            }
            .map_err(|_| "SERIALIZE_FAILED")?;
            for request in &requests {
                if saved
                    .original_result_mock(request)
                    .map_err(|_| "ORIGINAL_BINDING_CONFLICT")?
                    != (OriginalResultMock::Recorded {
                        original_receipt: receipt.clone(),
                    })
                {
                    return Err("ORIGINAL_BINDING_MISMATCH");
                }
            }
            if original != receipt {
                return Err("ORIGINAL_RECEIPT_MISMATCH");
            }
            capabilities.insert(match action {
                "register" => "confirm_subscription",
                "settings" => "update_reminder_settings",
                "cancel" => "create_cancellation_intent",
                _ => "revoke_cancellation_intent",
            });
            if matches!(action, "cancel" | "revoke") {
                let current = saved
                    .ledger_mock()
                    .list_mock(&owner)
                    .map_err(|_| "STATUS_UNAVAILABLE")?;
                let expected = if action == "cancel" {
                    LedgerStatus::Cancelling
                } else {
                    LedgerStatus::Active
                };
                if current.len() != 1 || current[0].status != expected {
                    return Err("CURRENT_STATUS_MISMATCH");
                }
                saved
                    .ledger_mock()
                    .intent_result_mock(&owner, action)
                    .map_err(|_| "INTENT_RESULT_UNAVAILABLE")?;
                capabilities.insert("get_cancellation_status");
            }
            steps.push(json!({"action":action,"receipt":receipt,"original_matches":true,
                "version":saved.version_mock(),"business_records":saved.business_records_mock(),"ledger":saved.ledger_mock().list_mock(&owner).map_err(|_| "LEDGER_UNAVAILABLE")?}));
        }
        let mut connection = pool.acquire().await.map_err(|_| "READ_CONNECTION_FAILED")?;
        let final_snapshot =
            SubscriptionReadSnapshotMock::load_on_connection_mock(&mut connection, &owner)
                .await
                .map_err(|_| "FINAL_READ_FAILED")?;
        let mut cursor = None;
        let mut incremental = Vec::new();
        let mut page_count = 0;
        loop {
            let page = final_snapshot
                .business_record_page_mock(cursor.as_ref(), 2)
                .map_err(|_| "PAGE_READ_FAILED")?;
            incremental.extend(page.records);
            page_count += 1;
            cursor = Some(page.next_cursor);
            if !page.has_more {
                break;
            }
        }
        if incremental != final_snapshot.business_records_mock()
            || incremental.len() != 4
            || capabilities.len() != 7
        {
            return Err("COMPLETE_HANDOFF_MISMATCH");
        }
        drop(connection);
        pool.close().await;
        Ok(
            json!({"fictional":true,"database_execution":true,"production_registered":false,
            "notification_sent":false,"merchant_action_executed":false,"audit_integrated":false,
            "foundation_integration_executed":false,"caller_owned_transactions":4,"capabilities":capabilities,
            "capability_count":capabilities.len(),"discovery":discovery_evidence,"audit_page_count":page_count,
            "incremental_record_count":incremental.len(),"steps":steps}),
        )
    }
}
