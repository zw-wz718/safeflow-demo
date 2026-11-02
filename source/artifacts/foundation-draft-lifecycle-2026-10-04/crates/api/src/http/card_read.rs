//! Explicit user read actions. Same cookie, origin, CSRF and timeout perimeter;
//! navigation POSTs retain only volatile read objects, never business commands.
use super::*;
use safeflow_contracts::ResourceId;
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
                    "card-read-context:{}:{}",
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
enum ReadRequest {
    QueryFeedback {
        feedback_operation_id: safeflow_contracts::OperationId,
        request_ref: String,
    },
    Query {
        account_id: ResourceId,
        request_ref: String,
    },
    SelectCard {
        step_ref: String,
        position: usize,
        request_ref: String,
    },
    SelectRecord {
        step_ref: String,
        position: usize,
        request_ref: String,
    },
    Cancel {
        step_ref: String,
    },
    CancelRequest {
        request_ref: String,
    },
}
fn reference(value: &str) -> Result<(), ApiError> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    Ok(())
}
pub(super) async fn read(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    let input: ReadRequest = body(request).await?;
    identity.check(&state)?;
    let value = tokio::time::timeout(state.0.request_timeout, async {
        match input {
            ReadRequest::QueryFeedback {
                feedback_operation_id,
                request_ref,
            } => {
                reference(&request_ref)?;
                let feedback = billing_feedback::service(&state)?;
                state
                    .0
                    .card_read
                    .query_feedback(
                        &identity.reference,
                        &feedback,
                        &feedback_operation_id,
                        &request_ref,
                    )
                    .await
                    .map_err(ApiError::storage)
            }
            ReadRequest::Query {
                account_id,
                request_ref,
            } => {
                reference(&request_ref)?;
                state
                    .0
                    .card_read
                    .query(&identity.reference, &account_id, &request_ref)
                    .await
                    .map_err(ApiError::storage)
            }
            ReadRequest::SelectCard {
                step_ref,
                position,
                request_ref,
            } => {
                reference(&step_ref)?;
                reference(&request_ref)?;
                state
                    .0
                    .card_read
                    .select_card(&identity.reference, &step_ref, position, &request_ref)
                    .await
                    .map_err(ApiError::storage)
            }
            ReadRequest::SelectRecord {
                step_ref,
                position,
                request_ref,
            } => {
                reference(&step_ref)?;
                reference(&request_ref)?;
                if state
                    .0
                    .card_read
                    .is_feedback_step(&identity.reference, &step_ref)
                    .map_err(ApiError::storage)?
                {
                    let feedback = billing_feedback::service(&state)?;
                    return state
                        .0
                        .card_read
                        .select_record_with_feedback(
                            &identity.reference,
                            &feedback,
                            &step_ref,
                            position,
                            &request_ref,
                        )
                        .await
                        .map_err(ApiError::storage);
                }
                state
                    .0
                    .card_read
                    .select_record(&identity.reference, &step_ref, position, &request_ref)
                    .await
                    .map_err(ApiError::storage)
            }
            ReadRequest::CancelRequest { request_ref } => {
                reference(&request_ref)?;
                state
                    .0
                    .card_read
                    .cancel_request(&identity.reference, &request_ref)
                    .map_err(ApiError::storage)?;
                Ok(serde_json::json!({"cancelled":true}))
            }
            ReadRequest::Cancel { step_ref } => {
                reference(&step_ref)?;
                state
                    .0
                    .card_read
                    .cancel(&identity.reference, &step_ref)
                    .map_err(ApiError::storage)?;
                Ok(serde_json::json!({"cancelled":true}))
            }
        }
    })
    .await
    .map_err(|_| ApiError::unavailable())??;
    identity.check(&state)?;
    Ok(Json(value))
}
