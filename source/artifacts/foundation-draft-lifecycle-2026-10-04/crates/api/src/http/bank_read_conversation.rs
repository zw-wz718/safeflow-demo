//! Independent, bounded read journal. It never binds or changes a money task.
use super::*;
use safeflow_contracts::ResourceId;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Mutex, time::Instant};

#[derive(Default)]
pub(super) struct Conversations(Mutex<HashMap<String, Arc<tokio::sync::Mutex<Turn>>>>);
struct Turn {
    session: SessionRef,
    binding: SessionBinding,
    created: Instant,
    revision: u64,
    history: Vec<String>,
    task: Option<TaskId>,
    response: Option<Value>,
    uncertain: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    conversation_id: String,
    revision: u64,
    message: String,
    account_id: Option<ResourceId>,
}
fn valid_id(id: &str) -> Result<(), ApiError> {
    BillingRecoveryNonce::parse(id).map(|_| ()).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
fn project(id: &str, turn: &Turn) -> Value {
    json!({"conversation_id":id,"revision":turn.revision,"view":turn.response,"uncertain":turn.uncertain})
}
fn planning_prompt(history: &[String]) -> Result<String, ApiError> {
    let prompt = json!({"untrusted_conversation":history,"instruction":"Understand the latest independent read request, using previous clarification only. This conversation cannot create, modify, cancel, confirm, authorize or execute any business action. If asked to write, CLARIFY that original money tasks remain unchanged and must be checked separately. Never request codes or full card numbers."}).to_string();
    if prompt.len() > 4096 { return Err(ApiError::validation(StatusCode::BAD_REQUEST)); }
    Ok(prompt)
}
pub(super) async fn message(State(state): State<ApiState>, request: Request) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: Input = payment_preparation::input(request).await?;
    valid_id(&input.conversation_id)?;
    if state.0.unified_source.is_none() || state.0.agent_fixed_plan_enabled { return Err(ApiError::disabled()); }
    if state.0.llm_gateway.is_none() { return Err(ApiError {status: StatusCode::SERVICE_UNAVAILABLE, reason:"LLM_NOT_CONFIGURED",task_id:None}); }
    if input.message.trim().is_empty() || input.message.len() > 4096 { return Err(ApiError::validation(StatusCode::BAD_REQUEST)); }
    // Reject pure input errors before reserving any global journal capacity.
    // An existing turn's complete history is validated again under its lock.
    planning_prompt(std::slice::from_ref(&input.message))?;
    let next_revision = input.revision.checked_add(1).ok_or_else(|| ApiError::conflict())?;
    let id = TaskId::parse(format!("bank_read_{}_{}",input.conversation_id,next_revision)).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let cell = {
        let mut map = state.0.read_conversations.0.lock().map_err(|_| ApiError::unavailable())?;
        map.retain(|_, c| c.try_lock().map_or(true, |t| t.created.elapsed() < Duration::from_secs(86400)));
        if !map.contains_key(&input.conversation_id) {
            if input.revision != 0 || map.len() >= 256 { return Err(ApiError::conflict()); }
            map.insert(input.conversation_id.clone(), Arc::new(tokio::sync::Mutex::new(Turn {
                session:identity.reference.clone(), binding:identity.binding.clone(), created:Instant::now(),
                revision:0,history:vec![],task:None,response:None,uncertain:false,
            })));
        }
        map[&input.conversation_id].clone()
    };
    let mut turn = cell.try_lock().map_err(|_| ApiError::conflict())?;
    if turn.session != identity.reference || turn.binding != identity.binding { return Err(ApiError::forbidden()); }
    if turn.revision != input.revision || turn.uncertain || turn.revision >= 12 { return Err(ApiError::conflict()); }
    let mut history = turn.history.clone();
    history.push(input.message);
    let prompt = planning_prompt(&history)?;
    turn.revision += 1;
    turn.uncertain = true;
    turn.response = None;
    turn.task = Some(id.clone());
    let deadline = state.0.llm_gateway.as_ref().ok_or_else(ApiError::unavailable)?.timeout() + state.0.request_timeout * 2;
    let result = tokio::time::timeout(deadline,agent::independent_read(&state,&identity,prompt,input.account_id,id))
        .await.map_err(|_|ApiError::unavailable())?;
    identity.check(&state)?;
    match result {
        Ok(view) => {
            turn.response = Some(serde_json::to_value(view).map_err(|_| ApiError::unavailable())?);
            turn.history = history;
            turn.uncertain = false;
            Ok(Json(project(&input.conversation_id,&turn)))
        }
        Err(error) => Err(error), // Client may GET this journal, never automatically re-POST.
    }
}
pub(super) async fn query(State(state): State<ApiState>, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(&headers)?;
    valid_id(&id)?;
    let cell = state.0.read_conversations.0.lock().map_err(|_|ApiError::unavailable())?.get(&id).cloned().ok_or_else(ApiError::not_found)?;
    let mut turn = cell.try_lock().map_err(|_|ApiError::conflict())?;
    if turn.session != identity.reference || turn.binding != identity.binding { return Err(ApiError::forbidden()); }
    if turn.created.elapsed() >= Duration::from_secs(86400) { return Err(ApiError::conflict()); }
    // Re-read a persisted result with the fixed READ registry and provenance.
    // An incomplete/failed query never schedules or continues a task on GET.
    if let Some(task) = &turn.task {
        if turn.response.as_ref().is_none_or(|v|v["kind"] == "READ") {
            let had_result = turn.response.is_some();
            let result = tokio::time::timeout(state.0.request_timeout,agent::independent_read_view(&state,&identity,task)).await;
            match result {
                Ok(Ok(view)) => {
                    turn.response=Some(serde_json::to_value(view).map_err(|_|ApiError::unavailable())?);
                    turn.uncertain=false;
                }
                Ok(Err(error)) if had_result => return Err(error),
                Err(_) if had_result => return Err(ApiError::unavailable()),
                _ => {}, // No saved task after a failed plan: retain uncertain, no re-POST.
            }
        }
    }
    identity.check(&state)?;
    Ok(Json(project(&id,&turn)))
}
