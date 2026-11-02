//! Explicit owned-PG stage fixture via normal persistence methods, never raw rows.
use safeflow_contracts::{AgentPlan, NodeId, OperationId, PlanNode, SessionRef, TaskId, ToolName, UserId};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{PersistenceService, PgRepository};
use serde_json::Value;
use std::sync::Arc;

#[tokio::test]
#[ignore = "requires a freshly provisioned owned PG and explicit nonsecret stage input"]
async fn create_revision_draft_stage_via_original_persistence() {
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("owned PG URL required");
    let namespace = std::env::var("SAFEFLOW_TEST_NAMESPACE").expect("owned namespace required");
    assert!(namespace.starts_with("sf_unified_noon_"));
    let path = std::env::var("SAFEFLOW_STAGE_INPUT").expect("explicit stage input required");
    let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let task = TaskId::parse(value["task_id"].as_str().unwrap()).unwrap();
    let op = OperationId::parse(value["operation_id"].as_str().unwrap()).unwrap();
    let parameters = value["parameters"].clone();
    let review = NodeId::parse("review_feedback").unwrap();
    let save = NodeId::parse("save_feedback").unwrap();
    let plan = AgentPlan::new(task, 1, vec![
        PlanNode::UserDecision { id: review.clone(), depends_on: vec![] },
        PlanNode::Tool { id: save.clone(), tool: ToolName::parse("revise_billing_feedback").unwrap(),
            arguments: parameters, depends_on: vec![review] },
    ]).unwrap();
    let session = SessionRef::parse("owned_revision_stage_session").unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001").unwrap());
    let repo = PgRepository::connect_existing(&url, &namespace).await.unwrap();
    let persistence = PersistenceService::for_trusted_backend(repo, Arc::new(sessions));
    let record = persistence.create_task(&session, plan, 1).await.unwrap();
    persistence.create_operation(&session, &record.task_id, record.revision, op,
        save, 1).await.unwrap();
    let recovered = persistence.recover(&session, &record.task_id).await.unwrap();
    assert_eq!(recovered.operations.len(), 1);
    assert_eq!(format!("{:?}", recovered.operations[0].state), "Draft");
    assert!(recovered.task.completed_nodes.is_empty());
}
