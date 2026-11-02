//! A fixed native-configured read flow through existing checked Runtime.
use super::*;
use safeflow_contracts::{
    AgentPlan, PlanNode, PlanningResult, ToolName, Understanding, UnderstandingDecision,
};
use serde_json::json;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {}
pub(super) async fn subscriptions(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let _: Input = body(request).await?;
    identity.check(&state)?;
    let preview = state
        .team_subscription_preview_for_backend(&identity.reference)
        .await
        .map_err(ApiError::storage)?;
    identity.check(&state)?;
    Ok(Json(preview))
}
pub(super) async fn billing(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let _: Input = body(request).await?;
    identity.check(&state)?;
    let team = state
        .0
        .team_source
        .as_ref()
        .ok_or_else(ApiError::unavailable)?;
    if identity.binding.user_id.as_str() != team.owner() {
        return Err(ApiError::unauthenticated());
    }
    let source_request = team.request();
    let id = new_task_id();
    let proposal=PlanningResult::for_plan(Understanding{intent:"READ_FIXED_TEAM_HISTORY".into(),entities:json!({}),missing_fields:vec![],context_refs:vec![],decision:UnderstandingDecision::Pass},
        AgentPlan::new(id.clone(),1,vec![PlanNode::Tool{id:NodeId::parse("read_team_billing").unwrap(),tool:ToolName::parse(crate::BILLING_REPORT_TOOL).unwrap(),
            arguments:json!({"input_version":1,"account_ids_json":serde_json::to_string(&source_request.account_ids).unwrap(),"start_inclusive":source_request.window.start_inclusive,"end_exclusive":source_request.window.end_exclusive,"timezone":source_request.window.timezone}),depends_on:vec![]}]).map_err(|_|ApiError::unavailable())?);
    let value=tokio::time::timeout(state.0.request_timeout,async {
        let runtime=state.0.runtime.runtime();
        let intake=PlanningTaskCreationService::for_trusted_backend(state.0.repository.clone(),runtime,Some(Arc::new(MockLlm::new(proposal))),state.0.runtime.contracts()).map_err(|_|ApiError::unavailable())?;
        let creation=intake.plan_and_create(&identity.reference,&id,1,1,"读取已采用的固定演示消费历史").await.map_err(|_|ApiError::unavailable())?;
        identity.check(&state)?;
        let initial=match creation{PlanningTaskCreationOutcome::TaskCreated(v)=>v,_=>return Err(ApiError::unavailable())};
        let execution=RuntimeTaskExecutionService::for_trusted_backend(state.0.repository.clone(),runtime,state.0.runtime.contracts()).map_err(ApiError::execution)?
            .schedule_for_agent_backend(&identity.reference,&id,RuntimeTaskVersion::of(&initial)).await.map_err(ApiError::execution)?;
        identity.check(&state)?;
        let task=state.response(&identity,&id,false).await?;
        Ok::<_,ApiError>(json!({"task":task,"results":execution.results,"as_of":safeflow_storage::TEAM_AS_OF,"mode":"PROJECT_SIMULATION","fixed_demo_clock":true,"writes_enabled":false}))
    }).await.map_err(|_|ApiError::unavailable())??;
    identity.check(&state)?;
    Ok(Json(value))
}
