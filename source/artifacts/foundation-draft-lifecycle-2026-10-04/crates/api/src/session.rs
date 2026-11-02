use cookie::{Cookie, SameSite};
use safeflow_contracts::{ReasonCode, SecurityError, SessionRef, UserId};
use safeflow_runtime::{SessionBinding, SessionResolver};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub(crate) const COOKIE_NAME: &str = "safeflow_sim_session";
const MAX_SESSIONS: usize = 2048;
struct StoredSession {
    binding: SessionBinding,
    expires: Instant,
}
struct Sessions {
    next_version: u64,
    entries: HashMap<SessionRef, StoredSession>,
}
/// Process-local project identity. Neither requests nor model outputs choose this owner.
pub(crate) struct SimulationSessions {
    secret_hash: [u8; 32],
    owner: UserId,
    ttl: Duration,
    sessions: Mutex<Sessions>,
}
impl SimulationSessions {
    pub(crate) fn new(
        secret: Zeroizing<String>,
        owner: UserId,
        ttl: Duration,
    ) -> Result<Self, &'static str> {
        if !(16..=256).contains(&secret.len()) || ttl.is_zero() || ttl.as_secs() > 86400 {
            return Err("simulation login configuration invalid");
        }
        Ok(Self {
            secret_hash: Sha256::digest(secret.as_bytes()).into(),
            owner,
            ttl,
            sessions: Mutex::new(Sessions {
                next_version: 1,
                entries: HashMap::new(),
            }),
        })
    }
    pub(crate) fn check_secret(&self,secret:&str)->bool{
        let candidate:[u8;32]=Sha256::digest(secret.as_bytes()).into();
        bool::from(candidate.ct_eq(&self.secret_hash))
    }
    pub(crate) fn issue(
        &self,
        secret: &str,
        prior: Option<&SessionRef>,
    ) -> Result<String, &'static str> {
        let candidate: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        if !bool::from(candidate.ct_eq(&self.secret_hash)) {
            return Err("invalid login");
        }
        let mut entropy = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(&mut *entropy).map_err(|_| "session unavailable")?;
        let token: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let reference = token_reference(&token).ok_or("session unavailable")?;
        let mut sessions = self.sessions.lock().map_err(|_| "session unavailable")?;
        let now = Instant::now();
        sessions.entries.retain(|_, value| value.expires > now);
        if let Some(prior) = prior {
            sessions.entries.remove(prior);
        }
        if sessions.entries.len() >= MAX_SESSIONS {
            return Err("session unavailable");
        }
        let version = sessions.next_version;
        sessions.next_version = version.checked_add(1).ok_or("session unavailable")?;
        let expires = now.checked_add(self.ttl).ok_or("session unavailable")?;
        sessions.entries.insert(
            reference,
            StoredSession {
                binding: SessionBinding {
                    user_id: self.owner.clone(),
                    binding_version: version,
                },
                expires,
            },
        );
        Ok(Cookie::build((COOKIE_NAME, token))
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .max_age(time::Duration::seconds(self.ttl.as_secs() as i64))
            .build()
            .to_string())
    }
    pub(crate) fn revoke(&self, reference: &SessionRef) -> Result<(), SecurityError> {
        let mut sessions = self.sessions.lock().map_err(|_| invalid())?;
        sessions.entries.remove(reference).ok_or_else(invalid)?;
        sessions.next_version = sessions.next_version.checked_add(1).ok_or_else(invalid)?;
        Ok(())
    }
    pub(crate) fn expired_cookie() -> String {
        Cookie::build((COOKIE_NAME, ""))
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .max_age(time::Duration::ZERO)
            .build()
            .to_string()
    }
}
pub(crate) fn token_reference(token: &str) -> Option<SessionRef> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    SessionRef::parse(format!("http_sim_{:x}", Sha256::digest(token.as_bytes()))).ok()
}
impl SessionResolver for SimulationSessions {
    fn resolve(&self, reference: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(reference)?.user_id)
    }
    fn resolve_binding(&self, reference: &SessionRef) -> Result<SessionBinding, SecurityError> {
        let sessions = self.sessions.lock().map_err(|_| invalid())?;
        let session = sessions.entries.get(reference).ok_or_else(invalid)?;
        if session.expires <= Instant::now() {
            return Err(invalid());
        }
        Ok(session.binding.clone())
    }
}
fn invalid() -> SecurityError {
    SecurityError {
        reason_code: ReasonCode::parse("SESSION_INVALID").expect("fixed identifier"),
        message: "simulation session invalid".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> SimulationSessions {
        SimulationSessions::new(
            Zeroizing::new("test-only-secret-123456".into()),
            UserId::parse("user_001").unwrap(),
            Duration::from_secs(30),
        )
        .unwrap()
    }
    fn reference(cookie: &str) -> SessionRef {
        token_reference(Cookie::parse(cookie).unwrap().value()).unwrap()
    }
    #[test]
    fn invalid_secret_cannot_issue_and_cookie_is_opaque_random_and_protected() {
        let store = store();
        assert!(store.issue("wrong", None).is_err());
        let a = store.issue("test-only-secret-123456", None).unwrap();
        let b = store.issue("test-only-secret-123456", None).unwrap();
        assert_ne!(a, b);
        assert!(a.contains("HttpOnly") && a.contains("SameSite=Strict"));
        assert!(!a.contains("user_001") && !a.contains("test-only"));
        assert_eq!(store.resolve(&reference(&a)).unwrap().as_str(), "user_001");
    }
    #[test]
    fn logout_rotation_expiry_and_new_process_invalidate_prior_binding() {
        let store = store();
        let a = reference(&store.issue("test-only-secret-123456", None).unwrap());
        let first = store.resolve_binding(&a).unwrap();
        let b = reference(&store.issue("test-only-secret-123456", Some(&a)).unwrap());
        assert!(store.resolve_binding(&a).is_err());
        assert!(store.resolve_binding(&b).unwrap().binding_version > first.binding_version);
        store
            .sessions
            .lock()
            .unwrap()
            .entries
            .get_mut(&b)
            .unwrap()
            .expires = Instant::now();
        assert!(store.resolve_binding(&b).is_err());
        let c = reference(&store.issue("test-only-secret-123456", None).unwrap());
        store.revoke(&c).unwrap();
        assert!(store.resolve_binding(&c).is_err());
        assert!(SimulationSessions::new(
            Zeroizing::new("test-only-secret-123456".into()),
            UserId::parse("user_001").unwrap(),
            Duration::from_secs(30)
        )
        .unwrap()
        .resolve_binding(&c)
        .is_err());
    }
}
