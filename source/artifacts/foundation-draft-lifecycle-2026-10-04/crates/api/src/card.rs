//! Backend binding of the received card module; no default source or tool approval.
use crate::ReadOnlyToolRegistration;
use safeflow_card_facts::account_service::{
    AccountCardFacts, AccountDataSource, AccountPage, AccountQueryService,
};
use safeflow_card_facts::service::SourceUnavailable;
use safeflow_card_runtime_adapter::{
    GetAccountCardFactsPolicy, GetAccountCardFactsTool, CARD_FACTS_POLICY_VERSION,
    CARD_FACTS_SCHEMA_VERSION,
};
use safeflow_contracts::{PolicyVersion, ResourceId, SessionRef};
use safeflow_runtime::{PreflightInputContract, SessionResolver, ToolSecurityProfile};
use safeflow_storage::StorageError;
use std::sync::Arc;

pub const CARD_SOURCE_REVISION: &str = "7ec2216343b4c33fbf89908800e7165c03e834c0";

#[derive(Clone)]
struct SharedSource(Arc<dyn AccountDataSource + Send + Sync>);
impl AccountDataSource for SharedSource {
    fn read_page(
        &self,
        user: &str,
        account: &str,
        cursor: Option<&str>,
        revision: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        self.0.read_page(user, account, cursor, revision)
    }
    fn read_current_revision(
        &self,
        user: &str,
        account: &str,
    ) -> Result<String, SourceUnavailable> {
        self.0.read_current_revision(user, account)
    }
}

pub struct SessionCardFacts {
    sessions: Arc<dyn SessionResolver>,
    source: SharedSource,
}
impl SessionCardFacts {
    pub(crate) fn new(
        sessions: Arc<dyn SessionResolver>,
        source: Arc<dyn AccountDataSource + Send + Sync>,
    ) -> Self {
        Self {
            sessions,
            source: SharedSource(source),
        }
    }
    /// Point-in-time facts only; no lock is retained through a funds transaction.
    pub fn query(
        &self,
        session: &SessionRef,
        account: &ResourceId,
    ) -> Result<AccountCardFacts, StorageError> {
        let identity = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if identity.binding_version == 0 {
            return Err(StorageError::Session);
        }
        let result = AccountQueryService::new(self.source.clone())
            .query(identity.user_id.as_str(), Some(account.as_str()));
        let current = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if current != identity {
            return Err(StorageError::Session);
        }
        result.map_err(|_| StorageError::DependencyUnavailable)
    }
}

