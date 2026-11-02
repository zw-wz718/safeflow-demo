//! Public foundation tests wrap the existing confirmed bank query, not a new business plugin.
use super::*;
use safeflow_runtime::PreflightInputContract;
use safeflow_storage::{
    PlanningTaskCreationOutcome, PlanningTaskCreationService, RuntimeTaskExecutionError,
    RuntimeTaskExecutionService, RuntimeTaskExecutionStatus, RuntimeTaskVersion, TaskQueryService,
};

pub(super) fn contracts() -> Vec<PreflightInputContract> {
    vec![PreflightInputContract {
        tool_name: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
        tool_schema_version: 1,
        policy_version: PolicyVersion::parse("balance-policy-v1").unwrap(),
        projection_revision: "bank-direct-input-v1".into(),
    }]
}
fn read_plan(wait: bool) -> AgentPlan {
    let mut plan = plan();
    if !wait {
        plan.nodes.pop();
    }
    plan
}
fn proposal(plan: AgentPlan) -> PlanningResult {
    PlanningResult::for_plan(
        Understanding {
            intent: "READ_BALANCE".into(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    )
}
pub(super) fn service<'a>(db: &Db, rig: &'a Rig) -> RuntimeTaskExecutionService<'a> {
    RuntimeTaskExecutionService::for_trusted_backend(db.repo.clone(), rig.runtime(), contracts())
        .unwrap()
}
pub(super) async fn create(db: &Db, rig: &Rig, plan: AgentPlan) -> safeflow_storage::TaskRecord {
    let id = plan.task_id.clone();
    let version = plan.plan_version;
    let intake = PlanningTaskCreationService::for_trusted_backend(
        db.repo.clone(),
        rig.runtime(),
        None,
        contracts(),
    )
    .unwrap();
    match intake
        .accept_json_and_create(
            &session(),
            &id,
            version,
            7,
            &serde_json::to_vec(&proposal(plan)).unwrap(),
        )
        .await
        .unwrap()
    {
        PlanningTaskCreationOutcome::TaskCreated(task) => task,
        _ => panic!("expected initial task"),
    }
}

#[tokio::test]
async fn schema_checked_read_only_execution_completes_and_old_wait_contract_stays_strict() {
    let rig = Rig::new(Sessions::new(), Options::default());
    let plan = read_plan(false);
    assert!(rig
        .runtime()
        .review_read_only_plan(plan.clone(), session(), 7)
        .await
        .is_err());
    let review = rig
        .runtime()
        .review_read_only_execution(plan.clone(), session(), 7, contracts())
        .await
        .unwrap();
    let verified = rig
        .runtime()
        .execute_reviewed_read_only(&review)
        .await
        .unwrap();
    assert_eq!(verified.plan(), &plan);
    assert!(verified.waiting_node().is_none());
    assert_eq!(verified.completed_nodes(), &[node("read")]);
    assert_eq!(verified.results().len(), 1);
    assert_eq!(rig.calls(), 1);
}

#[tokio::test]
async fn execution_review_rejects_bad_schema_and_all_plan_writes_before_calls() {
    for arguments in [
        json!({"account_id":"account_001","extra":true}),
        json!({}),
        json!({"account_id":1}),
    ] {
        let rig = Rig::new(Sessions::new(), Options::default());
        let mut plan = read_plan(false);
        let PlanNode::Tool {
            arguments: stored, ..
        } = &mut plan.nodes[0]
        else {
            unreachable!()
        };
        *stored = arguments;
        assert!(rig
            .runtime()
            .review_read_only_execution(plan, session(), 7, contracts())
            .await
            .is_err());
        assert_eq!(rig.calls(), 0);
    }
    let rig = Rig::new(
        Sessions::new(),
        Options {
            side_effect: true,
            ..Options::default()
        },
    );
    let mut plan = read_plan(true);
    plan.nodes.swap(0, 1);
    if let PlanNode::UserDecision { depends_on, .. } = &mut plan.nodes[0] {
        depends_on.clear();
    }
    if let PlanNode::Tool { depends_on, .. } = &mut plan.nodes[1] {
        *depends_on = vec![node("choose")];
    }
    assert!(matches!(
        rig.runtime()
            .review_read_only_execution(plan, session(), 7, contracts())
            .await,
        Err(RuntimeError::WriteToolRequiresLaterStage(_))
    ));
    assert_eq!(rig.calls(), 0);
}

