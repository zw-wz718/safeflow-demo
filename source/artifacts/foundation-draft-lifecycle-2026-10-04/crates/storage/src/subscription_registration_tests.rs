//! Echo wiring tests over the unchanged member discovery/ledger fixtures.
//! Typed fixture selection is NOT the missing direct-reader capture bridge.
use super::*;
#[path = "subscription_capture_tests.rs"]
mod capture_tests;
// Isolated bank port: ownership comes ONLY from the unchanged member fixture's
// typed accounts. Balance DTO bytes come from the existing bank fixture. These
// fixtures use different user IDs; no real bank ownership integration is claimed.
pub(super) struct FixtureAccounts;
impl safeflow_bank_gateway::BankGateway for FixtureAccounts {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::GetAccountBalanceOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async move {
            if !sample()
                .snapshot
                .accounts
                .iter()
                .any(|a| a.user_id == user.as_str() && a.account_id == account.as_str())
            {
                return Err(safeflow_bank_gateway::BankError::ResourceNotOwned);
            }
            let bank: Value =
                serde_json::from_str(include_str!("../../mock-bank/src/fixtures.json")).unwrap();
            serde_json::from_value(
                bank["accounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|a| a["balance"]["account"]["account_id"] == account.as_str())
                    .unwrap()["balance"]
                    .clone(),
            )
            .map_err(|_| safeflow_bank_gateway::BankError::ResourceNotOwned)
        })
    }
    fn find_payees<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a str,
    ) -> ContractFuture<
        'a,
        Result<safeflow_bank_gateway::FindPayeesOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async { Err(safeflow_bank_gateway::BankError::ResourceNotOwned) })
    }
}
pub(super) fn registration_writes<'a>(db: &Db, runtime: &'a Runtime) -> SubscriptionWriteService<'a> {
    let mut service = db.writes(runtime);
    service.bank = Arc::new(FixtureAccounts);
    service
}
fn sample() -> safeflow_subscription_evidence::Sample {
    let raw: Value = serde_json::from_str(include_str!(env!(
        "SAFEFLOW_SUBSCRIPTION_DISCOVERY_FIXTURE"
    )))
    .unwrap();
    serde_json::from_value(raw["sample"].clone()).unwrap()
}
async fn select(
    writes: &SubscriptionWriteService<'_>,
    db: &Db,
    host: &SubscriptionHost,
) -> SubscriptionRegistrationSelection {
    let f = sample();
    writes
        .select_registration_for_trusted_backend(
            &db.session,
            host,
            &f.trusted_context_fixture,
            &f.snapshot,
            0,
        )
        .await
        .unwrap()
}
async fn prepare(
    writes: &SubscriptionWriteService<'_>,
    db: &Db,
    host: &SubscriptionHost,
    nonce: &str,
    selection: &SubscriptionRegistrationSelection,
) -> Result<SubscriptionPreparation, StorageError> {
    writes
        .prepare_technical(
            &db.session,
            host,
            nonce,
            Preparation::Registration(selection),
        )
        .await
}
#[tokio::test]
#[ignore]
async fn subscription_registration_nonce_concurrency_complete_source_and_lost_response() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let selection = select(&writes, &db, &host).await;
    let stale = select(&writes, &db, &host).await;
    let nonce = uuid::Uuid::new_v4().to_string();
    let (a, b) = tokio::join!(
        prepare(&writes, &db, &host, &nonce, &selection),
        prepare(&writes, &db, &host, &nonce, &selection)
    );
    let a = a.unwrap();
    assert_eq!(json!(a), json!(b.unwrap()));
    let before = db.facts().await;
    assert_eq!(before["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(
        before["subscription_public_bindings_v1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let binding = &before["subscription_public_bindings_v1"][0];
    assert!(binding["member_binding"].get("prepared_at").is_none());
    assert_ne!(
        binding["public_parameters"]["public_prepared_at"],
        binding["member_binding"]["as_of"]
    );
    assert_eq!(
        binding["member_binding"]["expires_at"],
        binding["public_parameters"]["expires_at"]
    );
    let source_request = &before["subscription_preparations_v1"][0]["request_identity"]["request"];
    assert_eq!(source_request["input"], json!(sample().snapshot));
    assert_eq!(
        source_request["context"],
        json!(sample().trusted_context_fixture)
    );
    assert_eq!(source_request["index"], 0);
    assert_eq!(source_request["scope"]["schema"], db.schema);
    assert!(source_request["journal_digest"].as_str().unwrap().len() > 10);
    assert_eq!(
        json!(a),
        json!(prepare(&writes, &db, &host, &nonce, &selection)
            .await
            .unwrap())
    );
    // Only original nonce is needed; neither B nor the consumed opaque is sent.
    assert_eq!(
        json!(a),
        json!(writes.recover_technical(&db.session, &nonce).await.unwrap())
    );
    assert_eq!(before, db.facts().await);
    assert_eq!(
        prepare(
            &writes,
            &db,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            &stale
        )
        .await
        .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(before, db.facts().await);
    assert_eq!(
        writes
            .prepare_technical(
                &db.session,
                &host,
                &nonce,
                Preparation::Change(request(Action::RecordCancellation))
            )
            .await
            .err(),
        Some(StorageError::FactsConflict)
    );
    // Corrupt each captured-source parameter in an INTERNAL test only. There is
    // no production constructor/mutator/JSON decoder for this private handle.
    let mut selection = selection;
    for key in [
        "index",
        "input",
        "context",
        "storage_version",
        "journal_digest",
        "scope",
        "source_contract",
        "host_instance",
    ] {
        let original = selection.request[key].clone();
        selection.request[key] = json!("conflicting-source-parameter");
        assert_eq!(
            prepare(&writes, &db, &host, &nonce, &selection).await.err(),
            Some(StorageError::FactsConflict),
            "{key}"
        );
        selection.request[key] = original;
    }
    let another = SessionRef::parse("registration_same_owner_other_session").unwrap();
    db.sessions.bind(&another, "user_demo", 1);
    assert_eq!(
        writes.recover_technical(&another, &nonce).await.err(),
        Some(StorageError::FactsConflict)
    );
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        writes.recover_technical(&db.session, &nonce).await.err(),
        Some(StorageError::FactsConflict)
    );
    db.sessions.bind(&db.session, "other_owner", 3);
    assert_eq!(
        writes.recover_technical(&db.session, &nonce).await.err(),
        Some(StorageError::NotFound)
    );
    db.sessions.bind(&db.session, "user_demo", 1);
    assert_eq!(before, db.facts().await);
    // Public write/recovery/confirm/execute remain closed by A2.
    assert_eq!(
        writes
            .prepare_registration_from_selection(&db.session, &host, &nonce, &selection)
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(
        writes.recover_preparation(&db.session, &nonce).await.err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(
        writes
            .confirm(
                &db.session,
                &a.task.task_id,
                &a.confirmation.confirmation_ref
            )
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(
        writes
            .execute(
                &db.session,
                &host,
                &a.task.task_id,
                &a.operation.operation_id,
                &a.confirmation.confirmation_ref
            )
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(before, db.facts().await);
    drop(writes);
    drop(assembly);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_registration_invalid_uncommitted_handles_leave_no_facts() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let selection = select(&writes, &db, &host).await;
    let before = db.facts().await;
    let other_service = registration_writes(&db, assembly.runtime());
    assert_eq!(
        prepare(
            &other_service,
            &db,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            &selection
        )
        .await
        .err(),
        Some(StorageError::FactsConflict)
    );
    let f = sample();
    assert!(writes
        .select_registration_for_trusted_backend(
            &db.session,
            &host,
            &f.trusted_context_fixture,
            &f.snapshot,
            usize::MAX
        )
        .await
        .is_err());
    let mut context = f.trusted_context_fixture;
    context.user_id = "other_owner".into();
    assert_eq!(
        writes
            .select_registration_for_trusted_backend(&db.session, &host, &context, &f.snapshot, 0)
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    db.sessions.bind(&db.session, "user_demo", 2);
    assert_eq!(
        prepare(
            &writes,
            &db,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            &selection
        )
        .await
        .err(),
        Some(StorageError::FactsConflict)
    );
    db.sessions.bind(&db.session, "user_demo", 1);
    let mut expired = select(&writes, &db, &host).await;
    expired.expires = chrono::Utc::now() - chrono::Duration::seconds(1);
    assert_eq!(
        prepare(
            &writes,
            &db,
            &host,
            &uuid::Uuid::new_v4().to_string(),
            &expired
        )
        .await
        .err(),
        Some(StorageError::ConfirmationExpired)
    );
    for nonce in ["bad", "00000000-0000-0000-0000-000000000000"] {
        assert_eq!(
            prepare(&writes, &db, &host, nonce, &selection).await.err(),
            Some(StorageError::Invalid("preparation nonce v1"))
        );
    }
    assert_eq!(
        writes
            .recover_technical(&db.session, &uuid::Uuid::new_v4().to_string())
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(before, db.facts().await);
    // A real explicit host restart changes the saved version/Journal/generation.
    let new_host = db.read.startup_for_host(&db.session).await.unwrap();
    let restarted = db.facts().await;
    assert_eq!(
        prepare(
            &writes,
            &db,
            &new_host,
            &uuid::Uuid::new_v4().to_string(),
            &selection
        )
        .await
        .err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(restarted, db.facts().await);
    drop(other_service);
    drop(writes);
    drop(assembly);
    db.finish().await;
}
#[tokio::test]
#[ignore]
async fn subscription_registration_committed_faults_nonce_only_and_common_deadline() {
    for readback_fault in [true, false] {
        let db = Db::new().await;
        db.initialize("complete-empty").await;
        let host = db.read.startup_for_host(&db.session).await.unwrap();
        let assembly = db.assembly();
        let mut writes = registration_writes(&db, assembly.runtime());
        let selection = select(&writes, &db, &host).await;
        let nonce = uuid::Uuid::new_v4().to_string();
        if readback_fault {
            writes.store.repository.subscription_read_pool.close().await;
        } else {
            writes.confirmations = repair_tests::confirmation(&db, -120);
        }
        assert!(prepare(&writes, &db, &host, &nonce, &selection)
            .await
            .is_err());
        drop(selection);
        let committed = db.facts().await;
        assert_eq!(
            committed["subscription_public_bindings_v1"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            committed["subscription_preparations_v1"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(committed["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(committed["server_confirmations"], json!([]));
        let restored = PgRepository::connect_existing(
            &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
            &db.schema,
        )
        .await
        .unwrap();
        writes.store.repository = restored.clone();
        writes.confirmations = ConfirmationService::for_trusted_backend(
            restored.clone(),
            db.sessions.clone(),
            ConfirmationConfig::new(Duration::from_secs(60)).unwrap(),
        );
        let task: TaskRecord = serde_json::from_value(
            committed["subscription_preparations_v1"][0]["prepared_task"].clone(),
        )
        .unwrap();
        let operation: OperationRecord = serde_json::from_value(
            committed["subscription_preparations_v1"][0]["prepared_operation"].clone(),
        )
        .unwrap();
        let generic = writes
            .confirmations
            .prepare(&db.session, &task.task_id, &operation.operation_id)
            .await
            .unwrap();
        let p = writes.recover_technical(&db.session, &nonce).await.unwrap();
        assert_eq!(generic, p.confirmation);
        let original = db.facts().await;
        for name in [
            "subscription_runtime_records",
            "subscription_public_bindings_v1",
            "subscription_preparations_v1",
            "tasks",
            "operations",
        ] {
            assert_eq!(committed[name], original[name]);
        }
        let b = &original["subscription_public_bindings_v1"][0];
        let (start, end) =
            subscription_deadline(&b["public_parameters"], &b["member_binding"]).unwrap();
        assert!(p.confirmation.expires_at <= end);
        writes.confirmations = repair_tests::confirmation(&db, 61);
        assert_eq!(
            writes.recover_technical(&db.session, &nonce).await.err(),
            Some(StorageError::ConfirmationExpired)
        );
        assert_eq!(
            writes
                .confirmations
                .prepare(&db.session, &task.task_id, &operation.operation_id)
                .await
                .err(),
            Some(StorageError::ConfirmationExpired)
        );
        writes.confirmations = repair_tests::confirmation(&db, -120);
        assert_eq!(
            writes.recover_technical(&db.session, &nonce).await.err(),
            Some(StorageError::Invalid("server clock moved backwards"))
        );
        assert_eq!(
            writes
                .confirmations
                .prepare(&db.session, &task.task_id, &operation.operation_id)
                .await
                .err(),
            Some(StorageError::Invalid("server clock moved backwards"))
        );
        assert!(end > start);
        assert_eq!(original, db.facts().await);
        restored.close().await;
        drop(writes);
        drop(assembly);
        db.finish().await;
    }
}
#[tokio::test]
#[ignore]
async fn subscription_registration_real_stage_last_deadline_rolls_back_every_fact() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let mut writes = registration_writes(&db, assembly.runtime());
    let selection = select(&writes, &db, &host).await;
    let p = prepare(
        &writes,
        &db,
        &host,
        &uuid::Uuid::new_v4().to_string(),
        &selection,
    )
    .await
    .unwrap();
    writes
        .confirmations
        .record_explicit_confirmation(
            &db.session,
            &p.task.task_id,
            &p.confirmation.confirmation_ref,
            &p.confirmation.binding,
        )
        .await
        .unwrap();
    let before = db.facts().await;
    writes.stage_observer = Some(repair_tests::cross_deadline);
    assert_eq!(
        writes
            .execute_technical(
                &db.session,
                &host,
                &p.task.task_id,
                &p.operation.operation_id,
                &p.confirmation.confirmation_ref
            )
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    assert_eq!(before,db.facts().await,"real registration stage/CAS, consumed permit, public submission, audit/outbox all rollback");
    drop(writes);
    drop(assembly);
    db.finish().await;
}

#[tokio::test]
#[ignore]
async fn subscription_registration_consumed_but_uncommitted_selection_is_not_recoverable() {
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    let selection = select(&writes, &db, &host).await;
    let before = db.facts().await;
    let nonce = uuid::Uuid::new_v4().to_string();
    sqlx::raw_sql("CREATE FUNCTION reject_registration_test() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated write fault'; END $$; CREATE TRIGGER reject_registration_test BEFORE INSERT ON tasks FOR EACH ROW EXECUTE FUNCTION reject_registration_test();").execute(&db.read.repository.pool).await.unwrap();
    assert!(prepare(&writes, &db, &host, &nonce, &selection)
        .await
        .is_err());
    assert_eq!(before, db.facts().await);
    sqlx::raw_sql(
        "DROP TRIGGER reject_registration_test ON tasks; DROP FUNCTION reject_registration_test();",
    )
    .execute(&db.read.repository.pool)
    .await
    .unwrap();
    assert_eq!(
        prepare(&writes, &db, &host, &nonce, &selection).await.err(),
        Some(StorageError::FactsConflict)
    );
    assert_eq!(
        writes.recover_technical(&db.session, &nonce).await.err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(before, db.facts().await);
    drop(writes);
    drop(assembly);
    db.finish().await;
}

#[tokio::test]
#[ignore]
async fn subscription_registration_restart_seed() {
    let root =
        std::path::PathBuf::from(std::env::var("SAFEFLOW_SUBSCRIPTION_RESTART_ROOT").unwrap());
    let state = root.join("registration-restart-state.json");
    assert!(!state.exists());
    let db = Db::new().await;
    db.initialize("complete-empty").await;
    let host = db.read.startup_for_host(&db.session).await.unwrap();
    let assembly = db.assembly();
    let mut writes = registration_writes(&db, assembly.runtime());
    writes.config = ConfirmationConfig::new(Duration::from_secs(600)).unwrap();
    writes.confirmations = ConfirmationService::for_trusted_backend(
        db.read.repository.clone(),
        db.sessions.clone(),
        writes.config.clone(),
    );
    let selection = select(&writes, &db, &host).await;
    let never_committed = select(&writes, &db, &host).await;
    let unknown_nonce = uuid::Uuid::new_v4().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();
    let preparation = prepare(&writes, &db, &host, &nonce, &selection)
        .await
        .unwrap();
    drop(never_committed);
    drop(selection);
    std::fs::write(state,serde_json::to_vec_pretty(&json!({"schema":db.schema,"nonce":nonce,"unknown_nonce":unknown_nonce,"preparation":preparation,"facts":db.facts().await})).unwrap()).unwrap();
    drop(writes);
    drop(assembly);
    db.read.repository.close().await;
    db.admin.close().await;
}
#[tokio::test]
#[ignore]
async fn subscription_registration_restart_verify() {
    let root =
        std::path::PathBuf::from(std::env::var("SAFEFLOW_SUBSCRIPTION_RESTART_ROOT").unwrap());
    let saved: Value = serde_json::from_slice(
        &std::fs::read(root.join("registration-restart-state.json")).unwrap(),
    )
    .unwrap();
    let schema = saved["schema"].as_str().unwrap().to_string();
    assert!(schema.starts_with("sf_subscription_test_") && schema.len() == 53);
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let repo = PgRepository::connect_existing(&url, &schema).await.unwrap();
    let sessions = Arc::new(Sessions::default());
    let session = SessionRef::parse("http_sim_isolated_non_replay_reference").unwrap();
    sessions.bind(&session, "user_demo", 1);
    let db = Db {
        schema,
        admin: PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap(),
        read: SubscriptionService::for_trusted_backend(repo, sessions.clone()),
        sessions,
        session,
    };
    let assembly = db.assembly();
    let writes = registration_writes(&db, assembly.runtime());
    // New process has NO old opaque selection and knows only the original nonce.
    let recovered = writes
        .recover_technical(&db.session, saved["nonce"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(json!(recovered), saved["preparation"]);
    assert_eq!(
        writes
            .recover_technical(&db.session, saved["unknown_nonce"].as_str().unwrap())
            .await
            .err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(db.facts().await, saved["facts"]);
    let output = root.join("registration-restart-verified.json");
    assert!(!output.exists());
    std::fs::write(output,serde_json::to_vec_pretty(&json!({"nonce_only_exact_recovery":true,"new_process_no_handle":true,"immutable_public_prepared_at_and_member_expiry":true,"unknown_uncommitted_nonce_not_found":true,"all_facts_unchanged":true,"no_business_execution":true})).unwrap()).unwrap();
    drop(writes);
    drop(assembly);
    db.finish().await;
}
