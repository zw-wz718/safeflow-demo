//! Explicit owner reads. No Tool registration, client Snapshot or write grant.
use super::*;
pub(super) async fn session(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    use sha2::{Digest, Sha256};
    let context = state.identity(&headers).ok().map(|id| {
        format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "subscription-read-context:{}:{}",
                    id.reference, id.binding.binding_version
                )
                .as_bytes()
            )
        )
    });
    Json(serde_json::json!({"authenticated":context.is_some(),"context_ref":context}))
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum Read {
    List {},
    Original { original_ref: String },
    Details { selection_ref: String },
    ReminderAttempts {},
    ReminderAttempt { attempt_ref: String },
}
pub(super) async fn read(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: Read = body(request).await?;
    identity.check(&state)?;
    let result = tokio::time::timeout(state.0.request_timeout, async {
        match input {
            Read::ReminderAttempts {}=>state.persistent_subscription_reminder_view_for_backend(&identity.reference,None).await,
            Read::ReminderAttempt {attempt_ref}=>state.persistent_subscription_reminder_view_for_backend(&identity.reference,Some(&attempt_ref)).await,
            Read::List {} => if state.0.subscription_public_scope.is_some() {
                state.team_public_writes_for_backend()?.list(&identity.reference).await
            } else {state.0.subscription.read(&identity.reference).await},
            Read::Details { selection_ref } => {
                if state.0.subscription_public_scope.is_some(){
                    return state.team_public_writes_for_backend()?.details(&identity.reference,&selection_ref).await;
                }
                state
                    .0
                    .subscription
                    .details(&identity.reference, &selection_ref)
                    .await
            }
            Read::Original { original_ref } => {
                if state.0.subscription_public_scope.is_some(){
                    return state.team_public_writes_for_backend()?.original(&identity.reference,&original_ref).await;
                }
                state
                    .0
                    .subscription
                    .original(&identity.reference, &original_ref)
                    .await
            }
        }
    })
    .await
    .map_err(|_| ApiError::unavailable())?
    .map_err(ApiError::storage)?;
    identity.check(&state)?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum Change {
    RecordCancellation {},
    RevokeCancellation {},
    ReminderSettings {},
    Registration {},
    Capture {},
    Prepare { nonce: String, request: SubscriptionPublicRequest },
    Confirm { task_id: TaskId, confirmation_ref: safeflow_contracts::ConfirmationRef, explicitly_confirm: bool },
    Execute { task_id: TaskId, operation_id: safeflow_contracts::OperationId, confirmation_ref: safeflow_contracts::ConfirmationRef },
    QueryNonce { nonce: String },
    Query { operation_id: safeflow_contracts::OperationId },
}
pub(super) async fn change(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: Change = body(request).await?;
    identity.check(&state)?;
    let result=tokio::time::timeout(state.0.request_timeout,async {
        let writes=state.team_public_writes_for_backend()?;
        let session=&identity.reference;
        match input {
            Change::Capture{}=>writes.capture(session).await,
            Change::Prepare{nonce,request}=>{
                let prepared=writes.prepare(session,&nonce,request).await?;
                state.team_public_writes_for_backend()?.preparation_view(session,&prepared).await
            }
            Change::Confirm{task_id,confirmation_ref,explicitly_confirm:true}=>{
                writes.confirm(session,&task_id,&confirmation_ref).await?;
                Ok(serde_json::json!({"confirmed":true,"business_executed":false,"mode":"PROJECT_SIMULATION"}))
            }
            Change::Execute{task_id,operation_id,confirmation_ref}=>writes.execute(session,&task_id,&operation_id,&confirmation_ref).await,
            Change::QueryNonce{nonce}=>writes.query_nonce(session,&nonce).await,
            Change::Query{operation_id}=>writes.query(session,&operation_id).await,
            _=>Err(StorageError::DependencyUnavailable),
        }
    }).await.map_err(|_|ApiError::unavailable())?.map_err(ApiError::storage)?;
    identity.check(&state)?;
    Ok(Json(result))
}
