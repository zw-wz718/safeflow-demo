use super::*;
use safeflow_contracts::{AgentTool, DomainPolicy, PolicyVersion, SecurityLevel, ToolName};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{
    AuditEventKind, DomainPolicyRegistry, InMemoryAuditRecorder, MockDraftAuthority,
    MockDraftClock, RuntimeAssembly, RuntimeBootstrap, ToolRegistry, ToolSecurityCatalog,
    ToolSecurityProfile,
};
use safeflow_transfer_integration::{runtime_tools::*, DraftStatus, SystemClock};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::time::Duration;

#[path = "audit_source_tests.rs"]
mod audit_source_tests;
#[path = "sql_bridge_tests.rs"]
mod sql_bridge_tests;
#[path = "nonfunding_close_tests.rs"]
mod nonfunding_close_tests;
#[path = "intent_termination_tests.rs"]
mod intent_termination_tests;

fn session() -> SessionRef {
    SessionRef::parse("lifecycle_replay_original_session").unwrap()
}

struct Sessions;
impl SessionResolver for Sessions {
    fn resolve(
        &self,
        reference: &SessionRef,
    ) -> Result<safeflow_contracts::UserId, safeflow_contracts::SecurityError> {
        Ok(self.resolve_binding(reference)?.user_id)
    }

    fn resolve_binding(
        &self,
        reference: &SessionRef,
    ) -> Result<SessionBinding, safeflow_contracts::SecurityError> {
        if reference != &session() && reference.as_str()!="resumed_own_session" {
            return Err(safeflow_contracts::SecurityError {
                reason_code: safeflow_contracts::ReasonCode::parse("SESSION_MISSING").unwrap(),
                message: "isolated replay session unavailable".into(),
            });
        }
        Ok(SessionBinding {
            user_id: safeflow_contracts::UserId::parse("user_001").unwrap(),
            binding_version: 1,
        })
    }
}

struct AuthorityClock;
impl MockDraftClock for AuthorityClock {
    fn now_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn now_millis(&self) -> Option<u64> {
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis(),
        )
        .ok()
    }
}

struct StoreObserver(Arc<MockDraftStore<SystemClock>>);
impl LifecycleDraftObserver for StoreObserver {
    fn observe<'a>(
        &'a self,
        context: &'a ExecutionContext,
    ) -> ContractFuture<'a, Result<Option<LifecycleDraftObservation>, StorageError>> {
        Box::pin(async move {
            let Some(saved) = self
                .0
                .read_created_for_task(context)
                .await
                .map_err(|_| StorageError::DependencyUnavailable)?
            else {
                return Ok(None);
            };
            if saved.task_id != context.task_id || saved.user_id != context.authenticated_user_id {
                return Err(StorageError::DependencyUnavailable);
            }
            let details = saved.details.ok_or(StorageError::DependencyUnavailable)?;
            Ok(Some(LifecycleDraftObservation {
                draft_id: saved.draft_id,
                business_operation_id: BusinessOperationRef::parse(saved.operation_id.as_str())
                    .map_err(|_| StorageError::DependencyUnavailable)?,
                draft_version: saved.plan_version,
                status: match saved.status {
                    DraftStatus::ReadyToConfirm => "READY_TO_CONFIRM",
                    DraftStatus::Cancelled => "CANCELLED",
                    DraftStatus::Expired => "EXPIRED",
                    DraftStatus::NeedsInformation => "NEEDS_INFORMATION",
                }
                .into(),
                expires_at: chrono::DateTime::from_timestamp(
                    i64::try_from(saved.expires_at)
                        .map_err(|_| StorageError::DependencyUnavailable)?,
                    0,
                )
                .ok_or(StorageError::DependencyUnavailable)?,
                parameters: json!({"payer_account_id":details.account.account_id,"payee_id":details.payee.payee_id,"amount":details.amount,"currency":details.currency,"remark":details.remark}),
            }))
        })
    }
}

fn contracts() -> Vec<PreflightInputContract> {
    [
        MockDraftAction::Create,
        MockDraftAction::Modify,
        MockDraftAction::Cancel,
    ]
    .into_iter()
    .map(|action| PreflightInputContract {
        tool_name: ToolName::parse(action.tool_name()).unwrap(),
        tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
        policy_version: PolicyVersion::parse(if action == MockDraftAction::Create {
            MOCK_DRAFT_POLICY_VERSION
        } else {
            MOCK_DRAFT_LIFECYCLE_POLICY_VERSION
        })
        .unwrap(),
        projection_revision: "bounded-direct-input-v2".into(),
    })
    .collect()
}

