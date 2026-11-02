use crate::{
    AuditEvent, AuditEventKind, AuditRecorder, RuntimeError, RuntimeOutcome, SessionResolver,
};
use safeflow_contracts::{
    AgentPlan, EventId, ExecutionContext, IdempotencyKey, MockDraftAction, MockDraftPermit, NodeId,
    OperationId, PlanNode, PlanningResult, SessionRef, TaskId, ToolName, ToolResult, Understanding,
    UnderstandingDecision, UserId, MOCK_DRAFT_LIFECYCLE_CONTRACT_VERSION,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};

#[cfg(test)]
#[path = "mock_draft/persistent_tests.rs"]
mod persistent_tests;

pub trait MockDraftClock: Send + Sync {
    fn now_seconds(&self) -> u64;
    fn now_millis(&self) -> Option<u64> {
        None
    }
}

/// Server-owned envelope; never part of LLM Tool arguments or a public JSON API.
#[derive(Clone, Debug, PartialEq)]
pub struct MockDraftWriteRequest {
    pub contract_version: u16,
    pub request_id: IdempotencyKey,
    pub session_ref: SessionRef,
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub tool_name: ToolName,
    pub plan_version: u32,
    pub arguments: Value,
    pub expires_at: u64,
    pub(crate) persistent: Option<PersistentBinding>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PersistentBinding {
    pub(crate) operation_id: OperationId,
    pub(crate) facts_version: u32,
    session_binding_version: u64,
    expires_at_ms: i64,
    not_before_ms: i64,
    pub(crate) lifecycle: bool,
}

/// A verified single action, never evidence that the whole task completed.
/// ```compile_fail
/// let forged: safeflow_runtime::VerifiedMockDraftActionOutcome =
///     serde_json::from_str("{}").unwrap();
/// ```
pub struct VerifiedMockDraftActionOutcome {
    context: ExecutionContext,
    action: MockDraftAction,
    result: ToolResult,
}
impl VerifiedMockDraftActionOutcome {
    pub fn context(&self) -> &ExecutionContext {
        &self.context
    }
    pub fn action(&self) -> MockDraftAction {
        self.action
    }
    pub fn result(&self) -> &ToolResult {
        &self.result
    }
}

impl MockDraftWriteRequest {
    pub fn planning_result(&self) -> Result<PlanningResult, RuntimeError> {
        Ok(PlanningResult::for_plan(
            Understanding {
                intent: "PREPARE_MOCK_DRAFT_ONLY".into(),
                entities: json!({}),
                missing_fields: vec![],
                context_refs: vec![],
                decision: UnderstandingDecision::Pass,
            },
            AgentPlan::new(
                self.task_id.clone(),
                self.plan_version,
                vec![PlanNode::Tool {
                    id: self.node_id.clone(),
                    tool: self.tool_name.clone(),
                    arguments: self.arguments.clone(),
                    depends_on: vec![],
                }],
            )
            .map_err(|e| RuntimeError::InvalidPlan(e.to_string()))?,
        ))
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        if let Some(binding) = &self.persistent {
            return binding.operation_id.clone();
        }
        OperationId::parse(format!("operation_{}_{}", self.task_id, self.node_id))
            .expect("typed task/node IDs")
    }
}

/// Evidence of a simulated explicit user action recorded by trusted server code.
/// Private fields and object identity prevent client reconstruction.
pub struct MockDraftConfirmation {
    request_id: IdempotencyKey,
    identity: MockDraftPermit,
}

pub struct MockDraftAuthority {
    pub(crate) sessions: Arc<dyn SessionResolver>,
    clock: Arc<dyn MockDraftClock>,
    sequence: AtomicU64,
    persistent_time: AtomicU64,
    entries: Mutex<HashMap<IdempotencyKey, Entry>>,
}

struct Entry {
    request: MockDraftWriteRequest,
    user: UserId,
    session_binding_version: Option<u64>,
    confirmation: MockDraftPermit,
    permit: Option<MockDraftPermit>,
    revoked: bool,
    state: State,
    tool_result: Option<ToolResult>,
    waiters: Vec<Waker>,
}

enum State {
    Pending,
    Running { tool_claimed: bool },
    Finished(Result<RuntimeOutcome, RuntimeError>),
}

/// Holds the authority lock through the synchronous save. Revocation either
/// wins before this guard, or takes effect after that save has committed.
pub struct MockDraftSaveGuard<'a> {
    _guard: MutexGuard<'a, HashMap<IdempotencyKey, Entry>>,
}

