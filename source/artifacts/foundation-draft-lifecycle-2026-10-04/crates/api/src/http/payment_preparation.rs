use super::*;
use safeflow_contracts::{ConfirmationRef, OperationId};
use safeflow_storage::{
    PaymentExecutionLink, PaymentOriginalReader, PaymentOriginalResult, PaymentPreparationRequest,
    PaymentPreparationService, PAYMENT_PREPARATION_SCOPE,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    contract_version: u16,
    scope: String,
    source: PaymentPreparationRequest,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    contract_version: u16,
    scope: String,
    confirmation_ref: ConfirmationRef,
    display_ticket: String,
    action: Action,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    contract_version: u16,
    scope: String,
    confirmation_ref: ConfirmationRef,
    display_ticket: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Action {
    Confirm,
    Reject,
}

fn scope(version: u16, scope: &str) -> Result<(), ApiError> {
    if version != 1 || scope != PAYMENT_PREPARATION_SCOPE {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    Ok(())
}
pub(super) fn service(state: &ApiState) -> Result<PaymentPreparationService<'_>, ApiError> {
    let draft = state.0.draft.as_ref().ok_or_else(ApiError::disabled)?;
    Ok(PaymentPreparationService::for_trusted_backend(
        state.0.repository.clone(),
        state.0.sessions.clone(),
        draft.config.confirmation.clone(),
        draft
            .lifecycle_runtime
            .as_ref()
            .ok_or_else(ApiError::disabled)?
            .runtime(),
        draft
            .config
            .lifecycle_decision
            .clone()
            .ok_or_else(ApiError::disabled)?,
    ))
}
pub(super) async fn input<T: DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    .map_err(|_| ApiError::validation(StatusCode::REQUEST_TIMEOUT))?
    .map_err(|_| ApiError::validation(StatusCode::PAYLOAD_TOO_LARGE))?;
    let value = safeflow_runtime::parse_bounded_input_json_for_backend(&bytes)
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    serde_json::from_value(value).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
pub(super) fn operation(value: &str) -> Result<OperationId, ApiError> {
    parse_task(value)?;
    OperationId::parse(value).map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))
}
pub(super) fn error(error: StorageError) -> ApiError {
    match error {
        StorageError::ExecutionKeyConflict
        | StorageError::ExecutionOperationConflict
        | StorageError::ConfirmationRequired
        | StorageError::ReconcileRequired => ApiError::conflict(),
        other => draft_confirmation::error(other),
    }
}

pub(super) async fn prepare(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<super::payment_execution::PaymentView>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path(id) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let body: Prepare = input(request).await?;
    scope(body.contract_version, &body.scope)?;
    if body.source.task_revision > MAX_SAFE_INTEGER || body.source.facts_version > u32::MAX as u64 {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service(&state)?.prepare(&identity.reference, &id, body.source),
    )
    .await;
    identity.check(&state)?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(|p| Json(super::payment_execution::project(&state, p)))
        .map_err(|e| error(e).with_task(&id))
}

pub(super) async fn query(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<super::payment_execution::PaymentView>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path((id, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let op = operation(&op)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service(&state)?.query(&identity.reference, &id, &op),
    )
    .await;
    identity.check(&state)?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(|p| Json(super::payment_execution::project(&state, p)))
        .map_err(|e| error(e).with_task(&id))
}

pub(super) async fn query_task(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<Vec<super::payment_execution::PaymentView>>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path(id) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service(&state)?.query_task(&identity.reference, &id),
    )
    .await;
    identity.check(&state)?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(|ps| {
            Json(
                ps.into_iter()
                    .map(|p| super::payment_execution::project(&state, p))
                    .collect(),
            )
        })
        .map_err(|e| error(e).with_task(&id))
}

pub(super) async fn original_result(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<PaymentOriginalResult>, ApiError> {
    let identity = state.identity(&headers)?;
    let Path((id, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let op = operation(&op)?;
    if uri.query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let result = tokio::time::timeout(
        state.0.request_timeout,
        PaymentOriginalReader::for_trusted_backend(
            state.0.repository.clone(),
            state.0.sessions.clone(),
        )
        .query(&identity.reference, &id, &op),
    )
    .await;
    identity.check(&state)?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| error(e).with_task(&id))
}

pub(super) async fn decide(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<super::payment_execution::PaymentView>, ApiError> {
    decide_inner(state, path, request, false).await
}

pub(super) async fn register_execution(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<PaymentExecutionLink>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path((id, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let op = operation(&op)?;
    if request.uri().query().is_some() {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    let body: Registration = input(request).await?;
    if body.contract_version != 1 || body.scope != "PAYMENT_REGISTRATION_ONLY" {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    let result = tokio::time::timeout(
        state.0.request_timeout,
        service(&state)?.register_execution(
            &identity.reference,
            &id,
            &op,
            &body.confirmation_ref,
            &body.display_ticket,
        ),
    )
    .await;
    identity.check(&state)?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(Json)
        .map_err(|e| error(e).with_task(&id))
}

pub(super) async fn evaluate(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
) -> Result<Json<super::payment_execution::PaymentView>, ApiError> {
    decide_inner(state, path, request, true).await
}

async fn decide_inner(
    state: ApiState,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
    refresh: bool,
) -> Result<Json<super::payment_execution::PaymentView>, ApiError> {
    let identity = state.identity(request.headers())?;
    let Path((id, op)) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let id = parse_task(&id)?;
    let op = operation(&op)?;
    let body: Decision = input(request).await?;
    scope(body.contract_version, &body.scope)?;
    if refresh && !matches!(body.action, Action::Confirm) {
        return Err(ApiError::validation(StatusCode::BAD_REQUEST));
    }
    identity.check(&state)?;
    let service = service(&state)?;
    let action = async {
        if refresh {
            service
                .evaluate(
                    &identity.reference,
                    &id,
                    &op,
                    &body.confirmation_ref,
                    &body.display_ticket,
                )
                .await
        } else {
            service
                .decide(
                    &identity.reference,
                    &id,
                    &op,
                    &body.confirmation_ref,
                    &body.display_ticket,
                    matches!(body.action, Action::Confirm),
                )
                .await
        }
    };
    let result = tokio::time::timeout(state.0.request_timeout, action).await;
    identity.check(&state)?;
    result
        .map_err(|_| ApiError::unavailable().with_task(&id))?
        .map(|p| Json(super::payment_execution::project(&state, p)))
        .map_err(|e| error(e).with_task(&id))
}
