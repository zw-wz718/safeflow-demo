//! Explicit HTTPS deployment perimeter. Only the configured local TLS proxy is trusted.
use super::*;
use axum::extract::ConnectInfo;
use safeflow_contracts::OperationId;
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicDemoConfig {
    pub backend_authority: String,
    pub trusted_proxy: IpAddr,
}
impl PublicDemoConfig {
    pub(super) fn validate(&self, origin: &str) -> Result<(), ApiStartupError> {
        if self.trusted_proxy != IpAddr::from([127, 0, 0, 1])
            || !canonical_https_origin(origin)
            || !canonical_https_origin(&format!("https://{}", self.backend_authority))
        {
            return Err(ApiStartupError("exact public HTTPS origin, authority and loopback TLS proxy required"));
        }
        Ok(())
    }
    pub(super) fn accepts(&self, frontend_origin: &str, request: &Request) -> bool {
        let h = request.headers();
        let peer = request.extensions().get::<ConnectInfo<SocketAddr>>();
        let exact = |name: &str, value: &str| {
            h.get_all(name).iter().count() == 1
                && h.get(name).and_then(|v| v.to_str().ok()) == Some(value)
        };
        peer.is_some_and(|p| p.0.ip() == self.trusted_proxy)
            && exact("host", &self.backend_authority)
            && exact("x-forwarded-host", &self.backend_authority)
            && exact("x-forwarded-proto", "https")
            && h.get_all("forwarded").iter().count() == 0
            && h.get_all("origin").iter().count() <= 1
            && h.get("origin").map_or(
                request.method() == Method::GET || request.method() == Method::HEAD,
                |v| v.to_str().ok() == Some(frontend_origin),
            )
    }
}
fn canonical_https_origin(value: &str) -> bool {
    let Ok(u) = value.parse::<Uri>() else { return false; };
    let Some(authority) = u.authority() else { return false; };
    let Some(host) = u.host() else { return false; };
    let valid_host = if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        !ip.is_loopback() && !ip.is_private() && !ip.is_link_local() && !ip.is_unspecified()
            && !ip.is_multicast() && !ip.is_broadcast()
    } else {
        host.contains('.') && host.len() <= 253
            && host.split('.').all(|label| !label.is_empty() && label.len() <= 63
                && !label.starts_with('-') && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
            && ![".localhost", ".local", ".invalid", ".test", ".example"].iter().any(|s| host.ends_with(s))
    };
    valid_host && u.scheme_str() == Some("https") && !authority.as_str().contains('@')
        && u.port_u16().is_none() && u.path() == "/" && u.query().is_none()
        && value == format!("https://{authority}")
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum DisplayRequest {
    Payment { task_id: TaskId, operation_id: OperationId, consent_id: String },
    Card { operation_id: OperationId },
}
pub(super) async fn display_status(State(state): State<ApiState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    state.identity(&headers)?.check(&state)?;
    Ok(Json(serde_json::json!({"available":state.0.demo_controller.is_some()})))
}
pub(super) async fn display_code(
    State(state): State<ApiState>, request: Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let identity = state.identity(request.headers())?;
    if request.uri().query().is_some() { return Err(ApiError::validation(StatusCode::BAD_REQUEST)); }
    let secret = state.0.demo_controller.as_ref().ok_or_else(ApiError::forbidden)?;
    let input: DisplayRequest = body(request).await?;
    identity.check(&state)?;
    // Existing host enforces exact session/task/operation/consent, expiry and one-time delivery.
    // This endpoint cannot confirm, verify a code, issue a permit or execute a business action.
    let value = tokio::time::timeout(state.0.request_timeout, async {
        match input {
            DisplayRequest::Payment { task_id, operation_id, consent_id } => {
                let code = state.payment_background_challenge_for_native_controller(secret, &identity.reference, &task_id, &operation_id, &consent_id).await?;
                let code = code.ok_or(StorageError::DependencyUnavailable)?;
                Ok(serde_json::json!({"mode":"SAME_DEVICE_SIMULATION_ONLY","kind":"PAYMENT","task_id":task_id,"operation_id":operation_id,"code":code.expose_for_native_display(),"business_executed":false,"automatic_approval":false}))
            }
            DisplayRequest::Card { operation_id } => {
                let mut delivery = state.card_challenge_for_native_controller(secret, &identity.reference, &operation_id).await?;
                let code = delivery.take_secret().ok_or(StorageError::DependencyUnavailable)?;
                Ok(serde_json::json!({"mode":"SAME_DEVICE_SIMULATION_ONLY","kind":"CARD","operation_id":operation_id,"challenge_id":delivery.challenge_id,"code":code.expose_once_for_native_display(),"business_executed":false,"automatic_approval":false}))
            }
        }
    }).await.map_err(|_| ApiError::unavailable())?;
    value.map(Json).map_err(ApiError::storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_demo_config_and_new_proxy_boundary() {
        let c=PublicDemoConfig{backend_authority:"111.230.157.111".into(),trusted_proxy:"127.0.0.1".parse().unwrap()};
        assert!(c.validate("https://bank-production.vercel.app").is_ok());
        for o in ["http://bank-production.vercel.app","https://*.vercel.app","https://localhost","https://bank.vercel.app/path","https://u:p@bank.vercel.app","https://bank.vercel.app?x=1","https://127.0.0.1","https://bank.vercel.app/"] {assert!(c.validate(o).is_err(),"{o}");}
        let make=||Request::builder().method("POST").uri("/api/demo-verification").header("Host","111.230.157.111").header("Origin","https://bank-production.vercel.app").header("X-Forwarded-Host","111.230.157.111").header("X-Forwarded-Proto","https").extension(ConnectInfo("127.0.0.1:44200".parse::<SocketAddr>().unwrap())).body(axum::body::Body::empty()).unwrap();
        assert!(c.accepts("https://bank-production.vercel.app",&make()));
        for (k,v) in [("origin","https://preview.vercel.app"),("x-forwarded-proto","http"),("host","127.0.0.1:8787"),("x-forwarded-host","bad.invalid")] {let mut r=make();r.headers_mut().insert(k,v.parse().unwrap());assert!(!c.accepts("https://bank-production.vercel.app",&r));}
        let mut r=make();r.extensions_mut().clear();assert!(!c.accepts("https://bank-production.vercel.app",&r));
        let mut r=make();r.headers_mut().remove("origin");assert!(!c.accepts("https://bank-production.vercel.app",&r));
        let mut r=make();r.headers_mut().append("x-forwarded-proto","https".parse().unwrap());assert!(!c.accepts("https://bank-production.vercel.app",&r));
    }
}
