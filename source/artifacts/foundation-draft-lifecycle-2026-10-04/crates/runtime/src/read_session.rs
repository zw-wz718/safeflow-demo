//! Opaque identity context for backend reads. Only actual Runtime/Kernel calls
//! establish the scope; no DTO, guessed binding version or public constructor.
use crate::{SessionBinding, SessionResolver};
use safeflow_contracts::{ReasonCode, SecurityError, SessionRef};
use std::{future::Future, sync::Arc};

tokio::task_local! { static READ_SESSION: AuthenticatedReadSession; }
#[derive(Clone)]
pub struct AuthenticatedReadSession {
    sessions: Arc<dyn SessionResolver>,
    session: SessionRef,
    binding: SessionBinding,
}
impl AuthenticatedReadSession {
    pub fn current_for_backend() -> Result<Self, SecurityError> {
        let value = READ_SESSION
            .try_with(Clone::clone)
            .map_err(|_| unavailable())?;
        value.check()?;
        Ok(value)
    }
    pub fn check(&self) -> Result<SessionBinding, SecurityError> {
        if self.binding.binding_version == 0
            || self.sessions.resolve_binding(&self.session)? != self.binding
        {
            return Err(unavailable());
        }
        Ok(self.binding.clone())
    }
}
fn unavailable() -> SecurityError {
    SecurityError {
        reason_code: ReasonCode::parse("READ_SESSION_UNAVAILABLE").unwrap(),
        message: "authenticated read session unavailable".into(),
    }
}
pub(crate) async fn scope<F: Future>(
    sessions: &Arc<dyn SessionResolver>,
    session: &SessionRef,
    future: F,
) -> Result<F::Output, SecurityError> {
    // Ordinary adapters retain their existing resolver contract. A consumer
    // requiring the opaque context explicitly refuses when none is available.
    let binding = match sessions.resolve_binding(session) {
        Ok(binding) if binding.binding_version > 0 => binding,
        _ => return Ok(future.await),
    };
    let context = AuthenticatedReadSession {
        sessions: sessions.clone(),
        session: session.clone(),
        binding,
    };
    context.check()?;
    let result = READ_SESSION.scope(context.clone(), future).await;
    context.check()?;
    Ok(result)
}
