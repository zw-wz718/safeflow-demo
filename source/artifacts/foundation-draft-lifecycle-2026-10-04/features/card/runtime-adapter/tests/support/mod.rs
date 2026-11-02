#![allow(dead_code)]
// Fictional, in-memory fixtures and explicit test-only runtime assembly.
use safeflow_card_facts::account_service::{AccountDataSource, AccountPage};
use safeflow_card_facts::service::SourceUnavailable;
use safeflow_card_facts::{Account, Card, LockRecord, LockSource, LockState};
use safeflow_card_runtime_adapter::{
    GetAccountCardFactsPolicy, GetAccountCardFactsTool, CARD_FACTS_POLICY_VERSION,
    CARD_FACTS_SCHEMA_VERSION, GET_ACCOUNT_CARD_FACTS,
};
use safeflow_contracts::{
    AgentPlan, NodeId, PlanNode, PlanningResult, PolicyVersion, SecurityLevel, SessionRef, TaskId,
    ToolName, Understanding, UnderstandingDecision, UserId,
};
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeAssembly, RuntimeBootstrap,
    StaticSessionResolver, ToolRegistry, ToolSecurityCatalog, ToolSecurityProfile,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll, Wake, Waker};

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Locked,
    Clear,
    Foreign,
    Incomplete,
    Changed,
    Unavailable,
    ChangeAfterPolicy,
}

pub struct Source {
    pub mode: Mode,
    pub reads: AtomicUsize,
    pub revisions: AtomicUsize,
}
impl Source {
    pub fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            reads: AtomicUsize::new(0),
            revisions: AtomicUsize::new(0),
        })
    }
}
impl AccountDataSource for Source {
    fn read_page(
        &self,
        user: &str,
        account: &str,
        cursor: Option<&str>,
        revision: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, Mode::Unavailable) {
            return Err(SourceUnavailable);
        }
        assert!(cursor.is_none());
        assert!(revision.is_none());
        let owner = if matches!(self.mode, Mode::Foreign)
            || matches!(self.mode, Mode::ChangeAfterPolicy) && read > 1
            || user != "U1"
            || !matches!(account, "A" | "C")
        {
            "U2"
        } else {
            "U1"
        };
        let cards = vec![
            Card {
                id: format!("{account}-K1"),
                account_id: account.into(),
                last_four: "1234".into(),
            },
            Card {
                id: format!("{account}-K2"),
                account_id: account.into(),
                last_four: "5678".into(),
            },
        ];
        let locked = account == "A" && !matches!(self.mode, Mode::Clear);
        let locks = if locked {
            [
                LockSource::UserSelf,
                LockSource::AnomalyDispute,
                LockSource::RiskControl,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, source)| LockRecord {
                id: format!("L{index}"),
                card_id: format!("{account}-K2"),
                source,
                state: LockState::Active,
            })
            .collect()
        } else {
            vec![]
        };
        Ok(AccountPage {
            account: Account {
                id: account.into(),
                user_id: owner.into(),
            },
            revision: "r1".into(),
            total_cards: if matches!(self.mode, Mode::Incomplete) {
                3
            } else {
                2
            },
            total_locks: if locked { 3 } else { 0 },
            cards,
            locks,
            next_cursor: None,
        })
    }
    fn read_current_revision(
        &self,
        user: &str,
        account: &str,
    ) -> Result<String, SourceUnavailable> {
        self.revisions.fetch_add(1, Ordering::SeqCst);
        assert_eq!(user, "U1");
        assert!(matches!(account, "A" | "C"));
        Ok(if matches!(self.mode, Mode::Changed) {
            "r2"
        } else {
            "r1"
        }
        .into())
    }
}

pub fn assembly<S: AccountDataSource + Send + Sync + 'static>(
    source: Arc<S>,
    level: SecurityLevel,
) -> (RuntimeAssembly, Arc<InMemoryAuditRecorder>) {
    let name = ToolName::parse(GET_ACCOUNT_CARD_FACTS).unwrap();
    let version = PolicyVersion::parse(CARD_FACTS_POLICY_VERSION).unwrap();
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(GetAccountCardFactsTool::new(source.clone())))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(GetAccountCardFactsPolicy::new(source)),
            version.clone(),
            CARD_FACTS_SCHEMA_VERSION,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    // Test configuration only; production minimum level belongs to foundation.
    catalog
        .register(ToolSecurityProfile {
            tool_name: name,
            tool_schema_version: CARD_FACTS_SCHEMA_VERSION,
            policy_version: version,
            minimum_level: level,
        })
        .unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session(), UserId::parse("U1").unwrap());
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, Arc::new(sessions), audit.clone())
            .unwrap();
    (assembly, audit)
}

pub fn session() -> SessionRef {
    SessionRef::parse("mock_card_session_U1").unwrap()
}
pub fn plan(arguments: Value) -> PlanningResult {
    PlanningResult::for_plan(
        Understanding {
            intent: "READ_ACCOUNT_CARD_FACTS".into(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        AgentPlan::new(
            TaskId::parse("mock_card_task").unwrap(),
            1,
            vec![PlanNode::Tool {
                id: NodeId::parse("mock_card_query").unwrap(),
                tool: ToolName::parse(GET_ACCOUNT_CARD_FACTS).unwrap(),
                arguments,
                depends_on: vec![],
            }],
        )
        .unwrap(),
    )
}
