//! Bounded session journal for clarification only. Business tasks stay in the
//! existing PostgreSQL services; this journal grants no execution authority.
use super::*;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Mutex, time::Instant};

#[derive(Default)]
pub(super) struct Conversations(Mutex<HashMap<String, Arc<tokio::sync::Mutex<Turn>>>>);
struct Turn {
    session: SessionRef,
    binding: SessionBinding,
    revision: u64,
    created: Instant,
    history: Vec<String>,
    task: Option<TaskId>,
    response: Option<Value>,
    uncertain: bool,
    payee_query: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    conversation_id: String,
    revision: u64,
    message: String,
    selection: Option<agent::Selection>,
    context: Option<agent::Context>,
}
fn valid_id(id: &str) -> Result<(), ApiError> {
    BillingRecoveryNonce::parse(id)
        .map(|_| ())
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
pub(super) async fn message(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: Input = payment_preparation::input(request).await?;
    valid_id(&input.conversation_id)?;
    if state.0.unified_source.is_none()
        || state.0.bank_planning_read_only
        || state.0.agent_fixed_plan_enabled
    {
        return Err(ApiError::disabled());
    }
    if input.message.trim().is_empty() || input.message.len() > 4096 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let gateway = state
        .0
        .llm_gateway
        .clone()
        .ok_or_else(ApiError::unavailable)?;
    let cell = {
        let mut map = state
            .0
            .conversations
            .0
            .lock()
            .map_err(|_| ApiError::unavailable())?;
        // Retain active requests. Session TTL is independently checked on every access.
        map.retain(|_, cell| {
            cell.try_lock()
                .map_or(true, |t| t.created.elapsed() < Duration::from_secs(86400))
        });
        if !map.contains_key(&input.conversation_id) {
            if input.revision != 0 || map.len() >= 256 {
                return Err(ApiError::conflict());
            }
            map.insert(
                input.conversation_id.clone(),
                Arc::new(tokio::sync::Mutex::new(Turn {
                    session: identity.reference.clone(),
                    binding: identity.binding.clone(),
                    revision: 0,
                    created: Instant::now(),
                    history: vec![],
                    task: None,
                    response: None,
                    uncertain: false,
                    payee_query: None,
                })),
            );
        }
        map[&input.conversation_id].clone()
    };
    let mut turn = cell.try_lock().map_err(|_| ApiError::conflict())?;
    if turn.session != identity.reference
        || turn.binding != identity.binding
        || turn.revision != input.revision
        || turn.uncertain
    {
        return Err(ApiError::conflict());
    }
    if let Some(context) = &input.context {
        if turn.task.as_ref().is_some_and(|id| id != &context.task_id) {
            return Err(ApiError::conflict());
        }
    } else if turn.task.is_some() {
        return Err(ApiError::conflict());
    }
    let mut history = turn.history.clone();
    history.push(input.message);
    if history.len() > 12 || history.iter().map(String::len).sum::<usize>() > 12000 {
        return Err(ApiError::conflict());
    }
    // The gateway receives bounded untrusted conversation data. It must still
    // produce a checked plan with the server's identifiers and current versions.
    let prompt = json!({"untrusted_conversation":history,"instruction":"Understand the latest request using previous clarification. None of these messages are confirmation or verification. Ask missing details rather than inventing them."}).to_string();
    if prompt.len() > 4096 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    turn.revision += 1;
    turn.uncertain = true; // Disconnection never authorizes an automatic re-POST.
    let allocated = TaskId::parse(format!(
        "bank_chat_{}_{}",
        input.conversation_id, turn.revision
    ))
    .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let target = input
        .context
        .as_ref()
        .map(|c| c.task_id.clone())
        .unwrap_or_else(|| allocated.clone());
    let bound_task = input
        .context
        .as_ref()
        .map(|context| context.task_id.clone());
    let result = agent::process_message(
        &state,
        &identity,
        agent::MessageRequest {
            message: prompt,
            selection: input.selection,
            context: input.context,
        },
        None,
        Some(gateway),
        Some(allocated),
    )
    .await;
    identity.check(&state)?;
    match result {
        Ok(Json(view)) => {
            turn.payee_query = view.payee_query.clone();
            let value = serde_json::to_value(view).map_err(|_| ApiError::unavailable())?;
            turn.task = value["draft"]["task_id"]
                .as_str()
                .or_else(|| value["task"]["task_id"].as_str())
                .map(parse_task)
                .transpose()?;
            // A clarification is not an abandoned draft. The original context
            // was checked by process_message; following turns must keep that task
            // and recheck its current versions through the existing agent path.
            match value["kind"].as_str() {
                Some("CLARIFY") => turn.task = bound_task,
                Some("READ") => turn.task = None,
                _ => {}
            }
            turn.history = history;
            turn.response = Some(value.clone());
            turn.uncertain = false;
            Ok(Json(
                json!({"conversation_id":input.conversation_id,"revision":turn.revision,"task_id":turn.task,"view":value,"uncertain":false,"payee_query":turn.payee_query}),
            ))
        }
        Err(error) => {
            turn.task = Some(target.clone());
            Err(error.with_task(&target))
        }
    }
}
pub(super) async fn query(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let identity = state.identity(&headers)?;
    valid_id(&id)?;
    let cell = state
        .0
        .conversations
        .0
        .lock()
        .map_err(|_| ApiError::unavailable())?
        .get(&id)
        .cloned()
        .ok_or_else(ApiError::conflict)?;
    let turn = cell.try_lock().map_err(|_| ApiError::conflict())?;
    if turn.session != identity.reference || turn.binding != identity.binding {
        return Err(ApiError::forbidden());
    }
    identity.check(&state)?;
    // Cached response is DISPLAY ONLY. The client GETs the task itself before
    // making any controls available. Restart loses clarifications, not tasks.
    Ok(Json(
        json!({"conversation_id":id,"revision":turn.revision,"task_id":turn.task,"view":turn.response,"uncertain":turn.uncertain,"payee_query":turn.payee_query}),
    ))
}
