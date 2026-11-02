//! Per-request durations only: no message, endpoint, identity or credential log.
use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use std::{cell::RefCell, time::{Duration, Instant}};

tokio::task_local! {
    static METRICS: RefCell<Vec<(&'static str, Duration)>>;
}
fn model_time() -> Duration {
    METRICS.try_with(|m| m.borrow().iter().filter(|(name, _)| *name == "model").map(|(_, value)| *value).sum()).unwrap_or_default()
}
pub(super) struct Phase {
    name: &'static str,
    start: Instant,
    model_before: Duration,
}
impl Phase {
    pub(super) fn new(name: &'static str) -> Self {
        Self { name, start: Instant::now(), model_before: model_time() }
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        let mut duration = self.start.elapsed();
        if self.name == "plan" { duration = duration.saturating_sub(model_time().saturating_sub(self.model_before)); }
        let _ = METRICS.try_with(|m| m.borrow_mut().push((self.name, duration)));
    }
}
pub(super) async fn observe(request: Request, next: Next) -> Response {
    let readonly_tools = request.uri().path().starts_with("/api/agent/tasks/") && request.uri().path().ends_with("/execute");
    METRICS.scope(RefCell::new(Vec::new()), async move {
        let began = Instant::now();
        let mut response = {
            let _tools = readonly_tools.then(|| Phase::new("tools"));
            next.run(request).await
        };
        let timings = METRICS.with(|m| {
            let mut metrics = m.borrow_mut();
            metrics.push(("total", began.elapsed()));
            metrics.iter().map(|(name, duration)| format!("{name};dur={:.2}", duration.as_secs_f64() * 1000.0)).collect::<Vec<_>>().join(", ")
        });
        if let Ok(value) = HeaderValue::from_str(&timings) { response.headers_mut().insert("server-timing", value); }
        response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
        response.headers_mut().insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        response
    }).await
}
