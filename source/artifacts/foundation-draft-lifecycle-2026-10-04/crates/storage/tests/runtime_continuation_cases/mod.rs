//! Continuation tests use the existing balance Tool and confirmed direct-input contract.
use super::runtime_execution_cases::{contracts, create, service as initial_service};
use super::*;
use safeflow_runtime::ReadOnlyContinuationSource;
use safeflow_storage::{
    RuntimeTaskContinuationService, RuntimeTaskExecutionStatus, RuntimeTaskVersion,
    TaskQueryService,
};

fn multi_plan() -> AgentPlan {
    let mut p = plan();
    p.nodes.extend([
        PlanNode::Tool {
            id: node("read_two"),
            tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            arguments: json!({"account_id":"account_001"}),
            depends_on: vec![node("choose")],
        },
        PlanNode::UserDecision {
            id: node("choose_two"),
            depends_on: vec![node("read_two")],
        },
        PlanNode::Tool {
            id: node("read_three"),
            tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            arguments: json!({"account_id":"account_001"}),
            depends_on: vec![node("choose_two")],
        },
    ]);
    p
}
fn service<'a>(db: &Db, rig: &'a Rig) -> RuntimeTaskContinuationService<'a> {
    RuntimeTaskContinuationService::for_trusted_backend(db.repo.clone(), rig.runtime(), contracts())
        .unwrap()
}
async fn waiting(db: &Db, rig: &Rig) -> safeflow_storage::RuntimeTaskExecutionView {
    let initial = create(db, rig, multi_plan()).await;
    initial_service(db, rig)
        .schedule(
            &session(),
            &initial.task_id,
            RuntimeTaskVersion::of(&initial),
        )
        .await
        .unwrap()
}
struct TrustedCheckpoint {
    plan: AgentPlan,
    completed: Vec<NodeId>,
    accepted: NodeId,
}
impl ReadOnlyContinuationSource for TrustedCheckpoint {
    fn plan(&self) -> &AgentPlan {
        &self.plan
    }
    fn owner(&self) -> &UserId {
        static USER: std::sync::OnceLock<UserId> = std::sync::OnceLock::new();
        USER.get_or_init(|| UserId::parse("user_001").unwrap())
    }
    fn facts_version(&self) -> u64 {
        7
    }
    fn completed_nodes(&self) -> &[NodeId] {
        &self.completed
    }
    fn accepted_waiting_node(&self) -> &NodeId {
        &self.accepted
    }
}

#[tokio::test]
async fn reviewed_runtime_skips_prior_nodes_and_returns_only_new_segment_results() {
    let rig = Rig::new(Sessions::new(), Options::default());
    let p = multi_plan();
    let review = rig
        .runtime()
        .review_read_only_execution(p.clone(), session(), 7, contracts())
        .await
        .unwrap();
    let initial = rig
        .runtime()
        .execute_reviewed_read_only(&review)
        .await
        .unwrap();
    let mut source = TrustedCheckpoint {
        plan: p.clone(),
        completed: initial.completed_nodes().to_vec(),
        accepted: node("choose"),
    };
    let second = rig
        .runtime()
        .continue_reviewed_read_only(&review, &source)
        .await
        .unwrap();
    assert_eq!(second.waiting_node(), Some(&node("choose_two")));
    assert_eq!(second.results().len(), 1);
    assert_eq!(
        second.results()[0].operation_id.as_str(),
        format!("operation_{}_read_two", p.task_id.as_str())
    );
    source.completed = second.completed_nodes().to_vec();
    source.accepted = node("choose_two");
    let final_run = rig
        .runtime()
        .continue_reviewed_read_only(&review, &source)
        .await
        .unwrap();
    assert_eq!(final_run.completed_nodes().len(), 5);
    assert_eq!(final_run.results().len(), 1);
    assert!(final_run.waiting_node().is_none());
    assert_eq!(rig.calls(), 3);
}