pub(crate) enum Admission {
    Execute(Reservation),
    Wait,
    Replay(Result<RuntimeOutcome, RuntimeError>),
}

impl MockDraftAuthority {
    pub(crate) fn persistent_request(
        &self,
        context: &ExecutionContext,
        session_ref: SessionRef,
        session_binding_version: u64,
        facts_version: u32,
        expires_at_ms: i64,
        not_before_ms: i64,
        arguments: Value,
    ) -> Result<MockDraftWriteRequest, RuntimeError> {
        self.persistent_action_request(
            context,
            session_ref,
            session_binding_version,
            facts_version,
            expires_at_ms,
            not_before_ms,
            arguments,
            MockDraftAction::Create,
            false,
        )
    }

    fn persistent_action_request(
        &self,
        context: &ExecutionContext,
        session_ref: SessionRef,
        session_binding_version: u64,
        facts_version: u32,
        expires_at_ms: i64,
        not_before_ms: i64,
        arguments: Value,
        action: MockDraftAction,
        lifecycle: bool,
    ) -> Result<MockDraftWriteRequest, RuntimeError> {
        if not_before_ms < 0
            || expires_at_ms <= not_before_ms
            || session_binding_version == 0
            || facts_version == 0
        {
            return Err(denied("INVALID_PERSISTENT_DRAFT_SOURCES"));
        }
        let binding = self
            .sessions
            .resolve_binding(&session_ref)
            .map_err(|_| denied("VERSIONED_SESSION_REQUIRED"))?;
        if binding.user_id != context.authenticated_user_id
            || binding.binding_version != session_binding_version
        {
            return Err(denied("SESSION_BINDING_CHANGED"));
        }
        let request = MockDraftWriteRequest {
            contract_version: action.contract_version(),
            request_id: context
                .idempotency_key
                .clone()
                .ok_or_else(|| denied("PERSISTENT_CLAIM_REQUIRED"))?,
            session_ref,
            task_id: context.task_id.clone(),
            node_id: context.node_id.clone(),
            tool_name: ToolName::parse(action.tool_name()).expect("closed draft Tool"),
            plan_version: context.plan_version,
            arguments,
            expires_at: (expires_at_ms as u64).div_ceil(1000),
            persistent: Some(PersistentBinding {
                operation_id: context.operation_id.clone(),
                facts_version,
                session_binding_version,
                expires_at_ms,
                not_before_ms,
                lifecycle,
            }),
        };
        self.validate_request(&request)?;
        Ok(request)
    }