struct Db {
    repository: PgRepository,
    sql: PgPool,
    admin: PgPool,
    namespace: String,
    sessions: Arc<Sessions>,
    store: Arc<MockDraftStore<SystemClock>>,
    audit: Arc<InMemoryAuditRecorder>,
    assembly: RuntimeAssembly,
}

impl Db {
    async fn new() -> Self { Self::with_draft_ttl(300).await }

    async fn with_draft_ttl(ttl:u64) -> Self {
        let url =
            std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("real isolated PostgreSQL required");
        let namespace = format!("sf_lifecycle_replay_{}", uuid::Uuid::new_v4().simple());
        let repository = PgRepository::connect(&url, &namespace).await.unwrap();
        repository.migrate().await.unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let path = namespace.clone();
        let sql = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let sessions = Arc::new(Sessions);
        let store = Arc::new(
            MockDraftStore::new(Arc::new(MockBankGateway::new()), SystemClock, ttl).unwrap(),
        );
        let audit = Arc::new(InMemoryAuditRecorder::new());
        let authority = Arc::new(MockDraftAuthority::new(
            sessions.clone(),
            Arc::new(AuthorityClock),
        ));
        let mut tools = ToolRegistry::new();
        let actual_tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(CreateMockDraftTool::new(store.clone(), authority.clone())),
            Arc::new(MockDraftLifecycleTool::modify(
                store.clone(),
                authority.clone(),
            )),
            Arc::new(MockDraftLifecycleTool::cancel(
                store.clone(),
                authority.clone(),
            )),
        ];
        for tool in actual_tools {
            tools.register(tool).unwrap();
        }
        let mut policies = DomainPolicyRegistry::new();
        let actual_policies: Vec<(Arc<dyn DomainPolicy>, &str)> = vec![
            (
                Arc::new(CreateMockDraftPolicy::new(store.clone())),
                MOCK_DRAFT_POLICY_VERSION,
            ),
            (
                Arc::new(MockDraftLifecyclePolicy::modify(store.clone())),
                MOCK_DRAFT_LIFECYCLE_POLICY_VERSION,
            ),
            (
                Arc::new(MockDraftLifecyclePolicy::cancel(store.clone())),
                MOCK_DRAFT_LIFECYCLE_POLICY_VERSION,
            ),
        ];
        for (policy, version) in actual_policies {
            policies
                .register(
                    policy,
                    PolicyVersion::parse(version).unwrap(),
                    MOCK_DRAFT_SCHEMA_VERSION,
                )
                .unwrap();
        }
        let mut catalog = ToolSecurityCatalog::new();
        for contract in contracts() {
            catalog
                .register(ToolSecurityProfile {
                    tool_name: contract.tool_name,
                    tool_schema_version: contract.tool_schema_version,
                    policy_version: contract.policy_version,
                    minimum_level: SecurityLevel::Green,
                })
                .unwrap();
        }
        let assembly = RuntimeBootstrap::build_mock_draft(
            tools,
            policies,
            catalog,
            sessions.clone(),
            audit.clone(),
            authority,
        )
        .unwrap();
        Self {
            repository,
            sql,
            admin,
            namespace,
            sessions,
            store,
            audit,
            assembly,
        }
    }

    fn service(&self) -> LifecycleService<'_> {
        LifecycleService::for_trusted_backend(
            self.repository.clone(),
            self.assembly.runtime(),
            self.sessions.clone(),
            Arc::new(StoreObserver(self.store.clone())),
            contracts(),
            DecisionConfig::new(
                Duration::from_secs(300),
                "isolated-lifecycle-replay-test",
                contracts(),
            )
            .unwrap(),
            ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),
        )
        .unwrap()
    }

    async fn confirmed(&self) -> LifecycleView {
        let service = self.service();
        self.confirmed_using(&service).await
    }

    async fn confirmed_using(&self,service:&LifecycleService<'_>) -> LifecycleView {
        let task = new_task_id();
        let context = ExecutionContext {
            task_id: task.clone(),
            node_id: NodeId::parse("candidate_lookup").unwrap(),
            operation_id: new_operation_id(),
            authenticated_user_id: safeflow_contracts::UserId::parse("user_001").unwrap(),
            plan_version: 1,
            idempotency_key: None,
        };
        self.store
            .find_candidates(&context, "\u{5c0f}\u{674e}")
            .await
            .unwrap();
        let created = service.create(&session(), &task, json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null}), 7).await.unwrap();
        let prepared = service
            .prepare(&session(), &task, &created.operation.operation_id)
            .await
            .unwrap();
        service
            .act(
                &session(),
                &task,
                &created.operation.operation_id,
                &prepared.confirmation.as_ref().unwrap().confirmation_ref,
                prepared.display_ticket.as_ref().unwrap(),
                PlanningConfirmationAction::Confirm,
            )
            .await
            .unwrap()
    }

    fn calls(&self) -> usize {
        self.audit
            .events()
            .unwrap()
            .iter()
            .filter(|event| event.kind == AuditEventKind::ToolStarted)
            .count()
    }

    async fn claim_status(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM mock_draft_lifecycle_claims ORDER BY operation_id")
            .fetch_all(&self.sql)
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.repository.close().await;
        self.sql.close().await;
        assert!(self.namespace.starts_with("sf_lifecycle_replay_") && self.namespace.len() == 52);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.namespace).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}

