//! Full isolated shared-store fictional caller. No public permission or sender.
//! Requires preinstalled proposal schema in a dedicated local test database.
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
        confirmation_mock::*, managed_ledger_mock::LedgerFixture, reminder_mock::*, *,
    };
    use serde::Serialize;
    use serde_json::{json, Value};
    use sqlx::{Connection, PgConnection, Postgres, Transaction};
    use std::str::FromStr;
    type Issue = ProposedLedgerWriteIssue;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    const END: &str = "2026-10-14T12:05:00+08:00";
    struct FixtureGate {
        expected: Value,
        used: bool,
    }
    impl FixtureGate {
        fn new(b: &impl Serialize) -> Self {
            Self {
                expected: serde_json::to_value(b).expect("fixture binding"),
                used: false,
            }
        }
        fn verify(&mut self, b: &impl Serialize) -> MockGateOutcome {
            if !self.used && serde_json::to_value(b).ok().as_ref() == Some(&self.expected) {
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
            self.verify(b)
        }
    }
    impl MockIntentGateVerifier for FixtureGate {
        fn verify_and_consume_mock_intent(
            &mut self,
            b: &MockIntentBinding,
            _: &str,
        ) -> MockGateOutcome {
            self.verify(b)
        }
    }
    impl MockScheduleGate for FixtureGate {
        fn verify_and_consume_mock(&mut self, b: &ScheduleBinding) -> MockGateOutcome {
            self.verify(b)
        }
    }
    fn operation(owner: &str, action: &str) -> MockOperationContext {
        MockOperationContext {
            user_id: owner.into(),
            session_ref: "fictional-shared-workflow-session".into(),
            task_id: "fictional-shared-workflow".into(),
            node_id: action.into(),
            operation_id: action.into(),
            idempotency_key: format!("fictional-shared-{action}"),
            plan_version: 1,
        }
    }
    fn fixture(owner: &str) -> Result<Sample, Issue> {
        let root: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/discovery_ready_input.v1.json"
        )))
        .map_err(|_| Issue::Conflict)?;
        if root["fictional"] != true
            || root["production_registered"] != false
            || root["provenance"] != "HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE"
        {
            return Err(Issue::Conflict);
        }
        let mut f: Sample =
            serde_json::from_value(root["sample"].clone()).map_err(|_| Issue::Conflict)?;
        f.trusted_context_fixture.user_id = owner.into();
        for account in &mut f.snapshot.accounts {
            if account.user_id == "user_demo" {
                account.user_id = owner.into();
            }
        }
        Ok(f)
    }
    fn namespace(args: &[String]) -> Result<&str, Issue> {
        if args.len() != 2
            || args[0] != "--run-fictional"
            || !args[1].starts_with("sub_write_check_")
        {
            return Err(Issue::Conflict);
        }
        ProposedLedgerStoreMock::new(&args[1])?;
        Ok(&args[1])
    }
    async fn finish(
        handoff: SharedSubscriptionWriteHandoffMock,
        tx: Transaction<'_, Postgres>,
        reader: &mut PgConnection,
    ) -> Result<Value, Issue> {
        // Production host persists original_requests_mock() privately BEFORE COMMIT.
        // Never retry writes on either an uncertain commit or uncertain readback.
        let v = serde_json::to_value(
            handoff
                .complete_on_connection_mock(reader, tx.commit().await)
                .await,
        )
        .map_err(|_| Issue::Conflict)?;
        if v["state"] != "COMMITTED_VERIFIED" {
            return Err(Issue::CommitNotObserved);
        }
        Ok(v)
    }
    pub async fn run() -> Result<Value, Issue> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        let ns = namespace(&args)?;
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
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Issue::Unavailable)?
            .as_nanos();
        let owner = format!("fictional-shared-workflow-{nonce}");
        let sample = fixture(&owner)?;
        let store = ProposedLedgerStoreMock::new(ns)?;
        let mut writer = PgConnection::connect_with(&options)
            .await
            .map_err(|_| Issue::Unavailable)?;
        let mut reader = PgConnection::connect_with(&options)
            .await
            .map_err(|_| Issue::Unavailable)?;
        let initial = LedgerFixture {
            schema_version: 1,
            fictional: true,
            production_registered: false,
            owner_user_id: owner.clone(),
            snapshot_id: Some("fictional-shared-workflow".into()),
            read: ReadState {
                state: ReadKind::Complete,
                is_complete: true,
                next_cursor: None,
                reason: None,
            },
            entries: vec![],
        };
        let request = store.prepare_initialization_mock(&owner, initial)?;
        let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
        let pending = store
            .initialize_on_connection_mock(&mut tx, &owner, &request)
            .await?;
        tx.commit().await.map_err(|_| Issue::Unavailable)?;
        pending
            .read_committed_on_connection_mock(&mut reader)
            .await?;
        let discovery = store
            .query_on_connection_mock(
                &mut reader,
                &owner,
                &SharedSubscriptionReadQueryMock::Business(
                    SharedBusinessReadQueryMock::IdentifySubscriptions {
                        context: sample.trusted_context_fixture.clone(),
                        input: Box::new(sample.snapshot.clone()),
                    },
                ),
            )
            .await?;
        if discovery["groups"][0]["disposition"] != "NOT_IN_LEDGER" {
            return Err(Issue::Conflict);
        }
        let selection = store
            .select_discovery_candidate_on_connection_mock(
                &mut reader,
                &owner,
                &sample.trusted_context_fixture,
                &sample.snapshot,
                0,
                NOW,
            )
            .await?;
        let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
        let mut w = store.load_work_on_connection_mock(&mut tx, &owner).await?;
        let prepared = w
            .prepare_registration_from_selection_mock(
                selection,
                operation(&owner, "register"),
                NOW,
                END,
            )
            .await?;
        let registration = prepared.confirmation_binding;
        let pending = store.save_on_connection_mock(&mut tx, w).await?;
        tx.commit().await.map_err(|_| Issue::Unavailable)?;
        let saved = pending
            .read_committed_on_connection_mock(&mut reader)
            .await?;
        let registration_preview =
            saved.registration_preview_mock(&registration.draft_id, &registration.context, NOW)?;
        let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
        let mut w = store.load_work_on_connection_mock(&mut tx, &owner).await?;
        w.confirm_registration_mock(
            &registration.draft_id,
            &registration.context,
            NOW,
            &mut FixtureGate::new(&registration),
        )
        .await?;
        let handoff = store
            .save_with_handoff_on_connection_mock(&mut tx, w)
            .await?;
        let mut results = vec![finish(handoff, tx, &mut reader).await?];
        let saved = ProposedLedgerReaderMock::new(ns)
            .map_err(Issue::Read)?
            .load_on_connection_mock(&mut reader, &owner)
            .await
            .map_err(Issue::Read)?;
        let id = saved
            .ledger_mock()
            .list_mock(&owner)
            .map_err(|_| Issue::Conflict)?[0]
            .subscription_id
            .clone();
        let suggestion = saved.suggest_reminder_date_mock(
            &sample.trusted_context_fixture,
            &sample.snapshot,
            None,
            Some(&id),
            NOW,
        )?;
        let schedule = suggestion.suggested_schedule.ok_or(Issue::Conflict)?;
        let mut previews =
            vec![serde_json::to_value(registration_preview).map_err(|_| Issue::Conflict)?];
        for action in ["settings", "cancel", "revoke"] {
            let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
            let mut w = store.load_work_on_connection_mock(&mut tx, &owner).await?;
            let request = match action {
                "settings" => SubscriptionChangeRequestMock::UpdateReminderSettings {
                    subscription_id: id.clone(),
                    schedule: schedule.clone(),
                },
                "cancel" => SubscriptionChangeRequestMock::CreateCancellationIntent {
                    subscription_id: id.clone(),
                },
                _ => SubscriptionChangeRequestMock::RevokeCancellationIntent {
                    subscription_id: id.clone(),
                },
            };
            let prepared = w
                .prepare_subscription_change_mock(request, operation(&owner, action), NOW, END)
                .await?;
            let pending = store.save_on_connection_mock(&mut tx, w).await?;
            tx.commit().await.map_err(|_| Issue::Unavailable)?;
            previews.push(
                prepared
                    .read_committed_preview_on_connection_mock(&pending, &mut reader, NOW)
                    .await?,
            );
            let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
            let mut w = store.load_work_on_connection_mock(&mut tx, &owner).await?;
            match prepared.confirmation_binding_mock() {
                SubscriptionChangeBindingMock::Intent(b) => {
                    w.confirm_intent_mock(&b.draft_id, &b.context, NOW, &mut FixtureGate::new(b))
                        .await?
                }
                SubscriptionChangeBindingMock::Settings(b) => {
                    w.confirm_settings_mock(b, NOW, &mut FixtureGate::new(b))
                        .await?
                }
            }
            let handoff = store
                .save_with_handoff_on_connection_mock(&mut tx, w)
                .await?;
            results.push(finish(handoff, tx, &mut reader).await?);
        }
        // Leave one explicit unconfirmed draft to exercise only startup invalidation.
        let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
        let mut w = store.load_work_on_connection_mock(&mut tx, &owner).await?;
        let prepared = w
            .prepare_subscription_change_mock(
                SubscriptionChangeRequestMock::CreateCancellationIntent {
                    subscription_id: id.clone(),
                },
                operation(&owner, "unconfirmed-before-startup"),
                NOW,
                END,
            )
            .await?;
        let pending = store.save_on_connection_mock(&mut tx, w).await?;
        tx.commit().await.map_err(|_| Issue::Unavailable)?;
        prepared
            .read_committed_preview_on_connection_mock(&pending, &mut reader, NOW)
            .await?;
        let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
        let pending = store
            .stage_startup_recovery_on_connection_mock(&mut tx, &owner)
            .await?;
        tx.commit().await.map_err(|_| Issue::Unavailable)?;
        let startup = pending
            .read_committed_startup_summary_on_connection_mock(&mut reader, NOW)
            .await?;
        let details = store
            .query_on_connection_mock(
                &mut reader,
                &owner,
                &SharedSubscriptionReadQueryMock::Business(
                    SharedBusinessReadQueryMock::SubscriptionDetails {
                        subscription_id: id,
                        now: NOW.into(),
                    },
                ),
            )
            .await?;
        let mut cursor = None;
        let mut record_count = 0;
        loop {
            let page = store
                .business_record_page_on_connection_mock(&mut reader, &owner, cursor.as_ref(), 2)
                .await?;
            record_count += page.records.len();
            // Cursor persistence/roundtrip is backend-only; never put it in UI.
            cursor = Some(
                serde_json::from_value::<SharedBusinessRecordCursorMock>(
                    serde_json::to_value(&page.next_cursor).map_err(|_| Issue::Conflict)?,
                )
                .map_err(|_| Issue::Conflict)?,
            );
            if !page.has_more {
                break;
            }
        }
        if record_count != 4 || details["current_state"] != "ACTIVE" {
            return Err(Issue::Conflict);
        }
        Ok(
            json!({"fictional":true,"production_registered":false,"database_executed":true,
            "source":"HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE","discovery":discovery,"previews":previews,"business_results":results,
            "explicit_startup":startup,"current_subscription":details,"exported_business_record_count":record_count,
            "audit_received":false,"notification_sent":false,"merchant_cancellation_confirmed":false,"full_banking_system_recovered":false}),
        )
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn only_explicit_fictional_namespace_invocation_is_allowed() {
            for args in [
                vec![],
                vec!["--run-fictional".into(), "public".into()],
                vec!["--confirm".into(), "sub_write_check_example".into()],
            ] {
                assert!(namespace(&args).is_err());
            }
            assert!(
                namespace(&["--run-fictional".into(), "sub_write_check_example".into()]).is_ok()
            );
        }
        #[test]
        fn fixed_fixture_preserves_source_evidence_and_remaps_only_own_identity() {
            let s = fixture("fictional-other-owner").unwrap();
            assert_eq!(s.trusted_context_fixture.user_id, "fictional-other-owner");
            assert!(s
                .snapshot
                .accounts
                .iter()
                .all(|a| a.user_id == "fictional-other-owner"));
            assert_eq!(s.snapshot.transactions.len(), 2);
            assert!(s.snapshot.transactions_read.is_complete);
            assert_eq!(
                s.snapshot.managed_subscriptions_read.state,
                ReadKind::Unavailable
            );
        }
        #[test]
        fn fixture_gate_matches_exact_binding_once_without_customer_boolean() {
            let b = json!({"fixture":"exact"});
            let mut gate = FixtureGate::new(&b);
            assert_eq!(
                gate.verify(&json!({"fixture":"changed"})),
                MockGateOutcome::Denied
            );
            assert_eq!(gate.verify(&b), MockGateOutcome::Confirmed);
            assert_eq!(gate.verify(&b), MockGateOutcome::Denied);
        }
    }
}