    pub fn new(sessions: Arc<dyn SessionResolver>, clock: Arc<dyn MockDraftClock>) -> Self {
        Self {
            sessions,
            clock,
            sequence: AtomicU64::new(0),
            persistent_time: AtomicU64::new(0),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn request(
        &self,
        session_ref: SessionRef,
        task_id: TaskId,
        node_id: NodeId,
        plan_version: u32,
        arguments: Value,
        ttl_seconds: u64,
    ) -> Result<MockDraftWriteRequest, RuntimeError> {
        self.request_for_action(
            MockDraftAction::Create,
            session_ref,
            task_id,
            node_id,
            plan_version,
            arguments,
            ttl_seconds,
        )
    }

    /// Trusted assembly chooses an action from the closed storage-only whitelist.
    pub fn request_for_action(
        &self,
        action: MockDraftAction,
        session_ref: SessionRef,
        task_id: TaskId,
        node_id: NodeId,
        plan_version: u32,
        arguments: Value,
        ttl_seconds: u64,
    ) -> Result<MockDraftWriteRequest, RuntimeError> {
        if ttl_seconds == 0 || plan_version == 0 {
            return Err(denied("INVALID_VERSION_OR_TTL"));
        }
        let expires_at = self
            .clock
            .now_seconds()
            .checked_add(ttl_seconds)
            .ok_or_else(|| denied("TIME_OVERFLOW"))?;
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(MockDraftWriteRequest {
            contract_version: action.contract_version(),
            request_id: IdempotencyKey::parse(format!("mock_draft_request_{sequence}"))
                .expect("server ID"),
            session_ref,
            task_id,
            node_id,
            tool_name: ToolName::parse(action.tool_name()).expect("static Tool"),
            plan_version,
            arguments,
            expires_at,
            persistent: None,
        })
    }

    /// Trusted mock UI/test-server boundary. Call only after the user explicitly
    /// confirms the displayed draft-storage parameters. Never expose to an LLM.
    pub fn record_explicit_confirmation(
        &self,
        request: &MockDraftWriteRequest,
    ) -> Result<MockDraftConfirmation, RuntimeError> {
        self.validate_request(request)?;
        let (user, session_binding_version) = self.session_snapshot(request)?;
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        if let Some(entry) = entries.get(&request.request_id) {
            if entry.request == *request
                && entry.user == user
                && entry.session_binding_version == session_binding_version
                && !entry.revoked
            {
                return Ok(MockDraftConfirmation {
                    request_id: request.request_id.clone(),
                    identity: entry.confirmation.clone(),
                });
            }
            return Err(denied("CONFIRMATION_REQUEST_CONFLICT"));
        }
        let same_scope =
            |entry: &Entry| entry.user == user && entry.request.task_id == request.task_id;
        if let Some(current_version) = entries
            .values()
            .filter(|entry| same_scope(entry))
            .map(|entry| entry.request.plan_version)
            .max()
        {
            if request.plan_version < current_version {
                return Err(denied("CONFIRMATION_VERSION_ROLLBACK"));
            }
            if request.plan_version == current_version {
                return Err(denied("CONFIRMATION_SAME_VERSION_CONFLICT"));
            }
        }
        // One lock covers checking the latest confirmed version and invalidating
        // older grants. A running old Tool must re-check under this same lock.
        let mut waiters = vec![];
        for entry in entries.values_mut().filter(|entry| same_scope(entry)) {
            entry.revoked = true;
            waiters.append(&mut entry.waiters);
        }
        let identity = MockDraftPermit::unregistered();
        entries.insert(
            request.request_id.clone(),
            Entry {
                request: request.clone(),
                user,
                session_binding_version,
                confirmation: identity.clone(),
                permit: None,
                revoked: false,
                state: State::Pending,
                tool_result: None,
                waiters: vec![],
            },
        );
        drop(entries);
        for waker in waiters {
            waker.wake();
        }
        Ok(MockDraftConfirmation {
            request_id: request.request_id.clone(),
            identity,
        })
    }

    pub fn issue(
        &self,
        confirmation: &MockDraftConfirmation,
    ) -> Result<MockDraftPermit, RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .get_mut(&confirmation.request_id)
            .ok_or_else(|| denied("UNKNOWN_CONFIRMATION"))?;
        self.validate_request(&entry.request)?;
        self.validate_session(entry)?;
        if entry.revoked || !entry.confirmation.same_instance(&confirmation.identity) {
            return Err(denied("CONFIRMATION_NOT_ACTIVE"));
        }
        if let Some(permit) = &entry.permit {
            return Ok(permit.clone());
        }
        let permit = MockDraftPermit::unregistered();
        entry.permit = Some(permit.clone());
        Ok(permit)
    }

    pub fn revoke(&self, permit: &MockDraftPermit) -> Result<(), RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .values_mut()
            .find(|e| e.permit.as_ref().is_some_and(|p| p.same_instance(permit)))
            .ok_or_else(|| denied("UNKNOWN_PERMIT"))?;
        entry.revoked = true;
        let waiters = std::mem::take(&mut entry.waiters);
        drop(entries);
        for waker in waiters {
            waker.wake();
        }
        Ok(())
    }

