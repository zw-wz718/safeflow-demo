use super::*;
use safeflow_storage::{BillingRecoveryConfig, BillingRecoveryNonce};
use std::future::Future;

pub(in crate::http) enum Error {
    Early(ApiError),
    Recovery(StatusCode, &'static str),
}
impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Self::Early(e)
    }
}
impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        let (status, code) = match e {
            StorageError::BillingRecovery(code) => (
                match code {
                    "DISABLED" => StatusCode::NOT_IMPLEMENTED,
                    "PREPARATION_CLOSED" => StatusCode::GONE,
                    "PREPARATION_CAPACITY" => StatusCode::TOO_MANY_REQUESTS,
                    _ => StatusCode::CONFLICT,
                },
                code,
            ),
            StorageError::Session => (StatusCode::UNAUTHORIZED, "SESSION_UNAVAILABLE"),
            StorageError::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND_OR_NOT_OWNED"),
            StorageError::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
            StorageError::ReconcileRequired
            | StorageError::ConfirmationExpired
            | StorageError::ConfirmationRevoked
            | StorageError::ConfirmationRequired
            | StorageError::ConfirmationBindingMismatch => {
                (StatusCode::CONFLICT, "RECONCILE_REQUIRED")
            }
            _ => (StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE_OR_INTEGRITY"),
        };
        Self::Recovery(status, code)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Self::Early(e) => e.into_response(),
            Self::Recovery(status, code) => {
                let mut r =
                    (status, Json(json!({"contract_version":2,"code":code}))).into_response();
                r.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                r.headers_mut().insert(
                    "x-content-type-options",
                    HeaderValue::from_static("nosniff"),
                );
                r
            }
        }
    }
}
fn config(state: &ApiState) -> Result<&BillingRecoveryConfig, Error> {
    state
        .0
        .billing_feedback
        .as_ref()
        .and_then(|c| c.recovery.as_ref())
        .ok_or(Error::Recovery(StatusCode::NOT_IMPLEMENTED, "DISABLED"))
}
fn nonce(value: &str) -> Result<BillingRecoveryNonce, Error> {
    BillingRecoveryNonce::parse(value).map_err(Into::into)
}
fn version(value: u16) -> Result<(), Error> {
    if value == 2 {
        Ok(())
    } else {
        Err(Error::Recovery(StatusCode::BAD_REQUEST, "INVALID_REQUEST"))
    }
}
fn realm_query(uri: &Uri) -> Result<String, Error> {
    let value = uri
        .query()
        .and_then(|q| q.strip_prefix("recovery_realm="))
        .ok_or(Error::Recovery(StatusCode::BAD_REQUEST, "INVALID_REQUEST"))?;
    BillingRecoveryNonce::parse(value)?;
    Ok(value.into())
}
async fn empty(request: Request) -> Result<(), Error> {
    if !to_bytes(request.into_body(), MAX_BODY)
        .await
        .map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?
        .is_empty()
    {
        return Err(Error::Recovery(StatusCode::BAD_REQUEST, "INVALID_REQUEST"));
    }
    Ok(())
}
async fn timed(
    state: &ApiState,
    write: bool,
    future: impl Future<Output = Result<Value, StorageError>>,
) -> Result<Value, Error> {
    tokio::time::timeout(state.0.request_timeout, future)
        .await
        .map_err(|_| {
            Error::Recovery(
                if write {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                },
                if write {
                    "RECONCILE_REQUIRED"
                } else {
                    "UNAVAILABLE_OR_INTEGRITY"
                },
            )
        })?
        .map_err(Into::into)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTransaction {
    source_id: String,
    account_id: String,
    transaction_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    input_version: u16,
    report_id: String,
    transaction: WireTransaction,
    choice: FeedbackChoice,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    contract_version: u16,
    recovery_realm: String,
    nonce: String,
    intent: Intent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    contract_version: u16,
    recovery_realm: String,
    task_id: TaskId,
    operation_id: OperationId,
    confirmation_ref: ConfirmationRef,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Abandon {
    contract_version: u16,
    recovery_realm: String,
}

pub(in crate::http) async fn context(
    State(state): State<ApiState>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, Error> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    empty(request).await?;
    let result = if let Ok(config) = config(&state) {
        let service = service(&state)?;
        timed(
            &state,
            false,
            service.recovery_context_v2(&identity.reference, config),
        )
        .await?
    } else {
        json!({"contract_version":2,"scope":SCOPE,"recovery_realm":null,"available":false})
    };
    identity.check(&state)?;
    Ok(Json(result))
}
pub(in crate::http) async fn prepare(
    State(state): State<ApiState>,
    uri: Uri,
    request: Request,
) -> Result<(StatusCode, Json<Value>), Error> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    let input: Prepare = body(request).await?;
    version(input.contract_version)?;
    if input.intent.input_version != 1 {
        return Err(Error::Recovery(StatusCode::BAD_REQUEST, "INVALID_REQUEST"));
    }
    let nonce = nonce(&input.nonce)?;
    let config = config(&state)?;
    let feedback = service(&state)?;
    let intent = FeedbackIntent {
        report_id: input.intent.report_id,
        transaction: SourceKey {
            source_id: input.intent.transaction.source_id,
            account_id: input.intent.transaction.account_id,
            transaction_id: input.intent.transaction.transaction_id,
        },
        choice: input.intent.choice,
    };
    let result = timed(
        &state,
        true,
        feedback.prepare_v2(
            &identity.reference,
            config,
            &input.recovery_realm,
            &nonce,
            intent,
        ),
    )
    .await?;
    identity.check(&state)?;
    Ok((
        if result["kind"] == "CREATED" {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(result),
    ))
}
pub(in crate::http) async fn read(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, Error> {
    let identity = state.identity(request.headers())?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let nonce = nonce(&value)?;
    let realm = realm_query(&uri)?;
    empty(request).await?;
    let feedback = service(&state)?;
    let result = timed(
        &state,
        false,
        feedback.read_preparation_v2(&identity.reference, config(&state)?, &realm, &nonce),
    )
    .await?;
    identity.check(&state)?;
    Ok(Json(result))
}
pub(in crate::http) async fn read_operation(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, Error> {
    let identity = state.identity(request.headers())?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let operation = operation(&value)?;
    let realm = realm_query(&uri)?;
    empty(request).await?;
    let feedback = service(&state)?;
    let result = timed(
        &state,
        false,
        feedback.read_operation_preparation_v2(
            &identity.reference,
            config(&state)?,
            &realm,
            &operation,
        ),
    )
    .await?;
    identity.check(&state)?;
    Ok(Json(result))
}
async fn action(
    state: ApiState,
    path: Result<Path<String>, PathRejection>,
    uri: Uri,
    request: Request,
    execute: bool,
) -> Result<Json<Value>, Error> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let nonce = nonce(&value)?;
    let input: Action = body(request).await?;
    version(input.contract_version)?;
    let config = config(&state)?;
    let feedback = service(&state)?;
    let result = if execute {
        timed(
            &state,
            true,
            feedback.execute_preparation_v2(
                &identity.reference,
                config,
                &input.recovery_realm,
                &nonce,
                &input.task_id,
                &input.operation_id,
                &input.confirmation_ref,
            ),
        )
        .await?
    } else {
        timed(
            &state,
            true,
            feedback.confirm_preparation_v2(
                &identity.reference,
                config,
                &input.recovery_realm,
                &nonce,
                &input.task_id,
                &input.operation_id,
                &input.confirmation_ref,
            ),
        )
        .await?
    };
    identity.check(&state)?;
    Ok(Json(result))
}
pub(in crate::http) async fn confirm(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, Error> {
    action(state, path, uri, request, false).await
}
pub(in crate::http) async fn execute(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, Error> {
    action(state, path, uri, request, true).await
}
pub(in crate::http) async fn abandon(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, Error> {
    let identity = state.identity(request.headers())?;
    plain(&uri)?;
    let Path(value) = path.map_err(|_| ApiError::validation(StatusCode::BAD_REQUEST))?;
    let nonce = nonce(&value)?;
    let input: Abandon = body(request).await?;
    version(input.contract_version)?;
    let feedback = service(&state)?;
    let result = timed(
        &state,
        true,
        feedback.abandon_preparation_v2(
            &identity.reference,
            config(&state)?,
            &input.recovery_realm,
            &nonce,
        ),
    )
    .await?;
    identity.check(&state)?;
    Ok(Json(result))
}
