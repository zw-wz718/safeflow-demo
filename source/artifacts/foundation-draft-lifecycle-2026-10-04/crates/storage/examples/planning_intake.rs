#[path = "support/planning_runtime.rs"]
mod support;
use safeflow_contracts::*;
use safeflow_runtime::*;
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("typed");
    if args.len() > 1 || !matches!(mode, "typed" | "raw" | "clarify" | "reject") {
        return Err("Use typed, raw, clarify or reject; local proposal intake only".into());
    }
    let task = TaskId::parse("task_intake_demo_001")?;
    let session = SessionRef::parse("intake_demo_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let (assembly, observed, audit) = support::assemble(Arc::new(sessions), None);
    let mut result = support::proposal(&task, 1);
    if mode == "clarify" {
        result.understanding.decision = UnderstandingDecision::Clarify;
        result.understanding.missing_fields = vec!["account_id".into()];
        result.plan = None;
        result.clarification = Some(Clarification {
            question: "查询哪一个账户？".into(),
        });
    }
    if mode == "reject" {
        if let PlanNode::Tool { tool, .. } = &mut result.plan.as_mut().unwrap().nodes[0] {
            *tool = ToolName::parse("execute_funds")?;
        }
    }
    let mock = Arc::new(MockLlm::new(result.clone()));
    let gateway = if mode == "raw" {
        None
    } else {
        Some(mock.clone() as Arc<dyn LlmGateway>)
    };
    let intake = PlanningIntakeService::for_trusted_backend(
        assembly.runtime(),
        gateway,
        support::contracts(),
    )?;
    let proposed = if mode == "raw" {
        intake
            .accept_json(&session, &task, 1, &serde_json::to_vec(&result)?)
            .await
    } else {
        intake.plan(&session, &task, 1, "查询我的账户余额").await
    };
    observed.assert_intake_only();
    assert!(audit.events()?.is_empty());
    let (status, error, plan_present) = match proposed {
        Ok(proposal) => {
            assert_ne!(mode, "reject");
            assert_eq!(proposal.task_id(), &task);
            assert_eq!(proposal.plan_version(), 1);
            (
                "PROPOSAL_ACCEPTED",
                None,
                proposal.planning_result().plan.is_some(),
            )
        }
        Err(error) => {
            if mode != "reject" {
                return Err(error.into());
            }
            assert_eq!(error, PlanningIntakeError::Registration);
            ("PROPOSAL_REJECTED", Some(error.to_string()), false)
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "intake_contract_version":1, "mode":mode, "status":status, "error":error,
            "task_id":task, "plan_version":1, "plan_present":plan_present,
            "gateway_calls":mock.call_count(), "tool_calls":observed.tool.load(Ordering::SeqCst),
            "policy_calls":observed.policy.load(Ordering::SeqCst), "bank_calls":observed.bank.load(Ordering::SeqCst),
            "audit_events":audit.events()?.len(), "database_task_created":false,
            "execution_authority_provided":false, "local_mock_only":true
        }))?
    );
    Ok(())
}