    pub fn revoke_task(&self, task: &TaskId) -> Result<(), RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let mut waiters = vec![];
        for entry in entries.values_mut().filter(|e| &e.request.task_id == task) {
            entry.revoked = true;
            waiters.append(&mut entry.waiters);
        }
        drop(entries);
        for waker in waiters {
            waker.wake();
        }
        Ok(())
    }

    fn validate_request(&self, request: &MockDraftWriteRequest) -> Result<(), RuntimeError> {
        if MockDraftAction::for_tool(request.tool_name.as_str()).map_or(true, |action| {
            request.contract_version != action.contract_version()
        }) || request.plan_version == 0
        {
            return Err(denied("INVALID_DRAFT_AUTHORIZATION_CONTRACT"));
        }
        if let Some(binding) = &request.persistent {
            let now = self
                .clock
                .now_millis()
                .ok_or_else(|| denied("PRECISE_DRAFT_CLOCK_REQUIRED"))?;
            let previous = self.persistent_time.fetch_max(now, Ordering::SeqCst);
            if binding.facts_version == 0
                || binding.session_binding_version == 0
                || binding.expires_at_ms <= 0
                || u128::from(now) >= binding.expires_at_ms as u128
                || u128::from(now) < binding.not_before_ms as u128
                || now < previous
            {
                return Err(denied("PERSISTENT_DRAFT_SOURCE_EXPIRED"));
            }
        } else if self.clock.now_seconds() >= request.expires_at {
            return Err(denied("PERMIT_EXPIRED"));
        }
        Ok(())
    }

    fn validate_session(&self, entry: &Entry) -> Result<(), RuntimeError> {
        let (user, binding_version) = self.session_snapshot(&entry.request)?;
        if user != entry.user {
            return Err(denied("SESSION_USER_CHANGED"));
        }
        if binding_version != entry.session_binding_version {
            return Err(denied("SESSION_BINDING_CHANGED"));
        }
        Ok(())
    }

    fn session_snapshot(
        &self,
        request: &MockDraftWriteRequest,
    ) -> Result<(UserId, Option<u64>), RuntimeError> {
        if request.contract_version == MOCK_DRAFT_LIFECYCLE_CONTRACT_VERSION
            || request.persistent.is_some()
        {
            let binding = self
                .sessions
                .resolve_binding(&request.session_ref)
                .map_err(|_| denied("VERSIONED_SESSION_REQUIRED"))?;
            if binding.binding_version == 0 {
                return Err(denied("INVALID_SESSION_BINDING_VERSION"));
            }
            if request
                .persistent
                .as_ref()
                .is_some_and(|source| source.session_binding_version != binding.binding_version)
            {
                return Err(denied("SESSION_BINDING_CHANGED"));
            }
            Ok((binding.user_id, Some(binding.binding_version)))
        } else {
            self.sessions
                .resolve(&request.session_ref)
                .map(|user| (user, None))
                .map_err(|_| denied("INVALID_SESSION"))
        }
    }

    fn validate_entry(
        &self,
        entry: &Entry,
        request: &MockDraftWriteRequest,
        permit: &MockDraftPermit,
    ) -> Result<(), RuntimeError> {
        self.validate_request(request)?;
        self.validate_session(entry)?;
        if entry.revoked
            || entry.request != *request
            || !entry
                .permit
                .as_ref()
                .is_some_and(|p| p.same_instance(permit))
        {
            return Err(denied("PERMIT_BINDING_MISMATCH_OR_REVOKED"));
        }
        Ok(())
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        request: &MockDraftWriteRequest,
        permit: &MockDraftPermit,
    ) -> Result<Admission, RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .get_mut(&request.request_id)
            .ok_or_else(|| denied("UNKNOWN_PERMIT"))?;
        self.validate_entry(entry, request, permit)?;
        match &entry.state {
            State::Finished(result) => Ok(Admission::Replay(result.clone())),
            State::Running { .. } => Ok(Admission::Wait),
            State::Pending => {
                entry.state = State::Running {
                    tool_claimed: false,
                };
                Ok(Admission::Execute(Reservation {
                    authority: self.clone(),
                    request: request.clone(),
                    finished: false,
                    audit: None,
                }))
            }
        }
    }

    pub(crate) async fn wait(
        &self,
        request: &MockDraftWriteRequest,
        permit: &MockDraftPermit,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        std::future::poll_fn(|cx| {
            let mut entries = match self.entries.lock() {
                Ok(e) => e,
                Err(_) => return Poll::Ready(Err(denied("AUTHORITY_UNAVAILABLE"))),
            };
            let Some(entry) = entries.get_mut(&request.request_id) else {
                return Poll::Ready(Err(denied("UNKNOWN_PERMIT")));
            };
            if let Err(error) = self.validate_entry(entry, request, permit) {
                return Poll::Ready(Err(error));
            }
            if let State::Finished(result) = &entry.state {
                return Poll::Ready(result.clone());
            }
            if !entry.waiters.iter().any(|w| w.will_wake(cx.waker())) {
                entry.waiters.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }

    /// A permit is accepted only while Runtime owns its execution reservation.
    /// Exactly one Tool invocation may claim it; direct pre-Runtime use fails.
    pub fn claim_tool(
        &self,
        permit: &MockDraftPermit,
        context: &ExecutionContext,
        tool: &ToolName,
        arguments: &Value,
    ) -> Result<(), RuntimeError> {
        self.check_tool(permit, context, tool, arguments, true)
    }

    /// Repeat the live checks immediately before the synchronous business save.
    pub fn verify_claim(
        &self,
        permit: &MockDraftPermit,
        context: &ExecutionContext,
        tool: &ToolName,
        arguments: &Value,
    ) -> Result<(), RuntimeError> {
        self.check_tool(permit, context, tool, arguments, false)
    }

    pub fn lock_claim_for_save<'a>(
        &'a self,
        permit: &MockDraftPermit,
        context: &ExecutionContext,
        tool: &ToolName,
        arguments: &Value,
    ) -> Result<MockDraftSaveGuard<'a>, RuntimeError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .values()
            .find(|e| e.permit.as_ref().is_some_and(|p| p.same_instance(permit)))
            .ok_or_else(|| denied("UNKNOWN_PERMIT"))?;
        self.validate_entry(entry, &entry.request, permit)?;
        Self::validate_tool_binding(entry, context, tool, arguments)?;
        if !matches!(entry.state, State::Running { tool_claimed: true }) {
            return Err(denied("PERMIT_NOT_EXECUTING_OR_ALREADY_CONSUMED"));
        }
        Ok(MockDraftSaveGuard { _guard: entries })
    }

    /// Trusted Tool registers only a business-readback-verified result. Runtime
    /// compares the complete returned envelope and body against this record.
    pub fn record_tool_result(
        &self,
        permit: &MockDraftPermit,
        context: &ExecutionContext,
        arguments: &Value,
        result: &ToolResult,
    ) -> Result<(), RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .values_mut()
            .find(|e| e.permit.as_ref().is_some_and(|p| p.same_instance(permit)))
            .ok_or_else(|| denied("UNKNOWN_PERMIT"))?;
        self.validate_entry(entry, &entry.request, permit)?;
        Self::validate_tool_binding(entry, context, &result.tool_name, arguments)?;
        if !matches!(entry.state, State::Running { tool_claimed: true })
            || entry.tool_result.is_some()
            || result.operation_id != context.operation_id
        {
            return Err(denied("RESULT_NOT_RECORDABLE"));
        }
        entry.tool_result = Some(result.clone());
        Ok(())
    }

    pub(crate) fn verify_tool_result(
        &self,
        request: &MockDraftWriteRequest,
        permit: &MockDraftPermit,
        result: &ToolResult,
    ) -> Result<(), RuntimeError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .get(&request.request_id)
            .ok_or_else(|| denied("UNKNOWN_PERMIT"))?;
        self.validate_entry(entry, request, permit)?;
        if entry.tool_result.as_ref() != Some(result) {
            return Err(denied("UNVERIFIED_OR_MODIFIED_TOOL_RESULT"));
        }
        Ok(())
    }

    fn check_tool(
        &self,
        permit: &MockDraftPermit,
        context: &ExecutionContext,
        tool: &ToolName,
        arguments: &Value,
        claim: bool,
    ) -> Result<(), RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| denied("AUTHORITY_UNAVAILABLE"))?;
        let entry = entries
            .values_mut()
            .find(|e| e.permit.as_ref().is_some_and(|p| p.same_instance(permit)))
            .ok_or_else(|| denied("UNKNOWN_PERMIT"))?;
        self.validate_entry(entry, &entry.request, permit)?;
        Self::validate_tool_binding(entry, context, tool, arguments)?;
        match &mut entry.state {
            State::Running { tool_claimed } if claim && !*tool_claimed => {
                *tool_claimed = true;
                Ok(())
            }
            State::Running { tool_claimed: true } if !claim => Ok(()),
            _ => Err(denied("PERMIT_NOT_EXECUTING_OR_ALREADY_CONSUMED")),
        }
    }

    fn validate_tool_binding(
        entry: &Entry,
        context: &ExecutionContext,
        tool: &ToolName,
        arguments: &Value,
    ) -> Result<(), RuntimeError> {
        let request = &entry.request;
        if context.authenticated_user_id != entry.user
            || context.task_id != request.task_id
            || context.node_id != request.node_id
            || context.operation_id != request.operation_id()
            || context.plan_version != request.plan_version
            || context.idempotency_key.as_ref() != Some(&request.request_id)
            || tool != &request.tool_name
            || arguments != &request.arguments
        {
            return Err(denied("TOOL_BINDING_MISMATCH"));
        }
        Ok(())
    }

    fn finish(
        &self,
        request: &MockDraftWriteRequest,
        result: Result<RuntimeOutcome, RuntimeError>,
    ) {
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.get_mut(&request.request_id) {
                entry.state = State::Finished(result);
                let waiters = std::mem::take(&mut entry.waiters);
                drop(entries);
                for waker in waiters {
                    waker.wake();
                }
            }
        }
    }
}