#[tokio::test]
async fn trusted_runtime_seed_must_be_exact_scheduled_prefix_and_next_decision() {
    let rig = Rig::new(Sessions::new(), Options::default());
    let p = multi_plan();
    let review = rig
        .runtime()
        .review_read_only_execution(p.clone(), session(), 7, contracts())
        .await
        .unwrap();
    for (completed, accepted) in [
        (vec![], "choose"),
        (vec![node("read"), node("read")], "choose"),
        (vec![node("missing")], "choose"),
        (vec![node("read")], "read_two"),
        (vec![node("read")], "choose_two"),
        (vec![node("read"), node("choose")], "choose"),
    ] {
        let source = TrustedCheckpoint {
            plan: p.clone(),
            completed,
            accepted: node(accepted),
        };
        assert!(rig
            .runtime()
            .continue_reviewed_read_only(&review, &source)
            .await
            .is_err());
    }
    let mut parallel = plan();
    parallel.nodes[1] = PlanNode::UserDecision {
        id: node("choose"),
        depends_on: vec![],
    };
    let review = rig
        .runtime()
        .review_read_only_execution(parallel.clone(), session(), 7, contracts())
        .await
        .unwrap();
    assert!(rig
        .runtime()
        .continue_reviewed_read_only(
            &review,
            &TrustedCheckpoint {
                plan: parallel,
                completed: vec![],
                accepted: node("choose")
            }
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
}

#[tokio::test]
async fn full_plan_write_refusal_and_binding_drift_apply_to_continuation() {
    let sessions = Sessions::new();
    let rig = Rig::new(sessions.clone(), Options::default());
    let p = multi_plan();
    let review = rig
        .runtime()
        .review_read_only_execution(p.clone(), session(), 7, contracts())
        .await
        .unwrap();
    let source = TrustedCheckpoint {
        plan: p,
        completed: vec![node("read")],
        accepted: node("choose"),
    };
    rig.tool.definition.lock().unwrap().has_side_effect = true;
    assert!(rig
        .runtime()
        .continue_reviewed_read_only(&review, &source)
        .await
        .is_err());
    rig.tool.definition.lock().unwrap().has_side_effect = false;
    sessions.set(Some("user_001"));
    assert!(rig
        .runtime()
        .continue_reviewed_read_only(&review, &source)
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn multi_wait_continues_same_plan_and_new_process_relogin_replays_without_tools() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let initial = Rig::new(sessions.clone(), Options::default());
    let first = waiting(&db, &initial).await;
    assert_eq!(initial.calls(), 1);
    sessions.set(None);
    sessions.set(Some("user_001"));
    let fresh = Rig::new(sessions.clone(), Options::default());
    let continuation = service(&db, &fresh);
    let recovered = continuation
        .recover(
            &session(),
            &first.task.task_id,
            RuntimeTaskVersion::of(&first.task),
        )
        .await
        .unwrap();
    assert_eq!(fresh.calls(), 0);
    assert_eq!(recovered.result_references, first.result_references);
    let second = continuation
        .continue_task(
            &session(),
            &first.task.task_id,
            &node("choose"),
            RuntimeTaskVersion::of(&first.task),
        )
        .await
        .unwrap();
    assert_eq!(second.task.revision, first.task.revision + 2);
    assert_eq!(second.task.plan, first.task.plan);
    assert_eq!(
        second.execution_status,
        RuntimeTaskExecutionStatus::WaitingUser
    );
    assert_eq!(second.result_references.len(), 2);
    assert_eq!(fresh.calls(), 1);
    let repo = PgRepository::connect_existing(&db.url, &db.namespace)
        .await
        .unwrap();
    let process = Rig::new(sessions, Options::default());
    let continuation = RuntimeTaskContinuationService::for_trusted_backend(
        repo.clone(),
        process.runtime(),
        contracts(),
    )
    .unwrap();
    let completed = continuation
        .continue_task(
            &session(),
            &second.task.task_id,
            &node("choose_two"),
            RuntimeTaskVersion::of(&second.task),
        )
        .await
        .unwrap();
    assert_eq!(completed.task.state, TaskState::Completed);
    assert_eq!(completed.task.plan, first.task.plan);
    assert_eq!(completed.task.completed_nodes.len(), 5);
    assert_eq!(completed.result_references.len(), 3);
    assert_eq!(process.calls(), 1);
    let replay = continuation
        .continue_task(
            &session(),
            &first.task.task_id,
            &node("choose"),
            RuntimeTaskVersion::of(&first.task),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.task, second.task);
    assert_eq!(replay.current_task_revision, completed.task.revision);
    assert_eq!(process.calls(), 1);
    let read = continuation
        .recover(
            &session(),
            &completed.task.task_id,
            RuntimeTaskVersion::of(&completed.task),
        )
        .await
        .unwrap();
    assert_eq!(read, completed);
    assert_eq!(process.calls(), 1);
    let stored: String = sqlx::query_scalar("SELECT string_agg(claim::text || coalesce(receipt::text,''), '') FROM runtime_read_continuations").fetch_one(&db.scoped).await.unwrap();
    assert!(!stored.contains("available_balance") && !stored.contains("ledger_snapshot"));
    repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn wrong_node_versions_other_user_and_legacy_wait_have_no_calls() {
    let db = Db::new().await;
    let rig = Rig::new(Sessions::new(), Options::default());
    let first = waiting(&db, &rig).await;
    let current = RuntimeTaskVersion::of(&first.task);
    for expected in [
        RuntimeTaskVersion {
            task_revision: current.task_revision + 1,
            ..current
        },
        RuntimeTaskVersion {
            plan_version: current.plan_version + 1,
            ..current
        },
        RuntimeTaskVersion {
            facts_version: current.facts_version + 1,
            ..current
        },
    ] {
        assert!(service(&db, &rig)
            .continue_task(&session(), &first.task.task_id, &node("choose"), expected)
            .await
            .is_err());
    }
    for waiting_node in [node("read"), node("choose_two"), node("missing")] {
        assert!(service(&db, &rig)
            .continue_task(&session(), &first.task.task_id, &waiting_node, current)
            .await
            .is_err());
    }
    assert!(service(&db, &rig)
        .continue_task(
            &SessionRef::parse("other_session").unwrap(),
            &first.task.task_id,
            &node("choose"),
            current
        )
        .await
        .is_err());
    let legacy = db
        .service(&rig)
        .execute_until_wait(session(), plan(), 7)
        .await
        .unwrap();
    assert!(service(&db, &rig)
        .continue_task(
            &session(),
            &legacy.task.task_id,
            &node("choose"),
            RuntimeTaskVersion::of(&legacy.task)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_read_continuations")
            .fetch_one(&db.scoped)
            .await
            .unwrap(),
        0
    );
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn concurrent_pending_request_and_queries_never_repeat_tools_or_prompt_continue() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
    let gate = Gate::armed();
    let rig = Rig::new(
        sessions,
        Options {
            tool_gate: Some(gate.clone()),
            ..Options::default()
        },
    );
    let continuation = service(&db, &rig);
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            async {
                continuation
                    .continue_task(
                        &session(),
                        &first.task.task_id,
                        &node("choose"),
                        RuntimeTaskVersion::of(&first.task),
                    )
                    .await
            },
            async {
                gate.reached.notified().await;
                let pending = db
                    .raw(Sessions::new())
                    .recover(&session(), &first.task.task_id)
                    .await
                    .unwrap()
                    .task;
                assert_eq!(pending.state, TaskState::Running);
                let progress =
                    TaskQueryService::for_trusted_backend(db.repo.clone(), rig.runtime())
                        .query(&session(), &first.task.task_id)
                        .await
                        .unwrap();
                assert_eq!(progress.task_state, TaskState::Running);
                assert!(!serde_json::to_string(&progress)
                    .unwrap()
                    .contains("AWAIT_USER"));
                let view = continuation
                    .recover(
                        &session(),
                        &pending.task_id,
                        RuntimeTaskVersion::of(&pending),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    view.execution_status,
                    RuntimeTaskExecutionStatus::RequiresOriginalTaskReview
                );
                assert_eq!(view.result_references, first.result_references);
                assert!(continuation
                    .continue_task(
                        &session(),
                        &first.task.task_id,
                        &node("choose"),
                        RuntimeTaskVersion::of(&first.task)
                    )
                    .await
                    .is_err());
                assert_eq!(rig.calls(), 1);
                gate.release.notify_one();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(
        result.unwrap().execution_status,
        RuntimeTaskExecutionStatus::WaitingUser
    );
    assert_eq!(rig.calls(), 1);
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn interrupted_claim_survives_new_process_without_automatic_retry() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
    let gate = Gate::armed();
    let rig = Rig::new(
        sessions.clone(),
        Options {
            tool_gate: Some(gate.clone()),
            ..Options::default()
        },
    );
    {
        let continuation = service(&db, &rig);
        let session_ref = session();
        let accepted = node("choose");
        let attempt = continuation.continue_task(
            &session_ref,
            &first.task.task_id,
            &accepted,
            RuntimeTaskVersion::of(&first.task),
        );
        tokio::pin!(attempt);
        tokio::select! { _ = gate.reached.notified() => {}, result = &mut attempt => panic!("unexpected {result:?}") }
    }
    let repo = PgRepository::connect_existing(&db.url, &db.namespace)
        .await
        .unwrap();
    let process = Rig::new(sessions.clone(), Options::default());
    let pending = db
        .raw(sessions)
        .recover(&session(), &first.task.task_id)
        .await
        .unwrap()
        .task;
    let continuation = RuntimeTaskContinuationService::for_trusted_backend(
        repo.clone(),
        process.runtime(),
        contracts(),
    )
    .unwrap();
    let recovered = continuation
        .recover(
            &session(),
            &pending.task_id,
            RuntimeTaskVersion::of(&pending),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.execution_status,
        RuntimeTaskExecutionStatus::RequiresOriginalTaskReview
    );
    assert!(continuation
        .continue_task(
            &session(),
            &first.task.task_id,
            &node("choose"),
            RuntimeTaskVersion::of(&first.task)
        )
        .await
        .is_err());
    assert_eq!(process.calls(), 0);
    repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn relogin_or_registration_drift_during_tool_keeps_pending_original_claim() {
    let db = Db::new().await;
    for registration in [false, true] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let gate = Gate::armed();
        let rig = Rig::new(
            sessions.clone(),
            Options {
                tool_gate: Some(gate.clone()),
                ..Options::default()
            },
        );
        let continuation = service(&db, &rig);
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                async {
                    continuation
                        .continue_task(
                            &session(),
                            &first.task.task_id,
                            &node("choose"),
                            RuntimeTaskVersion::of(&first.task),
                        )
                        .await
                },
                async {
                    gate.reached.notified().await;
                    if registration {
                        rig.tool.definition.lock().unwrap().schema_version = 2;
                    } else {
                        sessions.set(None);
                        sessions.set(Some("user_001"));
                    }
                    gate.release.notify_one();
                }
            )
        })
        .await
        .unwrap();
        assert!(result.is_err());
        let pending = db
            .raw(sessions)
            .recover(&session(), &first.task.task_id)
            .await
            .unwrap()
            .task;
        assert_eq!(pending.state, TaskState::Running);
        assert_eq!(pending.completed_nodes, first.task.completed_nodes);
        assert_eq!(rig.calls(), 1);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn source_plan_parameters_and_facts_drift_before_or_during_continue_is_not_overwritten() {
    let db = Db::new().await;
    for before in [true, false] {
        for mutation in ["plan", "parameters", "facts"] {
            let sessions = Sessions::new();
            let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
            let gate = Gate::armed();
            let rig = Rig::new(
                sessions.clone(),
                Options {
                    policy_gate: before.then(|| gate.clone()),
                    tool_gate: (!before).then(|| gate.clone()),
                    ..Options::default()
                },
            );
            let continuation = service(&db, &rig);
            let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(async { continuation.continue_task(&session(), &first.task.task_id, &node("choose"), RuntimeTaskVersion::of(&first.task)).await }, async {
                    gate.reached.notified().await;
                    let sql = match mutation {
                        "plan" => "UPDATE tasks SET plan_version=plan_version+1,plan=jsonb_set(plan,'{plan_version}',to_jsonb(plan_version+1)) WHERE task_id=$1",
                        "parameters" => "UPDATE tasks SET plan=jsonb_set(plan,'{nodes,2,arguments,account_id}','\"changed\"') WHERE task_id=$1",
                        _ => "UPDATE tasks SET facts_version=facts_version+1 WHERE task_id=$1",
                    };
                    sqlx::query(sql).bind(first.task.task_id.as_str()).execute(&db.scoped).await.unwrap();
                    gate.release.notify_one();
                })
            }).await.unwrap();
            assert!(result.is_err());
            assert_eq!(rig.calls(), usize::from(!before));
            let value: i64 = sqlx::query_scalar("SELECT revision FROM tasks WHERE task_id=$1")
                .bind(first.task.task_id.as_str())
                .fetch_one(&db.scoped)
                .await
                .unwrap();
            assert_eq!(value as u64, first.task.revision + u64::from(!before));
        }
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn claim_update_audit_and_deferred_commit_failures_happen_before_any_tool() {
    let db = Db::new().await;
    for fault in ["update", "audit", "deferred"] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let rig = Rig::new(sessions, Options::default());
        let function = if fault == "update" {
            "CREATE FUNCTION reject_segment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state='RUNNING' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$"
        } else {
            "CREATE FUNCTION reject_segment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'runtime_read_continuation_claim' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$"
        };
        db.scoped.execute(function).await.unwrap();
        db.scoped.execute(match fault { "update" => "CREATE TRIGGER reject_segment BEFORE UPDATE ON tasks FOR EACH ROW EXECUTE FUNCTION reject_segment()",
            "audit" => "CREATE TRIGGER reject_segment BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_segment()",
            _ => "CREATE CONSTRAINT TRIGGER reject_segment AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_segment()" }).await.unwrap();
        assert!(service(&db, &rig)
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        assert_eq!(rig.calls(), 0);
        assert_eq!(
            db.raw(Sessions::new())
                .recover(&session(), &first.task.task_id)
                .await
                .unwrap()
                .task,
            first.task
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM runtime_read_continuations WHERE task_id=$1"
            )
            .bind(first.task.task_id.as_str())
            .fetch_one(&db.scoped)
            .await
            .unwrap(),
            0
        );
        db.scoped
            .execute(
                format!(
                    "DROP TRIGGER reject_segment ON {}",
                    if fault == "update" {
                        "tasks"
                    } else {
                        "audit_events"
                    }
                )
                .as_str(),
            )
            .await
            .unwrap();
        db.scoped
            .execute("DROP FUNCTION reject_segment()")
            .await
            .unwrap();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn settlement_failures_keep_prior_references_and_never_allow_a_second_execution() {
    let db = Db::new().await;
    for deferred in [false, true] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let rig = Rig::new(sessions, Options::default());
        db.scoped.execute("CREATE FUNCTION reject_segment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'runtime_read_continuation_execution' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$").await.unwrap();
        db.scoped.execute(if deferred { "CREATE CONSTRAINT TRIGGER reject_segment AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_segment()" }
            else { "CREATE TRIGGER reject_segment BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_segment()" }).await.unwrap();
        let continuation = service(&db, &rig);
        assert!(continuation
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        db.scoped
            .execute("DROP TRIGGER reject_segment ON audit_events")
            .await
            .unwrap();
        db.scoped
            .execute("DROP FUNCTION reject_segment()")
            .await
            .unwrap();
        let pending = db
            .raw(Sessions::new())
            .recover(&session(), &first.task.task_id)
            .await
            .unwrap()
            .task;
        let recovered = continuation
            .recover(
                &session(),
                &pending.task_id,
                RuntimeTaskVersion::of(&pending),
            )
            .await
            .unwrap();
        assert_eq!(pending.state, TaskState::Running);
        assert_eq!(recovered.result_references, first.result_references);
        assert!(continuation
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        assert_eq!(rig.calls(), 1);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn runtime_tool_result_policy_and_audit_failures_settle_failed_without_fake_new_refs() {
    let db = Db::new().await;
    for options in [
        Options {
            mode: ToolMode::Error,
            ..Options::default()
        },
        Options {
            mode: ToolMode::MismatchedResult,
            ..Options::default()
        },
        Options {
            mode: ToolMode::FailedResult,
            ..Options::default()
        },
        Options {
            post_deny: true,
            ..Options::default()
        },
        Options {
            audit_fail: Some(AuditEventKind::ToolCompleted),
            ..Options::default()
        },
    ] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let rig = Rig::new(sessions, options);
        let continuation = service(&db, &rig);
        assert!(continuation
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        let failed = db
            .raw(Sessions::new())
            .recover(&session(), &first.task.task_id)
            .await
            .unwrap()
            .task;
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.completed_nodes, vec![node("read"), node("choose")]);
        let recovered = continuation
            .recover(&session(), &failed.task_id, RuntimeTaskVersion::of(&failed))
            .await
            .unwrap();
        assert_eq!(recovered.result_references, first.result_references);
        assert_eq!(rig.calls(), 1);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn chain_receipt_claim_and_audit_corruption_rejects_replay_before_tools() {
    let db = Db::new().await;
    for corrupt in [
        "root_audit",
        "root_receipt",
        "claim",
        "receipt",
        "audit",
        "extra_segment",
    ] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let rig = Rig::new(sessions, Options::default());
        let second = service(&db, &rig)
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task),
            )
            .await
            .unwrap();
        let sql = match corrupt {
            "root_audit" => "DELETE FROM audit_events WHERE task_id=$1 AND details ? 'runtime_read_execution'",
            "root_receipt" => "UPDATE runtime_read_claims SET receipt=jsonb_set(receipt,'{result_references}','[]') WHERE task_id=$1",
            "claim" => "UPDATE runtime_read_continuations SET claim=jsonb_set(claim,'{waiting_node}','\"choose_two\"') WHERE task_id=$1",
            "receipt" => "UPDATE runtime_read_continuations SET receipt=jsonb_set(receipt,'{prior_receipt_hash}',to_jsonb(repeat('0',64))) WHERE task_id=$1",
            "audit" => "DELETE FROM audit_events WHERE task_id=$1 AND details ? 'runtime_read_continuation_execution'",
            _ => "INSERT INTO runtime_read_continuations(task_id,source_revision,claim_id,contract_version,claim,status,receipt) SELECT task_id,source_revision+100,claim_id || '_extra',contract_version,claim,status,receipt FROM runtime_read_continuations WHERE task_id=$1",
        };
        sqlx::query(sql)
            .bind(first.task.task_id.as_str())
            .execute(&db.scoped)
            .await
            .unwrap();
        assert!(service(&db, &rig)
            .recover(
                &session(),
                &second.task.task_id,
                RuntimeTaskVersion::of(&second.task)
            )
            .await
            .is_err());
        assert!(service(&db, &rig)
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        assert_eq!(rig.calls(), 1);
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn task_lock_wait_rechecks_revoked_or_rebound_session_before_claim() {
    let db = Db::new().await;
    for rebound in [false, true] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let rig = Rig::new(sessions.clone(), Options::default());
        let continuation = service(&db, &rig);
        let mut tx = db.scoped.begin().await.unwrap();
        sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
            .bind(first.task.task_id.as_str())
            .execute(&mut *tx)
            .await
            .unwrap();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(async { continuation.continue_task(&session(), &first.task.task_id, &node("choose"), RuntimeTaskVersion::of(&first.task)).await }, async {
                loop {
                    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE%'").fetch_one(&db.admin).await.unwrap();
                    if waiting > 0 { break; }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                sessions.set(None);
                if rebound { sessions.set(Some("user_001")); }
                tx.commit().await.unwrap();
            })
        }).await.unwrap();
        assert!(result.is_err());
        assert_eq!(rig.calls(), 0);
        sessions.set(Some("user_001"));
        assert_eq!(
            db.raw(sessions)
                .recover(&session(), &first.task.task_id)
                .await
                .unwrap()
                .task,
            first.task
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM runtime_read_continuations WHERE task_id=$1"
            )
            .bind(first.task.task_id.as_str())
            .fetch_one(&db.scoped)
            .await
            .unwrap(),
            0
        );
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn postcommit_reread_detects_deferred_audit_mutation_for_claim_and_settlement() {
    let db = Db::new().await;
    for claim_phase in [true, false] {
        let sessions = Sessions::new();
        let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
        let rig = Rig::new(sessions, Options::default());
        db.scoped.execute(if claim_phase {
            "CREATE FUNCTION mutate_segment_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'runtime_read_continuation_claim' THEN UPDATE audit_events SET details=jsonb_set(details,'{runtime_read_continuation_claim,id}','\"forged\"') WHERE sequence=NEW.sequence; END IF; RETURN NEW; END $$"
        } else {
            "CREATE FUNCTION mutate_segment_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'runtime_read_continuation_execution' THEN UPDATE audit_events SET details=jsonb_set(details,'{runtime_read_continuation_execution,claim_id}','\"forged\"') WHERE sequence=NEW.sequence; END IF; RETURN NEW; END $$"
        }).await.unwrap();
        db.scoped.execute("CREATE CONSTRAINT TRIGGER mutate_segment_audit AFTER INSERT ON audit_events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mutate_segment_audit()").await.unwrap();
        assert!(service(&db, &rig)
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        assert_eq!(rig.calls(), usize::from(!claim_phase));
        let stored = db
            .raw(Sessions::new())
            .recover(&session(), &first.task.task_id)
            .await
            .unwrap()
            .task;
        assert_eq!(
            stored.state,
            if claim_phase {
                TaskState::Running
            } else {
                TaskState::WaitingUser
            }
        );
        assert!(service(&db, &rig)
            .recover(&session(), &stored.task_id, RuntimeTaskVersion::of(&stored))
            .await
            .is_err());
        assert!(service(&db, &rig)
            .continue_task(
                &session(),
                &first.task.task_id,
                &node("choose"),
                RuntimeTaskVersion::of(&first.task)
            )
            .await
            .is_err());
        assert_eq!(rig.calls(), usize::from(!claim_phase));
        db.scoped
            .execute("DROP TRIGGER mutate_segment_audit ON audit_events")
            .await
            .unwrap();
        db.scoped
            .execute("DROP FUNCTION mutate_segment_audit()")
            .await
            .unwrap();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16"]
async fn current_policy_and_registration_denial_and_unavailable_database_have_no_new_calls() {
    let db = Db::new().await;
    let sessions = Sessions::new();
    let first = waiting(&db, &Rig::new(sessions.clone(), Options::default())).await;
    for options in [
        Options {
            schema: Some(2),
            ..Options::default()
        },
        Options {
            side_effect: true,
            ..Options::default()
        },
        Options {
            policy_version: Some("balance-policy-v2"),
            ..Options::default()
        },
        Options {
            empty: true,
            ..Options::default()
        },
    ] {
        let rig = Rig::new(sessions.clone(), options);
        if let Ok(continuation) = RuntimeTaskContinuationService::for_trusted_backend(
            db.repo.clone(),
            rig.runtime(),
            contracts(),
        ) {
            assert!(continuation
                .continue_task(
                    &session(),
                    &first.task.task_id,
                    &node("choose"),
                    RuntimeTaskVersion::of(&first.task)
                )
                .await
                .is_err());
        }
        assert_eq!(rig.calls(), 0);
    }
    let rig = Rig::new(sessions, Options::default());
    rig.policy.deny.store(true, Ordering::SeqCst);
    assert!(service(&db, &rig)
        .continue_task(
            &session(),
            &first.task.task_id,
            &node("choose"),
            RuntimeTaskVersion::of(&first.task)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    db.repo.close().await;
    assert!(service(&db, &rig)
        .continue_task(
            &session(),
            &first.task.task_id,
            &node("choose"),
            RuntimeTaskVersion::of(&first.task)
        )
        .await
        .is_err());
    assert_eq!(rig.calls(), 0);
    db.finish().await;
}