#[tokio::test]
async fn execution_review_drift_and_failed_audit_never_produce_sealed_success() {
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let review = rig
        .runtime()
        .review_read_only_execution(read_plan(false), session(), 7, contracts())
        .await
        .unwrap();
    sessions.set(Some("user_001"));
    assert!(rig
        .runtime()
        .execute_reviewed_read_only(&review)
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    for fail in [AuditEventKind::TaskCompleted, AuditEventKind::ToolCompleted] {
        let rig = Rig::new(
            Sessions::new(),
            Options {
                audit_fail: Some(fail),
                ..Options::default()
            },
        );
        let review = rig
            .runtime()
            .review_read_only_execution(read_plan(false), session(), 7, contracts())
            .await
            .unwrap();
        assert!(rig
            .runtime()
            .execute_reviewed_read_only(&review)
            .await
            .is_err());
        assert_eq!(rig.calls(), 1);
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn same_id_intake_schedule_complete_or_wait_then_new_runtime_recovers_without_execute() {
    for wait in [false, true] {
        let db = Db::new().await;
        let sessions = Sessions::new();
        let rig = Rig::new(sessions.clone(), Options::default());
        let initial = create(&db, &rig, read_plan(wait)).await;
        assert_eq!(rig.calls(), 0);
        let view = service(&db, &rig)
            .schedule(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&initial),
            )
            .await
            .unwrap();
        assert_eq!(view.task.task_id, initial.task_id);
        assert_eq!(
            view.task.state,
            if wait {
                TaskState::WaitingUser
            } else {
                TaskState::Completed
            }
        );
        assert_eq!(view.result_references.len(), 1);
        assert_eq!(rig.calls(), 1);
        let progress = TaskQueryService::for_trusted_backend(db.repo.clone(), rig.runtime())
            .query(&session(), &initial.task_id)
            .await
            .unwrap();
        assert_eq!(progress.task_state, view.task.state);
        assert!(service(&db, &rig)
            .schedule(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&view.task)
            )
            .await
            .is_err());
        let restarted = Rig::new(Sessions::new(), Options::default());
        let recovered = service(&db, &restarted)
            .recover(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&view.task),
            )
            .await
            .unwrap();
        assert_eq!(recovered, view);
        assert_eq!(restarted.calls(), 0);
        let new_login = SessionRef::parse("new_owner_login").unwrap();
        let mut new_sessions = safeflow_runtime::StaticSessionResolver::new();
        new_sessions.insert(new_login.clone(), UserId::parse("user_001").unwrap());
        let reauthenticated = Rig::with_resolver(Arc::new(new_sessions), Options::default());
        assert_eq!(
            service(&db, &reauthenticated)
                .recover(
                    &new_login,
                    &initial.task_id,
                    RuntimeTaskVersion::of(&view.task)
                )
                .await
                .unwrap(),
            view
        );
        assert_eq!(reauthenticated.calls(), 0);
        // Re-login by the same owner can inspect/revalidate the original task.
        sessions.set(Some("user_001"));
        let recovered = service(&db, &rig)
            .recover(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&view.task),
            )
            .await
            .unwrap();
        assert_eq!(recovered, view);
        assert_eq!(rig.calls(), 1);
        let other = SessionRef::parse("other_session").unwrap();
        assert_eq!(
            service(&db, &rig)
                .recover(&other, &initial.task_id, RuntimeTaskVersion::of(&view.task))
                .await
                .unwrap_err(),
            RuntimeTaskExecutionError::Storage(StorageError::NotFound)
        );
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn durable_claim_blocks_concurrent_duplicate_and_interrupted_reexecution() {
    for interrupt in [false, true] {
        let db = Db::new().await;
        let gate = Gate::armed();
        let rig = Arc::new(Rig::new(
            Sessions::new(),
            Options {
                tool_gate: Some(gate.clone()),
                ..Options::default()
            },
        ));
        let initial = create(&db, &rig, read_plan(false)).await;
        let running_rig = rig.clone();
        let repo = db.repo.clone();
        let task = initial.clone();
        let running = tokio::spawn(async move {
            RuntimeTaskExecutionService::for_trusted_backend(
                repo,
                running_rig.runtime(),
                contracts(),
            )
            .unwrap()
            .schedule(&session(), &task.task_id, RuntimeTaskVersion::of(&task))
            .await
        });
        gate.reached.notified().await;
        let error = service(&db, &rig)
            .schedule(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&initial),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RuntimeTaskExecutionError::Storage(StorageError::RuntimeAlreadyClaimed)
        );
        assert_eq!(rig.calls(), 1);
        if interrupt {
            running.abort();
            assert!(running.await.unwrap_err().is_cancelled());
            let restarted = Rig::new(Sessions::new(), Options::default());
            let view = service(&db, &restarted)
                .recover(
                    &session(),
                    &initial.task_id,
                    RuntimeTaskVersion::of(&initial),
                )
                .await
                .unwrap();
            assert_eq!(
                view.execution_status,
                RuntimeTaskExecutionStatus::RequiresOriginalTaskReview
            );
            assert!(service(&db, &restarted)
                .schedule(
                    &session(),
                    &initial.task_id,
                    RuntimeTaskVersion::of(&initial)
                )
                .await
                .is_err());
            assert_eq!(restarted.calls(), 0);
        } else {
            gate.release.notify_one();
            assert_eq!(
                running.await.unwrap().unwrap().task.state,
                TaskState::Completed
            );
        }
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn schedule_rejects_stale_versions_other_owner_schema_and_write_nodes_with_zero_calls() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let initial = create(&db, &rig, read_plan(true)).await;
    for expected in [
        RuntimeTaskVersion {
            task_revision: 2,
            ..RuntimeTaskVersion::of(&initial)
        },
        RuntimeTaskVersion {
            plan_version: 3,
            ..RuntimeTaskVersion::of(&initial)
        },
        RuntimeTaskVersion {
            facts_version: 8,
            ..RuntimeTaskVersion::of(&initial)
        },
    ] {
        assert!(service(&db, &rig)
            .schedule(&session(), &initial.task_id, expected)
            .await
            .is_err());
    }
    assert!(service(&db, &rig)
        .schedule(
            &SessionRef::parse("other_session").unwrap(),
            &initial.task_id,
            RuntimeTaskVersion::of(&initial)
        )
        .await
        .is_err());
    sqlx::query("UPDATE tasks SET plan=jsonb_set(plan,'{nodes,0,arguments}', '{\"account_id\":\"account_001\",\"extra\":true}'::jsonb) WHERE task_id=$1").bind(initial.task_id.as_str()).execute(&db.scoped).await.unwrap();
    assert!(service(&db, &rig)
        .schedule(
            &session(),
            &initial.task_id,
            RuntimeTaskVersion::of(&initial)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_read_claims")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(claimed, 0);
    let initial = create(&db, &rig, read_plan(true)).await;
    rig.tool.definition.lock().unwrap().has_side_effect = true;
    assert!(service(&db, &rig)
        .schedule(
            &session(),
            &initial.task_id,
            RuntimeTaskVersion::of(&initial)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    rig.tool.definition.lock().unwrap().has_side_effect = false;
    sessions.set(Some("user_001"));
    assert!(service(&db, &rig)
        .schedule(
            &session(),
            &initial.task_id,
            RuntimeTaskVersion::of(&initial)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn waiting_then_write_plan_and_handwritten_tasks_never_gain_execution_authority() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let rig = Rig::new(
        sessions.clone(),
        Options {
            side_effect: true,
            ..Options::default()
        },
    );
    let mut plan = read_plan(true);
    plan.nodes.swap(0, 1);
    if let PlanNode::UserDecision { depends_on, .. } = &mut plan.nodes[0] {
        depends_on.clear();
    }
    if let PlanNode::Tool { depends_on, .. } = &mut plan.nodes[1] {
        *depends_on = vec![node("choose")];
    }
    let initial = create(&db, &rig, plan).await;
    assert!(service(&db, &rig)
        .schedule(
            &session(),
            &initial.task_id,
            RuntimeTaskVersion::of(&initial)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    let normal = Rig::new(sessions.clone(), Options::default());
    let raw = db.raw(sessions);
    let handwritten = raw
        .create_task(&session(), read_plan(false), 7)
        .await
        .unwrap();
    assert_eq!(
        service(&db, &normal)
            .schedule(
                &session(),
                &handwritten.task_id,
                RuntimeTaskVersion::of(&handwritten)
            )
            .await
            .unwrap_err(),
        RuntimeTaskExecutionError::Storage(StorageError::RuntimeIntakeRequired)
    );
    assert_eq!(normal.calls(), 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn claim_database_wait_preserves_original_session_and_registration() {
    for registration_drift in [false, true] {
        let db = Db::new().await;
        let sessions = Sessions::new();
        let rig = Rig::new(sessions.clone(), Options::default());
        let initial = create(&db, &rig, read_plan(false)).await;
        let service = service(&db, &rig);
        let mut lock = db.scoped.begin().await.unwrap();
        lock.execute("LOCK TABLE runtime_read_claims IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let current_session = session();
        let (result, ()) = tokio::join!(
            service.schedule(
                &current_session,
                &initial.task_id,
                RuntimeTaskVersion::of(&initial)
            ),
            async {
                tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='runtime_read_claims'::regclass AND NOT granted)").fetch_one(&db.scoped).await.unwrap();
                    if waiting { break; }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
                if registration_drift {
                    rig.tool
                        .definition
                        .lock()
                        .unwrap()
                        .description
                        .push_str(" changed");
                } else {
                    sessions.set(Some("user_001"));
                }
                lock.commit().await.unwrap();
            }
        );
        assert!(result.is_err());
        assert_eq!(rig.calls(), 0);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_read_claims")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
        assert_eq!(count, 0);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn running_plan_session_or_registration_drift_cannot_save_stale_success() {
    for drift in 0..3 {
        let db = Db::new().await;
        let sessions = Sessions::new();
        let gate = Gate::armed();
        let rig = Arc::new(Rig::new(
            sessions.clone(),
            Options {
                tool_gate: Some(gate.clone()),
                ..Options::default()
            },
        ));
        let initial = create(&db, &rig, read_plan(false)).await;
        let running_rig = rig.clone();
        let repo = db.repo.clone();
        let task = initial.clone();
        let running = tokio::spawn(async move {
            RuntimeTaskExecutionService::for_trusted_backend(
                repo,
                running_rig.runtime(),
                contracts(),
            )
            .unwrap()
            .schedule(&session(), &task.task_id, RuntimeTaskVersion::of(&task))
            .await
        });
        gate.reached.notified().await;
        match drift {
            0 => {
                sqlx::query("UPDATE tasks SET revision=revision+1,plan_version=plan_version+1,plan=jsonb_set(plan,'{plan_version}',to_jsonb(plan_version+1)) WHERE task_id=$1").bind(initial.task_id.as_str()).execute(&db.scoped).await.unwrap();
            }
            1 => sessions.set(Some("user_001")),
            _ => rig
                .tool
                .definition
                .lock()
                .unwrap()
                .description
                .push_str(" changed"),
        }
        gate.release.notify_one();
        assert!(running.await.unwrap().is_err());
        let state: String = sqlx::query_scalar("SELECT state FROM tasks WHERE task_id=$1")
            .bind(initial.task_id.as_str())
            .fetch_one(&db.scoped)
            .await
            .unwrap();
        assert_eq!(state, "RUNNING");
        assert_eq!(rig.calls(), 1);
        let status: String =
            sqlx::query_scalar("SELECT status FROM runtime_read_claims WHERE task_id=$1")
                .bind(initial.task_id.as_str())
                .fetch_one(&db.scoped)
                .await
                .unwrap();
        assert_eq!(status, "CLAIMED");
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn executor_failures_are_persisted_failed_and_never_retried() {
    for mode in [
        ToolMode::Error,
        ToolMode::FailedResult,
        ToolMode::MismatchedResult,
    ] {
        let db = Db::new().await;
        let rig = Rig::new(
            Sessions::new(),
            Options {
                mode,
                ..Options::default()
            },
        );
        let initial = create(&db, &rig, read_plan(false)).await;
        assert!(service(&db, &rig)
            .schedule(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&initial)
            )
            .await
            .is_err());
        let progress = TaskQueryService::for_trusted_backend(db.repo.clone(), rig.runtime())
            .query(&session(), &initial.task_id)
            .await
            .unwrap();
        assert_eq!(progress.task_state, TaskState::Failed);
        let expected = RuntimeTaskVersion {
            task_revision: progress.revision,
            plan_version: progress.plan_version,
            facts_version: progress.facts_version,
        };
        let recovered = service(&db, &rig)
            .recover(&session(), &initial.task_id, expected)
            .await
            .unwrap();
        assert_eq!(
            recovered.execution_status,
            RuntimeTaskExecutionStatus::Failed
        );
        assert!(recovered.result_references.is_empty());
        assert!(service(&db, &rig)
            .schedule(&session(), &initial.task_id, expected)
            .await
            .is_err());
        assert_eq!(rig.calls(), 1);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn claim_or_completion_audit_and_trigger_faults_never_return_fake_completion() {
    for fault in 0..10 {
        let db = Db::new().await;
        let rig = Rig::new(Sessions::new(), Options::default());
        let initial = create(&db, &rig, read_plan(false)).await;
        let function = if fault == 8 {
            "IF NEW.details ? 'runtime_read_claim' THEN DELETE FROM audit_events WHERE event_id=NEW.event_id; END IF;"
        } else if fault == 9 {
            "IF NEW.details ? 'runtime_read_claim' THEN UPDATE audit_events SET details='{}' WHERE event_id=NEW.event_id; END IF;"
        } else {
            match fault % 4 {
            0 => "IF NEW.details ? 'runtime_read_claim' THEN RAISE EXCEPTION 'claim audit fault'; END IF;",
            1 => "IF NEW.details ? 'runtime_read_execution' THEN RAISE EXCEPTION 'completion audit fault'; END IF;",
            2 => "IF NEW.details ? 'runtime_read_claim' THEN UPDATE tasks SET state='FAILED',checkpoint='{\"kind\":\"FINISHED\"}' WHERE task_id=NEW.task_id; END IF;",
            _ => "IF NEW.details ? 'runtime_read_execution' THEN UPDATE tasks SET state='FAILED',checkpoint='{\"kind\":\"FINISHED\"}' WHERE task_id=NEW.task_id; END IF;",
        }
        };
        db.scoped.execute(format!("CREATE FUNCTION runtime_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN {function} RETURN NEW; END $$").as_str()).await.unwrap();
        db.scoped.execute(if fault < 4 || fault >= 8 { "CREATE TRIGGER runtime_fault AFTER INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION runtime_fault()" }
            else { "CREATE CONSTRAINT TRIGGER runtime_fault AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION runtime_fault()" }).await.unwrap();
        assert!(service(&db, &rig)
            .schedule(
                &session(),
                &initial.task_id,
                RuntimeTaskVersion::of(&initial)
            )
            .await
            .is_err());
        assert_eq!(
            rig.calls(),
            if fault >= 8 || fault % 4 == 0 || fault % 4 == 2 {
                0
            } else {
                1
            }
        );
        let progress = TaskQueryService::for_trusted_backend(db.repo.clone(), rig.runtime())
            .query(&session(), &initial.task_id)
            .await
            .unwrap();
        assert_eq!(
            progress.task_state,
            if matches!(fault, 6 | 7) {
                TaskState::Failed
            } else {
                TaskState::Running
            }
        );
        if fault < 8 && (fault % 4 == 1 || fault % 4 == 3) {
            assert!(service(&db, &rig)
                .schedule(
                    &session(),
                    &initial.task_id,
                    RuntimeTaskVersion::of(&initial)
                )
                .await
                .is_err());
            assert_eq!(rig.calls(), 1);
        }
        db.finish().await;
    }
}