impl crate::Runtime {
    pub async fn execute_persistent_mock_draft_action_for_backend(
        &self,
        context: ExecutionContext,
        session: SessionRef,
        session_binding_version: u64,
        facts_version: u32,
        expires_at_ms: i64,
        not_before_ms: i64,
        arguments: Value,
        action: MockDraftAction,
    ) -> Result<VerifiedMockDraftActionOutcome, RuntimeError> {
        let authority = self
            .mock_drafts
            .as_ref()
            .ok_or_else(|| denied("MOCK_DRAFT_PATH_NOT_CONFIGURED"))?;
        let request = authority.persistent_action_request(
            &context,
            session,
            session_binding_version,
            facts_version,
            expires_at_ms,
            not_before_ms,
            arguments,
            action,
            true,
        )?;
        let confirmation = authority.record_explicit_confirmation(&request)?;
        let permit = authority.issue(&confirmation)?;
        let RuntimeOutcome::MockDraftActionVerified {
            task_id,
            operation_id,
            result,
        } = self.execute_mock_draft(request, &permit).await?
        else {
            return Err(denied("ACTION_RESULT_REQUIRED"));
        };
        if task_id != context.task_id || operation_id != context.operation_id {
            return Err(denied("ACTION_RESULT_BINDING_MISMATCH"));
        }
        Ok(VerifiedMockDraftActionOutcome {
            context,
            action,
            result,
        })
    }

