use super::*;
use crate::security::DeterministicSecurityKernel;
use crate::{
    DomainPolicyRegistry, InMemoryAuditRecorder, SessionResolver, ToolRegistry, ToolSecurityCatalog,
};
use safeflow_contracts::*;
use std::future::poll_fn;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    RwLock,
};
use std::task::{Context, Poll, Wake, Waker};

struct Sessions(RwLock<Option<SessionBinding>>);
impl SessionResolver for Sessions {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        panic!("versioned identity required")
    }
    fn resolve_binding(&self, _: &SessionRef) -> Result<SessionBinding, SecurityError> {
        self.0.read().unwrap().clone().ok_or(SecurityError {
            reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
            message: "private".into(),
        })
    }
}

// Test-only scheduling around the actual deterministic Kernel, never a success provider.
struct WaitingKernel {
    actual: DeterministicSecurityKernel,
    release: AtomicBool,
}
impl SecurityKernel for WaitingKernel {
    fn validate_plan<'a>(
        &'a self,
        request: PlanAcceptanceRequest,
    ) -> ContractFuture<'a, Result<PlanAcceptanceDecision, SecurityError>> {
        Box::pin(async move {
            poll_fn(|_| {
                if self.release.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            self.actual.validate_plan(request).await
        })
    }
    fn evaluate<'a>(
        &'a self,
        _: OperationSecurityRequest,
    ) -> ContractFuture<'a, Result<SecurityDecision, SecurityError>> {
        panic!("intake cannot evaluate an operation")
    }
    fn verify_execution<'a>(
        &'a self,
        _: PostExecuteRequest,
    ) -> ContractFuture<'a, Result<PostExecuteAssessment, SecurityError>> {
        panic!("intake cannot verify execution")
    }
}
struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

// Registration-only public-contract sentinel; it has no business implementation.
struct RegistrationSentinel {
    name: ToolName,
    drift: AtomicBool,
}
impl AgentTool for RegistrationSentinel {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            if self.drift.load(Ordering::SeqCst) {
                "changed registration"
            } else {
                "contract registration"
            },
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            1,
            false,
            "PUBLIC_CONTRACT_SENTINEL",
        )
        .unwrap()
    }
    fn execute<'a>(
        &'a self,
        _: &'a ExecutionContext,
        _: ToolExecutionAuthorization<'a>,
        _: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        panic!("no business execution in registration test")
    }
}
impl DomainPolicy for RegistrationSentinel {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        panic!("no operation evaluation in registration test")
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        panic!("no execution verification in registration test")
    }
}

#[test]
fn kernel_wait_cannot_recapture_identity_or_registration_on_either_route() {
    for typed in [false, true] {
        for mode in 0..4 {
            let sessions = Arc::new(Sessions(RwLock::new(Some(SessionBinding {
                user_id: UserId::parse("user_001").unwrap(),
                binding_version: 1,
            }))));
            let sentinel = Arc::new(RegistrationSentinel {
                name: ToolName::parse("contract_sentinel").unwrap(),
                drift: AtomicBool::new(false),
            });
            let mut tools = ToolRegistry::new();
            tools.register(sentinel.clone()).unwrap();
            let mut policies = DomainPolicyRegistry::new();
            let policy_version = PolicyVersion::parse("contract-v1").unwrap();
            policies
                .register(sentinel.clone(), policy_version.clone(), 1)
                .unwrap();
            let mut catalog = ToolSecurityCatalog::new();
            catalog
                .register(crate::ToolSecurityProfile {
                    tool_name: sentinel.name.clone(),
                    tool_schema_version: 1,
                    policy_version,
                    minimum_level: SecurityLevel::Green,
                })
                .unwrap();
            let tools = Arc::new(tools);
            let policies = Arc::new(policies);
            let catalog = Arc::new(catalog);
            let kernel = Arc::new(WaitingKernel {
                actual: DeterministicSecurityKernel::new(
                    tools.clone(),
                    policies.clone(),
                    catalog.clone(),
                    sessions.clone(),
                ),
                release: AtomicBool::new(false),
            });
            let audit = Arc::new(InMemoryAuditRecorder::new());
            let runtime = Runtime::new(
                tools,
                kernel.clone(),
                audit.clone(),
                sessions.clone(),
                policies,
                catalog,
            );
            let task = TaskId::parse("task_kernel_wait").unwrap();
            let result = PlanningResult {
                contract_version: 1,
                understanding: Understanding {
                    intent: "WAIT_USER".into(),
                    entities: serde_json::json!({}),
                    missing_fields: vec![],
                    context_refs: vec![],
                    decision: UnderstandingDecision::Pass,
                },
                plan: Some(
                    AgentPlan::new(
                        task.clone(),
                        1,
                        vec![PlanNode::UserDecision {
                            id: NodeId::parse("wait").unwrap(),
                            depends_on: vec![],
                        }],
                    )
                    .unwrap(),
                ),
                clarification: None,
            };
            let mock = Arc::new(crate::MockLlm::new(result.clone()));
            let intake =
                PlanningIntakeService::for_trusted_backend(&runtime, Some(mock.clone()), vec![])
                    .unwrap();
            let session = SessionRef::parse("session_kernel_wait").unwrap();
            let raw = serde_json::to_vec(&result).unwrap();
            let mut request: ContractFuture<
                '_,
                Result<PlanningPersistenceReview<'_>, PlanningIntakeError>,
            > = if typed {
                Box::pin(intake.plan_for_persistence(&session, &task, 1, "wait"))
            } else {
                Box::pin(intake.accept_json_for_persistence(&session, &task, 1, &raw))
            };
            let waker = Waker::from(Arc::new(NoopWake));
            let mut context = Context::from_waker(&waker);
            assert!(request.as_mut().poll(&mut context).is_pending());
            if mode == 3 {
                sentinel.drift.store(true, Ordering::SeqCst);
            } else {
                *sessions.0.write().unwrap() = [None, Some("user_001"), Some("user_other")][mode]
                    .map(|u| SessionBinding {
                        user_id: UserId::parse(u).unwrap(),
                        binding_version: 2,
                    });
            }
            kernel.release.store(true, Ordering::SeqCst);
            let expected = if mode == 3 {
                PlanningIntakeError::Registration
            } else {
                PlanningIntakeError::Session
            };
            assert!(
                matches!(request.as_mut().poll(&mut context), Poll::Ready(Err(error)) if error == expected)
            );
            assert_eq!(mock.call_count(), usize::from(typed));
            assert!(audit.events().unwrap().is_empty());
        }
    }
}