fn version(view: &LifecycleView) -> MockDraftExecutionVersion {
    MockDraftExecutionVersion {
        task_revision: view.task.revision,
        operation_revision: view.operation.revision,
        plan_version: view.task.plan.plan_version,
        facts_version: view.task.facts_version,
    }
}

async fn reached(pause: &ExecutionPause) {
    tokio::time::timeout(Duration::from_secs(10), pause.reached.notified())
        .await
        .expect("execution did not reach deterministic barrier");
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn lifecycle_replay_after_empty_read_returns_settled_receipt_and_rejects_conflicts() {
    // Each contender has read None before the real winner executes and settles.
    for conflict in 0..6 {
        let db = Db::new().await;
        let confirmed = db.confirmed().await;
        let original = confirmed
            .confirmation
            .as_ref()
            .unwrap()
            .confirmation_ref
            .clone();
        let expected = version(&confirmed);
        let mut request = expected.clone();
        match conflict {
            2 => request.task_revision += 1,
            3 => request.operation_revision += 1,
            4 => request.plan_version += 1,
            5 => request.facts_version += 1,
            _ => {}
        }
        let reference = if conflict == 1 {
            ConfirmationRef::parse("different_original_confirmation").unwrap()
        } else {
            original.clone()
        };
        let pause = Arc::new(ExecutionPause::default());
        let mut contender = db.service();
        contender.execution_schedule.after_empty_replay = Some(pause.clone());
        let winner = db.service();
        let session = session();
        let mut waiting = Box::pin(contender.execute(
            &session,
            &confirmed.task.task_id,
            &confirmed.operation.operation_id,
            &reference,
            &request,
        ));
        tokio::select! {
            _ = &mut waiting => panic!("contender completed before empty-read barrier"),
            _ = reached(&pause) => {}
        }
        assert!(db.claim_status().await.is_empty());
        assert_eq!(db.calls(), 0);
        let settled = winner
            .execute(
                &session,
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                &original,
                &expected,
            )
            .await
            .unwrap();
        assert_eq!(settled.task.state, TaskState::WaitingUser);
        assert_eq!(settled.operation.state, OperationState::Succeeded);
        assert_eq!(ensure_active(&settled), Err(StorageError::Terminal));
        let receipt = settled.receipt.as_ref().unwrap();
        let actual = db
            .store
            .read_created_for_task(&LifecycleService::context(
                &settled.task,
                &settled.operation,
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(actual.draft_id, receipt.draft_id);
        assert_eq!(
            actual.operation_id.as_str(),
            receipt.business_operation_id.as_str()
        );
        assert_eq!(actual.plan_version, receipt.draft_version);
        assert_eq!(db.calls(), 1);
        pause.release.notify_one();
        let replay = waiting.await;
        if conflict == 0 {
            let replay = replay.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.task, settled.task);
            assert_eq!(replay.operation, settled.operation);
            assert_eq!(replay.receipt, settled.receipt);
        } else {
            assert_eq!(replay.err(), Some(StorageError::DecisionConflict));
        }
        assert_eq!(db.claim_status().await, vec!["SUCCEEDED"]);
        assert_eq!(db.calls(), 1);
        drop(contender);
        drop(winner);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn lifecycle_replay_after_empty_read_returns_original_unsettled_claim() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    let original = &confirmed.confirmation.as_ref().unwrap().confirmation_ref;
    let expected = version(&confirmed);
    let a_pause = Arc::new(ExecutionPause::default());
    let b_pause = Arc::new(ExecutionPause::default());
    let mut contender = db.service();
    contender.execution_schedule.after_empty_replay = Some(a_pause.clone());
    let mut winner = db.service();
    winner.execution_schedule.after_claim_commit = Some(b_pause.clone());
    let session = session();
    let mut a = Box::pin(contender.execute(
        &session,
        &confirmed.task.task_id,
        &confirmed.operation.operation_id,
        original,
        &expected,
    ));
    tokio::select! {
        _ = &mut a => panic!("contender completed before empty-read barrier"),
        _ = reached(&a_pause) => {}
    }
    let mut b = Box::pin(winner.execute(
        &session,
        &confirmed.task.task_id,
        &confirmed.operation.operation_id,
        original,
        &expected,
    ));
    tokio::select! {
        _ = &mut b => panic!("winner completed before claim-commit barrier"),
        _ = reached(&b_pause) => {}
    }
    assert_eq!(db.claim_status().await, vec!["CLAIMED"]);
    assert_eq!(db.calls(), 0);
    a_pause.release.notify_one();
    let unknown = a.await.unwrap();
    assert!(unknown.replayed);
    assert_eq!(unknown.task.task_id, confirmed.task.task_id);
    assert_eq!(
        unknown.operation.operation_id,
        confirmed.operation.operation_id
    );
    assert_eq!(
        unknown.unknown_operation,
        Some(confirmed.operation.operation_id.clone())
    );
    assert!(unknown.receipt.is_none());
    assert_eq!(
        ensure_active(&unknown),
        Err(StorageError::RuntimeAlreadyClaimed)
    );
    assert_eq!(db.calls(), 0);
    assert_eq!(db.claim_status().await, vec!["CLAIMED"]);
    b_pause.release.notify_one();
    let settled = b.await.unwrap();
    assert!(settled.receipt.is_some());
    assert_eq!(db.claim_status().await, vec!["SUCCEEDED"]);
    let replay = contender
        .execute(
            &session,
            &confirmed.task.task_id,
            &confirmed.operation.operation_id,
            original,
            &expected,
        )
        .await
        .unwrap();
    assert_eq!(replay.receipt, settled.receipt);
    assert_eq!(db.calls(), 1);
    drop(contender);
    drop(winner);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual draft Tool/Policy/Store"]
async fn lifecycle_replay_without_claim_preserves_original_errors_and_never_executes() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    let original = &confirmed.confirmation.as_ref().unwrap().confirmation_ref;
    let expected = version(&confirmed);
    let service = db.service();
    let reference = ConfirmationRef::parse("different_original_confirmation").unwrap();
    assert_eq!(
        service
            .execute(
                &session(),
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                &reference,
                &expected
            )
            .await
            .err(),
        Some(StorageError::ConfirmationRequired)
    );
    for field in 0..4 {
        let mut request = expected.clone();
        let error = match field {
            0 => {
                request.task_revision += 1;
                StorageError::RevisionConflict
            }
            1 => {
                request.operation_revision += 1;
                StorageError::RevisionConflict
            }
            2 => {
                request.plan_version += 1;
                StorageError::StalePlan
            }
            _ => {
                request.facts_version += 1;
                StorageError::FactsConflict
            }
        };
        assert_eq!(
            service
                .execute(
                    &session(),
                    &confirmed.task.task_id,
                    &confirmed.operation.operation_id,
                    original,
                    &request
                )
                .await
                .err(),
            Some(error)
        );
    }
    assert_eq!(
        service
            .execute(
                &SessionRef::parse("unavailable_session").unwrap(),
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                original,
                &expected
            )
            .await
            .err(),
        Some(StorageError::Session)
    );
    assert!(db.claim_status().await.is_empty());
    assert_eq!(db.calls(), 0);
    drop(service);
    db.finish().await;
}