    /// Trusted PostgreSQL orchestrator only, while it owns a committed one-time claim
    /// and locks the actual confirmation/decision sources. Never an HTTP input API.
    pub async fn execute_persistent_mock_draft_for_backend(
        &self,
        context: ExecutionContext,
        session: SessionRef,
        session_binding_version: u64,
        facts_version: u32,
        expires_at_ms: i64,
        not_before_ms: i64,
        arguments: Value,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        let authority = self
            .mock_drafts
            .as_ref()
            .ok_or_else(|| denied("MOCK_DRAFT_PATH_NOT_CONFIGURED"))?;
        let request = authority.persistent_request(
            &context,
            session,
            session_binding_version,
            facts_version,
            expires_at_ms,
            not_before_ms,
            arguments,
        )?;
        // This internal trusted handoff follows real persisted source verification;
        // no client boolean, confirmation renewal or production Permit is accepted.
        let confirmation = authority.record_explicit_confirmation(&request)?;
        let permit = authority.issue(&confirmation)?;
        self.execute_mock_draft(request, &permit).await
    }
}

pub(crate) struct Reservation {
    authority: Arc<MockDraftAuthority>,
    request: MockDraftWriteRequest,
    finished: bool,
    pub(crate) audit: Option<Arc<dyn AuditRecorder>>,
}

impl Reservation {
    pub(crate) fn finish(mut self, result: Result<RuntimeOutcome, RuntimeError>) {
        self.authority.finish(&self.request, result);
        self.finished = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.finished {
            self.authority.finish(
                &self.request,
                Err(denied("EXECUTION_INTERRUPTED_RESULT_UNKNOWN")),
            );
            if let Some(audit) = &self.audit {
                let _ = audit.record(AuditEvent {
                    event_id: EventId::parse(format!(
                        "mock_interrupted_{}",
                        self.request.request_id
                    ))
                    .expect("server ID"),
                    kind: AuditEventKind::WriteFailed,
                    task_id: self.request.task_id.clone(),
                    node_id: Some(self.request.node_id.clone()),
                    operation_id: Some(self.request.operation_id()),
                    tool_name: Some(self.request.tool_name.clone()),
                    details: json!({"reason": "EXECUTION_INTERRUPTED_RESULT_UNKNOWN"}),
                });
            }
        }
    }
}

fn denied(code: &str) -> RuntimeError {
    RuntimeError::MockDraftAuthorization(code.into())
}
