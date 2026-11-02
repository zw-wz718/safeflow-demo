use super::*;
use crate::SessionBinding;
use safeflow_contracts::SecurityError;

struct Clock {
    millis: AtomicU64,
    precise: bool,
}
impl MockDraftClock for Clock {
    fn now_seconds(&self) -> u64 {
        self.millis.load(Ordering::SeqCst) / 1000
    }
    fn now_millis(&self) -> Option<u64> {
        self.precise.then(|| self.millis.load(Ordering::SeqCst))
    }
}
struct Sessions(AtomicU64);
impl SessionResolver for Sessions {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(UserId::parse("persistent_user").unwrap())
    }
    fn resolve_binding(&self, reference: &SessionRef) -> Result<SessionBinding, SecurityError> {
        Ok(SessionBinding {
            user_id: self.resolve(reference)?,
            binding_version: self.0.load(Ordering::SeqCst),
        })
    }
}
struct Fixture {
    authority: Arc<MockDraftAuthority>,
    sessions: Arc<Sessions>,
    clock: Arc<Clock>,
    context: ExecutionContext,
}
impl Fixture {
    fn new(precise: bool) -> Self {
        let sessions = Arc::new(Sessions(AtomicU64::new(1)));
        let clock = Arc::new(Clock {
            millis: AtomicU64::new(1_000_499),
            precise,
        });
        Self {
            authority: Arc::new(MockDraftAuthority::new(sessions.clone(), clock.clone())),
            sessions,
            clock,
            context: ExecutionContext {
                task_id: TaskId::parse("persisted_task").unwrap(),
                node_id: NodeId::parse("prepare_draft").unwrap(),
                operation_id: OperationId::parse("original_public_operation").unwrap(),
                authenticated_user_id: UserId::parse("persistent_user").unwrap(),
                plan_version: 1,
                idempotency_key: Some(IdempotencyKey::parse("committed_claim").unwrap()),
            },
        }
    }
    fn request(&self) -> Result<MockDraftWriteRequest, RuntimeError> {
        self.authority.persistent_request(
            &self.context,
            SessionRef::parse("persistent_session").unwrap(),
            1,
            7,
            1_000_500,
            1_000_400,
            json!({"existing_arguments":"unchanged"}),
        )
    }
    fn begin(&self) -> (MockDraftWriteRequest, MockDraftPermit, Reservation) {
        let request = self.request().unwrap();
        let confirmation = self
            .authority
            .record_explicit_confirmation(&request)
            .unwrap();
        let permit = self.authority.issue(&confirmation).unwrap();
        let Admission::Execute(reservation) = self.authority.begin(&request, &permit).unwrap()
        else {
            panic!("first claim must begin");
        };
        self.authority
            .claim_tool(
                &permit,
                &self.context,
                &request.tool_name,
                &request.arguments,
            )
            .unwrap();
        (request, permit, reservation)
    }
    fn save_guard<'a>(
        &'a self,
        request: &MockDraftWriteRequest,
        permit: &MockDraftPermit,
    ) -> Result<MockDraftSaveGuard<'a>, RuntimeError> {
        self.authority.lock_claim_for_save(
            permit,
            &self.context,
            &request.tool_name,
            &request.arguments,
        )
    }
}

#[test]
fn persistent_authority_requires_precise_clock() {
    let fixture = Fixture::new(false);
    assert!(
        matches!(fixture.request(), Err(RuntimeError::MockDraftAuthorization(code))
        if code == "PRECISE_DRAFT_CLOCK_REQUIRED")
    );
}

#[test]
fn persistent_save_guard_rejects_at_original_millisecond_deadline_after_tool_claim() {
    let fixture = Fixture::new(true);
    let (request, permit, _reservation) = fixture.begin();
    assert_eq!(request.operation_id(), fixture.context.operation_id);
    assert_eq!(request.persistent.as_ref().unwrap().facts_version, 7);
    drop(fixture.save_guard(&request, &permit).unwrap());
    fixture.clock.millis.store(1_000_500, Ordering::SeqCst);
    assert!(matches!(fixture.save_guard(&request, &permit),
        Err(RuntimeError::MockDraftAuthorization(code)) if code == "PERSISTENT_DRAFT_SOURCE_EXPIRED"));
}

#[test]
fn persistent_save_guard_rejects_same_user_session_version_change_after_tool_claim() {
    let fixture = Fixture::new(true);
    let (request, permit, _reservation) = fixture.begin();
    drop(fixture.save_guard(&request, &permit).unwrap());
    fixture.sessions.0.store(2, Ordering::SeqCst);
    assert!(matches!(fixture.save_guard(&request, &permit),
        Err(RuntimeError::MockDraftAuthorization(code)) if code == "SESSION_BINDING_CHANGED"));
}

#[test]
fn persistent_save_guard_rejects_clock_rollback_after_tool_claim() {
    let fixture = Fixture::new(true);
    let (request, permit, _reservation) = fixture.begin();
    drop(fixture.save_guard(&request, &permit).unwrap());
    fixture.clock.millis.store(1_000_498, Ordering::SeqCst);
    assert!(matches!(fixture.save_guard(&request, &permit),
        Err(RuntimeError::MockDraftAuthorization(code)) if code == "PERSISTENT_DRAFT_SOURCE_EXPIRED"));
}