/// The central caller must explicitly approve the proposed tool/profile. Merely
/// importing this module never registers the proposed name or supplies a source.
pub fn card_facts_registration_for_backend(
    source: Arc<dyn AccountDataSource + Send + Sync>,
    approved_profile: ToolSecurityProfile,
) -> ReadOnlyToolRegistration {
    let source = Arc::new(SharedSource(source));
    ReadOnlyToolRegistration {
        tool: Arc::new(GetAccountCardFactsTool::new(source.clone())),
        policy: Arc::new(GetAccountCardFactsPolicy::new(source)),
        policy_version: PolicyVersion::parse(CARD_FACTS_POLICY_VERSION).expect("member policy"),
        compatible_schema_version: CARD_FACTS_SCHEMA_VERSION,
        input: PreflightInputContract {
            tool_name: approved_profile.tool_name.clone(),
            tool_schema_version: approved_profile.tool_schema_version,
            policy_version: approved_profile.policy_version.clone(),
            projection_revision: "bounded-direct-input-v2".into(),
        },
        profile: approved_profile,
        approved_action: "READ_ACCOUNT_CARD_FACTS".into(),
        implementation_revision: CARD_SOURCE_REVISION.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use safeflow_card_facts::Lookup;
    use safeflow_contracts::*;
    use safeflow_runtime::SessionBinding;
    use std::sync::RwLock;

    struct Source {
        sessions: Option<Arc<MutableSession>>,
    }
    impl AccountDataSource for Source {
        fn read_page(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<AccountPage, SourceUnavailable> {
            let snapshot = safeflow_card_facts::fixtures::restricted();
            let Lookup::Complete(accounts) = snapshot.accounts else {
                unreachable!()
            };
            let Lookup::Complete(cards) = snapshot.cards else {
                unreachable!()
            };
            let Lookup::Complete(locks) = snapshot.locks else {
                unreachable!()
            };
            let cards: Vec<_> = cards
                .into_iter()
                .filter(|c| c.account_id == "ACCOUNT-A")
                .collect();
            Ok(AccountPage {
                account: accounts[0].clone(),
                revision: "fixture-revision-1".into(),
                total_cards: cards.len(),
                total_locks: locks.len(),
                cards,
                locks,
                next_cursor: None,
            })
        }
        fn read_current_revision(&self, _: &str, _: &str) -> Result<String, SourceUnavailable> {
            if let Some(sessions) = &self.sessions {
                sessions
                    .0
                    .write()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .binding_version += 1;
            }
            Ok("fixture-revision-1".into())
        }
    }
    struct MutableSession(RwLock<Option<SessionBinding>>);
    impl SessionResolver for MutableSession {
        fn resolve(&self, s: &SessionRef) -> Result<UserId, SecurityError> {
            Ok(self.resolve_binding(s)?.user_id)
        }
        fn resolve_binding(&self, _: &SessionRef) -> Result<SessionBinding, SecurityError> {
            self.0.read().unwrap().clone().ok_or_else(|| SecurityError {
                reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
                message: "revoked".into(),
            })
        }
    }
    fn profile() -> ToolSecurityProfile {
        ToolSecurityProfile {
            tool_name: ToolName::parse(safeflow_card_runtime_adapter::GET_ACCOUNT_CARD_FACTS)
                .unwrap(),
            tool_schema_version: CARD_FACTS_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(CARD_FACTS_POLICY_VERSION).unwrap(),
            minimum_level: SecurityLevel::Green,
        }
    }

    #[test]
    fn card_session_reader_preserves_member_identity_and_rejects_rebinding() {
        let session = SessionRef::parse("isolated-session").unwrap();
        let sessions = Arc::new(MutableSession(RwLock::new(Some(SessionBinding {
            user_id: UserId::parse("U-A").unwrap(),
            binding_version: 1,
        }))));
        let reader = SessionCardFacts::new(sessions.clone(), Arc::new(Source { sessions: None }));
        let facts = reader
            .query(&session, &ResourceId::parse("ACCOUNT-A").unwrap())
            .unwrap();
        assert!(facts.outgoing_restricted());
        assert_eq!(facts.active_card_lock_ids.len(), 2);
        assert!(reader
            .query(&session, &ResourceId::parse("ACCOUNT-B").unwrap())
            .is_err());
        let changed = SessionCardFacts::new(
            sessions.clone(),
            Arc::new(Source {
                sessions: Some(sessions.clone()),
            }),
        );
        assert_eq!(
            changed
                .query(&session, &ResourceId::parse("ACCOUNT-A").unwrap())
                .unwrap_err(),
            StorageError::Session
        );
        *sessions.0.write().unwrap() = None;
        assert_eq!(
            reader
                .query(&session, &ResourceId::parse("ACCOUNT-A").unwrap())
                .unwrap_err(),
            StorageError::Session
        );
    }

    #[tokio::test]
    async fn card_explicit_test_registration_reaches_current_api_runtime_and_validates_profile() {
        use crate::{assembly, session::SimulationSessions};
        let secret = "isolated-card-registration-secret";
        let sessions = Arc::new(
            SimulationSessions::new(
                zeroize::Zeroizing::new(secret.into()),
                UserId::parse("U-A").unwrap(),
                std::time::Duration::from_secs(60),
            )
            .unwrap(),
        );
        let cookie = sessions.issue(secret, None).unwrap();
        let cookie = cookie::Cookie::parse(cookie).unwrap();
        let reference = crate::session::token_reference(cookie.value()).unwrap();
        let registration =
            card_facts_registration_for_backend(Arc::new(Source { sessions: None }), profile());
        let assembly = assembly::assemble_with_read_only(
            assembly::AssemblyScope::Balance,
            sessions.clone(),
            Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
            Arc::new(safeflow_mock_bank::MockBankGateway::new()),
            None,
            &[registration],
        )
        .unwrap();
        let plan = AgentPlan::new(
            TaskId::parse("isolated-card-task").unwrap(),
            1,
            vec![PlanNode::Tool {
                id: NodeId::parse("read").unwrap(),
                tool: profile().tool_name,
                arguments: serde_json::json!({"account_id":"ACCOUNT-A"}),
                depends_on: vec![],
            }],
        )
        .unwrap();
        let planning = PlanningResult::for_plan(
            Understanding {
                intent: "READ_ACCOUNT_CARD_FACTS".into(),
                entities: serde_json::json!({}),
                missing_fields: vec![],
                context_refs: vec![],
                decision: UnderstandingDecision::Pass,
            },
            plan,
        );
        let safeflow_runtime::RuntimeOutcome::Completed { results, .. } = assembly
            .runtime()
            .execute(planning, reference)
            .await
            .unwrap()
        else {
            panic!("completed read expected")
        };
        assert_eq!(
            results[0].data["card_lock_restrictions"]["all_outgoing"],
            true
        );
        let mut bad = profile();
        bad.tool_schema_version += 1;
        let registration =
            card_facts_registration_for_backend(Arc::new(Source { sessions: None }), bad);
        assert!(assembly::assemble_with_read_only(
            assembly::AssemblyScope::Balance,
            sessions,
            Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
            Arc::new(safeflow_mock_bank::MockBankGateway::new()),
            None,
            &[registration]
        )
        .is_err());
    }
}
